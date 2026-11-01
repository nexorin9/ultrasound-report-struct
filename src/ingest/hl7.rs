//! 入站适配：HL7 v2 `ORU^R01` 消息 → 权威字段 + **同一套**解析内核。
//!
//! 超声工作站的 RIS 接口把报告发成 HL7 v2 `ORU^R01`（字段来源：**HL7 v2 标准**）：
//!
//! | 字段 | 含义 | 本产品用途 |
//! |---|---|---|
//! | `MSH-9` | 消息类型 | 通道标识（`ORU^R01`） |
//! | `PID-3` | 患者标识列表（CX） | `patient_id`（患者主索引） |
//! | `OBR-3` | 检查号（EI） | `study_no`（与 RIS 同源的跨系统匹配键） |
//! | `OBR-7` | 检查日期（TS） | 检查日期 |
//! | `OBX-5` | 观测值 | 所见文本（`OBX-2=TX` 文本型） |
//!
//! 本适配层只做三件事：
//!
//! 1. 按 HL7 v2 的分隔规则切段切字段 —— **分隔符与编码字符从 `MSH-2` 取**，不写死；
//! 2. 取权威字段（检查号 / 病历号 / 检查日期）与所见文本；
//! 3. 把所见文本交给**同一套**解析内核（[`crate::parse::parse_report`]）——
//!    **不另开一条抽取路径**，避免 TXT 与 HL7 两套口径。
//!
//! **缺段不静默**：缺 `MSH` / `PID` / `OBR` / `OBX` 或必需字段为空，
//! 一律收敛成带稳定错误码 `HL7_SEGMENT_MISSING` 的**失败条目**（与解析内核的失败条目
//! 同一形状）—— 缺段消息进失败清单与运行摘要，不会被悄悄吞掉，也不产出任何数值。
//!
//! **位置指回原消息**：解析内核跑在「所见文本」上，但失败条目要指回**整条消息**的
//! 第几行第几列 —— 每个所见字符都记住它在原消息里的行列（转义序列解码后仍然对得上）。
//!
//! **不猜转码**：消息文本按 UTF-8 **严格**解码；解码失败交由编码模块判别（Task 11）。

use std::path::Path;

use crate::ingest::encoding::{decode_report, Encoding, EncodingError};
use crate::parse::errors::{DetailCode, ErrorCode, Failure};
use crate::parse::extract::ParseOutcome;
use crate::parse::parse_report;

/// 投递目录里的 HL7 v2 报告文件名。
pub const REPORT_HL7: &str = "report.hl7";

/// 段名：消息头（分隔符与编码字符都在这里）。
const SEG_MSH: &str = "MSH";
/// 段名：患者标识。
const SEG_PID: &str = "PID";
/// 段名：检查申请（订单）。
const SEG_OBR: &str = "OBR";
/// 段名：观测结果。
const SEG_OBX: &str = "OBX";

/// HL7 v2 分隔符与编码字符。
///
/// 默认值取 HL7 v2 标准；实际取值**一律从 `MSH-2` 读**（不同厂商可能改编码字符）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Separators {
    /// 字段分隔符（`MSH-1`）。
    pub field: char,
    /// 组件分隔符（`MSH-2` 第 1 字符）。
    pub component: char,
    /// 重复分隔符（`MSH-2` 第 2 字符）。
    pub repetition: char,
    /// 转义字符（`MSH-2` 第 3 字符）。
    pub escape: char,
    /// 子组件分隔符（`MSH-2` 第 4 字符）。
    pub subcomponent: char,
}

impl Default for Separators {
    fn default() -> Self {
        Self {
            field: '|',
            component: '^',
            repetition: '~',
            escape: '\\',
            subcomponent: '&',
        }
    }
}

/// 消息中的一段。
#[derive(Clone, Debug)]
pub struct Segment<'a> {
    /// 段名（3 字符，如 `MSH` / `OBX`）。
    pub name: &'a str,
    /// 段内字段（按字段分隔符切分；`fields[0]` 是段名本身）。
    pub fields: Vec<&'a str>,
    /// 该段在原消息中的 1 基行号。
    pub line: usize,
}

