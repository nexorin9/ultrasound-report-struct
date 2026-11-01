//! 测项别名词典 + 版本化 —— 逐条断言（Task 25）。
//!
//! **这一组用例守什么**：
//!
//! 1. **每条别名都归一到预期测项码后缀** —— 测项码与部位键共同构成对拍用的
//!    规范键；别名归一错了，历史值就比不到一起（看起来是两条互不相干的记录）。
//! 2. **词典外的写法查不到** —— 查不到就是查不到，不猜、不回落默认测项；
//!    由调用方转 `ITEM_DICT_MISSING`，运维照着补词典。
//! 3. **测项码与部位键同源** —— 规范测项码 = 部位基名 + `.` + 后缀；
//!    测项词典引用的部位基名必须在部位词典里存在，否则键永远匹配不到历史值。
//! 4. **版本号来自词典文件** —— 台账行落的是**词典自己的版本号**
//!    （不是写死在代码里的常量），口径变了历史行仍留旧版本。
//!
//! 断言用的期望表**独立于词典文件**（写死在本文件里）：
//! 词典文件改了而期望表没改，用例立刻失败 —— 这正是要的「口径漂了看得见」。

use crate::coerce::dictionary::{
    check_items_bases_known, embedded_items_json, item_entry_of, item_suffix_of, items_dictionary,
    items_version, part_base_of, parts_dictionary, DictionaryError, ItemsDictionary,
};
use crate::parse::parse_report;

/// 期望的「测项词面 → 测项码后缀」对照表（独立于词典文件写死）。
const EXPECTED_ALIAS_TO_SUFFIX: &[(&str, &str)] = &[
    ("残余尿量", "residual_urine_volume"),
    ("前后径", "ap_diameter"),
    ("长度", "length"),
    ("宽度", "width"),
    ("内径", "inner_diameter"),
    ("长", "length"),
    ("宽", "width"),
];

/// 期望的「测项词面 → 适用部位基名」对照表（独立于词典文件写死）。
const EXPECTED_ALIAS_TO_BASES: &[(&str, &[&str])] = &[
    ("残余尿量", &["bladder"]),
    ("前后径", &["kidney", "renal_pelvis"]),
    ("长度", &["kidney", "renal_pelvis"]),
    ("宽度", &["kidney", "renal_pelvis"]),
    ("内径", &["ureter", "bladder"]),
    ("长", &["kidney", "renal_pelvis"]),
    ("宽", &["kidney", "renal_pelvis"]),
];

/// 测项词典（从仓库随附的文件加载）。
fn dict() -> std::sync::Arc<ItemsDictionary> {
    items_dictionary().expect("测项词典必须可用")
}

#[test]
fn 每条别名都归一到预期测项码后缀() {
    let dict = dict();
    for (alias, expected_suffix) in EXPECTED_ALIAS_TO_SUFFIX {
        let base = EXPECTED_ALIAS_TO_BASES
            .iter()
            .find(|(a, _)| a == alias)
            .and_then(|(_, bases)| bases.first())
            .copied();
        assert_eq!(
            item_suffix_of(alias, base).as_deref(),
            Some(*expected_suffix),
            "测项词 {} 在部位 {:?} 下应归一到 {}",
            alias,
            base,
            expected_suffix
        );
        assert_eq!(
            dict.entry_of(alias).map(|e| e.suffix.as_str()),
            Some(*expected_suffix),
            "词典直查：测项词 {} 应归一到 {}",
            alias,
            expected_suffix
        );
    }
}

#[test]
fn 词典收录的别名与期望表逐条一致() {
    // 词典多收一个别名、少收一个别名，都在这里暴露（口径漂了看得见）。
    let dict = dict();
    let mut actual: Vec<&str> = dict.aliases();
    actual.sort_unstable();
    let mut expected: Vec<&str> = EXPECTED_ALIAS_TO_SUFFIX.iter().map(|(a, _)| *a).collect();
    expected.sort_unstable();
    assert_eq!(actual, expected);
}

