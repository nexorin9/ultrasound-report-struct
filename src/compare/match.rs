//! 对拍引擎第 2 层：规范键匹配 + 差值与方向计算。
//!
//! **这一层解决的接缝**：超声医师签报告前要回答一句话 ——
//! 「这个患者**同侧**这个测项，跟上次比是大了还是小了」。
//! 现在这件事靠手翻历史报告：翻到同一份、认出同一个侧别、抄下上次那个数、
//! 再心算差值。**最容易出错的就是「认错侧别」** ——
//! 拿右肾的值去比左肾，差值算得出来，结论却是错的。
//!
//! **规范键**（本层的匹配依据）：
//!
//! ```text
//! (patient_id, part_key, item_code)
//! ```
//!
//! 三个要素缺一不可 —— 缺患者认不出跟谁比，缺部位认不出哪一侧，
//! 缺测项认不出比的是哪一项。**三者齐全才成立一条键**。
//!
//! **两侧口径必须同源**：当前值来自本产品解析 + 强转（`value_si` / `unit_code`），
//! 历史值来自 RIS（`value` + `unit`）—— **RIS 侧的原值也要先经强转内核归一**
//! （[`crate::coerce::path::coerce`]），归一后才谈得上相减。
//! 单位不同（`cm` vs `mm`）**不是**问题：先归一再算。
//! **跨量纲族相减才是问题**（长度 mm vs 容量 mL）—— 那不是同一个测量，硬减出来的
//! 差值**看着正常却毫无意义**，所以宁可不比。
//!
//! **fail-closed 的三个「不比」**：
//!
//! 1. **部位认不出规范键** → 不比（`BODY_PART_UNRESOLVED`）——
//!    猜不出侧别的历史值，碰巧对上也是运气，不是依据；
//! 2. **历史值归一不出来**（未知单位 / 负值 / 超量级）→ 不比（`HISTORY_VALUE_NOT_COERCIBLE`）；
//! 3. **跨量纲族** → 不比（`FAMILY_MISMATCH`）。
//!
//! **差值方向**：差值 = 当前值 − 历史值（归一后同单位相减）；
//! 增大 / 减小 / 持平三态，持平按容差判（浮点残差不算「变了」）。
//!
//! **本层不做的两件事**（留给后续任务，边界写在这里免得越界）：
//!
//! - **三态判定**（有历史 / 无历史 / 多历史歧义）在本层的
//!   [`MatchOutcome`] 里表现为「候选条数」：**0 条 / 1 条 / 多条**，
//!   判定与歧义处置由 `compare::states` 落地 ——
//!   **本层命中多条时绝不任取其一**（那是最危险的失败方式：
//!   给出一个看起来正常、可能指错对象的差值）。
//! - **同侧可比性规则**由 [`crate::compare::side`] 的规则表落地；
//!   本层的部位闸**调用那张规则表**判定可比性 ——
//!   「左 vs 右」「双侧 vs 单侧」「侧别未标注」各有**独立的排除原因**，
//!   运维一眼分得清「拿错侧」与「口径不可比」。

use crate::coerce::dictionary::{items_dictionary, parts_dictionary};
use crate::coerce::path::coerce;
use crate::coerce::units::round3;
use crate::compare::ris_client::RisReport;
use crate::compare::side::{compare_sides, SideComparability, SideMismatch};
use crate::parse::extract::ParsedItem;

/// 差值判「持平」的容差（归一到 3 位小数后仍可能残留浮点尾数）。
const EPSILON: f64 = 1e-9;

/// **对拍规范键**：患者主索引 × 规范部位键 × 规范测项码。
///
/// 三者齐全才成立 —— 缺任何一样都**不构成一次可比的对拍**。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComparisonKey {
    /// 患者主索引。
    pub patient_id: String,
    /// 规范部位键（`kidney.left` 等）。
    pub part_key: String,
    /// 规范测项码（`kidney.ap_diameter` 等）。
    pub item_code: String,
}

impl ComparisonKey {
    /// 建一条规范键。
    pub fn new(
        patient_id: impl Into<String>,
        part_key: impl Into<String>,
        item_code: impl Into<String>,
    ) -> ComparisonKey {
        ComparisonKey {
            patient_id: patient_id.into(),
            part_key: part_key.into(),
            item_code: item_code.into(),
        }
    }

    /// 从一条**已解析条目**建规范键。
    ///
    /// 部位键或测项码缺一（词典未命中）→ **返回 `None`**：
    /// 键不全就没有可比的对象，宁可不比不错比。
    pub fn from_item(patient_id: impl Into<String>, item: &ParsedItem) -> Option<ComparisonKey> {
        let part_key = item.part_key.as_deref()?.trim();
        let item_code = item.item_code.as_deref()?.trim();
        if part_key.is_empty() || item_code.is_empty() {
            return None;
        }
        Some(ComparisonKey::new(patient_id, part_key, item_code))
    }
}

