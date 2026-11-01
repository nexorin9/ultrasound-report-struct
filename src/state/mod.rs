//! 状态库：schema、迁移、仓储（事务边界）与幂等键。
//!
//! 主路径：`报告 → 解析产出 → [`repo::Repo::commit_ingestion`] → 台账行 + 审计留痕`。
//!
//! **口径纪律**：台账行只落**结构化结论**（部位码 / 测项码 / 数值 / 单位 / 位置 /
//! 强转路径），原始可识别文本不进库（出站报文同理，见 `spec.md` §2.3）。

pub mod repo;
pub mod schema;

pub use repo::{
    AuditEntry, IdempotencyRow, IngestRecord, IngestResult, NewComparison, NewItem, NewReport,
    PendingEvent, Repo, RepoError, ReportRow, ACTION_DUPLICATE_SKIPPED, ACTION_FAILED,
    ACTION_PARSED, ACTION_RECEIVED,
};
pub use schema::{migrate, SCHEMA_VERSION};

use sha2::{Digest, Sha256};

/// 幂等键 = `sha256(study_no ‖ 报告内容)`。
///
/// 口径与 `spec.md` §2.3 / §4 一致（回写请求头 `Idempotency-Key` 同源）。
/// **拼接顺序固定**：先检查号、后报告内容。
pub fn idempotency_key(study_no: &str, body: &str) -> String {
    idempotency_key_bytes(study_no, body.as_bytes())
}

/// 幂等键 = `sha256(study_no ‖ 报告内容字节)`。
///
/// 与 [`idempotency_key`] **同源**（同一算法、同一拼接顺序），差别只在输入是**字节**
/// 还是字符串：入站监听在读到文件快照时算摘要，手上没有解码正文，用字节更贴近现场。
/// UTF-8 报告的原始字节与解码正文逐字节相同，两条路径算出的键必然一致。
pub fn idempotency_key_bytes(study_no: &str, body: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(study_no.as_bytes());
    hasher.update(body);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{:02x}", byte));
    }
    out
}

/// 当前 UTC 时间（`YYYY-MM-DD HH:mm:ss`）。
pub fn now_datetime() -> String {
    datetime_from_unix(utc_now_secs())
}

/// 当前 UTC 时间（**毫秒精度**：`YYYY-MM-DD HH:mm:ss.mmm`）。
///
/// **审计留痕专用**：一份报告的处理会在**同一秒内**落下多条留痕
/// （接收 / 解析 / 强转 / 对拍 / 确认 / 回写 / 推送）。
/// 秒级时间戳会让这七条留痕**看起来是同一时刻**，分不清先后 ——
/// 审计链的价值恰恰在于顺序。
///
/// 毫秒精度是**分辨率**不是**保证**：同一毫秒内的多条留痕时间戳仍可能相同，
/// 审计链的**权威顺序**是自增主键 `id`（严格递增）。
pub fn now_datetime_millis() -> String {
    let millis_total = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let secs = (millis_total / 1000) as u64;
    let millis = (millis_total % 1000) as u32;
    format!("{}.{:03}", datetime_from_unix(secs), millis)
}

/// 当前 Unix 秒。
pub fn utc_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Unix 秒 → `YYYY-MM-DD HH:mm:ss`（UTC）。
pub fn datetime_from_unix(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = (secs % 86_400) as u32;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        year,
        month,
        day,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// 天数 → (年, 月, 日)（Howard Hinnant 的 civil_from_days 算法，UTC 无时区偏移）。
///
/// **公开**：运行摘要的「按日窗口」要算「下一天是哪天」，
/// 用的是本函数的逆运算 [`days_from_civil`] —— 日期换算只有这一处实现，
/// 别处再抄一份算法，两边迟早漂移。
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// (年, 月, 日) → 天数（[`civil_from_days`] 的逆运算，UTC 无时区偏移）。
///
/// 同一个 Howard Hinnant 算法 —— 日期换算只有这一处实现。
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 纪元起点换算正确() {
        assert_eq!(datetime_from_unix(0), "1970-01-01 00:00:00");
        assert_eq!(datetime_from_unix(86_399), "1970-01-01 23:59:59");
        assert_eq!(datetime_from_unix(86_400), "1970-01-02 00:00:00");
    }

    #[test]
    fn 二零二六年日期换算正确() {
        // 2026-10-31 00:00:00 UTC
        assert_eq!(datetime_from_unix(1_793_404_800), "2026-10-31 00:00:00");
        // 2024-02-29 00:00:00 UTC（闰年）
        assert_eq!(datetime_from_unix(1_709_164_800), "2024-02-29 00:00:00");
    }

    #[test]
    fn 幂等键稳定且随内容变化() {
        let a = idempotency_key("SAMPLE-A", "左肾积水 3.2cm");
        let b = idempotency_key("SAMPLE-A", "左肾积水 3.2cm");
        let c = idempotency_key("SAMPLE-A", "左肾积水 3.3cm");
        assert_eq!(a, b, "同一份报告的幂等键必须稳定");
        assert_ne!(a, c, "内容变了幂等键必须变");
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|ch| ch.is_ascii_hexdigit()));
    }
}
