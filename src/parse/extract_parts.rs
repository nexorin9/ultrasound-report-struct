//! 解析内核第 3 层：部位抽取与规范部位键归一（对拍匹配的前提）。
//!
//! **这一层解决什么**：「同一患者同侧部位」要能自动对上，部位写法就必须先归一 ——
//! 「左肾」「左侧肾脏」「左肾脏」三种写法必须落到同一个规范部位键 `kidney.left`，
//! 否则对拍会拿左肾去比右肾、拿双侧去比单侧 —— 那类差值**看起来正常，实际比错**。
//!
//! **两条纪律**：
//!
//! 1. **词典是唯一真源** —— 部位词面 → 规范解剖基名由部位词典决定
//!    （[`crate::coerce::dictionary`]），合并层与抽取层走同一张表，
//!    不各带一份映射（两处真源必然漂移）。
//! 2. **查不到就不猜** —— 词典外的部位词（如「胆囊」）产出**未命中**：
//!    规范部位键为空 + 一条 `PART_UNRESOLVED` 失败条目，**不参与后续对拍**。
//!    宁可漏一条，也不要一条对错了对象的差值。
//!
//! **未命中指回哪里**（两种未命中，指的位置不同，各有道理）：
//!
//! - **部位候选词存在、词典查不到**（如「胆囊」）：失败条目指向**部位词自身**，
//!   片段是部位词面 —— 运维要补词典时，一眼知道该补哪个词。
//! - **报告里根本没有部位候选词**（如「肝内胆管」这种词法层就没认出来的写法）：
//!   失败条目指向**测量值**，片段是原值词面 —— 运维要判断的是「这个值没有归属」。
//!
//! 两种情况都**不产出部位键**，也**不产出测项码**（合成键缺一部就不成立）——
//! 这是「宁可不比不错比」的代码落点：关键实体绑定不成立时，这一行不参与对拍。
//!
//! **侧别归一**：侧别由报告原文的修饰词决定（「左」「左侧」「右」「右侧」「双」「双侧」），
//! **不烧进部位词面** —— 「双侧肾」合并成词面后就分不清双侧还是单侧了。

use super::lexer::TokenKind;
use super::lookahead::{merge_side, part_key_of, side_of, Merged, PartRef, Side};

/// 一条部位候选（词典命中与未命中都产出 —— 未命中也是一条要交代的结果）。
#[derive(Clone, Debug)]
pub struct PartHit {
    /// 规范部位键（如 `kidney.left`）；**词典未命中为 `None`**。
    pub part_key: Option<String>,
    /// 规范解剖基名（如 `kidney`）；词典未命中为 `None`。
    pub base: Option<String>,
    /// 侧别。
    pub side: Side,
    /// 部位词面（**不含**侧别修饰词，如「肾脏」）—— 词典的键。
    pub raw_part: String,
    /// 合并后的词面（含侧别修饰词，如「左肾」）—— 原文回显用。
    pub text: String,
    /// 1 基字符光标（部位词起点）。
    pub start: usize,
    /// 是否命中部位词典。
    pub resolved: bool,
}

impl PartHit {
    /// 部位是否命中词典（等价于 `part_key.is_some()`，读起来更直白）。
    pub fn is_resolved(&self) -> bool {
        self.resolved
    }
}

/// 按合并后的 token 序列抽取部位候选。
///
/// 侧别取「部位自带侧别 → 前置修饰词侧别」的顺序（[`merge_side`]）；
/// 词典查得到就出规范部位键，查不到就出未命中 —— **两种都产出**，不静默丢弃。
pub fn extract_parts(merged: &[Merged]) -> Vec<PartHit> {
    let mut hits: Vec<PartHit> = Vec::new();
    let mut pending_side: Option<Side> = None;

    for m in merged {
        match m {
            Merged::Part(p) => {
                let side = merge_side(p.side, pending_side);
                pending_side = None;
                hits.push(hit_of(p, side));
            }
            // 「部位 + 测项」合并出的测项短语自带部位 —— 部位候选同样要落一条。
            Merged::Item { part: Some(p), .. } => {
                let side = merge_side(p.side, pending_side);
                pending_side = None;
                hits.push(hit_of(p, side));
            }
            Merged::Raw(t) if t.kind == TokenKind::Modifier => {
                pending_side = side_of(&t.text);
            }
            _ => {}
        }
    }
    hits
}

