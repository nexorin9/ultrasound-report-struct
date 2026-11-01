//! **自检通道**（`selftest`）：一条命令出可核对结果。
//!
//! ## 回答哪三个问题
//!
//! | 数字 | 含义 | 从哪来 |
//! |---|---|---|
//! | **台账行数** | 解析 + 强转走完之后，真正落下来的测值有几条 | 内存状态库（真实入库路径） |
//! | **失败条目数** | 有几条测值**没读出来**（带稳定错误码与行列） | 内存状态库的失败留痕 |
//! | **差值条目数** | 有多少条测值**与 RIS 同侧历史值对上了**（给出差值） | 差值基准集逐条驱动真实对拍主路径 |
//!
//! ## 自检**不碰业务数据**
//!
//! - 状态库开在**内存**里（[`Repo::open_in_memory`]）——运维的真库一个字节都不动；
//! - 语料、对照表、负样例全部来自**仓库内置资产**（`assets/`），
//!   不连 RIS / EMR / 企微（**真实出站不在自检范围内**，这一点会打印出来）；
//! - 词典用仓库内置的那一份（版本号一并打印，口径说得清）。
//!
//! ## 缺凭据怎么办
//!
//! 自检**不需要**出站凭据 —— 它跑的是离线主路径。
//! 凭据没配时**明确打印**「本次自检不含真实出站」，**不 panic**、
//! **不把没配当成失败**；只有显式要求（`--require-credentials`）时才因缺项判失败。
//!
//! ## 任一项不过 → 退出码非 0
//!
//! 自检的意义就是「一条命令就能回答这套东西是不是好的」——
//! 打印一屏好看的话不算自检。

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::OptionalExtension;

use crate::compare::baseline::BaselineTable;
use crate::compare::ComparisonState;
use crate::coerce::table::CoercionTable;
use crate::ingest::ingest_inbox;
use crate::parse::raw_parse;
use crate::state::repo::Repo;

/// 自检参数。
#[derive(Clone, Debug)]
pub struct SelfTestOptions {
    /// 资产目录（默认 `assets`）。
    pub assets_dir: PathBuf,
    /// 是否要求出站凭据齐备（缺了就判失败）。
    pub require_credentials: bool,
}

impl Default for SelfTestOptions {
    fn default() -> Self {
        SelfTestOptions {
            assets_dir: PathBuf::from("assets"),
            require_credentials: false,
        }
    }
}

/// 一项自检。
#[derive(Clone, Debug)]
pub struct SelfCheck {
    /// 检查项名字。
    pub name: String,
    /// 是否通过。
    pub passed: bool,
    /// 人话说明（数字与差异都写出来）。
    pub detail: String,
}

/// 自检结果。
#[derive(Clone, Debug, Default)]
pub struct SelfTestReport {
    /// 逐项结果。
    pub checks: Vec<SelfCheck>,
    /// 台账行数。
    pub ledger_rows: i64,
    /// 失败条目数。
    pub failure_entries: i64,
    /// 差值条目数。
    pub comparison_entries: i64,
    /// 没配的出站凭据（**只是提示**，除非要求凭据齐备）。
    pub missing_credentials: Vec<String>,
}

impl SelfTestReport {
    /// 是否全部通过。
    pub fn passed(&self) -> bool {
        self.checks.iter().all(|c| c.passed)
    }

    /// 通过了几项。
    pub fn passed_count(&self) -> usize {
        self.checks.iter().filter(|c| c.passed).count()
    }

    /// 人话摘要（打印到终端）。
    pub fn render(&self) -> String {
        let mut out = String::from("超声报告结构化系统 · 自检\n");
        out.push_str("────────────────────────────────────────\n");
        out.push_str(&format!(
            "台账行数：{} 行\n",
            self.ledger_rows
        ));
        out.push_str(&format!(
            "失败条目数：{} 条\n",
            self.failure_entries
        ));
        out.push_str(&format!(
            "差值条目数：{} 条\n",
            self.comparison_entries
        ));
        out.push_str("────────────────────────────────────────\n");
        for check in &self.checks {
            out.push_str(&format!(
                "[{}] {}：{}\n",
                if check.passed { "通过" } else { "未过" },
                check.name,
                check.detail
            ));
        }
        if !self.missing_credentials.is_empty() {
            out.push_str(&format!(
                "提示：未配置出站凭据（{}）—— 本次自检**不含真实出站**，只跑离线主路径。\n",
                self.missing_credentials.join("、")
            ));
        }
        out.push_str(&format!(
            "自检结论：{}（{} / {} 项通过）\n",
            if self.passed() { "通过" } else { "未通过" },
            self.passed_count(),
            self.checks.len()
        ));
        out
    }
}

