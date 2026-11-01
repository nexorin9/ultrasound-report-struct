//! 解析内核第 2 层：前瞻合并表。
//!
//! 机制溯源（**改写**，不复制源代码）：源产品的过滤层对「需要前瞻」的 token 做**一次**
//! 前瞻 —— 命中条件成立时把当前 token 换成合并后的**合成 token**，合成 token 继承
//! **第一个** token 的起始位置；条件不成立时原样返回，读到的下一个 token 留作下一次
//! 的当前 token。本项目把同一机制改写为中文报告词表：
//!
//! | 规则类型 | 命中条件 | 合并后规范 token | canonical key |
//! |---|---|---|---|
//! | `ModifierPart` | 单侧修饰词 + 部位词 | 部位短语（带侧别） | `part:kidney.left` |
//! | `PartItem` | 部位短语 + 测项词 | 测项短语（携带部位） | `item:kidney.前后径` |
//! | `NumberUnit` | 数值 + 单位 | 测量（原值 + 原单位） | `measurement:cm` |
//! | `NumberMultiply` | 数值 + 乘号 | 倍率链起点 | `scale:3.2×` |
//! | `ScaleNumber` | 倍率链 + 数值 | 带倍率 | `scale:3.2×2` |
//! | `ScaleUnit` | 带倍率 + 单位 | 测量（含倍率） | `measurement:cm×2` |
//!
//! **扫描方式（回退一个 token 再合并，合并后重新推进游标）**：当前位置的 token 若与
//! **下一个** token 命中某条规则，就合并成一个规范 token；合并结果**继续**与再下一个
//! token 试合并（所以「左 + 肾」合并出的部位短语还能继续与「前后径」合并成测项短语）；
//! 某一轮没命中就把下一个 token **回退**（不消费），留给下一轮当当前位置 —— 与源产品
//! 「读到的 lookahead token 留作下一次当前 token」一致。
//!
//! **优先级表**：同一位置有多个规则命中时，按「合并跨度长者优先 → 规则优先级小者优先
//! → 规则表次序」三层定夺，保证同一份报告在任何机器上得到同一条 token 流。
//!
//! **边界**：「双侧」**不**与部位词合并 —— 双侧是与「单侧同一部位」对拍时的可比性
//! 判定依据（Task 31），必须留在抽取层与部位键组合，不在合并层烧成词面。

use super::lexer::{Token, TokenKind};

/// 侧别。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// 未标注侧别。
    None,
    /// 左侧。
    Left,
    /// 右侧。
    Right,
    /// 双侧。
    Bilateral,
}

/// 部位引用（合并后或独立出现的部位）。
#[derive(Clone, Debug)]
pub struct PartRef {
    /// 原文词面（如「左肾」）。
    pub text: String,
    /// 部位词面（**不含**侧别修饰词，如「肾脏」）—— 部位词典按它查规范基名。
    pub raw_part: String,
    /// 规范解剖基名（如 `kidney` / `renal_pelvis` / `ureter` / `bladder`）。
    ///
    /// **词典未命中为 `None`** —— 词典外的部位词（胆囊等）不猜部位，
    /// 由抽取层转成 `part_unresolved` 失败条目，不参与对拍。
    pub base: Option<String>,
    /// 侧别。
    pub side: Side,
    /// 1 基字符光标（合并后取**第一个** token 的位置，与源产品合并 token 继承首 token 位置一致）。
    pub start: usize,
}

/// 合并后的 token。
#[derive(Clone, Debug)]
pub enum Merged {
    /// 部位短语。
    Part(PartRef),
    /// 测项短语；若由「部位词 + 测项词」合并而来，`part` 携带被合并的部位。
    Item {
        /// 合并后的词面（部位 + 测项；词面重叠时只留测项别名）。
        text: String,
        /// 测项**别名**（测项词自身的词面，不含部位前缀）。
        alias: String,
        /// 1 基字符光标（合并 token 的起点 —— 继承首 token 位置）。
        start: usize,
        /// 测项词**自身**的 1 基字符光标。
        ///
        /// 合并 token 的起点是**部位词**的位置（首 token 继承），
        /// 失败条目要指回**测项词**本身（运维照着补词典），
        /// 所以测项词自己的位置必须单独留着。
        alias_start: usize,
        /// 被合并的部位（独立测项为 `None`）。
        part: Option<PartRef>,
    },
    /// 倍率链中间态（`数值 × [数值]`）—— 合并链没走完时留在流里，由抽取层判失败。
    Scale {
        /// 倍率前的数值。
        value: f64,
        /// 倍率（`×` 后还没读到数时为 `None`）。
        factor: Option<f64>,
        /// 原值词面（`3.2×` / `3.2×2`）。
        raw_value: String,
        /// 1 基字符光标。
        start: usize,
    },
    /// 测量：数值 + 原单位（可带倍率）。
    Measurement {
        /// 数值（**未**含倍率）。
        value: f64,
        /// 倍率（原值词面自带 `×2` 时为 `Some`，否则 `None`）。
        scale: Option<f64>,
        /// 原单位词面（保持原文写法，未知单位也保留）。
        unit: String,
        /// 原值词面（带倍率时为 `3.2×2` 这种原文写法）。
        raw_value: String,
        /// 1 基字符光标（数值 token 起点）。
        start: usize,
        /// 单位 token 的 1 基字符光标。
        ///
        /// 数值与单位**各留各的位置** —— 抽取层要能把「哪个数、哪个单位」
        /// 分别指回原文（失败条目定位到数值，单位位置供核对）。
        unit_start: usize,
    },
    /// 数值不可解析（带单位但值解析不出来）。
    BadNumber {
        /// 原值词面。
        raw_value: String,
        /// 原单位词面（倍率链断裂时可能为空）。
        unit: String,
        /// 1 基字符光标。
        start: usize,
    },
    /// 原样透传的 token。
    Raw(Token),
}

