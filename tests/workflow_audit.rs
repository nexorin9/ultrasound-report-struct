//! 工作流第 2 层验收：审计日志逐动作留痕（九类动作）。
//!
//! **验收的是一条完整处理链**：投递 → 解析 → 强转 → 失败留痕 → 对拍 →
//! 医师确认 → 回写 EMR → 企微推送。
//!
//! 三条主张逐条有断言：
//!
//! 1. **九类动作都有明确的写入点** —— 库里九类动作齐全（确认与驳回互斥，
//!    单份报告的链上最多八类，所以「齐全」是**库级**口径）；
//! 2. **每条留痕带摘要哈希与词典版本** —— 半年后能说清「这份数据是哪套口径算出来的」；
//! 3. **留痕顺序可查** —— 自增主键严格递增、挂钟时间非递减。

use ultrasound_report_struct::compare::ris_client::{RisReport, RisReportMeasurement};
use ultrasound_report_struct::compare::round::ComparisonRound;
use ultrasound_report_struct::daemon::processor::{IngestProcessor, ReportProcessor};
use ultrasound_report_struct::ingest::watcher::{Candidate, CandidateKind};
use ultrasound_report_struct::parse::parse_report;
use ultrasound_report_struct::state::repo::Repo;
use ultrasound_report_struct::state::{idempotency_key, idempotency_key_bytes};
use ultrasound_report_struct::workflow::audit::{
    compared_detail, Actor, AuditAction, AuditContext, AuditRecorder, ACTION_COERCED,
    ACTION_DUPLICATE_SKIPPED, ACTION_PARSED, ACTION_RECEIVED,
};

use std::path::PathBuf;

/// 一份脱敏报告（占位检查号 / 占位患者主索引，不含任何真实标识）。
const BODY: &str = "病历号：PID-EXAMPLE\n超声所见：\n左肾前后径 3.2cm\n膀胱残余尿量 约 120ml";

/// 造一个投递候选（守护进程拿到的那种候选）。
fn candidate(study_no: &str, body: &str) -> Candidate {
    Candidate {
        study_no: study_no.to_string(),
        path: PathBuf::from(format!("inbox/{}/report.txt", study_no)),
        kind: CandidateKind::Txt,
        byte_len: body.len() as u64,
        idempotency_key: idempotency_key(study_no, body),
        raw_bytes: body.as_bytes().to_vec(),
    }
}

/// RIS 里该患者同侧部位的真实历史测量值（oracle，测试替身）。
fn history(study_no: &str, value: f64) -> RisReport {
    RisReport {
        study_no: study_no.to_string(),
        patient_id: Some("PID-EXAMPLE".to_string()),
        body_part: Some("kidney.left".to_string()),
        performed_at: Some("2026-01-01".to_string()),
        measurements: vec![RisReportMeasurement {
            item_code: "kidney.ap_diameter".to_string(),
            value: Some(value),
            unit: Some("mm".to_string()),
        }],
    }
}

/// 取一份报告的主键（按检查号查）。
fn find_report_id(repo: &Repo, study_no: &str) -> i64 {
    repo.connection()
        .query_row(
            "SELECT id FROM reports WHERE study_no = ?1",
            [study_no],
            |row| row.get(0),
        )
        .expect("报告行应存在")
}

