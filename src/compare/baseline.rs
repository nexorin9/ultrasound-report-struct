//! 对拍引擎第 5 层：**差值基准回归集**（oracle = RIS 真实历史值）。
//!
//! **这一层守什么**：把「差值 / 方向 / 三态 / 降级」四件事钉成一张**可逐条复核的表**。
//!
//! 源产品的做法是「**拿真实系统的期望输出当 oracle 逐条对拍**」
//! （源 README：「keep the real Postgres tests as the oracle」+ `scripts/run-regression`
//! 把 4.6 万+ 条回归查询逐条比对）。本项目把 oracle **换成院内 RIS 的真实历史测量值** ——
//! 差值不是自己跟自己比，而是「报告这次的测值」对「RIS 里该患者同侧部位上次的历史值」。
//!
//! # 为什么要有这张表
//!
//! 现状是「人工抄差值 + 无对照」：医师手翻历史报告，抄一个数，心算一个差，
//! 抄错了没有任何东西会发现。这张表把**每一次对拍的口径**固定成逐条断言 ——
//! 匹配键改了、同侧规则改了、强转改了、快照语义改了，
//! 这张表立刻变红，而不是等到某份差值清单里混进一个指错对象的差值。
//!
//! # 表结构（列序固定）
//!
//! ```text
//! case_id,expect_state,patient_id,part_key,item_code,current_value,current_unit,
//! upstream_error,history,expect_delta,expect_direction,expect_hit_count,
//! expect_error_code,expect_detail_code,note
//! ```
//!
//! - `current_value` / `current_unit` 是**报告里的原值原单位**（`3.2` / `cm`）——
//!   与生产路径一致：当前值先走强转内核归一到院内口径，再与历史值相减。
//! - `history` 是**RIS 会返回的那种历史**，多条用 `;` 分隔，
//!   每条用 `|` 分隔 `检查号|患者主索引|部位|测项码|数值|单位`。
//!   部位与测项可以写**院内别名**（`左肾` / `前后径`）—— 归一后应当对上；
//!   某一格写空即「RIS 侧这一项没给」（缺值 / 缺单位 / 缺部位 / 没带患者标识）。
//! - `upstream_error` 非 `none` 时表示**这一轮没比成**（RIS 够不着），
//!   对拍引擎走降级路径，**不产差值**。
//! - **空位一律写 `none`**（不写空串、不写 `-`）——
//!   少一个逗号就会串列，串列比没有表更糟。
//!
//! # 四类状态都要有行
//!
//! | 期望状态 | 含义 | 差值 |
//! |---|---|---|
//! | `has_history` | 恰好命中一条历史 | 有 |
//! | `first_time` | 命中 0 条（首次检查） | **无**（首次不是失败） |
//! | `ambiguous` | 命中多条，差值无法唯一确定 | **无**（绝不任取其一） |
//! | `comparison_skipped` | 本轮没比成（RIS 够不着） | **无**（不等于无差异） |
//!
//! # 失败行报告
//!
//! 任一行对不上即产出 [`Mismatch`]，报告带**行号 / 输入 / 期望 / 实际**四列 ——
//! 运维拿到报告能直接定位到哪一行、哪一个字段的口径漂了。

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use super::r#match::Direction;
use super::ris_client::{RisError, RisErrorKind, RisReport, RisReportMeasurement};
use super::round::ComparisonRound;
use super::states::ComparisonState;
use crate::coerce::path::decide;
use crate::parse::errors::{DetailCode, ErrorCode};
use crate::parse::extract::ParsedItem;

/// 基准表表头（列序固定）。
pub const HEADER: [&str; 15] = [
    "case_id",
    "expect_state",
    "patient_id",
    "part_key",
    "item_code",
    "current_value",
    "current_unit",
    "upstream_error",
    "history",
    "expect_delta",
    "expect_direction",
    "expect_hit_count",
    "expect_error_code",
    "expect_detail_code",
    "note",
];

/// 基准表最少行数（低于这个数就谈不上「基准集」）。
pub const MIN_ROWS: usize = 20;

/// 数值比较容差（差值保留 3 位小数）。
const VALUE_TOLERANCE: f64 = 1e-9;

/// **空位标记**：可选字段一律写 `none`，不写空串、不写 `-`。
const NONE_MARKER: &str = "none";

/// **固定取数时点** —— 基准回归必须可复现，不能跟着墙钟走。
const FIXED_TAKEN_AT: u64 = 1_700_000_000;