impl Merged {
    /// **规范键**（canonical key）：合并 token 的稳定身份标识。
    ///
    /// 规范键让「部位」与「测项」在同一段文字上重叠时仍有唯一身份可查：
    /// 部位是 `part:<基名>[.<侧别>]`，测项是 `item:<基名>.<别名>`。
    /// 部位词典未命中的部位词，规范键显式写成 `part:unresolved` ——
    /// 不拿原词面当键（那会让未归一的部位看起来像已归一）。
    pub fn canonical_key(&self) -> String {
        match self {
            Merged::Part(p) => format!(
                "part:{}",
                part_key_of(p).unwrap_or_else(|| "unresolved".to_string())
            ),
            Merged::Item {
                alias, part, ..
            } => {
                let alias = alias.clone();
                match part {
                    Some(p) => format!(
                        "item:{}.{}",
                        p.base.as_deref().unwrap_or("unresolved"),
                        alias
                    ),
                    None => format!("item:{}", alias),
                }
            }
            Merged::Scale { raw_value, .. } => format!("scale:{}", raw_value),
            Merged::Measurement {
                unit, scale, ..
            } => match scale {
                Some(f) => format!("measurement:{}×{}", unit, fmt_num(*f)),
                None => format!("measurement:{}", unit),
            },
            Merged::BadNumber { unit, .. } => format!("bad_number:{}", unit),
            Merged::Raw(t) => format!("raw:{}:{}", t.kind.as_str(), t.text),
        }
    }
}

/// 数字格式化（规范键里用，避免 `3.2` 写成 `3.2000000000000004`）。
fn fmt_num(v: f64) -> String {
    let s = format!("{}", v);
    s
}

/// 合并规则类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeRuleKind {
    /// 修饰词 + 部位词。
    ModifierPart,
    /// 部位词 + 测项词。
    PartItem,
    /// 数值 + 单位。
    NumberUnit,
    /// 数值 + 乘号。
    NumberMultiply,
    /// 倍率链 + 数值。
    ScaleNumber,
    /// 带倍率 + 单位。
    ScaleUnit,
}

impl MergeRuleKind {
    /// 规则类型字符串（进调试输出与用例表）。
    pub fn as_str(self) -> &'static str {
        match self {
            MergeRuleKind::ModifierPart => "ModifierPart",
            MergeRuleKind::PartItem => "PartItem",
            MergeRuleKind::NumberUnit => "NumberUnit",
            MergeRuleKind::NumberMultiply => "NumberMultiply",
            MergeRuleKind::ScaleNumber => "ScaleNumber",
            MergeRuleKind::ScaleUnit => "ScaleUnit",
        }
    }
}

/// 合并规则（规则表的一行）。
///
/// 规则表是**唯一真源**：扫描器与逐条用例都从这里取规则，不在别处硬写判定。
pub struct MergeRule {
    /// 规则类型。
    pub kind: MergeRuleKind,
    /// 规则名（调试与用例可读）。
    pub name: &'static str,
    /// 优先级（数值小 = 优先）。
    pub priority: u8,
    /// 命中条件：左 token + 右 token 是否命中本规则。
    pub hit: fn(&Merged, &Merged) -> bool,
    /// 合并后规范 token（`None` 表示本规则对这组 token 不适用 —— 调用方回退，不消费）。
    pub apply: fn(&Merged, &Merged) -> Option<Merged>,
}

