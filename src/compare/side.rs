//! 对拍引擎第 4 层（一）：**同侧判定规则表**。
//!
//! **这一层解决的接缝**：超声报告里写的是「左肾」「右肾」「双肾」「膀胱」，
//! 而历史值那一侧可能写的是「左肾」「左侧肾脏」甚至只有「肾」。
//! 拿左肾的值去比右肾，差值算得出来、看着正常，结论却是**错的** ——
//! 这是差值类台账最隐蔽的一种错法：它不报错，只是悄悄指错对象。
//!
//! **规则表的口径（逐条可断言）**：可比 = **同一个器官** 且 **侧别能对上**。
//!
//! | 当前 \ 历史 | 左侧 | 右侧 | 双侧 | 未标注侧别 | 无侧别概念（单器官） |
//! |---|---|---|---|---|---|
//! | **左侧** | ✅ 可比 | ❌ 不同侧 | ❌ 双侧↔单侧 | ❌ 侧别未标注 | ❌ 不同器官 |
//! | **右侧** | ❌ 不同侧 | ✅ 可比 | ❌ 双侧↔单侧 | ❌ 侧别未标注 | ❌ 不同器官 |
//! | **双侧** | ❌ 双侧↔单侧 | ❌ 双侧↔单侧 | ✅ 可比 | ❌ 侧别未标注 | ❌ 不同器官 |
//! | **未标注侧别** | ❌ 侧别未标注 | ❌ 侧别未标注 | ❌ 侧别未标注 | ❌ 侧别未标注 | ❌ 不同器官 |
//! | **无侧别概念** | ❌ 不同器官 | ❌ 不同器官 | ❌ 不同器官 | ❌ 不同器官 | ✅ 可比 |
//!
//! **三条要命的口径，逐条说清为什么**：
//!
//! 1. **双侧 ↔ 单侧不可比** —— 「双肾 3.2cm」与「左肾 2.8cm」**不是同一个测量**。
//!    双侧值是两侧合并或取代表值，单侧值只覆盖一侧；
//!    两者相减出来的差值**看着像变化、其实可能什么都没变**。宁可不比。
//! 2. **未标注侧别不可比（连同为未标注也不可比）** —— 成对器官写「肾」而没写左/右，
//!    拿它去比另一条同样没写侧别的「肾」，**可能拿左肾比右肾**。
//!    两次都「没写」不构成「是同一侧」的证据 —— 口径按**能确认才算**执行。
//! 3. **单器官（无侧别概念）可比** —— 膀胱只有一个，
//!    「膀胱残余尿量 120mL」与历史同项当然可比。
//!    把单器官也按「侧别未标注」处理，会把整类膀胱测量全部排除掉 ——
//!    **fail-closed 也要有分寸：挡掉的是「比错对象」，不是「比不成」**。
//!
//! **侧别从哪来**：**部位词典自己的 `side` 字段**（`left` / `right` / `bilateral` / `none`），
//! 不在代码里另写一份侧别表 —— 口径变了词典版本号跟着变，差值也就跟着换口径。
//! 词典查不到的写法按「基名 + 侧别后缀」的构成规则拆，**拆不出侧别就是未标注**。
//!
//! **本层只判可比性，不算差值**：差值仍由 [`crate::compare::r#match`] 算，
//! 本层只回答「这两个部位能不能放在一起比」。

use crate::coerce::dictionary::parts_dictionary;

/// **侧别**（成对器官的左右 / 双侧；单器官为无侧别概念）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Laterality {
    /// 左侧。
    Left,
    /// 右侧。
    Right,
    /// 双侧。
    Bilateral,
    /// **未标注侧别** —— 成对器官没写左 / 右 / 双侧。
    ///
    /// 与 [`Laterality::NotApplicable`] 是两回事：这里是「该写却没写」，
    /// 那里是「压根没有这个概念」。
    Unspecified,
    /// **无侧别概念** —— 单器官（词典里 `side = none`，如膀胱）。
    NotApplicable,
}

impl Laterality {
    /// **全部侧别**（穷举清单，便于测试逐条覆盖）。
    pub const ALL: [Laterality; 5] = [
        Laterality::Left,
        Laterality::Right,
        Laterality::Bilateral,
        Laterality::Unspecified,
        Laterality::NotApplicable,
    ];

    /// 侧别字符串（进日志与留痕）。
    pub fn as_str(self) -> &'static str {
        match self {
            Laterality::Left => "left",
            Laterality::Right => "right",
            Laterality::Bilateral => "bilateral",
            Laterality::Unspecified => "unspecified",
            Laterality::NotApplicable => "none",
        }
    }

    /// 中文说明。
    pub fn message(self) -> &'static str {
        match self {
            Laterality::Left => "左侧",
            Laterality::Right => "右侧",
            Laterality::Bilateral => "双侧",
            Laterality::Unspecified => "未标注侧别",
            Laterality::NotApplicable => "无侧别概念（单器官）",
        }
    }
}