/// 一条历史测值（基准表 `history` 列里的一格）。
#[derive(Clone, Debug, PartialEq)]
pub struct HistoryEntry {
    /// 历史报告检查号。
    pub study_no: String,
    /// 患者主索引（RIS 侧没给时为 `None` —— 空的患者标识不构成跨患者比对）。
    pub patient_id: Option<String>,
    /// 部位（可以是院内别名；RIS 侧没给时为 `None`）。
    pub body_part: Option<String>,
    /// 测项码（可以是测项词面；RIS 侧没给时为 `None`）。
    pub item_code: Option<String>,
    /// 测值（RIS 侧没给时为 `None`）。
    pub value: Option<f64>,
    /// 原单位（RIS 侧没给时为 `None`）。
    pub unit: Option<String>,
}

impl HistoryEntry {
    /// 转成 RIS 历史报告（**与出站读解析出来的结构一致**）。
    pub fn to_report(&self) -> RisReport {
        RisReport {
            study_no: self.study_no.clone(),
            patient_id: self.patient_id.clone(),
            body_part: self.body_part.clone(),
            performed_at: Some("2026-01-08".to_string()),
            measurements: vec![RisReportMeasurement {
                item_code: self.item_code.clone().unwrap_or_default(),
                value: self.value,
                unit: self.unit.clone(),
            }],
        }
    }

    /// 这一格的可读形式（进失败报告的「输入」列）。
    pub fn label(&self) -> String {
        format!(
            "{}|{}|{}|{}|{}",
            self.study_no,
            self.patient_id.clone().unwrap_or_else(|| "-".to_string()),
            self.body_part.clone().unwrap_or_else(|| "-".to_string()),
            self.item_code.clone().unwrap_or_else(|| "-".to_string()),
            self.value
                .map(|v| format!("{}", v))
                .unwrap_or_else(|| "-".to_string()),
        )
    }
}

/// 基准表的一行（一组对拍场景）。
#[derive(Clone, Debug, PartialEq)]
pub struct BaselineCase {
    /// **行号**（1 基，按 CSV 物理行号记 —— 报告里就是给人看的定位）。
    pub line_no: usize,
    /// 案例号（表内唯一，便于口头指认）。
    pub case_id: String,
    /// 期望对拍状态。
    pub expect_state: ComparisonState,
    /// 患者主索引。
    pub patient_id: String,
    /// 规范部位键。
    pub part_key: String,
    /// 规范测项码。
    pub item_code: String,
    /// 当前测值（**报告原值**）。
    pub current_value: f64,
    /// 当前测值原单位。
    pub current_unit: String,
    /// 上游失败语义（`Some` = 这一轮没比成）。
    pub upstream_error: Option<RisErrorKind>,
    /// RIS 历史（一条或多条）。
    pub history: Vec<HistoryEntry>,
    /// 期望差值（只有 `has_history` 有）。
    pub expect_delta: Option<f64>,
    /// 期望方向（只有 `has_history` 有）。
    pub expect_direction: Option<Direction>,
    /// 期望命中条数。
    pub expect_hit_count: usize,
    /// 期望失败码（`first_time` 无 —— 首次不是失败）。
    pub expect_error_code: Option<ErrorCode>,
    /// 期望明细归类。
    pub expect_detail_code: Option<DetailCode>,
    /// 这一行在讲什么（口径说明）。
    pub note: String,
}

impl BaselineCase {
    /// 造当前测值条目（**原值原单位进 强转内核归一** —— 与生产路径同源）。
    fn current_item(&self) -> ParsedItem {
        let decision = decide(self.current_value, None, &self.current_unit);
        ParsedItem {
            part_key: Some(self.part_key.clone()),
            item_code: Some(self.item_code.clone()),
            value_raw: Some(self.current_value),
            unit_raw: Some(self.current_unit.clone()),
            value_si: decision.value_si,
            unit_code: decision.unit_code.clone(),
            coercion_path: decision.path,
            // 基准表不建模报告原文位置 —— 位置未知就是 0，不编造。
            line: 0,
            column: 0,
            char_start: 0,
            notes: Vec::new(),
            dictionary_version: crate::dictionary_version(),
        }
    }

    /// RIS 历史报告列表（转成出站读同构的结构）。
    fn history_reports(&self) -> Vec<RisReport> {
        self.history.iter().map(|h| h.to_report()).collect()
    }