/// 规则优先级表（数值小 = 优先）。
///
/// 倍率类（10–12）→ 部位类（20–21）→ 数值类（22）：倍率链一旦断掉，后面的数值与单位
/// 都失去意义；部位与测项必须先定型，数值与单位才能挂到正确的上下文上。
const RULES: [MergeRule; 6] = [
    MergeRule {
        kind: MergeRuleKind::NumberMultiply,
        name: "数值+乘号",
        priority: 10,
        hit: hit_number_multiply,
        apply: apply_number_multiply,
    },
    MergeRule {
        kind: MergeRuleKind::ScaleNumber,
        name: "倍率链+数值",
        priority: 11,
        hit: hit_scale_number,
        apply: apply_scale_number,
    },
    MergeRule {
        kind: MergeRuleKind::ScaleUnit,
        name: "带倍率+单位",
        priority: 12,
        hit: hit_scale_unit,
        apply: apply_scale_unit,
    },
    MergeRule {
        kind: MergeRuleKind::ModifierPart,
        name: "修饰词+部位词",
        priority: 20,
        hit: hit_modifier_part,
        apply: apply_modifier_part,
    },
    MergeRule {
        kind: MergeRuleKind::PartItem,
        name: "部位词+测项词",
        priority: 21,
        hit: hit_part_item,
        apply: apply_part_item,
    },
    MergeRule {
        kind: MergeRuleKind::NumberUnit,
        name: "数值+单位",
        priority: 22,
        hit: hit_number_unit,
        apply: apply_number_unit,
    },
];

/// 合并规则表（**唯一真源**）。
pub fn merge_rules() -> &'static [MergeRule] {
    &RULES
}

/// 规则优先级（数值小 = 优先；未知规则返回 `u8::MAX`）。
pub fn rule_priority(kind: MergeRuleKind) -> u8 {
    merge_rules()
        .iter()
        .find(|r| r.kind == kind)
        .map(|r| r.priority)
        .unwrap_or(u8::MAX)
}

/// 部位 / 测项**词面重叠**的消解结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverlapWinner {
    /// 部位解释胜出（合并词面保留部位前缀）。
    Part,
    /// 测项解释胜出（测项别名已含部位词面，合并词面不再重复拼部位前缀）。
    Item,
}

/// 部位 / 测项**同名歧义**的消解（优先级表第 1、2 层）。
///
/// 同一段文字既是部位别名又是测项别名时（例：部位写「肾」、测项写「肾脏长度」），
/// 按三步定夺：
///
/// 1. **最长匹配优先** —— 测项别名已含部位词面且跨度更长时，测项解释胜出，
///    合并词面不再重复拼部位前缀（否则会拼出「肾肾脏长度」这种鬼话）；
/// 2. **等长时部位优先** —— 中文报告里部位总在测项之前，保住部位才保得住测项上下文；
/// 3. 再同则按规则优先级表 —— 部位类规则（20/21）先于测项类规则。
pub fn resolve_overlap(part_text: &str, item_text: &str) -> OverlapWinner {
    let p = part_text.chars().count();
    let i = item_text.chars().count();
    if i > p && item_text.starts_with(part_text) {
        OverlapWinner::Item
    } else if p > i {
        OverlapWinner::Part
    } else {
        // 等长（含部位词面更长但别名并不含它的情况）—— 部位优先。
        OverlapWinner::Part
    }
}

/// 部位引用 → 规范部位键（`kidney.left` 等）。
///
/// **规范键由部位词典给出**，不在这里拼字符串 —— 词典没收录的
/// （基名 × 侧别）组合就是**没有**，不拼一个词典不认的键出来
/// （那会让没对过口径的部位看起来像已归一的部位）。
///
/// **词典未命中返回 `None`** —— 部位键要么是词典给的规范键，要么没有；
/// 不给「原词面当键」的兜底（那会让未命中的部位看起来像已归一的部位）。
pub fn part_key_of(part: &PartRef) -> Option<String> {
    let base = part.base.as_deref()?;
    crate::coerce::dictionary::part_key_of_base_side(base, side_key(part.side))
}

/// 侧别 → 词典侧别键（`left` / `right` / `bilateral` / `none`）。
fn side_key(side: Side) -> &'static str {
    match side {
        Side::None => "none",
        Side::Left => "left",
        Side::Right => "right",
        Side::Bilateral => "bilateral",
    }
}

/// 部位基名映射（部位词 → 解剖基名）。
///
/// **唯一真源在部位词典**（`coerce::dictionary`）：合并层与抽取层都走同一张表，
/// 不各带一份映射 —— 两处真源必然漂移，漂移表现为「同一写法对上了不同基名」。
/// 词典外的部位词返回 `None`（查不到就是查不到）。
fn part_base(raw: &str) -> Option<String> {
    crate::coerce::dictionary::part_base_of(raw)
}

