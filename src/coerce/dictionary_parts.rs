//! 部位别名词典 + 版本化 —— 逐条断言（Task 24）。
//!
//! **这一组用例守什么**：
//!
//! 1. **每条别名都归一到预期规范部位键** —— 部位词典是「同一患者同侧部位」
//!    能自动对上的前提；别名归一错了，差值就会拿左肾去比右肾
//!    （看起来正常、实际比错）。
//! 2. **词典外的写法查不到** —— 查不到就是查不到，不猜、不回落默认部位；
//!    由调用方转 `part_unresolved`，那一行不参与对拍。
//! 3. **词典版本号随条目透传** —— 台账行落的是**词典自己的版本号**
//!    （不是写死在代码里的常量），口径变了历史行仍留旧版本。
//!
//! 断言用的期望表**独立于词典文件**（写死在本文件里）：
//! 词典文件改了而期望表没改，用例立刻失败 —— 这正是要的「口径漂了看得见」。

use crate::coerce::dictionary::{
    embedded_parts_json, item_entry_of, items_version, part_base_of, part_key_of_alias,
    parts_dictionary, parts_version, PartsDictionary,
};
use crate::export::export_ledger;
use crate::parse::parse_report;
use crate::state::idempotency_key;
use crate::state::repo::{IngestRecord, Repo};

/// 期望的「完整别名 → 规范部位键」对照表（独立于词典文件写死）。
const EXPECTED_ALIAS_TO_KEY: &[(&str, &str)] = &[
    ("左肾", "kidney.left"),
    ("左侧肾脏", "kidney.left"),
    ("左肾脏", "kidney.left"),
    ("左侧肾", "kidney.left"),
    ("右肾", "kidney.right"),
    ("右侧肾脏", "kidney.right"),
    ("右肾脏", "kidney.right"),
    ("右侧肾", "kidney.right"),
    ("双肾", "kidney.bilateral"),
    ("双侧肾", "kidney.bilateral"),
    ("两侧肾", "kidney.bilateral"),
    ("左肾盂", "renal_pelvis.left"),
    ("左侧肾盂", "renal_pelvis.left"),
    ("右肾盂", "renal_pelvis.right"),
    ("右侧肾盂", "renal_pelvis.right"),
    ("双肾盂", "renal_pelvis.bilateral"),
    ("双侧肾盂", "renal_pelvis.bilateral"),
    ("左肾盏", "renal_calyx.left"),
    ("左侧肾盏", "renal_calyx.left"),
    ("右肾盏", "renal_calyx.right"),
    ("右侧肾盏", "renal_calyx.right"),
    ("双肾盏", "renal_calyx.bilateral"),
    ("双侧肾盏", "renal_calyx.bilateral"),
    ("左输尿管", "ureter.left"),
    ("左侧输尿管", "ureter.left"),
    ("右输尿管", "ureter.right"),
    ("右侧输尿管", "ureter.right"),
    ("双输尿管", "ureter.bilateral"),
    ("双侧输尿管", "ureter.bilateral"),
    ("膀胱", "bladder"),
];

/// 期望的「部位词面 → 解剖基名」对照表（独立于词典文件写死）。
const EXPECTED_FACE_TO_BASE: &[(&str, &str)] = &[
    ("肾", "kidney"),
    ("肾脏", "kidney"),
    ("肾盂", "renal_pelvis"),
    ("肾盏", "renal_calyx"),
    ("输尿管", "ureter"),
    ("膀胱", "bladder"),
];

/// 部位词典（从仓库随附的文件加载）。
fn dict() -> std::sync::Arc<PartsDictionary> {
    parts_dictionary().expect("部位词典必须可用")
}

#[test]
fn 每条别名都归一到预期规范部位键() {
    let dict = dict();
    for (alias, expected_key) in EXPECTED_ALIAS_TO_KEY {
        assert_eq!(
            part_key_of_alias(alias).as_deref(),
            Some(*expected_key),
            "别名 {} 应归一到 {}",
            alias,
            expected_key
        );
        assert_eq!(
            dict.part_key_of_alias(alias),
            Some(*expected_key),
            "词典直查：别名 {} 应归一到 {}",
            alias,
            expected_key
        );
    }
}

#[test]
fn 词典收录的别名与期望表逐条一致() {
    // 词典多收一个别名、少收一个别名，都在这里暴露（口径漂了看得见）。
    let dict = dict();
    let mut actual: Vec<&str> = dict.aliases();
    actual.sort_unstable();
    let mut expected: Vec<&str> = EXPECTED_ALIAS_TO_KEY.iter().map(|(a, _)| *a).collect();
    expected.sort_unstable();
    assert_eq!(actual, expected);
}