/// **不可比的原因**（侧别规则表判出来的结论）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SideMismatch {
    /// **不同器官** —— 基名就不是一个（肾 vs 膀胱）。这不是侧别问题。
    DifferentBase,
    /// **不同侧** —— 左 vs 右。
    DifferentSide,
    /// **双侧 ↔ 单侧** —— 「双肾」的值与「左肾」的值不是同一个测量。
    BilateralVsUnilateral,
    /// **侧别未标注** —— 成对器官没写清是哪一侧（连同为未标注也不可比）。
    SideUnspecified,
}

impl SideMismatch {
    /// **全部不可比原因**（穷举清单）。
    pub const ALL: [SideMismatch; 4] = [
        SideMismatch::DifferentBase,
        SideMismatch::DifferentSide,
        SideMismatch::BilateralVsUnilateral,
        SideMismatch::SideUnspecified,
    ];

    /// 原因字符串（进日志与留痕；与失败清单的错误码是**两套东西**，
    /// 这里是「对拍候选筛选」阶段的排除原因）。
    pub fn as_str(self) -> &'static str {
        match self {
            SideMismatch::DifferentBase => "DIFFERENT_BASE",
            SideMismatch::DifferentSide => "SIDE_MISMATCH",
            SideMismatch::BilateralVsUnilateral => "BILATERAL_VS_UNILATERAL",
            SideMismatch::SideUnspecified => "SIDE_UNSPECIFIED",
        }
    }

    /// 中文说明（运维看得懂「为什么这条没比上」）。
    pub fn message(self) -> &'static str {
        match self {
            SideMismatch::DifferentBase => "不是同一个器官，比不了",
            SideMismatch::DifferentSide => "左右侧不同，不是同一侧",
            SideMismatch::BilateralVsUnilateral => "双侧值与单侧值不是同一个测量，不相减",
            SideMismatch::SideUnspecified => "成对器官未标注侧别，无法确认是同一侧",
        }
    }
}

/// **可比性结论**。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SideComparability {
    /// 可比（同一个器官、侧别能对上）。
    Comparable,
    /// 不可比（带原因）。
    NotComparable(SideMismatch),
}

impl SideComparability {
    /// 是否可比。
    pub fn is_comparable(self) -> bool {
        matches!(self, SideComparability::Comparable)
    }

    /// 不可比时的原因（可比时为 `None`）。
    pub fn mismatch(self) -> Option<SideMismatch> {
        match self {
            SideComparability::Comparable => None,
            SideComparability::NotComparable(reason) => Some(reason),
        }
    }
}

/// 查一个规范部位键的侧别。
///
/// **优先查词典**（侧别是词典里的字段，口径跟着词典版本走）；
/// 词典查不到时按「基名 + 侧别后缀」的构成规则拆 —— 拆不出侧别即**未标注**。
pub fn laterality_of(part_key: &str) -> Laterality {
    let trimmed = part_key.trim();
    if trimmed.is_empty() {
        return Laterality::Unspecified;
    }
    if let Ok(dict) = parts_dictionary() {
        if let Some(entry) = dict.entries().iter().find(|e| e.part_key == trimmed) {
            return laterality_from_side(entry.side.as_str());
        }
    }
    let (_, side) = split_part_key(trimmed);
    laterality_from_side(side)
}

/// 侧别字符串 → 侧别枚举（`none` = 无侧别概念；认不出的按**未标注**处理）。
fn laterality_from_side(side: &str) -> Laterality {
    match side.trim() {
        "left" => Laterality::Left,
        "right" => Laterality::Right,
        "bilateral" => Laterality::Bilateral,
        "none" => Laterality::NotApplicable,
        _ => Laterality::Unspecified,
    }
}

/// 规范部位键 → (基名, 侧别后缀)。
///
/// 按**最后一个点**拆 —— 基名本身不含点（`renal_pelvis` 是一个基名，不是两段）。
fn split_part_key(part_key: &str) -> (&str, &str) {
    match part_key.rfind('.') {
        Some(idx) => (&part_key[..idx], &part_key[idx + 1..]),
        None => (part_key, ""),
    }
}

/// 规范部位键 → 解剖基名。
///
/// 词典查得到就用词典的基名（口径与词典一致）；查不到按构成规则拆。
///
/// **返回自有字符串**：词典是进程内共享的 `Arc`，借它的字段会跨不过临时值 ——
/// 基名只是比较用的键，复制一次的成本可以忽略。
pub fn base_of(part_key: &str) -> String {
    let trimmed = part_key.trim();
    if let Ok(dict) = parts_dictionary() {
        if let Some(entry) = dict.entries().iter().find(|e| e.part_key == trimmed) {
            return entry.base.clone();
        }
    }
    split_part_key(trimmed).0.to_string()
}

