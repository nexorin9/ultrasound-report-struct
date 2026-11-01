//! 企业微信推送**契约测试** —— 真实 HTTP 报文，本地替身只作测试数据层。
//!
//! **替身不是最终对接边界**：这里起的是一个 `127.0.0.1` 上的裸 HTTP 服务，
//! 用来断言**客户端真的按契约发了什么**（方法 / 完整地址 / 头 / 请求体逐字段）。
//! 院内真实对接边界是 `spec.md` §2.3 那一行 —— `POST {WECOM_WEBHOOK_URL}`
//! 走 HTTPS 到企业微信群机器人，地址里的 key 就是凭据；
//! 替身**不模拟** TLS、**不模拟**企微的真实业务数据，
//! 真实企微群机器人尚未实连（凭据不在本环境），见 `progress.txt`。
//!
//! 五组断言：
//!
//! 1. **请求形状** —— 方法、完整地址（含 key）、`Content-Type`；
//! 2. **请求体结构** —— `{"msgtype":"text","text":{"content":"…"}}`，顶层不多不少；
//! 3. **只发结构化摘要** —— 推送正文扫描：原始报告的叙述段落**一段都不许出现**；
//! 4. **错误语义** —— 5xx / 4xx / **HTTP 200 但 errcode 非 0** / 超时 / 未配置，
//!    各自落到各自的语义与稳定码；
//! 5. **地址打码** —— key 不进错误说明与展示地址（地址即凭据）。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use ultrasound_report_struct::outbound::wecom::{
    classify_errcode, redact_webhook_url, PushComparison, PushKind, PushSummary, WeComClient,
    WeComErrorKind, WeComMessage, WeComText, WECOM_MSG_TYPE_TEXT,
};
use ultrasound_report_struct::parse::errors::{DetailCode, ErrorCode};

/// 一份**带叙述段**的脱敏超声报告（叙述段是不许外发的东西）。
const REPORT: &str = "超声所见：\n左肾前后径 3.2cm\n肾盂轻度扩张\n提示：左肾积水，请结合临床。";

/// 替身的 webhook key（**假值** —— 断言里用它确认地址真的带上去了）。
const STUB_KEY: &str = "stub-webhook-key-0001";

/// 替身的一次应答。
#[derive(Clone, Debug)]
enum Reply {
    /// 回一个 HTTP 响应。
    Respond { status: u16, body: String },
    /// 收下连接但**不回话** —— 用来触发客户端超时。
    Hang,
}

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

    /// 请求体解析成 JSON（请求体是本产品自己组装的，必然是合法 JSON）。
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).expect("请求体应是合法 JSON")
    }
}

