//! 强转内核：量纲路径判定、单位目录、UCUM 码、院内词典。
//!
//! 主路径：[`path::decide`] 一次判定给出**路径标签 + 归一值 + 单位码 + 步骤链**；
//! 查不到合法路径返回 `path = None` 且**不产出数值** —— 显式失败，不静默兜底。
//!
//! - [`path::find_coercion_path`]：纯数值的路径判定。
//! - [`path::coerce_raw`]：从**报告原值词面**驱动（含原文写明的倍率 `3.2×2`），
//!   强转路径对照表按这个入口逐条断言。
//! - [`path::decide`]：抽取层用的判定入口 —— 路径与换算**同源**，
//!   不存在「判了一条路、换算走另一条」的分叉。
//! - [`ucum::canonical_code`]：**名字层** —— UCUM 码 ↔ 中文单位名 的双向映射
//!   （大小写不敏感、未知即拒绝）；[`units::canonical_unit`] 经它认单位。
//! - [`table`]：**强转路径对照表** —— 口径的判据表；逐行驱动判定内核，
//!   任一行对不上就是口径漂了（[`table::CoercionTable::assert_all`]）。

pub mod dictionary;
pub mod dictionary_lifecycle;
pub mod path;
pub mod table;
pub mod ucum;
pub mod units;

#[cfg(test)]
mod dictionary_items;
#[cfg(test)]
mod dictionary_parts;

pub use dictionary::{
    check_items_bases_known, default_items_path, default_parts_path, dictionary_version,
    embedded_items_json, embedded_parts_json, item_entry_of, item_suffix_of, items_dictionary,
    items_version, part_base_of, part_key_of_alias, part_key_of_base_side, parts_dictionary,
    parts_version, BaseEntry, DictionaryError, ItemEntry, ItemsDictionary, PartEntry,
    PartsDictionary,
};
pub use dictionary_lifecycle::{
    current_snapshot, reload_current_if_changed, scope_current, scoped_snapshot, Dictionaries,
    DictionaryHandle, DictionarySnapshot, ReloadOutcome, ScopeGuard,
};
pub use path::{
    coerce, coerce_raw, coerce_scaled, comparable, decide, find_coercion_path,
    find_coercion_path_raw, CoercionDecision, CoercionFailure, CoercionPath, CoercionStep,
};
pub use table::{check_row, report, CoercionTable, Mismatch, TableError, TableRow};
pub use ucum::{
    canonical_code, code_of_zh, is_known, ucum_name_en, zh_name, UcumEntry, ENTRIES,
};
pub use units::{
    canonical_unit, catalog, conversion_source, conversion_table, convert, convert_to_target,
    is_measurement_value, round3, target_code, Conversion, Family, UnitSpec,
    MAX_MEASUREMENT_MAGNITUDE,
};
