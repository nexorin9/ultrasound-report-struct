//! 工作流第 1 层：状态机与非法迁移守卫 —— 集成用例。
//!
//! **这一组用例守什么**：
//!
//! 1. **状态机与真实状态库不脱节**：入库路径真正写进 `reports.status` 的值，
//!    必须在状态机的取值集合里；库里出现表里没有的状态 = 数据不对，
//!    状态机守卫就守不住它（这是最现实的一类漂移）。
//! 2. **守卫在真实库上生效**：一份已解析的报告，**跳过医师确认**推到
//!    `written_back` 必须被拒 —— 不是只在内存里的枚举之间比一比。
//! 3. **终态在真实库上同样不可再推进**。

use ultrasound_report_struct::coerce::path::CoercionPath;
use ultrasound_report_struct::parse::{ErrorCode, Failure, ParsedItem};
use ultrasound_report_struct::state::{IngestRecord, NewReport, Repo};
use ultrasound_report_struct::workflow::{transition, Status};

/// 一条能落台账的条目（脱敏）。
fn item() -> ParsedItem {
    ParsedItem {
        part_key: Some("kidney.left".to_string()),
        item_code: Some("kidney.ap_diameter".to_string()),
        value_raw: Some(3.2),
        unit_raw: Some("cm".to_string()),
        value_si: Some(32.0),
        unit_code: Some("mm".to_string()),
        coercion_path: CoercionPath::UnitConvert,
        line: 2,
        column: 6,
        char_start: 12,
        notes: Vec::new(),
        dictionary_version: ultrasound_report_struct::dictionary_version(),
    }
}

/// 一条失败条目（带错误码与行列）。
fn failure() -> Failure {
    Failure::new(ErrorCode::UnitMissing, 12, 2, 6, "3.2")
}

#[test]
fn 入库路径落的状态都在状态机取值集合里() {
    let repo = Repo::open_in_memory().expect("内存库应可打开");

    // 有条目 → parsed。
    let items = vec![item()];
    let rec = IngestRecord {
        study_no: "S-STATE-001",
        patient_id: Some("PID-EXAMPLE"),
        source_path: Some("inbox/S-STATE-001/report.txt"),
        idempotency_key: "key-state-001",
        dictionary_version: ultrasound_report_struct::dictionary_version(),
        items: &items,
        failures: &[],
    };
    let result = repo.commit_ingestion(&rec).expect("入库应成功");
    assert_eq!(result.status, "parsed");
    // 库里那一行的状态，状态机认得。
    let row = repo
        .find_by_idempotency_key("key-state-001")
        .expect("查幂等键应成功")
        .expect("报告行应存在");
    assert_eq!(Status::lookup(&row.status), Some(Status::Parsed));

    // 只有失败条目 → parse_failed。
    let failures = vec![failure()];
    let rec = IngestRecord {
        study_no: "S-STATE-002",
        patient_id: Some("PID-EXAMPLE"),
        source_path: Some("inbox/S-STATE-002/report.txt"),
        idempotency_key: "key-state-002",
        dictionary_version: ultrasound_report_struct::dictionary_version(),
        items: &[],
        failures: &failures,
    };
    let result = repo.commit_ingestion(&rec).expect("入库应成功");
    assert_eq!(result.status, "parse_failed");
    assert_eq!(
        Status::lookup(&result.status),
        Some(Status::ParseFailed)
    );
}

#[test]
fn 跳过确认在真实库上被拒() {
    let repo = Repo::open_in_memory().expect("内存库应可打开");
    let items = vec![item()];
    let rec = IngestRecord {
        study_no: "S-STATE-003",
        patient_id: Some("PID-EXAMPLE"),
        source_path: Some("inbox/S-STATE-003/report.txt"),
        idempotency_key: "key-state-003",
        dictionary_version: ultrasound_report_struct::dictionary_version(),
        items: &items,
        failures: &[],
    };
    let result = repo.commit_ingestion(&rec).expect("入库应成功");
    let current = Status::lookup(&result.status).expect("落库状态必须在状态机取值集合里");

    // 合法下一步：解析完 → 待医师确认。
    let next = transition(current, Status::AwaitingReview).expect("待确认是合法去向");
    assert_eq!(next, Status::AwaitingReview);

    // 非法：跳过医师确认直接回写 —— 被拒。
    let err = transition(current, Status::WrittenBack).expect_err("跳过确认必须被拒");
    assert_eq!(err.code(), "ILLEGAL_TRANSITION");
    assert!(err.to_string().contains("awaiting_review"));

    // 合法收尾：待确认 → 已回写 / 驳回。
    assert!(transition(next, Status::WrittenBack).is_ok());
    assert!(transition(next, Status::Rejected).is_ok());
    // 终态不可再推进。
    let written = transition(next, Status::WrittenBack).expect("合法");
    let err = transition(written, Status::AwaitingReview).expect_err("终态不可再推进");
    assert_eq!(err.code(), "TERMINAL_STATE");
}

#[test]
fn 全状态都能落库且原样读回() {
    let repo = Repo::open_in_memory().expect("内存库应可打开");
    for (idx, status) in Status::ALL.iter().enumerate() {
        let key = format!("key-state-all-{}", idx);
        let report = NewReport {
            study_no: format!("S-STATE-ALL-{}", idx),
            patient_id: Some("PID-EXAMPLE".to_string()),
            status: status.as_str().to_string(),
            source_path: None,
            dictionary_version: ultrasound_report_struct::dictionary_version(),
            idempotency_key: key.clone(),
        };
        repo.insert_report(&report).expect("写报告行应成功");
        let row = repo
            .find_by_idempotency_key(&key)
            .expect("查幂等键应成功")
            .expect("报告行应存在");
        // 状态字符串落库原样读回 —— 状态机与状态库用的是同一套取值。
        assert_eq!(row.status, status.as_str());
        assert_eq!(Status::lookup(&row.status), Some(*status));
    }
}