#[test]
fn 每条部位词面都归一到预期解剖基名() {
    let dict = dict();
    for (face, expected_base) in EXPECTED_FACE_TO_BASE {
        assert_eq!(
            part_base_of(face).as_deref(),
            Some(*expected_base),
            "词面 {} 应归一到 {}",
            face,
            expected_base
        );
        assert_eq!(
            dict.base_of(face),
            Some(*expected_base),
            "词典直查：词面 {} 应归一到 {}",
            face,
            expected_base
        );
    }
    let mut actual: Vec<&str> = dict.word_faces();
    actual.sort_unstable();
    let mut expected: Vec<&str> = EXPECTED_FACE_TO_BASE.iter().map(|(f, _)| *f).collect();
    expected.sort_unstable();
    assert_eq!(actual, expected);
}

#[test]
fn 规范部位键覆盖任务约定的泌尿系部位() {
    // 部位键集合由词典给出：泌尿系（肾 / 输尿管 / 膀胱）左右与双侧。
    let dict = dict();
    for key in [
        "kidney.left",
        "kidney.right",
        "kidney.bilateral",
        "ureter.left",
        "ureter.right",
        "bladder",
    ] {
        assert!(
            dict.entries().iter().any(|e| e.part_key == key),
            "词典应收录规范部位键 {}",
            key
        );
    }
}

#[test]
fn 词典外的写法查不到规范部位键() {
    // 腹部器官不在本产品承诺的泌尿系口径内：查不到就是查不到，
    // 由调用方转 part_unresolved（那一行不参与对拍）。
    for alias in ["胆囊", "肝", "脾", "前列腺", "子宫", "左侧胆囊", ""] {
        assert_eq!(
            part_key_of_alias(alias),
            None,
            "{} 不应命中部位词典",
            alias
        );
    }
}

#[test]
fn 词典版本号随条目透传到台账行() {
    // 台账行落的是**词典自己的版本号**：口径变了历史行仍留旧版本。
    let body = "左肾积水 3.2cm";
    let outcome = parse_report(body);
    assert_eq!(outcome.items.len(), 1, "应解析出 1 条");

    let repo = Repo::open_in_memory().unwrap();
    let key = idempotency_key("SAMPLE-A", body);
    repo.commit_ingestion(&IngestRecord {
        study_no: "SAMPLE-A",
        patient_id: Some("PID-EXAMPLE"),
        source_path: None,
        idempotency_key: &key,
        dictionary_version: crate::dictionary_version(),
        items: &outcome.items,
        failures: &outcome.failures,
    })
    .unwrap();

    let row_version: String = repo
        .connection()
        .query_row("SELECT dictionary_version FROM items LIMIT 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        row_version,
        crate::dictionary_version(),
        "台账行的词典版本必须与当前词典一致"
    );
    assert!(
        row_version.starts_with(&format!("{}+", parts_version())),
        "台账行版本应以部位词典版本打头：{}",
        row_version
    );
    assert!(row_version.ends_with(&items_version()));
}