/// 本地 HTTP 替身（**只在测试数据层**）。
struct Stub {
    base_url: String,
    recorded: Arc<Mutex<Vec<Recorded>>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Stub {
    fn start(replies: Vec<Reply>) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定替身端口");
        let addr = listener.local_addr().expect("取替身地址");
        listener.set_nonblocking(true).expect("替身转非阻塞");

        let recorded = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_recorded = Arc::clone(&recorded);
        let thread_stop = Arc::clone(&stop);

        let handle = thread::spawn(move || {
            let mut index = 0usize;
            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let reply = replies.get(index).cloned().unwrap_or(Reply::Respond {
                            status: 503,
                            body: "{\"errcode\":-1,\"errmsg\":\"替身没有更多应答了\"}".to_string(),
                        });
                        index += 1;
                        let rec = Arc::clone(&thread_recorded);
                        let stop = Arc::clone(&thread_stop);
                        // 每次连接单独一个线程 —— 重试时不会排在上一条后面。
                        thread::spawn(move || serve(stream, reply, rec, stop));
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

    /// 收到的请求（按到达顺序）。
    fn requests(&self) -> Vec<Recorded> {
        self.recorded.lock().expect("取替身记录").clone()
    }

    /// 带 webhook 路径与 key 的完整地址。
    fn webhook_url(&self) -> String {
        format!(
            "{}/cgi-bin/webhook/send?key={}",
            self.base_url, STUB_KEY
        )
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

/// 处理一条连接：读完请求行、请求头与请求体，记下来，再按脚本应答。
fn serve(
    mut stream: TcpStream,
    reply: Reply,
    recorded: Arc<Mutex<Vec<Recorded>>>,
    stop: Arc<AtomicBool>,
) {
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
    // 请求体按 Content-Length 读满（客户端分片发也要读全）。
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
        target,
        headers,
        body: String::from_utf8_lossy(&body_bytes).to_string(),
    });

    match reply {
        Reply::Hang => {
            // 握住连接不回话，直到替身被关闭 —— 客户端那边应当超时。
            while !stop.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(20));
            }
        }
        Reply::Respond { status, body } => {
            let reason = match status {
                200 => "OK",
                400 => "Bad Request",
                401 => "Unauthorized",
                403 => "Forbidden",
                404 => "Not Found",
                500 => "Internal Server Error",
                503 => "Service Unavailable",
                _ => "Status",
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
    }
}

/// 企微成功应答（真实协议的形状：`errcode` 为 0）。
fn ok_reply() -> Reply {
    Reply::Respond {
        status: 200,
        body: "{\"errcode\":0,\"errmsg\":\"ok\"}".to_string(),
    }
}

/// 一条典型的「回写完成」摘要（**只含结构化字段**）。
fn summary() -> PushSummary {
    PushSummary {
        study_no: "SAMPLE-URO-001".to_string(),
        kind: PushKind::WritebackCompleted,
        item_count: 3,
        comparisons: vec![PushComparison {
            item_code: "kidney.ap_diameter".to_string(),
            current_value: 32.0,
            previous_value: 28.0,
            delta: 4.0,
            unit_code: "mm".to_string(),
        }],
        pending_failure_count: 2,
        dictionary_version: "parts-0.2.0+items-0.1.0".to_string(),
        error_code: None,
        attempts: None,
    }
}

#[test]
fn 契约_请求打到webhook地址且头与体合契约() {
    let stub = Stub::start(vec![ok_reply()]);
    let client = WeComClient::new(&stub.webhook_url(), Duration::from_secs(5)).unwrap();

    let receipt = client.push(&summary()).expect("推送应成功");
    assert_eq!(receipt.status, 200);
    assert_eq!(receipt.errcode, 0);
    assert_eq!(receipt.errmsg, "ok");

    let requests = stub.requests();
    assert_eq!(requests.len(), 1, "只应发一次请求");
    let req = &requests[0];

    // 方法与**完整地址**（key 在地址里 —— 地址即凭据）。
    assert_eq!(req.method, "POST");
    assert!(
        req.target.starts_with("/cgi-bin/webhook/send"),
        "请求路径应命中 webhook 路径，实际 {}",
        req.target
    );
    assert!(
        req.target.contains(STUB_KEY),
        "请求地址应带上 webhook key，实际 {}",
        req.target
    );

    // 头：只有内容类型与接收类型 —— **不带令牌头**（key 在地址里）。
    assert_eq!(req.header("Content-Type"), Some("application/json"));
    assert_eq!(req.header("Accept"), Some("application/json"));
    assert!(
        req.header("Authorization").is_none(),
        "群机器人不另带令牌头，实际 {:?}",
        req.header("Authorization")
    );

    // 请求体：顶层严格两个字段。
    let json = req.json();
    let mut keys: Vec<&str> = json.as_object().unwrap().keys().map(|k| k.as_str()).collect();
    keys.sort();
    assert_eq!(keys, vec!["msgtype", "text"]);
    assert_eq!(json["msgtype"], WECOM_MSG_TYPE_TEXT);
    assert_eq!(json["text"]["content"], summary().render());
}

#[test]
fn 契约_推送正文只含结构化摘要不含原文叙述() {
    let stub = Stub::start(vec![ok_reply()]);
    let client = WeComClient::new(&stub.webhook_url(), Duration::from_secs(5)).unwrap();
    client.push(&summary()).expect("推送应成功");

    let requests = stub.requests();
    let body = requests[0].body.clone();

    // 出站共用的扫描器：报告原文里的中文叙述片段一段都不许出现在报文里。
    let hits = ultrasound_report_struct::outbound::wecom::scan_message_forbidden_text(
        &WeComMessage::from_summary(&summary()),
        REPORT,
    );
    assert!(hits.is_empty(), "推送正文里混进了原文叙述：{:?}", hits);
    // 逐段点名：四段叙述都不在正文里。
    for fragment in ["超声所见", "肾盂轻度扩张", "请结合临床", "左肾积水"] {
        assert!(
            !body.contains(fragment),
            "推送正文里出现了原文叙述「{}」",
            fragment
        );
    }
    // 可核对的部分**在**正文里。
    assert!(body.contains("SAMPLE-URO-001"));
    assert!(body.contains("kidney.ap_diameter"));
    assert!(body.contains("32mm"));
}

#[test]
fn 契约_服务端点不回话时判上游不可达不挂死() {
    let stub = Stub::start(vec![Reply::Hang]);
    let client = WeComClient::new(&stub.webhook_url(), Duration::from_millis(400)).unwrap();
    let err = client.push(&summary()).expect_err("上游不回话应判失败");
    assert_eq!(err.kind, WeComErrorKind::UpstreamUnavailable);
    assert_eq!(err.code(), "WECOM_UPSTREAM_UNAVAILABLE");
    assert_eq!(err.detail, Some(DetailCode::WecomTimeout));
    assert!(err.retryable());
}

#[test]
fn 契约_五xx与四xx各落各的语义() {
    // 5xx → 上游不可达（可重试）。
    let stub = Stub::start(vec![Reply::Respond {
        status: 503,
        body: "{\"errcode\":-1,\"errmsg\":\"busy\"}".to_string(),
    }]);
    let client = WeComClient::new(&stub.webhook_url(), Duration::from_secs(5)).unwrap();
    let err = client.push(&summary()).expect_err("5xx 应判失败");
    assert_eq!(err.kind, WeComErrorKind::UpstreamUnavailable);
    assert_eq!(err.detail, Some(DetailCode::WecomServerError));
    assert!(err.retryable());

    // 4xx → 响应不合契约（不重试）。
    let stub = Stub::start(vec![Reply::Respond {
        status: 404,
        body: "not found".to_string(),
    }]);
    let client = WeComClient::new(&stub.webhook_url(), Duration::from_secs(5)).unwrap();
    let err = client.push(&summary()).expect_err("4xx 应判失败");
    assert_eq!(err.kind, WeComErrorKind::BadResponse);
    assert_eq!(err.detail, Some(DetailCode::WecomClientError));
    assert!(!err.retryable());
}

#[test]
fn 契约_状态码200但errcode非0判失败不静默成功() {
    // **这一组是本层最容易漏的一组**：企微把业务错误放在 errcode 里，
    // 状态码照样是 200 —— 只看状态码的客户端会把「被拒收」当成「已送达」。
    let stub = Stub::start(vec![Reply::Respond {
        status: 200,
        body: "{\"errcode\":93000,\"errmsg\":\"invalid webhook url\"}".to_string(),
    }]);
    let client = WeComClient::new(&stub.webhook_url(), Duration::from_secs(5)).unwrap();
    let err = client.push(&summary()).expect_err("errcode 非 0 必须判失败");
    assert_eq!(err.kind, WeComErrorKind::AuthFailed);
    assert_eq!(err.code(), "WECOM_AUTH_FAILED");
    assert_eq!(err.detail, Some(DetailCode::WecomUnauthorized));
    assert!(!err.retryable());
    assert!(err.reason.contains("93000"));
}

#[test]
fn 契约_响应体不是json也判失败() {
    let stub = Stub::start(vec![Reply::Respond {
        status: 200,
        body: "<html>502 Bad Gateway</html>".to_string(),
    }]);
    let client = WeComClient::new(&stub.webhook_url(), Duration::from_secs(5)).unwrap();
    let err = client.push(&summary()).expect_err("非 JSON 响应必须判失败");
    assert_eq!(err.kind, WeComErrorKind::BadResponse);
    assert_eq!(err.detail, Some(DetailCode::WecomBodyNotJson));
    assert!(!err.retryable());
}

#[test]
fn 契约_地址缺失判未配置且不发请求() {
    let err = WeComClient::new("", Duration::from_secs(1)).expect_err("缺地址应失败");
    assert_eq!(err.kind, WeComErrorKind::NotConfigured);
    assert_eq!(err.code(), "WECOM_NOT_CONFIGURED");
    assert!(err.reason.contains("WECOM_WEBHOOK_URL"));
    // 未配置的处置动作与 EMR 回写失败**不同**（查的是 webhook 那一端）。
    assert_eq!(
        err.error_code().disposition(),
        ultrasound_report_struct::parse::errors::Disposition::AwaitingNotification
    );
}

#[test]
fn 契约_地址里的key不进错误说明与展示地址() {
    let secret = "super-secret-webhook-key";
    let url = format!(
        "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key={}",
        secret
    );
    let client = WeComClient::new(&url, Duration::from_secs(1)).unwrap();

    // 展示地址：key 打码，主机与路径保留。
    let display = client.display_endpoint();
    assert!(!display.contains(secret), "key 泄出来了：{}", display);
    assert!(display.contains("key=***"));
    assert!(display.contains("qyapi.weixin.qq.com"));

    // 打码函数本身：非密钥参数不动。
    assert_eq!(
        redact_webhook_url("https://example.internal/hook?foo=bar"),
        "https://example.internal/hook?foo=bar"
    );
    assert_eq!(
        redact_webhook_url("https://example.internal/hook"),
        "https://example.internal/hook"
    );

    // 失败说明里也不能出现完整地址 —— 错误说明会进失败清单与审计。
    let err = classify_errcode(93000, "invalid webhook url", &url);
    assert!(!err.message().contains(secret), "错误说明里泄出了 key：{}", err.message());
    assert!(!err.reason.contains(secret));
}

#[test]
fn 契约_地址只从环境变量读配置文件不落() {
    use ultrasound_report_struct::config::Credentials;

    // 配置结构里**没有** webhook 字段 —— 端点与凭据都不进配置文件。
    let config = ultrasound_report_struct::config::Config::default();
    let creds = Credentials {
        ris_api_token: None,
        emr_api_token: None,
        wecom_webhook_url: Some("https://example.internal/hook?key=from-env".to_string()),
        daemon_api_token: None,
    };
    let client = WeComClient::from_config(&config, &creds).expect("环境变量里的地址可用");
    assert!(client.endpoint().contains("from-env"));

    // 样例配置文件里不得出现 webhook 地址或 key。
    let sample = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.toml"),
    )
    .expect("读配置样例");
    assert!(
        !sample.contains("qyapi.weixin.qq.com"),
        "配置样例里不该出现 webhook 地址"
    );
    for line in sample.lines() {
        let trimmed = line.trim();
        assert!(
            !trimmed.starts_with("webhook"),
            "配置样例里不该出现 webhook 键：{}",
            trimmed
        );
    }
}

#[test]
fn 契约_凭据位从真实环境变量读到并真的发了出去() {
    // 凭据位是**环境变量** —— 这一条走的是 `from_env()` 的真实路径：
    // 环境变量 → 客户端 → 真实 TCP 连接 → 替身收到带 key 的请求。
    let stub = Stub::start(vec![ok_reply()]);
    let url = stub.webhook_url();
    std::env::set_var("WECOM_WEBHOOK_URL", &url);

    let client = WeComClient::from_env().expect("环境变量里的地址应可用");
    let receipt = client.push(&summary()).expect("推送应成功");
    assert_eq!(receipt.errcode, 0);

    let requests = stub.requests();
    assert_eq!(requests.len(), 1, "只应发一次请求");
    assert!(
        requests[0].target.contains(STUB_KEY),
        "请求地址应带上环境变量里的 key，实际 {}",
        requests[0].target
    );

    std::env::remove_var("WECOM_WEBHOOK_URL");
    // 环境变量撤掉后立刻显式判未配置 —— 不静默沿用上一次的地址。
    let err = WeComClient::from_env().expect_err("撤掉环境变量后应判未配置");
    assert_eq!(err.kind, WeComErrorKind::NotConfigured);
}

#[test]
fn 契约_失败条目转共用码表且带处置动作() {
    let err = WeComClient::new("", Duration::from_secs(1)).unwrap_err();
    let failure = err.to_failure();
    assert_eq!(failure.code, ErrorCode::WecomNotConfigured);
    assert_eq!(failure.line, 0, "出站失败不编造位置");
    assert_eq!(failure.column, 0);
    assert_eq!(
        failure.disposition(),
        ultrasound_report_struct::parse::errors::Disposition::AwaitingNotification
    );

    // 消息体自己组装时字段名要对得上企微契约。
    let message = WeComMessage {
        msgtype: "text".to_string(),
        text: WeComText {
            content: "正文".to_string(),
        },
    };
    let json = serde_json::to_string(&message).unwrap();
    assert!(json.contains("\"msgtype\":\"text\""));
    assert!(json.contains("\"content\":\"正文\""));
}
