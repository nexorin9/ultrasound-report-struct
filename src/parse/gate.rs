//! 解析内核第 8 层配套：**文档闸门** —— 先判「这份文本是不是本产品要处理的报告」。
//!
//! **为什么要有这道闸门**：超声工作站把报告投进 `inbox/` 之后，目录里偶尔会混进
//! **不是超声报告**的文件 —— 英文模板导出的文本、别的科室的 PDF 抽取文本、
//! 一张 CSV 导出表。这些文本里有大量「数字 + 拉丁词」的组合
//! （`17 Years` / `60 s` / `11 institutions` / `2.5 mm`），而词法层把**任何**拉丁字母串
//! 都当作单位候选 —— 于是解析器会把它们当成测量值抽出来，在台账里留下一行行
//! **看着像数字、其实不是测值**的垃圾行。
//!
//! **闸门口径**：正文里**一个汉字都没有** → 不是中文报告 → **整段不解析**
//! （0 条目、0 失败条目）。这不是「解析失败」，是「不适用」——
//! 失败清单是医师的工作清单，塞进一堆不相干的数字只会稀释真正要看的那几行。
//!
//! **判据为什么落在语种上**：本产品接的是**中文**超声工作站投递目录；
//! 中文报告里跟在数字后面的拉丁词面几乎必然是单位（`cm` / `mm` / `mL`），
//! 而英文叙述句里同样的位置是普通单词（`Years` / `s` / `institutions`）。
//! 语种是这个区分**最省事也最不含糊**的判据 —— 不靠猜，也不靠维护英文停用词表。
//!
//! **已知边界（不在本轮闸门覆盖内）**：**中英混排**文本（中文抬头 + 英文正文）
//! 会被判为中文报告并进入解析；英文正文里的数值仍可能按值级准入规则处理。
//! 这一点写在 `docs/negatives/recall_report.md` 与负样例集说明里，不藏着。

use crate::parse::lexer::is_han;

/// 文档类别（闸门判定结果）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DocumentKind {
    /// 超声报告（可解析）。
    #[default]
    UltrasoundReport,
    /// 非中文文本：整段不解析（**不是**解析失败，是不适用）。
    NotChinese,
    /// 空文本。
    Empty,
}

impl DocumentKind {
    /// 类别字符串（进日志与对照报告）。
    pub fn as_str(self) -> &'static str {
        match self {
            DocumentKind::UltrasoundReport => "ULTRASOUND_REPORT",
            DocumentKind::NotChinese => "NOT_CHINESE",
            DocumentKind::Empty => "EMPTY",
        }
    }

    /// 中文说明（进日志与对照报告）。
    pub fn message(self) -> &'static str {
        match self {
            DocumentKind::UltrasoundReport => "中文超声报告，进入解析",
            DocumentKind::NotChinese => "正文无汉字，不是中文超声报告，整段不解析",
            DocumentKind::Empty => "正文为空，无可解析内容",
        }
    }

    /// 这份文档是否应当进入解析。
    pub fn should_parse(self) -> bool {
        matches!(self, DocumentKind::UltrasoundReport)
    }
}

/// 正文里是否含汉字。
fn has_han(text: &str) -> bool {
    text.chars().any(is_han)
}

/// 判定一份报告正文的类别（文档闸门）。
///
/// **三态**：空文本 / 非中文文本 / 中文超声报告。
/// 判据只看**语种**（有没有汉字），不猜内容 —— 猜内容会引出一整套
/// 「哪些词算超声语境词」的清单，而那份清单必然随词典一起漂移。
pub fn classify_document(text: &str) -> DocumentKind {
    if text.trim().is_empty() {
        return DocumentKind::Empty;
    }
    if !has_han(text) {
        return DocumentKind::NotChinese;
    }
    DocumentKind::UltrasoundReport
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 中文报告判为可解析() {
        assert_eq!(
            classify_document("超声所见\n左肾积水 3.2cm"),
            DocumentKind::UltrasoundReport
        );
        assert!(classify_document("超声所见\n左肾积水 3.2cm").should_parse());
    }

    #[test]
    fn 纯英文文本判为非中文且不解析() {
        let kind = classify_document("FINDINGS\nThe lesion measured 2.5 mm in size.");
        assert_eq!(kind, DocumentKind::NotChinese);
        assert!(!kind.should_parse());
    }

    #[test]
    fn 空白与空文本判为空() {
        assert_eq!(classify_document(""), DocumentKind::Empty);
        assert_eq!(classify_document("   \n\t "), DocumentKind::Empty);
        assert!(!classify_document("").should_parse());
    }

    #[test]
    fn 数字与拉丁字母不算汉字() {
        // 纯数字 + 单位：没有汉字 → 非中文（不是超声报告）。
        assert_eq!(
            classify_document("3.2cm 45mm"),
            DocumentKind::NotChinese
        );
    }

    #[test]
    fn 含汉字的混排文本判为中文报告() {
        // 已知边界：中文抬头 + 英文正文 → 进解析（不靠语种以外的猜测）。
        assert_eq!(
            classify_document("姓名：张三\nFINDINGS\nThe lesion measured 2.5 mm."),
            DocumentKind::UltrasoundReport
        );
    }

    #[test]
    fn 生僻字扩展区也算汉字() {
        // 扩展 A 的「㐂」：是汉字 → 中文文档（与词法层同一套判定，不各写一份）。
        assert_eq!(classify_document("㐂㐂"), DocumentKind::UltrasoundReport);
    }
}
