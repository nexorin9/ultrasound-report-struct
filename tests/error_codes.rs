//! 失败分类与**稳定错误码表** —— 集成验收。
//!
//! 三组断言：
//!
//! 1. **每个错误码各有 1 条产生路径** —— 码 → 中文文案 → 处理策略，逐条对得上；
//! 2. **码表穷举且自洽** —— 码字符串互不相同、往返可查、策略口径齐全；
//!    且**码表里没有「登记了却没有产生路径」的码**（登记即须落地）。
//! 3. **不留自由文本错误分支** —— 30 份脱敏语料 + 3 份 HL7 + 编码坏件跑一遍，
//!    每条失败的码都在码表里、每条明细都在明细码表里、每条备注都是登记过的码。
//!
//! 为什么要这样验：失败清单是运维的**唯一入口** —— 认不出的值指不回原文、
//! 失败又不能按码聚合，「抄漏了没人知道」这件事就换个说法继续存在。
//! 码表是这套口径的**唯一真源**，必须可穷举、可回归。

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use ultrasound_report_struct::compare::ris_client::{classify_status, RisClient};
use ultrasound_report_struct::ingest::encoding::{decode_report, EncodingError};
use ultrasound_report_struct::ingest::hl7;
use ultrasound_report_struct::parse::{parse_report, DetailCode, Disposition, ErrorCode, Failure};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// 某段文本解析出的第一条失败（失败清单的第一条）。
fn first_failure(text: &str) -> Failure {
    let out = parse_report(text);
    assert!(
        !out.failures.is_empty(),
        "「{}」应产出失败条目",
        text
    );
    out.failures.into_iter().next().unwrap()
}

/// HL7 缺段样例产出的失败条目。
fn hl7_missing_failure() -> Failure {
    let text = std::fs::read_to_string(manifest_dir().join("assets/samples/hl7/sample-hl7-03.hl7"))
        .expect("读 HL7 样例");
    let report = hl7::adapt(&text);
    assert!(
        !report.outcome.failures.is_empty(),
        "缺 OBX 段的报文应产出失败条目"
    );
    report.outcome.failures.into_iter().next().unwrap()
}

/// 编码类失败：码 + 明细归类（由编码判别层产出，入库层据此落失败条目）。
fn encoding_failure(bytes: &[u8]) -> (ErrorCode, DetailCode, String) {
    match decode_report(bytes) {
        Ok(_) => panic!("这份字节应判不出编码"),
        Err(e) => {
            let code = e.error_code();
            let detail = e.detail_code();
            assert!(
                code.policy().blocks_report,
                "编码类失败应阻断整份报告"
            );
            (code, detail, e.reason().to_string())
        }
    }
}

/// 把一段中文文本编成 GBK 字节（用字符表自带的编码器，不手抄字节）。
fn gbk_bytes(text: &str) -> Vec<u8> {
    let (bytes, _, _) = encoding_rs::GBK.encode(text);
    bytes.into_owned()
}

/// **对拍歧义**的产生路径：同一规范键命中**多条**历史 → `COMPARISON_AMBIGUOUS`。
///
/// 历史用 RIS 会返回的那种 JSON —— 歧义不是构造出来的，
/// 是「同侧同测项有两份历史」这条真实情形跑出来的。
fn ambiguity_failure() -> Failure {
    use ultrasound_report_struct::compare::{judge_item, RisReport};

    let out = parse_report("左肾前后径 3.2cm");
    assert_eq!(out.failures.len(), 0, "这份报告不该有失败条目");
    let item = out.items.into_iter().next().expect("应解析出一条测值");
    let history: Vec<RisReport> = serde_json::from_str(
        r#"[{"study_no":"S-HIST-901","patient_id":"PID-EXAMPLE","body_part":"kidney.left",
             "performed_at":"2026-01-08",
             "measurements":[{"item_code":"kidney.ap_diameter","value":28,"unit":"mm"}]},
            {"study_no":"S-HIST-902","patient_id":"PID-EXAMPLE","body_part":"kidney.left",
             "performed_at":"2026-03-08",
             "measurements":[{"item_code":"kidney.ap_diameter","value":30,"unit":"mm"}]}]"#,
    )
    .expect("历史报文应可解析");
    let verdict = judge_item("PID-EXAMPLE", &item, &history).expect("应能判定");
    verdict.failure.expect("歧义必须产出失败条目")
}

