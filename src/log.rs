//! 结构化行日志 + 运行摘要。
//!
//! **为什么是行日志**：守护进程常驻在超声科机房里，没人盯着屏幕；出问题时运维要的是
//! 「能 grep 的几行」，不是一屏花哨表格。所以每条事件是**一行**：
//!
//! ```text
//! 2026-10-31 21:30:00 INFO  REPORT_PARSED study_no=sample-uro-001 dictionary_version=parts-0.1.0 items=1 failures=0
//! ```
//!
//! 字段固定顺序：`时间 级别 事件码 [键=值…]`。事件码是**稳定**的（大写下划线），
//! 运维可按事件码聚合；`study_no` 与 `dictionary_version` 随事件带出，
//! 便于把一条台账行和它当时的词典版本对上（口径追溯）。
//!
//! **运行摘要**（[`RunSummary`]）回答运维那个最常被问到的问题：
//! 「昨天有多少报告没解析出来」—— `processed / failed / failed_rate /
//! awaiting_review / pending_writeback` 五个数，随心跳一起打出来。

use std::io::Write;
use std::sync::Mutex;

use crate::state::now_datetime;

/// 日志级别。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// 调试细节。
    Debug,
    /// 正常事件。
    Info,
    /// 需要人留意但不阻断。
    Warn,
    /// 失败。
    Error,
}

impl Level {
    /// 级别字符串（固定 5 字符宽，便于列对齐）。
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }
}

/// 结构化行日志器。
///
/// 输出目标可替换（测试里换成内存缓冲），因此业务代码不必关心日志落到哪。
pub struct Logger {
    sink: Mutex<Box<dyn Write + Send>>,
    min_level: Level,
    /// 显式指定的词典版本（`None` = 每次写出时取**当前生效**的词典版本）。
    ///
    /// **不在建日志器时快照版本**：建日志器的那一刻词典可能还没加载，
    /// 快照会把词典单例按**默认路径**抢先初始化 —— 配置文件里指定的
    /// 词典路径就被悄悄忽略（运维改了词典却没生效，且没有任何提示）。
    /// 改成「写出时取当前版本」，日志行说的才是**这条事件发生时**的口径。
    dictionary_version: Option<String>,
}

impl Logger {
    /// 输出到标准输出（守护进程默认）。
    pub fn stdout() -> Logger {
        Logger::with_sink(Box::new(std::io::stdout()))
    }

    /// 输出到指定目标（测试用）。
    pub fn with_sink(sink: Box<dyn Write + Send>) -> Logger {
        Logger {
            sink: Mutex::new(sink),
            min_level: Level::Info,
            dictionary_version: None,
        }
    }

    /// 设置最低级别（低于该级别的事件不输出）。
    pub fn set_min_level(&mut self, level: Level) {
        self.min_level = level;
    }

    /// 设置默认词典版本（每行随事件带出，供口径追溯）。
    pub fn set_dictionary_version(&mut self, version: &str) {
        self.dictionary_version = Some(version.to_string());
    }

    /// 输出一条事件。
    pub fn event(&self, level: Level, code: &str, fields: &[(&str, String)]) {
        if level < self.min_level {
            return;
        }

        let mut line = String::with_capacity(128);
        line.push_str(&now_datetime());
        line.push(' ');
        line.push_str(level.as_str());
        for _ in level.as_str().len()..5 {
            line.push(' ');
        }
        line.push(' ');
        line.push_str(code);

        // 词典版本默认随行带出（调用方显式给了就不重复）。
        if !fields.iter().any(|(key, _)| *key == "dictionary_version") {
            line.push_str(" dictionary_version=");
            let version = self
                .dictionary_version
                .clone()
                .unwrap_or_else(crate::dictionary_version);
            line.push_str(&sanitize(&version));
        }
        for (key, value) in fields {
            line.push(' ');
            line.push_str(key);
            line.push('=');
            line.push_str(&sanitize(value));
        }

        let mut sink = match self.sink.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _ = writeln!(sink, "{}", line);
        let _ = sink.flush();
    }

    /// 普通事件。
    pub fn info(&self, code: &str, fields: &[(&str, String)]) {
        self.event(Level::Info, code, fields);
    }

    /// 需要留意的事件。
    pub fn warn(&self, code: &str, fields: &[(&str, String)]) {
        self.event(Level::Warn, code, fields);
    }

    /// 失败事件。
    pub fn error(&self, code: &str, fields: &[(&str, String)]) {
        self.event(Level::Error, code, fields);
    }
}

/// 字段值清洗：空白压成下划线、换行转义、过长截断 —— 保证**一行**就是一行。
fn sanitize(value: &str) -> String {
    const MAX_LEN: usize = 200;
    let mut out = String::with_capacity(value.len().min(MAX_LEN));
    for ch in value.chars() {
        match ch {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push(' '),
            ' ' => out.push('_'),
            c if (c as u32) < 0x20 => out.push(' '),
            c => out.push(c),
        }
        if out.chars().count() >= MAX_LEN {
            out.push('…');
            break;
        }
    }
    out
}

