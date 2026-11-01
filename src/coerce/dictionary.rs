//! 院内词典 · 部位与测项（版本化）。
//!
//! **这一层解决什么**：对拍要能自动对上「同一患者同侧部位」，前提是部位写法先归一 ——
//! 「左肾」「左侧肾脏」「左肾脏」三种写法必须落到同一个规范部位键 `kidney.left`。
//! 归一不靠猜，靠**词典**：部位词面是词典的键，规范解剖基名是词典的值。
//!
//! **本轮口径**：部位词典只收**泌尿系**部位 —— 肾 / 肾盂 / 肾盏 / 输尿管 / 膀胱。
//! 词典外的部位词（胆囊、脾、前列腺等腹部器官）一律**未命中**：产出的台账行
//! **不参与对拍**（宁可不比不错比），失败条目指回原文让运维补词典。
//!
//! **侧别不在词面里**：侧别（left / right / bilateral）由报告原文的修饰词决定
//! （「左」「右侧」「双侧」「双」），与部位词面**分开**归一 —— 词典只回答「这是哪个器官」。
//!
//! **为什么肾盂 / 肾盏独立成基名**：肾盂前后径与肾前后径是**不同的测量**。
//! 合成到同一个基名，两者的历史值会被互相比较 —— 那是看起来正常、实际比错的差值。
//! 临床上的「积水」说的是肾盂，报告里的「双肾大小」说的是肾，两者不可混。
//!
//! **词典真源**：`assets/dictionary/parts.json`（版本化 JSON，**随仓库发布**）——
//! 部位词面表与规范部位键表都在这一份文件里，加载时互相校验：
//! 基名必须在词面表里、规范键必须等于「基名 + 侧别后缀」、别名与词面都不得重复。
//! 任何一条对不上即判词典不合法（**不猜、不回落**）。
//!
//! **为什么编译期还内置一份**：值班机可能只拷了可执行文件；词典文件**缺失**时
//! 退回编译期带入的**同一份**文件内容（不会与文件版本漂移）。
//! 但文件**存在却坏了** → **硬失败**：运维改坏的词典不被悄悄忽略，
//! 更不会退回旧词典继续跑（那会让口径变了却看不出来）。
//!
//! **版本留痕**：词典版本随台账行落库（`dictionary_version`）——
//! 口径变了历史行仍留旧版本，这是「口径可追溯」的落点。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;

/// 词典生命周期接口（加载 → 使用 → 释放 / 热更新）。
///
/// 实现在 [`super::dictionary_lifecycle`]：单列一个模块装三段式生命周期、引用计数
/// 与作用域，与词典内容本身分开；这里转出是**同一个公开入口** ——
/// 「词典怎么用」与「词典里有什么」都在 `coerce::dictionary` 下找得到。
pub use super::dictionary_lifecycle::{
    current_snapshot, reload_current_if_changed, scope_current, scoped_snapshot, Dictionaries,
    DictionaryHandle, DictionarySnapshot, ReloadOutcome, ScopeGuard,
};

/// 编译期内置的部位词典（与 `assets/dictionary/parts.json` **同一份文件**，编译期带入）。
const EMBEDDED_PARTS_JSON: &str = include_str!("../../assets/dictionary/parts.json");

/// 词典类别：部位词典（错误文案里的说法，运维照着找文件）。
pub(crate) const SUBJECT_PARTS: &str = "部位";
/// 词典类别：测项词典。
pub(crate) const SUBJECT_ITEMS: &str = "测项";

/// 词典加载错误（稳定错误码 + 中文说明）。
///
/// **带 `subject`**：部位词典与测项词典共用一套错误码，但文案要说清是**哪本词典**
/// —— 运维看到「测项词典结构不合法」才知道该去改 `items.json`，
/// 而不是把 `parts.json` 翻三遍。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DictionaryError {
    /// 词典文件不存在。
    FileNotFound {
        /// 词典类别（`部位` / `测项`）。
        subject: &'static str,
        /// 文件路径。
        path: PathBuf,
    },
    /// 词典文件读不到。
    Unreadable {
        /// 词典类别（`部位` / `测项`）。
        subject: &'static str,
        /// 文件路径。
        path: PathBuf,
        /// 底层错误说明。
        message: String,
    },
    /// 词典文件不是合法 JSON。
    ParseFailed {
        /// 词典类别（`部位` / `测项`）。
        subject: &'static str,
        /// 错误说明（含文件路径）。
        message: String,
    },
    /// 词典文件结构不合法（缺字段 / 别名重复 / 规范键与基名对不上 …）。
    SchemaInvalid {
        /// 词典类别（`部位` / `测项`）。
        subject: &'static str,
        /// 错误说明。
        message: String,
    },
    /// 词典**当前不可用**（装载失败或根本还没装上）。
    ///
    /// 与「文件不存在」分开：文件不存在可以退回编译期内置的那一份，
    /// 「不可用」说的是**连内置副本都取不到**（或装载硬失败）——
    /// 这种情况一律**不猜口径**，由调用方转显式失败。
    Unavailable {
        /// 词典类别（`部位` / `测项`）。
        subject: &'static str,
        /// 错误说明。
        message: String,
    },
}

