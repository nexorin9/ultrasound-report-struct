//! 公开数据获取的集成测试：**本地 HTTP 替身** + **不可达地址降级**。
//!
//! 替身只出现在**测试数据层**：它顶的是「公开数据来源」这一个边界，
//! 不是产品的最终对接边界 —— 产品代码里的地址仍然是清单里的公开地址
//! （见 `src/publicdata/manifest.rs`）。
//!
//! 覆盖三条主张：
//! 1. 请求打到**预期的公开地址**上（断言请求行）；
//! 2. **完整性校验**：字节数 / sha256 对不上就报出来，并点名来源与文件名；
//! 3. **断网降级**：地址不可达时走内置样本集，**不算失败**（退出码 0）。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ultrasound_report_struct::publicdata::integrity::{sha256_hex, write_checksums, ChecksumEntry};
use ultrasound_report_struct::publicdata::report::OutcomeStatus;
use ultrasound_report_struct::publicdata::{
    fetch_public_data_with, FetchOptions, SecondaryFile, SourceKind, SourceSpec,
};

/// 一段最小可解析的 UCUM essential 片段。
const UCUM_XML: &str = r#"<?xml version="1.0" encoding="ascii"?>
<root xmlns="http://unitsofmeasure.org/ucum-essence" version="2.2">
   <unit Code="mm" CODE="mm" isMetric="yes" class="length">
      <name>millimeter</name><printSymbol>mm</printSymbol></unit>
   <unit Code="cm" CODE="cm" isMetric="yes" class="length">
      <name>centimeter</name><printSymbol>cm</printSymbol></unit>
   <unit Code="mL" CODE="ML" isMetric="yes" class="volume">
      <name>millilitre</name><printSymbol>mL</printSymbol></unit>
</root>
"#;

/// UCUM 文件的 contents 接口响应（文件内容以 base64 给出）。
fn ucum_contents_body() -> String {
    format!(
        r#"{{"name":"ucum-essence.xml","encoding":"base64","content":"{}"}}"#,
        base64_encode(UCUM_XML.as_bytes())
    )
}

/// 极简 base64 编码（测试夹具用）。
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 { ALPHABET[((n >> 6) & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[(n & 63) as usize] as char } else { '=' });
    }
    out
}

/// UCUM 完整发布包（最小片段，含 mm / cm / mL 三行码表）。
const UCUM_SPEC_XML: &str = r#"<spec>
   <table><tbody>
   <tr><td>mm</td><td/><td>millimeter</td><td/><td>0.001</td><td>m</td><td/></tr>
   <tr><td>cm</td><td/><td>centimeter</td><td/><td>0.01</td><td>m</td><td/></tr>
   <tr><td>mL</td><td/><td>milliliter</td><td/><td>10-6</td><td>m3</td><td/></tr>
   </tbody></table>
</spec>
"#;

/// UCUM 完整发布包的 contents 接口响应。
fn ucum_spec_contents_body() -> String {
    format!(
        r#"{{"name":"ucum.xml","encoding":"base64","content":"{}"}}"#,
        base64_encode(UCUM_SPEC_XML.as_bytes())
    )
}

/// MeSH 检索响应（esearch）。
const MESH_ESEARCH_BODY: &str =
    r#"{"esearchresult":{"count":"1","idlist":["68007668"]}}"#;

/// MeSH 摘要响应（esummary）。
const MESH_ESUMMARY_BODY: &str = r#"{"result":{"uids":["68007668"],"68007668":{
  "ds_meshui":"D007668","ds_meshterms":["Kidney","Kidneys"],"ds_yearintroduced":"1963"}}}"#;