#[test]
fn 词典版本号随台账行落进导出的csv() {
    let body = "左肾积水 3.2cm";
    let outcome = parse_report(body);
    let repo = Repo::open_in_memory().unwrap();
    let key = idempotency_key("SAMPLE-A", body);
    repo.commit_ingestion(&IngestRecord {
        study_no: "SAMPLE-A",
        patient_id: Some("PID-EXAMPLE"),
        source_path: None,
        idempotency_key: &key,
        dictionary_version: crate::dictionary_version(),
        items: &outcome.items,
        failures: &outcome.failures,
    })
    .unwrap();

    let dir = std::env::temp_dir().join(format!("urs-dict-parts-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("ledger.csv");
    export_ledger(repo.connection(), &out).unwrap();
    let text = std::fs::read_to_string(&out).unwrap();
    assert!(
        text.contains(&crate::dictionary_version()),
        "导出台账应带词典版本：{}",
        text
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn 词典版本随解析结果透传到条目字段() {
    // 解析出的条目本身带着规范部位键 —— 规范键来自词典，不是代码里拼的。
    let outcome = parse_report("左肾积水 3.2cm");
    assert_eq!(outcome.items[0].part_key.as_deref(), Some("kidney.left"));
    let outcome = parse_report("双侧输尿管内径 0.6cm");
    assert_eq!(
        outcome.items[0].part_key.as_deref(),
        Some("ureter.bilateral")
    );
}

#[test]
fn 词典版本来自词典文件而不是写死的常量() {
    // 词典文件里的 version 字段就是生效版本号（改文件即改口径留痕）。
    let file: serde_json::Value =
        serde_json::from_str(embedded_parts_json()).expect("内置词典必须是合法 JSON");
    assert_eq!(
        file["version"].as_str(),
        Some(parts_version().as_str()),
        "生效版本号必须等于词典文件里的 version"
    );
    assert!(parts_version().starts_with("parts-"));
}

#[test]
fn 词典文件与编译期内置副本是同一份内容() {
    // 值班机只拷可执行文件时退回内置副本 —— 内置副本必须与文件一致，
    // 否则「文件缺失退回内置」会退回一份旧口径。
    let repo_file = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/dictionary/parts.json"),
    )
    .expect("仓库必须随附部位词典文件");
    let from_file = PartsDictionary::from_json(&repo_file).unwrap();
    let from_embedded = PartsDictionary::from_json(embedded_parts_json()).unwrap();
    assert_eq!(from_file.version(), from_embedded.version());
    assert_eq!(from_file.entries(), from_embedded.entries());
    assert_eq!(from_file.bases(), from_embedded.bases());
}

#[test]
fn 词典加载失败时显式报错而不是悄悄兜底() {
    // 词典文件存在却坏了 → 硬失败：运维改坏的词典不被悄悄忽略。
    let dir = std::env::temp_dir().join(format!("urs-dict-bad-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bad = dir.join("parts.json");
    std::fs::write(&bad, "{\"version\":\"parts-9.9.9\",\"bases\":[],\"entries\":[]}").unwrap();
    let err = PartsDictionary::load(&bad).unwrap_err();
    assert_eq!(err.code(), "DICT_SCHEMA_INVALID");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn 解析结果自带生效的词典版本() {
    // 解析结果**自己**带着版本 —— 版本是随解析结果走的，不是落库时现盖的。
    let outcome = parse_report("左肾积水 3.2cm");
    assert_eq!(outcome.items.len(), 1);
    assert_eq!(
        outcome.items[0].dictionary_version,
        crate::dictionary_version(),
        "条目应带着解析当时生效的词典版本"
    );
    assert_eq!(
        outcome.dictionary_version,
        crate::dictionary_version(),
        "解析结果级版本应与条目级一致"
    );
    assert!(outcome.items[0].dictionary_version.starts_with("parts-"));
}

#[test]
fn 一条条目都没解析出来时结果仍带版本() {
    // 全是失败条目时没有条目可挂版本 —— 结果级版本仍在，
    // 说明「这份报告是在哪套口径下判的」。
    let outcome = parse_report("左肾积水 3.2");
    assert!(outcome.items.is_empty());
    assert!(!outcome.failures.is_empty());
    assert_eq!(outcome.dictionary_version, crate::dictionary_version());
}

#[test]
fn 台账行的版本以条目自带的版本为准() {
    // 调用方盖错版本盖不翻条目自带的版本：
    // 口径留痕必须说的是「这条数据是哪套口径算出来的」，
    // 而不是「落库那一刻词典是哪个版本」（热更新期间两者会不一样）。
    let body = "左肾积水 3.2cm";
    let outcome = parse_report(body);
    let repo = Repo::open_in_memory().unwrap();
    let key = idempotency_key("SAMPLE-A", body);
    repo.commit_ingestion(&IngestRecord {
        study_no: "SAMPLE-A",
        patient_id: Some("PID-EXAMPLE"),
        source_path: None,
        idempotency_key: &key,
        // 故意盖一个**过期**版本：调用方不该有能力改写条目的口径留痕。
        dictionary_version: "parts-0.0.1-stale+items-0.0.1-stale".to_string(),
        items: &outcome.items,
        failures: &outcome.failures,
    })
    .unwrap();

    let row_version: String = repo
        .connection()
        .query_row("SELECT dictionary_version FROM items LIMIT 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        row_version,
        outcome.items[0].dictionary_version,
        "台账行的版本必须来自条目本身，而不是调用方盖的版本"
    );
    assert_eq!(row_version, crate::dictionary_version());
    assert_ne!(row_version, "parts-0.0.1-stale+items-0.0.1-stale");
}

#[test]
fn 测项词典仍按部位限定() {
    // 词典分两张表：部位管「哪个器官」，测项管「量什么」。
    // 残余尿量是膀胱的测量，不是肾的测量 —— 部位不适用即查不到。
    assert_eq!(
        item_entry_of("残余尿量").map(|e| e.suffix.clone()),
        Some("residual_urine_volume".to_string())
    );
    assert!(!item_entry_of("残余尿量")
        .map(|e| e.bases.iter().any(|b| b == "kidney"))
        .unwrap_or(false));
}
