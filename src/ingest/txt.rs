//! 入站适配：自由文本报告读取 → [`SourceDocument`]（原文字节 + 解码文本 + 字节↔字符光标映射）。
//!
//! 入站形态是**文件投递**：超声工作站把报告写到 `inbox/<检查号>/report.txt`。
//! 本模块只做一件事 —— 把那个文件读成一份**可定位**的文档：
//! 原文一字不改地留在 `raw_bytes`（幂等摘要与审计以字节为准），
//! 解码后的正文在 `text`（解析内核的输入），
//! 字节偏移与 1 基字符光标之间的换算在 `char_map`（失败条目要指回原文第几行第几列）。
//!
//! 机制溯源（**改写**，不复制源代码）：源产品的解析驱动层保留输入缓冲区位置，
//! 错误位置再由**字节偏移**换算成 1 基**字符**光标，位置不可确定时返回 0（不编造）。
//! 本项目把同一换算落到中文超声报告上：UTF-8 多字节按**字符**计（不按字节），
//! 全角字符占 1 列；字节偏移落在多字节字符内部时取该字符的光标，
//! 越界或字节不是合法 UTF-8 一律返回 0（位置未知，不编造）。
//!
//! **编码边界**：正文解码走**唯一入口** [`crate::ingest::encoding::decode_report`] ——
//! UTF-8 严格解码 → GBK → 混编 / 未知；判不出来一律**显式失败**
//! （`ENCODING_MIXED` / `ENCODING_UNKNOWN`），**本层不猜转码** ——
//! 猜错转码会让测量值与单位一起乱码，而乱码在台账里看起来仍然像一个数字。
//!
//! **不产生中间文件**：读取只读源文件，**不在投递目录里写任何东西**
//! （不落转码副本、不落临时文件），源目录保持工作站投递时的原样。

use std::path::Path;

use crate::ingest::encoding::{decode_report, Encoding, EncodingError};

/// 投递目录里的自由文本报告文件名。
pub const REPORT_TXT: &str = "report.txt";

/// 字节偏移 → 1 基字符光标映射表。
///
/// 下标是**字节偏移**，值是该字节所属字符的 **1 基字符光标**；
/// 多字节字符的后续字节与首字节同值；位置不可确定时为 0（不编造）。
#[derive(Clone, Debug, Default)]
pub struct CharMap {
    entries: Vec<usize>,
}

impl CharMap {
    /// 按字节建立映射。
    ///
    /// 合法 UTF-8：每个字节都指向它所属字符的 1 基光标。
    /// 非法 UTF-8：整张表置 0（位置不可确定，不编造）。
    pub fn build(bytes: &[u8]) -> Self {
        let mut entries = vec![0usize; bytes.len()];
        if let Ok(text) = std::str::from_utf8(bytes) {
            for (idx, (byte_offset, ch)) in text.char_indices().enumerate() {
                let cursor = idx + 1;
                for slot in entries.iter_mut().skip(byte_offset).take(ch.len_utf8()) {
                    *slot = cursor;
                }
            }
        }
        CharMap { entries }
    }

    /// 字节偏移 → 1 基字符光标；越界或不可确定返回 0。
    pub fn char_offset(&self, byte_index: usize) -> usize {
        self.entries.get(byte_index).copied().unwrap_or(0)
    }

    /// 映射表覆盖的字节数。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 映射表是否为空（空文件时为空）。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// 字节偏移 → 1 基字符光标（独立函数，供后续定位任务复用）。
///
/// 与 [`CharMap::char_offset`] 同口径：落在多字节字符内部时取该字符的光标；
/// 越界或字节不是合法 UTF-8 返回 0（位置未知，不编造）。
pub fn to_char_offset(bytes: &[u8], byte_index: usize) -> usize {
    if byte_index >= bytes.len() {
        return 0;
    }
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => return 0,
    };
    for (idx, (byte_offset, ch)) in text.char_indices().enumerate() {
        if byte_index < byte_offset + ch.len_utf8() {
            return idx + 1;
        }
    }
    0
}

