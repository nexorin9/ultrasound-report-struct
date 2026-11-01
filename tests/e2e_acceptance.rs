//! Task 43 的**端到端验收**：投递 → 台账 → 对拍 → 确认 → 回写 → 推送。
//!
//! ## 这一份测试在证明什么
//!
//! 不是「每个模块各自的单测都绿」，而是**一条链走通**：
//!
//! ```text
//! 超声工作站投递报告
//!   → 解析（中文报告 → 结构化条目 + 失败条目带行列）
//!   → 强转（3.2cm → 32mm，路径 UnitConvert）
//!   → 台账（items 表 + audit_log 留痕）
//!   → 对拍（RIS 真实出站读 → 规范键匹配 → 差值 4mm，方向「增大」）
//!   → 医师确认（POST /api/v1/reports/{study_no}/review，Bearer 令牌）
//!   → EMR 回写（POST {EMR_BASE_URL}/api/v1/structured-reports + Idempotency-Key）
//!   → 企微推送（POST {WECOM_WEBHOOK_URL}）
//!   → 状态 written_back（审计链逐动作留痕）
//! ```
//!
//! 四组断言：
//!
//! 1. **逐段断言** —— 台账行内容与强转路径、差值条目、失败条目带行列、
//!    状态迁移序列、EMR 请求体与 `Idempotency-Key`、企微消息体；
//! 2. **幂等** —— 同一批报告重放一次，台账行数与 EMR 写请求数均不变；
//! 3. **失败链路一** —— RIS 不可达：状态 `comparison_skipped`、**无 delta**、
//!    带上游错误码（绝不冒充「跟上次一样」）；
//! 4. **失败链路二** —— EMR 写失败：转 `pending_writeback`、台账行保留、
//!    企微告警发出（**失败不静默改数据**）。
//!
//! **替身不是最终对接边界**：这里起的是 `127.0.0.1` 上的裸 HTTP 服务，
//! 用来断言**客户端真的按契约发了什么**。院内真实对接边界是 `spec.md` §2.3
//! 那三行（RIS 读、EMR 写、企微推），凭据只从环境变量读；
//! 真实 RIS / EMR / 企微尚未实连（凭据不在本环境），见 `progress.txt`。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use ultrasound_report_struct::compare::pipeline::compare_report;
use ultrasound_report_struct::compare::ris_client::RisClient;
use ultrasound_report_struct::ingest::ingest_inbox;
use ultrasound_report_struct::outbound::emr::EmrClient;
use ultrasound_report_struct::outbound::review_api::{ReviewHandler, ReviewRequest};
use ultrasound_report_struct::outbound::wecom::WeComClient;
use ultrasound_report_struct::state::idempotency_key;
use ultrasound_report_struct::state::repo::Repo;
use ultrasound_report_struct::workflow::audit::AuditRecorder;

/// 回执端点令牌（测试用；真实环境从 `DAEMON_API_TOKEN` 读）。
const TOKEN: &str = "e2e-review-token";
/// EMR 凭据（测试替身用）。
const EMR_TOKEN: &str = "e2e-emr-token";
/// RIS 凭据（测试替身用）。
const RIS_TOKEN: &str = "e2e-ris-token";
/// 确认人账号。
const REVIEWER: &str = "USR-SONO-EXAMPLE";
/// 患者主索引（脱敏占位）。
const PATIENT: &str = "PID-EXAMPLE";

/// 一份**能读出测值**的脱敏报告。
///
/// 「左肾前后径」在测项词典里能查到（`kidney.ap_diameter`）——
/// 有测项码才有规范键，才有资格跟 RIS 历史值对拍。
const REPORT_OK: &str = "病历号：PID-EXAMPLE\n超声所见\n左肾前后径 3.2cm\n提示：左肾积水，建议结合临床复查。";

/// 一份**缺单位**的脱敏报告（读不出数值 → 失败条目带行列）。
const REPORT_NO_UNIT: &str = "病历号：PID-EXAMPLE\n超声所见\n左肾前后径 3.2";

/// 台账行读出的元组（部位键 / 测项码 / 归一值 / 单位码 / 强转路径 / 行 / 列）。
type LedgerRow = (
    Option<String>,
    Option<String>,
    Option<f64>,
    Option<String>,
    String,
    i64,
    i64,
);