#[test]
fn 每条测项的适用部位与期望表逐条一致() {
    // 不限定部位就会编出「肾残余尿量」这种不存在的口径：
    // 拿不存在的历史值去比，看起来正常、实际比错。
    let dict = dict();
    for (alias, expected_bases) in EXPECTED_ALIAS_TO_BASES {
        let entry = dict.entry_of(alias).expect("词典应收录该测项");
        let mut actual: Vec<&str> = entry.bases.iter().map(|b| b.as_str()).collect();
        actual.sort_unstable();
        let mut expected: Vec<&str> = expected_bases.to_vec();
        expected.sort_unstable();
        assert_eq!(actual, expected, "测项 {} 的适用部位对不上", alias);
    }
}

#[test]
fn 多种写法归一到同一测项码() {
    // 「长」与「长度」是同一个测量：写法不同，测项码必须相同，
    // 否则同一侧的历史值比不到一起（看起来是两条互不相干的记录）。
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
    // 残余尿量是膀胱的测量，不是肾的测量：部位不适用即**查不到**。
    assert_eq!(item_suffix_of("残余尿量", Some("kidney")), None);
    assert_eq!(item_suffix_of("前后径", Some("bladder")), None);
    // 部位缺一部 → 合成键不成立 → 查不到。
    assert_eq!(item_suffix_of("前后径", None), None);
}

#[test]
fn 词典外的测项词查不到() {
    // 词典外的测项词显式失败（ITEM_DICT_MISSING），不回落默认测项。
    for alias in ["厚度", "容积", "大小", "积水", "", "肾积水"] {
        assert_eq!(
            item_suffix_of(alias, Some("kidney")),
            None,
            "{} 不应命中测项词典",
            alias
        );
        assert!(item_entry_of(alias).is_none(), "{} 不应有词典条目", alias);
    }
}

#[test]
fn 词典版本号来自词典文件而不是写死的常量() {
    // 词典文件里的 version 字段就是生效版本号（改文件即改口径留痕）。
    let file: serde_json::Value =
        serde_json::from_str(embedded_items_json()).expect("内置测项词典必须是合法 JSON");
    assert_eq!(file["version"].as_str(), Some(items_version().as_str()));
    assert!(items_version().starts_with("items-"));
}

#[test]
fn 词典文件与编译期内置副本是同一份内容() {
    // 值班机只拷可执行文件时退回内置副本 —— 内置副本必须与文件一致，
    // 否则「文件缺失退回内置」会退回一份旧口径。
    let repo_file = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/dictionary/items.json"),
    )
    .expect("仓库必须随附测项词典文件");
    let from_file = ItemsDictionary::from_json(&repo_file).unwrap();
    let from_embedded = ItemsDictionary::from_json(embedded_items_json()).unwrap();
    assert_eq!(from_file.version(), from_embedded.version());
    assert_eq!(from_file.entries(), from_embedded.entries());
}

#[test]
fn 测项词典引用的部位基名必须在部位词典里存在() {
    // 测项码 = 部位基名 + "." + 后缀：基名不在部位词典里，
    // 拼出来的键永远匹配不到历史值 —— 对拍静悄悄地一条都不比上。
    let parts = parts_dictionary().expect("部位词典必须可用");
    check_items_bases_known(&dict(), &parts).expect("两本词典的部位基名应当同源");

    // 引用了部位词典没有的基名 → 显式报错，不让错口径进台账。
    let bad = r#"{"version":"items-9.9.9",
        "entries":[{"alias":"积水深度","suffix":"depth","bases":["gallbladder"]}]}"#;
    let bad_dict = ItemsDictionary::from_json(bad).unwrap();
    let err = check_items_bases_known(&bad_dict, &parts).unwrap_err();
    assert_eq!(err.code(), "DICT_SCHEMA_INVALID");
    assert!(err.message().contains("gallbladder"), "{}", err.message());
}