#[test]
fn 完整处理链留下九类动作且顺序可查() {
    let repo = Repo::open_in_memory().unwrap();
    let recorder = AuditRecorder::new(&repo);

    // --- 1) 投递：走真实入库路径（守护处理器 → 解析 → 强转 → 落库 + 留痕） ---
    let outcome = IngestProcessor
        .process(&repo, &candidate("S-AUDIT-001", BODY))
        .expect("处理应成功");
    assert_eq!(outcome.items, 2, "应抽出 2 条测值");
    assert_eq!(outcome.failures, 0);
    let report_id = find_report_id(&repo, "S-AUDIT-001");

    // 摘要与词典版本：留痕自己算（审计层不依赖调用方传值）。
    let ctx = AuditContext::for_body(
        report_id,
        "S-AUDIT-001",
        BODY.as_bytes(),
        &ultrasound_report_struct::dictionary_version(),
    );

    // --- 2) 对拍：RIS 同侧历史 28mm vs 本次 32mm → 差值 +4mm ---
    let parsed = parse_report(BODY);
    let item = parsed
        .items
        .iter()
        .find(|i| i.item_code.as_deref() == Some("kidney.ap_diameter"))
        .expect("应有肾盂前后径");
    let round = ComparisonRound::from_reports_at(vec![history("S-HIST-001", 28.0)], 1_700_000_000);
    let verdict = round
        .compare_item("PID-EXAMPLE", item)
        .expect("应能判定");
    assert_eq!(verdict.state.as_str(), "has_history");
    let delta = verdict.delta.as_ref().expect("有历史应给出差值");
    assert_eq!(delta.delta, 4.0);
    recorder
        .compared(
            &ctx,
            &Actor::System,
            Some(&compared_detail(
                verdict.state.as_str(),
                Some(delta.delta),
                Some(delta.direction.as_str()),
            )),
        )
        .unwrap();

    // --- 3) 医师确认（人工确认点） ---
    recorder
        .confirmed(
            &ctx,
            &Actor::user("USR-SONO-EXAMPLE"),
            Some("超声医师逐条确认：2 条测值口径一致，可回写"),
        )
        .unwrap();

    // --- 4) 回写 EMR 成功 ---
    recorder
        .written_back(
            &ctx,
            &Actor::System,
            Some("已回写 EMR 结构化报告接口（幂等键同源）"),
        )
        .unwrap();

    // --- 5) 企微推送成功 ---
    recorder
        .pushed(&ctx, &Actor::System, Some("已推送企业微信群机器人"))
        .unwrap();

    // --- 断言 A：链上动作顺序 = 业务顺序 ---
    let chain = recorder.chain(report_id).unwrap();
    let actions: Vec<&str> = chain.iter().map(|r| r.action.as_str()).collect();
    assert_eq!(
        actions,
        vec![
            "received", "parsed", "coerced", "compared", "confirmed", "written_back", "pushed"
        ],
        "审计链的动作顺序必须与业务顺序一致"
    );

    // --- 断言 B：每条留痕都带摘要哈希与词典版本 ---
    for row in &chain {
        assert!(
            row.input_digest.as_deref().is_some_and(|d| d.len() == 64),
            "留痕 {} 缺输入摘要哈希",
            row.action
        );
        assert!(
            row.dictionary_version.as_deref().is_some_and(|v| !v.is_empty()),
            "留痕 {} 缺词典版本",
            row.action
        );
        assert!(!row.created_at.is_empty(), "留痕 {} 缺时间", row.action);
        assert!(row.actor.is_some(), "留痕 {} 缺责任人", row.action);
    }

    // --- 断言 C：顺序可查（自增主键严格递增、挂钟时间非递减） ---
    for pair in chain.windows(2) {
        assert!(
            pair[0].id < pair[1].id,
            "留痕顺序必须严格递增：{} → {}",
            pair[0].id,
            pair[1].id
        );
        assert!(
            pair[0].created_at <= pair[1].created_at,
            "留痕时间序不得倒退：{} → {}",
            pair[0].created_at,
            pair[1].created_at
        );
    }

    // --- 断言 D：人工动作带账号，系统动作带 system ---
    let confirmed = chain.iter().find(|r| r.action == "confirmed").unwrap();
    assert_eq!(confirmed.actor.as_deref(), Some("USR-SONO-EXAMPLE"));
    let received = chain.iter().find(|r| r.action == "received").unwrap();
    assert_eq!(received.actor.as_deref(), Some("system"));

    // --- 断言 E：摘要与幂等键同源（同一个值） ---
    assert_eq!(
        ctx.input_digest.as_deref(),
        Some(idempotency_key("S-AUDIT-001", BODY).as_str()),
        "留痕摘要必须与幂等键同源"
    );
    let row = chain.iter().find(|r| r.action == "received").unwrap();
    assert_eq!(row.input_digest.as_deref(), ctx.input_digest.as_deref());
}

