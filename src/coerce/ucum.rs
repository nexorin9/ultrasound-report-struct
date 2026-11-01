//! 强转内核第 3 层：**UCUM 码 ↔ 中文单位名** 的双向映射（名字层）。
//!
//! 机制溯源（**改写**，不复制源代码）：源产品把「类型名 ↔ 类型码」做成**显式往返** ——
//! `pg_checksum_parse_type` 大小写不敏感地解析名字、**未知名字返回 `None`**，
//! `pg_checksum_type_name` 反向给出规范名。本项目把同一套契约搬到**单位码**上：
//! `cm` / `CM` / `厘米` 归一到同一 UCUM 码，**未知单位一律拒绝**。
//!
//! # 三条纪律
//!
//! 1. **双向可往返** —— 中文名 → 码 → 中文名 与 码 → 中文名 → 码 都回到原处。
//! 2. **大小写不敏感** —— `cm` / `CM` / `Cm` 同义；`mL` / `ml` / `ML` 同义。
//! 3. **未知即拒绝** —— 未知码或未知中文名返回 `None`，**不回落默认单位**。
//!
//! # 分层：词法层切词面，名字层认单位
//!
//! | 层 | 回答的问题 | 落点 |
//! |---|---|---|
//! | 词法层 | 「这个词面到哪结束」（`cm³` 切到哪） | [`crate::parse::lexer`] |
//! | **名字层（本模块）** | 「这个词面是什么单位」（`㎝` / `CM` / `厘米` 都是 `cm`） | 本模块 |
//! | 量纲层 | 「这个单位怎么换算」（族内换算、跨族不换） | [`crate::coerce::units`] |
//!
//! 两层职责不重叠：词法层只管切出词面，**认定「这是什么单位」在本模块**。
//!
//! # 公开码表
//!
//! 英文 UCUM 名取自**公开码表**（`assets/public/ucum/unit_crosswalk.csv` 的
//! `ucum_name` 列）：`mm`→`millimeter`、`cm`→`centimeter`、`m`→`meter`、
//! `mL`→`milliliter`、`L`→`liter`。
//!
//! `cm3` 是本项目的**简化写法**，UCUM 两份发布包**均未收录** —— 故
//! [`UcumEntry::ucum_name_en`] 为 `None`，**不编一个英文名**充数。
//!
//! # 口径声明
//!
//! 换算因子是**经验约定的院内口径**，不是法定计量标准；本模块只管**名字**，
//! 因子与族属归 [`crate::coerce::units`]。

/// 一条 UCUM 码 ↔ 中文单位名 的目录项。
#[derive(Clone, Copy, Debug)]
pub struct UcumEntry {
    /// 规范 UCUM 码（按 UCUM 原样：`mL` 不是 `ml`、`L` 不是 `l`）。
    pub code: &'static str,
    /// 规范中文单位名（往返的**锚点**：码 ↔ 这个名字）。
    pub zh: &'static str,
    /// 其他中文写法（归一到同一码）。
    ///
    /// 别名**只认明确同义的写法** —— `cc` 不在列：强转路径对照表把 `cc` 当作
    /// **未知单位**（不默认按 `cm` 处理），把它收进别名就等于悄悄改了口径。
    pub zh_aliases: &'static [&'static str],
    /// UCUM 英文名（公开码表 `ucum_name` 列）；`None` = **UCUM 未收录**。
    pub ucum_name_en: Option<&'static str>,
}

/// UCUM 码 ↔ 中文单位名 目录（本轮口径内的全部单位）。
///
/// 与 [`crate::coerce::units`] 的**单位目录**（族属 + 换算因子）一一对应 ——
/// 两个模块的码集合必须相等，由 `tests/coerce_ucum.rs` 的跨模块断言守住。
pub const ENTRIES: &[UcumEntry] = &[
    UcumEntry {
        code: "mm",
        zh: "毫米",
        zh_aliases: &[],
        ucum_name_en: Some("millimeter"),
    },
    UcumEntry {
        code: "cm",
        zh: "厘米",
        zh_aliases: &["公分"],
        ucum_name_en: Some("centimeter"),
    },
    UcumEntry {
        code: "m",
        zh: "米",
        zh_aliases: &[],
        ucum_name_en: Some("meter"),
    },
    UcumEntry {
        code: "mL",
        zh: "毫升",
        zh_aliases: &[],
        ucum_name_en: Some("milliliter"),
    },
    UcumEntry {
        code: "L",
        zh: "升",
        zh_aliases: &[],
        ucum_name_en: Some("liter"),
    },
    UcumEntry {
        code: "cm3",
        zh: "立方厘米",
        zh_aliases: &[],
        // UCUM 两份发布包均未收录 `cm³` —— 本项目简化写法，不编英文名。
        ucum_name_en: None,
    },
];