impl<'a> Segment<'a> {
    /// 取第 n 号字段（1 基）。
    ///
    /// `MSH-1` 是字段分隔符本身（切分时被吃掉），所以 MSH 的第 n 号字段落在切分数组的
    /// 下标 `n-1`；其余段的第 n 号字段落在下标 `n`。
    pub fn field(&self, n: usize) -> Option<&'a str> {
        let idx = self.field_index(n)?;
        self.fields.get(idx).copied()
    }

    /// 第 n 号字段在切分数组里的下标（`MSH-1` 没有对应字段）。
    fn field_index(&self, n: usize) -> Option<usize> {
        if n == 0 {
            return None;
        }
        if self.name == SEG_MSH {
            if n == 1 {
                return None; // MSH-1 = 分隔符本身
            }
            Some(n - 1)
        } else {
            Some(n)
        }
    }

    /// 第 n 号字段在本行中的起始列（1 基，按字符计）。
    pub fn field_column(&self, n: usize) -> usize {
        let idx = match self.field_index(n) {
            Some(i) => i,
            None => return 1,
        };
        let mut col = 1usize;
        for (i, f) in self.fields.iter().enumerate() {
            if i == idx {
                break;
            }
            col += f.chars().count() + 1;
        }
        col
    }
}

/// 一条 HL7 v2 消息（按段切分后）。
#[derive(Clone, Debug)]
pub struct Hl7Message<'a> {
    /// 分隔符与编码字符（取自 `MSH-2`）。
    pub separators: Separators,
    /// 段列表（文档顺序）。
    pub segments: Vec<Segment<'a>>,
}

impl<'a> Hl7Message<'a> {
    /// 找第一个指定段名的段。
    pub fn find(&self, name: &str) -> Option<&Segment<'a>> {
        self.segments.iter().find(|s| s.name == name)
    }

    /// 找全部指定段名的段（`OBX` 可有多段）。
    pub fn find_all(&self, name: &str) -> Vec<&Segment<'a>> {
        self.segments.iter().filter(|s| s.name == name).collect()
    }
}

/// HL7 v2 适配产物。
#[derive(Clone, Debug)]
pub struct Hl7Report {
    /// 检查号（`OBR-3`，权威来源；`OBR` 段缺失或字段为空时为 `None`）。
    pub study_no: Option<String>,
    /// 患者主索引（`PID-3`，权威来源）。
    pub patient_id: Option<String>,
    /// 检查日期（`OBR-7`，原样保留）。
    pub study_date: Option<String>,
    /// 消息类型（`MSH-9`）。
    pub message_type: String,
    /// 所见文本（各 `OBX` 段的 `OBX-5` 依序拼接）。
    pub findings_text: String,
    /// 解析结果：结构化条目 + 失败条目（行列已指回**原消息**）。
    pub outcome: ParseOutcome,
}

impl Hl7Report {
    /// 消息结构是否完整（没有 `HL7_SEGMENT_MISSING` 失败条目）。
    pub fn is_complete(&self) -> bool {
        !self
            .outcome
            .failures
            .iter()
            .any(|f| f.code == ErrorCode::Hl7SegmentMissing)
    }
}

/// HL7 v2 适配错误（**只含运行期错误**；消息结构问题走失败条目，不走 `Err`）。
#[derive(Debug)]
pub enum Hl7Error {
    /// 消息头段缺失 —— 没有 `MSH` 就定不出分隔符，整条消息无法解析。
    MissingMessageHeader,
    /// 文件系统错误。
    Io(std::io::Error),
    /// 编码判别失败（混编 / 无法判定）—— **不猜转码**，不产出正文。
    Encoding(EncodingError),
}

impl std::fmt::Display for Hl7Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Hl7Error::MissingMessageHeader => write!(f, "HL7 必需段缺失：MSH（消息头段）"),
            Hl7Error::Io(e) => write!(f, "读取 HL7 报告失败：{}", e),
            Hl7Error::Encoding(e) => write!(f, "HL7 报告编码判别失败：{}", e),
        }
    }
}

impl std::error::Error for Hl7Error {}

impl From<std::io::Error> for Hl7Error {
    fn from(e: std::io::Error) -> Self {
        Hl7Error::Io(e)
    }
}

impl From<EncodingError> for Hl7Error {
    fn from(e: EncodingError) -> Self {
        Hl7Error::Encoding(e)
    }
}

/// 一条读到的 HL7 消息（正文 + 原始字节 + 编码）。
///
/// **原始字节一并留着**：幂等键按**投递文件的字节**算，
/// 与监听侧的预查同源（任何编码下都一致，不因解码方式不同而算出两个键）。
#[derive(Clone, Debug)]
pub struct DecodedMessage {
    /// 消息正文（解析内核的输入）。
    pub text: String,
    /// 正文采用的编码。
    pub encoding: Encoding,
    /// 投递文件的原始字节（一字不改）。
    pub raw_bytes: Vec<u8>,
}

