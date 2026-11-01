//! 台账 / 失败清单 CSV 导出。
//!
//! 两个产物对应两份**可核对**的输出：
//!
//! | 产物 | 内容 | 给谁看 |
//! |---|---|---|
//! | `ledger.csv` | 台账行（检查号 / 病历号 / 部位码 / 测项码 / 归一值 / 单位码 / 强转路径 / 词典版本） | 超声科报告质控岗：可筛选、可统计 |
//! | `failures.csv` | 失败清单（检查号 / 错误码 / 行 / 列 / 原文片段） | 超声医师与信息科：**认不出的值指回原文第几行第几列** |
//!
//! CSV 带 UTF-8 BOM —— 中文医院里双击用 Excel 打开不乱码。

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use rusqlite::Connection;

/// UTF-8 BOM：让 Excel 双击直接按 UTF-8 打开中文 CSV。
const BOM: &str = "\u{feff}";

/// 台账 CSV 的表头。
pub const LEDGER_HEADER: &str =
    "study_no,patient_id,part_key,item_code,value_si,unit_code,coercion_path,dictionary_version";

/// 失败清单 CSV 的表头。
pub const FAILURE_HEADER: &str = "study_no,error_code,line,column,raw_snippet";

/// 导出错误。
#[derive(Debug)]
pub enum ExportError {
    /// 文件系统错误。
    Io(std::io::Error),
    /// 状态库查询错误。
    Sqlite(rusqlite::Error),
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportError::Io(e) => write!(f, "导出文件错误：{}", e),
            ExportError::Sqlite(e) => write!(f, "状态库错误：{}", e),
        }
    }
}

impl std::error::Error for ExportError {}

impl From<std::io::Error> for ExportError {
    fn from(e: std::io::Error) -> Self {
        ExportError::Io(e)
    }
}

impl From<rusqlite::Error> for ExportError {
    fn from(e: rusqlite::Error) -> Self {
        ExportError::Sqlite(e)
    }
}

/// 一次导出的结果。
#[derive(Clone, Debug)]
pub struct ExportSummary {
    /// 台账 CSV 路径。
    pub ledger_path: PathBuf,
    /// 失败清单 CSV 路径。
    pub failure_path: PathBuf,
    /// 台账行数（不含表头）。
    pub ledger_rows: usize,
    /// 失败条目行数（不含表头）。
    pub failure_rows: usize,
}

/// 导出两个产物到 `out_dir`。
pub fn export_all(conn: &Connection, out_dir: &Path) -> Result<ExportSummary, ExportError> {
    std::fs::create_dir_all(out_dir)?;
    let ledger_path = out_dir.join("ledger.csv");
    let failure_path = out_dir.join("failures.csv");
    let ledger_rows = export_ledger(conn, &ledger_path)?;
    let failure_rows = export_failures(conn, &failure_path)?;
    Ok(ExportSummary {
        ledger_path,
        failure_path,
        ledger_rows,
        failure_rows,
    })
}

/// 导出台账 CSV。
pub fn export_ledger(conn: &Connection, out: &Path) -> Result<usize, ExportError> {
    let mut writer = CsvWriter::create(out)?;
    writer.line(LEDGER_HEADER)?;

    let mut stmt = conn.prepare(
        "SELECT r.study_no, r.patient_id, i.part_key, i.item_code, i.value_si, i.unit_code, \
         i.coercion_path, i.dictionary_version \
         FROM items i JOIN reports r ON r.id = i.report_id \
         ORDER BY r.study_no, i.id",
    )?;
    let mut rows = stmt.query([])?;
    let mut count = 0usize;
    while let Some(row) = rows.next()? {
        let study_no: String = row.get(0)?;
        let patient_id: Option<String> = row.get(1)?;
        let part_key: Option<String> = row.get(2)?;
        let item_code: Option<String> = row.get(3)?;
        let value_si: Option<f64> = row.get(4)?;
        let unit_code: Option<String> = row.get(5)?;
        let coercion_path: String = row.get(6)?;
        let dictionary_version: String = row.get(7)?;

        writer.record(&[
            study_no.as_str(),
            patient_id.as_deref().unwrap_or(""),
            part_key.as_deref().unwrap_or(""),
            item_code.as_deref().unwrap_or(""),
            &fmt_number(value_si),
            unit_code.as_deref().unwrap_or(""),
            coercion_path.as_str(),
            dictionary_version.as_str(),
        ])?;
        count += 1;
    }
    writer.finish()?;
    Ok(count)
}

/// 导出失败清单 CSV（失败条目来自审计表的 `failed` 动作）。
pub fn export_failures(conn: &Connection, out: &Path) -> Result<usize, ExportError> {
    let mut writer = CsvWriter::create(out)?;
    writer.line(FAILURE_HEADER)?;

    let mut stmt = conn.prepare(
        "SELECT r.study_no, a.error_code, a.line, a.column, a.raw_snippet \
         FROM audit_log a JOIN reports r ON r.id = a.report_id \
         WHERE a.action = 'failed' AND a.error_code IS NOT NULL \
         ORDER BY r.study_no, a.id",
    )?;
    let mut rows = stmt.query([])?;
    let mut count = 0usize;
    while let Some(row) = rows.next()? {
        let study_no: String = row.get(0)?;
        let error_code: Option<String> = row.get(1)?;
        let line: Option<i64> = row.get(2)?;
        let column: Option<i64> = row.get(3)?;
        let raw_snippet: Option<String> = row.get(4)?;

        writer.record(&[
            study_no.as_str(),
            error_code.as_deref().unwrap_or(""),
            &line.map(|v| v.to_string()).unwrap_or_default(),
            &column.map(|v| v.to_string()).unwrap_or_default(),
            raw_snippet.as_deref().unwrap_or(""),
        ])?;
        count += 1;
    }
    writer.finish()?;
    Ok(count)
}