/// **当前测值**（已由解析 + 强转内核归一到院内口径）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CurrentMeasure {
    /// 归一值（长度 mm / 容量 mL）；`None` 表示这条测值没有可用数值。
    pub value_si: Option<f64>,
    /// 归一单位码（`mm` / `mL`）。
    pub unit_code: Option<String>,
}

impl CurrentMeasure {
    /// 从一条已解析条目取当前测值。
    pub fn from_item(item: &ParsedItem) -> CurrentMeasure {
        CurrentMeasure {
            value_si: item.value_si,
            unit_code: item.unit_code.clone(),
        }
    }
}

/// **差值方向**（增大 / 减小 / 持平）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// 增大：当前值比历史值大。
    Increased,
    /// 减小：当前值比历史值小。
    Decreased,
    /// 持平：两者在院内口径精度下相同。
    Unchanged,
}

impl Direction {
    /// 中文方向（进台账 / 差值清单 / 企微推送）。
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Increased => "增大",
            Direction::Decreased => "减小",
            Direction::Unchanged => "持平",
        }
    }

    /// 由差值判方向（容差内判持平）。
    ///
    /// `delta = 当前值 − 历史值`：正数增大、负数减小、零（或容差内）持平。
    pub fn from_delta(delta: f64) -> Direction {
        if delta > EPSILON {
            Direction::Increased
        } else if delta < -EPSILON {
            Direction::Decreased
        } else {
            Direction::Unchanged
        }
    }
}

/// **一条命中的历史测值**（已归一到院内口径）。
#[derive(Clone, Debug, PartialEq)]
pub struct HistoryHit {
    /// 历史报告检查号。
    pub study_no: String,
    /// 历史检查日期时间（RIS 侧原样带回）。
    pub performed_at: Option<String>,
    /// 历史原值（RIS 侧给的原数）。
    pub value_raw: Option<f64>,
    /// 历史原单位（RIS 侧给的原单位）。
    pub unit_raw: Option<String>,
    /// 归一值（长度 mm / 容量 mL）。
    pub value_si: f64,
    /// 归一单位码。
    pub unit_code: String,
}

/// **差值结果**（当前值 vs 历史值，同单位）。
#[derive(Clone, Debug, PartialEq)]
pub struct Delta {
    /// 历史值（归一后）。
    pub previous_value: f64,
    /// 当前值（归一后）。
    pub current_value: f64,
    /// 差值 = 当前值 − 历史值（3 位小数口径）。
    pub delta: f64,
    /// 方向。
    pub direction: Direction,
    /// 差值所在单位码。
    pub unit_code: String,
}

impl Delta {
    /// 算一次差值（同单位相减）。
    pub fn compute(current: f64, previous: f64, unit_code: &str) -> Delta {
        let delta = round3(current - previous);
        Delta {
            previous_value: previous,
            current_value: current,
            delta,
            direction: Direction::from_delta(delta),
            unit_code: unit_code.to_string(),
        }
    }
}

/// **一条历史被排除的原因**（稳定码，进运维摘要）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// 当前测值没有归一值 —— 无从对拍。
    CurrentValueMissing,
    /// 历史报告归属的患者与当前患者不同（**绝不跨患者比对**）。
    PatientMismatch,
    /// 历史报告没写部位 —— 无从判同侧。
    BodyPartMissing,
    /// 历史报告的部位写法归一到不了规范键 —— 无从判同侧。
    BodyPartUnresolved,
    /// 历史报告的部位与当前**不是同一个器官**（肾 vs 膀胱）。
    BodyPartMismatch,
    /// **左右侧不同** —— 当前左、历史右（或反之）。
    ///
    /// 与 [`SkipReason::BodyPartMismatch`] 分开：一个是「拿错侧」，
    /// 一个是「拿错器官」—— 处置动作不同，运维不能只看一个码。
    SideMismatch,
    /// **双侧 ↔ 单侧不可比** —— 「双肾」的值与「左肾」的值不是同一个测量。
    BilateralVsUnilateral,
    /// **侧别未标注** —— 成对器官没写清哪一侧，无法确认是同一侧。
    SideUnspecified,
    /// 历史测项与当前测项不同。
    ItemMismatch,
    /// 历史测值缺数值。
    HistoryValueMissing,
    /// 历史测值缺单位 —— 归一不了。
    HistoryUnitMissing,
    /// 历史测值归一到不了院内口径（未知单位 / 负值 / 超量级）。
    HistoryValueNotCoercible,
    /// 历史值与当前值不在同一量纲族（长度 vs 容量）—— 不硬减。
    FamilyMismatch,
}