    /// **驱动一次对拍**（走真实主路径：条目 → 强转 → 匹配 → 三态判定 / 降级）。
    pub fn drive(&self) -> BaselineOutcome {
        let item = self.current_item();
        let round = match self.upstream_error {
            Some(kind) => ComparisonRound::from_ris_error(&ris_error_for(kind)),
            None => ComparisonRound::from_reports_at(self.history_reports(), FIXED_TAKEN_AT),
        };
        BaselineOutcome::from_verdict(round.compare_item(&self.patient_id, &item))
    }

    /// 这一行在报告里的可读输入。
    pub fn input_label(&self) -> String {
        let current = format!("{} {}", self.current_value, self.current_unit);
        if self.history.is_empty() {
            return current;
        }
        let history = self
            .history
            .iter()
            .map(|h| h.label())
            .collect::<Vec<_>>()
            .join(" ; ");
        format!("当前 [{}] vs 历史 [{}]", current, history)
    }
}

/// 上游失败语义 → 出站读失败（**明细归类固定**，便于运维按码聚合）。
fn ris_error_for(kind: RisErrorKind) -> RisError {
    let detail = match kind {
        RisErrorKind::UpstreamUnavailable => Some(DetailCode::RisTimeout),
        RisErrorKind::AuthFailed => Some(DetailCode::RisUnauthorized),
        RisErrorKind::BadResponse => Some(DetailCode::RisBodyNotJson),
        RisErrorKind::NotConfigured => None,
    };
    RisError::new(kind, detail, format!("基准表指定的 RIS 失败：{}", kind.as_str()))
}

/// **一次对拍的结论**（基准表比对用的结构化结果）。
#[derive(Clone, Debug, PartialEq)]
pub struct BaselineOutcome {
    /// 对拍状态（`None` = 判不出结论，例如当前测值本身不可用）。
    pub state: Option<ComparisonState>,
    /// 差值（只有 `has_history` 有）。
    pub delta: Option<f64>,
    /// 方向（只有 `has_history` 有）。
    pub direction: Option<Direction>,
    /// 命中条数。
    pub hit_count: usize,
    /// 失败码。
    pub error_code: Option<ErrorCode>,
    /// 明细归类。
    pub detail_code: Option<DetailCode>,
}

impl BaselineOutcome {
    /// 从对拍结论取基准结果。
    pub fn from_verdict(verdict: Option<super::states::ComparisonVerdict>) -> BaselineOutcome {
        match verdict {
            None => BaselineOutcome {
                state: None,
                delta: None,
                direction: None,
                hit_count: 0,
                error_code: None,
                detail_code: None,
            },
            Some(v) => BaselineOutcome {
                state: Some(v.state),
                delta: v.delta.as_ref().map(|d| d.delta),
                direction: v.delta.as_ref().map(|d| d.direction),
                hit_count: v.hit_count,
                error_code: v.failure.as_ref().map(|f| f.code),
                detail_code: v.failure.as_ref().and_then(|f| f.detail_code()),
            },
        }
    }
}

/// 一行不匹配的详情。
#[derive(Clone, Debug, PartialEq)]
pub struct Mismatch {
    /// 行号（1 基）。
    pub line_no: usize,
    /// 案例号。
    pub case_id: String,
    /// 输入（当前测值 + 历史）。
    pub input: String,
    /// 哪些字段对不上（`状态` / `差值` / `方向` / `命中条数` / `失败码` / `明细码`）。
    pub reasons: Vec<&'static str>,
    /// 期望。
    pub expected: String,
    /// 实际。
    pub actual: String,
    /// 这一行在讲什么。
    pub note: String,
}

impl fmt::Display for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "第 {} 行 · {} · 输入 {}",
            self.line_no, self.case_id, self.input
        )?;
        write!(f, "  不符：{}", self.reasons.join("、"))?;
        if !self.note.is_empty() {
            write!(f, "（{}）", self.note)?;
        }
        writeln!(f)?;
        writeln!(f, "  期望：{}", self.expected)?;
        write!(f, "  实际：{}", self.actual)
    }
}

/// 基准表加载 / 校验出错。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BaselineError {
    /// 表头与约定列序不一致。
    Header(String),
    /// 某一行不合法（带行号）。
    Row { line_no: usize, message: String },
    /// 文件读不到。
    Io(String),
}