/// 自检错误（**资产缺失**一类；跑不起来就说清楚，不 panic）。
#[derive(Debug)]
pub enum SelfTestError {
    /// 资产目录 / 文件缺失。
    AssetMissing { path: PathBuf, hint: String },
    /// 资产内容不合法。
    AssetInvalid { path: PathBuf, message: String },
    /// 状态库错误。
    Repo(crate::state::repo::RepoError),
    /// 入站错误。
    Ingest(crate::ingest::IngestError),
}

impl std::fmt::Display for SelfTestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SelfTestError::AssetMissing { path, hint } => {
                write!(f, "找不到内置资产：{}（{}）", path.display(), hint)
            }
            SelfTestError::AssetInvalid { path, message } => {
                write!(f, "内置资产不可用：{}（{}）", path.display(), message)
            }
            SelfTestError::Repo(e) => write!(f, "状态库错误：{}", e),
            SelfTestError::Ingest(e) => write!(f, "入库错误：{}", e),
        }
    }
}

impl std::error::Error for SelfTestError {}

impl From<crate::state::repo::RepoError> for SelfTestError {
    fn from(e: crate::state::repo::RepoError) -> Self {
        SelfTestError::Repo(e)
    }
}

impl From<crate::ingest::IngestError> for SelfTestError {
    fn from(e: crate::ingest::IngestError) -> Self {
        SelfTestError::Ingest(e)
    }
}