impl SkipReason {
    /// 稳定码（进日志与运维摘要）。
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::CurrentValueMissing => "CURRENT_VALUE_MISSING",
            SkipReason::PatientMismatch => "PATIENT_MISMATCH",
            SkipReason::BodyPartMissing => "BODY_PART_MISSING",
            SkipReason::BodyPartUnresolved => "BODY_PART_UNRESOLVED",
            SkipReason::BodyPartMismatch => "BODY_PART_MISMATCH",
            SkipReason::SideMismatch => "SIDE_MISMATCH",
            SkipReason::BilateralVsUnilateral => "BILATERAL_VS_UNILATERAL",
            SkipReason::SideUnspecified => "SIDE_UNSPECIFIED",
            SkipReason::ItemMismatch => "ITEM_MISMATCH",
            SkipReason::HistoryValueMissing => "HISTORY_VALUE_MISSING",
            SkipReason::HistoryUnitMissing => "HISTORY_UNIT_MISSING",
            SkipReason::HistoryValueNotCoercible => "HISTORY_VALUE_NOT_COERCIBLE",
            SkipReason::FamilyMismatch => "FAMILY_MISMATCH",
        }
    }

    /// 中文说明（人看的处置理由）。
    pub fn message(self) -> &'static str {
        match self {
            SkipReason::CurrentValueMissing => "当前测值没有归一值，无从对拍",
            SkipReason::PatientMismatch => "历史报告归属的患者与当前患者不同，不跨患者比对",
            SkipReason::BodyPartMissing => "历史报告未写部位，无从判同侧",
            SkipReason::BodyPartUnresolved => "历史报告的部位写法归一到不了规范键，无从判同侧",
            SkipReason::BodyPartMismatch => "历史报告的部位与当前不是同一个器官",
            SkipReason::SideMismatch => "历史报告与当前左右侧不同，不是同一侧",
            SkipReason::BilateralVsUnilateral => "双侧值与单侧值不是同一个测量，不相减",
            SkipReason::SideUnspecified => "成对器官未标注侧别，无法确认是同一侧",
            SkipReason::ItemMismatch => "历史测项与当前测项不同",
            SkipReason::HistoryValueMissing => "历史测值缺数值",
            SkipReason::HistoryUnitMissing => "历史测值缺单位，归一到不了院内口径",
            SkipReason::HistoryValueNotCoercible => "历史测值归一到不了院内口径（未知单位／负值／超量级）",
            SkipReason::FamilyMismatch => "历史值与当前值不在同一量纲族，不相减",
        }
    }

    /// **侧别规则表判出来的不可比原因 → 本层的排除原因**。
    ///
    /// 「不是同一个器官」仍归到 [`SkipReason::BodyPartMismatch`]（部位问题），
    /// 其余三种侧别问题各有独立码 —— 运维按码聚合时能分清
    /// 「拿错侧」「口径不可比」「侧别没写清」。
    pub fn from_side_mismatch(mismatch: SideMismatch) -> SkipReason {
        match mismatch {
            SideMismatch::DifferentBase => SkipReason::BodyPartMismatch,
            SideMismatch::DifferentSide => SkipReason::SideMismatch,
            SideMismatch::BilateralVsUnilateral => SkipReason::BilateralVsUnilateral,
            SideMismatch::SideUnspecified => SkipReason::SideUnspecified,
        }
    }
}

/// **被排除的一条历史**（连同原因，便于运维对账「为什么这条没比上」）。
#[derive(Clone, Debug, PartialEq)]
pub struct SkippedHistory {
    /// 历史报告检查号。
    pub study_no: String,
    /// 历史测项码（RIS 侧原样）。
    pub item_code: Option<String>,
    /// 排除原因。
    pub reason: SkipReason,
}

/// **一次匹配的结果**。
///
/// **候选条数即三态的原料**（判定留给 `compare::states`）：
///
/// | 候选条数 | 含义 | 本层的差值 |
/// |---|---|---|
/// | 0 | 无历史（首次） | `None` |
/// | 1 | 有历史 | `Some`（唯一命中才给差值） |
/// | > 1 | 多历史歧义 | `None`（**绝不任取其一**） |
#[derive(Clone, Debug, PartialEq)]
pub struct MatchOutcome {
    /// 本次匹配用的规范键。
    pub key: ComparisonKey,
    /// 命中的历史测值（按 RIS 返回顺序）。
    pub hits: Vec<HistoryHit>,
    /// 差值（**仅在恰好命中一条且量纲可比时有值**）。
    pub delta: Option<Delta>,
    /// 被排除的历史条目及原因。
    pub skipped: Vec<SkippedHistory>,
}

impl MatchOutcome {
    /// 命中条数。
    pub fn hit_count(&self) -> usize {
        self.hits.len()
    }

    /// 是否恰好命中一条（有历史）。
    pub fn is_single_hit(&self) -> bool {
        self.hits.len() == 1
    }

