//! 对拍第 4 层集成验收：**同侧判定 + 快照一致性 + RIS 不可达降级**。
//!
//! 这一组用例走**真实出站读**（真 HTTP 报文，真 Bearer 头，真超时），
//! 断言三件事：
//!
//! 1. **同侧可比性** —— 左肾对左肾能算出差值；双侧历史对单侧当前**算不出**
//!    （不相减、不给差值），左右不同同理；
//! 2. **RIS 不可达降级** —— 上游 503 与「真连不上」都落
//!    `comparison_skipped`：**不产差值**、带 `RIS_UPSTREAM_UNAVAILABLE`、
//!    **绝不冒充「首次检查」**；
//! 3. **快照一致性** —— 一轮对拍固定一个取数时点；取数期间历史变过要留痕，
//!    但**差值仍按固定快照算**（不因漂移而悄悄改数）。
//!
//! **替身不是最终对接边界**：这里起的是 `127.0.0.1` 上的裸 HTTP 服务，
//! 用来驱动**真实客户端**发出真实请求。院内真实对接边界是
//! `spec.md` §2.3 那一行 —— `GET {RIS_BASE_URL}/api/v1/ultrasound-reports?...`
//! + `Authorization: Bearer $RIS_API_TOKEN` 走 HTTPS 到院内 RIS。
//!
//! 替身**不模拟** TLS、不模拟 RIS 的真实业务数据，真实 RIS 尚未实连
//! （凭据不在本环境），见 `progress.txt`。

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use ultrasound_report_struct::compare::ris_client::{RisClient, RisQuery};
use ultrasound_report_struct::compare::round::ComparisonRound;
use ultrasound_report_struct::compare::snapshot::HistorySnapshot;
use ultrasound_report_struct::compare::{
    compare_sides, judge_item, ComparisonState, Laterality, SkipReason, SideMismatch,
};
use ultrasound_report_struct::parse::errors::ErrorCode;
use ultrasound_report_struct::parse::parse_report;

/// 替身的一次应答。
#[derive(Clone, Debug)]
enum Reply {
    /// 回一个 HTTP 响应。
    Respond { status: u16, body: String },
}

/// 本地 HTTP 替身（**只在测试数据层**）。
struct Stub {
    base_url: String,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Stub {
    fn start(replies: Vec<Reply>) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定替身端口");
        let addr = listener.local_addr().expect("取替身地址");
        listener.set_nonblocking(true).expect("替身转非阻塞");

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);

        let handle = thread::spawn(move || {
            let mut index = 0usize;
            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let reply = replies.get(index).cloned().unwrap_or(Reply::Respond {
                            status: 503,
                            body: "{\"error\":\"替身没有更多应答了\"}".to_string(),
                        });
                        index += 1;
                        let stop = Arc::clone(&thread_stop);
                        thread::spawn(move || serve(stream, reply, stop));
                    }
                    Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Stub {
            base_url: format!("http://{}", addr),
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// 处理一条连接：读完请求行与请求头，再按脚本应答。
fn serve(mut stream: TcpStream, reply: Reply, stop: Arc<AtomicBool>) {
    stream.set_nonblocking(false).ok();
    let mut reader = BufReader::new(stream.try_clone().expect("克隆连接"));
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if line.trim_end().is_empty() {
                    break;
                }
            }
        }
    }
    match reply {
        Reply::Respond { status, body } => {
            let reason = match status {
                200 => "OK",
                503 => "Service Unavailable",
                _ => "Status",
            };
            let response = format!(
                "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                status,
                reason,
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    }
    let _ = stop.load(Ordering::Relaxed);
}

/// 一份历史报告的 JSON（部位写法可换，用来驱动侧别规则）。
fn history_json(body_part: &str, value: f64) -> String {
    format!(
        r#"[{{"study_no":"S-HIST-801","patient_id":"PID-EXAMPLE","body_part":"{}",
             "performed_at":"2026-01-08",
             "measurements":[{{"item_code":"kidney.ap_diameter","value":{},"unit":"mm"}}]}}]"#,
        body_part, value
    )
}