impl fmt::Display for BaselineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BaselineError::Header(msg) => write!(f, "基准表表头不对：{}", msg),
            BaselineError::Row { line_no, message } => {
                write!(f, "基准表第 {} 行不合法：{}", line_no, message)
            }
            BaselineError::Io(msg) => write!(f, "基准表读不到：{}", msg),
        }
    }
}

impl std::error::Error for BaselineError {}

/// **差值基准回归集**。
#[derive(Clone, Debug, Default)]
pub struct BaselineTable {
    rows: Vec<BaselineCase>,
}

impl BaselineTable {
    /// 从 CSV 文本解析基准表。
    ///
    /// **表头必须与 [`HEADER`] 一致**（列序固定）—— 列序错位会让人按列号读错值，
    /// 那比没有基准表更糟。
    pub fn parse(csv: &str) -> Result<BaselineTable, BaselineError> {
        let mut lines = csv.lines().enumerate();
        let (_, header) = lines
            .next()
            .ok_or_else(|| BaselineError::Header("表是空的".to_string()))?;
        let head: Vec<&str> = header.split(',').map(|s| s.trim()).collect();
        if head != HEADER {
            return Err(BaselineError::Header(format!(
                "期望 [{}]，实际 [{}]",
                HEADER.join(", "),
                head.join(", ")
            )));
        }

        let mut rows = Vec::new();
        for (idx, line) in lines {
            let line_no = idx + 1;
            if line.trim().is_empty() {
                continue;
            }
            let f: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
            if f.len() != HEADER.len() {
                return Err(BaselineError::Row {
                    line_no,
                    message: format!(
                        "列数不对：期望 {} 列，实际 {} 列",
                        HEADER.len(),
                        f.len()
                    ),
                });
            }
            rows.push(parse_case(line_no, &f)?);
        }
        Ok(BaselineTable { rows })
    }

    /// 从文件读取基准表。
    pub fn load(path: &Path) -> Result<BaselineTable, BaselineError> {
        let csv = std::fs::read_to_string(path)
            .map_err(|e| BaselineError::Io(format!("{}：{}", path.display(), e)))?;
        BaselineTable::parse(&csv)
    }

    /// 全部行。
    pub fn rows(&self) -> &[BaselineCase] {
        &self.rows
    }

    /// 行数。
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// 表是否为空。
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// 按对拍状态统计行数（四类状态的覆盖度一眼可见）。
    pub fn count_by_state(&self) -> BTreeMap<&'static str, usize> {
        let mut out: BTreeMap<&'static str, usize> = BTreeMap::new();
        for state in ComparisonState::ALL {
            out.insert(state.as_str(), 0);
        }
        for row in &self.rows {
            *out.entry(row.expect_state.as_str()).or_insert(0) += 1;
        }
        out
    }

    /// 逐行驱动对拍引擎，返回**全部不匹配的行**（空 = 全部命中）。
    pub fn check(&self) -> Vec<Mismatch> {
        self.rows.iter().filter_map(check_case).collect()
    }

    /// 逐行驱动对拍引擎；**任一行不匹配即返回带完整报告的 `Err`**。
    pub fn assert_all(&self) -> Result<(), String> {
        let mismatches = self.check();
        if mismatches.is_empty() {
            return Ok(());
        }
        Err(report(&mismatches, self.len()))
    }
}