/// 本地 HTTP 替身：按路径前缀返回固定内容，并记录收到的请求行。
struct Double {
    port: u16,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Double {
    /// 起一个只服务固定内容的小 HTTP 服务。
    fn start(routes: Vec<(&str, &str)>) -> Double {
        let listener = TcpListener::bind("127.0.0.1:0").expect("本地替身应能监听");
        let port = listener.local_addr().expect("应拿到本地端口").port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let owned: Vec<(String, String)> = routes
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let mut buf = [0u8; 4096];
                let read = stream.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..read]).to_string();
                let request_line = head.lines().next().unwrap_or("").to_string();
                recorded.lock().unwrap().push(request_line.clone());
                let body = owned
                    .iter()
                    .find(|(key, _)| request_line.contains(key.as_str()))
                    .map(|(_, value)| value.clone())
                    .unwrap_or_else(|| "not found".to_string());
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body.as_bytes());
                let _ = stream.flush();
            }
        });

        Double { port, requests }
    }

    /// 本地地址（拼在路径前）。
    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{}", self.port, path)
    }

    /// 收到的请求行。
    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

/// 造一个临时项目根（`<root>/assets/public`）。
fn temp_project(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("urs-publicdata-{}", tag));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("assets/public")).unwrap();
    root
}

/// 单位码来源（指向本地替身；含第二份文件 —— 完整码表）。
fn unit_source(url: &str, sha: Option<String>, bytes: Option<u64>) -> SourceSpec {
    SourceSpec {
        id: "ucum-test".to_string(),
        kind: SourceKind::UnitCodeList,
        url: url.to_string(),
        license: "test".to_string(),
        attribution: "test".to_string(),
        purpose: "test".to_string(),
        pinned_sha256: sha,
        pinned_bytes: bytes,
        secondary: Some(SecondaryFile {
            url: url.replace("ucum-essence.xml", "ucum.xml"),
            raw_url: url.replace("ucum-essence.xml", "ucum.xml"),
            file_name: "ucum.xml".to_string(),
            sha256: sha256_hex(UCUM_SPEC_XML.as_bytes()),
            bytes: UCUM_SPEC_XML.len() as u64,
        }),
    }
}

/// 部位编码来源（指向本地替身）。
fn part_source(url: &str) -> SourceSpec {
    SourceSpec {
        id: "mesh-test".to_string(),
        kind: SourceKind::BodyPartCodeList,
        url: url.to_string(),
        license: "test".to_string(),
        attribution: "test".to_string(),
        purpose: "test".to_string(),
        pinned_sha256: None,
        pinned_bytes: None,
        secondary: None,
    }
}

#[test]
fn 请求打到预期地址且产物落地() {
    let double = Double::start(vec![
        ("/ucum-essence.xml", &ucum_contents_body()),
        ("/ucum.xml", &ucum_spec_contents_body()),
    ]);
    let root = temp_project("ok");
    let dest = root.join("assets/public");
    let sha = sha256_hex(UCUM_XML.as_bytes());

    let report = fetch_public_data_with(
        &FetchOptions {
            dest: dest.clone(),
            offline: false,
            timeout_secs: 20,
        },
        &[unit_source(
            &double.url("/ucum-essence.xml"),
            Some(sha.clone()),
            Some(UCUM_XML.len() as u64),
        )],
    )
    .expect("取回不应报错");

    assert!(!report.has_failure(), "{}", report.render());
    assert_eq!(report.outcomes.len(), 1);
    assert_eq!(report.outcomes[0].status, OutcomeStatus::Fetched);

    // 请求行断言：打到了清单里写的那个路径。
    let requests = double.requests();
    assert_eq!(requests.len(), 2, "{:?}", requests);
    assert!(
        requests[0].contains("/ucum-essence.xml"),
        "请求地址不对：{:?}",
        requests
    );
    assert!(
        requests[1].contains("/ucum.xml"),
        "第二份文件请求地址不对：{:?}",
        requests
    );
    assert!(requests[0].starts_with("GET "), "{:?}", requests);

    // 产物落地：单位码核对表 + 校验清单。
    let cross = dest.join("ucum/unit_crosswalk.csv");
    assert!(cross.exists(), "{}", cross.display());
    let text = std::fs::read_to_string(&cross).unwrap();
    // 夹具里的 mm / cm / mL 都在 essential 片段里 → 精确；
    // 夹具里没有的 m / L / cm3 → 未收录（不猜）。
    assert!(text.contains("mm,Length,mm,millimeter,length,精确"), "{}", text);
    // 未收录的行：ucum_code / ucum_name / ucum_class 三列留空，match_type 写「未收录」——
    // 整行 7 列，与表头对齐（曾经多一个逗号，整行右移一列）。
    assert!(text.contains("m,Length,,,,未收录"), "{}", text);
    assert!(text.contains("cm3,Volume,,,,未收录"), "{}", text);
    // 每行列数都与表头一致（7 列）—— 按列号读表的人不会读错。
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        assert_eq!(line.split(',').count(), 7, "核对表列数不对：{}", line);
    }
    assert!(dest.join("CHECKSUMS.sha256").exists());
}