impl DictionaryError {
    /// 稳定错误码（进日志与运维排查）。
    pub fn code(&self) -> &'static str {
        match self {
            DictionaryError::FileNotFound { .. } => "DICT_FILE_NOT_FOUND",
            DictionaryError::Unreadable { .. } => "DICT_UNREADABLE",
            DictionaryError::ParseFailed { .. } => "DICT_PARSE_FAILED",
            DictionaryError::SchemaInvalid { .. } => "DICT_SCHEMA_INVALID",
            DictionaryError::Unavailable { .. } => "DICT_UNAVAILABLE",
        }
    }

    /// 词典类别（`部位` / `测项`）—— 运维照着去改对应的那本词典。
    pub fn subject(&self) -> &'static str {
        match self {
            DictionaryError::FileNotFound { subject, .. }
            | DictionaryError::Unreadable { subject, .. }
            | DictionaryError::ParseFailed { subject, .. }
            | DictionaryError::SchemaInvalid { subject, .. }
            | DictionaryError::Unavailable { subject, .. } => subject,
        }
    }

    /// 中文说明。
    pub fn message(&self) -> String {
        match self {
            DictionaryError::FileNotFound { subject, path } => {
                format!("{}词典文件不存在：{}", subject, path.display())
            }
            DictionaryError::Unreadable {
                subject,
                path,
                message,
            } => {
                format!("{}词典文件读不到（{}）：{}", subject, path.display(), message)
            }
            DictionaryError::ParseFailed { subject, message } => {
                format!("{}词典解析失败：{}", subject, message)
            }
            DictionaryError::SchemaInvalid { subject, message } => {
                format!("{}词典结构不合法：{}", subject, message)
            }
            DictionaryError::Unavailable { subject, message } => {
                format!("{}词典当前不可用：{}", subject, message)
            }
        }
    }
}

impl std::fmt::Display for DictionaryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "错误码={} 说明={}", self.code(), self.message())
    }
}

/// 部位词典文件（JSON）的落盘结构。
#[derive(Debug, Deserialize)]
struct PartsFile {
    /// 词典版本号。
    ///
    /// **带 `default`**：缺 `version` 字段的词典是**结构不合法**（`DICT_SCHEMA_INVALID`），
    /// 不是 JSON 解析失败（`DICT_PARSE_FAILED`）—— 运维看到的是「词典少了版本号」，
    /// 而不是一句没头没尾的 serde 报错。空串同样走到结构校验里判非法。
    #[serde(default)]
    version: String,
    /// 部位词面表（部位词面 → 解剖基名）。
    #[serde(default)]
    bases: Vec<BaseFile>,
    /// 规范部位键表（基名 + 侧别 → 完整别名）。
    #[serde(default)]
    entries: Vec<EntryFile>,
}

/// 部位词面表的一行。
#[derive(Debug, Deserialize)]
struct BaseFile {
    /// 解剖基名。
    base: String,
    /// 部位词面（不含侧别修饰词）。
    #[serde(default)]
    word_faces: Vec<String>,
}

/// 规范部位键表的一行。
#[derive(Debug, Deserialize)]
struct EntryFile {
    /// 规范部位键。
    part_key: String,
    /// 解剖基名。
    base: String,
    /// 侧别（`left` / `right` / `bilateral` / `none`）。
    side: String,
    /// 该条目引入时的版本号（缺省取文件版本）。
    #[serde(default)]
    version: Option<String>,
    /// 完整别名（含侧别修饰词）。
    #[serde(default)]
    aliases: Vec<String>,
}

/// 部位基名条目：部位词面（不含侧别）→ 解剖基名。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaseEntry {
    /// 解剖基名（`kidney` / `renal_pelvis` / `renal_calyx` / `ureter` / `bladder`）。
    pub base: String,
    /// 部位词面（词典键；与词法层 `Part` token 的词面一致，不含侧别修饰词）。
    pub word_faces: Vec<String>,
}

/// 部位词典条目：规范部位键 + 别名。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartEntry {
    /// 规范部位键（`kidney.left` / `bladder` 等）。
    pub part_key: String,
    /// 解剖基名。
    pub base: String,
    /// 侧别（`left` / `right` / `bilateral` / `none`）。
    pub side: String,
    /// 该条目引入时的版本号。
    pub version: String,
    /// 完整别名（含侧别修饰词，如「左侧肾脏」）。
    pub aliases: Vec<String>,
}

/// 部位别名词典（版本化，从 `assets/dictionary/parts.json` 加载）。
///
/// 词典版本随台账行一起落库（`dictionary_version`），口径变了历史行仍留旧版本 ——
/// 这是「口径可追溯」的落点。
#[derive(Debug)]
pub struct PartsDictionary {
    /// 词典版本号。
    version: String,
    /// 词典来源（文件路径 / 内置副本）—— 运维看得见当前用的是哪一份。
    source: String,
    /// 部位词面表。
    bases: Vec<BaseEntry>,
    /// 规范部位键表。
    entries: Vec<PartEntry>,
}

/// 侧别合法取值。
const SIDES: [&str; 4] = ["left", "right", "bilateral", "none"];

/// 规范部位键的构成规则：基名 + 侧别后缀（`none` 时不加后缀）。
///
/// **这条规则是词典自洽性的判据**：文件里的 `part_key` 必须等于按本规则算出的值，
/// 否则说明词典写错了 —— 加载即报错，不让错的键进台账。
fn canonical_key(base: &str, side: &str) -> String {
    match side {
        "none" => base.to_string(),
        other => format!("{}.{}", base, other),
    }
}

/// 结构不合法的错误。
fn schema_invalid(subject: &'static str, message: impl Into<String>) -> DictionaryError {
    DictionaryError::SchemaInvalid {
        subject,
        message: message.into(),
    }
}