/// 读取投递目录 `inbox/<study_no>/report.hl7` 的消息。
///
/// 解码走**唯一入口** [`crate::ingest::encoding::decode_report`]（与自由文本通道同一套判定），
/// 判不出编码即报错（不猜转码）。
/// 只读不写：源目录里不产生任何中间文件。
pub fn read_message(dir: &Path) -> Result<DecodedMessage, Hl7Error> {
    let bytes = std::fs::read(dir.join(REPORT_HL7))?;
    let decoded = decode_report(&bytes)?;
    Ok(DecodedMessage {
        text: decoded.text,
        encoding: decoded.encoding,
        raw_bytes: bytes,
    })
}

/// 适配一条 HL7 v2 `ORU^R01` 消息。
///
/// **不因结构不全而整体失败**：缺段 / 缺字段被收敛成带稳定错误码
/// `HL7_SEGMENT_MISSING` 的失败条目，与解析内核的失败条目同一形状 ——
/// 缺段消息进失败清单与运行摘要，不会被悄悄吞掉。
pub fn adapt(text: &str) -> Hl7Report {
    match parse_message(text) {
        Ok(msg) => adapt_message(&msg),
        Err(e) => Hl7Report {
            study_no: None,
            patient_id: None,
            study_date: None,
            message_type: String::new(),
            findings_text: String::new(),
            outcome: ParseOutcome {
                items: Vec::new(),
                failures: vec![Failure::new(
                    ErrorCode::Hl7SegmentMissing,
                    0,
                    1,
                    1,
                    &e.to_string(),
                )
                .with_detail_code(DetailCode::Hl7SegmentAbsent)],
                doc_kind: crate::parse::gate::DocumentKind::UltrasoundReport,
                dictionary_version: crate::dictionary_version(),
            },
        },
    }
}

/// 按 HL7 v2 分隔规则把消息切成段。
///
/// 分隔符与编码字符**从 `MSH-2` 取**：不同厂商可能改分隔符，写死就会切错段。
pub fn parse_message(text: &str) -> Result<Hl7Message<'_>, Hl7Error> {
    let lines = split_lines(text);
    let (_, msh_line) = lines
        .first()
        .copied()
        .ok_or(Hl7Error::MissingMessageHeader)?;
    if !msh_line.starts_with(SEG_MSH) {
        return Err(Hl7Error::MissingMessageHeader);
    }

    let mut separators = Separators::default();
    // MSH-1 = 第 4 个字符（字段分隔符本身）。
    if let Some(c) = msh_line.chars().nth(3) {
        separators.field = c;
    }
    // MSH-2 = 编码字符。
    let msh_fields = split_fields(msh_line, separators.field);
    if let Some(enc) = msh_fields.get(1) {
        let mut it = enc.chars();
        if let Some(c) = it.next() {
            separators.component = c;
        }
        if let Some(c) = it.next() {
            separators.repetition = c;
        }
        if let Some(c) = it.next() {
            separators.escape = c;
        }
        if let Some(c) = it.next() {
            separators.subcomponent = c;
        }
    }

    let mut segments = Vec::with_capacity(lines.len());
    for (line_no, line) in lines {
        let name = segment_name(line, separators.field);
        let fields = split_fields(line, separators.field);
        segments.push(Segment {
            name,
            fields,
            line: line_no,
        });
    }

    Ok(Hl7Message {
        separators,
        segments,
    })
}