#[test]
fn 部位编码逐条核对到公开术语码() {
    let double = Double::start(vec![
        ("esearch", MESH_ESEARCH_BODY),
        ("esummary", MESH_ESUMMARY_BODY),
    ]);
    let root = temp_project("mesh");
    let dest = root.join("assets/public");

    let report = fetch_public_data_with(
        &FetchOptions {
            dest: dest.clone(),
            offline: false,
            timeout_secs: 20,
        },
        &[part_source(&double.url("/esearch.fcgi"))],
    )
    .expect("取回不应报错");

    assert!(!report.has_failure(), "{}", report.render());
    let csv = std::fs::read_to_string(dest.join("mesh/body_part_codes.csv")).unwrap();
    assert!(csv.contains("kidney.left"), "{}", csv);
    assert!(csv.contains("D007668"), "{}", csv);
    assert!(csv.contains("Urinary Bladder"), "{}", csv);

    // 请求断言：先检索再取摘要，检索词按 URL 编码带出去。
    let requests = double.requests();
    assert!(requests.len() >= 2, "{:?}", requests);
    assert!(requests[0].contains("esearch.fcgi"), "{:?}", requests);
    assert!(requests[0].contains("Kidney%5BMeSH%5D"), "{:?}", requests);
    assert!(requests[1].contains("esummary.fcgi"), "{:?}", requests);
}

#[test]
fn 校验值对不上时报错并点名来源与文件名() {
    let double = Double::start(vec![
        ("/ucum-essence.xml", &ucum_contents_body()),
        ("/ucum.xml", &ucum_spec_contents_body()),
    ]);
    let root = temp_project("badsum");
    let dest = root.join("assets/public");

    let report = fetch_public_data_with(
        &FetchOptions {
            dest: dest.clone(),
            offline: false,
            timeout_secs: 20,
        },
        &[unit_source(
            &double.url("/ucum-essence.xml"),
            Some("0".repeat(64)),
            Some(UCUM_XML.len() as u64),
        )],
    )
    .expect("取回流程本身不应报错");

    assert!(report.has_failure(), "{}", report.render());
    let detail = &report.outcomes[0].detail;
    assert!(detail.contains("ucum-test"), "{}", detail);
    assert!(detail.contains("ucum-essence.xml"), "{}", detail);
    assert!(detail.contains("sha256 不符"), "{}", detail);
    // 校验不过的产物不能落地。
    assert!(!dest.join("ucum/unit_crosswalk.csv").exists());
}

#[test]
fn 字节数对不上时同样报错() {
    let double = Double::start(vec![
        ("/ucum-essence.xml", &ucum_contents_body()),
        ("/ucum.xml", &ucum_spec_contents_body()),
    ]);
    let root = temp_project("badbytes");
    let dest = root.join("assets/public");

    let report = fetch_public_data_with(
        &FetchOptions {
            dest: dest.clone(),
            offline: false,
            timeout_secs: 20,
        },
        &[unit_source(
            &double.url("/ucum-essence.xml"),
            Some(sha256_hex(UCUM_XML.as_bytes())),
            Some(UCUM_XML.len() as u64 + 17),
        )],
    )
    .expect("取回流程本身不应报错");

    assert!(report.has_failure(), "{}", report.render());
    assert!(
        report.outcomes[0].detail.contains("字节数不符"),
        "{}",
        report.outcomes[0].detail
    );
}

