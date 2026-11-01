//! 解析内核第 3 层：抽取（部位 / 测项 / 数值 / 单位 + 原文位置）。
//!
//! 抽取只做**认得出来的**事：词典命中才给部位键与测项码；认不出的写失败条目，
//! 部位未命中就标 `part_unresolved`（**宁可不比不错比**，绝不猜部位）。
//!
//! 部位侧的归一口径见 [`crate::parse::extract_parts`]：部位键由部位词典决定，
//! 未命中时**既不产出部位键也不产出测项码**（合成键缺一部就不成立）。
//!
//! 测项侧的归一口径见 [`crate::parse::extract_items`]：测项码由测项词典决定，
//! 查不到时产出 `item_dict_missing` **失败条目**（带行列）——
//! **漏项不静默丢弃**：运维一眼知道该补哪个词。

use crate::coerce::path::{decide, CoercionDecision, CoercionFailure, CoercionPath};
use crate::coerce::units::canonical_unit;
use crate::parse::errors::{ErrorCode, Failure};
use crate::parse::extract_items::{resolve_item_code, ItemCandidate};
use crate::parse::extract_values::{
    classify_values, unit_missing_failure, value_unparseable_failure, ValueVerdict,
};
use crate::parse::gate::DocumentKind;
use crate::parse::lexer::TokenKind;
use crate::parse::locate::locate;
use crate::parse::lookahead::{merge_side, side_of, Merged, PartRef, Side};

/// 一条结构化条目（台账行的来源）。
#[derive(Clone, Debug)]
pub struct ParsedItem {
    /// 规范部位键（`kidney.left` 等）；未命中部位词典为 `None`。
    pub part_key: Option<String>,
    /// 测项码；未命中测项词典为 `None`。
    pub item_code: Option<String>,
    /// 报告原值。
    pub value_raw: Option<f64>,
    /// 报告原单位。
    pub unit_raw: Option<String>,
    /// 归一值（长度 mm / 容量 mL）；无合法强转路径为 `None`。
    pub value_si: Option<f64>,
    /// 归一单位码（UCUM）。
    pub unit_code: Option<String>,
    /// 强转路径。
    pub coercion_path: CoercionPath,
    /// 1 基行号（未知 0）。
    pub line: usize,
    /// 1 基列号（未知 0）。
    pub column: usize,
    /// 1 基字符光标（未知 0）。
    pub char_start: usize,
    /// 备注（未命中项等；不静默丢弃，只在此留痕）。
    pub notes: Vec<String>,
    /// 产出这条条目时生效的**词典版本**（部位 + 测项）。
    ///
    /// **为什么由条目自己记**：版本号是「这条数据是用哪套口径算出来的」的凭据。
    /// 若由调用方在落库时统一盖章，词典热更新（Task 26）期间
    /// 「解析时用的旧词典」会被盖成「新词典版本」——
    /// 口径变了却看不出来，正是版本留痕要防的那件事。
    /// 条目自己记着解析当时生效的版本，调用方盖错不了。
    pub dictionary_version: String,
}

impl ParsedItem {
    /// 条目自带的词典版本；条目没带版本时退回调用方给的版本（兜底）。
    ///
    /// 正常路径下条目**总是**带着版本（解析当时取的）——
    /// 这个兜底只服务于手工构造条目的调用点（如测试替身）。
    pub fn dictionary_version_if_empty(&self, fallback: &str) -> String {
        if self.dictionary_version.is_empty() {
            fallback.to_string()
        } else {
            self.dictionary_version.clone()
        }
    }
}