/// 差值行读出的元组（历史值 / 本次值 / 差值 / 方向 / 状态）。
type DeltaRow = (
    Option<f64>,
    Option<f64>,
    Option<f64>,
    Option<String>,
    String,
);

/// 报告正文里**不许**出现在外发报文与推送里的叙述片段。
const RAW_FRAGMENTS: [&str; 4] = ["超声所见", "左肾积水", "建议结合临床复查", "提示"];

/// 替身收到的一次请求（**含请求体**）。
#[derive(Clone, Debug)]
struct Recorded {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).expect("请求体应是合法 JSON")
    }
}

/// 替身行为（RIS / EMR / 企微三合一）。
#[derive(Clone, Debug)]
struct StubConfig {
    /// RIS 那一端的应答状态码（200 = 取到历史；503 = 上游不可达）。
    ris_status: u16,
    /// RIS 应答体。
    ris_body: String,
    /// EMR 那一端的应答状态码（200 = 写成功；500 = 写失败）。
    emr_status: u16,
}

impl Default for StubConfig {
    fn default() -> Self {
        StubConfig {
            ris_status: 200,
            ris_body: ris_history_body("S-HIST-202", 28.0),
            emr_status: 200,
        }
    }
}

/// RIS 历史报文（**该患者同侧部位的一条真实历史值**）。
fn ris_history_body(study_no: &str, value: f64) -> String {
    serde_json::json!([{
        "study_no": study_no,
        "patient_id": PATIENT,
        "body_part": "kidney.left",
        "performed_at": "2026-01-08 09:12:00",
        "measurements": [{
            "item_code": "kidney.ap_diameter",
            "value": value,
            "unit": "mm"
        }]
    }])
    .to_string()
}

/// 本地 HTTP 替身（**只在测试数据层**）：同时扮演 RIS、EMR 与企业微信群机器人。
struct Stub {
    base_url: String,
    recorded: Arc<Mutex<Vec<Recorded>>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Stub {
    fn start(config: StubConfig) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定替身端口");
        let addr = listener.local_addr().expect("取替身地址");
        listener.set_nonblocking(true).expect("替身转非阻塞");

        let recorded = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_recorded = Arc::clone(&recorded);
        let thread_stop = Arc::clone(&stop);

        let handle = thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let rec = Arc::clone(&thread_recorded);
                        let cfg = config.clone();
                        thread::spawn(move || serve(stream, cfg, rec));
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
            recorded,
            stop,
            handle: Some(handle),
        }
    }

    fn requests(&self) -> Vec<Recorded> {
        self.recorded.lock().expect("取替身记录").clone()
    }

    /// RIS 历史取数请求。
    fn ris_reads(&self) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(|r| r.target.starts_with("/api/v1/ultrasound-reports"))
            .collect()
    }

    /// EMR 写请求。
    fn emr_writes(&self) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(|r| r.target.starts_with("/api/v1/structured-reports"))
            .collect()
    }

    /// 企微推送。
    fn wecom_pushes(&self) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(|r| r.target.contains("/cgi-bin/webhook/send"))
            .collect()
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