/// 一份报告的入站文档。
#[derive(Clone, Debug)]
pub struct SourceDocument {
    /// 检查号（取自投递目录名 `inbox/<study_no>/`）。
    pub study_no: String,
    /// 原文字节（一字不改；幂等摘要与审计以字节为准）。
    pub raw_bytes: Vec<u8>,
    /// 解码后的正文（解析内核的输入）。
    pub text: String,
    /// 正文采用的编码（UTF-8 / GBK）—— 口径追溯用。
    pub encoding: Encoding,
    /// 字节偏移 → 1 基字符光标映射。
    pub char_map: CharMap,
}

impl SourceDocument {
    /// 由原文字节构造（编码判别见 [`crate::ingest::encoding`]）。
    ///
    /// 判不出编码即报错 —— **不猜转码**。
    pub fn from_bytes(
        study_no: impl Into<String>,
        raw_bytes: Vec<u8>,
    ) -> Result<Self, TxtError> {
        let decoded = decode_report(&raw_bytes).map_err(TxtError::Encoding)?;
        let text = decoded.text;
        let char_map = CharMap::build(text.as_bytes());
        Ok(SourceDocument {
            study_no: study_no.into(),
            raw_bytes: raw_bytes.clone(),
            text,
            encoding: decoded.encoding,
            char_map,
        })
    }

    /// 字节偏移 → 1 基字符光标。
    pub fn char_offset(&self, byte_index: usize) -> usize {
        self.char_map.char_offset(byte_index)
    }

    /// 原文字节数。
    pub fn byte_len(&self) -> usize {
        self.raw_bytes.len()
    }

    /// 原文字符数（1 基光标的上界）。
    pub fn char_len(&self) -> usize {
        self.text.chars().count()
    }

    /// 正文是否全为空白（空报告 / 纯空白报告都走这里，不报错）。
    pub fn is_blank(&self) -> bool {
        self.text.trim().is_empty()
    }
}

/// 自由文本报告的读取错误。
#[derive(Debug)]
pub enum TxtError {
    /// 文件系统错误。
    Io(std::io::Error),
    /// 编码判别失败（混编 / 无法判定）—— **不猜转码**，不产出正文。
    Encoding(EncodingError),
    /// 投递目录名取不到检查号。
    MissingStudyNo,
}

impl std::fmt::Display for TxtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TxtError::Io(e) => write!(f, "读取报告文件失败：{}", e),
            TxtError::Encoding(e) => write!(f, "报告正文编码判别失败：{}", e),
            TxtError::MissingStudyNo => write!(f, "投递目录名取不到检查号"),
        }
    }
}

impl std::error::Error for TxtError {}

impl From<std::io::Error> for TxtError {
    fn from(e: std::io::Error) -> Self {
        TxtError::Io(e)
    }
}

impl From<EncodingError> for TxtError {
    fn from(e: EncodingError) -> Self {
        TxtError::Encoding(e)
    }
}

