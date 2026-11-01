//! 入站适配：GBK / UTF-8 编码探测与**显式失败**（**不猜转码**）。
//!
//! 超声工作站投递的报告文件有三种来路：UTF-8、GBK（老设备 / 老接口的默认编码），
//! 以及**混编**（同一份文件里有的段是 UTF-8、有的段是 GBK ——
//! 多见于「模板是 UTF-8、所见是 GBK」的导出）。
//!
//! **为什么必须显式判别**：猜错转码会让测量值与单位**一起**乱码，
//! 而乱码在台账里**看起来仍然像一个数字** —— 32mm 可能变成「3���mm」，
//! 或者干脆变成另一个数。台账是医师核对差值的依据，
//! **乱码比缺这一行更危险**。所以本模块的立场是：
//! **判不出来就不判** —— 把文件按失败处理，带**稳定错误码**进失败清单，
//! 而不是硬转一个「像那么回事」的文本。
//!
//! **判定顺序**（fail-closed）：
//!
//! 1. 整份字节能按 **UTF-8 严格解码** → [`Encoding::Utf8`]；
//! 2. 否则试 **GBK**：解得干净 → [`Encoding::Gbk`]；
//! 3. GBK 解到**非法序列**（前半段能解、后面接不上）→ [`Encoding::Mixed`]；
//! 4. **连一个字节都解不出**（二进制 / 非文本）→ [`Encoding::Unknown`]；
//! 5. GBK 解得干净、但文件里**夹着一段合法的 UTF-8 中文** → 仍判 [`Encoding::Mixed`]
//!    （同一份文件里两种编码，不猜哪一段是哪一种）。
//!
//! `Mixed` 与 `Unknown` 一律 **fail-closed**：不产出正文、不产出条目，
//! 只产出**错误码 + 判定依据**，由调用方落进失败清单与运行摘要。
//!
//! **与源产品的同构之处**（改写，不复制源代码）：源产品的宽字符运行时内置一张
//! **UTF-8 校验状态机表**（`UTF8_TRANSITION`）—— 多字节序列按状态机逐字节校验，
//! 遇到非法字节**报出来**而不是猜；本项目把同一立场落到中文报告的**编码**这一层：
//! 先严格校验 UTF-8 → 再试 GBK → 判不出就带**位置**报出来，
//! 判定依据（哪一段、哪个字节）一并留痕，
//! 运维能分清「编码问题」与「词典缺口」。

use encoding_rs::{DecoderResult, GBK};

use crate::parse::errors::{DetailCode, ErrorCode};

/// 报告正文的编码判定结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    /// UTF-8（整份字节严格解码通过）。
    Utf8,
    /// GBK（字节结构合法且字符表可解）。
    Gbk,
    /// 混编 / 破损：解出一部分、后面接不上，或文件里夹着另一种编码的中文。
    Mixed,
    /// 无法判定：两种编码都解不出（多半是二进制文件或根本不是文本）。
    Unknown,
}

impl Encoding {
    /// 编码名（进日志、失败清单与运维摘要的稳定取值）。
    pub fn as_str(self) -> &'static str {
        match self {
            Encoding::Utf8 => "utf-8",
            Encoding::Gbk => "gbk",
            Encoding::Mixed => "mixed",
            Encoding::Unknown => "unknown",
        }
    }

    /// 是否可解码（`Utf8` / `Gbk` 可解；`Mixed` / `Unknown` 一律 fail-closed）。
    pub fn is_decodable(self) -> bool {
        matches!(self, Encoding::Utf8 | Encoding::Gbk)
    }

    /// 判定失败时对应的稳定错误码。
    pub fn error_code(self) -> Option<ErrorCode> {
        match self {
            Encoding::Utf8 | Encoding::Gbk => None,
            Encoding::Mixed => Some(ErrorCode::EncodingMixed),
            Encoding::Unknown => Some(ErrorCode::EncodingUnknown),
        }
    }
}