/// 处理一条连接：读完请求行、请求头与请求体，记下来，再按路径应答。
fn serve(mut stream: TcpStream, config: StubConfig, recorded: Arc<Mutex<Vec<Recorded>>>) {
    stream.set_nonblocking(false).ok();
    let mut reader = BufReader::new(stream.try_clone().expect("克隆连接"));
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    let mut headers = Vec::new();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let text = line.trim_end();
                if text.is_empty() {
                    break;
                }
                if let Some((key, value)) = text.split_once(':') {
                    let key = key.trim().to_string();
                    let value = value.trim().to_string();
                    if key.eq_ignore_ascii_case("content-length") {
                        content_length = value.parse().unwrap_or(0);
                    }
                    headers.push((key, value));
                }
            }
        }
    }
    let mut body_bytes = vec![0u8; content_length];
    if content_length > 0 {
        let mut read = 0usize;
        while read < content_length {
            match reader.read(&mut body_bytes[read..]) {
                Ok(0) => break,
                Ok(n) => read += n,
                Err(_) => break,
            }
        }
    }

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    recorded.lock().expect("记替身请求").push(Recorded {
        method,
        target: target.clone(),
        headers,
        body: String::from_utf8_lossy(&body_bytes).to_string(),
    });

    let (status, body) = if target.starts_with("/api/v1/ultrasound-reports") {
        (
            config.ris_status,
            if config.ris_status == 200 {
                config.ris_body.clone()
            } else {
                "{\"error\":\"RIS 上游不可用\"}".to_string()
            },
        )
    } else if target.starts_with("/api/v1/structured-reports") {
        (
            config.emr_status,
            if config.emr_status == 200 {
                "{\"id\":\"SR-EXAMPLE\",\"status\":\"accepted\"}".to_string()
            } else {
                "{\"error\":\"EMR 侧出错\"}".to_string()
            },
        )
    } else if target.contains("/cgi-bin/webhook/send") {
        (200, "{\"errcode\":0,\"errmsg\":\"ok\"}".to_string())
    } else {
        (404, "{\"error\":\"unknown path\"}".to_string())
    };
    let reason = match status {
        200 => "OK",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status,
        reason,
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

/// 测试用临时目录。
fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("urs-e2e-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 造一个投递目录：`<inbox>/<检查号>/report.txt`。
fn write_delivery(inbox: &Path, study_no: &str, body: &str) {
    let dir = inbox.join(study_no);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("report.txt"), body).unwrap();
}

/// 台账行数。
fn ledger_rows(repo: &Repo) -> i64 {
    repo.connection()
        .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
        .unwrap()
}

/// 差值行数。
fn comparison_rows(repo: &Repo) -> i64 {
    repo.connection()
        .query_row("SELECT COUNT(*) FROM comparisons", [], |row| row.get(0))
        .unwrap()
}

/// 报告状态。
fn report_status(repo: &Repo, study_no: &str) -> String {
    repo.find_by_study_no(study_no)
        .unwrap()
        .expect("报告应在库里")
        .status
}

/// 回执端点（指向替身）。
fn review_handler(db: &Path, stub: &Stub) -> ReviewHandler {
    let emr = EmrClient::new(&stub.base_url, EMR_TOKEN, Duration::from_secs(3)).unwrap();
    let wecom = WeComClient::new(
        &format!("{}/cgi-bin/webhook/send?key=DEMO-KEY-NOT-REAL", stub.base_url),
        Duration::from_secs(3),
    )
    .unwrap();
    ReviewHandler::new(db.to_path_buf(), TOKEN.to_string(), Some(emr), Some(wecom))
}

/// 确认回执体。
fn confirm_body() -> String {
    serde_json::json!({
        "decision": "confirm",
        "reviewer": REVIEWER,
        "note": "核对无误"
    })
    .to_string()
}

#[test]
fn 端到端链路走通且逐段可核对() {
    let dir = temp_dir("chain");
    let inbox = dir.join("inbox");
    std::fs::create_dir_all(&inbox).unwrap();
    write_delivery(&inbox, "S-E2E-001", REPORT_OK);
    write_delivery(&inbox, "S-E2E-002", REPORT_NO_UNIT);

    let db = dir.join("state.sqlite");
    let repo = Repo::open(&db).unwrap();

    // ── 第 1 段：投递 → 解析 → 强转 → 台账 ──────────────────────────────
    let summary = ingest_inbox(&repo, &inbox).unwrap();
    assert_eq!(summary.reports, 2, "两份报告都应被受理");
    assert_eq!(summary.items, 1, "只有一份读得出测值");
    assert_eq!(summary.failures, 1, "缺单位那一份应落 1 条失败条目");

    // 台账行内容与强转路径：**原值原单位留着，归一值给院内口径**。
    let row: LedgerRow = repo
        .connection()
        .query_row(
            "SELECT part_key, item_code, value_si, unit_code, coercion_path, line, column \
             FROM items ORDER BY id LIMIT 1",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(row.0.as_deref(), Some("kidney.left"), "部位键要归一");
    assert_eq!(row.1.as_deref(), Some("kidney.ap_diameter"), "测项码要归一");
    assert_eq!(row.2, Some(32.0), "3.2cm 归一后应是 32mm");
    assert_eq!(row.3.as_deref(), Some("mm"));
    assert_eq!(row.4, "UnitConvert", "强转路径要留痕");
    assert_eq!(row.5, 3, "行号要指回原文");
    assert_eq!(row.6, 7, "列号要指回原文");

    // 原值原单位也留着 —— 医师核对时要能看见「报告上写的是什么」。
    let raw: (Option<f64>, Option<String>) = repo
        .connection()
        .query_row("SELECT value_raw, unit_raw FROM items LIMIT 1", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert_eq!(raw, (Some(3.2), Some("cm".to_string())));

    // 失败条目**带行列** —— 认不出的值不猜，直接指回原文第几行第几列。
    let failure: (String, i64, i64, String) = repo
        .connection()
        .query_row(
            "SELECT error_code, line, column, raw_snippet FROM audit_log \
             WHERE action = 'failed' ORDER BY id LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(failure.0, "UNIT_MISSING");
    assert_eq!(failure.1, 3, "失败条目要指回原文行号");
    assert_eq!(failure.2, 7, "失败条目要指回原文列号");
    assert_eq!(failure.3, "3.2", "失败条目要带原文片段");

    // ── 第 2 段：对拍（RIS 真实出站读 → 差值） ──────────────────────────
    let stub = Stub::start(StubConfig::default());
    let ris = RisClient::new(&stub.base_url, RIS_TOKEN, Duration::from_secs(2)).unwrap();

    let outcome = compare_report(&repo, "S-E2E-001", Some(&ris)).unwrap();
    assert_eq!(outcome.compared, 1, "一条测值进对拍");
    assert_eq!(outcome.deltas, 1, "一条测值对上历史值");
    assert!(!outcome.degraded, "RIS 够得着就不该降级");
    assert_eq!(outcome.state, "has_history");
    assert_eq!(outcome.report_status, "awaiting_review", "对完拍进人工确认队列");

    // RIS 那一端真的收到了取数请求（真实出站读，不是本地伪造）。
    let reads = stub.ris_reads();
    assert_eq!(reads.len(), 1, "应向 RIS 取过一次历史");
    assert_eq!(reads[0].method, "GET");
    assert!(reads[0].target.contains("patient_id=PID-EXAMPLE"));
    assert_eq!(
        reads[0].header("Authorization"),
        Some(format!("Bearer {}", RIS_TOKEN).as_str())
    );

    // 差值条目：32mm vs 28mm → 差 4mm，方向「增大」。
    let delta: DeltaRow = repo
        .connection()
        .query_row(
            "SELECT previous_value, current_value, delta, direction, state \
             FROM comparisons ORDER BY id LIMIT 1",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(delta.0, Some(28.0), "历史值取自 RIS");
    assert_eq!(delta.1, Some(32.0), "本次值取自台账");
    assert_eq!(delta.2, Some(4.0), "差值 = 32 − 28");
    assert_eq!(delta.3.as_deref(), Some("增大"));
    assert_eq!(delta.4, "has_history");

    // ── 第 3 段：医师确认 → EMR 回写 → 企微推送 ─────────────────────────
    let handler = review_handler(&db, &stub);
    let response = handler.handle(&ReviewRequest::new(
        "POST",
        "/api/v1/reports/S-E2E-001/review",
        Some(TOKEN),
        &confirm_body(),
    ));
    assert_eq!(response.status, 200, "响应体：{}", response.body);
    let value = response.value().expect("响应体应是 JSON");
    assert_eq!(value["status"], "written_back");
    assert_eq!(value["emr_written"], true);
    assert_eq!(value["item_count"], 1);
    assert_eq!(value["comparison_count"], 1);
    assert_eq!(value["reviewer"], REVIEWER);

    // EMR 写请求：逐字段对得上契约。
    let writes = stub.emr_writes();
    assert_eq!(writes.len(), 1, "确认后应恰好发一次 EMR 写请求");
    assert_eq!(writes[0].method, "POST");
    assert_eq!(writes[0].target, "/api/v1/structured-reports");
    assert_eq!(
        writes[0].header("Authorization"),
        Some(format!("Bearer {}", EMR_TOKEN).as_str())
    );
    // `Idempotency-Key` = sha256(study_no‖报告内容) —— 与入库幂等键同一把钥匙。
    assert_eq!(
        writes[0].header("Idempotency-Key"),
        Some(idempotency_key("S-E2E-001", REPORT_OK).as_str())
    );
    let payload = writes[0].json();
    assert_eq!(payload["study_no"], "S-E2E-001");
    assert_eq!(payload["patient_id"], PATIENT);
    assert_eq!(payload["items"][0]["part_key"], "kidney.left");
    assert_eq!(payload["items"][0]["item_code"], "kidney.ap_diameter");
    assert_eq!(payload["items"][0]["value"], 32.0);
    assert_eq!(payload["items"][0]["unit_code"], "mm");
    assert_eq!(payload["items"][0]["coercion_path"], "UnitConvert");
    assert_eq!(payload["items"][0]["line"], 3);
    assert_eq!(payload["items"][0]["column"], 7);
    assert_eq!(payload["comparisons"][0]["previous_value"], 28.0);
    assert_eq!(payload["comparisons"][0]["delta"], 4.0);
    assert!(
        payload["dictionary_version"].as_str().is_some_and(|v| !v.is_empty()),
        "报文要带词典版本（口径留痕）"
    );

    // **只发结构化结论**：报告里的叙述片段一段都不许出现在报文里。
    let body = writes[0].body.clone();
    for fragment in RAW_FRAGMENTS {
        assert!(
            !body.contains(fragment),
            "外发报文里混进了原文叙述「{}」：{}",
            fragment,
            body
        );
    }

    // 企微推送：一条，内容由结构化字段装配。
    let pushes = stub.wecom_pushes();
    assert_eq!(pushes.len(), 1, "确认回写后应推送一次");
    let message = pushes[0].json();
    assert_eq!(message["msgtype"], "text");
    let content = message["text"]["content"].as_str().unwrap();
    assert!(content.contains("S-E2E-001"), "推送正文要说得清是哪份报告");
    for fragment in RAW_FRAGMENTS {
        assert!(
            !content.contains(fragment),
            "推送正文混进了原文叙述「{}」：{}",
            fragment,
            content
        );
    }

    // 状态：终态 `written_back`。
    assert_eq!(report_status(&repo, "S-E2E-001"), "written_back");

    // ── 第 4 段：状态迁移序列（审计链逐动作留痕） ────────────────────────
    let report = repo.find_by_study_no("S-E2E-001").unwrap().unwrap();
    let chain = AuditRecorder::new(&repo).chain(report.id).unwrap();
    let actions: Vec<&str> = chain.iter().map(|row| row.action.as_str()).collect();
    assert_eq!(
        actions,
        vec![
            "received",
            "parsed",
            "coerced",
            "compared",
            "confirmed",
            "written_back",
            "pushed"
        ],
        "审计链要逐段留痕"
    );
    // 确认是**人工动作**（留痕带人），其余是系统动作。
    let confirmed = chain
        .iter()
        .find(|row| row.action == "confirmed")
        .expect("应有确认留痕");
    assert_eq!(confirmed.actor.as_deref(), Some(REVIEWER));
    assert!(
        confirmed.input_digest.is_some(),
        "每条留痕都要带输入摘要哈希"
    );
    // 留痕按时间序递增。
    let ids: Vec<i64> = chain.iter().map(|row| row.id).collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(ids, sorted, "审计链要按时间序排列");
}

#[test]
fn 重放同一批报告不产生第二条台账行也不产生第二次回写() {
    let dir = temp_dir("replay");
    let inbox = dir.join("inbox");
    std::fs::create_dir_all(&inbox).unwrap();
    write_delivery(&inbox, "S-E2E-R01", REPORT_OK);

    let db = dir.join("state.sqlite");
    let repo = Repo::open(&db).unwrap();
    let stub = Stub::start(StubConfig::default());
    let ris = RisClient::new(&stub.base_url, RIS_TOKEN, Duration::from_secs(2)).unwrap();

    // 首次：受理 → 对拍 → 确认回写。
    let first = ingest_inbox(&repo, &inbox).unwrap();
    assert_eq!(first.reports, 1);
    assert_eq!(first.duplicates, 0);
    compare_report(&repo, "S-E2E-R01", Some(&ris)).unwrap();
    let handler = review_handler(&db, &stub);
    let response = handler.handle(&ReviewRequest::new(
        "POST",
        "/api/v1/reports/S-E2E-R01/review",
        Some(TOKEN),
        &confirm_body(),
    ));
    assert_eq!(response.status, 200, "响应体：{}", response.body);

    let ledger_after_first = ledger_rows(&repo);
    let comparisons_after_first = comparison_rows(&repo);
    let writes_after_first = stub.emr_writes().len();
    assert_eq!(ledger_after_first, 1);
    assert_eq!(comparisons_after_first, 1);
    assert_eq!(writes_after_first, 1);

    // 重放：同一批报告再投一次 —— **台账行数不变、EMR 写请求数不变**。
    let second = ingest_inbox(&repo, &inbox).unwrap();
    assert_eq!(second.reports, 0, "重放不该新增受理");
    assert_eq!(second.duplicates, 1, "重放应被幂等键命中");
    assert_eq!(ledger_rows(&repo), ledger_after_first, "不得产生第二条台账行");
    assert_eq!(
        comparison_rows(&repo),
        comparisons_after_first,
        "不得产生第二条差值行"
    );
    assert_eq!(
        stub.emr_writes().len(),
        writes_after_first,
        "不得产生第二次回写"
    );

    // 再确认一次：报告已是终态 → 显式拒绝，**一条写请求都不多发**。
    let again = handler.handle(&ReviewRequest::new(
        "POST",
        "/api/v1/reports/S-E2E-R01/review",
        Some(TOKEN),
        &confirm_body(),
    ));
    assert_eq!(again.status, 409, "响应体：{}", again.body);
    assert_eq!(stub.emr_writes().len(), writes_after_first);
    assert_eq!(stub.wecom_pushes().len(), 1, "不得产生第二次推送");
}

#[test]
fn ris_不可达时落_comparison_skipped_且不产差值() {
    let dir = temp_dir("risdown");
    let inbox = dir.join("inbox");
    std::fs::create_dir_all(&inbox).unwrap();
    write_delivery(&inbox, "S-E2E-RIS", REPORT_OK);

    let db = dir.join("state.sqlite");
    let repo = Repo::open(&db).unwrap();
    // RIS 那一端 503 —— 上游不可达。
    let stub = Stub::start(StubConfig {
        ris_status: 503,
        ris_body: String::new(),
        emr_status: 200,
    });
    let ris = RisClient::new(&stub.base_url, RIS_TOKEN, Duration::from_secs(2)).unwrap();

    ingest_inbox(&repo, &inbox).unwrap();
    let outcome = compare_report(&repo, "S-E2E-RIS", Some(&ris)).unwrap();

    assert!(outcome.degraded, "RIS 不可达应降级");
    assert_eq!(outcome.state, "comparison_skipped");
    assert_eq!(outcome.deltas, 0, "没比成就不该有差值");
    assert_eq!(outcome.skipped, 1, "一条测值没比成");
    assert_eq!(
        outcome.error_code.as_deref(),
        Some("RIS_UPSTREAM_UNAVAILABLE"),
        "降级要带上游错误码"
    );

    // 差值行在，但**没有 delta** —— 绝不冒充「跟上次一样」。
    let row: (Option<f64>, Option<f64>, String) = repo
        .connection()
        .query_row(
            "SELECT previous_value, delta, state FROM comparisons ORDER BY id LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(row.0, None, "没比成不得有历史值");
    assert_eq!(row.1, None, "没比成不得有差值");
    assert_eq!(row.2, "comparison_skipped");

    // 失败留痕带稳定错误码（运维一眼分得清「该查 RIS」还是「该补词典」）。
    let failure_code: String = repo
        .connection()
        .query_row(
            "SELECT error_code FROM audit_log WHERE action = 'failed' \
             AND error_code LIKE 'RIS_%' ORDER BY id LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(failure_code, "RIS_UPSTREAM_UNAVAILABLE");

    // **台账行不丢**：RIS 够不着只是对不上历史，测值照样落、照样等医师确认。
    assert_eq!(ledger_rows(&repo), 1, "RIS 不可达不该让台账行丢");
    assert_eq!(report_status(&repo, "S-E2E-RIS"), "awaiting_review");
}

#[test]
fn emr_写失败时转_pending_writeback_并告警_台账行保留() {
    let dir = temp_dir("emrfail");
    let inbox = dir.join("inbox");
    std::fs::create_dir_all(&inbox).unwrap();
    write_delivery(&inbox, "S-E2E-EMR", REPORT_OK);

    let db = dir.join("state.sqlite");
    let repo = Repo::open(&db).unwrap();
    // EMR 那一端 500 —— 写不进去。
    let stub = Stub::start(StubConfig {
        ris_status: 200,
        ris_body: ris_history_body("S-HIST-202", 28.0),
        emr_status: 500,
    });
    let ris = RisClient::new(&stub.base_url, RIS_TOKEN, Duration::from_secs(2)).unwrap();

    ingest_inbox(&repo, &inbox).unwrap();
    compare_report(&repo, "S-E2E-EMR", Some(&ris)).unwrap();

    let handler = review_handler(&db, &stub);
    let response = handler.handle(&ReviewRequest::new(
        "POST",
        "/api/v1/reports/S-E2E-EMR/review",
        Some(TOKEN),
        &confirm_body(),
    ));
    assert_eq!(response.status, 502, "响应体：{}", response.body);

    // 状态如实：转 `pending_writeback`（等重试 / 人工介入），不是「已回写」。
    assert_eq!(report_status(&repo, "S-E2E-EMR"), "pending_writeback");
    // 台账行**保留** —— 结论没丢，丢的是回写这一步。
    assert_eq!(ledger_rows(&repo), 1, "回写失败不该让台账行丢");
    assert_eq!(comparison_rows(&repo), 1, "差值行也保留");

    // 告警：企微收到一条，说清是哪份报告、哪个错误码。
    let pushes = stub.wecom_pushes();
    assert_eq!(pushes.len(), 1, "回写失败应发一条告警");
    let content = pushes[0].json()["text"]["content"].as_str().unwrap().to_string();
    assert!(content.contains("S-E2E-EMR"), "告警要说清是哪份报告");
    assert!(
        content.contains("EMR_UPSTREAM_UNAVAILABLE"),
        "告警要带稳定错误码：{}",
        content
    );
    for fragment in RAW_FRAGMENTS {
        assert!(
            !content.contains(fragment),
            "告警正文混进了原文叙述「{}」：{}",
            fragment,
            content
        );
    }

    // 失败留痕带稳定错误码。
    let failure_code: String = repo
        .connection()
        .query_row(
            "SELECT error_code FROM audit_log WHERE action = 'failed' \
             AND error_code LIKE 'EMR_%' ORDER BY id LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(failure_code, "EMR_UPSTREAM_UNAVAILABLE");
}

#[test]
fn 解析失败的报告不再对拍_也不进确认队列() {
    let dir = temp_dir("parsefail");
    let inbox = dir.join("inbox");
    std::fs::create_dir_all(&inbox).unwrap();
    write_delivery(&inbox, "S-E2E-BAD", REPORT_NO_UNIT);

    let db = dir.join("state.sqlite");
    let repo = Repo::open(&db).unwrap();
    let stub = Stub::start(StubConfig::default());
    let ris = RisClient::new(&stub.base_url, RIS_TOKEN, Duration::from_secs(2)).unwrap();

    ingest_inbox(&repo, &inbox).unwrap();
    assert_eq!(
        report_status(&repo, "S-E2E-BAD"),
        "parse_failed",
        "读不出数值的报告状态是解析失败"
    );

    // 终态报告不再对拍（显式拒绝，不是静默跳过）。
    let err = compare_report(&repo, "S-E2E-BAD", Some(&ris)).unwrap_err();
    assert!(
        err.to_string().contains("parse_failed"),
        "拒绝理由要说得清：{}",
        err
    );
    assert_eq!(stub.ris_reads().len(), 0, "不该向 RIS 取数");
    assert_eq!(comparison_rows(&repo), 0, "不该产差值行");
}