/// 一次解析的产出：条目 + 失败条目。
#[derive(Clone, Debug, Default)]
pub struct ParseOutcome {
    /// 解析出的结构化条目。
    pub items: Vec<ParsedItem>,
    /// 失败条目（带稳定错误码与原文位置）。
    pub failures: Vec<Failure>,
    /// 文档类别（闸门判定结果）—— 调用方据此回答「这份为什么没有台账行」。
    pub doc_kind: DocumentKind,
    /// 本次解析生效的词典版本（部位 + 测项）—— 报告级口径留痕。
    ///
    /// 即使一条条目都没解析出来（全是失败条目），这个版本仍然说明
    /// 「这份报告是在哪套词典口径下判的」。
    pub dictionary_version: String,
}

/// 部位引用 → 规范部位键（与合并层的规范键同源）。
pub use crate::parse::lookahead::part_key_of;

/// 测项别名 → 测项码（部位基名 + `.` + 测项码后缀）。
///
/// 委托给测项层（[`crate::parse::extract_items`]）与测项词典 —— **唯一真源**，
/// 不在抽取层另带一份别名表（两处真源必然漂移）。
pub fn item_code_of(alias: &str, base: Option<&str>) -> Option<String> {
    resolve_item_code(alias, base, 0).item_code
}

/// **条目准入**：这条值进不进台账。
///
/// 台账是「部位 × 测项 × 数值」的台账 —— 只有满足其一才是一行台账：
///
/// - **有解剖锚点**（前面出现过部位词 / 测项词）—— 这是某个部位的某个测项；
/// - **单位在口径目录内**（长度族 / 容量族）—— 哪怕部位没归一（如「肝内胆管 0.4cm」），
///   它仍是一条**长度**测值，认不出部位就留 `PART_UNRESOLVED` 痕迹等人补词典。
///
/// 两者都不满足（「血红蛋白 132 g/L」「17 Years」）→ **不是本产品的测值**：
/// 别的量纲的数字进不了对拍，占着台账行只会让人以为这份报告已经解析过了。
fn admits_item(anatomy_anchor: bool, unit: &str) -> bool {
    anatomy_anchor || canonical_unit(unit).is_some()
}

/// **留痕判据**：这条值要不要进失败清单。
///
/// 失败清单是医师的**工作清单**，不是数字黑名单 —— 进来一行就得有人看一眼。
/// 于是判据比条目准入宽一档：除了「像测值」的两种情形，
/// **报告正文里**的裸数字也留痕（报告里的「左肾积水 3.2」是测值缺单位，
/// 必须让人看见）；而报告之外的叙述片段（「高血压 3 年」）不进清单。
fn leaves_trace(anatomy_anchor: bool, report_context: bool, unit: &str) -> bool {
    anatomy_anchor || report_context || canonical_unit(unit).is_some()
}