/// 解析「左肾前后径 3.2cm」得到的那条测值（32mm）。
fn current_item() -> ultrasound_report_struct::parse::extract::ParsedItem {
    let out = parse_report("左肾前后径 3.2cm");
    assert!(out.failures.is_empty(), "这份报告不该有失败条目");
    out.items.into_iter().next().expect("应解析出一条测值")
}

/// 取历史（真实 HTTP 出站读）。
fn fetch(base_url: &str, body_part: &str) -> Result<Vec<ultrasound_report_struct::compare::RisReport>, ultrasound_report_struct::compare::RisError> {
    let client = RisClient::new(base_url, "token-from-env", Duration::from_secs(2)).expect("建客户端");
    client.fetch_history(&RisQuery::for_patient("PID-EXAMPLE").with_body_part(body_part))
}

#[test]
fn 同侧历史经真实出站读算出差值() {
    let stub = Stub::start(vec![Reply::Respond {
        status: 200,
        body: history_json("kidney.left", 28.0),
    }]);
    let reports = fetch(&stub.base_url, "kidney.left").expect("出站读应成功");
    assert_eq!(reports.len(), 1);

    let round = ComparisonRound::from_reports(reports);
    let verdict = round
        .compare_item("PID-EXAMPLE", &current_item())
        .expect("应能判定");
    assert_eq!(verdict.state, ComparisonState::HasHistory);
    let delta = verdict.delta.expect("同侧应给出差值");
    assert_eq!(delta.delta, 4.0);
    assert_eq!(delta.direction.as_str(), "增大");
    assert_eq!(delta.unit_code, "mm");
}

#[test]
fn 双侧历史对单侧当前不算差值() {
    // 「双肾 3.2cm」与「左肾 2.8cm」不是同一个测量 —— 不相减。
    let stub = Stub::start(vec![Reply::Respond {
        status: 200,
        body: history_json("kidney.bilateral", 28.0),
    }]);
    let reports = fetch(&stub.base_url, "kidney.bilateral").expect("出站读应成功");
    let round = ComparisonRound::from_reports(reports);
    let verdict = round
        .compare_item("PID-EXAMPLE", &current_item())
        .expect("应能判定");
    assert_eq!(verdict.state, ComparisonState::FirstTime);
    assert!(verdict.delta.is_none());
    // 排除原因落在「双侧↔单侧」这一条上。
    let outcome = ultrasound_report_struct::compare::match_history(
        &ultrasound_report_struct::compare::ComparisonKey::new(
            "PID-EXAMPLE",
            "kidney.left",
            "kidney.ap_diameter",
        ),
        &ultrasound_report_struct::compare::CurrentMeasure {
            value_si: Some(32.0),
            unit_code: Some("mm".to_string()),
        },
        round.snapshot().reports(),
    );
    assert_eq!(
        outcome.skipped_count(SkipReason::BilateralVsUnilateral),
        1
    );
    assert!(outcome.delta.is_none());
}

#[test]
fn ris返回503时落跳过且不产差值() {
    // 上游不可达 → 显式跳过，**不当作无差异**。
    let stub = Stub::start(vec![Reply::Respond {
        status: 503,
        body: "{\"error\":\"上游忙\"}".to_string(),
    }]);
    let err = fetch(&stub.base_url, "kidney.left").expect_err("503 应判失败");
    assert_eq!(err.code(), "RIS_UPSTREAM_UNAVAILABLE");

    let round = ComparisonRound::from_ris_error(&err);
    assert!(round.is_degraded());
    let verdict = round
        .compare_item("PID-EXAMPLE", &current_item())
        .expect("降级也要给出结论");
    assert_eq!(verdict.state, ComparisonState::Skipped);
    assert_eq!(verdict.state.as_str(), "comparison_skipped");
    assert!(verdict.delta.is_none());
    let failure = verdict.failure.as_ref().expect("降级必须带失败条目");
    assert_eq!(failure.code, ErrorCode::RisUpstreamUnavailable);
    // 降级不是「首次检查」。
    assert!(!verdict.is_first_time());
    assert!(verdict.is_consistent());
}