/// RIS 出站读失败的四类产生路径（**语义 → 共用稳定码 → 失败条目**）。
///
/// 出站读的失败**不另造一套码** —— 四类语义映射到全项目共用的那张码表，
/// 运维按错误码聚合时，「RIS 连不上」和「测值缺单位」在同一张表里。
fn ris_failure_cases() -> Vec<(ErrorCode, Failure, Disposition)> {
    // 上游不可达（5xx）/ 鉴权失败（401）/ 响应不合契约（404）。
    let upstream = classify_status(503, "上游忙");
    let auth = classify_status(401, "令牌无效");
    let bad = classify_status(404, "路径不存在");
    // 未配置：端点与凭据都缺（还没发请求就失败）。
    let not_configured = RisClient::new("", "", Duration::from_secs(1)).expect_err("缺端点与凭据应失败");
    vec![
        (
            upstream.error_code(),
            upstream.to_failure(),
            Disposition::AwaitingUpstream,
        ),
        (auth.error_code(), auth.to_failure(), Disposition::AwaitingUpstream),
        (
            bad.error_code(),
            bad.to_failure(),
            Disposition::AwaitingUpstream,
        ),
        (
            not_configured.error_code(),
            not_configured.to_failure(),
            Disposition::AwaitingUpstream,
        ),
    ]
}

/// EMR 出站写失败的四类产生路径（**语义 → 共用稳定码 → 失败条目**）。
///
/// 出站**写**的失败**不另造一套码** —— 四类语义映射到全项目共用的那张码表，
/// 运维按错误码聚合时，「EMR 写不进去」和「测值缺单位」在同一张表里。
///
/// 处置动作与出站**读**不同：出站读失败挂「待上游恢复」（RIS 那一端），
/// 出站写失败挂「待回写重试」（EMR 那一端）—— 挂错了会把人引到另一端白查半天。
fn emr_failure_cases() -> Vec<(ErrorCode, Failure, Disposition)> {
    use ultrasound_report_struct::outbound::emr::{classify_status as emr_classify_status, EmrClient};

    // 上游不可达（5xx）/ 鉴权失败（401）/ 请求被拒（422）。
    let upstream = emr_classify_status(503, "上游忙");
    let auth = emr_classify_status(401, "令牌无效");
    let bad = emr_classify_status(422, "字段不合契约");
    // 未配置：端点与凭据都缺（还没发请求就失败）。
    let not_configured = EmrClient::new("", "", Duration::from_secs(1)).expect_err("缺端点与凭据应失败");
    vec![
        (
            upstream.error_code(),
            upstream.to_failure(),
            Disposition::AwaitingWriteback,
        ),
        (auth.error_code(), auth.to_failure(), Disposition::AwaitingWriteback),
        (
            bad.error_code(),
            bad.to_failure(),
            Disposition::AwaitingWriteback,
        ),
        (
            not_configured.error_code(),
            not_configured.to_failure(),
            Disposition::AwaitingWriteback,
        ),
    ]
}