    /// 是否存在多历史歧义（命中多条）。
    ///
    /// **歧义时本层不产出差值** —— 任取其一可能指错对象。
    pub fn is_ambiguous(&self) -> bool {
        self.hits.len() > 1
    }

    /// 某条排除原因的条数（运维按原因聚合）。
    pub fn skipped_count(&self, reason: SkipReason) -> usize {
        self.skipped.iter().filter(|s| s.reason == reason).count()
    }
}

/// **部位写法 → 规范部位键**的查找结果。
#[derive(Clone, Debug, PartialEq, Eq)]
enum PartLookup {
    /// 认得出规范键。
    Canonical(String),
    /// 认不出（词典未收录 / 词典加载失败）—— **fail-closed**。
    Unresolved,
}

/// 查部位写法对应的规范部位键。
///
/// 两种写法都认：**已是规范键**（RIS 侧直接给 `kidney.left`）
/// 与**院内别名**（`左肾`）。两者都认不出 → [`PartLookup::Unresolved`]。
fn lookup_part_key(raw: &str) -> PartLookup {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return PartLookup::Unresolved;
    }
    let dict = match parts_dictionary() {
        Ok(dict) => dict,
        // 词典加载不了就认不出部位 —— 不猜。
        Err(_) => return PartLookup::Unresolved,
    };
    if let Some(entry) = dict.entries().iter().find(|e| e.part_key == trimmed) {
        return PartLookup::Canonical(entry.part_key.clone());
    }
    match dict.part_key_of_alias(trimmed) {
        Some(key) => PartLookup::Canonical(key.to_string()),
        None => PartLookup::Unresolved,
    }
}

/// 规范部位键 → 解剖基名（`kidney.left` → `kidney`）。
///
/// 基名是查测项词典的钥匙（测项码 = 基名 + `.` + 后缀）。
fn base_of_part_key(part_key: &str) -> Option<String> {
    let dict = parts_dictionary().ok()?;
    dict.entries()
        .iter()
        .find(|e| e.part_key == part_key)
        .map(|e| e.base.clone())
}

/// 查测项写法对应的规范测项码。
///
/// **两种写法都认**：已是规范码（`kidney.ap_diameter`）
/// 与**测项词面**（`前后径`）。词面按**部位基名**限定 ——
/// 「残余尿量」只归到膀胱基名下，不会在肾上编出一个不存在的测项码。
fn lookup_item_code(raw: &str, base: Option<&str>) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let base = base?;
    let dict = items_dictionary().ok()?;
    // 写法一：测项词面 → 规范码。
    if let Some(suffix) = dict.suffix_of(trimmed, Some(base)) {
        return Some(format!("{}.{}", base, suffix));
    }
    // 写法二：已经是规范码形状 —— 逐条核对它确实是词典里的真码。
    for entry in dict.entries() {
        if entry.bases.iter().any(|b| b == base)
            && format!("{}.{}", base, entry.suffix) == trimmed
        {
            return Some(trimmed.to_string());
        }
    }
    None
}