#[test]
fn 地址不可达时降级且不算失败() {
    // 127.0.0.1:1 上不会有服务在听 —— 模拟「网络不可用」。
    let root = temp_project("unreachable");
    let dest = root.join("assets/public");

    let report = fetch_public_data_with(
        &FetchOptions {
            dest: dest.clone(),
            offline: false,
            timeout_secs: 5,
        },
        &[part_source("http://127.0.0.1:1/esearch.fcgi")],
    )
    .expect("取回流程本身不应报错");

    // 断网不是失败：没有校验失败，退出码仍是 0。
    assert!(!report.has_failure(), "{}", report.render());
    assert!(report.has_degradation(), "{}", report.render());
    assert_eq!(report.outcomes[0].status, OutcomeStatus::Degraded);
    assert!(
        report.degradation_note.contains("离线降级"),
        "{}",
        report.degradation_note
    );
    // 降级原因里要带网络错误串与改用的样本集版本。
    assert!(
        report.degradation_note.contains("网络错误"),
        "{}",
        report.degradation_note
    );
    assert!(
        report.degradation_note.contains("public-negatives-2026.10"),
        "{}",
        report.degradation_note
    );
}

#[test]
fn 离线模式只校验内置样本集不触网() {
    let root = temp_project("offline");
    let dest = root.join("assets/public");
    std::fs::create_dir_all(dest.join("negatives")).unwrap();
    std::fs::write(dest.join("negatives/neg-001.txt"), b"sample").unwrap();

    // 先用一次联网模式生成校验清单（这里手工写一份等价物）。
    let rel = "assets/public/negatives/neg-001.txt";
    let body = b"sample".to_vec();
    write_checksums(
        &dest.join("CHECKSUMS.sha256"),
        &[ChecksumEntry {
            sha256: sha256_hex(&body),
            bytes: body.len() as u64,
            rel_path: rel.to_string(),
        }],
    )
    .unwrap();

    let report = fetch_public_data_with(
        &FetchOptions {
            dest: dest.clone(),
            offline: true,
            timeout_secs: 5,
        },
        &[part_source("http://127.0.0.1:1/esearch.fcgi")],
    )
    .expect("离线校验不应报错");

    assert!(!report.has_failure(), "{}", report.render());
    assert!(report.offline);
    assert!(
        report.degradation_note.contains("离线降级"),
        "{}",
        report.degradation_note
    );
    assert_eq!(report.outcomes[0].status, OutcomeStatus::Degraded);
    assert!(
        report.outcomes[0].detail.contains("校验通过"),
        "{}",
        report.outcomes[0].detail
    );
}

#[test]
fn 内置样本集被改动时离线校验报不通过() {
    let root = temp_project("tampered");
    let dest = root.join("assets/public");
    std::fs::create_dir_all(dest.join("negatives")).unwrap();
    std::fs::write(dest.join("negatives/neg-001.txt"), b"sample").unwrap();

    let rel = "assets/public/negatives/neg-001.txt";
    let body = b"sample".to_vec();
    write_checksums(
        &dest.join("CHECKSUMS.sha256"),
        &[ChecksumEntry {
            sha256: sha256_hex(&body),
            bytes: body.len() as u64,
            rel_path: rel.to_string(),
        }],
    )
    .unwrap();

    // 样本集被改动。
    std::fs::write(dest.join("negatives/neg-001.txt"), b"tampered").unwrap();

    let report = fetch_public_data_with(
        &FetchOptions {
            dest: dest.clone(),
            offline: true,
            timeout_secs: 5,
        },
        &[],
    )
    .expect("离线校验流程不应报错");

    assert!(report.has_failure(), "{}", report.render());
    assert!(
        report.outcomes[0].detail.contains("neg-001.txt"),
        "{}",
        report.outcomes[0].detail
    );
}

/// 让 `HashMap` 与 `Path` 的导入不被判为未用（本文件用到的辅助类型）。
#[test]
fn 辅助类型导入可用() {
    let mut map: HashMap<String, String> = HashMap::new();
    map.insert("a".to_string(), "b".to_string());
    assert_eq!(map.len(), 1);
    let p = Path::new("/tmp");
    assert!(p.exists());
}