#[test]
fn 词典结构不合法时报错而不是悄悄兜底() {
    // 缺 version → 结构不合法（不是 serde 解析失败）。
    let no_version = r#"{"entries":[{"alias":"长","suffix":"length","bases":["kidney"]}]}"#;
    assert!(matches!(
        ItemsDictionary::from_json(no_version),
        Err(DictionaryError::SchemaInvalid { .. })
    ));
    // 别名重复 → 报错（重复会让查表结果依赖顺序）。
    let dup = r#"{"version":"items-9.9.9","entries":[
        {"alias":"长","suffix":"length","bases":["kidney"]},
        {"alias":"长","suffix":"length","bases":["renal_pelvis"]}]}"#;
    assert!(matches!(
        ItemsDictionary::from_json(dup),
        Err(DictionaryError::SchemaInvalid { .. })
    ));
    // 没有适用部位 → 报错（不限定就会编出不存在的口径）。
    let no_bases = r#"{"version":"items-9.9.9",
        "entries":[{"alias":"积水深度","suffix":"depth","bases":[]}]}"#;
    assert!(matches!(
        ItemsDictionary::from_json(no_bases),
        Err(DictionaryError::SchemaInvalid { .. })
    ));
    // 不是 JSON → 解析失败。
    assert!(matches!(
        ItemsDictionary::from_json("{不是 json"),
        Err(DictionaryError::ParseFailed { .. })
    ));
}

#[test]
fn 词典加载失败时显式报错而不是悄悄兜底() {
    // 词典文件存在却坏了 → 硬失败：运维改坏的词典不被悄悄忽略。
    let dir = std::env::temp_dir().join(format!("urs-dict-items-bad-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bad = dir.join("items.json");
    std::fs::write(&bad, "{\"version\":\"items-9.9.9\",\"entries\":[]}").unwrap();
    let err = ItemsDictionary::load(&bad).unwrap_err();
    assert_eq!(err.code(), "DICT_SCHEMA_INVALID");
    assert!(err.message().contains("测项词典"), "{}", err.message());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn 规范键_部位键与测项码组合后稳定可复现() {
    // 对拍用的规范键 =（部位键, 测项码）。两边都由词典给出，
    // 同一份报告反复解析必须得到同一个键 —— 键不稳定，历史值就永远比不上。
    for (body, expected_part, expected_item) in [
        ("左肾前后径 2.1cm", "kidney.left", "kidney.ap_diameter"),
        ("左肾长 8.5cm", "kidney.left", "kidney.length"),
        ("右肾宽度 4.3cm", "kidney.right", "kidney.width"),
        ("左肾盂前后径 2.1cm", "renal_pelvis.left", "renal_pelvis.ap_diameter"),
        ("膀胱残余尿量 120ml", "bladder", "bladder.residual_urine_volume"),
        ("双侧输尿管内径 0.6cm", "ureter.bilateral", "ureter.inner_diameter"),
    ] {
        let mut seen: Vec<(Option<String>, Option<String>)> = Vec::new();
        for _ in 0..3 {
            let outcome = parse_report(body);
            assert_eq!(outcome.items.len(), 1, "应从「{}」解析出 1 条", body);
            let key = (
                outcome.items[0].part_key.clone(),
                outcome.items[0].item_code.clone(),
            );
            assert_eq!(
                key,
                (
                    Some(expected_part.to_string()),
                    Some(expected_item.to_string())
                ),
                "「{}」的规范键对不上",
                body
            );
            seen.push(key);
        }
        // 同一份报告反复解析 → 规范键逐条一致（可复现，不随时钟漂移）。
        assert_eq!(seen[0], seen[1]);
        assert_eq!(seen[1], seen[2]);
    }
}

#[test]
fn 测项码由部位基名与后缀拼成_不靠代码里硬写的字符串() {
    // 规范测项码 = 部位基名 + "." + 后缀 —— 两段都来自词典。
    // 这里把「部位基名 → 部位键」与「测项词 → 后缀」两段拼起来，
    // 拼出来的键必须与解析出的 item_code 一致。
    let outcome = parse_report("左肾前后径 2.1cm");
    let base = part_base_of("肾").expect("肾应命中部位词典");
    let suffix = item_suffix_of("前后径", Some(&base)).expect("前后径应命中测项词典");
    assert_eq!(
        outcome.items[0].item_code.as_deref(),
        Some(format!("{}.{}", base, suffix).as_str())
    );
}

#[test]
fn 词典版本号随条目透传到台账行() {
    // 台账行落的是**词典自己的版本号**（部位版本 + 测项版本）。
    let outcome = parse_report("左肾积水 3.2cm");
    assert_eq!(outcome.items.len(), 1);
    assert_eq!(
        outcome.items[0].dictionary_version,
        crate::dictionary_version(),
        "条目应带着解析当时生效的词典版本"
    );
    assert!(outcome.items[0].dictionary_version.contains(&items_version()));
}