/// 从一条已切段的消息取权威字段并跑解析内核。
fn adapt_message(msg: &Hl7Message) -> Hl7Report {
    let mut failures: Vec<Failure> = Vec::new();

    // --- 患者主索引：PID-3 ---
    let pid = msg.find(SEG_PID);
    let patient_id = pid.and_then(|s| first_component(s.field(3), &msg.separators));
    match (pid, patient_id.as_ref()) {
        (None, _) => failures.push(missing_segment_failure(
            msg,
            SEG_MSH,
            "HL7 必需段缺失：PID（患者标识段）",
        )),
        (Some(seg), None) => failures.push(empty_field_failure(
            seg,
            3,
            "HL7 必需字段为空：PID-3（患者主索引）",
        )),
        _ => {}
    }

    // --- 检查号与检查日期：OBR-3 / OBR-7 ---
    let obr = msg.find(SEG_OBR);
    let study_no = obr.and_then(|s| first_component(s.field(3), &msg.separators));
    match (obr, study_no.as_ref()) {
        (None, _) => failures.push(missing_segment_failure(
            msg,
            SEG_PID,
            "HL7 必需段缺失：OBR（检查申请段）",
        )),
        (Some(seg), None) => failures.push(empty_field_failure(
            seg,
            3,
            "HL7 必需字段为空：OBR-3（检查号）",
        )),
        _ => {}
    }
    let study_date = obr
        .and_then(|s| s.field(7))
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    if let Some(seg) = obr {
        if study_date.is_none() {
            failures.push(empty_field_failure(
                seg,
                7,
                "HL7 必需字段为空：OBR-7（检查日期）",
            ));
        }
    }

    // --- 所见文本：各 OBX 段的 OBX-5 ---
    let obx_segments = msg.find_all(SEG_OBX);
    if obx_segments.is_empty() {
        failures.push(missing_segment_failure(
            msg,
            SEG_OBR,
            "HL7 必需段缺失：OBX（观测结果段）",
        ));
    }
    let (findings_text, positions) = collect_findings(&obx_segments, msg);
    if !obx_segments.is_empty() && findings_text.trim().is_empty() {
        failures.push(missing_segment_failure(
            msg,
            SEG_OBR,
            "HL7 所见文本为空：OBX 段没有可用的 OBX-5",
        ));
    }

    // --- 同一套解析内核（不另开一条抽取路径） ---
    let mut outcome = parse_report(&findings_text);
    remap_positions(&mut outcome, &positions);
    // 结构不全 → **不产出条目**（`HL7_SEGMENT_MISSING` 阻断整份报告）：
    // 没有病历号 / 检查号的台账行挂不到患者也挂不到 RIS 历史，比缺这一行更危险。
    // 值级失败仍保留 —— 运维要看到「除结构问题外，这条数值还缺单位」。
    let structure_broken = !failures.is_empty();
    outcome.failures.extend(failures);
    if structure_broken {
        outcome.items.clear();
    }

    Hl7Report {
        study_no,
        patient_id,
        study_date,
        message_type: msg
            .find(SEG_MSH)
            .and_then(|s| s.field(9))
            .map(str::trim)
            .unwrap_or("")
            .to_string(),
        findings_text,
        outcome,
    }
}

/// 拼各 `OBX` 段的 `OBX-5` 成所见文本，并记住每个字符在**原消息**中的行列。
///
/// 返回 `(所见文本, 逐字符位置)`：位置表的第 i 项是所见文本第 i 个字符
/// （1 基）在原消息中的 `(行, 列)`。
fn collect_findings(obx: &[&Segment], msg: &Hl7Message) -> (String, Vec<(usize, usize)>) {
    let mut text = String::new();
    let mut positions: Vec<(usize, usize)> = Vec::new();

    for seg in obx {
        let raw = match seg.field(5) {
            Some(v) if !v.trim().is_empty() => v,
            _ => continue,
        };
        let start_col = seg.field_column(5);
        if !text.is_empty() {
            // 段之间用换行分隔；换行本身也占一个位置（指向本段 OBX-5 的起始）。
            text.push('\n');
            positions.push((seg.line, start_col));
        }
        for (ch, offset) in decode_field(raw, msg.separators) {
            text.push(ch);
            positions.push((seg.line, start_col + offset));
        }
    }

    (text, positions)
}

/// 把解析结果的行列指回**原消息**。
///
/// 解析内核跑在「所见文本」上；HL7 报告的原文是整条消息 ——
/// 失败条目要能指到消息里的具体位置，运维才不用对着文本猜。
fn remap_positions(outcome: &mut ParseOutcome, positions: &[(usize, usize)]) {
    for item in &mut outcome.items {
        if let Some((line, column)) = position_of(positions, item.char_start) {
            item.line = line;
            item.column = column;
        }
    }
    for failure in &mut outcome.failures {
        if let Some((line, column)) = position_of(positions, failure.char_start) {
            failure.line = line;
            failure.column = column;
        }
    }
}

/// 所见文本第 `char_start` 个字符（1 基）在原消息中的行列。
fn position_of(positions: &[(usize, usize)], char_start: usize) -> Option<(usize, usize)> {
    if char_start == 0 {
        return None; // 位置未知，不编造
    }
    positions.get(char_start - 1).copied()
}

/// 取复合字段的**第一重复的第一组件**。
///
/// `PID-3` = `PID-EXAMPLE^^^HOSPITAL-MAIN^MR` → `PID-EXAMPLE`
/// （HL7 v2 CX：ID^校验位^assigning authority…）。
fn first_component(raw: Option<&str>, sep: &Separators) -> Option<String> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    let repetition = raw.split(sep.repetition).next().unwrap_or(raw);
    let component = repetition.split(sep.component).next().unwrap_or(repetition);
    let value = component.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// 缺段失败条目：锚点指到**本该跟在这段之后**的前一段（缺 `OBX` 时指 `OBR`）。
