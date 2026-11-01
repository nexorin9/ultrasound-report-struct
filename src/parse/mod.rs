//! 解析内核：词法扫描、前瞻合并、抽取与字符光标定位。
//!
//! 主路径（对应源产品的「每条输入一个解析入口 + 按模式播种」）：
//! `parse_report(text) → ParseOutcome`
//! —— 文本 → 词法 token 流 → 前瞻合并 → 抽取 → 结构化条目 + 失败条目。
//!
//! 与源产品同构之处：**一条输入一个入口**（HL7 报告同样走这一个入口，见 Task 10），
//! 中间层不另开抽取路径，避免两套口径。

pub mod errors;
pub mod extract;
pub mod extract_items;
pub mod extract_parts;
pub mod extract_values;
pub mod gate;
pub mod lexer;
pub mod locate;
pub mod lookahead;

pub use errors::{DetailCode, Disposition, ErrorCode, Failure, FailurePolicy};
pub use extract::{item_code_of, ParseOutcome, ParsedItem};
pub use extract_items::{item_candidates, resolve_item_code, ItemCandidate, ItemResolution};
pub use extract_parts::{extract_parts, PartHit};
pub use extract_values::{
    classify_values, parse_measurement_value, unit_missing_failure, value_unparseable_failure,
    ValueDefect, ValueHit, ValueParse, ValueVerdict,
};
pub use gate::{classify_document, DocumentKind};
pub use lexer::{tokenize, Token, TokenKind};
pub use locate::{char_index_at, locate, snippet_at};
pub use lookahead::{
    merge, merge_rules, merge_side, part_key_of, resolve_overlap, rule_priority, side_of, Merged,
    MergeRule, MergeRuleKind, OverlapWinner, PartRef, Side,
};

/// **解析入口**：一条输入 → 一个入口 → 一份产出。
///
/// 机制溯源（**改写**，不复制源代码）：源产品的解析驱动层把每条输入交给**同一个**
/// 解析入口（`raw_parser`），入口内部再按模式播种不同解析路径；
/// 本项目把同一契约落到中文超声报告上 —— **TXT 与 HL7 两条入站通道共用这一个入口**，
/// 不另开第二条抽取路径（两套口径必然漂移）。
///
/// 入口内部三步：
/// 1. **文档闸门**（[`gate::classify_document`]）：不是中文超声报告就整段不解析；
/// 2. 词法扫描（[`tokenize`]）→ 前瞻合并（[`merge`]）→ 抽取（[`extract::extract`]）；
/// 3. 产出结构化条目 + 失败条目（带 1 基行列）。
pub fn raw_parse(text: &str) -> ParseOutcome {
    // **词典作用域**：解析期间固定用同一代词典 —— 运维在解析途中热更新词典，
    // 这一份报告不会前半段旧口径、后半段新口径（口径留痕要说得清）。
    let _scope = crate::coerce::dictionary_lifecycle::scope_current();
    let kind = classify_document(text);
    if !kind.should_parse() {
        // 不适用 ≠ 解析失败：0 条目、0 失败条目，判定结果由 `doc_kind` 带着走。
        return ParseOutcome {
            items: Vec::new(),
            failures: Vec::new(),
            doc_kind: kind,
            dictionary_version: crate::dictionary_version(),
        };
    }
    let tokens = tokenize(text);
    let merged = merge(&tokens);
    let mut outcome = extract::extract(&merged, text);
    outcome.doc_kind = kind;
    outcome
}

/// 解析一份报告正文，产出结构化条目与失败条目。
///
/// 与 [`raw_parse`] 同一实现 —— 保留这个名字是为了让既有调用点（入站适配、
/// 导出、守护处理器）继续用同一份口径，**不另开一条路径**。
pub fn parse_report(text: &str) -> ParseOutcome {
    raw_parse(text)
}