impl PartsDictionary {
    /// 从 JSON 文本构造词典（**加载时做结构校验**，任一条对不上即报错）。
    pub fn from_json(text: &str) -> Result<Self, DictionaryError> {
        let file: PartsFile = serde_json::from_str(text).map_err(|e| DictionaryError::ParseFailed {
            subject: SUBJECT_PARTS,
            message: format!("JSON 解析失败：{}", e),
        })?;

        let version = file.version.trim().to_string();
        if version.is_empty() {
            return Err(schema_invalid(SUBJECT_PARTS, "词典缺少 version 字段"));
        }
        if file.bases.is_empty() {
            return Err(schema_invalid(SUBJECT_PARTS, "词典缺少 bases（部位词面表）"));
        }
        if file.entries.is_empty() {
            return Err(schema_invalid(SUBJECT_PARTS, "词典缺少 entries（规范部位键表）"));
        }

        // 1) 部位词面表：基名不重复、词面不重复、词面非空。
        let mut bases: Vec<BaseEntry> = Vec::new();
        let mut seen_word_faces: Vec<String> = Vec::new();
        for b in &file.bases {
            let base = b.base.trim().to_string();
            if base.is_empty() {
                return Err(schema_invalid(SUBJECT_PARTS, "基名不得为空"));
            }
            if bases.iter().any(|e| e.base == base) {
                return Err(schema_invalid(SUBJECT_PARTS, format!("基名重复：{}", base)));
            }
            if b.word_faces.is_empty() {
                return Err(schema_invalid(SUBJECT_PARTS, format!("基名 {} 没有部位词面", base)));
            }
            let mut word_faces: Vec<String> = Vec::new();
            for wf in &b.word_faces {
                let wf = wf.trim().to_string();
                if wf.is_empty() {
                    return Err(schema_invalid(SUBJECT_PARTS, format!("基名 {} 的部位词面不得为空", base)));
                }
                if word_faces.contains(&wf) {
                    return Err(schema_invalid(SUBJECT_PARTS, format!("基名 {} 的部位词面重复：{}", base, wf)));
                }
                if seen_word_faces.contains(&wf) {
                    return Err(schema_invalid(SUBJECT_PARTS, format!(
                        "部位词面 {} 出现在多个基名下",
                        wf
                    )));
                }
                seen_word_faces.push(wf.clone());
                word_faces.push(wf);
            }
            bases.push(BaseEntry { base, word_faces });
        }

        // 2) 规范部位键表：别名不重复、基名必须存在、侧别合法、规范键必须自洽。
        let mut entries: Vec<PartEntry> = Vec::new();
        let mut seen_aliases: Vec<String> = Vec::new();
        for e in &file.entries {
            let part_key = e.part_key.trim().to_string();
            let base = e.base.trim().to_string();
            let side = e.side.trim().to_string();
            if part_key.is_empty() {
                return Err(schema_invalid(SUBJECT_PARTS, "规范部位键不得为空"));
            }
            if !bases.iter().any(|b| b.base == base) {
                return Err(schema_invalid(SUBJECT_PARTS, format!(
                    "规范键 {} 的基名 {} 不在部位词面表里",
                    part_key, base
                )));
            }
            if !SIDES.contains(&side.as_str()) {
                return Err(schema_invalid(SUBJECT_PARTS, format!(
                    "规范键 {} 的侧别非法：{}（可用：{}）",
                    part_key,
                    side,
                    SIDES.join(" / ")
                )));
            }
            let expected = canonical_key(&base, &side);
            if part_key != expected {
                return Err(schema_invalid(SUBJECT_PARTS, format!(
                    "规范键 {} 与基名+侧别（{}）对不上",
                    part_key, expected
                )));
            }
            if e.aliases.is_empty() {
                return Err(schema_invalid(SUBJECT_PARTS, format!("规范键 {} 没有别名", part_key)));
            }
            let mut aliases: Vec<String> = Vec::new();
            for a in &e.aliases {
                let a = a.trim().to_string();
                if a.is_empty() {
                    return Err(schema_invalid(SUBJECT_PARTS, format!(
                        "规范键 {} 的别名不得为空",
                        part_key
                    )));
                }
                if seen_aliases.contains(&a) {
                    return Err(schema_invalid(SUBJECT_PARTS, format!("别名重复：{}", a)));
                }
                seen_aliases.push(a.clone());
                aliases.push(a);
            }
            entries.push(PartEntry {
                part_key,
                base,
                side,
                version: e
                    .version
                    .clone()
                    .unwrap_or_else(|| version.clone()),
                aliases,
            });
        }

        // 3) 每个基名至少有一条规范键条目 —— 否则该基名永远产不出部位键。
        for b in &bases {
            if !entries.iter().any(|e| e.base == b.base) {
                return Err(schema_invalid(SUBJECT_PARTS, format!(
                    "基名 {} 没有对应的规范部位键条目",
                    b.base
                )));
            }
        }

        Ok(PartsDictionary {
            version,
            source: "内置副本".to_string(),
            bases,
            entries,
        })
    }