fn missing_segment_failure(msg: &Hl7Message, prefer: &str, snippet: &str) -> Failure {
    let line = msg
        .find(prefer)
        .map(|s| s.line)
        .or_else(|| msg.segments.last().map(|s| s.line))
        .unwrap_or(1);
    Failure::new(ErrorCode::Hl7SegmentMissing, 0, line, 1, snippet)
        .with_detail_code(DetailCode::Hl7SegmentAbsent)
}

/// 必需字段为空的失败条目：锚点指到该字段在本行中的起始列。
fn empty_field_failure(seg: &Segment, field_no: usize, snippet: &str) -> Failure {
    Failure::new(
        ErrorCode::Hl7SegmentMissing,
        0,
        seg.line,
        seg.field_column(field_no),
        snippet,
    )
    .with_detail_code(DetailCode::Hl7SegmentAbsent)
}

/// 消息按行切分。
///
/// HL7 v2 用 CR 作段分隔符；院内落地常见 CR / LF / CRLF 三种，一并接受。
/// 行号按**原始行序**计数（空行也占号），失败条目的行号才对得上原文。
fn split_lines(text: &str) -> Vec<(usize, &str)> {
    text.split(['\r', '\n'])
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(i, line)| (i + 1, line))
        .collect()
}

/// 按字段分隔符切字段。
fn split_fields(line: &str, sep: char) -> Vec<&str> {
    line.split(sep).collect()
}

/// 取段名：字段分隔符之前的 3 个字符。
fn segment_name(line: &str, field_sep: char) -> &str {
    match line.char_indices().nth(3) {
        Some((idx, ch)) if ch == field_sep => &line[..idx],
        _ => {
            let end = line.char_indices().nth(3).map(|(i, _)| i).unwrap_or(line.len());
            &line[..end]
        }
    }
}

/// 解 HL7 v2 转义序列，返回 `(解码字符, 在原字段内的 0 基字符偏移)`。
///
/// 只解标准序列：`\F\` `\S\` `\T\` `\R\` `\E\` `\.br\` `\X<十六进制>\`。
/// **未知转义原样保留**（不报错也不丢字）—— 猜错转义会让测量值与单位一起乱码，
/// 而乱码在台账里看起来仍然像一个数字。
fn decode_field(raw: &str, sep: Separators) -> Vec<(char, usize)> {
    let chars: Vec<char> = raw.chars().collect();
    let mut out: Vec<(char, usize)> = Vec::with_capacity(chars.len());
    let mut i = 0usize;

    while i < chars.len() {
        let c = chars[i];
        if c != sep.escape {
            out.push((c, i));
            i += 1;
            continue;
        }
        let end = match find_escape_end(&chars, i, sep.escape) {
            Some(e) => e,
            None => {
                out.push((c, i));
                i += 1;
                continue;
            }
        };
        let body: String = chars[i + 1..end].iter().collect();
        match decode_escape_body(&body, sep) {
            Some(decoded) => {
                // 解码出的字符都指向转义序列的起始位置。
                for ch in decoded.chars() {
                    out.push((ch, i));
                }
            }
            None => {
                // 未知转义：原样保留（不丢字）。
                for (k, ch) in chars[i..=end].iter().enumerate() {
                    out.push((*ch, i + k));
                }
            }
        }
        i = end + 1;
    }

    out
}

/// 转义序列的结束位置（下一个转义字符的下标）。
fn find_escape_end(chars: &[char], start: usize, escape: char) -> Option<usize> {
    (start + 1..chars.len()).find(|&j| chars[j] == escape)
}