/// 读取投递目录 `inbox/<study_no>/` 里的 `report.txt`。
///
/// 检查号取自**目录名**（工作站投递时的目录名即检查号）。
/// 只读不写：源目录里不产生任何中间文件。
pub fn read_report_txt(dir: &Path) -> Result<SourceDocument, TxtError> {
    let study_no = dir
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty() && *name != "." && *name != "..")
        .ok_or(TxtError::MissingStudyNo)?
        .to_string();
    let raw_bytes = std::fs::read(dir.join(REPORT_TXT))?;
    SourceDocument::from_bytes(study_no, raw_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 含生僻字（盂 / 盏，均为 3 字节 UTF-8）与全角标点的样例。
    ///
    /// 人工标注（第 2 行第 3 个字符是「盂」）：
    /// - 全局 1 基字符光标 = 8（第 1 行 4 字 + 换行 1 字 + 第 2 行前 2 字）；
    /// - 行号 = 2，列号 = 3。
    const SAMPLE: &str = "超声所见\n左肾盂扩张（双侧），肾盂前后径 2.1cm\n右输尿管内径 0.6cm";

    /// 生僻字「盂」的 1 基字符光标（人工标注）。
    const RARE_CHAR_CURSOR: usize = 8;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "urs-ingest-txt-{}-{}",
            tag,
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_report(dir: &Path, study_no: &str, body: &str) -> std::path::PathBuf {
        let report_dir = dir.join(study_no);
        std::fs::create_dir_all(&report_dir).unwrap();
        std::fs::write(report_dir.join(REPORT_TXT), body).unwrap();
        report_dir
    }

    fn listing(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn 生僻字与全角标点的字符光标与人工标注一致() {
        let doc = SourceDocument::from_bytes("S-001", SAMPLE.as_bytes().to_vec()).unwrap();
        let byte_offset = SAMPLE.find('盂').expect("样例应含生僻字「盂」");

        assert_eq!(
            doc.char_offset(byte_offset),
            RARE_CHAR_CURSOR,
            "生僻字的 1 基字符光标应与人工标注一致"
        );
        assert_eq!(
            to_char_offset(SAMPLE.as_bytes(), byte_offset),
            RARE_CHAR_CURSOR,
            "独立函数与源文档映射须同口径"
        );
        // 与解析内核的失败定位口径对齐：字符光标 → (行, 列)。
        assert_eq!(
            crate::parse::locate::locate(&doc.text, doc.char_offset(byte_offset)),
            (2, 3),
            "生僻字应在第 2 行第 3 列"
        );
        // 全角标点各占 1 列。
        assert_eq!(doc.char_offset(SAMPLE.find('（').unwrap()), 11);
        assert_eq!(doc.char_offset(SAMPLE.find('，').unwrap()), 15);
    }

    #[test]
    fn 字节偏移落在多字节字符内部取该字符光标() {
        let doc = SourceDocument::from_bytes("S-001", SAMPLE.as_bytes().to_vec()).unwrap();
        let byte_offset = SAMPLE.find('盂').unwrap();
        // 「盂」占 3 字节：首字节与两个后续字节都指向同一个字符光标。
        for delta in 0..3 {
            assert_eq!(
                doc.char_offset(byte_offset + delta),
                RARE_CHAR_CURSOR,
                "多字节字符内部偏移 {} 应指向同一字符",
                delta
            );
        }
    }

    #[test]
    fn 越界字节偏移返回零不编造() {
        let doc = SourceDocument::from_bytes("S-001", SAMPLE.as_bytes().to_vec()).unwrap();
        assert_eq!(doc.char_offset(doc.byte_len()), 0, "末尾之后位置未知");
        assert_eq!(doc.char_offset(doc.byte_len() + 100), 0, "越界位置未知");
        assert_eq!(to_char_offset(SAMPLE.as_bytes(), SAMPLE.len()), 0);
    }

    #[test]
    fn 非法utf8字节返回零不猜位置() {
        let bytes = [0xE5, 0xB7, 0x2C, 0xFF]; // 末字节非法
        assert_eq!(to_char_offset(&bytes, 3), 0, "非法字节位置未知");
        let map = CharMap::build(&bytes);
        assert_eq!(map.char_offset(3), 0, "非法字节位置未知");
    }

    #[test]
    fn 空文件读取不报错() {
        let dir = temp_dir("empty");
        let report_dir = write_report(&dir, "S-EMPTY", "");
        let doc = read_report_txt(&report_dir).unwrap();
        assert_eq!(doc.study_no, "S-EMPTY");
        assert_eq!(doc.text, "");
        assert_eq!(doc.byte_len(), 0);
        assert_eq!(doc.char_len(), 0);
        assert!(doc.is_blank());
        assert!(doc.char_map.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 纯空白文件读取不报错() {
        let dir = temp_dir("blank");
        let report_dir = write_report(&dir, "S-BLANK", "  \n\t\r\n  ");
        let doc = read_report_txt(&report_dir).unwrap();
        assert!(doc.is_blank(), "纯空白应判为空白正文");
        assert_eq!(doc.char_len(), 8, "空白字符照样计入光标");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 源目录不产生任何未解码的中间文件() {
        let dir = temp_dir("no-intermediate");
        let report_dir = write_report(&dir, "S-001", SAMPLE);
        let before = listing(&report_dir);
        let before_bytes = std::fs::read(report_dir.join(REPORT_TXT)).unwrap();

        let doc = read_report_txt(&report_dir).unwrap();
        assert!(!doc.text.is_empty());

        let after = listing(&report_dir);
        assert_eq!(before, after, "源目录不得出现转码副本或临时文件");
        assert_eq!(
            before_bytes,
            std::fs::read(report_dir.join(REPORT_TXT)).unwrap(),
            "源文件字节不得被改写"
        );
        assert_eq!(
            listing(&dir),
            vec!["S-001".to_string()],
            "投递目录层级不得多出文件"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 检查号取自投递目录名() {
        let dir = temp_dir("study-no");
        let report_dir = write_report(&dir, "S-2026-0001", SAMPLE);
        let doc = read_report_txt(&report_dir).unwrap();
        assert_eq!(doc.study_no, "S-2026-0001");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 原文字节与解码文本同时保留() {
        let doc = SourceDocument::from_bytes("S-001", SAMPLE.as_bytes().to_vec()).unwrap();
        assert_eq!(doc.raw_bytes, SAMPLE.as_bytes(), "原文字节一字不改");
        assert_eq!(doc.text, SAMPLE, "解码文本保留全角与生僻字");
        assert_eq!(doc.char_map.len(), doc.byte_len(), "映射表覆盖全部字节");
    }

    #[test]
    fn gbk报告按gbk解出正文() {
        let dir = temp_dir("gbk");
        let report_dir = dir.join("S-GBK");
        std::fs::create_dir_all(&report_dir).unwrap();
        // GBK 的「左肾」字节（CP936：D7F3 C9F6）：不是合法 UTF-8，但按 GBK 解得出来。
        std::fs::write(report_dir.join(REPORT_TXT), [0xD7, 0xF3, 0xC9, 0xF6]).unwrap();
        let doc = read_report_txt(&report_dir).unwrap();
        assert_eq!(doc.encoding, crate::ingest::encoding::Encoding::Gbk);
        assert_eq!(doc.text, "左肾");
        assert_eq!(doc.raw_bytes, vec![0xD7, 0xF3, 0xC9, 0xF6], "原文字节一字不改");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 编码判不出时显式失败不猜转码() {
        let dir = temp_dir("mixed");
        let report_dir = dir.join("S-MIXED");
        std::fs::create_dir_all(&report_dir).unwrap();
        // 二进制垃圾：UTF-8 与 GBK 都解不出。
        std::fs::write(report_dir.join(REPORT_TXT), [0xFF, 0xFE, 0x00, 0x01]).unwrap();
        match read_report_txt(&report_dir) {
            Err(TxtError::Encoding(e)) => {
                assert_eq!(e.error_code(), crate::parse::errors::ErrorCode::EncodingUnknown);
                assert!(!e.reason().is_empty(), "失败要带判定依据");
            }
            other => panic!("应显式报编码失败，实际：{:?}", other),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 源文档喂进解析内核产出结构化条目() {
        let dir = temp_dir("parse");
        let body = "病历号：PID-EXAMPLE\n超声所见\n左肾积水 3.2cm\n";
        let report_dir = write_report(&dir, "S-001", body);
        let doc = read_report_txt(&report_dir).unwrap();
        let outcome = crate::parse::parse_report(&doc.text);
        assert_eq!(outcome.items.len(), 1, "应抽出一条结构化条目");
        assert_eq!(outcome.items[0].value_si, Some(32.0));
        assert_eq!(outcome.items[0].unit_code.as_deref(), Some("mm"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