    /// 从文件加载词典。
    pub fn load(path: &Path) -> Result<Self, DictionaryError> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(DictionaryError::FileNotFound {
                    subject: SUBJECT_PARTS,
                    path: path.to_path_buf(),
                })
            }
            Err(e) => {
                return Err(DictionaryError::Unreadable {
                    subject: SUBJECT_PARTS,
                    path: path.to_path_buf(),
                    message: e.to_string(),
                })
            }
        };
        let mut dict = Self::from_json(&text).map_err(|e| match e {
            DictionaryError::ParseFailed { subject, message } => DictionaryError::ParseFailed {
                subject,
                message: format!("{}：{}", path.display(), message),
            },
            other => other,
        })?;
        dict.source = format!("文件：{}", path.display());
        Ok(dict)
    }

    /// 词典版本号。
    pub fn version(&self) -> &str {
        &self.version
    }

    /// 词典来源（文件路径 / 内置副本）。
    pub fn source(&self) -> &str {
        &self.source
    }

    /// 标注词典来源（退回内置副本时用 —— 运维要看得见当前用的是哪一份）。
    pub fn set_source(&mut self, source: impl Into<String>) {
        self.source = source.into();
    }

    /// 部位词面 → 解剖基名；**查不到返回 `None`**（不猜、不回落默认部位）。
    pub fn base_of(&self, word_face: &str) -> Option<&str> {
        self.bases
            .iter()
            .find(|b| b.word_faces.iter().any(|w| w == word_face))
            .map(|b| b.base.as_str())
    }

    /// 完整别名（含侧别，如「左侧肾脏」）→ 规范部位键；**查不到返回 `None`**。
    pub fn part_key_of_alias(&self, alias: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|e| e.aliases.iter().any(|a| a == alias))
            .map(|e| e.part_key.as_str())
    }

    /// 完整别名 → 词典条目（运维补词典时要看的是整条：规范键 + 基名 + 侧别）。
    pub fn entry_of_alias(&self, alias: &str) -> Option<&PartEntry> {
        self.entries
            .iter()
            .find(|e| e.aliases.iter().any(|a| a == alias))
    }

    /// （解剖基名 × 侧别）→ 规范部位键；**查不到返回 `None`**。
    ///
    /// 规范键**由词典给出**，不是在代码里拼字符串 —— 词典没收录的组合就是没有，
    /// 不拼一个词典不认的键出来。
    pub fn part_key_of(&self, base: &str, side: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|e| e.base == base && e.side == side)
            .map(|e| e.part_key.as_str())
    }

    /// 词典收录的全部部位词面（用例与词典资产核对用）。
    pub fn word_faces(&self) -> Vec<&str> {
        self.bases
            .iter()
            .flat_map(|b| b.word_faces.iter().map(|w| w.as_str()))
            .collect()
    }

    /// 词典收录的全部完整别名（用例与词典资产核对用）。
    pub fn aliases(&self) -> Vec<&str> {
        self.entries
            .iter()
            .flat_map(|e| e.aliases.iter().map(|a| a.as_str()))
            .collect()
    }

    /// 规范部位键表（逐条断言用）。
    pub fn entries(&self) -> &[PartEntry] {
        &self.entries
    }

    /// 部位词面表（逐条断言用）。
    pub fn bases(&self) -> &[BaseEntry] {
        &self.bases
    }

    /// 词典条目数（规范部位键条数）。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 词典是否为空。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// 部位词典默认路径（配置文件 `dictionary.parts_file` 可覆盖）。
pub fn default_parts_path() -> PathBuf {
    PathBuf::from(crate::config::DEFAULT_PARTS_FILE)
}

/// 编译期内置的部位词典 JSON（与 `assets/dictionary/parts.json` **同一份文件**）。
///
/// 文件缺失时退回的就是这一份 —— 用例拿它与文件逐条比对，
/// 保证「退回内置」退回来的不是另一份口径。
pub fn embedded_parts_json() -> &'static str {
    EMBEDDED_PARTS_JSON
}

/// 部位词典（**当前生效的那一代**）。
///
/// 解析口径由 [`crate::coerce::dictionary_lifecycle`] 统一管：
/// 热更新换代时返回的是**新的一代**，在途任务靠作用域固定住自己那一代。
pub fn parts_dictionary() -> Result<Arc<PartsDictionary>, DictionaryError> {
    let snapshot = super::dictionary_lifecycle::current_snapshot().ok_or_else(|| {
        DictionaryError::Unavailable {
            subject: SUBJECT_PARTS,
            message: "词典尚未装载，或装载时硬失败".to_string(),
        }
    })?;
    Ok(Arc::clone(snapshot.parts_handle()))
}

/// 部位词面 → 解剖基名；**查不到返回 `None`**（不猜部位）。
///
/// 这是全项目唯一的「部位词面 → 基名」映射入口：合并层与抽取层都走这里，
/// 不各带一份映射（两处真源必然漂移）。
pub fn part_base_of(word_face: &str) -> Option<String> {
    let snapshot = super::dictionary_lifecycle::current_snapshot()?;
    snapshot.parts().base_of(word_face).map(|s| s.to_string())
}

/// 完整别名 → 规范部位键；**查不到返回 `None`**（由调用方转 `part_unresolved`）。
pub fn part_key_of_alias(alias: &str) -> Option<String> {
    let snapshot = super::dictionary_lifecycle::current_snapshot()?;
    snapshot
        .parts()
        .part_key_of_alias(alias)
        .map(|s| s.to_string())
}

/// （解剖基名 × 侧别）→ 规范部位键；**查不到返回 `None`**。
pub fn part_key_of_base_side(base: &str, side: &str) -> Option<String> {
    let snapshot = super::dictionary_lifecycle::current_snapshot()?;
    snapshot
        .parts()
        .part_key_of(base, side)
        .map(|s| s.to_string())
}

/// 部位词典版本号（随条目透传到台账行）。
///
/// 词典不可用时返回 `parts-unavailable` —— 这不是一个可用的版本号，
/// 守护进程在词典加载失败时已直接退出，不会拿它落台账。
pub fn parts_version() -> String {
    super::dictionary_lifecycle::current_snapshot()
        .map(|s| s.parts_version().to_string())
        .unwrap_or_else(|| "parts-unavailable".to_string())
}

/// 编译期内置的测项词典（与 `assets/dictionary/items.json` **同一份文件**，编译期带入）。
const EMBEDDED_ITEMS_JSON: &str = include_str!("../../assets/dictionary/items.json");