#[test]
fn 失败条目逐条留痕且能反查原文() {
    let repo = Repo::open_in_memory().unwrap();
    let recorder = AuditRecorder::new(&repo);

    // 缺单位的报告：条目不产出，但失败条目必须留在审计表里（失败清单由它导出）。
    let body = "超声所见：\n左肾积水 3.2";
    let outcome = IngestProcessor
        .process(&repo, &candidate("S-AUDIT-002", body))
        .expect("处理应成功");
    assert_eq!(outcome.items, 0, "缺单位不得产出条目");
    assert_eq!(outcome.failures, 1);
    let report_id = find_report_id(&repo, "S-AUDIT-002");

    let chain = recorder.chain(report_id).unwrap();
    let actions: Vec<&str> = chain.iter().map(|r| r.action.as_str()).collect();
    assert_eq!(
        actions,
        vec!["received", "parsed", "coerced", "failed"],
        "失败报告的链：接收 → 解析 → 强转 → 失败"
    );

    let failed = chain.iter().find(|r| r.action == "failed").unwrap();
    assert_eq!(failed.error_code.as_deref(), Some("UNIT_MISSING"));
    assert!(failed.line.unwrap_or(0) >= 1, "失败留痕要带行号");
    assert!(failed.column.unwrap_or(0) >= 1, "失败留痕要带列号");
    assert_eq!(failed.raw_snippet.as_deref(), Some("3.2"));
    // 失败留痕同样带摘要与词典版本。
    assert!(failed.input_digest.is_some());
    assert!(failed.dictionary_version.is_some());
}

#[test]
fn 驳回链与确认链互斥库里九类齐全() {
    let repo = Repo::open_in_memory().unwrap();
    let recorder = AuditRecorder::new(&repo);

    // --- 一份被医师驳回的报告 ---
    let body = "超声所见：\n右肾前后径 2.8cm";
    IngestProcessor
        .process(&repo, &candidate("S-AUDIT-003", body))
        .unwrap();
    let report_id = find_report_id(&repo, "S-AUDIT-003");
    let ctx = AuditContext::for_body(
        report_id,
        "S-AUDIT-003",
        body.as_bytes(),
        &ultrasound_report_struct::dictionary_version(),
    );
    recorder
        .rejected(
            &ctx,
            &Actor::user("USR-SONO-EXAMPLE"),
            Some("超声医师驳回：原文与本次口径对不上，不回写"),
        )
        .unwrap();

    // 驳回的报告：链上有驳回，**没有**确认。
    let actions = recorder.actions_of(report_id).unwrap();
    assert!(actions.contains(&AuditAction::Rejected));
    assert!(!actions.contains(&AuditAction::Confirmed));

    // --- 再走一份确认 + 回写 + 推送的完整链 ---
    let body2 = "超声所见：\n左肾前后径 3.2cm";
    IngestProcessor
        .process(&repo, &candidate("S-AUDIT-004", body2))
        .unwrap();
    let report_id2 = find_report_id(&repo, "S-AUDIT-004");
    let ctx2 = AuditContext::for_body(
        report_id2,
        "S-AUDIT-004",
        body2.as_bytes(),
        &ultrasound_report_struct::dictionary_version(),
    );
    recorder.compared(&ctx2, &Actor::System, Some("对拍完成")).unwrap();
    recorder
        .confirmed(&ctx2, &Actor::user("USR-SONO-EXAMPLE"), Some("确认"))
        .unwrap();
    recorder
        .written_back(&ctx2, &Actor::System, Some("已回写"))
        .unwrap();
    recorder.pushed(&ctx2, &Actor::System, Some("已推送")).unwrap();

    // --- 再走一份带失败条目的报告（失败动作的留痕来源） ---
    let body3 = "超声所见：\n左肾积水 3.2";
    let out3 = IngestProcessor
        .process(&repo, &candidate("S-AUDIT-007", body3))
        .unwrap();
    assert_eq!(out3.failures, 1, "缺单位应落一条失败条目");

    // --- 库里九类动作齐全（库级口径：确认与驳回互斥，单份报告凑不齐九类） ---
    let distinct = recorder.distinct_actions().unwrap();
    for action in AuditAction::ALL {
        assert!(
            distinct.contains(&action),
            "库里缺少动作 {} 的留痕（已出现：{:?}）",
            action.as_str(),
            distinct
        );
    }
    assert_eq!(distinct.len(), 9, "九类动作各出现一次即为齐全");
}