/// 解一个转义序列体（不含首尾转义字符）。
fn decode_escape_body(body: &str, sep: Separators) -> Option<String> {
    match body {
        "F" => Some(sep.field.to_string()),
        "S" => Some(sep.subcomponent.to_string()),
        "T" => Some(sep.component.to_string()),
        "R" => Some(sep.repetition.to_string()),
        "E" => Some(sep.escape.to_string()),
        ".br" => Some("\n".to_string()),
        _ => {
            // `\X<十六进制字节>\`：按字节解码（多字节序列按 UTF-8 解）。
            let hex = body.strip_prefix('X').or_else(|| body.strip_prefix('x'))?;
            if hex.is_empty() || hex.len() % 2 != 0 {
                return None;
            }
            let mut bytes: Vec<u8> = Vec::with_capacity(hex.len() / 2);
            for pair in hex.as_bytes().chunks(2) {
                let s = std::str::from_utf8(pair).ok()?;
                bytes.push(u8::from_str_radix(s, 16).ok()?);
            }
            String::from_utf8(bytes).ok()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 样例目录（仓库内脱敏语料）。
    fn samples_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/samples")
    }

    fn read_sample(name: &str) -> String {
        std::fs::read_to_string(samples_dir().join(name)).expect("样例应可读")
    }

    /// 消息头行（照抄仓库样例，只换了控制号）。
    const MSH_LINE: &str =
        "MSH|^~\\&|ULTRASOUND-STATION|ULTRASOUND-ROOM|RIS|RIS-HOSPITAL|202603||ORU^R01|S-1|P|2.5.1";

    /// 按**字段号**拼一段（字段号 = HL7 v2 字段序号）。
    ///
    /// 手写管道符个数容易数错 —— 按字段号拼，位置与标准一一对应。
    fn seg(name: &str, fields: &[(usize, &str)]) -> String {
        seg_with('|', name, fields)
    }

    /// 按字段号拼一段（自定义分隔符）。
    fn seg_with(sep: char, name: &str, fields: &[(usize, &str)]) -> String {
        let max = fields.iter().map(|(n, _)| *n).max().unwrap_or(0);
        let mut slots: Vec<String> = vec![String::new(); max];
        for (n, v) in fields {
            slots[*n - 1] = v.to_string();
        }
        let mut s = String::from(name);
        for slot in slots {
            s.push(sep);
            s.push_str(&slot);
        }
        s
    }

    /// 一条完整的 `ORU^R01` 消息（字段值可覆盖）。
    fn message(segments: &[String]) -> String {
        segments.join("\r")
    }

    #[test]
    fn 三份脱敏样本的权威字段与人工标注一致() {
        let ann = std::fs::read_to_string(samples_dir().join("hl7_annotations.csv"))
            .expect("标注对照表应可读");
        let mut rows = 0usize;
        for line in ann.lines().skip(1) {
            if line.trim().is_empty() {
                continue;
            }
            let cols: Vec<&str> = line.split(',').collect();
            assert_eq!(cols.len(), 8, "标注表列数应固定：{}", line);
            let file = cols[0];
            let (exp_study, exp_patient, exp_type, exp_date) =
                (cols[1], cols[2], cols[3], cols[4]);
            let exp_findings = cols[5].replace("\\n", "\n");
            let exp_items: usize = cols[6].parse().expect("期望条目数应为整数");
            let exp_error = cols[7];

            let report = adapt(&read_sample(file));

            assert_eq!(
                report.study_no.as_deref(),
                Some(exp_study),
                "{}：检查号应取自 OBR-3",
                file
            );
            assert_eq!(
                report.patient_id.as_deref(),
                Some(exp_patient),
                "{}：病历号应取自 PID-3",
                file
            );
            assert_eq!(report.message_type, exp_type, "{}：消息类型应取自 MSH-9", file);
            assert_eq!(
                report.study_date.as_deref(),
                Some(exp_date),
                "{}：检查日期应取自 OBR-7",
                file
            );
            assert_eq!(
                report.findings_text, exp_findings,
                "{}：所见文本应等于 OBX-5 依序拼接",
                file
            );
            assert_eq!(
                report.outcome.items.len(),
                exp_items,
                "{}：解析条目数应与人工标注一致",
                file
            );
            if exp_error.is_empty() {
                assert!(
                    report.outcome.failures.is_empty(),
                    "{}：不应有失败条目，实际 {:?}",
                    file,
                    report.outcome.failures
                );
                assert!(report.is_complete(), "{}：消息结构应完整", file);
            } else {
                assert!(
                    report
                        .outcome
                        .failures
                        .iter()
                        .any(|f| f.code.as_str() == exp_error),
                    "{}：应产出错误码 {}",
                    file,
                    exp_error
                );
                assert!(!report.is_complete(), "{}：消息结构不完整", file);
            }
            rows += 1;
        }
        assert_eq!(rows, 3, "三份脱敏 ORU^R01 都应逐条对拍");
    }

    #[test]
    fn 缺观测结果段返回显式错误码且不产出条目() {
        let report = adapt(&read_sample("hl7/sample-hl7-03.hl7"));
        assert!(report.outcome.items.is_empty(), "缺 OBX 不得产出条目");
        assert_eq!(report.outcome.failures.len(), 1);
        let failure = &report.outcome.failures[0];
        assert_eq!(failure.code, ErrorCode::Hl7SegmentMissing);
        assert_eq!(failure.code.as_str(), "HL7_SEGMENT_MISSING");
        assert!(
            failure.raw_snippet.contains("OBX"),
            "错误信息应含缺失段名：{}",
            failure.raw_snippet
        );
        assert!(failure.line >= 1, "缺段失败也应带位置");
        assert_eq!(report.patient_id.as_deref(), Some("PID-EXAMPLE"));
    }

    #[test]
    fn 缺消息头段返回显式错误码() {
        let msg = message(&[
            seg("PID", &[(3, "PID-EXAMPLE")]),
            seg("OBR", &[(1, "1"), (3, "S-1"), (7, "202603")]),
            seg("OBX", &[(1, "1"), (2, "TX"), (3, "K^L"), (5, "左肾 3.2cm")]),
        ]);
        let report = adapt(&msg);
        assert!(report.outcome.items.is_empty());
        assert_eq!(report.outcome.failures.len(), 1);
        assert_eq!(report.outcome.failures[0].code, ErrorCode::Hl7SegmentMissing);
        assert!(report.outcome.failures[0].raw_snippet.contains("MSH"));
    }

    #[test]
    fn 缺患者标识段返回显式错误码() {
        let msg = message(&[
            MSH_LINE.to_string(),
            seg("OBR", &[(1, "1"), (3, "S-1"), (7, "202603")]),
            seg("OBX", &[(1, "1"), (2, "TX"), (3, "K^L"), (5, "左肾 3.2cm")]),
        ]);
        let report = adapt(&msg);
        assert!(report.outcome.items.is_empty());
        assert_eq!(report.outcome.failures.len(), 1);
        assert_eq!(report.outcome.failures[0].code, ErrorCode::Hl7SegmentMissing);
        assert!(report.outcome.failures[0].raw_snippet.contains("PID"));
    }

    #[test]
    fn 缺检查申请段返回显式错误码() {
        let msg = message(&[
            MSH_LINE.to_string(),
            seg("PID", &[(3, "PID-EXAMPLE")]),
            seg("OBX", &[(1, "1"), (2, "TX"), (3, "K^L"), (5, "左肾 3.2cm")]),
        ]);
        let report = adapt(&msg);
        assert!(report.outcome.items.is_empty());
        assert_eq!(report.outcome.failures.len(), 1);
        assert_eq!(report.outcome.failures[0].code, ErrorCode::Hl7SegmentMissing);
        assert!(report.outcome.failures[0].raw_snippet.contains("OBR"));
    }

    #[test]
    fn 所见文本喂进同一套解析内核产出同样的强转结果() {
        let report = adapt(&read_sample("hl7/sample-hl7-01.hl7"));
        assert_eq!(report.outcome.items.len(), 1);
        let item = &report.outcome.items[0];
        assert_eq!(item.part_key.as_deref(), Some("kidney.left"));
        assert_eq!(item.value_raw, Some(3.2));
        assert_eq!(item.unit_raw.as_deref(), Some("cm"));
        assert_eq!(item.value_si, Some(32.0));
        assert_eq!(item.unit_code.as_deref(), Some("mm"));
        assert_eq!(item.coercion_path.as_str(), "UnitConvert");
    }

    #[test]
    fn 多条观测结果段依序拼成所见文本() {
        let report = adapt(&read_sample("hl7/sample-hl7-02.hl7"));
        assert_eq!(
            report.findings_text,
            "左侧输尿管内径 0.6cm\n膀胱残余尿量 120ml"
        );
        assert_eq!(report.outcome.items.len(), 2);
        assert_eq!(
            report.outcome.items[0].part_key.as_deref(),
            Some("ureter.left")
        );
        assert_eq!(report.outcome.items[0].value_si, Some(6.0));
        assert_eq!(
            report.outcome.items[1].item_code.as_deref(),
            Some("bladder.residual_urine_volume")
        );
        assert_eq!(report.outcome.items[1].value_si, Some(120.0));
        assert_eq!(report.outcome.items[1].unit_code.as_deref(), Some("mL"));
    }

    #[test]
    fn 失败条目的行列指回原消息() {
        // OBX-5 里的数值缺单位 → 失败条目应指到**消息里** OBX 段那一行。
        let obx = seg("OBX", &[(1, "1"), (2, "TX"), (3, "KIDNEY^积水^L"), (5, "左肾积水 3.2")]);
        let msg = message(&[
            MSH_LINE.to_string(),
            seg("PID", &[(3, "PID-EXAMPLE")]),
            seg("OBR", &[(1, "1"), (3, "S-1"), (7, "202603")]),
            obx.clone(),
        ]);
        let report = adapt(&msg);
        assert!(report.outcome.items.is_empty());
        assert_eq!(report.outcome.failures.len(), 1);
        let failure = &report.outcome.failures[0];
        assert_eq!(failure.code, ErrorCode::UnitMissing);
        assert_eq!(failure.line, 4, "失败应指到第 4 行的 OBX 段");
        // 第 4 行原文里的「3」所在列 —— 失败条目就指在它身上。
        let expected_col = obx.chars().position(|c| c == '3').unwrap() + 1;
        assert_eq!(failure.column, expected_col);
    }

    #[test]
    fn 分隔符与编码字符从消息头取不写死() {
        // 厂商改用 '#' 作字段分隔符、'@' 作组件分隔符。
        let msh = "MSH#@~\\&#ULTRASOUND-STATION#ULTRASOUND-ROOM#RIS#RIS-HOSPITAL#202603##ORU^R01#S-1#P#2.5.1";
        let msg = message(&[
            msh.to_string(),
            seg_with('#', "PID", &[(3, "PID-EXAMPLE")]),
            seg_with('#', "OBR", &[(1, "1"), (3, "S-1"), (7, "202603")]),
            seg_with(
                '#',
                "OBX",
                &[(1, "1"), (2, "TX"), (3, "KIDNEY@积水@L"), (5, "左肾积水 3.2cm")],
            ),
        ]);
        let report = adapt(&msg);
        assert_eq!(report.study_no.as_deref(), Some("S-1"));
        assert_eq!(report.patient_id.as_deref(), Some("PID-EXAMPLE"));
        assert_eq!(report.outcome.items.len(), 1);
        assert_eq!(report.outcome.items[0].value_si, Some(32.0));
    }

    #[test]
    fn 转义序列解码且未知转义原样保留() {
        let msg = message(&[
            MSH_LINE.to_string(),
            seg("PID", &[(3, "PID-EXAMPLE")]),
            seg("OBR", &[(1, "1"), (3, "S-1"), (7, "202603")]),
            seg(
                "OBX",
                &[
                    (1, "1"),
                    (2, "TX"),
                    (3, "KIDNEY^积水^L"),
                    (5, "左肾积水 3.2cm\\F\\未知\\Q\\"),
                ],
            ),
        ]);
        let report = adapt(&msg);
        // `\F\` → `|`（字段分隔符），未知转义 `\Q\` 原样保留（不丢字）。
        assert!(
            report.findings_text.contains('|'),
            "\\F\\ 应解码成字段分隔符：{}",
            report.findings_text
        );
        assert!(report.findings_text.contains("\\Q\\"));
        assert_eq!(report.outcome.items.len(), 1);
        assert_eq!(report.outcome.items[0].value_si, Some(32.0));
    }

    #[test]
    fn 投递目录读取缺文件时报错不静默() {
        let dir = std::env::temp_dir().join(format!("urs-hl7-missing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(matches!(read_message(&dir), Err(Hl7Error::Io(_))));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn gbk消息按gbk解出正文() {
        let dir = std::env::temp_dir().join(format!("urs-hl7-gbk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // GBK 的「左肾」字节（CP936：D7F3 C9F6）：不是合法 UTF-8，但按 GBK 解得出来。
        std::fs::write(dir.join(REPORT_HL7), [0xD7, 0xF3, 0xC9, 0xF6]).unwrap();
        let message = read_message(&dir).unwrap();
        assert_eq!(message.encoding, crate::ingest::encoding::Encoding::Gbk);
        assert_eq!(message.text, "左肾");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 编码判不出时显式失败不猜转码() {
        let dir = std::env::temp_dir().join(format!("urs-hl7-garbage-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(REPORT_HL7), [0xFF, 0xFE, 0x00, 0x01]).unwrap();
        match read_message(&dir) {
            Err(Hl7Error::Encoding(e)) => {
                assert_eq!(
                    e.error_code(),
                    crate::parse::errors::ErrorCode::EncodingUnknown
                );
            }
            other => panic!("应显式报编码失败，实际：{:?}", other),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 源目录读取后不产生任何中间文件() {
        let dir = std::env::temp_dir().join(format!("urs-hl7-noio-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let body = read_sample("hl7/sample-hl7-01.hl7");
        std::fs::write(dir.join(REPORT_HL7), &body).unwrap();
        let before: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        let message = read_message(&dir).unwrap();
        assert!(!message.text.is_empty());
        let after: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(before, after, "源目录不得出现转码副本或临时文件");
        std::fs::remove_dir_all(&dir).ok();
    }
}