/// 编码探测结果（判定 + 依据）。
#[derive(Clone, Debug)]
pub struct EncodingProbe {
    /// 判定结果。
    pub encoding: Encoding,
    /// 原始字节数。
    pub byte_len: usize,
    /// 判定依据（中文一句话，进失败清单与日志）。
    pub reason: String,
    /// 首个非法字节的 0 基偏移（判定为 `Mixed` 且定位得到时才有值）。
    pub illegal_at: Option<usize>,
}

/// 解码后的报告正文。
#[derive(Clone, Debug)]
pub struct DecodedReport {
    /// 正文（解析内核的输入）。
    pub text: String,
    /// 正文采用的编码。
    pub encoding: Encoding,
}

/// 编码判别失败（fail-closed）。
///
/// **不产出正文**：调用方拿不到「猜出来的」文本，只能拿错误码与判定依据。
#[derive(Clone, Debug)]
pub enum EncodingError {
    /// 混编 / 破损：解出一部分、后面接不上。
    Mixed {
        /// 原始字节数。
        byte_len: usize,
        /// 判定依据。
        reason: String,
        /// 首个非法字节的 0 基偏移（定位得到时）。
        illegal_at: Option<usize>,
    },
    /// 无法判定：两种编码都解不出。
    Unknown {
        /// 原始字节数。
        byte_len: usize,
        /// 判定依据。
        reason: String,
    },
}

impl EncodingError {
    /// 对应的稳定错误码（`ENCODING_MIXED` / `ENCODING_UNKNOWN`）。
    pub fn error_code(&self) -> ErrorCode {
        match self {
            EncodingError::Mixed { .. } => ErrorCode::EncodingMixed,
            EncodingError::Unknown { .. } => ErrorCode::EncodingUnknown,
        }
    }

    /// 对应的**明细归类**（登记在码表里，不在调用点拼字符串）。
    pub fn detail_code(&self) -> DetailCode {
        match self {
            EncodingError::Mixed { .. } => DetailCode::EncodingMixedBytes,
            EncodingError::Unknown { .. } => DetailCode::EncodingUnknownBytes,
        }
    }

    /// 判定依据（中文，进失败清单）。
    pub fn reason(&self) -> &str {
        match self {
            EncodingError::Mixed { reason, .. } => reason,
            EncodingError::Unknown { reason, .. } => reason,
        }
    }

    /// 原始字节数。
    pub fn byte_len(&self) -> usize {
        match self {
            EncodingError::Mixed { byte_len, .. } => *byte_len,
            EncodingError::Unknown { byte_len, .. } => *byte_len,
        }
    }
}

impl std::fmt::Display for EncodingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.reason())
    }
}

impl std::error::Error for EncodingError {}

/// UTF-8 BOM（`EF BB BF`）。
const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// 判定报告正文的编码。
///
/// 见模块文档的**判定顺序**。判定不了就判 `Mixed` / `Unknown` —— 不猜。
pub fn detect_encoding(bytes: &[u8]) -> Encoding {
    probe_encoding(bytes).encoding
}