/// **按规范键匹配历史条目，并算出差值**。
///
/// `history` 是 RIS 取回的历史报告列表（[`RisReport`]）。
/// 匹配逐份报告做三道闸：**患者 → 部位（同侧）→ 测项**，
/// 三道都过才比数值；任一道不过就把该份历史**连同原因**记进
/// [`MatchOutcome::skipped`] —— 医师与运维看得见「这条为什么没比上」。
///
/// **差值只在恰好命中一条时给出** —— 命中多条是歧义，
/// 由三态判定层处置（**不任取其一**）；命中零条是「无历史」。
pub fn match_history(
    key: &ComparisonKey,
    current: &CurrentMeasure,
    history: &[RisReport],
) -> MatchOutcome {
    let mut outcome = MatchOutcome {
        key: key.clone(),
        hits: Vec::new(),
        delta: None,
        skipped: Vec::new(),
    };

    // 当前测值没有归一值 —— 无从对拍，直接返回（不产出差值）。
    let (current_value, current_unit) = match (current.value_si, current.unit_code.as_deref()) {
        (Some(v), Some(u)) if !u.trim().is_empty() => (v, u.trim().to_string()),
        _ => {
            outcome.skipped.push(SkippedHistory {
                study_no: String::new(),
                item_code: Some(key.item_code.clone()),
                reason: SkipReason::CurrentValueMissing,
            });
            return outcome;
        }
    };

    // 部位基名（查测项词典的钥匙）—— 由**当前**部位键决定，
    // 历史侧必须归一到同一个部位键才谈得上比测项。
    let base = base_of_part_key(&key.part_key);

    for report in history {
        // 闸一：患者。显式不同的患者绝不比对。
        if let Some(patient) = report.patient_id.as_deref() {
            if !patient.trim().is_empty() && patient.trim() != key.patient_id.trim() {
                outcome.skipped.push(SkippedHistory {
                    study_no: report.study_no.clone(),
                    item_code: None,
                    reason: SkipReason::PatientMismatch,
                });
                continue;
            }
        }

        // 闸二：部位（同侧）。认不出规范键 → 不比。
        let body_part = match report.body_part.as_deref() {
            Some(part) if !part.trim().is_empty() => part.trim().to_string(),
            _ => {
                outcome.skipped.push(SkippedHistory {
                    study_no: report.study_no.clone(),
                    item_code: None,
                    reason: SkipReason::BodyPartMissing,
                });
                continue;
            }
        };
        let part_key = match lookup_part_key(&body_part) {
            PartLookup::Canonical(key) => key,
            PartLookup::Unresolved => {
                outcome.skipped.push(SkippedHistory {
                    study_no: report.study_no.clone(),
                    item_code: None,
                    reason: SkipReason::BodyPartUnresolved,
                });
                continue;
            }
        };
        // **同侧判定规则表**：可比 = 同一个器官 **且** 侧别能对上。
        // 「左 vs 右」「双侧 vs 单侧」「侧别未标注」各有独立排除原因 ——
        // 三种都比「部位不同」细，运维按码聚合时能分清该改哪一头。
        if let SideComparability::NotComparable(mismatch) =
            compare_sides(&key.part_key, &part_key)
        {
            outcome.skipped.push(SkippedHistory {
                study_no: report.study_no.clone(),
                item_code: None,
                reason: SkipReason::from_side_mismatch(mismatch),
            });
            continue;
        }

        // 闸三：测项。逐条测量值判 —— 一份报告里测项可以有多条。
        for measurement in &report.measurements {
            let item_code = match lookup_item_code(&measurement.item_code, base.as_deref()) {
                Some(code) => code,
                None => {
                    outcome.skipped.push(SkippedHistory {
                        study_no: report.study_no.clone(),
                        item_code: Some(measurement.item_code.clone()),
                        reason: SkipReason::ItemMismatch,
                    });
                    continue;
                }
            };
            if item_code != key.item_code {
                outcome.skipped.push(SkippedHistory {
                    study_no: report.study_no.clone(),
                    item_code: Some(measurement.item_code.clone()),
                    reason: SkipReason::ItemMismatch,
                });
                continue;
            }

            // 数值：缺值 / 缺单位 / 归一不出来，都不比。
            let raw_value = match measurement.value {
                Some(value) => value,
                None => {
                    outcome.skipped.push(SkippedHistory {
                        study_no: report.study_no.clone(),
                        item_code: Some(measurement.item_code.clone()),
                        reason: SkipReason::HistoryValueMissing,
                    });
                    continue;
                }
            };
            let raw_unit = match measurement.unit.as_deref() {
                Some(unit) if !unit.trim().is_empty() => unit.trim().to_string(),
                _ => {
                    outcome.skipped.push(SkippedHistory {
                        study_no: report.study_no.clone(),
                        item_code: Some(measurement.item_code.clone()),
                        reason: SkipReason::HistoryUnitMissing,
                    });
                    continue;
                }
            };
            // **历史值也走强转内核**：RIS 给的是原值原单位，
            // 与当前值同口径后才谈得上相减（`cm` vs `mm` 在此归一）。
            let (value_si, unit_code) = match coerce(raw_value, &raw_unit) {
                Some(pair) => pair,
                None => {
                    outcome.skipped.push(SkippedHistory {
                        study_no: report.study_no.clone(),
                        item_code: Some(measurement.item_code.clone()),
                        reason: SkipReason::HistoryValueNotCoercible,
                    });
                    continue;
                }
            };
            // 跨量纲族（长度 vs 容量）—— 不硬减。
            if unit_code != current_unit {
                outcome.skipped.push(SkippedHistory {
                    study_no: report.study_no.clone(),
                    item_code: Some(measurement.item_code.clone()),
                    reason: SkipReason::FamilyMismatch,
                });
                continue;
            }

            outcome.hits.push(HistoryHit {
                study_no: report.study_no.clone(),
                performed_at: report.performed_at.clone(),
                value_raw: measurement.value,
                unit_raw: measurement.unit.clone(),
                value_si,
                unit_code,
            });
        }
    }

    // 差值只在**恰好命中一条**时给出；多条是歧义（交三态判定层处置），零条是无历史。
    if outcome.hits.len() == 1 {
        let hit = &outcome.hits[0];
        outcome.delta = Some(Delta::compute(current_value, hit.value_si, &hit.unit_code));
    }
    outcome
}