/// **同侧判定规则表**：两个部位能不能放在一起比。
///
/// `current_part_key` 是**当前报告**的规范部位键，
/// `history_part_key` 是**历史**那侧的规范部位键。
///
/// 规则按顺序判（先器官、再侧别）：
///
/// 1. **器官不同** → 不可比（`DifferentBase`）；
/// 2. **两侧都是单器官** → 可比（没有侧别这回事）；
/// 3. **任一侧未标注侧别** → 不可比（`SideUnspecified`）；
/// 4. **侧别相同** → 可比；
/// 5. **一侧双侧、一侧单侧** → 不可比（`BilateralVsUnilateral`）；
/// 6. **左右不同** → 不可比（`DifferentSide`）。
///
/// 第 4 与第 5 的次序有讲究：**双侧 ↔ 单侧必须先于左右不同判**，
/// 否则「双肾 vs 左肾」会被误报成「左右不同」—— 运维看到的处置动作就不对了。
pub fn compare_sides(current_part_key: &str, history_part_key: &str) -> SideComparability {
    let current = current_part_key.trim();
    let history = history_part_key.trim();

    // 规则一：不是同一个器官 —— 连器官都不对，谈侧别没意义。
    if base_of(current) != base_of(history) {
        return SideComparability::NotComparable(SideMismatch::DifferentBase);
    }

    let current_side = laterality_of(current);
    let history_side = laterality_of(history);

    // 规则二：单器官（无侧别概念）—— 同一器官自然可比。
    if current_side == Laterality::NotApplicable && history_side == Laterality::NotApplicable {
        return SideComparability::Comparable;
    }

    // 规则三：任一侧未标注侧别 —— 无法确认是同一侧。
    if current_side == Laterality::Unspecified || history_side == Laterality::Unspecified {
        return SideComparability::NotComparable(SideMismatch::SideUnspecified);
    }

    // 规则四：侧别相同（含双侧对双侧）。
    if current_side == history_side {
        return SideComparability::Comparable;
    }

    // 规则五：双侧 ↔ 单侧 —— 不是同一个测量。
    if current_side == Laterality::Bilateral || history_side == Laterality::Bilateral {
        return SideComparability::NotComparable(SideMismatch::BilateralVsUnilateral);
    }

    // 规则六：左 vs 右。
    SideComparability::NotComparable(SideMismatch::DifferentSide)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 期望矩阵（**独立于实现写死**）——
    /// `(当前部位键, 历史部位键, 期望可比性)`。
    ///
    /// 期望表独立写死是这个模块的关键：规则表改了而期望表没改，用例立刻失败 ——
    /// 这正是要的「口径漂了看得见」。
    const EXPECTED: &[(&str, &str, SideComparability)] = &[
        // 同侧可比。
        ("kidney.left", "kidney.left", SideComparability::Comparable),
        ("kidney.right", "kidney.right", SideComparability::Comparable),
        ("kidney.bilateral", "kidney.bilateral", SideComparability::Comparable),
        ("renal_pelvis.left", "renal_pelvis.left", SideComparability::Comparable),
        ("renal_calyx.right", "renal_calyx.right", SideComparability::Comparable),
        ("ureter.bilateral", "ureter.bilateral", SideComparability::Comparable),
        // 单器官（无侧别概念）可比。
        ("bladder", "bladder", SideComparability::Comparable),
        // 不同侧。
        ("kidney.left", "kidney.right", SideComparability::NotComparable(SideMismatch::DifferentSide)),
        ("kidney.right", "kidney.left", SideComparability::NotComparable(SideMismatch::DifferentSide)),
        ("renal_pelvis.left", "renal_pelvis.right", SideComparability::NotComparable(SideMismatch::DifferentSide)),
        ("ureter.left", "ureter.right", SideComparability::NotComparable(SideMismatch::DifferentSide)),
        // 双侧 ↔ 单侧。
        ("kidney.bilateral", "kidney.left", SideComparability::NotComparable(SideMismatch::BilateralVsUnilateral)),
        ("kidney.left", "kidney.bilateral", SideComparability::NotComparable(SideMismatch::BilateralVsUnilateral)),
        ("kidney.bilateral", "kidney.right", SideComparability::NotComparable(SideMismatch::BilateralVsUnilateral)),
        ("ureter.bilateral", "ureter.left", SideComparability::NotComparable(SideMismatch::BilateralVsUnilateral)),
        // 不同器官。
        ("kidney.left", "bladder", SideComparability::NotComparable(SideMismatch::DifferentBase)),
        ("bladder", "kidney.left", SideComparability::NotComparable(SideMismatch::DifferentBase)),
        ("kidney.left", "ureter.left", SideComparability::NotComparable(SideMismatch::DifferentBase)),
        // 未标注侧别（成对器官没写左 / 右 / 双侧）。
        ("kidney", "kidney", SideComparability::NotComparable(SideMismatch::SideUnspecified)),
        ("kidney", "kidney.left", SideComparability::NotComparable(SideMismatch::SideUnspecified)),
        ("kidney.left", "kidney", SideComparability::NotComparable(SideMismatch::SideUnspecified)),
        ("kidney", "kidney.bilateral", SideComparability::NotComparable(SideMismatch::SideUnspecified)),
    ];

    #[test]
    fn 规则表逐条与期望表一致() {
        for (current, history, expected) in EXPECTED {
            assert_eq!(
                compare_sides(current, history),
                *expected,
                "{} 与 {} 的可比性应与期望表一致",
                current,
                history
            );
        }
    }

    #[test]
    fn 词典里的每个部位键两两组合都不panic且结论自洽() {
        // 穷举词典中每个部位键与自身的组合：同键必然可比。
        let dict = parts_dictionary().expect("部位词典必须可用");
        for entry in dict.entries() {
            assert_eq!(
                compare_sides(&entry.part_key, &entry.part_key),
                SideComparability::Comparable,
                "同一部位键应可比：{}",
                entry.part_key
            );
        }
    }

    #[test]
    fn 侧别取自部位词典而不是代码里另写一份() {
        let dict = parts_dictionary().expect("部位词典必须可用");
        for entry in dict.entries() {
            let expected = match entry.side.as_str() {
                "left" => Laterality::Left,
                "right" => Laterality::Right,
                "bilateral" => Laterality::Bilateral,
                "none" => Laterality::NotApplicable,
                other => panic!("词典里出现未登记的侧别：{}", other),
            };
            assert_eq!(
                laterality_of(&entry.part_key),
                expected,
                "{} 的侧别应取自词典",
                entry.part_key
            );
        }
    }

    #[test]
    fn 词典外的写法按未标注侧别处理() {
        // 查不到就是查不到 —— 不猜侧别。
        assert_eq!(laterality_of("胆囊"), Laterality::Unspecified);
        assert_eq!(laterality_of(""), Laterality::Unspecified);
        assert_eq!(laterality_of("kidney"), Laterality::Unspecified);
        // 词典外的写法与词典内的写法相比 → 未标注侧别（不可比）。
        assert_eq!(
            compare_sides("kidney.left", "胆囊"),
            SideComparability::NotComparable(SideMismatch::DifferentBase)
        );
    }

    #[test]
    fn 基名按最后一个点拆_带下划线的基名不被拆错() {
        assert_eq!(base_of("renal_pelvis.left"), "renal_pelvis");
        assert_eq!(base_of("renal_calyx.right"), "renal_calyx");
        assert_eq!(base_of("bladder"), "bladder");
        assert_eq!(laterality_of("renal_pelvis.left"), Laterality::Left);
        assert_eq!(laterality_of("renal_calyx.bilateral"), Laterality::Bilateral);
    }

    #[test]
    fn 不可比原因各有中文说明与稳定字符串() {
        for reason in SideMismatch::ALL {
            assert!(!reason.message().is_empty());
            assert!(!reason.as_str().is_empty());
        }
        // 稳定字符串互不相同（按码聚合的前提）。
        let mut strings: Vec<&str> = SideMismatch::ALL.iter().map(|r| r.as_str()).collect();
        strings.sort_unstable();
        let before = strings.len();
        strings.dedup();
        assert_eq!(strings.len(), before, "不可比原因的字符串重复了");
    }

    #[test]
    fn 侧别枚举穷举且各有说明() {
        for side in Laterality::ALL {
            assert!(!side.message().is_empty());
            assert!(!side.as_str().is_empty());
        }
    }

    #[test]
    fn 双侧对单侧先于左右不同判() {
        // 「双肾 vs 左肾」不能被误报成「左右不同」—— 处置动作不一样。
        assert_eq!(
            compare_sides("kidney.bilateral", "kidney.left").mismatch(),
            Some(SideMismatch::BilateralVsUnilateral)
        );
        assert_eq!(
            compare_sides("kidney.left", "kidney.right").mismatch(),
            Some(SideMismatch::DifferentSide)
        );
    }

    #[test]
    fn 可比性结论自带自洽判据() {
        assert!(SideComparability::Comparable.is_comparable());
        assert!(SideComparability::Comparable.mismatch().is_none());
        let not_comparable = SideComparability::NotComparable(SideMismatch::DifferentSide);
        assert!(!not_comparable.is_comparable());
        assert_eq!(not_comparable.mismatch(), Some(SideMismatch::DifferentSide));
    }
}