/// 判定报告正文的编码，并给出**判定依据**。
pub fn probe_encoding(bytes: &[u8]) -> EncodingProbe {
    // 1) UTF-8 严格解码：整份通过就是 UTF-8。
    if std::str::from_utf8(bytes).is_ok() {
        return EncodingProbe {
            encoding: Encoding::Utf8,
            byte_len: bytes.len(),
            reason: format!("整份 {} 字节可按 UTF-8 严格解码", bytes.len()),
            illegal_at: None,
        };
    }

    // 2) 试 GBK（WHATWG GBK 字符表；**不产出替换字符**）。
    let (text, had_errors, at) = gbk_decode(bytes);
    if !had_errors {
        // GBK 解得干净：若文件里夹着一段合法 UTF-8 中文 → 仍是混编。
        if let Some(run_at) = utf8_cjk_run_at(bytes) {
            return EncodingProbe {
                encoding: Encoding::Mixed,
                byte_len: bytes.len(),
                reason: format!(
                    "GBK 结构完好，但第 {} 字节起是一段合法的 UTF-8 中文（同一文件两种编码）",
                    run_at
                ),
                illegal_at: Some(run_at),
            };
        }
        return EncodingProbe {
            encoding: Encoding::Gbk,
            byte_len: bytes.len(),
            reason: format!(
                "UTF-8 解不出，但 {} 字节可按 GBK 解出（{} 个字符）",
                bytes.len(),
                text.chars().count()
            ),
            illegal_at: None,
        };
    }

    // 3) GBK 解到非法序列（位置 = 第一个解不出来的字节偏移）。
    let prefix = &bytes[..at.min(bytes.len())];

    // 前缀本身就是一段合法 UTF-8 中文 → UTF-8 头 + 非 UTF-8 尾（典型混编）。
    if let Some(prefix_len) = utf8_cjk_prefix_len(prefix) {
        return EncodingProbe {
            encoding: Encoding::Mixed,
            byte_len: bytes.len(),
            reason: format!(
                "前 {} 字节是 UTF-8 中文，第 {} 字节起不是合法 UTF-8（UTF-8 段 + 非 UTF-8 段）",
                prefix_len, at
            ),
            illegal_at: Some(at),
        };
    }
    // 尾部夹着 UTF-8 中文 → GBK 头 + UTF-8 尾（另一种典型混编）。
    if let Some(tail_at) = utf8_cjk_run_at(&bytes[at.min(bytes.len())..]) {
        return EncodingProbe {
            encoding: Encoding::Mixed,
            byte_len: bytes.len(),
            reason: format!(
                "第 {} 字节起接不上 GBK，其后第 {} 字节起是一段合法的 UTF-8 中文（GBK 段 + UTF-8 段）",
                at, tail_at
            ),
            illegal_at: Some(at),
        };
    }
    // 一个字节都解不出 → 多半是二进制 / 非文本。
    if prefix.is_empty() {
        return EncodingProbe {
            encoding: Encoding::Unknown,
            byte_len: bytes.len(),
            reason: format!(
                "第 {} 字节即非法：UTF-8 与 GBK 都解不出（多半不是文本文件）",
                at
            ),
            illegal_at: Some(at),
        };
    }
    // 前缀像正文、后面却断了 → GBK 前缀 + 破损段。
    if is_text_like(prefix) {
        return EncodingProbe {
            encoding: Encoding::Mixed,
            byte_len: bytes.len(),
            reason: format!(
                "前 {} 字节可解，第 {} 字节起不是合法 GBK（GBK 段 + 破损段）",
                prefix.len(),
                at
            ),
            illegal_at: Some(at),
        };
    }
    // 前缀是二进制 → 无法判定。
    EncodingProbe {
        encoding: Encoding::Unknown,
        byte_len: bytes.len(),
        reason: format!(
            "前 {} 字节含控制字符，判为非文本文件（第 {} 字节起非法）",
            prefix.len(),
            at
        ),
        illegal_at: Some(at),
    }
}