/// 按合并后的 token 序列抽取结构化条目与失败条目。
///
/// **数值与单位交给数值层判**（[`crate::parse::extract_values`]）：本层只做
/// 「上下文绑定」—— 把数值挂到部位与测项上，再走强转内核；
/// 「这个数能不能用、缺不缺单位、坏在哪一类」由数值层一处判定，
/// 不在两处各认一遍 token（两处真源必然漂移）。
///
/// **两条准入规则（负语料不误抽的落点）**：
///
/// | 规则 | 判据 | 不满足时 |
/// |---|---|---|
/// | **条目准入** | 有**解剖锚点**（部位词 / 测项词）**或** 单位在**口径目录**内（长度族 / 容量族） | 不落台账条目；留一条 `COERCION_NO_PATH` 失败条目指回数值 |
/// | **裸数字留痕** | 有解剖锚点**或** 处于**报告正文**（出现过段落标题） | 不留痕 —— 叙述里的「3 年」「随访号 12」不是测值，不塞进医师的工作清单 |
///
/// **为什么要有准入规则**：词法层把**任何**拉丁字母串都当单位候选，
/// 于是「血红蛋白 132 g/L」「17 Years」「11 institutions」这类**别的量纲的数字**
/// 会被当成测值抽出来，在台账里留下一行行**看着像数字、其实不是测值**的垃圾行。
/// 台账是「部位 × 测项 × 数值」的台账 —— 认不出归属的行没有对拍价值，
/// 但也不能悄悄吞掉（要指回原文第几行第几列）。
pub fn extract(merged: &[Merged], text: &str) -> ParseOutcome {
    let mut outcome = ParseOutcome::default();
    // 词典版本在**解析当时**取一次：条目记着自己解析当时生效的口径，
    // 之后词典热更新（Task 26）不影响已解析出的条目。
    let dictionary_version = crate::dictionary_version();
    outcome.dictionary_version = dictionary_version.clone();
    let mut current_part: Option<PartRef> = None;
    let mut current_item: Option<ItemCandidate> = None;
    let mut pending_side: Option<Side> = None;
    // 已报过未命中的部位起点（同一个部位词只报一次，别把失败清单刷成重复行）。
    let mut reported_parts: Vec<usize> = Vec::new();
    // 解剖锚点：出现过部位词或测项词 —— 「这个数是某个部位的某个测项」。
    let mut anatomy_anchor = false;
    // 报告正文：出现过段落标题（超声所见 / 结论 / 提示）——「这段文字是报告正文」。
    let mut report_context = false;
    // 数值层判定（与 merged 逐位对应）。
    let verdicts = classify_values(merged, text);

    for (token, verdict) in merged.iter().zip(verdicts.iter()) {
        match token {
            Merged::Part(part) => {
                let mut part = part.clone();
                part.side = merge_side(part.side, pending_side);
                pending_side = None;
                current_part = Some(part);
                current_item = None;
                anatomy_anchor = true;
            }
            Merged::Item {
                alias: item_alias,
                alias_start,
                part,
                ..
            } => {
                let mut own_part = part.clone();
                if let Some(p) = own_part.as_mut() {
                    p.side = merge_side(p.side, pending_side);
                }
                pending_side = None;
                let alias = item_alias.clone();
                if let Some(p) = own_part.clone() {
                    current_part = Some(p);
                }
                // 测项词**自身**的位置（合并 token 继承的是部位词的位置）——
                // 失败条目要指回测项词，运维照着补词典。
                current_item = Some(ItemCandidate {
                    alias,
                    start: *alias_start,
                    part: own_part,
                });
                anatomy_anchor = true;
            }
            Merged::Raw(tok) if tok.kind == TokenKind::Modifier => {
                // 侧别修饰词（「左」「左侧」「右」「右侧」「双」「双侧」）。
                pending_side = side_of(&tok.text);
            }
            Merged::Raw(tok) if tok.kind == TokenKind::Item => {
                current_item = Some(ItemCandidate {
                    alias: tok.text.clone(),
                    start: tok.start,
                    part: None,
                });
                anatomy_anchor = true;
            }
            Merged::Raw(tok) if tok.kind == TokenKind::Section => {
                // 段落标题：报告正文从这里开始（裸数字留痕的判据之一）。
                report_context = true;
            }
            _ => match verdict {
                ValueVerdict::Measured(hit) => {
                    // **条目准入**：有解剖锚点，或单位在口径目录内（长度 / 容量）。
                    // 两者都不满足 → 这不是本产品的测值（化验值、别的量纲的数字），
                    // 不进台账；但要留一条失败条目指回原文（不静默丢弃）——
                    // 留不留痕看这条值**像不像一条测值**（见 [`leaves_trace`]）。
                    if !admits_item(anatomy_anchor, hit.unit_text()) {
                        if leaves_trace(anatomy_anchor, report_context, hit.unit_text()) {
                            outcome.failures.push(Failure::new(
                                ErrorCode::CoercionNoPath,
                                hit.start,
                                hit.line,
                                hit.column,
                                &hit.value_text,
                            ));
                        }
                        continue;
                    }
                    let part_ref = current_item
                        .as_ref()
                        .and_then(|it| it.part.clone())
                        .or_else(|| current_part.clone());
                    let (line, column) = (hit.line, hit.column);
                    let mut notes: Vec<String> = Vec::new();

                    // 部位解析：词典命中 → 规范部位键；未命中 → 显式失败条目，**不猜部位**。
                    //
                    // 两种未命中指回的位置不同：
                    // - 部位候选词存在但词典查不到 → 指**部位词自身**（运维要补哪个词一目了然）；
                    // - 报告里根本没有部位候选词 → 指**测量值**（运维要判断的是「这个值没有归属」）。
                    let part_key = match part_ref.as_ref() {
                        Some(p) => match part_key_of(p) {
                            Some(key) => Some(key),
                            None => {
                                notes.push(ErrorCode::PartUnresolved.as_str().to_string());
                                if !reported_parts.contains(&p.start) {
                                    reported_parts.push(p.start);
                                    let (pl, pc) = locate(text, p.start);
                                    outcome.failures.push(Failure::new(
                                        ErrorCode::PartUnresolved,
                                        p.start,
                                        pl,
                                        pc,
                                        &p.raw_part,
                                    ));
                                }
                                None
                            }
                        },
                        None => {
                            notes.push(ErrorCode::PartUnresolved.as_str().to_string());
                            outcome.failures.push(Failure::new(
                                ErrorCode::PartUnresolved,
                                hit.start,
                                line,
                                column,
                                &hit.value_text,
                            ));
                            None
                        }
                    };

                    // 测项码要**部位 + 测项**合成：部位没归一时合成键不成立，
                    // 不产出「看着能用、其实对不上对象」的测项码。
                    let part_base = part_ref.as_ref().and_then(|p| p.base.clone());
                    let item_res = current_item
                        .as_ref()
                        .map(|c| resolve_item_code(&c.alias, part_base.as_deref(), c.start));
                    let item_code = item_res.as_ref().and_then(|r| r.item_code.clone());
                    if item_code.is_none() {
                        notes.push(ErrorCode::ItemDictMissing.as_str().to_string());
                    }

                    // 测项词典未命中 → **显式失败条目**（带 1 基行列），指回测项词。
                    //
                    // 「不静默丢弃测项文本」：词典缺这个词，运维就得知道该补哪个词、
                    // 报告里哪一列是漏项 —— 只在条目上挂个 note，台账与失败清单里
                    // 谁都看不见，漏抄的项就这么过去了。
                    //
                    // 部位没归一时**不重复报**（那是 `PART_UNRESOLVED` 已经交代的事）。
                    if let Some(res) = item_res.as_ref() {
                        if res.dict_miss {
                            let (il, ic) = locate(text, res.start);
                            outcome.failures.push(Failure::new(
                                ErrorCode::ItemDictMissing,
                                res.start,
                                il,
                                ic,
                                &res.alias,
                            ));
                        }
                    }

                    // 原值词面自带倍率（`3.2×2`）时，倍率先折算进原值，
                    // 强转路径记 `DirectScale`（不是 `UnitConvert`）—— 台账里能看出
                    // 这一行的原值是带倍率写出来的。
                    let effective = hit.effective_value();
                    // 强转：**一次判定同源** —— 判定结果同时给出路径标签与归一值，
                    // 不存在「判了一条路、换算走另一条」的分叉（单位查不到目录，
                    // 路径判定与换算两边都拿不到）。
                    //
                    // **拿不到数值就落同一个错误码的失败条目** ——
                    // 静默丢值等于「这条测值不存在」，台账里看不出是丢了还是本来就没有。
                    let decision = match hit.value {
                        Some(v) => decide(v, hit.scale, hit.unit_text()),
                        None => CoercionDecision::failed(CoercionFailure::ValueInvalid),
                    };
                    let path = decision.path;
                    let (value_si, unit_code) = match (decision.value_si, decision.unit_code) {
                        (Some(v), Some(c)) => (Some(v), Some(c)),
                        _ => {
                            outcome.failures.push(Failure::new(
                                ErrorCode::CoercionNoPath,
                                hit.start,
                                line,
                                column,
                                &hit.value_text,
                            ));
                            (None, None)
                        }
                    };

                    outcome.items.push(ParsedItem {
                        part_key,
                        item_code,
                        value_raw: effective,
                        unit_raw: hit.unit.clone(),
                        value_si,
                        unit_code,
                        coercion_path: path,
                        line,
                        column,
                        char_start: hit.start,
                        notes,
                        dictionary_version: dictionary_version.clone(),
                    });
                }
                ValueVerdict::MissingUnit(hit) => {
                    // 数值有、单位缺：保留原值，不产出数值。
                    //
                    // **留痕判据**：落在解剖锚点或报告正文里才留 —— 报告里的
                    // 「左肾积水 3.2」是**测值缺单位**，必须让人看见；
                    // 叙述里的「高血压 3 年」「随访号 12」不是测值，
                    // 塞进失败清单只会稀释医师真正要看的那几行。
                    if leaves_trace(anatomy_anchor, report_context, hit.unit_text()) {
                        outcome.failures.push(unit_missing_failure(hit, text));
                    }
                }
                ValueVerdict::Unparseable(hit) => {
                    // 数值不可解析：不产出数值。留痕判据同上。
                    if leaves_trace(anatomy_anchor, report_context, hit.unit_text()) {
                        outcome.failures.push(value_unparseable_failure(hit, text));
                    }
                }
                ValueVerdict::NotAValue => {}
            },
        }
    }

    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::lookahead::merge;
    use crate::parse::lexer::tokenize;

    fn run(text: &str) -> ParseOutcome {
        let toks = tokenize(text);
        let merged = merge(&toks);
        extract(&merged, text)
    }

    #[test]
    fn 左肾三二厘米抽出部位键与强转路径() {
        let out = run("左肾 3.2cm");
        assert_eq!(out.items.len(), 1);
        let item = &out.items[0];
        assert_eq!(item.part_key.as_deref(), Some("kidney.left"));
        assert_eq!(item.value_raw, Some(3.2));
        assert_eq!(item.unit_raw.as_deref(), Some("cm"));
        assert_eq!(item.value_si, Some(32.0));
        assert_eq!(item.unit_code.as_deref(), Some("mm"));
        assert_eq!(item.coercion_path, CoercionPath::UnitConvert);
        assert!(out.failures.is_empty());
    }

    #[test]
    fn 缺单位产出失败条目且不产出数值() {
        let out = run("左肾 3.2");
        assert!(out.items.is_empty());
        assert_eq!(out.failures.len(), 1);
        assert_eq!(out.failures[0].code, ErrorCode::UnitMissing);
        assert!(out.failures[0].line >= 1);
        assert!(out.failures[0].column >= 1);
    }

    #[test]
    fn 部位词典未命中标未解析() {
        let out = run("肝 3.2cm");
        assert_eq!(out.items.len(), 1);
        assert_eq!(out.items[0].part_key, None);
        assert_eq!(out.failures.len(), 1);
        assert_eq!(out.failures[0].code, ErrorCode::PartUnresolved);
    }

    #[test]
    fn 未知单位查不到路径不产出归一值() {
        let out = run("左肾 3.2xx");
        assert_eq!(out.items.len(), 1);
        assert_eq!(out.items[0].coercion_path, CoercionPath::None);
        assert_eq!(out.items[0].value_si, None);
        assert_eq!(out.failures[0].code, ErrorCode::CoercionNoPath);
    }

    #[test]
    fn 双侧肾积水归一到双侧部位键() {
        let out = run("双侧肾积水 3.2cm");
        assert_eq!(out.items.len(), 1);
        assert_eq!(out.items[0].part_key.as_deref(), Some("kidney.bilateral"));
    }

    #[test]
    fn 测项与部位合并后取到测项码() {
        let out = run("左肾前后径 2.1cm");
        assert_eq!(out.items.len(), 1);
        assert_eq!(out.items[0].part_key.as_deref(), Some("kidney.left"));
        assert_eq!(out.items[0].item_code.as_deref(), Some("kidney.ap_diameter"));
        assert_eq!(out.items[0].value_si, Some(21.0));
    }

    #[test]
    fn 残余尿量独立出现时取上下文部位() {
        let out = run("膀胱 残余尿量 120ml");
        assert_eq!(out.items.len(), 1);
        assert_eq!(out.items[0].item_code.as_deref(), Some("bladder.residual_urine_volume"));
        assert_eq!(out.items[0].value_si, Some(120.0));
        assert_eq!(out.items[0].unit_code.as_deref(), Some("mL"));
    }

    #[test]
    fn 词典外部位词产出未命中且部位键为空() {
        // 「胆囊」是腹部器官，不在泌尿系口径内：未命中，不猜部位。
        let out = run("胆囊 3.2cm");
        assert_eq!(out.items.len(), 1);
        assert_eq!(out.items[0].part_key, None, "未命中不得产出部位键");
        assert_eq!(out.items[0].item_code, None, "部位未归一时合成键不成立");
        assert!(out.items[0]
            .notes
            .iter()
            .any(|n| n == ErrorCode::PartUnresolved.as_str()));
        assert_eq!(out.failures.len(), 1);
        assert_eq!(out.failures[0].code, ErrorCode::PartUnresolved);
    }

    #[test]
    fn 未命中部位词失败条目指回部位词自身() {
        // 运维要补词典 → 失败条目要指到**那个词**，不是指到数值。
        let out = run("超声所见\n胆囊 3.2cm");
        assert_eq!(out.failures.len(), 1);
        let f = &out.failures[0];
        assert_eq!(f.code, ErrorCode::PartUnresolved);
        assert_eq!(f.line, 2);
        assert_eq!(f.column, 1);
        assert_eq!(f.raw_snippet, "胆囊");
    }

    #[test]
    fn 报告里没有部位候选词时失败条目指回测量值() {
        // 「肝内胆管」在词法层就没认出来是部位：失败条目指到数值（值没有归属）。
        let out = run("肝内胆管 0.4cm");
        assert_eq!(out.failures.len(), 1);
        let f = &out.failures[0];
        assert_eq!(f.code, ErrorCode::PartUnresolved);
        assert_eq!(f.line, 1);
        assert_eq!(f.column, 6);
        assert_eq!(f.raw_snippet, "0.4");
    }

    #[test]
    fn 同一个未命中部位只报一次失败() {
        // 一个部位词下挂两个测量值：失败清单不刷重复行。
        let out = run("胆囊 3.2cm 4.1cm");
        assert_eq!(out.items.len(), 2);
        assert_eq!(out.failures.len(), 1);
    }

    #[test]
    fn 同一未命中部位出现两次各报一次() {
        let out = run("胆囊 3.2cm\n胆囊 2.8cm");
        assert_eq!(out.failures.len(), 2);
        assert_eq!(out.failures[0].line, 1);
        assert_eq!(out.failures[1].line, 2);
    }

    #[test]
    fn 双肾与双侧肾落到同一部位键() {
        let a = run("双肾积水 2.6cm");
        let b = run("双侧肾积水 2.6cm");
        assert_eq!(a.items[0].part_key.as_deref(), Some("kidney.bilateral"));
        assert_eq!(b.items[0].part_key.as_deref(), Some("kidney.bilateral"));
    }

    #[test]
    fn 部位别名写法落到同一部位键() {
        for text in ["左肾 3.2cm", "左侧肾脏 3.2cm", "左肾脏 3.2cm"] {
            let out = run(text);
            assert_eq!(
                out.items[0].part_key.as_deref(),
                Some("kidney.left"),
                "{} 应归一到 kidney.left",
                text
            );
        }
    }

    #[test]
    fn 肾盂与肾测项码互不串用() {
        // 肾盂前后径与肾前后径是不同测量：合成键不能混。
        let a = run("左肾盂前后径 2.1cm");
        let b = run("左肾前后径 2.1cm");
        assert_eq!(a.items[0].part_key.as_deref(), Some("renal_pelvis.left"));
        assert_eq!(a.items[0].item_code.as_deref(), Some("renal_pelvis.ap_diameter"));
        assert_eq!(b.items[0].part_key.as_deref(), Some("kidney.left"));
        assert_eq!(b.items[0].item_code.as_deref(), Some("kidney.ap_diameter"));
    }

    // —— 条目准入（负语料不误抽的落点）——

    #[test]
    fn 别的量纲的化验值不抽条目() {
        // 「132 g」不是长度/容量，也没有解剖归属 → 不是本产品的测值。
        let out = run("血常规检查未见明显异常，血红蛋白 132 g/L。");
        assert_eq!(out.items.len(), 0, "化验值不得进台账");
    }

    #[test]
    fn 化验值在报告正文里留一条痕迹但仍不进台账() {
        // 看到了、不是我们的量纲 → 留痕迹指回原文；但**不占台账行**。
        let out = run("超声所见\n血红蛋白 132 g/L");
        assert_eq!(out.items.len(), 0);
        assert_eq!(out.failures.len(), 1);
        assert_eq!(out.failures[0].code, ErrorCode::CoercionNoPath);
        assert_eq!(out.failures[0].line, 2);
        assert_eq!(out.failures[0].column, 6);
    }

    #[test]
    fn 叙述里的裸数字不留痕() {
        // 报告之外的叙述片段：年龄、年份、编号不是测值，不塞进工作清单。
        let out = run("既往高血压病史 3 年，糖尿病 5 年。");
        assert_eq!(out.items.len(), 0);
        assert_eq!(out.failures.len(), 0);
    }

    #[test]
    fn 报告正文里的裸数字留痕() {
        // 报告正文里的裸数字可能是**缺单位的测值** → 必须让人看见。
        let out = run("超声所见\n左肾积水 3.2");
        assert_eq!(out.items.len(), 0);
        assert_eq!(out.failures.len(), 1);
        assert_eq!(out.failures[0].code, ErrorCode::UnitMissing);
    }

    #[test]
    fn 英文叙述里的数字不抽条目() {
        // 「17 Years」：单位不是我们口径里的，也没有解剖归属。
        let out = run("The patient was followed for 17 Years in the clinic.");
        assert_eq!(out.items.len(), 0);
    }

    #[test]
    fn 长度值即使认不出部位仍进台账但留未命中痕迹() {
        // 「肝内胆管 0.4cm」：单位在口径目录内 → 是长度测值；
        // 部位认不出 → 留 PART_UNRESOLVED 痕迹等人补词典，**不丢**。
        let out = run("肝内胆管 0.4cm");
        assert_eq!(out.items.len(), 1);
        assert_eq!(out.items[0].part_key, None);
        assert_eq!(out.items[0].value_si, Some(4.0));
        assert_eq!(out.failures.len(), 1);
        assert_eq!(out.failures[0].code, ErrorCode::PartUnresolved);
    }

    #[test]
    fn 未知单位但有解剖锚点仍进台账并留无路径痕迹() {
        // 「左肾 3.2xx」：认得部位、认不得单位 → 进台账（值待补），
        // 强转路径显式为 None，不静默按某单位兜底。
        let out = run("左肾 3.2xx");
        assert_eq!(out.items.len(), 1);
        assert_eq!(out.items[0].part_key.as_deref(), Some("kidney.left"));
        assert_eq!(out.items[0].coercion_path, CoercionPath::None);
        assert_eq!(out.items[0].value_si, None);
        assert_eq!(out.failures.len(), 1);
        assert_eq!(out.failures[0].code, ErrorCode::CoercionNoPath);
    }
}