/// 跑一遍自检。
pub fn run_selftest(options: &SelfTestOptions) -> Result<SelfTestReport, SelfTestError> {
    let mut report = SelfTestReport::default();

    // 凭据：没配就**明确说出来**（不 panic、不当成失败）。
    let creds = crate::config::Credentials::from_env();
    let mut missing = Vec::new();
    if creds.ris_api_token.is_none() {
        missing.push(crate::config::ENV_RIS_API_TOKEN.to_string());
    }
    if creds.emr_api_token.is_none() {
        missing.push(crate::config::ENV_EMR_API_TOKEN.to_string());
    }
    if creds.wecom_webhook_url.is_none() {
        missing.push(crate::config::ENV_WECOM_WEBHOOK_URL.to_string());
    }
    report.missing_credentials = missing.clone();

    // 1) 解析 + 强转主干（**真实入库路径**，状态库开在内存里）。
    let inbox = options.assets_dir.join("samples/inbox");
    require_dir(&inbox)?;
    let repo = Repo::open_in_memory()?;
    let first = ingest_inbox(&repo, &inbox)?;
    let ledger_rows = count(&repo, "items")?;
    let failure_entries = count_failed(&repo)?;
    report.ledger_rows = ledger_rows;
    report.failure_entries = failure_entries;

    // 主干自检：台账行的那条必须是 32mm / UnitConvert（3.2cm 换算过来）。
    let ledger_ok = ledger_value_ok(&repo)?;
    report.checks.push(SelfCheck {
        name: "解析与强转主干".to_string(),
        passed: ledger_ok && ledger_rows >= 1 && first.reports >= 1,
        detail: format!(
            "内置样例 {} 份 → 台账 {} 行；归一值 3.2cm → 32mm（UnitConvert）：{}",
            first.reports,
            ledger_rows,
            if ledger_ok { "一致" } else { "不一致" }
        ),
    });

    // 失败条目自检：失败要带**稳定错误码与行列**（不是一句「解析失败」）。
    let failure_detail = failure_detail(&repo)?;
    report.checks.push(SelfCheck {
        name: "失败条目带错误码与行列".to_string(),
        passed: failure_entries >= 1 && failure_detail.contains("UNIT_MISSING"),
        detail: format!("失败条目 {} 条（{}）", failure_entries, failure_detail),
    });

    // 2) 幂等：同一批再走一遍，**不得产生第二条台账行**。
    let second = ingest_inbox(&repo, &inbox)?;
    let ledger_after = count(&repo, "items")?;
    let idempotent = second.duplicates == first.reports && ledger_after == ledger_rows;
    report.checks.push(SelfCheck {
        name: "重复投递不产生第二条台账行".to_string(),
        passed: idempotent,
        detail: format!(
            "重投 {} 份：受理 0 份 / 幂等命中 {} 份；台账行 {} → {} 行",
            second.reports + second.duplicates,
            second.duplicates,
            ledger_rows,
            ledger_after
        ),
    });

    // 3) 差值（oracle = 差值基准集，RIS 真实历史值口径）。
    let baseline_path = options.assets_dir.join("samples/compare_baseline.csv");
    require_file(&baseline_path)?;
    let table = BaselineTable::load(&baseline_path).map_err(|e| SelfTestError::AssetInvalid {
        path: baseline_path.clone(),
        message: e.to_string(),
    })?;
    let mut deltas = 0usize;
    let mut mismatched = 0usize;
    for row in table.rows() {
        let outcome = row.drive();
        if outcome.delta.is_some() {
            deltas += 1;
        }
        if outcome.state != Some(row.expect_state) {
            mismatched += 1;
        }
    }
    report.comparison_entries = deltas as i64;
    report.checks.push(SelfCheck {
        name: "对拍差值（基准集逐条驱动）".to_string(),
        passed: mismatched == 0 && deltas > 0,
        detail: format!(
            "基准集 {} 行逐条对拍：状态不符 {} 行；产出差值 {} 条（有历史 {} / 首次 {} / 歧义 {} / 跳过 {}）",
            table.len(),
            mismatched,
            deltas,
            table.count_by_state()
                .get(ComparisonState::HasHistory.as_str())
                .copied()
                .unwrap_or(0),
            table.count_by_state()
                .get(ComparisonState::FirstTime.as_str())
                .copied()
                .unwrap_or(0),
            table.count_by_state()
                .get(ComparisonState::Ambiguous.as_str())
                .copied()
                .unwrap_or(0),
            table.count_by_state()
                .get(ComparisonState::Skipped.as_str())
                .copied()
                .unwrap_or(0),
        ),
    });

    // 4) 强转路径对照表（四类路径逐条断言）。
    let coercion_path = options.assets_dir.join("samples/coercion_table.csv");
    require_file(&coercion_path)?;
    let coercion = CoercionTable::load(&coercion_path).map_err(|e| SelfTestError::AssetInvalid {
        path: coercion_path.clone(),
        message: e.to_string(),
    })?;
    let coercion_mismatches = coercion.check();
    report.checks.push(SelfCheck {
        name: "强转路径对照表逐条命中".to_string(),
        passed: coercion_mismatches.is_empty(),
        detail: format!(
            "对照表 {} 行逐条断言：不符 {} 行（路径四类：UnitConvert {} / DirectScale {} / Composite {} / None {}）",
            coercion.len(),
            coercion_mismatches.len(),
            coercion.count_by_path().get("UnitConvert").copied().unwrap_or(0),
            coercion.count_by_path().get("DirectScale").copied().unwrap_or(0),
            coercion.count_by_path().get("Composite").copied().unwrap_or(0),
            coercion.count_by_path().get("None").copied().unwrap_or(0),
        ),
    });

    // 5) 负语料不误抽（院内 + 公开双来源）。
    let (neg_files, neg_mis) = scan_negatives(&options.assets_dir.join("samples/negatives"))?;
    let (pub_files, pub_mis) = scan_negatives(&options.assets_dir.join("public/negatives"))?;
    report.checks.push(SelfCheck {
        name: "负语料不误抽（双来源）".to_string(),
        passed: neg_mis == 0 && pub_mis == 0 && neg_files > 0 && pub_files > 0,
        detail: format!(
            "院内负样例 {} 条（误抽 {}）/ 公开负语料 {} 条（误抽 {}）",
            neg_files, neg_mis, pub_files, pub_mis
        ),
    });

    // 凭据要求（只有显式要求时才因缺项判失败）。
    if options.require_credentials && !missing.is_empty() {
        report.checks.push(SelfCheck {
            name: "出站凭据齐备".to_string(),
            passed: false,
            detail: format!("缺少环境变量：{}", missing.join("、")),
        });
    }

    Ok(report)
}