/// 解码报告正文：判得出来才给正文，判不出来一律**显式失败**。
///
/// 这是**唯一**的正文解码入口：自由文本与 HL7 两条通道都走它，
/// 免得两条通道各判一次、判出两个结果。
pub fn decode_report(bytes: &[u8]) -> Result<DecodedReport, EncodingError> {
    let probe = probe_encoding(bytes);
    match probe.encoding {
        Encoding::Utf8 => {
            // 走到这里 UTF-8 必然可解（探测阶段已验过）。
            let text = std::str::from_utf8(bytes).map_err(|_| EncodingError::Unknown {
                byte_len: bytes.len(),
                reason: "UTF-8 严格解码失败（探测与解码不一致）".to_string(),
            })?;
            // BOM 不进正文：它是编码标记，不是报告内容。
            let text = text.strip_prefix('\u{feff}').unwrap_or(text);
            Ok(DecodedReport {
                text: text.to_string(),
                encoding: Encoding::Utf8,
            })
        }
        Encoding::Gbk => {
            let (text, had_errors, _) = gbk_decode(bytes);
            if had_errors {
                return Err(EncodingError::Mixed {
                    byte_len: bytes.len(),
                    reason: "GBK 解码中途出现非法序列，不产出正文".to_string(),
                    illegal_at: None,
                });
            }
            Ok(DecodedReport {
                text,
                encoding: Encoding::Gbk,
            })
        }
        Encoding::Mixed => Err(EncodingError::Mixed {
            byte_len: bytes.len(),
            reason: probe.reason,
            illegal_at: probe.illegal_at,
        }),
        Encoding::Unknown => Err(EncodingError::Unknown {
            byte_len: bytes.len(),
            reason: probe.reason,
        }),
    }
}

/// GBK 字符表解码（WHATWG GBK 字符表，**不产出替换字符**）。
///
/// 返回 `(解出的前缀文本, 是否出错, 出错字节的起始偏移)`。
/// 出错时 `text` 只含**成功解出**的那一段 —— 后面的字节接不上，
/// 不拿替换字符糊过去（糊过去的乱码看起来仍像一个数字）。
fn gbk_decode(bytes: &[u8]) -> (String, bool, usize) {
    let mut decoder = GBK.new_decoder_without_bom_handling();
    // 一个 GBK 汉字最多占 3 个 UTF-8 字节，留足容量避免解码器中途报「输出缓冲区满」。
    let mut out = String::with_capacity(bytes.len() * 3 + 16);
    let (result, read) = decoder.decode_to_string_without_replacement(bytes, &mut out, true);
    let had_errors = !matches!(result, DecoderResult::InputEmpty);
    // 出错位置取**畸形序列的起点**：`read` 含畸形序列本身，要把它减掉。
    let at = match result {
        DecoderResult::Malformed(len, consumed) => {
            read.saturating_sub(len as usize + consumed as usize)
        }
        _ => read.min(bytes.len()),
    };
    (out, had_errors, at)
}

/// 文件里是否夹着一段**合法的 UTF-8 中文**（≥ 2 个连续汉字）。
///
/// **为什么要求连续 2 个**：单个汉字的偶然命中约 1%（GBK 的两个字节
/// 偶尔能凑成一个合法 UTF-8 汉字），连续 2 个降到万分之几，
/// 而真实的 UTF-8 段落必然远长于此 —— 这条线把误判压到可忽略。
fn utf8_cjk_run_at(bytes: &[u8]) -> Option<usize> {
    let mut idx = 0usize;
    let mut run = 0usize;
    let mut run_start = 0usize;
    while idx < bytes.len() {
        match decode_utf8_char(&bytes[idx..]) {
            Some((ch, len)) => {
                if is_cjk(ch) {
                    if run == 0 {
                        run_start = idx;
                    }
                    run += 1;
                    if run >= 2 {
                        return Some(run_start);
                    }
                } else {
                    run = 0;
                }
                idx += len;
            }
            None => {
                // 不是合法 UTF-8 起点：跳过 1 字节继续找（可能在多字节序列内部）。
                run = 0;
                idx += 1;
            }
        }
    }
    None
}

/// 一段字节是否是**合法 UTF-8 且含中文**。
///
/// 允许末尾**截断半个字符**（最多回退 3 字节）：判混编时，
/// 出错位置常常正落在一个 UTF-8 字符中间 —— 那时前缀本身不是合法 UTF-8，
/// 但「这是一个 UTF-8 文件」这件事仍然成立。
fn utf8_cjk_prefix_len(bytes: &[u8]) -> Option<usize> {
    for trim in 0..4usize {
        let end = bytes.len().saturating_sub(trim);
        if let Ok(text) = std::str::from_utf8(&bytes[..end]) {
            return if text.chars().any(is_cjk) {
                Some(end)
            } else {
                None
            };
        }
    }
    None
}