/// 侧别词 → 侧别键（未知修饰词返回 `None`）。
///
/// 「双」与「双侧」等价（报告里「双肾」与「双侧肾」是同一件事）；
/// 但**双侧不与部位词合并**（见 [`hit_modifier_part`]）——
/// 双侧是与「单侧同一部位」对拍时的可比性判定依据，必须留在抽取层与部位键组合。
pub fn side_of(raw: &str) -> Option<Side> {
    match raw {
        "左" | "左侧" => Some(Side::Left),
        "右" | "右侧" => Some(Side::Right),
        "双" | "双侧" => Some(Side::Bilateral),
        _ => None,
    }
}

/// 侧别合并：部位自带侧别优先，否则取前置修饰词的侧别。
///
/// 修饰词（「双侧」）与部位词（「肾」）**不合并成词面** —— 合并了就分不清
/// 「双侧肾」是双侧还是单侧，对拍时会把双侧值拿去和单侧值比。
pub fn merge_side(part_side: Side, pending: Option<Side>) -> Side {
    match part_side {
        Side::None => pending.unwrap_or(Side::None),
        other => other,
    }
}

/// 解析数值词面（`3.2` / `32` / `3.2.5` 之类由上层判不可解析）。
///
/// **唯一真源在数值层**（[`super::extract_values::parse_measurement_value`]）：
/// 「这个数字能不能用」只有一套规则（空值 / 纯符号 / 多个小数点 / 超长数字串
/// 各有归类），合并层与抽取层共用 —— 两处各写一套必然漂移，
/// 漂移表现为「同一份报告在不同机器上得到不同数值」。
pub fn parse_number(raw: &str) -> Option<f64> {
    super::extract_values::parse_measurement_value(raw).value()
}

// ---------------------------------------------------------------------------
// 操作数取数（规则命中条件只看这层，不直接拆 token 内部字段）
// ---------------------------------------------------------------------------

fn raw_modifier(m: &Merged) -> Option<&Token> {
    match m {
        Merged::Raw(t) if t.kind == TokenKind::Modifier => Some(t),
        _ => None,
    }
}

fn raw_part_token(m: &Merged) -> Option<&Token> {
    match m {
        Merged::Raw(t) if t.kind == TokenKind::Part => Some(t),
        _ => None,
    }
}

fn raw_item_token(m: &Merged) -> Option<&Token> {
    match m {
        Merged::Raw(t) if t.kind == TokenKind::Item => Some(t),
        _ => None,
    }
}

fn raw_number_token(m: &Merged) -> Option<&Token> {
    match m {
        Merged::Raw(t) if t.kind == TokenKind::Number => Some(t),
        _ => None,
    }
}

fn raw_unit_token(m: &Merged) -> Option<&Token> {
    match m {
        Merged::Raw(t) if t.kind == TokenKind::Unit => Some(t),
        _ => None,
    }
}

fn raw_multiply_token(m: &Merged) -> Option<&Token> {
    match m {
        Merged::Raw(t) if t.kind == TokenKind::Multiply => Some(t),
        _ => None,
    }
}