/// 运行摘要聚合器：处理数 / 失败数 / 待确认数 / 待回写数。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunSummary {
    /// 已处理报告数。
    pub processed: u64,
    /// 失败条目数（解析失败 / 强转无路径 / 部位未命中等）。
    pub failed: u64,
    /// 待医师确认数。
    pub awaiting_review: u64,
    /// 待回写数（回写失败后重试耗尽，转人工介入）。
    pub pending_writeback: u64,
}

impl RunSummary {
    /// 新建空摘要。
    pub fn new() -> RunSummary {
        RunSummary::default()
    }

    /// 记一份处理过的报告。
    pub fn record_processed(&mut self) {
        self.processed += 1;
    }

    /// 记若干份处理过的报告。
    pub fn record_processed_n(&mut self, n: u64) {
        self.processed += n;
    }

    /// 记一条失败。
    pub fn record_failed(&mut self) {
        self.failed += 1;
    }

    /// 记若干条失败。
    pub fn record_failed_n(&mut self, n: u64) {
        self.failed += n;
    }

    /// 设待确认数。
    pub fn set_awaiting_review(&mut self, n: u64) {
        self.awaiting_review = n;
    }

    /// 设待回写数。
    pub fn set_pending_writeback(&mut self, n: u64) {
        self.pending_writeback = n;
    }

    /// 失败率（处理数为 0 时取 0）。
    pub fn failed_rate(&self) -> f64 {
        if self.processed == 0 {
            return 0.0;
        }
        self.failed as f64 / self.processed as f64
    }

    /// 渲染成一行摘要（随心跳打出）。
    pub fn render(&self) -> String {
        format!(
            "运行摘要 processed={} failed={} failed_rate={:.3} awaiting_review={} pending_writeback={}",
            self.processed,
            self.failed,
            self.failed_rate(),
            self.awaiting_review,
            self.pending_writeback
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// 收集日志行的内存缓冲（测试替身，仅测试数据层）。
    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Buffer {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).to_string()
        }
    }

    #[test]
    fn 日志行含时间级别事件码与关键字段() {
        let buffer = Buffer::default();
        let logger = Logger::with_sink(Box::new(buffer.clone()));
        logger.info(
            "REPORT_PARSED",
            &[
                ("study_no", "sample-uro-001".to_string()),
                ("items", "1".to_string()),
                ("failures", "0".to_string()),
            ],
        );
        let text = buffer.text();
        let line = text.trim_end();
        assert!(line.contains("INFO"), "{}", line);
        assert!(line.contains("REPORT_PARSED"), "{}", line);
        assert!(line.contains("study_no=sample-uro-001"), "{}", line);
        assert!(line.contains("dictionary_version="), "{}", line);
        assert_eq!(text.lines().count(), 1, "一条事件一行");
    }

    #[test]
    fn 显式给出的词典版本不重复追加() {
        let buffer = Buffer::default();
        let logger = Logger::with_sink(Box::new(buffer.clone()));
        logger.info(
            "REPORT_PARSED",
            &[("dictionary_version", "parts-9.9.9".to_string())],
        );
        let text = buffer.text();
        assert_eq!(text.matches("dictionary_version=").count(), 1, "{}", text);
        assert!(text.contains("dictionary_version=parts-9.9.9"));
    }

    #[test]
    fn 字段值里的空格与换行不破坏单行结构() {
        let buffer = Buffer::default();
        let logger = Logger::with_sink(Box::new(buffer.clone()));
        logger.info("REPORT_PARSED", &[("raw_snippet", "左肾 积水\n3.2cm".to_string())]);
        let text = buffer.text();
        assert_eq!(text.lines().count(), 1, "换行必须被转义：{}", text);
        assert!(text.contains("raw_snippet=左肾_积水\\n3.2cm"), "{}", text);
    }

    #[test]
    fn 低于最低级别的事件不输出() {
        let buffer = Buffer::default();
        let mut logger = Logger::with_sink(Box::new(buffer.clone()));
        logger.set_min_level(Level::Warn);
        logger.info("IGNORED", &[]);
        logger.warn("KEPT", &[]);
        let text = buffer.text();
        assert!(!text.contains("IGNORED"));
        assert!(text.contains("KEPT"));
    }

    #[test]
    fn 摘要计数与失败率正确() {
        let mut summary = RunSummary::new();
        for _ in 0..10 {
            summary.record_processed();
        }
        summary.record_failed_n(3);
        summary.set_awaiting_review(4);
        summary.set_pending_writeback(1);
        assert_eq!(summary.processed, 10);
        assert_eq!(summary.failed, 3);
        assert_eq!(summary.awaiting_review, 4);
        assert_eq!(summary.pending_writeback, 1);
        assert!((summary.failed_rate() - 0.3).abs() < 1e-9);
        let rendered = summary.render();
        assert!(rendered.contains("processed=10"), "{}", rendered);
        assert!(rendered.contains("failed=3"), "{}", rendered);
        assert!(rendered.contains("failed_rate=0.300"), "{}", rendered);
        assert!(rendered.contains("awaiting_review=4"), "{}", rendered);
        assert!(rendered.contains("pending_writeback=1"), "{}", rendered);
    }

    #[test]
    fn 空摘要失败率为零不出现除零() {
        let summary = RunSummary::new();
        assert_eq!(summary.failed_rate(), 0.0);
    }
}