/// 一段字节是否**像正文文本**（不含控制字符）。
///
/// 控制字符（除 `\t` / `\r` / `\n` 外）意味着这是二进制内容，不是报告。
fn is_text_like(bytes: &[u8]) -> bool {
    !bytes.is_empty()
        && bytes
            .iter()
            .all(|b| *b >= 0x20 || *b == b'\t' || *b == b'\r' || *b == b'\n')
}

/// 解码一个 UTF-8 字符（返回字符与字节长度）。
fn decode_utf8_char(bytes: &[u8]) -> Option<(char, usize)> {
    let len = utf8_char_len(*bytes.first()?)?;
    if bytes.len() < len {
        return None;
    }
    let text = std::str::from_utf8(&bytes[..len]).ok()?;
    let ch = text.chars().next()?;
    Some((ch, len))
}

/// UTF-8 字符的字节长度（按首字节）。
fn utf8_char_len(lead: u8) -> Option<usize> {
    match lead {
        0x00..=0x7F => Some(1),
        0xC2..=0xDF => Some(2),
        0xE0..=0xEF => Some(3),
        0xF0..=0xF4 => Some(4),
        _ => None,
    }
}

/// 是否是「中文」字符（汉字 / CJK 标点 / 全角符号）。
fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x3000..=0x303F      // CJK 标点
        | 0x3400..=0x4DBF    // CJK 扩展 A
        | 0x4E00..=0x9FFF    // CJK 基本区
        | 0xF900..=0xFAFF    // CJK 兼容汉字
        | 0xFF00..=0xFFEF)   // 全角形式
}

