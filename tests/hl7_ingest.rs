//! HL7 v2 `ORU^R01` 入站适配的集成验收：投递 → 适配 → 落台账 → 导出。
//!
//! 断言口径与 `spec.md` §2.1 / §2.3 一致：
//! - 权威字段取自 HL7 字段（`OBR-3` 检查号 / `PID-3` 病历号 / `OBR-7` 检查日期）；
//! - 所见文本走**同一套**解析内核：`3.2cm → 32mm`，强转路径 `UnitConvert`；
//! - 缺段消息落**失败条目**（`HL7_SEGMENT_MISSING`），不产出台账行；
//! - 重复投递不产生第二条台账行。

use std::path::{Path, PathBuf};

use ultrasound_report_struct::export::export_all;
use ultrasound_report_struct::ingest::ingest_inbox;
use ultrasound_report_struct::state::repo::Repo;

fn samples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/samples")
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "urs-hl7-ingest-{}-{}",
        tag,
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 把一份样例 ORU^R01 投递到 `<inbox>/<study_no>/report.hl7`。
fn deliver(inbox: &Path, study_no: &str, sample: &str) {
    let dir = inbox.join(study_no);
    std::fs::create_dir_all(&dir).unwrap();
    let body = std::fs::read_to_string(samples_dir().join("hl7").join(sample)).unwrap();
    std::fs::write(dir.join("report.hl7"), body).unwrap();
}

fn read_rows(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| l.trim_start_matches('\u{feff}').to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

#[test]
fn 投递标准报文报告产出可核对台账行与失败清单() {
    let dir = temp_dir("main");
    let db = dir.join("urs.db");
    let inbox = dir.join("inbox");
    let out = dir.join("export");

    // 1) 投递两份 ORU^R01：一份正常、一份缺观测结果段。
    deliver(&inbox, "sample-hl7-01", "sample-hl7-01.hl7");
    deliver(&inbox, "sample-hl7-03", "sample-hl7-03.hl7");

    // 2) 建库 + 批量入库。
    let repo = Repo::open(&db).unwrap();
    let summary = ingest_inbox(&repo, &inbox).unwrap();
    assert_eq!(summary.reports, 2, "两份消息都应入库");
    assert_eq!(summary.items, 1, "只有结构完整那份产出结构化条目");
    assert_eq!(summary.failures, 1, "缺段那份产出 1 条失败条目");
    assert_eq!(summary.encoding_failed, 0);

    // 3) 导出：台账行 + 失败清单。
    export_all(repo.connection(), &out).unwrap();
    let ledger = read_rows(&out.join("ledger.csv"));
    assert_eq!(ledger.len(), 2, "表头 + 1 行台账");
    assert!(
        ledger[1].contains("sample-hl7-01"),
        "台账行的检查号应取自 OBR-3：{}",
        ledger[1]
    );
    assert!(
        ledger[1].contains("32") && ledger[1].contains("mm") && ledger[1].contains("UnitConvert"),
        "3.2cm 应强转为 32mm（UnitConvert）：{}",
        ledger[1]
    );

    let failures = read_rows(&out.join("failures.csv"));
    assert_eq!(failures.len(), 2, "表头 + 1 行失败");
    assert!(
        failures[1].contains("HL7_SEGMENT_MISSING"),
        "缺段应落稳定错误码：{}",
        failures[1]
    );
    assert!(
        failures[1].contains("OBX"),
        "失败信息应含缺失段名：{}",
        failures[1]
    );

    // 4) 病历号取自 PID-3（不是目录名，也不是自由文本里的「病历号：」）。
    let patient: String = repo
        .connection()
        .query_row(
            "SELECT patient_id FROM reports WHERE study_no = ?1",
            ["sample-hl7-01"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(patient, "PID-EXAMPLE");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn 重复投递标准报文报告不产生第二条台账行() {
    let dir = temp_dir("idem");
    let db = dir.join("urs.db");
    let inbox = dir.join("inbox");

    deliver(&inbox, "sample-hl7-01", "sample-hl7-01.hl7");

    let repo = Repo::open(&db).unwrap();
    let first = ingest_inbox(&repo, &inbox).unwrap();
    assert_eq!(first.items, 1);
    assert_eq!(first.duplicates, 0);

    // 工作站重投同一份消息 → 幂等命中，不产生第二条台账行。
    let second = ingest_inbox(&repo, &inbox).unwrap();
    assert_eq!(second.reports, 0);
    assert_eq!(second.duplicates, 1);
    assert_eq!(second.items, 0);

    let rows: i64 = repo
        .connection()
        .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 1, "不得产生第二条台账行");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn 自由文本与标准报文两条通道产出同一套抽取口径() {
    let dir = temp_dir("twins");
    let db = dir.join("urs.db");
    let inbox = dir.join("inbox");

    // 同一句所见文本，一条走自由文本、一条走 HL7 —— 条目字段应完全一致。
    let txt_dir = inbox.join("S-TXT");
    std::fs::create_dir_all(&txt_dir).unwrap();
    std::fs::write(
        txt_dir.join("report.txt"),
        "病历号：PID-EXAMPLE\n超声所见\n左肾积水 3.2cm\n",
    )
    .unwrap();
    deliver(&inbox, "sample-hl7-01", "sample-hl7-01.hl7");

    let repo = Repo::open(&db).unwrap();
    let summary = ingest_inbox(&repo, &inbox).unwrap();
    assert_eq!(summary.reports, 2);
    assert_eq!(summary.items, 2, "两条通道各抽出一条，条目数相同");

    let mut rows: Vec<(String, String, String, String)> = {
        let mut stmt = repo
            .connection()
            .prepare(
                "SELECT part_key, unit_code, coercion_path, CAST(value_si AS TEXT) \
                 FROM items ORDER BY part_key",
            )
            .unwrap();
        let mapped = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect::<Vec<_>>();
        mapped
    };
    assert_eq!(rows.len(), 2);
    // 两行除检查号外完全一致 —— 抽取口径只有一套。
    let first = rows.remove(0);
    let second = rows.remove(0);
    assert_eq!(first, second, "TXT 与 HL7 两条通道的抽取口径必须一致");

    std::fs::remove_dir_all(&dir).ok();
}
