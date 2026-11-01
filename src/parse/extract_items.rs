//! 解析内核第 4 层：测项抽取与 item_code 归一（漏项不静默丢弃）。
//!
//! **这一层解决什么**：测项码与部位键共同构成对拍用的**规范键**
//! （`(patient_id, part_key, item_code)`）。部位归一了、测项没归一，
//! 对拍照样对不上 —— 「左肾长 85mm」与「左肾长度 9.5cm」是同一个测量，
//! 必须落到同一个 `item_code`（`kidney.length`），否则历史值会互相比较。
//!
//! **三条纪律**：
//!
//! 1. **词典是唯一真源** —— 测项词面 → 测项码后缀由测项词典决定
//!    （[`crate::coerce::dictionary`]），合并层与抽取层走同一张表，
//!    不各带一份映射（两处真源必然漂移）。
//! 2. **查不到就不猜** —— 测项词在词典里查不到（没收录，或该部位没这个
//!    测项口径）时产出**显式失败条目**（`ITEM_DICT_MISSING`，带 1 基行列），
//!    **不静默丢弃测项文本**：运维一眼知道该补哪个词、报告里哪一列是漏项。
//! 3. **多种写法归一** —— 「长」「长度」都归到 `length`；
//!    归一的是**测项后缀**，部位基名由部位层给 —— 两侧各管一段。
//!
//! **两种「没有测项码」不是一回事**（运维补词典的动作也不同）：
//!
//! - **词典缺口**（[`ItemResolution::dict_miss`] = `true`）：测项词**在报告里写了**，
//!   但词典里查不到这个词、或这个词不适用于该部位（残余尿量是膀胱的测量，
//!   不是肾的测量）。→ **产出失败条目**，指回**测项词自身**，
//!   运维照着补词典即可。
//! - **部位没归一**：合成键缺一部就不成立 —— 那是**部位**那条失败
//!   （`PART_UNRESOLVED`）已经交代过的事，**不重复报一次测项未命中**。
//!
//! **机制溯源（改写，不复制源代码）**：源产品的解析驱动层用 `mode_token` /
//! `mode_seed` 按解析模式**播种**首个前瞻 token，让后续 token 与模式对齐；
//! 对齐不上的 token 不会被静默接受。本项目把同一思路改写为中文报告词表：
//! 合并后的 token 流按测项词典查表，产出 `item_code` 与**原文位置**；
//! 查不到即显式失败条目（带行列），不静默丢弃。

use super::errors::{ErrorCode, Failure};
use super::lexer::TokenKind;
use super::locate::locate;
use super::lookahead::{Merged, PartRef};

use crate::coerce::dictionary::{item_entry_of, item_suffix_of};

/// 一条测项候选（词典命中与未命中都产出 —— 未命中也是一条要交代的结果）。
#[derive(Clone, Debug)]
pub struct ItemCandidate {
    /// 测项别名（词典的键，如「前后径」「长」）。
    pub alias: String,
    /// 1 基字符光标（**测项词自身**的起点，不是合并 token 的起点）。
    ///
    /// 合并 token 继承的是**部位词**的位置；失败条目要指回**测项词**
    /// （运维照着补词典），所以测项词自己的位置必须单独留着。
    pub start: usize,
    /// 绑定部位（合并时携带的部位；独立测项词为 `None`）。
    pub part: Option<PartRef>,
}

/// 测项解析结果。
#[derive(Clone, Debug)]
pub struct ItemResolution {
    /// 规范测项码（如 `kidney.ap_diameter`）；未命中或部位缺一部为 `None`。
    pub item_code: Option<String>,
    /// 测项码后缀（词典查到时给出）。
    pub suffix: Option<String>,
    /// 测项词面（查表用的键；失败条目回显它）。
    pub alias: String,
    /// 测项词自身的 1 基字符光标（失败条目指回这里）。
    pub start: usize,
    /// 是否**真的**没命中测项词典。
    ///
    /// `true` = 词面没收录，或词面收录了但**该部位没这个测项口径** ——
    /// 两种都要补词典，产出 `ITEM_DICT_MISSING` 失败条目。
    ///
    /// `false` 且 `item_code` 为 `None` = **部位没归一**（合成键缺一部），
    /// 那是部位那条失败已经交代过的事，**不重复报测项未命中**。
    pub dict_miss: bool,
}

impl ItemResolution {
    /// 测项码是否成立（部位 + 测项都归一了）。
    pub fn is_resolved(&self) -> bool {
        self.item_code.is_some()
    }
}

/// 从合并后的 token 里取测项候选（部位按「自带部位 → 上下文部位」绑定）。
///
/// **绑定口径**：合并出的测项短语自带部位；独立测项词用上下文部位
/// （`当前测项词之前最近的部位`）—— 与抽取层的部位继承口径一致。
pub fn item_candidate(m: &Merged, context_part: Option<&PartRef>) -> Option<ItemCandidate> {
    match m {
        Merged::Item {
            alias,
            alias_start,
            part,
            ..
        } => Some(ItemCandidate {
            alias: alias.clone(),
            start: *alias_start,
            part: part.clone(),
        }),
        Merged::Raw(t) if t.kind == TokenKind::Item => Some(ItemCandidate {
            alias: t.text.clone(),
            start: t.start,
            part: context_part.cloned(),
        }),
        _ => None,
    }
}