/// 解析一行（列序与表头一致）。
fn parse_case(line_no: usize, f: &[&str]) -> Result<BaselineCase, BaselineError> {
    let row_err = |message: String| BaselineError::Row { line_no, message };

    let expect_state = ComparisonState::lookup(f[1]).ok_or_else(|| {
        row_err(format!(
            "未知对拍状态 {:?}（合法值：{}）",
            f[1],
            ComparisonState::ALL
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(" / ")
        ))
    })?;
    let current_value = f[5].parse::<f64>().map_err(|_| {
        row_err(format!("当前测值 {:?} 不是数值", f[5]))
    })?;
    if f[6].is_empty() {
        return Err(row_err("当前测值缺单位".to_string()));
    }
    let upstream_error = match f[7] {
        NONE_MARKER => None,
        raw => Some(RisErrorKind::ALL.iter().copied().find(|k| k.as_str() == raw).ok_or_else(
            || {
                row_err(format!(
                    "未知上游失败语义 {:?}（合法值：{}）",
                    raw,
                    RisErrorKind::ALL
                        .iter()
                        .map(|k| k.as_str())
                        .collect::<Vec<_>>()
                        .join(" / ")
                ))
            },
        )?),
    };
    let history = parse_history(f[8], line_no)?;
    let expect_delta = match f[9] {
        NONE_MARKER => None,
        raw => Some(raw.parse::<f64>().map_err(|_| {
            row_err(format!("期望差值 {:?} 不是数值", raw))
        })?),
    };
    let expect_direction = match f[10] {
        NONE_MARKER => None,
        raw => Some(parse_direction(raw).ok_or_else(|| {
            row_err(format!("未知期望方向 {:?}（合法值：增大 / 减小 / 持平）", raw))
        })?),
    };
    let expect_hit_count = f[11].parse::<usize>().map_err(|_| {
        row_err(format!("期望命中条数 {:?} 不是整数", f[11]))
    })?;
    let expect_error_code = match f[12] {
        NONE_MARKER => None,
        raw => Some(ErrorCode::lookup(raw).ok_or_else(|| {
            row_err(format!("未知期望失败码 {:?}（合法值：{})", raw, error_code_list()))
        })?),
    };
    let expect_detail_code = match f[13] {
        NONE_MARKER => None,
        raw => Some(DetailCode::lookup(raw).ok_or_else(|| {
            row_err(format!("未知期望明细码 {:?}", raw))
        })?),
    };

    let case = BaselineCase {
        line_no,
        case_id: f[0].to_string(),
        expect_state,
        patient_id: f[2].to_string(),
        part_key: f[3].to_string(),
        item_code: f[4].to_string(),
        current_value,
        current_unit: f[6].to_string(),
        upstream_error,
        history,
        expect_delta,
        expect_direction,
        expect_hit_count,
        expect_error_code,
        expect_detail_code,
        note: f[14].to_string(),
    };

    check_case_consistency(&case).map_err(row_err)?;
    Ok(case)
}