/// 企微出站推失败的四类产生路径（**语义 → 共用稳定码 → 失败条目**）。
///
/// 出站**推**的失败**不另造一套码** —— 四类语义映射到全项目共用的那张码表。
///
/// 处置动作与出站读写都不同：出站推失败挂「待推送重试」（查 webhook 那一端）——
/// 结论已经算出来了、甚至可能已经写进 EMR，差的只是「通知没发出去」，
/// 挂到「待上游恢复」（查 RIS）或「待回写重试」（查 EMR）都会把人引到错的那一端。
fn wecom_failure_cases() -> Vec<(ErrorCode, Failure, Disposition)> {
    use ultrasound_report_struct::outbound::wecom::{
        classify_errcode as wecom_classify_errcode, classify_status as wecom_classify_status,
        WeComClient,
    };

    // 上游不可达（5xx）/ 鉴权失败（地址或 key 无效）/ 请求被拒（4xx）。
    let upstream = wecom_classify_status(503, "上游忙");
    let auth = wecom_classify_errcode(93000, "invalid webhook url", "{}");
    let bad = wecom_classify_status(404, "路径不存在");
    // 未配置：地址缺失（还没发请求就失败）。
    let not_configured = WeComClient::new("", Duration::from_secs(1)).expect_err("缺地址应失败");
    vec![
        (
            upstream.error_code(),
            upstream.to_failure(),
            Disposition::AwaitingNotification,
        ),
        (
            auth.error_code(),
            auth.to_failure(),
            Disposition::AwaitingNotification,
        ),
        (
            bad.error_code(),
            bad.to_failure(),
            Disposition::AwaitingNotification,
        ),
        (
            not_configured.error_code(),
            not_configured.to_failure(),
            Disposition::AwaitingNotification,
        ),
    ]
}