/// 测项词典条目：测项词面 → 测项码后缀 + 适用部位基名。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemEntry {
    /// 测项词面（词典键；与词法层 `Item` token 的词面一致）。
    pub alias: String,
    /// 测项码后缀（如 `ap_diameter`）—— 规范测项码 = 部位基名 + `.` + 后缀。
    pub suffix: String,
    /// 适用部位基名（该测项在这些部位上才有对拍口径）。
    ///
    /// **按部位限定**：残余尿量是膀胱的测量，不是肾的测量；
    /// 不限定就会把「肾残余尿量」也编成同一个测项码，
    /// 拿不存在的历史值去比 —— **看起来正常，实际比错**。
    pub bases: Vec<String>,
    /// 该条目引入时的版本号。
    pub version: String,
}

/// 测项词典文件（JSON）的落盘结构。
#[derive(Debug, Deserialize)]
struct ItemsFile {
    /// 词典版本号。
    ///
    /// **带 `default`**：缺 `version` 字段的词典是**结构不合法**
    /// （`DICT_SCHEMA_INVALID`），不是 JSON 解析失败 ——
    /// 运维看到的是「词典少了版本号」，而不是一句没头没尾的 serde 报错。
    #[serde(default)]
    version: String,
    /// 测项词面表（词面 → 后缀 + 适用部位）。
    #[serde(default)]
    entries: Vec<ItemEntryFile>,
}

/// 测项词面表的一行。
#[derive(Debug, Deserialize)]
struct ItemEntryFile {
    /// 测项词面（词典键）。
    #[serde(default)]
    alias: String,
    /// 测项码后缀。
    #[serde(default)]
    suffix: String,
    /// 适用部位基名。
    #[serde(default)]
    bases: Vec<String>,
    /// 该条目引入时的版本号（缺省取文件版本）。
    #[serde(default)]
    version: Option<String>,
}

/// 测项别名词典（版本化，从 `assets/dictionary/items.json` 加载）。
///
/// **多种写法归一**：同一测项的不同写法（「长」/「长度」、「宽」/「宽度」）
/// 映射到**同一个后缀** —— 否则同一测量会被拆成两个测项码，
/// 历史值就比不到一起（看起来是两条互不相干的记录）。
///
/// **按部位限定**：同一测项词在不同部位上口径不同，词典按
/// （测项词面 × 部位基名）判定；不适用即**未命中** ——
/// 显式失败（`ITEM_DICT_MISSING`）并指回那个词，
/// 运维照着补词典；**不静默丢弃测项文本**。
///
/// **词典真源是文件**（`assets/dictionary/items.json`，随仓库发布）：
/// 口径改动落成一次词典版本变更，而不是改代码重发版 ——
/// 运维照着失败清单补词，改完词典即时生效（热更新见 Task 26）。
#[derive(Debug)]
pub struct ItemsDictionary {
    /// 词典版本号。
    version: String,
    /// 词典来源（文件路径 / 内置副本）—— 运维看得见当前用的是哪一份。
    source: String,
    /// 测项词面表。
    entries: Vec<ItemEntry>,
}

impl ItemsDictionary {
    /// 从 JSON 文本构造词典（**加载时做结构校验**，任一条对不上即报错）。
    pub fn from_json(text: &str) -> Result<Self, DictionaryError> {
        let file: ItemsFile = serde_json::from_str(text).map_err(|e| DictionaryError::ParseFailed {
            subject: SUBJECT_ITEMS,
            message: format!("JSON 解析失败：{}", e),
        })?;

        let version = file.version.trim().to_string();
        if version.is_empty() {
            return Err(schema_invalid(SUBJECT_ITEMS, "词典缺少 version 字段"));
        }
        if file.entries.is_empty() {
            return Err(schema_invalid(
                SUBJECT_ITEMS,
                "词典缺少 entries（测项词面表）",
            ));
        }

        let mut entries: Vec<ItemEntry> = Vec::new();
        let mut seen_aliases: Vec<String> = Vec::new();
        for e in &file.entries {
            let alias = e.alias.trim().to_string();
            let suffix = e.suffix.trim().to_string();
            if alias.is_empty() {
                return Err(schema_invalid(SUBJECT_ITEMS, "测项词面不得为空"));
            }
            if seen_aliases.contains(&alias) {
                return Err(schema_invalid(
                    SUBJECT_ITEMS,
                    format!("测项词面重复：{}", alias),
                ));
            }
            if suffix.is_empty() {
                return Err(schema_invalid(
                    SUBJECT_ITEMS,
                    format!("测项 {} 缺少测项码后缀", alias),
                ));
            }
            if e.bases.is_empty() {
                // 不限定部位就会编出「肾残余尿量」这种不存在的口径 ——
                // 拿不存在的历史值去比，看起来正常、实际比错。
                return Err(schema_invalid(
                    SUBJECT_ITEMS,
                    format!("测项 {} 没有适用部位", alias),
                ));
            }
            let mut bases: Vec<String> = Vec::new();
            for b in &e.bases {
                let b = b.trim().to_string();
                if b.is_empty() {
                    return Err(schema_invalid(
                        SUBJECT_ITEMS,
                        format!("测项 {} 的适用部位不得为空", alias),
                    ));
                }
                if bases.contains(&b) {
                    return Err(schema_invalid(
                        SUBJECT_ITEMS,
                        format!("测项 {} 的适用部位重复：{}", alias, b),
                    ));
                }
                bases.push(b);
            }
            seen_aliases.push(alias.clone());
            entries.push(ItemEntry {
                alias,
                suffix,
                bases,
                version: e.version.clone().unwrap_or_else(|| version.clone()),
            });
        }

        Ok(ItemsDictionary {
            version,
            source: "内置副本".to_string(),
            entries,
        })
    }