/// 全部目录项。
pub fn entries() -> &'static [UcumEntry] {
    ENTRIES
}

/// 按**规范码**查目录项（大小写敏感 —— UCUM 码按原样）。
fn entry_by_code(code: &str) -> Option<&'static UcumEntry> {
    ENTRIES.iter().find(|e| e.code == code)
}

// ---------------------------------------------------------------------------
// 书写形式归一（名字层的第一步）
// ---------------------------------------------------------------------------

/// 上标数字 → ASCII 数字（`cm³` → `cm3`）。
pub fn superscript_digit(c: char) -> Option<char> {
    match c {
        '\u{b3}' => Some('3'),
        '\u{b2}' => Some('2'),
        '\u{b9}' => Some('1'),
        _ => None,
    }
}

/// 兼容方块单位 → ASCII 词面（`㎝` → `cm`、`㎤` → `cm3`）。
///
/// 这些字符在中文报告里真实存在（输入法直接打方块单位），不是边角料。
pub fn compat_unit(c: char) -> Option<&'static str> {
    match c {
        '\u{339c}' => Some("mm"),
        '\u{339d}' => Some("cm"),
        '\u{339e}' => Some("km"),
        '\u{339f}' => Some("mm2"),
        '\u{33a0}' => Some("cm2"),
        '\u{33a1}' => Some("m2"),
        '\u{33a2}' => Some("km2"),
        '\u{33a3}' => Some("mm3"),
        '\u{33a4}' => Some("cm3"),
        '\u{33a5}' => Some("m3"),
        '\u{33a6}' => Some("km3"),
        _ => None,
    }
}

/// 是否全角拉丁字母（`Ｃ` / `ｍ`）。
pub fn is_fullwidth_latin(c: char) -> bool {
    matches!(c as u32, 0xFF21..=0xFF3A | 0xFF41..=0xFF5A)
}

/// 全角拉丁字母 → 半角（`Ｃ` → `C`）。
pub fn to_halfwidth_latin(c: char) -> char {
    match c as u32 {
        0xFF21..=0xFF3A => char::from_u32(c as u32 - 0xFF21 + 0x41).unwrap_or(c),
        0xFF41..=0xFF5A => char::from_u32(c as u32 - 0xFF41 + 0x61).unwrap_or(c),
        _ => c,
    }
}