/// 从一条已解析条目对拍（部位键或测项码缺失 → `None`：键不全就没有可比对象）。
pub fn match_item(
    patient_id: &str,
    item: &ParsedItem,
    history: &[RisReport],
) -> Option<MatchOutcome> {
    let key = ComparisonKey::from_item(patient_id, item)?;
    let current = CurrentMeasure::from_item(item);
    Some(match_history(&key, &current, history))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::ris_client::RisReportMeasurement;

    /// 造一份历史报告（三道闸都过的最小形状）。
    fn report(
        study_no: &str,
        patient_id: &str,
        body_part: &str,
        performed_at: &str,
        measurements: Vec<(&str, Option<f64>, Option<&str>)>,
    ) -> RisReport {
        RisReport {
            study_no: study_no.to_string(),
            patient_id: Some(patient_id.to_string()),
            body_part: Some(body_part.to_string()),
            performed_at: Some(performed_at.to_string()),
            measurements: measurements
                .into_iter()
                .map(|(item_code, value, unit)| RisReportMeasurement {
                    item_code: item_code.to_string(),
                    value,
                    unit: unit.map(|u| u.to_string()),
                })
                .collect(),
        }
    }

    fn key() -> ComparisonKey {
        ComparisonKey::new("PID-EXAMPLE", "kidney.left", "kidney.ap_diameter")
    }

    fn current(value: f64, unit: &str) -> CurrentMeasure {
        CurrentMeasure {
            value_si: Some(value),
            unit_code: Some(unit.to_string()),
        }
    }

    #[test]
    fn 三组手算基准差值与方向都对() {
        // 基准一：32mm vs 28mm → 差 4mm，增大。
        let history = vec![report(
            "S-HIST-01",
            "PID-EXAMPLE",
            "kidney.left",
            "2026-01-01",
            vec![("kidney.ap_diameter", Some(28.0), Some("mm"))],
        )];
        let outcome = match_history(&key(), &current(32.0, "mm"), &history);
        let delta = outcome.delta.expect("命中一条应有差值");
        assert_eq!(delta.previous_value, 28.0);
        assert_eq!(delta.current_value, 32.0);
        assert_eq!(delta.delta, 4.0);
        assert_eq!(delta.direction, Direction::Increased);
        assert_eq!(delta.direction.as_str(), "增大");
        assert_eq!(delta.unit_code, "mm");

        // 基准二：25mm vs 32mm → 差 -7mm，减小。
        let history = vec![report(
            "S-HIST-01",
            "PID-EXAMPLE",
            "kidney.left",
            "2026-01-01",
            vec![("kidney.ap_diameter", Some(32.0), Some("mm"))],
        )];
        let delta = match_history(&key(), &current(25.0, "mm"), &history).delta.unwrap();
        assert_eq!(delta.delta, -7.0);
        assert_eq!(delta.direction, Direction::Decreased);

        // 基准三：28mm vs 28mm → 差 0，持平。
        let history = vec![report(
            "S-HIST-01",
            "PID-EXAMPLE",
            "kidney.left",
            "2026-01-01",
            vec![("kidney.ap_diameter", Some(28.0), Some("mm"))],
        )];
        let delta = match_history(&key(), &current(28.0, "mm"), &history).delta.unwrap();
        assert_eq!(delta.delta, 0.0);
        assert_eq!(delta.direction, Direction::Unchanged);
        assert_eq!(delta.direction.as_str(), "持平");
    }

    #[test]
    fn 单位不同先归一再算差值() {
        // 历史给 cm，当前是 mm：先归一（2.8cm → 28mm）再相减。
        let history = vec![report(
            "S-HIST-02",
            "PID-EXAMPLE",
            "kidney.left",
            "2026-01-01",
            vec![("kidney.ap_diameter", Some(2.8), Some("cm"))],
        )];
        let outcome = match_history(&key(), &current(32.0, "mm"), &history);
        let delta = outcome.delta.expect("跨写法也应能比");
        assert_eq!(delta.previous_value, 28.0);
        assert_eq!(delta.delta, 4.0);
        assert_eq!(delta.direction, Direction::Increased);
        assert_eq!(delta.unit_code, "mm");
        // 历史原值原单位保留，便于回溯「上次那边写的是 2.8cm」。
        assert_eq!(outcome.hits[0].value_raw, Some(2.8));
        assert_eq!(outcome.hits[0].unit_raw.as_deref(), Some("cm"));
    }

    #[test]
    fn 跨量纲族不相减() {
        // 膀胱残余尿量（mL）与肾前后径（mm）不是同一个测量 —— 不硬减。
        let history = vec![report(
            "S-HIST-03",
            "PID-EXAMPLE",
            "kidney.left",
            "2026-01-01",
            vec![("kidney.ap_diameter", Some(28.0), Some("ml"))],
        )];
        let outcome = match_history(&key(), &current(32.0, "mm"), &history);
        assert!(outcome.delta.is_none());
        assert_eq!(outcome.hit_count(), 0);
        assert_eq!(outcome.skipped_count(SkipReason::FamilyMismatch), 1);
    }

    #[test]
    fn 不同侧不同部位都排除并留原因() {
        // 右肾的历史值不该拿去跟左肾比 —— **拿错侧**有独立的原因码
        // （与「不是同一个器官」分开，运维按码聚合时要能分清）。
        let history = vec![report(
            "S-HIST-04",
            "PID-EXAMPLE",
            "kidney.right",
            "2026-01-01",
            vec![("kidney.ap_diameter", Some(28.0), Some("mm"))],
        )];
        let outcome = match_history(&key(), &current(32.0, "mm"), &history);
        assert_eq!(outcome.hit_count(), 0);
        assert_eq!(outcome.skipped_count(SkipReason::SideMismatch), 1);
        assert_eq!(outcome.skipped_count(SkipReason::BodyPartMismatch), 0);
        assert!(outcome.delta.is_none());
    }

    #[test]
    fn 不同器官排除并留部位原因() {
        // 膀胱的历史值不该拿去跟左肾比 —— 这是「不是同一个器官」。
        let history = vec![report(
            "S-HIST-04B",
            "PID-EXAMPLE",
            "bladder",
            "2026-01-01",
            vec![("kidney.ap_diameter", Some(28.0), Some("mm"))],
        )];
        let outcome = match_history(&key(), &current(32.0, "mm"), &history);
        assert_eq!(outcome.hit_count(), 0);
        assert_eq!(outcome.skipped_count(SkipReason::BodyPartMismatch), 1);
        assert_eq!(outcome.skipped_count(SkipReason::SideMismatch), 0);
        assert!(outcome.delta.is_none());
    }

    #[test]
    fn 双侧历史与单侧当前不可比() {
        // 「双肾 3.2cm」与「左肾 2.8cm」不是同一个测量 —— 不相减。
        let history = vec![report(
            "S-HIST-04C",
            "PID-EXAMPLE",
            "kidney.bilateral",
            "2026-01-01",
            vec![("kidney.ap_diameter", Some(28.0), Some("mm"))],
        )];
        let outcome = match_history(&key(), &current(32.0, "mm"), &history);
        assert_eq!(outcome.hit_count(), 0);
        assert_eq!(
            outcome.skipped_count(SkipReason::BilateralVsUnilateral),
            1
        );
        assert!(outcome.delta.is_none());
    }

    #[test]
    fn 部位写法归一到规范键后能对上() {
        // RIS 侧写院内别名「左肾」—— 归一到 kidney.left 后可比。
        let history = vec![report(
            "S-HIST-05",
            "PID-EXAMPLE",
            "左肾",
            "2026-01-01",
            vec![("kidney.ap_diameter", Some(28.0), Some("mm"))],
        )];
        let outcome = match_history(&key(), &current(32.0, "mm"), &history);
        assert_eq!(outcome.hit_count(), 1);
        assert_eq!(outcome.delta.unwrap().delta, 4.0);
    }

    #[test]
    fn 部位认不出规范键就不比() {
        let history = vec![report(
            "S-HIST-06",
            "PID-EXAMPLE",
            "肝门部",
            "2026-01-01",
            vec![("kidney.ap_diameter", Some(28.0), Some("mm"))],
        )];
        let outcome = match_history(&key(), &current(32.0, "mm"), &history);
        assert_eq!(outcome.hit_count(), 0);
        assert_eq!(outcome.skipped_count(SkipReason::BodyPartUnresolved), 1);
        assert!(outcome.delta.is_none());
    }

    #[test]
    fn 历史报告没写部位就不比() {
        let mut r = report(
            "S-HIST-07",
            "PID-EXAMPLE",
            "kidney.left",
            "2026-01-01",
            vec![("kidney.ap_diameter", Some(28.0), Some("mm"))],
        );
        r.body_part = None;
        let outcome = match_history(&key(), &current(32.0, "mm"), &[r]);
        assert_eq!(outcome.skipped_count(SkipReason::BodyPartMissing), 1);
        assert!(outcome.delta.is_none());
    }

    #[test]
    fn 不同患者的历史绝不比对() {
        let history = vec![report(
            "S-HIST-08",
            "PID-OTHER",
            "kidney.left",
            "2026-01-01",
            vec![("kidney.ap_diameter", Some(28.0), Some("mm"))],
        )];
        let outcome = match_history(&key(), &current(32.0, "mm"), &history);
        assert_eq!(outcome.hit_count(), 0);
        assert_eq!(outcome.skipped_count(SkipReason::PatientMismatch), 1);
    }

    #[test]
    fn 测项不同不参与对拍() {
        let history = vec![report(
            "S-HIST-09",
            "PID-EXAMPLE",
            "kidney.left",
            "2026-01-01",
            vec![("kidney.length", Some(85.0), Some("mm"))],
        )];
        let outcome = match_history(&key(), &current(32.0, "mm"), &history);
        assert_eq!(outcome.hit_count(), 0);
        assert_eq!(outcome.skipped_count(SkipReason::ItemMismatch), 1);
    }

    #[test]
    fn 历史值缺值缺单位归一不了都排除() {
        let history = vec![
            report(
                "S-HIST-10",
                "PID-EXAMPLE",
                "kidney.left",
                "2026-01-01",
                vec![("kidney.ap_diameter", None, Some("mm"))],
            ),
            report(
                "S-HIST-11",
                "PID-EXAMPLE",
                "kidney.left",
                "2026-01-01",
                vec![("kidney.ap_diameter", Some(28.0), None)],
            ),
            report(
                "S-HIST-12",
                "PID-EXAMPLE",
                "kidney.left",
                "2026-01-01",
                vec![("kidney.ap_diameter", Some(28.0), Some("xx"))],
            ),
        ];
        let outcome = match_history(&key(), &current(32.0, "mm"), &history);
        assert_eq!(outcome.hit_count(), 0);
        assert_eq!(outcome.skipped_count(SkipReason::HistoryValueMissing), 1);
        assert_eq!(outcome.skipped_count(SkipReason::HistoryUnitMissing), 1);
        assert_eq!(outcome.skipped_count(SkipReason::HistoryValueNotCoercible), 1);
        assert!(outcome.delta.is_none());
    }

    #[test]
    fn 命中多条是歧义不产差值() {
        // **绝不任取其一** —— 取错一份历史，差值会指着错的对象。
        let history = vec![
            report(
                "S-HIST-13",
                "PID-EXAMPLE",
                "kidney.left",
                "2026-01-01",
                vec![("kidney.ap_diameter", Some(28.0), Some("mm"))],
            ),
            report(
                "S-HIST-14",
                "PID-EXAMPLE",
                "kidney.left",
                "2026-06-01",
                vec![("kidney.ap_diameter", Some(30.0), Some("mm"))],
            ),
        ];
        let outcome = match_history(&key(), &current(32.0, "mm"), &history);
        assert_eq!(outcome.hit_count(), 2);
        assert!(outcome.is_ambiguous());
        assert!(!outcome.is_single_hit());
        assert!(outcome.delta.is_none());
    }

    #[test]
    fn 没有历史时给零命中() {
        let outcome = match_history(&key(), &current(32.0, "mm"), &[]);
        assert_eq!(outcome.hit_count(), 0);
        assert!(!outcome.is_ambiguous());
        assert!(outcome.delta.is_none());
    }

    #[test]
    fn 当前测值没有归一值时直接不产出() {
        let empty = CurrentMeasure {
            value_si: None,
            unit_code: None,
        };
        let outcome = match_history(&key(), &empty, &[]);
        assert_eq!(outcome.skipped_count(SkipReason::CurrentValueMissing), 1);
        assert!(outcome.delta.is_none());
    }

    #[test]
    fn 规范键从条目派生_键不全不成立() {
        let mut item = ParsedItem {
            part_key: Some("kidney.left".to_string()),
            item_code: Some("kidney.ap_diameter".to_string()),
            value_raw: Some(3.2),
            unit_raw: Some("cm".to_string()),
            value_si: Some(32.0),
            unit_code: Some("mm".to_string()),
            coercion_path: crate::coerce::path::CoercionPath::UnitConvert,
            line: 1,
            column: 6,
            char_start: 5,
            notes: Vec::new(),
            dictionary_version: crate::dictionary_version(),
        };
        assert_eq!(
            ComparisonKey::from_item("PID-EXAMPLE", &item).unwrap(),
            key()
        );

        // 部位键缺失 → 键不成立（宁可不比不错比）。
        item.part_key = None;
        assert!(ComparisonKey::from_item("PID-EXAMPLE", &item).is_none());

        // 测项码缺失 → 键不成立。
        item.part_key = Some("kidney.left".to_string());
        item.item_code = None;
        assert!(ComparisonKey::from_item("PID-EXAMPLE", &item).is_none());
    }

    #[test]
    fn 从条目对拍走同一条口径() {
        let item = ParsedItem {
            part_key: Some("kidney.left".to_string()),
            item_code: Some("kidney.ap_diameter".to_string()),
            value_raw: Some(3.2),
            unit_raw: Some("cm".to_string()),
            value_si: Some(32.0),
            unit_code: Some("mm".to_string()),
            coercion_path: crate::coerce::path::CoercionPath::UnitConvert,
            line: 1,
            column: 6,
            char_start: 5,
            notes: Vec::new(),
            dictionary_version: crate::dictionary_version(),
        };
        let history = vec![report(
            "S-HIST-15",
            "PID-EXAMPLE",
            "kidney.left",
            "2026-01-01",
            vec![("kidney.ap_diameter", Some(2.8), Some("cm"))],
        )];
        let outcome = match_item("PID-EXAMPLE", &item, &history).unwrap();
        assert_eq!(outcome.delta.unwrap().delta, 4.0);
    }
}