    /// 从文件加载词典。
    pub fn load(path: &Path) -> Result<Self, DictionaryError> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(DictionaryError::FileNotFound {
                    subject: SUBJECT_ITEMS,
                    path: path.to_path_buf(),
                })
            }
            Err(e) => {
                return Err(DictionaryError::Unreadable {
                    subject: SUBJECT_ITEMS,
                    path: path.to_path_buf(),
                    message: e.to_string(),
                })
            }
        };
        let mut dict = Self::from_json(&text).map_err(|e| match e {
            DictionaryError::ParseFailed { subject, message } => DictionaryError::ParseFailed {
                subject,
                message: format!("{}：{}", path.display(), message),
            },
            other => other,
        })?;
        dict.source = format!("文件：{}", path.display());
        Ok(dict)
    }

    /// 词典版本号。
    pub fn version(&self) -> &str {
        &self.version
    }

    /// 词典来源（文件路径 / 内置副本）。
    pub fn source(&self) -> &str {
        &self.source
    }

    /// 标注词典来源（退回内置副本时用 —— 运维要看得见当前用的是哪一份）。
    pub fn set_source(&mut self, source: impl Into<String>) {
        self.source = source.into();
    }

    /// 测项词面 → 词典条目；**查不到返回 `None`**（该测项词没收进词典）。
    pub fn entry_of(&self, alias: &str) -> Option<&ItemEntry> {
        self.entries.iter().find(|e| e.alias == alias)
    }

    /// （测项词面 × 部位基名）→ 测项码后缀；**查不到返回 `None`**（不猜、不回落默认测项）。
    ///
    /// 部位基名为 `None` 时（部位未归一）**查不到** ——
    /// 合成键缺一部就不成立，不能拿「一个部位的后缀」去套另一个部位。
    pub fn suffix_of(&self, alias: &str, base: Option<&str>) -> Option<&str> {
        let base = base?;
        let entry = self.entry_of(alias)?;
        if entry.bases.iter().any(|b| b == base) {
            Some(entry.suffix.as_str())
        } else {
            None
        }
    }

    /// 词典引用的部位基名里，**部位词典没有**的那些。
    ///
    /// 测项码 = 部位基名 + `.` + 后缀 —— 基名不在部位词典里，
    /// 拼出来的键**永远匹配不到任何历史值**：对拍会静悄悄地一条都不比上，
    /// 报告看上去「都处理了」，其实一条都没对上。
    pub fn unknown_bases<'a>(&'a self, known: &[&str]) -> Vec<&'a str> {
        let mut out: Vec<&str> = Vec::new();
        for entry in &self.entries {
            for b in &entry.bases {
                if !known.contains(&b.as_str()) && !out.contains(&b.as_str()) {
                    out.push(b.as_str());
                }
            }
        }
        out
    }

    /// 词典收录的全部别名（用例与词典资产核对用）。
    pub fn aliases(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.alias.as_str()).collect()
    }

    /// 测项词面表（逐条断言用）。
    pub fn entries(&self) -> &[ItemEntry] {
        &self.entries
    }

    /// 词典条目数。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 词典是否为空。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// 测项词典与部位词典的**口径一致性检查**（部位基名同源）。
///
/// 测项码 = 部位基名 + `.` + 后缀。基名不在部位词典里，
/// 拼出来的键永远匹配不到历史值 —— 对拍静悄悄地一条都不比上。
/// 这里显式拦住：词典写错了就报错，不让错口径进台账。
pub fn check_items_bases_known(
    items: &ItemsDictionary,
    parts: &PartsDictionary,
) -> Result<(), DictionaryError> {
    let known: Vec<&str> = parts.bases().iter().map(|b| b.base.as_str()).collect();
    let unknown = items.unknown_bases(&known);
    if unknown.is_empty() {
        Ok(())
    } else {
        Err(schema_invalid(
            SUBJECT_ITEMS,
            format!(
                "测项词典引用了部位词典里没有的部位基名：{}",
                unknown.join(" / ")
            ),
        ))
    }
}

/// 测项词典默认路径（配置文件 `dictionary.items_file` 可覆盖）。
pub fn default_items_path() -> PathBuf {
    PathBuf::from(crate::config::DEFAULT_ITEMS_FILE)
}

/// 编译期内置的测项词典 JSON（与 `assets/dictionary/items.json` **同一份文件**）。
///
/// 文件缺失时退回的就是这一份 —— 用例拿它与文件逐条比对，
/// 保证「退回内置」退回来的不是另一份口径。
pub fn embedded_items_json() -> &'static str {
    EMBEDDED_ITEMS_JSON
}

/// 测项词典（**当前生效的那一代**）。
pub fn items_dictionary() -> Result<Arc<ItemsDictionary>, DictionaryError> {
    let snapshot = super::dictionary_lifecycle::current_snapshot().ok_or_else(|| {
        DictionaryError::Unavailable {
            subject: SUBJECT_ITEMS,
            message: "词典尚未装载，或装载时硬失败".to_string(),
        }
    })?;
    Ok(Arc::clone(snapshot.items_handle()))
}

/// 测项词典版本号（随条目透传到台账行）。
///
/// 词典不可用时返回 `items-unavailable` —— 这不是一个可用的版本号，
/// 守护进程在词典加载失败时已直接退出，不会拿它落台账。
pub fn items_version() -> String {
    super::dictionary_lifecycle::current_snapshot()
        .map(|s| s.items_version().to_string())
        .unwrap_or_else(|| "items-unavailable".to_string())
}