/// 单位词面归一：全角拉丁 → 半角、上标数字 → ASCII、兼容方块单位 → ASCII、幂写法（`^3`）→ 数字。
///
/// 只改**书写形式**，不改量纲：`cm³` 与 `cm3` 归到同一词面，`mm2` 归一后仍是
/// `mm2` —— 它不在目录里，于是照旧是**未知单位**（不默认按毫米处理）。
pub fn normalize_unit_spelling(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(d) = superscript_digit(c) {
            out.push(d);
        } else if let Some(rep) = compat_unit(c) {
            out.push_str(rep);
        } else if is_fullwidth_latin(c) {
            out.push(to_halfwidth_latin(c));
        } else if c == '^' {
            // 幂写法只吃紧跟的一个数字：`cm^3` → `cm3`；`cm^` 原样保留。
            if let Some(&d) = chars.peek() {
                if d.is_ascii_digit() {
                    out.push(d);
                    chars.next();
                } else {
                    out.push('^');
                }
            } else {
                out.push('^');
            }
        } else {
            out.push(c);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 双向映射
// ---------------------------------------------------------------------------

/// 中文单位名 / 别名 → 规范 UCUM 码（**精确匹配**）。
///
/// 未知中文名返回 `None` —— **不回落默认单位**。
pub fn code_of_zh(name: &str) -> Option<&'static str> {
    let t = name.trim();
    if t.is_empty() {
        return None;
    }
    for e in ENTRIES {
        if e.zh == t {
            return Some(e.code);
        }
        if e.zh_aliases.contains(&t) {
            return Some(e.code);
        }
    }
    None
}

/// 任意写法 → 规范 UCUM 码（**大小写不敏感** + 书写形式归一 + 中文名反查）。
///
/// | 输入 | 输出 |
/// |---|---|
/// | `cm` / `CM` / `Cm` / `㎝` | `cm` |
/// | `ml` / `mL` / `ML` / `毫升` | `mL` |
/// | `cm3` / `cm³` / `cm^3` / `㎤` / `立方厘米` | `cm3` |
/// | `xx` / `cc` / `mm2` / `""` | `None`（未知即拒绝） |
///
/// **fail-closed**：查不到就是查不到 —— 未知单位**不按任何单位兜底**。
pub fn canonical_code(raw: &str) -> Option<&'static str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // 书写形式先归一（`㎝` → `cm`、`cm³` → `cm3`、`ＣＭ` → `CM`）。
    let normalized = normalize_unit_spelling(trimmed);
    // 中文名 / 别名 → 码（精确匹配，不做前缀猜测：「毫升」不是「升」）。
    if let Some(code) = code_of_zh(&normalized) {
        return Some(code);
    }
    // 拉丁码大小写不敏感：`CM` / `cm` / `Cm` 同义；`ml` / `mL` / `ML` 同义。
    let lower = normalized.to_ascii_lowercase();
    ENTRIES
        .iter()
        .find(|e| e.code.to_ascii_lowercase() == lower)
        .map(|e| e.code)
}

/// 任意写法 → 规范中文单位名。
///
/// 码与中文名互为入口：`zh_name("CM")` 与 `zh_name("厘米")` 都给 `厘米`。
/// 别名映射到**规范名**（`公分` → `厘米`）—— 往返的锚点是规范名，不是别名。
pub fn zh_name(raw: &str) -> Option<&'static str> {
    let code = canonical_code(raw)?;
    entry_by_code(code).map(|e| e.zh)
}

/// 任意写法 → UCUM 英文名（公开码表 `ucum_name` 列）。
///
/// `cm3` 返回 `None` —— UCUM 未收录，**不编一个英文名**。
pub fn ucum_name_en(raw: &str) -> Option<&'static str> {
    let code = canonical_code(raw)?;
    entry_by_code(code).and_then(|e| e.ucum_name_en)
}