/// 从合并后的 token 序列抽取测项候选（**每次出现都保留，不去重**）。
///
/// 同一部位同一测项在报告里出现两次（`左肾前后径 2.1cm` 与
/// `左肾前后径 2.3cm`）是**两条记录**，各有各的位置 —— 去重会让后一次
/// 测量凭空消失，台账少一行，医师看不到「这次测的值跟上次不一样」。
pub fn item_candidates(merged: &[Merged]) -> Vec<ItemCandidate> {
    let mut out: Vec<ItemCandidate> = Vec::new();
    let mut context_part: Option<PartRef> = None;

    for m in merged {
        match m {
            Merged::Part(p) => context_part = Some(p.clone()),
            Merged::Item { part: Some(p), .. } => context_part = Some(p.clone()),
            _ => {}
        }
        if let Some(c) = item_candidate(m, context_part.as_ref()) {
            out.push(c);
        }
    }

    out
}

/// 按（测项词面 × 部位基名）解析测项码。
///
/// **部位基名缺失 → 不产出测项码**（合成键缺一部就不成立），
/// 但这**不是**词典未命中 —— 部位那条失败（`PART_UNRESOLVED`）已经交代了，
/// 这里不再重复报一次测项未命中（`dict_miss` = `false`）。
///
/// **词典未命中**有两种：词面没收录；词面收录了但**该部位没这个测项口径**
/// （「残余尿量」是膀胱的测量，不是肾的测量）。两种都算 `dict_miss`，
/// 都产出失败条目。
///
/// `start` 是测项词自身的 1 基字符光标 —— 失败条目指回**那个词**。
pub fn resolve_item_code(
    alias: &str,
    part_base: Option<&str>,
    start: usize,
) -> ItemResolution {
    let part_base = match part_base {
        Some(b) if !b.is_empty() => b,
        _ => {
            // 部位没归一：测项口径无从判定 —— 不算词典未命中（部位那条已报）。
            return ItemResolution {
                item_code: None,
                suffix: None,
                alias: alias.to_string(),
                start,
                dict_miss: false,
            };
        }
    };

    match item_suffix_of(alias, Some(part_base)) {
        Some(suffix) => ItemResolution {
            item_code: Some(format!("{}.{}", part_base, suffix)),
            suffix: Some(suffix),
            alias: alias.to_string(),
            start,
            dict_miss: false,
        },
        None => ItemResolution {
            item_code: None,
            suffix: None,
            alias: alias.to_string(),
            start,
            dict_miss: true,
        },
    }
}

/// 测项词是否在词典里收录（与部位无关）。
///
/// 用于区分「词面根本没收录」与「词面收录但该部位不适用」——
/// 两者都未命中，但运维补词典时的动作不同。
pub fn alias_known(alias: &str) -> bool {
    item_entry_of(alias).is_some()
}