/// 部位引用 → 部位候选（词典命中与未命中都在这里定型）。
fn hit_of(p: &PartRef, side: Side) -> PartHit {
    let resolved_ref = PartRef {
        side,
        ..p.clone()
    };
    PartHit {
        part_key: part_key_of(&resolved_ref),
        base: resolved_ref.base.clone(),
        side,
        raw_part: p.raw_part.clone(),
        text: p.text.clone(),
        start: p.start,
        resolved: resolved_ref.base.is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::lexer::tokenize;
    use crate::parse::lookahead::merge;

    /// 合并 + 抽部位（用例入口）。
    fn parts(text: &str) -> Vec<PartHit> {
        extract_parts(&merge(&tokenize(text)))
    }

    /// 第一个部位候选的规范键。
    fn key_of(text: &str) -> Option<String> {
        parts(text).into_iter().next().map(|h| h.part_key.expect("应命中词典"))
    }

    #[test]
    fn 左肾归一到左肾键() {
        let hit = &parts("左肾 3.2cm")[0];
        assert_eq!(hit.part_key.as_deref(), Some("kidney.left"));
        assert_eq!(hit.base.as_deref(), Some("kidney"));
        assert_eq!(hit.side, Side::Left);
        assert_eq!(hit.raw_part, "肾");
        assert_eq!(hit.text, "左肾");
        assert!(hit.is_resolved());
    }

    #[test]
    fn 右肾归一到右肾键() {
        let hit = &parts("右肾 3.2cm")[0];
        assert_eq!(hit.part_key.as_deref(), Some("kidney.right"));
        assert_eq!(hit.side, Side::Right);
    }

    #[test]
    fn 双侧肾归一到双侧键() {
        let hit = &parts("双侧肾积水 2.6cm")[0];
        assert_eq!(hit.part_key.as_deref(), Some("kidney.bilateral"));
        assert_eq!(hit.side, Side::Bilateral);
    }

    #[test]
    fn 双肾与双侧肾归一到同一键() {
        // 两种写法都是双侧 —— 写法不同，键必须相同。
        assert_eq!(key_of("双肾积水 2.6cm"), Some("kidney.bilateral".to_string()));
        assert_eq!(
            key_of("双侧肾积水 2.6cm"),
            Some("kidney.bilateral".to_string())
        );
    }

    #[test]
    fn 膀胱无侧别键() {
        let hit = &parts("膀胱 2.5cm³")[0];
        assert_eq!(hit.part_key.as_deref(), Some("bladder"));
        assert_eq!(hit.side, Side::None);
    }

    #[test]
    fn 输尿管左右各一例() {
        assert_eq!(
            key_of("左侧输尿管内径 0.6cm"),
            Some("ureter.left".to_string())
        );
        assert_eq!(
            key_of("右侧输尿管内径 0.7cm"),
            Some("ureter.right".to_string())
        );
    }

    #[test]
    fn 别名写法归一到同一部位键() {
        // 「左肾」「左侧肾脏」「左肾脏」三种写法 → 同一个 kidney.left。
        let expected = Some("kidney.left".to_string());
        assert_eq!(key_of("左肾 3.2cm"), expected);
        assert_eq!(key_of("左侧肾脏 3.2cm"), expected);
        assert_eq!(key_of("左肾脏 3.2cm"), expected);
    }

    #[test]
    fn 肾盂与肾盏是独立基名() {
        // 肾盂前后径与肾前后径是不同测量：合成同一基名会让历史值互相比较。
        assert_eq!(key_of("左肾盂前后径 2.1cm"), Some("renal_pelvis.left".to_string()));
        assert_eq!(key_of("左肾盏 1.2cm"), Some("renal_calyx.left".to_string()));
        assert_eq!(key_of("左肾前后径 2.1cm"), Some("kidney.left".to_string()));
    }

    #[test]
    fn 词典外部位词产出未命中且不带部位键() {
        // 「胆囊」是腹部器官，不在本产品承诺的泌尿系口径内：未命中，不猜部位。
        let hit = &parts("胆囊 3.2cm")[0];
        assert_eq!(hit.part_key, None);
        assert_eq!(hit.base, None);
        assert!(!hit.is_resolved());
        assert_eq!(hit.raw_part, "胆囊");
        assert_eq!(hit.start, 1);
    }

    #[test]
    fn 未命中部位不带侧别键() {
        // 未命中时**不产出任何形式的部位键**（哪怕词面看着像部位）。
        let hit = &parts("左侧胆囊 3.2cm")[0];
        assert_eq!(hit.part_key, None);
        assert_eq!(hit.side, Side::Left, "侧别仍如实记录，供人工判断");
    }

    #[test]
    fn 部位与测项合并出的测项短语同样落部位候选() {
        let hits = parts("左肾前后径 2.1cm");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].part_key.as_deref(), Some("kidney.left"));
        assert_eq!(hits[0].raw_part, "肾");
    }

    #[test]
    fn 多个部位候选按出现顺序落全() {
        let hits = parts("左肾 3.2cm\n右肾 2.8cm");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].part_key.as_deref(), Some("kidney.left"));
        assert_eq!(hits[1].part_key.as_deref(), Some("kidney.right"));
    }

    #[test]
    fn 报告里没有部位时产出零候选() {
        assert!(parts("积水 3.2cm").is_empty());
    }

    #[test]
    fn 词典版本可读且与全局口径一致() {
        // 版本号来自**已加载的部位词典文件**（不是写死的常量）：
        // 词典改了版本号，部位归一用的就是新口径。
        assert_eq!(
            crate::coerce::dictionary::parts_version(),
            "parts-0.2.0"
        );
        assert!(crate::coerce::dictionary::parts_version().starts_with("parts-"));
        assert_eq!(
            crate::coerce::dictionary::parts_dictionary()
                .expect("部位词典应可用")
                .version(),
            crate::coerce::dictionary::parts_version()
        );
    }
}