/// **确认回执端点**失败的五类产生路径（**真实端点路径**）。
///
/// 回执端点的失败**不另造一套码** —— 五类语义映射到全项目共用的那张码表：
/// 「医师的确认没走通」和「测值缺单位」在同一张表里，运维按码聚合时不用记两套规则。
fn review_failure_cases() -> Vec<(ErrorCode, Failure, Disposition)> {
    use ultrasound_report_struct::outbound::review_api::{ReviewHandler, ReviewRequest};
    use ultrasound_report_struct::parse::parse_report;
    use ultrasound_report_struct::state::idempotency_key;
    use ultrasound_report_struct::state::repo::{IngestRecord, Repo};

    // 每次调用用**不同的检查号**（这个辅助函数会被两条用例各调一次，
    // 同一个检查号落两次库会撞幂等键）。
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let study_no = format!("S-ERR-{:03}", seq);

    let dir = std::env::temp_dir().join(format!("urs-error-codes-review-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    let db = dir.join("state.sqlite");
    let token = "token-for-error-codes";
    let handler = ReviewHandler::new(db.clone(), token.to_string(), None, None);
    let path = format!("/api/v1/reports/{}/review", study_no);
    let body = "{\"decision\":\"confirm\",\"reviewer\":\"USR-SONO-EXAMPLE\"}";

    // 1) 鉴权失败：缺头 / 令牌不对。
    let missing = handler.handle(&ReviewRequest::new("POST", &path, None, body));
    let wrong = handler.handle(&ReviewRequest::new("POST", &path, Some("wrong"), body));
    // 2) 请求不合法：决定值不认识 / 请求体不是 JSON。
    let bad_decision = handler.handle(&ReviewRequest::new(
        "POST",
        &path,
        Some(token),
        "{\"decision\":\"maybe\",\"reviewer\":\"USR-SONO-EXAMPLE\"}",
    ));
    let bad_body = handler.handle(&ReviewRequest::new("POST", &path, Some(token), "not-json"));
    // 3) 找不到对象：路径不匹配 / 检查号不存在。
    let bad_route = handler.handle(&ReviewRequest::new(
        "POST",
        &format!("/api/v1/reports/{}", study_no),
        Some(token),
        body,
    ));
    let no_report = handler.handle(&ReviewRequest::new(
        "POST",
        "/api/v1/reports/NO-SUCH-REPORT/review",
        Some(token),
        body,
    ));

    // 4) 状态不可推进：造一份**已是终态**的报告（已回写），再确认。
    let repo = Repo::open(&db).expect("开状态库");
    let text = "左肾前后径 3.2cm";
    let outcome = parse_report(text);
    let result = repo
        .commit_ingestion(&IngestRecord {
            study_no: &study_no,
            patient_id: Some("PID-EXAMPLE"),
            source_path: None,
            idempotency_key: &idempotency_key(&study_no, text),
            dictionary_version: ultrasound_report_struct::dictionary_version(),
            items: &outcome.items,
            failures: &outcome.failures,
        })
        .expect("落库");
    repo.set_report_status(result.report_id, "written_back")
        .expect("推进状态");
    drop(repo);
    let illegal_state = handler.handle(&ReviewRequest::new("POST", &path, Some(token), body));

    // 5) 状态库打不开：让状态库的父目录是一个**文件**（建目录必然失败）。
    let blocker = dir.join("blocker");
    std::fs::write(&blocker, b"not-a-directory").expect("造挡路文件");
    let broken = ReviewHandler::new(
        blocker.join("state.sqlite"),
        token.to_string(),
        None,
        None,
    );
    let store_down = broken.handle(&ReviewRequest::new("POST", &path, Some(token), body));

    let responses = [
        missing,
        wrong,
        bad_decision,
        bad_body,
        bad_route,
        no_report,
        illegal_state,
        store_down,
    ];
    let mut out = Vec::new();
    for response in responses {
        let failure = response.failure().expect("失败应答应带失败条目");
        out.push((failure.code, failure, Disposition::AwaitingReview));
    }
    out
}

#[test]
fn 每个错误码各有产生路径且文案与策略对得上() {
    // 编码类坏件：前半段 GBK、后半段 UTF-8（模板与所见不是同一种编码）。
    let mut mixed = gbk_bytes("左肾积水 3.2cm\n");
    mixed.extend_from_slice("积水明显".as_bytes());
    // 二进制坏件：UTF-8 与 GBK 都解不出。
    let unknown = vec![0x00u8, 0x01, 0x02, 0xFF, 0xFE, 0x00, 0x7F, 0x03];

    let (enc_mixed_code, enc_mixed_detail, _) = encoding_failure(&mixed);
    let (enc_unknown_code, enc_unknown_detail, _) = encoding_failure(&unknown);

    // (错误码, 产生路径上的失败, 期望处置动作)
    let cases: Vec<(ErrorCode, Failure, Disposition)> = vec![
        (
            ErrorCode::PartUnresolved,
            first_failure("胆囊 3.2cm"),
            Disposition::AwaitingDictionary,
        ),
        (
            ErrorCode::UnitMissing,
            first_failure("左肾 3.2"),
            Disposition::AwaitingValue,
        ),
        (
            ErrorCode::ValueUnparseable,
            first_failure("左肾 3.2.5cm"),
            Disposition::AwaitingValue,
        ),
        (
            ErrorCode::ItemDictMissing,
            first_failure("膀胱前后径 2.1cm"),
            Disposition::AwaitingDictionary,
        ),
        (
            ErrorCode::CoercionNoPath,
            first_failure("左肾 3.2xx"),
            Disposition::AwaitingValue,
        ),
        (
            ErrorCode::Hl7SegmentMissing,
            hl7_missing_failure(),
            Disposition::BlockReport,
        ),
        (enc_mixed_code, Failure::new(enc_mixed_code, 0, 0, 0, "混编"), Disposition::BlockReport),
        (enc_unknown_code, Failure::new(enc_unknown_code, 0, 0, 0, "非文本"), Disposition::BlockReport),
        (
            ErrorCode::ComparisonAmbiguous,
            ambiguity_failure(),
            Disposition::AwaitingDisambiguation,
        ),
    ];
    // 出站读失败四类（RIS 客户端的生产路径）。
    let mut cases = cases;
    cases.extend(ris_failure_cases());
    // 出站写失败四类（EMR 客户端的生产路径）。
    cases.extend(emr_failure_cases());
    // 出站推失败四类（企微客户端的生产路径）。
    cases.extend(wecom_failure_cases());
    // 确认回执端点失败五类（回执端点的真实生产路径）。
    cases.extend(review_failure_cases());

    for (expected, failure, disposition) in &cases {
        assert_eq!(failure.code, *expected, "失败码不符");
        assert_eq!(
            failure.code.policy().disposition,
            *disposition,
            "{} 的处置动作不符",
            expected.as_str()
        );
        // 码 → 文案：中文文案非空，且说明与处置口径一致。
        assert!(
            !expected.message().is_empty(),
            "{} 缺中文文案",
            expected.as_str()
        );
        assert!(!expected.disposition().message().is_empty());
        // 码 → 策略：失败条目一律排除在对拍之外（对不上就别对）。
        assert!(
            expected.policy().excluded_from_comparison,
            "{} 应排除在对拍之外",
            expected.as_str()
        );
        // 处置动作 ↔ 报告级阻断：整份退回的码必须阻断整份报告。
        assert_eq!(
            expected.policy().blocks_report,
            *disposition == Disposition::BlockReport,
            "{} 的报告级阻断口径与处置动作不一致",
            expected.as_str()
        );
    }

    // 编码类失败的明细归类也必须是登记过的码。
    assert!(DetailCode::lookup(enc_mixed_detail.as_str()).is_some());
    assert!(DetailCode::lookup(enc_unknown_detail.as_str()).is_some());
}

#[test]
fn 码表穷举且自洽且没有空挂的码() {
    // 码字符串互不相同 + 往返可查 + 文案齐全。
    let mut strings: BTreeSet<&str> = BTreeSet::new();
    for code in ErrorCode::ALL {
        assert!(strings.insert(code.as_str()), "错误码字符串重复：{}", code.as_str());
        assert_eq!(ErrorCode::lookup(code.as_str()), Some(code));
        assert!(!code.message().is_empty());
        assert!(!code.disposition().message().is_empty());
        // 策略四条口径齐全（bool 恒有值，但这里断言「口径有名字」——
        // 处置动作不能是未知值）。
        assert!(!code.policy().disposition.as_str().is_empty());
    }
    assert_eq!(ErrorCode::lookup("NOT_A_CODE"), None);
        assert!("NOT_A_CODE".parse::<ErrorCode>().is_err());

    // 明细归类同样穷举且可反查。
    let mut details: BTreeSet<&str> = BTreeSet::new();
    for detail in DetailCode::ALL {
        assert!(details.insert(detail.as_str()), "明细码重复：{}", detail.as_str());
        assert_eq!(DetailCode::lookup(detail.as_str()), Some(detail));
        assert!(!detail.message().is_empty());
    }
    assert_eq!(DetailCode::lookup("NOT_A_DETAIL"), None);
        assert!("NOT_A_DETAIL".parse::<DetailCode>().is_err());

    // **码表里没有「登记了却没有产生路径」的码**：上一条用例驱动的码集合
    // 必须与码表完全一致（多一个少一个都不行）。
    let mut mixed = gbk_bytes("左肾积水 3.2cm\n");
    mixed.extend_from_slice("积水明显".as_bytes());
    let driven: BTreeSet<ErrorCode> = [
        first_failure("胆囊 3.2cm").code,
        first_failure("左肾 3.2").code,
        first_failure("左肾 3.2.5cm").code,
        first_failure("膀胱前后径 2.1cm").code,
        first_failure("左肾 3.2xx").code,
        hl7_missing_failure().code,
        encoding_failure(&mixed).0,
        encoding_failure(&[0x00u8, 0x01, 0xFF, 0xFE, 0x7F]).0,
        ambiguity_failure().code,
    ]
    .into_iter()
    // 出站读失败四类（RIS 客户端的生产路径）。
    .chain(ris_failure_cases().into_iter().map(|(code, _, _)| code))
    // 出站写失败四类（EMR 客户端的生产路径）。
    .chain(emr_failure_cases().into_iter().map(|(code, _, _)| code))
    // 出站推失败四类（企微客户端的生产路径）。
    .chain(wecom_failure_cases().into_iter().map(|(code, _, _)| code))
    // 确认回执端点失败五类（回执端点的真实生产路径）。
    .chain(review_failure_cases().into_iter().map(|(code, _, _)| code))
    .collect();
    let registered: BTreeSet<ErrorCode> = ErrorCode::ALL.into_iter().collect();
    assert_eq!(
        driven, registered,
        "有码没有产生路径，或有产生路径没登记进码表"
    );
}

#[test]
fn 语料与消息里的失败一律带已登记的码与明细() {
    let mut failures_checked = 0usize;
    let mut codes_seen: BTreeSet<&str> = BTreeSet::new();

    // 1) 30 份脱敏自由文本语料。
    let reports_dir = manifest_dir().join("assets/samples/reports");
    let mut report_files: Vec<PathBuf> = std::fs::read_dir(&reports_dir)
        .expect("读语料目录")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "txt").unwrap_or(false))
        .collect();
    report_files.sort();
    assert!(!report_files.is_empty(), "语料目录是空的");

    for path in &report_files {
        let text = std::fs::read_to_string(path).expect("读语料");
        let out = parse_report(&text);
        for failure in &out.failures {
            failures_checked += 1;
            codes_seen.insert(failure.code.as_str());
            // 码已登记（可反查）。
            assert_eq!(
                ErrorCode::lookup(failure.code.as_str()),
                Some(failure.code),
                "{}：失败码未登记进码表",
                path.display()
            );
            // 明细要么为空，要么是登记过的明细码（**不留自由文本分支**）。
            if !failure.detail.is_empty() {
                assert!(
                    DetailCode::lookup(&failure.detail).is_some(),
                    "{}：明细归类未登记（{}）",
                    path.display(),
                    failure.detail
                );
            }
            // 面向人的说明非空。
            assert!(!failure.message().is_empty());
        }
        // 条目上的备注同样只能是登记过的码。
        for item in &out.items {
            for note in &item.notes {
                assert!(
                    ErrorCode::lookup(note).is_some(),
                    "{}：备注里出现未登记的自由文本（{}）",
                    path.display(),
                    note
                );
            }
        }
    }

    // 2) 3 份脱敏 HL7 报文。
    let hl7_dir = manifest_dir().join("assets/samples/hl7");
    let mut hl7_files: Vec<PathBuf> = std::fs::read_dir(&hl7_dir)
        .expect("读 HL7 样例目录")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "hl7").unwrap_or(false))
        .collect();
    hl7_files.sort();
    assert_eq!(hl7_files.len(), 3, "HL7 样例应为 3 份");

    for path in &hl7_files {
        let text = std::fs::read_to_string(path).expect("读 HL7 样例");
        let report = hl7::adapt(&text);
        for failure in &report.outcome.failures {
            failures_checked += 1;
            codes_seen.insert(failure.code.as_str());
            assert_eq!(ErrorCode::lookup(failure.code.as_str()), Some(failure.code));
            if !failure.detail.is_empty() {
                assert!(
                    DetailCode::lookup(&failure.detail).is_some(),
                    "{}：明细归类未登记（{}）",
                    path.display(),
                    failure.detail
                );
            }
            assert!(!failure.message().is_empty());
        }
    }

    assert!(failures_checked > 0, "没验到任何失败条目");
    // 语料里出现的失败码都在码表内（登记过的码不能凭空多出来）。
    for code in &codes_seen {
        assert!(
            ErrorCode::lookup(code).is_some(),
            "语料里出现了未登记的失败码：{}",
            code
        );
    }
}

#[test]
fn 编码类失败带码带明细且阻断整份报告() {
    // 混编：前半段 GBK、后半段 UTF-8。
    let mut mixed = gbk_bytes("左肾积水 3.2cm\n");
    mixed.extend_from_slice("积水明显".as_bytes());
    let err: EncodingError = match decode_report(&mixed) {
        Ok(_) => panic!("混编件应判不出"),
        Err(e) => e,
    };
    assert_eq!(err.error_code(), ErrorCode::EncodingMixed);
    assert_eq!(err.detail_code(), DetailCode::EncodingMixedBytes);
    assert!(err.error_code().policy().blocks_report);
    assert!(err.reason().contains("字节"), "判定依据应说明字节位置");

    // 非文本：UTF-8 与 GBK 都解不出。
    let err = match decode_report(&[0x00u8, 0x01, 0xFF, 0xFE, 0x7F, 0x00]) {
        Ok(_) => panic!("二进制件应判不出"),
        Err(e) => e,
    };
    assert_eq!(err.error_code(), ErrorCode::EncodingUnknown);
    assert_eq!(err.detail_code(), DetailCode::EncodingUnknownBytes);
    assert!(err.error_code().policy().blocks_report);
}