/// 测项词典未命中 → 显式失败条目（指回**测项词自身**的位置）。
///
/// 运维要补哪个词、报告里哪一列是漏项，一眼就知道；
/// **不静默丢弃测项文本**（丢了就没人知道漏抄了）。
///
/// 位置不可确定时按 [`super::locate::locate`] 的口径返回 `(0, 0)`（不编造）。
pub fn dict_missing_failure(candidate: &ItemCandidate, text: &str) -> Failure {
    let (line, column) = locate(text, candidate.start);
    Failure::new(
        ErrorCode::ItemDictMissing,
        candidate.start,
        line,
        column,
        &candidate.alias,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::lexer::tokenize;
    use crate::parse::lookahead::merge;

    /// 合并后的 token 序列（用例入口）。
    fn merged(text: &str) -> Vec<Merged> {
        merge(&tokenize(text))
    }

    /// 测项候选（用例入口）。
    fn candidates(text: &str) -> Vec<ItemCandidate> {
        item_candidates(&merged(text))
    }

    /// 测项码（用例入口）：部位基名由部位层给出。
    fn resolve(text: &str) -> Vec<ItemResolution> {
        candidates(text)
            .into_iter()
            .map(|c| {
                let base = c
                    .part
                    .as_ref()
                    .and_then(|p| p.base.clone())
                    .unwrap_or_else(|| "kidney".to_string());
                resolve_item_code(&c.alias, Some(&base), c.start)
            })
            .collect()
    }

    #[test]
    fn 肾长肾宽肾盂前后径残余尿量各一例() {
        // 肾长
        let a = &resolve("左肾长 8.5cm")[0];
        assert_eq!(a.item_code.as_deref(), Some("kidney.length"));
        assert_eq!(a.suffix.as_deref(), Some("length"));
        assert!(!a.dict_miss);

        // 肾宽
        let b = &resolve("左肾宽 4.3cm")[0];
        assert_eq!(b.item_code.as_deref(), Some("kidney.width"));

        // 肾盂前后径（部位基名是 renal_pelvis，不与肾串用）
        let c = &resolve("左肾盂前后径 2.1cm")[0];
        assert_eq!(c.item_code.as_deref(), Some("renal_pelvis.ap_diameter"));

        // 残余尿量（膀胱的测项）
        let d = &resolve("膀胱残余尿量 120ml")[0];
        assert_eq!(
            d.item_code.as_deref(),
            Some("bladder.residual_urine_volume")
        );
    }

    #[test]
    fn 同一测项的多种写法归一到同一测项码() {
        // 「长」「长度」是同一个测量：写法不同，测项码必须相同，
        // 否则同一侧的历史值比不到一起（看起来是两条互不相干的记录）。
        assert_eq!(
            resolve("左肾长 8.5cm")[0].item_code,
            resolve("左肾长度 8.5cm")[0].item_code
        );
        assert_eq!(
            resolve("左肾长 8.5cm")[0].item_code.as_deref(),
            Some("kidney.length")
        );
        assert_eq!(
            resolve("左肾宽 4.3cm")[0].item_code,
            resolve("左肾宽度 4.3cm")[0].item_code
        );
        assert_eq!(
            resolve("左肾宽 4.3cm")[0].item_code.as_deref(),
            Some("kidney.width")
        );
    }

    #[test]
    fn 重复测项两条都保留且位置不同() {
        // 同一部位同一测项出现两次是**两条记录**：去重会让后一次测量凭空消失，
        // 台账少一行，医师看不到「这次测的值跟上次不一样」。
        let cands = candidates("左肾前后径 2.1cm\n左肾前后径 2.3cm");
        assert_eq!(cands.len(), 2, "重复测项不得被去重");
        assert_eq!(cands[0].alias, "前后径");
        assert_eq!(cands[1].alias, "前后径");
        assert_ne!(cands[0].start, cands[1].start, "两条各带各的位置");
        assert_eq!(cands[0].start, 3);
        // 第二行：第 1 行 11 个字符 + 换行 1 个 → 「左肾」从 13 起，「前后径」从 15 起。
        assert_eq!(cands[1].start, 15);
    }

    #[test]
    fn 测项词位置指回测项词自身而非部位词() {
        // 合并 token 继承的是部位词的位置；失败条目要指回测项词，
        // 运维照着补词典才知道该补哪个词。
        let cands = candidates("左肾前后径 2.1cm");
        assert_eq!(cands[0].start, 3, "「前后径」在第 3 个字符");
        assert_eq!(cands[0].part.as_ref().unwrap().start, 1, "部位词在第 1 个字符");
    }

    #[test]
    fn 词典未命中标词典缺口且不产出测项码() {
        // 「残余尿量」是膀胱的测项，不是肾的：合成键不成立 → 词典缺口。
        let r = resolve_item_code("残余尿量", Some("kidney"), 3);
        assert!(r.dict_miss, "该部位不适用也算词典缺口");
        assert_eq!(r.item_code, None);
        assert_eq!(r.alias, "残余尿量");
        assert_eq!(r.start, 3);

        // 词面根本没收录：同样是词典缺口。
        let r2 = resolve_item_code("积水深度", Some("kidney"), 3);
        assert!(r2.dict_miss);
        assert_eq!(r2.item_code, None);
    }

    #[test]
    fn 部位没归一时不算词典缺口() {
        // 合成键缺一部就不成立 —— 那是部位那条失败已经交代过的事，
        // 不重复报一次测项未命中。
        let r = resolve_item_code("前后径", None, 3);
        assert!(!r.dict_miss, "部位没归一不是词典缺口");
        assert_eq!(r.item_code, None);
    }

    #[test]
    fn 词面收录与否可分辨() {
        // 运维补词典时的动作不同：收录了但部位不适用 vs 词面没收录。
        assert!(alias_known("残余尿量"));
        assert!(alias_known("前后径"));
        assert!(!alias_known("积水"));
        assert!(!alias_known("厚度"));
    }

    #[test]
    fn 未命中失败条目指回测项词并带行列() {
        let text = "超声所见\n膀胱前后径 2.1cm";
        let cands = candidates(text);
        let c = &cands[0];
        let failure = dict_missing_failure(c, text);
        assert_eq!(failure.code, ErrorCode::ItemDictMissing);
        assert_eq!(failure.raw_snippet, "前后径");
        assert_eq!(failure.line, 2, "第 2 行");
        assert_eq!(failure.column, 3, "「前后径」在第 3 列");
        assert_eq!(failure.char_start, c.start);
    }

    #[test]
    fn 独立测项词取上下文部位() {
        // 「膀胱 残余尿量」：测项词自身不带部位时用上下文部位。
        let cands = candidates("膀胱 残余尿量 120ml");
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].alias, "残余尿量");
        assert_eq!(cands[0].part.as_ref().unwrap().base.as_deref(), Some("bladder"));
    }
}