#[test]
fn 审计侧只写不读判重仍走幂等表() {
    let repo = Repo::open_in_memory().unwrap();
    let recorder = AuditRecorder::new(&repo);
    let body = "超声所见：\n左肾前后径 3.2cm";

    IngestProcessor
        .process(&repo, &candidate("S-AUDIT-005", body))
        .unwrap();
    let report_id = find_report_id(&repo, "S-AUDIT-005");
    let chain = recorder.chain(report_id).unwrap();
    let first_digest = chain[0].input_digest.clone().expect("应有摘要");

    // 重复投递：幂等命中，**不产生第二条台账行**。
    let again = IngestProcessor
        .process(&repo, &candidate("S-AUDIT-005", body))
        .unwrap();
    assert!(again.duplicate, "重复投递应被判为幂等命中");

    // 台账行数不变。
    let items: i64 = repo
        .connection()
        .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
        .unwrap();
    assert_eq!(items, 1, "重复投递不得产生第二条台账行");

    // 审计链没有多出「接收 / 解析 / 强转」—— 判重走的是幂等表，
    // 审计表只是被读了一次（读链），**不参与判重**。
    //
    // 但**必须**多出一条 `duplicate_skipped`：重复投递要被跳过，
    // 跳过的原因也得留下来 —— 否则运维看到「收到 30 份、库里只有 28 份」
    // 无从判断少的 2 份是丢了还是被跳过了。
    let after = recorder.chain(report_id).unwrap();
    let count_action = |action: &str| after.iter().filter(|r| r.action == action).count();
    assert_eq!(
        count_action(ACTION_RECEIVED),
        1,
        "重复投递不得再写一条接收留痕"
    );
    assert_eq!(
        count_action(ACTION_PARSED),
        1,
        "重复投递不得再写一条解析留痕"
    );
    assert_eq!(
        count_action(ACTION_COERCED),
        1,
        "重复投递不得再写一条强转留痕"
    );
    assert_eq!(
        count_action(ACTION_DUPLICATE_SKIPPED),
        1,
        "重复投递应留一条 duplicate_skipped"
    );
    assert_eq!(
        after.len(),
        chain.len() + 1,
        "只应多出那一条 duplicate_skipped"
    );
    assert_eq!(after[0].input_digest.as_deref(), Some(first_digest.as_str()));

    // 幂等表里确有登记（判重确实是走它，而不是碰巧）。
    let key = idempotency_key("S-AUDIT-005", body);
    assert!(repo.find_idempotency(&key).unwrap().is_some());
    // 摘要与幂等键同源：值逐字节相同。
    assert_eq!(
        first_digest,
        idempotency_key_bytes("S-AUDIT-005", body.as_bytes())
    );
}

#[test]
fn 留痕内容写进库后可原样读回() {
    let repo = Repo::open_in_memory().unwrap();
    let recorder = AuditRecorder::new(&repo);

    // 先落一份真实报告行（留痕挂外键：audit_log.report_id → reports.id）。
    let body = "超声所见：\n左肾前后径 3.2cm";
    IngestProcessor
        .process(&repo, &candidate("S-AUDIT-006", body))
        .unwrap();
    let report_id = find_report_id(&repo, "S-AUDIT-006");

    let ctx = AuditContext::for_body(
        report_id,
        "S-AUDIT-006",
        body.as_bytes(),
        "parts-0.2.0+items-0.1.0",
    );
    recorder
        .confirmed(
            &ctx,
            &Actor::user("USR-SONO-EXAMPLE"),
            Some("超声医师确认：口径一致"),
        )
        .unwrap();

    let chain = recorder.chain(report_id).unwrap();
    // 入库已经留下接收 / 解析 / 强转三条，这条是第四条（确认）。
    assert_eq!(chain.len(), 4);
    let row = chain
        .iter()
        .find(|r| r.action == "confirmed")
        .expect("应有确认留痕");
    assert_eq!(row.actor.as_deref(), Some("USR-SONO-EXAMPLE"));
    assert_eq!(row.study_no.as_deref(), Some("S-AUDIT-006"));
    assert_eq!(
        row.dictionary_version.as_deref(),
        Some("parts-0.2.0+items-0.1.0")
    );
    assert!(row.input_digest.is_some());
    assert_eq!(row.detail.as_deref(), Some("超声医师确认：口径一致"));
    // 毫秒精度时间戳。
    assert!(
        row.created_at.contains('.') && row.created_at.ends_with(|c: char| c.is_ascii_digit()),
        "留痕时间应为毫秒精度：{}",
        row.created_at
    );
}