/// 资产目录必须在（否则明确报错，不 panic）。
fn require_dir(path: &Path) -> Result<(), SelfTestError> {
    if path.is_dir() {
        Ok(())
    } else {
        Err(SelfTestError::AssetMissing {
            path: path.to_path_buf(),
            hint: "请在项目根目录运行，或用 --assets <目录> 指定资产目录".to_string(),
        })
    }
}

/// 资产文件必须在（否则明确报错，不 panic）。
fn require_file(path: &Path) -> Result<(), SelfTestError> {
    if path.is_file() {
        Ok(())
    } else {
        Err(SelfTestError::AssetMissing {
            path: path.to_path_buf(),
            hint: "请在项目根目录运行，或用 --assets <目录> 指定资产目录".to_string(),
        })
    }
}

/// 表里某一行的行数。
fn count(repo: &Repo, table: &str) -> Result<i64, SelfTestError> {
    Ok(repo
        .connection()
        .query_row(&format!("SELECT COUNT(*) FROM {}", table), [], |row| {
            row.get(0)
        })
        .map_err(crate::state::repo::RepoError::from)?)
}

/// 失败条目数（`audit_log` 里 `failed` 动作的留痕）。
fn count_failed(repo: &Repo) -> Result<i64, SelfTestError> {
    Ok(repo
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM audit_log WHERE action = 'failed'",
            [],
            |row| row.get(0),
        )
        .map_err(crate::state::repo::RepoError::from)?)
}

/// 台账行里那条样例测值是不是 32mm / UnitConvert。
fn ledger_value_ok(repo: &Repo) -> Result<bool, SelfTestError> {
    let row: Option<(Option<f64>, Option<String>, String)> = repo
        .connection()
        .query_row(
            "SELECT value_si, unit_code, coercion_path FROM items LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(crate::state::repo::RepoError::from)?;
    Ok(match row {
        Some((value, unit, path)) => {
            value == Some(32.0) && unit.as_deref() == Some("mm") && path == "UnitConvert"
        }
        None => false,
    })
}

/// 失败条目的错误码与行列（人话）。
fn failure_detail(repo: &Repo) -> Result<String, SelfTestError> {
    let row: Option<(String, i64, i64)> = repo
        .connection()
        .query_row(
            "SELECT error_code, line, column FROM audit_log WHERE action = 'failed' LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(crate::state::repo::RepoError::from)?;
    Ok(match row {
        Some((code, line, column)) => format!("{} @ 第 {} 行第 {} 列", code, line, column),
        None => "无".to_string(),
    })
}

/// 扫一个负语料目录：返回（条数，误抽条数）。
fn scan_negatives(dir: &Path) -> Result<(usize, usize), SelfTestError> {
    if !dir.is_dir() {
        return Ok((0, 0));
    }
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|e| SelfTestError::AssetInvalid {
            path: dir.to_path_buf(),
            message: e.to_string(),
        })?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("txt"))
        .collect();
    files.sort();
    let mut mis = 0usize;
    for path in &files {
        let text = fs::read_to_string(path).map_err(|e| SelfTestError::AssetInvalid {
            path: path.clone(),
            message: e.to_string(),
        })?;
        mis += raw_parse(&text).items.len();
    }
    Ok((files.len(), mis))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assets_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets")
    }

    #[test]
    fn 自检全过且三个数字都打出来() {
        let options = SelfTestOptions {
            assets_dir: assets_dir(),
            require_credentials: false,
        };
        let report = run_selftest(&options).expect("自检应跑得起来");
        assert!(report.passed(), "自检应全过：{:#?}", report.checks);
        assert!(report.ledger_rows >= 1, "台账行数应 ≥ 1");
        assert!(report.failure_entries >= 1, "失败条目数应 ≥ 1");
        assert!(report.comparison_entries >= 1, "差值条目数应 ≥ 1");
        let text = report.render();
        assert!(text.contains("台账行数"));
        assert!(text.contains("失败条目数"));
        assert!(text.contains("差值条目数"));
        assert!(text.contains("自检结论：通过"));
    }

    #[test]
    fn 资产目录缺失时明确报错不panic() {
        let options = SelfTestOptions {
            assets_dir: PathBuf::from("/nonexistent-assets-dir-for-selftest"),
            require_credentials: false,
        };
        let err = run_selftest(&options).expect_err("资产缺失应报错");
        assert!(err.to_string().contains("找不到内置资产"));
    }
}