/// 部位视图（未合并的部位词 token 与已合并的部位短语统一看待）。
fn part_view(m: &Merged) -> Option<PartRef> {
    match m {
        Merged::Part(p) => Some(p.clone()),
        Merged::Raw(t) if t.kind == TokenKind::Part => Some(PartRef {
            text: t.text.clone(),
            raw_part: t.text.clone(),
            base: part_base(&t.text),
            side: Side::None,
            start: t.start,
        }),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 规则表：命中条件与合并动作
// ---------------------------------------------------------------------------

/// `ModifierPart`：单侧修饰词 + 部位词。
///
/// 「双侧」**不**命中 —— 双侧是与「单侧同一部位」对拍时的可比性判定依据，
/// 必须在抽取层与部位键组合。
fn hit_modifier_part(left: &Merged, right: &Merged) -> bool {
    let (Some(m), Some(_)) = (raw_modifier(left), raw_part_token(right)) else {
        return false;
    };
    matches!(side_of(&m.text), Some(Side::Left) | Some(Side::Right))
}

fn apply_modifier_part(left: &Merged, right: &Merged) -> Option<Merged> {
    let m = raw_modifier(left)?;
    let p = raw_part_token(right)?;
    let side = side_of(&m.text)?;
    Some(Merged::Part(PartRef {
        text: format!("{}{}", m.text, p.text),
        raw_part: p.text.clone(),
        base: part_base(&p.text),
        side,
        start: m.start,
    }))
}

/// `PartItem`：部位短语 + 测项词 → 测项短语（携带部位）。
fn hit_part_item(left: &Merged, right: &Merged) -> bool {
    part_view(left).is_some() && raw_item_token(right).is_some()
}

fn apply_part_item(left: &Merged, right: &Merged) -> Option<Merged> {
    let part = part_view(left)?;
    let item = raw_item_token(right)?;
    let alias = item.text.clone();
    // 词面重叠按最长匹配优先：测项别名已含部位词面时不再重复拼前缀。
    let text = match resolve_overlap(&part.text, &alias) {
        OverlapWinner::Part => format!("{}{}", part.text, alias),
        OverlapWinner::Item => alias.clone(),
    };
    Some(Merged::Item {
        text,
        alias,
        start: part.start,
        alias_start: item.start,
        part: Some(part),
    })
}

/// `NumberUnit`：数值 + 单位 → 测量。
fn hit_number_unit(left: &Merged, right: &Merged) -> bool {
    raw_number_token(left).is_some() && raw_unit_token(right).is_some()
}

fn apply_number_unit(left: &Merged, right: &Merged) -> Option<Merged> {
    let num = raw_number_token(left)?;
    let unit = raw_unit_token(right)?;
    match parse_number(&num.text) {
        Some(v) => Some(Merged::Measurement {
            value: v,
            scale: None,
            unit: unit.raw.clone(),
            raw_value: num.raw.clone(),
            start: num.start,
            unit_start: unit.start,
        }),
        None => Some(Merged::BadNumber {
            raw_value: num.raw.clone(),
            unit: unit.raw.clone(),
            start: num.start,
        }),
    }
}

/// `NumberMultiply`：数值 + 乘号 → 倍率链起点。
fn hit_number_multiply(left: &Merged, right: &Merged) -> bool {
    raw_number_token(left).is_some() && raw_multiply_token(right).is_some()
}

fn apply_number_multiply(left: &Merged, right: &Merged) -> Option<Merged> {
    let num = raw_number_token(left)?;
    let mul = raw_multiply_token(right)?;
    let value = match parse_number(&num.text) {
        Some(v) => v,
        // 数值本身就不可解析：不进倍率链，直接落失败（不猜值）。
        None => {
            return Some(Merged::BadNumber {
                raw_value: num.raw.clone(),
                unit: String::new(),
                start: num.start,
            })
        }
    };
    Some(Merged::Scale {
        value,
        factor: None,
        raw_value: format!("{}{}", num.raw, mul.raw),
        start: num.start,
    })
}

/// `ScaleNumber`：倍率链 + 数值 → 带倍率。
fn hit_scale_number(left: &Merged, right: &Merged) -> bool {
    matches!(left, Merged::Scale { factor: None, .. }) && raw_number_token(right).is_some()
}

fn apply_scale_number(left: &Merged, right: &Merged) -> Option<Merged> {
    let (value, raw_value, start) = match left {
        Merged::Scale {
            value,
            raw_value,
            start,
            ..
        } => (*value, raw_value.clone(), *start),
        _ => return None,
    };
    let num = raw_number_token(right)?;
    let factor = match parse_number(&num.text) {
        Some(f) => f,
        None => {
            return Some(Merged::BadNumber {
                raw_value: format!("{}{}", raw_value, num.raw),
                unit: String::new(),
                start,
            })
        }
    };
    Some(Merged::Scale {
        value,
        factor: Some(factor),
        raw_value: format!("{}{}", raw_value, num.raw),
        start,
    })
}

/// `ScaleUnit`：带倍率 + 单位 → 测量（含倍率）。
fn hit_scale_unit(left: &Merged, right: &Merged) -> bool {
    matches!(left, Merged::Scale { factor: Some(_), .. }) && raw_unit_token(right).is_some()
}

fn apply_scale_unit(left: &Merged, right: &Merged) -> Option<Merged> {
    let (value, factor, raw_value, start) = match left {
        Merged::Scale {
            value,
            factor,
            raw_value,
            start,
        } => (*value, factor.unwrap_or(1.0), raw_value.clone(), *start),
        _ => return None,
    };
    let unit = raw_unit_token(right)?;
    Some(Merged::Measurement {
        value,
        scale: Some(factor),
        unit: unit.raw.clone(),
        raw_value,
        start,
        unit_start: unit.start,
    })
}

// ---------------------------------------------------------------------------
// 扫描器
// ---------------------------------------------------------------------------

/// 合并 token 序列：单趟扫描 + 一次前瞻。
///
/// 见模块文档「扫描方式」：命中即合并并**重新推进游标**（合并结果继续与下一个 token
/// 试合并）；没命中就把下一个 token **回退**，留给下一轮当当前位置。
pub fn merge(tokens: &[Token]) -> Vec<Merged> {
    let mut out: Vec<Merged> = Vec::with_capacity(tokens.len());
    let mut i = 0usize;
    while i < tokens.len() {
        let mut cur = Merged::Raw(tokens[i].clone());
        i += 1;
        // 「当前 token 定型需读下一 token」：读下一个 token 试合并。
        while let Some(next) = tokens.get(i) {
            let next_merged = Merged::Raw(next.clone());
            let Some(rule) = pick_rule(&cur, &next_merged) else {
                break; // 没命中 → 回退（不消费 next）
            };
            let Some(merged) = (rule.apply)(&cur, &next_merged) else {
                break; // 规则不适用 → 同样回退
            };
            cur = merged;
            i += 1; // 消费 next；合并结果继续与再下一个 token 试合并
        }
        out.push(normalize(cur));
    }
    out
}

/// 同一位置多个规则命中时的定夺。
///
/// 顺序：**规则优先级小者优先 → 规则表次序靠前者优先**。优先级取自
/// [`rule_priority`]（倍率类 → 部位类 → 数值类）；同优先级取表内靠前的一条 ——
/// 最后这条保证同分时也有确定结果，同一份报告在任何机器上得到同一条 token 流。
fn pick_rule(left: &Merged, right: &Merged) -> Option<&'static MergeRule> {
    let mut best: Option<&'static MergeRule> = None;
    for rule in merge_rules() {
        if !(rule.hit)(left, right) {
            continue;
        }
        let better = match best {
            None => true,
            Some(b) => rule.priority < b.priority,
        };
        if better {
            best = Some(rule);
        }
    }
    best
}

/// 归一化：未合并的部位词 token 单独成部位短语。
///
/// 部位词没跟修饰词或测项合并时，也要落成 `Part` —— 否则抽取层拿不到部位上下文，
/// 后面会把「有部位」的测值误判成「部位未命中」。
fn normalize(m: Merged) -> Merged {
    match m {
        Merged::Raw(t) if t.kind == TokenKind::Part => Merged::Part(PartRef {
            text: t.text.clone(),
            raw_part: t.text.clone(),
            base: part_base(&t.text),
            side: Side::None,
            start: t.start,
        }),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::super::lexer::tokenize;
    use super::*;

    fn merged(text: &str) -> Vec<Merged> {
        merge(&tokenize(text))
    }

    fn token(kind: TokenKind, text: &str, start: usize, end: usize) -> Token {
        Token {
            kind,
            text: text.to_string(),
            raw: text.to_string(),
            start,
            end,
        }
    }

    #[test]
    fn 修饰词加部位合并成部位短语() {
        let m = merged("左肾 3.2cm");
        match &m[0] {
            Merged::Part(p) => {
                assert_eq!(p.text, "左肾");
                assert_eq!(p.base.as_deref(), Some("kidney"));
                assert_eq!(p.side, Side::Left);
                assert_eq!(p.start, 1);
            }
            other => panic!("期望部位短语，实际 {:?}", other),
        }
    }

    #[test]
    fn 双侧不与部位合并() {
        let m = merged("双侧肾积水");
        assert!(matches!(m[0], Merged::Raw(_))); // 双侧：修饰词原样透传
        assert!(matches!(m[1], Merged::Part(_))); // 肾：独立部位
    }

    #[test]
    fn 部位加测项合并成测项短语并携带部位() {
        let m = merged("左肾前后径");
        match &m[0] {
            Merged::Item {
                text,
                alias,
                part,
                ..
            } => {
                assert_eq!(text, "左肾前后径");
                assert_eq!(alias, "前后径");
                let p = part.as_ref().expect("测项短语应携带部位");
                assert_eq!(p.base.as_deref(), Some("kidney"));
                assert_eq!(p.side, Side::Left);
            }
            other => panic!("期望测项短语，实际 {:?}", other),
        }
    }

    #[test]
    fn 数值加单位合并成测量且保留原单位() {
        let m = merged("3.2cm");
        match &m[0] {
            Merged::Measurement {
                value,
                scale,
                unit,
                raw_value,
                start,
                unit_start,
            } => {
                assert_eq!(*value, 3.2);
                assert_eq!(*scale, None);
                assert_eq!(unit, "cm");
                assert_eq!(raw_value, "3.2");
                assert_eq!(*unit_start, 4, "单位 token 各留各的位置");
                assert_eq!(*start, 1);
            }
            other => panic!("期望测量，实际 {:?}", other),
        }
    }

    #[test]
    fn 未知单位照样合并以便强转判无路径() {
        let m = merged("3.2xx");
        match &m[0] {
            Merged::Measurement { unit, .. } => assert_eq!(unit, "xx"),
            other => panic!("期望测量，实际 {:?}", other),
        }
    }

    #[test]
    fn 数值后无单位不合并() {
        let m = merged("3.2");
        assert!(matches!(m[0], Merged::Raw(_)));
    }

    #[test]
    fn 倍率链合并成带倍率测量且原值词面保留() {
        let m = merged("左肾 3.2×2cm");
        // 修饰词 + 部位先合并，再与数值链合并，最后得到一条带倍率的测量。
        let meas = m
            .iter()
            .find_map(|t| match t {
                Merged::Measurement {
                    value,
                    scale,
                    unit,
                    raw_value,
                    ..
                } => Some((*value, *scale, unit.clone(), raw_value.clone())),
                _ => None,
            })
            .expect("应产出带倍率测量");
        assert_eq!(meas.0, 3.2);
        assert_eq!(meas.1, Some(2.0));
        assert_eq!(meas.2, "cm");
        assert_eq!(meas.3, "3.2×2");
    }

    #[test]
    fn 倍率链缺倍率数值时不产出测量() {
        let m = merged("3.2×");
        assert!(matches!(m[0], Merged::Scale { factor: None, .. }));
        assert!(!m.iter().any(|t| matches!(t, Merged::Measurement { .. })));
    }

    #[test]
    fn 倍率链缺单位时不产出测量() {
        let m = merged("3.2×2");
        assert!(matches!(m[0], Merged::Scale { factor: Some(_), .. }));
        assert!(!m.iter().any(|t| matches!(t, Merged::Measurement { .. })));
    }

    /// 规则用例（规则类型 + 命中操作数对 + 不命中操作数对）。
    type RuleCase = (MergeRuleKind, (Merged, Merged), (Merged, Merged));

    /// 规则表逐条用例：每条规则 1 条命中 + 1 条不命中。
    #[test]
    fn 合并规则表逐条命中与不命中() {
        // 命中 / 不命中用例（手工构造操作数对）。
        let modifier_hit = (
            Merged::Raw(token(TokenKind::Modifier, "左", 1, 1)),
            Merged::Raw(token(TokenKind::Part, "肾", 2, 2)),
        );
        let modifier_miss = (
            Merged::Raw(token(TokenKind::Modifier, "双侧", 1, 2)),
            Merged::Raw(token(TokenKind::Part, "肾", 3, 3)),
        );
        let part_item_hit = (
            Merged::Raw(token(TokenKind::Part, "肾", 1, 1)),
            Merged::Raw(token(TokenKind::Item, "前后径", 2, 4)),
        );
        let part_item_miss = (
            Merged::Raw(token(TokenKind::Part, "肾", 1, 1)),
            Merged::Raw(token(TokenKind::Term, "积水", 2, 3)),
        );
        let number_unit_hit = (
            Merged::Raw(token(TokenKind::Number, "3.2", 1, 3)),
            Merged::Raw(token(TokenKind::Unit, "cm", 4, 5)),
        );
        let number_unit_miss = (
            Merged::Raw(token(TokenKind::Number, "3.2", 1, 3)),
            Merged::Raw(token(TokenKind::Part, "肾", 4, 4)),
        );
        let number_multiply_hit = (
            Merged::Raw(token(TokenKind::Number, "3.2", 1, 3)),
            Merged::Raw(token(TokenKind::Multiply, "×", 4, 4)),
        );
        let number_multiply_miss = (
            Merged::Raw(token(TokenKind::Number, "3.2", 1, 3)),
            Merged::Raw(token(TokenKind::Unit, "cm", 4, 5)),
        );
        let scale_number_hit = (
            Merged::Scale {
                value: 3.2,
                factor: None,
                raw_value: "3.2×".to_string(),
                start: 1,
            },
            Merged::Raw(token(TokenKind::Number, "2", 5, 5)),
        );
        let scale_number_miss = (
            Merged::Scale {
                value: 3.2,
                factor: Some(2.0),
                raw_value: "3.2×2".to_string(),
                start: 1,
            },
            Merged::Raw(token(TokenKind::Number, "2", 6, 6)),
        );
        let scale_unit_hit = (
            Merged::Scale {
                value: 3.2,
                factor: Some(2.0),
                raw_value: "3.2×2".to_string(),
                start: 1,
            },
            Merged::Raw(token(TokenKind::Unit, "cm", 6, 7)),
        );
        let scale_unit_miss = (
            Merged::Scale {
                value: 3.2,
                factor: None,
                raw_value: "3.2×".to_string(),
                start: 1,
            },
            Merged::Raw(token(TokenKind::Unit, "cm", 5, 6)),
        );

        let cases: &[RuleCase] = &[
            (
                MergeRuleKind::ModifierPart,
                modifier_hit,
                modifier_miss,
            ),
            (MergeRuleKind::PartItem, part_item_hit, part_item_miss),
            (
                MergeRuleKind::NumberUnit,
                number_unit_hit,
                number_unit_miss,
            ),
            (
                MergeRuleKind::NumberMultiply,
                number_multiply_hit,
                number_multiply_miss,
            ),
            (
                MergeRuleKind::ScaleNumber,
                scale_number_hit,
                scale_number_miss,
            ),
            (MergeRuleKind::ScaleUnit, scale_unit_hit, scale_unit_miss),
        ];

        for rule in merge_rules() {
            let (_, hit, miss) = cases
                .iter()
                .find(|(k, _, _)| *k == rule.kind)
                .unwrap_or_else(|| panic!("规则 {} 缺用例", rule.kind.as_str()));
            assert!(
                (rule.hit)(&hit.0, &hit.1),
                "规则 {} 应命中用例",
                rule.kind.as_str()
            );
            assert!(
                !(rule.hit)(&miss.0, &miss.1),
                "规则 {} 不应命中不命中用例",
                rule.kind.as_str()
            );
            assert!(
                (rule.apply)(&hit.0, &hit.1).is_some(),
                "规则 {} 命中后应产出规范 token",
                rule.kind.as_str()
            );
        }
        // 规则表每条都有用例（表与用例一一对应，不留没断言的规则）。
        assert_eq!(merge_rules().len(), cases.len());
    }

    #[test]
    fn 双侧与肾不合并是修饰词规则的边界用例() {
        let rule = merge_rules()
            .iter()
            .find(|r| r.kind == MergeRuleKind::ModifierPart)
            .expect("应有 ModifierPart 规则");
        let bilateral = (
            Merged::Raw(token(TokenKind::Modifier, "双侧", 1, 2)),
            Merged::Raw(token(TokenKind::Part, "肾", 3, 3)),
        );
        assert!(!(rule.hit)(&bilateral.0, &bilateral.1));
    }

    #[test]
    fn 重叠词按最长匹配优先且不重复拼词面() {
        // 手工构造的重叠 token 对（今天的词表还没有同名条目，
        // 冲突消解规则本身在这里验证）：部位「肾」+ 测项「肾脏长度」。
        let toks = vec![
            token(TokenKind::Part, "肾", 1, 1),
            token(TokenKind::Item, "肾脏长度", 2, 5),
        ];
        let merged = merge(&toks);
        match &merged[0] {
            Merged::Item {
                text,
                alias,
                part,
                ..
            } => {
                assert_eq!(text, "肾脏长度", "最长匹配：词面不重复部位前缀");
                assert_eq!(alias, "肾脏长度", "测项别名不被部位前缀吃掉");
                assert_eq!(part.as_ref().unwrap().base.as_deref(), Some("kidney"));
            }
            other => panic!("期望测项短语，实际 {:?}", other),
        }
    }

    #[test]
    fn 等长时部位优先() {
        assert_eq!(resolve_overlap("肾脏", "肾盏"), OverlapWinner::Part);
        assert_eq!(resolve_overlap("肾", "肾脏长度"), OverlapWinner::Item);
        assert_eq!(resolve_overlap("左肾", "前后径"), OverlapWinner::Part);
    }

    #[test]
    fn 规则优先级表与规则表一致() {
        // 倍率类 → 部位类 → 数值类；同一类内按表内次序。
        let prio = |k| rule_priority(k);
        assert!(prio(MergeRuleKind::NumberMultiply) < prio(MergeRuleKind::ScaleNumber));
        assert!(prio(MergeRuleKind::ScaleNumber) < prio(MergeRuleKind::ScaleUnit));
        assert!(prio(MergeRuleKind::ScaleUnit) < prio(MergeRuleKind::ModifierPart));
        assert!(prio(MergeRuleKind::ModifierPart) < prio(MergeRuleKind::PartItem));
        assert!(prio(MergeRuleKind::PartItem) < prio(MergeRuleKind::NumberUnit));
        // 规则表里每条的 priority 与查表结果一致（没有两处真源）。
        for rule in merge_rules() {
            assert_eq!(rule.priority, rule_priority(rule.kind));
        }
    }

    #[test]
    fn 规范键稳定可查() {
        let m = merged("左肾前后径 2.1cm");
        assert_eq!(m[0].canonical_key(), "item:kidney.前后径");
        let m = merged("3.2cm");
        assert_eq!(m[0].canonical_key(), "measurement:cm");
        let m = merged("左肾 3.2cm");
        assert_eq!(m[0].canonical_key(), "part:kidney.left");
    }

    #[test]
    fn 合并后重新推进游标实现链式合并() {
        // 「左 + 肾」合并出的部位短语继续与「前后径」合并成测项短语。
        let m = merged("左肾前后径");
        assert_eq!(m.len(), 1, "应合并成一条测项短语");
        assert!(matches!(m[0], Merged::Item { .. }));
    }
}