/// UTF-8 BOM 判定（供调用方按需剥离）。
pub fn has_utf8_bom(bytes: &[u8]) -> bool {
    bytes.starts_with(&UTF8_BOM)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 把一段中文文本编成 GBK 字节（用字符表自带的编码器，不手抄字节）。
    fn gbk_bytes(text: &str) -> Vec<u8> {
        let (bytes, _, _) = GBK.encode(text);
        bytes.into_owned()
    }

    /// 样例：一份泌尿系报告正文（UTF-8）。
    const REPORT: &str = "病历号：PID-EXAMPLE\n超声所见\n左肾积水 3.2cm\n右输尿管内径 0.6cm\n";

    #[test]
    fn 纯utf8判为utf8() {
        let bytes = REPORT.as_bytes();
        assert_eq!(detect_encoding(bytes), Encoding::Utf8);
        let probe = probe_encoding(bytes);
        assert!(probe.reason.contains("UTF-8"), "{}", probe.reason);
        assert_eq!(probe.illegal_at, None);
    }

    #[test]
    fn 纯gbk判为gbk且能解回原文() {
        let bytes = gbk_bytes(REPORT);
        assert_eq!(detect_encoding(&bytes), Encoding::Gbk);
        let decoded = decode_report(&bytes).expect("纯 GBK 应解得出正文");
        assert_eq!(decoded.encoding, Encoding::Gbk);
        assert_eq!(decoded.text, REPORT, "GBK 解码应还原原文");
    }

    #[test]
    fn 混编判为mixed且不产出正文() {
        // 前半段 GBK（"左肾积水 3.2cm"），后半段 UTF-8（"积水明显"）。
        let mut bytes = gbk_bytes("左肾积水 3.2cm\n");
        bytes.extend_from_slice("积水明显".as_bytes());
        assert_eq!(detect_encoding(&bytes), Encoding::Mixed);
        match decode_report(&bytes) {
            Err(EncodingError::Mixed { reason, .. }) => assert!(!reason.is_empty()),
            other => panic!("混编应显式失败，实际：{:?}", other.map(|d| d.encoding)),
        }
    }

    #[test]
    fn utf8头接gbk尾判为mixed() {
        // 前半段 UTF-8（"超声所见"），后半段 GBK（"左肾"）。
        let mut bytes = "超声所见\n".as_bytes().to_vec();
        bytes.extend_from_slice(&gbk_bytes("左肾"));
        assert_eq!(detect_encoding(&bytes), Encoding::Mixed);
        assert!(matches!(
            decode_report(&bytes),
            Err(EncodingError::Mixed { .. })
        ));
    }

    #[test]
    fn 非法字节判为unknown且不产出正文() {
        // 二进制垃圾：第一个字节就非法。
        let bytes = vec![0xFF, 0xFE, 0x00, 0x01, 0x8F];
        assert_eq!(detect_encoding(&bytes), Encoding::Unknown);
        match decode_report(&bytes) {
            Err(EncodingError::Unknown { reason, .. }) => assert!(!reason.is_empty()),
            other => panic!("非法字节应显式失败，实际：{:?}", other.map(|d| d.encoding)),
        }
    }

    #[test]
    fn 控制字符前缀判为unknown而非mixed() {
        // 前缀是二进制（ZIP 头），不是正文 → 不该当成「混编」。
        let mut bytes = vec![0x50, 0x4B, 0x03, 0x04, 0x00];
        bytes.extend_from_slice(&[0xFF, 0xFE]);
        assert_eq!(detect_encoding(&bytes), Encoding::Unknown);
    }

    #[test]
    fn 错误码稳定且阻断整份报告() {
        assert_eq!(Encoding::Mixed.error_code(), Some(ErrorCode::EncodingMixed));
        assert_eq!(
            Encoding::Unknown.error_code(),
            Some(ErrorCode::EncodingUnknown)
        );
        assert_eq!(Encoding::Utf8.error_code(), None);
        assert_eq!(Encoding::Gbk.error_code(), None);
        assert!(ErrorCode::EncodingMixed.blocks_report());
        assert!(ErrorCode::EncodingUnknown.blocks_report());
        assert_eq!(ErrorCode::EncodingMixed.as_str(), "ENCODING_MIXED");
        assert_eq!(ErrorCode::EncodingUnknown.as_str(), "ENCODING_UNKNOWN");
    }

    #[test]
    fn utf8_bom被剥离不进正文() {
        let mut bytes = UTF8_BOM.to_vec();
        bytes.extend_from_slice(REPORT.as_bytes());
        assert!(has_utf8_bom(&bytes));
        assert_eq!(detect_encoding(&bytes), Encoding::Utf8);
        let decoded = decode_report(&bytes).unwrap();
        assert_eq!(decoded.text, REPORT, "BOM 不进正文");
    }

    #[test]
    fn 空文件判为utf8且解出空正文() {
        let decoded = decode_report(b"").expect("空文件按 UTF-8 处理");
        assert_eq!(decoded.encoding, Encoding::Utf8);
        assert!(decoded.text.is_empty());
    }

    #[test]
    fn 仓库脱敏样例编成gbk后仍判为gbk() {
        // 真实语料回归：把仓库里的 UTF-8 脱敏报告编成 GBK，逐份判别 ——
        // 若判定规则误把 GBK 当混编，院内老设备投的报告会被整批拒收。
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/samples/reports");
        let mut checked = 0usize;
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .expect("脱敏样例目录应存在")
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|ext| ext == "txt").unwrap_or(false))
            .collect();
        entries.sort_by_key(|e| e.path());
        for entry in entries {
            let text = std::fs::read_to_string(entry.path()).expect("样例应可读");
            let bytes = gbk_bytes(&text);
            assert_eq!(
                detect_encoding(&bytes),
                Encoding::Gbk,
                "{} 转 GBK 后应判为 GBK",
                entry.path().display()
            );
            let decoded = decode_report(&bytes).expect("GBK 应解得出正文");
            assert_eq!(decoded.text, text, "GBK 解码应还原原文");
            checked += 1;
        }
        assert!(checked >= 20, "脱敏样例应至少有 20 份，实际 {}", checked);
    }
}