/// 该写法是否是本轮口径内的**已知单位**（码或中文名）。
pub fn is_known(raw: &str) -> bool {
    canonical_code(raw).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 双向往返 ----

    #[test]
    fn 中文名到码再回中文名往返一致() {
        for e in ENTRIES {
            let code = code_of_zh(e.zh).expect("目录里的中文名应查得到码");
            assert_eq!(code, e.code, "{} 的中文名应回到自身", e.zh);
            let name = zh_name(code).expect("目录里的码应查得到中文名");
            assert_eq!(name, e.zh, "{} 的码应回到自身中文名", e.code);
            // 两跳：中文名 → 码 → 中文名。
            assert_eq!(zh_name(code_of_zh(e.zh).unwrap()), Some(e.zh));
            // 两跳：码 → 中文名 → 码。
            assert_eq!(code_of_zh(zh_name(e.code).unwrap()), Some(e.code));
        }
    }

    #[test]
    fn 别名映射到规范名并回到同一码() {
        // `公分` 是 `cm` 的别名：别名 → 码 → 规范名，往返后落在规范名上。
        assert_eq!(code_of_zh("公分"), Some("cm"));
        assert_eq!(zh_name("公分"), Some("厘米"));
        assert_eq!(code_of_zh(zh_name("公分").unwrap()), Some("cm"));
    }

    #[test]
    fn 全部目录项都能双向往返() {
        assert_eq!(ENTRIES.len(), 6);
        for e in ENTRIES {
            assert!(!e.zh.is_empty(), "{} 缺中文名", e.code);
            assert_eq!(canonical_code(e.code), Some(e.code));
            assert_eq!(code_of_zh(e.zh), Some(e.code));
            assert_eq!(zh_name(e.code), Some(e.zh));
        }
    }

    // ---- 大小写不敏感 ----

    #[test]
    fn 拉丁码大小写归一到同一码() {
        for raw in ["cm", "CM", "Cm", "cM"] {
            assert_eq!(canonical_code(raw), Some("cm"), "{} 应归一到 cm", raw);
        }
        for raw in ["mm", "MM", "Mm"] {
            assert_eq!(canonical_code(raw), Some("mm"), "{} 应归一到 mm", raw);
        }
        for raw in ["ml", "mL", "ML", "Ml"] {
            assert_eq!(canonical_code(raw), Some("mL"), "{} 应归一到 mL", raw);
        }
        for raw in ["L", "l"] {
            assert_eq!(canonical_code(raw), Some("L"), "{} 应归一到 L", raw);
        }
        for raw in ["m", "M"] {
            assert_eq!(canonical_code(raw), Some("m"), "{} 应归一到 m", raw);
        }
    }

    #[test]
    fn 码与中文名大小写归一到同一码() {
        for raw in ["厘米", "CM", "cm"] {
            assert_eq!(canonical_code(raw), Some("cm"));
        }
        for raw in ["毫升", "ML", "ml"] {
            assert_eq!(canonical_code(raw), Some("mL"));
        }
        for raw in ["毫米", "MM"] {
            assert_eq!(canonical_code(raw), Some("mm"));
        }
    }

    // ---- 书写形式归一 ----

    #[test]
    fn 书写形式归一到同一码() {
        for raw in ["cm", "㎝", "cm³", "cm^3", "㎤", "立方厘米"] {
            let code = canonical_code(raw).expect("应查得到码");
            assert_eq!(
                code,
                if raw == "cm³" || raw == "cm^3" || raw == "㎤" || raw == "立方厘米" {
                    "cm3"
                } else {
                    "cm"
                },
                "{} 归一结果不符",
                raw
            );
        }
        assert_eq!(canonical_code("ＣＭ"), Some("cm"));
    }

    // ---- 未知即拒绝 ----

    #[test]
    fn 未知码与未知中文名一律拒绝() {
        for raw in ["xx", "cc", "g", "in", "mm2", "cm2", "", "  "] {
            assert_eq!(canonical_code(raw), None, "{} 不该查得到码", raw);
            assert_eq!(zh_name(raw), None, "{} 不该查得到中文名", raw);
            assert_eq!(ucum_name_en(raw), None, "{} 不该查得到英文名", raw);
            assert!(!is_known(raw), "{} 不该是已知单位", raw);
        }
        // 未知中文名同样拒绝。
        for raw in ["寸", "英尺", "公毫", "公克"] {
            assert_eq!(code_of_zh(raw), None, "{} 不该查得到码", raw);
            assert_eq!(canonical_code(raw), None, "{} 不该查得到码", raw);
        }
    }

    #[test]
    fn cc不按厘米处理() {
        // 强转路径对照表把 `cc` 当未知单位：不收进别名，口径不偷改。
        assert_eq!(code_of_zh("cc"), None);
        assert_eq!(canonical_code("cc"), None);
    }

    #[test]
    fn 空白与大小写不敏感的中文名处理() {
        assert_eq!(code_of_zh("  厘米  "), Some("cm"));
        assert_eq!(canonical_code("  CM  "), Some("cm"));
    }

    // ---- 英文名 ----

    #[test]
    fn 英文名与公开码表一致() {
        assert_eq!(ucum_name_en("mm"), Some("millimeter"));
        assert_eq!(ucum_name_en("cm"), Some("centimeter"));
        assert_eq!(ucum_name_en("m"), Some("meter"));
        assert_eq!(ucum_name_en("mL"), Some("milliliter"));
        assert_eq!(ucum_name_en("L"), Some("liter"));
        // `cm3` 是本项目简化写法，UCUM 未收录 —— 不编英文名。
        assert_eq!(ucum_name_en("cm3"), None);
        assert_eq!(ucum_name_en("立方厘米"), None);
    }

    #[test]
    fn 英文名也可反查到码() {
        // 英文名不是本项目的输入入口（报告里不会写 millimeter），
        // 但它必须指向唯一的一个码 —— 不能有两个码共用一个英文名。
        let mut seen: Vec<(&str, &str)> = ENTRIES
            .iter()
            .filter_map(|e| e.ucum_name_en.map(|n| (n, e.code)))
            .collect();
        seen.sort_unstable();
        let before = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), before, "英文名与码应一一对应");
    }
}