/// 数值格式化：整数不带小数尾巴（`32` 而不是 `32.0`），非整数保留最多 3 位。
pub fn fmt_number(value: Option<f64>) -> String {
    match value {
        None => String::new(),
        Some(n) => {
            if !n.is_finite() {
                return String::new();
            }
            if n.fract() == 0.0 && n.abs() < 1e15 {
                format!("{}", n as i64)
            } else {
                let s = format!("{:.3}", n);
                let trimmed = s.trim_end_matches('0');
                let trimmed = trimmed.trim_end_matches('.');
                trimmed.to_string()
            }
        }
    }
}

/// CSV 字段转义：含逗号 / 引号 / 换行时加引号并把内部引号翻倍。
pub fn csv_field(value: &str) -> String {
    if value.contains(',') || value.contains('"') || value.contains('\n') || value.contains('\r') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

/// 简易 CSV 写出器（带 BOM，逐行刷新到缓冲）。
struct CsvWriter {
    inner: BufWriter<File>,
}

impl CsvWriter {
    fn create(path: &Path) -> Result<Self, ExportError> {
        let file = File::create(path)?;
        let mut inner = BufWriter::new(file);
        inner.write_all(BOM.as_bytes())?;
        Ok(CsvWriter { inner })
    }

    fn line(&mut self, content: &str) -> Result<(), ExportError> {
        writeln!(self.inner, "{}", content)?;
        Ok(())
    }

    fn record(&mut self, fields: &[&str]) -> Result<(), ExportError> {
        let line = fields
            .iter()
            .map(|f| csv_field(f))
            .collect::<Vec<_>>()
            .join(",");
        writeln!(self.inner, "{}", line)?;
        Ok(())
    }

    fn finish(mut self) -> Result<(), ExportError> {
        self.inner.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_report;
    use crate::state::repo::{IngestRecord, Repo};
    use crate::state::idempotency_key;

    fn seeded_repo() -> Repo {
        let repo = Repo::open_in_memory().unwrap();
        for (study_no, body) in [
            ("SAMPLE-A", "左肾积水 3.2cm"),
            ("SAMPLE-B", "左肾积水 3.2"),
        ] {
            let outcome = parse_report(body);
            repo.commit_ingestion(&IngestRecord {
                study_no,
                patient_id: Some("PID-EXAMPLE"),
                source_path: None,
                idempotency_key: &idempotency_key(study_no, body),
                dictionary_version: crate::dictionary_version(),
                items: &outcome.items,
                failures: &outcome.failures,
            })
            .unwrap();
        }
        repo
    }

    #[test]
    fn 台账导出一行且强转路径与归一值正确() {
        let repo = seeded_repo();
        let dir = std::env::temp_dir().join(format!("urs-export-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("ledger.csv");
        let rows = export_ledger(repo.connection(), &out).unwrap();
        assert_eq!(rows, 1);

        let content = std::fs::read_to_string(&out).unwrap();
        assert!(content.starts_with(BOM), "中文 CSV 应带 UTF-8 BOM");
        let header = content.lines().next().unwrap();
        assert!(header.contains("coercion_path"));
        let data = content.lines().nth(1).unwrap();
        assert!(data.contains("SAMPLE-A"), "数据行：{}", data);
        assert!(data.contains("32"), "归一值应为 32mm：{}", data);
        assert!(data.contains("mm"));
        assert!(data.contains("UnitConvert"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 失败清单导出带行列与错误码() {
        let repo = seeded_repo();
        let dir = std::env::temp_dir().join(format!("urs-export-fail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("failures.csv");
        let rows = export_failures(repo.connection(), &out).unwrap();
        assert_eq!(rows, 1);

        let content = std::fs::read_to_string(&out).unwrap();
        let header = content.lines().next().unwrap();
        assert!(header.contains("error_code"));
        assert!(header.contains("line"));
        assert!(header.contains("column"));
        let data = content.lines().nth(1).unwrap();
        assert!(data.contains("SAMPLE-B"), "数据行：{}", data);
        assert!(data.contains("UNIT_MISSING"), "数据行：{}", data);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 字段转义处理逗号与引号() {
        assert_eq!(csv_field("正常"), "正常");
        assert_eq!(csv_field("含,逗号"), "\"含,逗号\"");
        assert_eq!(csv_field("含\"引号\""), "\"含\"\"引号\"\"\"");
    }

    #[test]
    fn 数值格式化整数不带小数尾巴() {
        assert_eq!(fmt_number(Some(32.0)), "32");
        assert_eq!(fmt_number(Some(3.25)), "3.25");
        assert_eq!(fmt_number(None), "");
    }
}