#[test]
fn ris真连不上时同样落跳过() {
    // 指向一个**没有服务**的端口 —— 真实连接失败，不是构造出来的错误。
    let listener = TcpListener::bind("127.0.0.1:0").expect("绑一个空端口");
    let addr = listener.local_addr().expect("取地址");
    drop(listener); // 端口随即关闭 → 连不上。

    let err = fetch(&format!("http://{}", addr), "kidney.left").expect_err("应连不上");
    assert_eq!(err.code(), "RIS_UPSTREAM_UNAVAILABLE");
    assert_eq!(err.kind.as_str(), "upstream_unavailable");

    let round = ComparisonRound::from_ris_error(&err);
    let verdict = round
        .compare_item("PID-EXAMPLE", &current_item())
        .expect("降级也要给出结论");
    assert_eq!(verdict.state, ComparisonState::Skipped);
    assert!(verdict.delta.is_none());
    assert_eq!(
        verdict.failure.as_ref().expect("降级必须带失败条目").code,
        ErrorCode::RisUpstreamUnavailable
    );
}

#[test]
fn 快照在取数期间变了要留痕但不改差值() {
    // 第一轮取到 28mm（差值 +4mm）；随后 RIS 侧那份历史被改成 30mm。
    let stub = Stub::start(vec![
        Reply::Respond {
            status: 200,
            body: history_json("kidney.left", 28.0),
        },
        Reply::Respond {
            status: 200,
            body: history_json("kidney.left", 30.0),
        },
    ]);
    let first = fetch(&stub.base_url, "kidney.left").expect("第一次出站读应成功");
    let mut round = ComparisonRound::from_reports(first);
    let before = round
        .compare_item("PID-EXAMPLE", &current_item())
        .expect("应能判定")
        .delta
        .expect("有历史应给出差值")
        .delta;
    assert_eq!(before, 4.0);

    // 再取一次 —— 历史内容变了。
    let second = fetch(&stub.base_url, "kidney.left").expect("第二次出站读应成功");
    let later = HistorySnapshot::capture(second);
    assert!(round.verify_against(&later), "应检出快照变化");
    let drift = round.drift().expect("漂移应留痕");
    assert_eq!(drift.changed_studies, vec!["S-HIST-801".to_string()]);

    // 差值仍按**固定快照**算 —— 不因漂移而悄悄改数。
    let after = round
        .compare_item("PID-EXAMPLE", &current_item())
        .expect("应能判定")
        .delta
        .expect("仍应给出差值")
        .delta;
    assert_eq!(after, before);
}

#[test]
fn 侧别规则表在公开接口上逐条可断言() {
    assert_eq!(laterality_of_public("kidney.left"), Laterality::Left);
    assert_eq!(laterality_of_public("kidney.bilateral"), Laterality::Bilateral);
    assert_eq!(laterality_of_public("bladder"), Laterality::NotApplicable);
    assert_eq!(
        compare_sides("kidney.bilateral", "kidney.left").mismatch(),
        Some(SideMismatch::BilateralVsUnilateral)
    );
    assert_eq!(
        compare_sides("kidney.left", "kidney.right").mismatch(),
        Some(SideMismatch::DifferentSide)
    );
    assert_eq!(
        compare_sides("kidney.left", "kidney.left").mismatch(),
        None
    );
}

/// 通过公开接口取侧别（避免在测试里直接依赖内部路径）。
fn laterality_of_public(part_key: &str) -> Laterality {
    ultrasound_report_struct::compare::laterality_of(part_key)
}

#[test]
fn 三态判定的老路径不受影响() {
    // 同侧历史 + 当前测值 → 仍是「有历史」；这条路径没被本层改坏。
    let out = parse_report("左肾前后径 3.2cm");
    let item = out.items.into_iter().next().expect("应解析出一条测值");
    let history: Vec<ultrasound_report_struct::compare::RisReport> =
        serde_json::from_str(&history_json("kidney.left", 28.0)).expect("历史报文应可解析");
    let verdict = judge_item("PID-EXAMPLE", &item, &history).expect("应能判定");
    assert_eq!(verdict.state, ComparisonState::HasHistory);
    assert_eq!(verdict.delta.expect("应给出差值").delta, 4.0);
}