/// （测项词面 × 部位基名）→ 测项码后缀；**查不到返回 `None`**（不猜测项）。
///
/// 这是全项目唯一的「测项词面 → 后缀」映射入口：合并层与抽取层都走这里，
/// 不各带一份映射（两处真源必然漂移）。
pub fn item_suffix_of(alias: &str, base: Option<&str>) -> Option<String> {
    let snapshot = super::dictionary_lifecycle::current_snapshot()?;
    snapshot
        .items()
        .suffix_of(alias, base)
        .map(|s| s.to_string())
}

/// 测项词面 → 词典条目；**查不到返回 `None`**（该测项词没收进词典）。
///
/// 与 [`item_suffix_of`] 分工：这个只看**词面收没收**，
/// 那个还看**该部位适不适用** —— 运维补词典时的动作不同。
pub fn item_entry_of(alias: &str) -> Option<ItemEntry> {
    let snapshot = super::dictionary_lifecycle::current_snapshot()?;
    snapshot.items().entry_of(alias).cloned()
}

/// 词典版本（部位 + 测项）—— 随台账行落库，供口径追溯。
///
/// 部位版本来自**已加载的部位词典文件**（口径变了历史行仍留旧版本）；
/// 测项版本来自测项词典（同样由文件给出）。
///
/// **热更新期间的口径留痕**：这里给的是**当前生效那一代**的版本号 ——
/// 条目自己记着解析当时生效的版本（[`crate::parse::extract::ParsedItem`]），
/// 台账行以条目自带的版本为准，调用方盖错盖不翻。
pub fn dictionary_version() -> String {
    match super::dictionary_lifecycle::current_snapshot() {
        Some(snapshot) => snapshot.version().to_string(),
        None => format!("{}+{}", parts_version(), items_version()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 仓库随附的部位词典文件。
    fn repo_parts_json() -> &'static str {
        EMBEDDED_PARTS_JSON
    }

    /// 加载仓库随附的词典（结构必须自洽）。
    fn repo_dict() -> PartsDictionary {
        PartsDictionary::from_json(repo_parts_json()).expect("随仓库发布的部位词典必须合法")
    }

    #[test]
    fn 随仓库发布的词典结构自洽() {
        let dict = repo_dict();
        assert_eq!(dict.version(), "parts-0.2.0");
        assert!(!dict.is_empty());
        assert_eq!(dict.len(), 13);
        assert_eq!(dict.bases().len(), 5);
        // 词面与别名都不得重复（重复会让查表结果依赖顺序）。
        let mut faces = dict.word_faces();
        faces.sort_unstable();
        faces.dedup();
        assert_eq!(faces.len(), dict.word_faces().len());
        let mut aliases = dict.aliases();
        aliases.sort_unstable();
        aliases.dedup();
        assert_eq!(aliases.len(), dict.aliases().len());
    }

    #[test]
    fn 泌尿系部位词面查到规范基名() {
        let dict = repo_dict();
        assert_eq!(dict.base_of("肾"), Some("kidney"));
        assert_eq!(dict.base_of("肾脏"), Some("kidney"));
        assert_eq!(dict.base_of("肾盂"), Some("renal_pelvis"));
        assert_eq!(dict.base_of("肾盏"), Some("renal_calyx"));
        assert_eq!(dict.base_of("输尿管"), Some("ureter"));
        assert_eq!(dict.base_of("膀胱"), Some("bladder"));
    }

    #[test]
    fn 词典外部位词查不到基名() {
        // 腹部器官不在泌尿系口径内：查不到就是查不到，不回落默认部位。
        let dict = repo_dict();
        for alias in ["胆囊", "肝", "脾", "前列腺", "子宫", ""] {
            assert_eq!(dict.base_of(alias), None, "{} 不应命中部位词典", alias);
        }
    }

    #[test]
    fn 别名写法归一到同一基名() {
        let dict = repo_dict();
        assert_eq!(dict.base_of("肾"), dict.base_of("肾脏"));
    }

    #[test]
    fn 肾盂与肾是不同基名() {
        // 肾盂前后径与肾前后径是不同测量：合成同一基名会让历史值互相比较。
        let dict = repo_dict();
        assert_ne!(dict.base_of("肾盂"), dict.base_of("肾"));
        assert_ne!(dict.base_of("肾盏"), dict.base_of("肾"));
    }

    #[test]
    fn 完整别名查到规范部位键() {
        let dict = repo_dict();
        assert_eq!(dict.part_key_of_alias("左肾"), Some("kidney.left"));
        assert_eq!(dict.part_key_of_alias("左侧肾脏"), Some("kidney.left"));
        assert_eq!(dict.part_key_of_alias("右肾"), Some("kidney.right"));
        assert_eq!(dict.part_key_of_alias("双肾"), Some("kidney.bilateral"));
        assert_eq!(dict.part_key_of_alias("左侧输尿管"), Some("ureter.left"));
        assert_eq!(dict.part_key_of_alias("膀胱"), Some("bladder"));
    }

    #[test]
    fn 词典外的别名查不到规范部位键() {
        let dict = repo_dict();
        for alias in ["胆囊", "肝", "", "左肝"] {
            assert_eq!(
                dict.part_key_of_alias(alias),
                None,
                "{} 不应命中部位词典",
                alias
            );
        }
    }

    #[test]
    fn 基名加侧别查到规范部位键() {
        let dict = repo_dict();
        assert_eq!(dict.part_key_of("kidney", "left"), Some("kidney.left"));
        assert_eq!(dict.part_key_of("kidney", "right"), Some("kidney.right"));
        assert_eq!(
            dict.part_key_of("kidney", "bilateral"),
            Some("kidney.bilateral")
        );
        assert_eq!(dict.part_key_of("bladder", "none"), Some("bladder"));
        // 词典没收的组合就是没有：不拼一个词典不认的键出来。
        assert_eq!(dict.part_key_of("bladder", "left"), None);
        assert_eq!(dict.part_key_of("gallbladder", "left"), None);
    }

    #[test]
    fn 词典带版本号且词面可枚举() {
        let dict = repo_dict();
        assert!(dict.version().starts_with("parts-"));
        let faces = dict.word_faces();
        assert!(faces.contains(&"肾"));
        assert!(faces.contains(&"膀胱"));
    }

    #[test]
    fn 词典结构不合法时报错而不是悄悄兜底() {
        // 规范键与基名+侧别对不上 → 报错（不拿错的键进台账）。
        let bad = r#"{"version":"parts-9.9.9",
            "bases":[{"base":"kidney","word_faces":["肾"]}],
            "entries":[{"part_key":"kidney.lefty","base":"kidney","side":"left","aliases":["左肾"]}]}"#;
        assert!(matches!(
            PartsDictionary::from_json(bad),
            Err(DictionaryError::SchemaInvalid { .. })
        ));
        // 别名重复 → 报错。
        let dup = r#"{"version":"parts-9.9.9",
            "bases":[{"base":"kidney","word_faces":["肾"]}],
            "entries":[
              {"part_key":"kidney.left","base":"kidney","side":"left","aliases":["左肾"]},
              {"part_key":"kidney.right","base":"kidney","side":"right","aliases":["左肾"]}]}"#;
        assert!(matches!(
            PartsDictionary::from_json(dup),
            Err(DictionaryError::SchemaInvalid { .. })
        ));
        // 缺 version → 报错。
        let no_version = r#"{"bases":[],"entries":[]}"#;
        assert!(matches!(
            PartsDictionary::from_json(no_version),
            Err(DictionaryError::SchemaInvalid { .. })
        ));
        // 不是 JSON → 解析失败。
        assert!(matches!(
            PartsDictionary::from_json("{不是 json"),
            Err(DictionaryError::ParseFailed { .. })
        ));
    }

    #[test]
    fn 词典加载错误带稳定错误码() {
        let err = DictionaryError::SchemaInvalid {
            subject: SUBJECT_PARTS,
            message: "x".to_string(),
        };
        assert_eq!(err.code(), "DICT_SCHEMA_INVALID");
        assert!(err.message().contains("部位词典"));
        assert_eq!(
            DictionaryError::FileNotFound {
                subject: SUBJECT_PARTS,
                path: PathBuf::from("/x/parts.json")
            }
            .code(),
            "DICT_FILE_NOT_FOUND"
        );
        // 测项词典的错要说清是哪本词典 —— 运维照着去改对应文件。
        let items_err = DictionaryError::SchemaInvalid {
            subject: SUBJECT_ITEMS,
            message: "x".to_string(),
        };
        assert!(items_err.message().contains("测项词典"));
        assert_eq!(items_err.code(), "DICT_SCHEMA_INVALID");
    }

    #[test]
    fn 测项词面查到规范后缀() {
        // 测项码按（词面 × 部位基名）判定 —— 同一词在不同部位上口径不同。
        assert_eq!(
            item_suffix_of("残余尿量", Some("bladder")).as_deref(),
            Some("residual_urine_volume")
        );
        assert_eq!(
            item_suffix_of("前后径", Some("kidney")).as_deref(),
            Some("ap_diameter")
        );
        assert_eq!(
            item_suffix_of("内径", Some("ureter")).as_deref(),
            Some("inner_diameter")
        );
    }

    #[test]
    fn 测项多种写法归一到同一后缀() {
        // 「长」与「长度」是同一个测量：写法不同，测项码必须相同，
        // 否则同一侧的历史值比不到一起。
        assert_eq!(
            item_suffix_of("长", Some("kidney")),
            item_suffix_of("长度", Some("kidney"))
        );
        assert_eq!(
            item_suffix_of("宽", Some("kidney")),
            item_suffix_of("宽度", Some("kidney"))
        );
        assert_eq!(item_suffix_of("长", Some("kidney")).as_deref(), Some("length"));
        assert_eq!(item_suffix_of("宽", Some("kidney")).as_deref(), Some("width"));
    }

    #[test]
    fn 测项按部位限定_不适用即查不到() {
        // 残余尿量是膀胱的测量，不是肾的测量：部位不适用即**查不到**，
        // 不编出「肾残余尿量」这种不存在的口径（那会拿不存在的历史值去比）。
        assert_eq!(item_suffix_of("残余尿量", Some("kidney")), None);
        assert_eq!(item_suffix_of("前后径", Some("bladder")), None);
        // 部位缺一部 → 合成键不成立 → 查不到。
        assert_eq!(item_suffix_of("前后径", None), None);
    }

    #[test]
    fn 测项词典外的词查不到后缀() {
        // 词典外的测项词显式失败，不回落默认测项。
        for alias in ["厚度", "容积", "大小", "", "积水"] {
            assert_eq!(
                item_suffix_of(alias, Some("kidney")),
                None,
                "{} 不应命中测项词典",
                alias
            );
        }
    }

    #[test]
    fn 测项词典带版本号且别名可枚举() {
        let dict = items_dictionary().expect("测项词典必须可用");
        assert_eq!(dict.version(), "items-0.1.0");
        assert!(!dict.is_empty());
        assert_eq!(dict.len(), 7);
        let aliases = dict.aliases();
        assert!(aliases.contains(&"残余尿量"));
        assert!(aliases.contains(&"前后径"));
        let mut uniq = aliases.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(uniq.len(), aliases.len());
    }
}