/// 解析 `history` 列（`;` 分条、`|` 分格）。
fn parse_history(raw: &str, line_no: usize) -> Result<Vec<HistoryEntry>, BaselineError> {
    let row_err = |message: String| BaselineError::Row { line_no, message };
    if raw == NONE_MARKER {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for chunk in raw.split(';') {
        let chunk = chunk.trim();
        if chunk.is_empty() {
            continue;
        }
        let cells: Vec<&str> = chunk.split('|').collect();
        if cells.len() != 6 {
            return Err(row_err(format!(
                "历史条目列数不对：期望 6 列（检查号|患者|部位|测项码|数值|单位），实际 {} 列",
                cells.len()
            )));
        }
        let value = match cells[4] {
            "" => None,
            v => Some(v.parse::<f64>().map_err(|_| {
                row_err(format!("历史测值 {:?} 不是数值", v))
            })?),
        };
        out.push(HistoryEntry {
            study_no: cells[0].to_string(),
            patient_id: non_empty(cells[1]),
            body_part: non_empty(cells[2]),
            item_code: non_empty(cells[3]),
            value,
            unit: non_empty(cells[5]),
        });
    }
    Ok(out)
}

/// 空串 → `None`（RIS 侧没给这一项）。
fn non_empty(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// 方向字符串 → [`Direction`]（中文口径）。
fn parse_direction(raw: &str) -> Option<Direction> {
    match raw {
        "增大" => Some(Direction::Increased),
        "减小" => Some(Direction::Decreased),
        "持平" => Some(Direction::Unchanged),
        _ => None,
    }
}

/// 全部失败码（错误提示里列全，省得运维自己去查码表）。
fn error_code_list() -> String {
    ErrorCode::ALL
        .iter()
        .map(|c| c.as_str())
        .collect::<Vec<_>>()
        .join(" / ")
}

/// **表内自洽**：期望值必须与期望状态对得上。
///
/// 表写错了（比如给 `first_time` 行填了期望差值）就是**数据不对**，
/// 不是「跑不通」—— 硬跑下去只会把表里的错当成对的。
fn check_case_consistency(case: &BaselineCase) -> Result<(), String> {
    let state = case.expect_state;
    let want_delta = state == ComparisonState::HasHistory;
    if want_delta && case.expect_delta.is_none() {
        return Err("状态是有历史 但没写期望差值".to_string());
    }
    if !want_delta && case.expect_delta.is_some() {
        return Err(format!(
            "状态是 {} 不该有期望差值",
            state.as_str()
        ));
    }
    if want_delta && case.expect_direction.is_none() {
        return Err("状态是有历史 但没写期望方向".to_string());
    }
    if !want_delta && case.expect_direction.is_some() {
        return Err(format!("状态是 {} 不该有期望方向", state.as_str()));
    }
    match state {
        ComparisonState::FirstTime => {
            if case.expect_error_code.is_some() {
                return Err("首次检查不该带失败码（首次不是失败）".to_string());
            }
            if case.expect_hit_count != 0 {
                return Err("首次检查的命中条数必须是 0".to_string());
            }
        }
        ComparisonState::Ambiguous => {
            if case.expect_error_code != Some(ErrorCode::ComparisonAmbiguous) {
                return Err("歧义行的失败码必须是 COMPARISON_AMBIGUOUS".to_string());
            }
            if case.expect_hit_count < 2 {
                return Err("歧义行的命中条数必须 ≥ 2".to_string());
            }
        }
        ComparisonState::Skipped => {
            if case.expect_error_code.is_none() {
                return Err("跳过行必须写期望失败码（RIS 够不着要有码）".to_string());
            }
            if case.expect_hit_count != 0 {
                return Err("跳过行的命中条数必须是 0".to_string());
            }
            if case.upstream_error.is_none() {
                return Err("跳过行必须写上游失败语义".to_string());
            }
        }
        ComparisonState::HasHistory => {
            if case.expect_hit_count != 1 {
                return Err("有历史行的命中条数必须是 1".to_string());
            }
            if case.expect_error_code.is_some() {
                return Err("有历史不是失败 不该带失败码".to_string());
            }
        }
    }
    Ok(())
}

/// 校验一行；命中返回 `None`，不匹配返回 [`Mismatch`]。
pub fn check_case(case: &BaselineCase) -> Option<Mismatch> {
    let outcome = case.drive();
    let mut reasons: Vec<&'static str> = Vec::new();

    let actual_state = outcome.state.map(|s| s.as_str());
    if actual_state != Some(case.expect_state.as_str()) {
        reasons.push("状态");
    }
    if !value_matches(outcome.delta, case.expect_delta) {
        reasons.push("差值");
    }
    let actual_direction = outcome.direction.map(|d| d.as_str());
    let expect_direction = case.expect_direction.map(|d| d.as_str());
    if actual_direction != expect_direction {
        reasons.push("方向");
    }
    if outcome.hit_count != case.expect_hit_count {
        reasons.push("命中条数");
    }
    if outcome.error_code != case.expect_error_code {
        reasons.push("失败码");
    }
    if outcome.detail_code != case.expect_detail_code {
        reasons.push("明细码");
    }
    if reasons.is_empty() {
        return None;
    }

    Some(Mismatch {
        line_no: case.line_no,
        case_id: case.case_id.clone(),
        input: case.input_label(),
        reasons,
        expected: format!(
            "状态={} 差值={} 方向={} 命中={} 失败码={} 明细码={}",
            case.expect_state.as_str(),
            render_value(case.expect_delta),
            case.expect_direction.map(|d| d.as_str()).unwrap_or("—"),
            case.expect_hit_count,
            case.expect_error_code.map(|c| c.as_str()).unwrap_or("—"),
            case.expect_detail_code.map(|c| c.as_str()).unwrap_or("—"),
        ),
        actual: format!(
            "状态={} 差值={} 方向={} 命中={} 失败码={} 明细码={}",
            actual_state.unwrap_or("—（判不出结论）"),
            render_value(outcome.delta),
            actual_direction.unwrap_or("—"),
            outcome.hit_count,
            outcome.error_code.map(|c| c.as_str()).unwrap_or("—"),
            outcome.detail_code.map(|c| c.as_str()).unwrap_or("—"),
        ),
        note: case.note.clone(),
    })
}

/// 差值比较（两侧都空 = 一致；一侧空 = 不一致；都有值按容差比）。
fn value_matches(actual: Option<f64>, expected: Option<f64>) -> bool {
    match (actual, expected) {
        (None, None) => true,
        (Some(a), Some(e)) => (a - e).abs() <= VALUE_TOLERANCE,
        _ => false,
    }
}

/// 差值的可读形式（空 = 「未产出差值」，**不用 0 冒充**）。
fn render_value(v: Option<f64>) -> String {
    match v {
        Some(x) => format!("{}", x),
        None => "—（未产出差值）".to_string(),
    }
}

/// 不匹配行的完整报告（行号 / 输入 / 期望 / 实际 四列齐全）。
pub fn report(mismatches: &[Mismatch], total_rows: usize) -> String {
    let mut out = format!(
        "差值基准回归集：共 {} 行，{} 行不符\n",
        total_rows,
        mismatches.len()
    );
    for m in mismatches {
        out.push_str(&format!("{}\n", m));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "case_id,expect_state,patient_id,part_key,item_code,current_value,current_unit,upstream_error,history,expect_delta,expect_direction,expect_hit_count,expect_error_code,expect_detail_code,note\n\
CB-T01,has_history,PID-EXAMPLE,kidney.left,kidney.ap_diameter,3.2,cm,none,S-HIST-001|PID-EXAMPLE|kidney.left|kidney.ap_diameter|28|mm,4,增大,1,none,none,样例：有历史\n\
CB-T02,first_time,PID-EXAMPLE,kidney.left,kidney.ap_diameter,3.2,cm,none,none,none,none,0,none,none,样例：首次\n";

    fn table() -> BaselineTable {
        BaselineTable::parse(SAMPLE).expect("样例表应能解析")
    }

    #[test]
    fn 解析表头与数据行() {
        let t = table();
        assert_eq!(t.len(), 2);
        let first = &t.rows()[0];
        assert_eq!(first.line_no, 2, "行号按 CSV 物理行号记（1 基，含表头）");
        assert_eq!(first.case_id, "CB-T01");
        assert_eq!(first.expect_state, ComparisonState::HasHistory);
        assert_eq!(first.expect_delta, Some(4.0));
        assert_eq!(first.expect_direction, Some(Direction::Increased));
        assert_eq!(first.expect_hit_count, 1);
        assert_eq!(first.history.len(), 1);
        assert_eq!(first.history[0].study_no, "S-HIST-001");
    }

    #[test]
    fn 全部命中时不报不符() {
        let t = table();
        let mismatches = t.check();
        assert!(
            mismatches.is_empty(),
            "样例表应全部命中，实际不符：{}",
            report(&mismatches, t.len())
        );
        assert!(t.assert_all().is_ok());
    }

    #[test]
    fn 不匹配时报告含行号输入期望与实际() {
        let csv = "case_id,expect_state,patient_id,part_key,item_code,current_value,current_unit,upstream_error,history,expect_delta,expect_direction,expect_hit_count,expect_error_code,expect_detail_code,note\n\
CB-T01,has_history,PID-EXAMPLE,kidney.left,kidney.ap_diameter,3.2,cm,none,S-HIST-001|PID-EXAMPLE|kidney.left|kidney.ap_diameter|28|mm,99,增大,1,none,none,故意写错的期望差值\n";
        let t = BaselineTable::parse(csv).expect("应能解析");
        let mismatches = t.check();
        assert_eq!(mismatches.len(), 1, "只有写错的那一行不符");
        let m = &mismatches[0];
        assert_eq!(m.line_no, 2);
        assert!(m.reasons.contains(&"差值"));
        assert!(m.expected.contains("99"));
        assert!(m.actual.contains("4"));

        let text = report(&mismatches, t.len());
        assert!(text.contains("第 2 行"), "报告须含行号：{}", text);
        assert!(text.contains("期望"), "报告须含期望列：{}", text);
        assert!(text.contains("实际"), "报告须含实际列：{}", text);
        assert!(text.contains("共 1 行，1 行不符"));

        let err = t.assert_all().expect_err("不匹配应返回 Err");
        assert!(err.contains("第 2 行"));
    }

    #[test]
    fn 状态错了也报出来() {
        let csv = "case_id,expect_state,patient_id,part_key,item_code,current_value,current_unit,upstream_error,history,expect_delta,expect_direction,expect_hit_count,expect_error_code,expect_detail_code,note\n\
CB-T01,first_time,PID-EXAMPLE,kidney.left,kidney.ap_diameter,3.2,cm,none,S-HIST-001|PID-EXAMPLE|kidney.left|kidney.ap_diameter|28|mm,none,none,0,none,none,状态写错\n";
        let t = BaselineTable::parse(csv).expect("应能解析");
        let mismatches = t.check();
        assert_eq!(mismatches.len(), 1);
        assert!(mismatches[0].reasons.contains(&"状态"));
        assert!(mismatches[0].expected.contains("first_time"));
        assert!(mismatches[0].actual.contains("has_history"));
    }

    #[test]
    fn 表头不对报错() {
        let bad = "case_id,expect_state\nCB-T01,has_history\n";
        let err = BaselineTable::parse(bad).expect_err("列数不对的表头应报错");
        assert!(matches!(err, BaselineError::Header(_)));
    }

    #[test]
    fn 列数不对报错且带行号() {
        let csv = "case_id,expect_state,patient_id,part_key,item_code,current_value,current_unit,upstream_error,history,expect_delta,expect_direction,expect_hit_count,expect_error_code,expect_detail_code,note\n\
CB-T01,has_history,PID-EXAMPLE,kidney.left\n";
        let err = BaselineTable::parse(csv).expect_err("少一列应报错");
        match err {
            BaselineError::Row { line_no, message } => {
                assert_eq!(line_no, 2);
                assert!(message.contains("列数不对"));
            }
            other => panic!("应报行错误，实际 {:?}", other),
        }
    }

    #[test]
    fn 未知状态码报错不套默认状态() {
        let csv = "case_id,expect_state,patient_id,part_key,item_code,current_value,current_unit,upstream_error,history,expect_delta,expect_direction,expect_hit_count,expect_error_code,expect_detail_code,note\n\
CB-T01,whatever,PID-EXAMPLE,kidney.left,kidney.ap_diameter,3.2,cm,none,none,none,none,0,none,none,\n";
        let err = BaselineTable::parse(csv).expect_err("表外状态码应报错");
        match err {
            BaselineError::Row { line_no, message } => {
                assert_eq!(line_no, 2);
                assert!(message.contains("未知对拍状态"));
            }
            other => panic!("应报行错误，实际 {:?}", other),
        }
    }

    #[test]
    fn 首次行带失败码报错() {
        // 首次不是失败 —— 表里写错就该在加载时报出来。
        let csv = "case_id,expect_state,patient_id,part_key,item_code,current_value,current_unit,upstream_error,history,expect_delta,expect_direction,expect_hit_count,expect_error_code,expect_detail_code,note\n\
CB-T01,first_time,PID-EXAMPLE,kidney.left,kidney.ap_diameter,3.2,cm,none,none,none,none,0,COMPARISON_AMBIGUOUS,none,首次不该带失败码\n";
        let err = BaselineTable::parse(csv).expect_err("首次带失败码应报错");
        match err {
            BaselineError::Row { message, .. } => {
                assert!(message.contains("首次检查不该带失败码"));
            }
            other => panic!("应报行错误，实际 {:?}", other),
        }
    }

    #[test]
    fn 空行跳过但行号不跳号() {
        let csv = "case_id,expect_state,patient_id,part_key,item_code,current_value,current_unit,upstream_error,history,expect_delta,expect_direction,expect_hit_count,expect_error_code,expect_detail_code,note\n\
CB-T01,first_time,PID-EXAMPLE,kidney.left,kidney.ap_diameter,3.2,cm,none,none,none,none,0,none,none,\n\
\n\
CB-T02,first_time,PID-EXAMPLE,kidney.left,kidney.ap_diameter,3.2,cm,none,none,none,none,0,none,none,\n";
        let t = BaselineTable::parse(csv).expect("应能解析");
        assert_eq!(t.len(), 2);
        assert_eq!(t.rows()[1].line_no, 4, "空行不占数据行，但行号按物理行");
    }

    #[test]
    fn 按状态统计覆盖度() {
        let t = table();
        let counts = t.count_by_state();
        assert_eq!(counts.get("has_history"), Some(&1));
        assert_eq!(counts.get("first_time"), Some(&1));
        // 空表也有全部状态的键，便于断言覆盖度。
        assert_eq!(counts.len(), ComparisonState::ALL.len());
    }

    #[test]
    fn 上游失败语义映射到稳定码() {
        // 降级路径的码由上游语义决定，基准表不许自己写死别的码。
        let err = ris_error_for(RisErrorKind::UpstreamUnavailable);
        assert_eq!(err.error_code(), ErrorCode::RisUpstreamUnavailable);
        assert_eq!(err.detail, Some(DetailCode::RisTimeout));
        assert_eq!(ris_error_for(RisErrorKind::AuthFailed).error_code(), ErrorCode::RisAuthFailed);
        assert_eq!(
            ris_error_for(RisErrorKind::BadResponse).error_code(),
            ErrorCode::RisBadResponse
        );
        assert_eq!(
            ris_error_for(RisErrorKind::NotConfigured).error_code(),
            ErrorCode::RisNotConfigured
        );
    }
}
