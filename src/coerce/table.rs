//! 强转内核：**强转路径对照表**（口径的「可复现对比结果」）。
//!
//! 这张表是本项目**核心差异化模块的判据**：每一行给出
//! 「报告原值词面 + 原单位词面 → 期望强转路径 + 期望归一值 + 期望单位码」，
//! 逐行驱动判定内核（[`super::path::coerce_raw`]），
//! **任一行对不上就是口径漂了** —— 不是「跑不通」，是「跑通但算错了」。
//!
//! # 为什么要有这张表
//!
//! 现状是「人工换算 + 无对照」：报告写 `3.2cm`，有人手抄成 `32mm`，
//! 有人抄成 `3.2`，抄错了没有任何东西会发现。对照表把**每一次换算的口径**
//! 固定成可逐条复核的断言 —— 换算规则改了、词典改了、单位目录改了，
//! 这张表立刻变红，而不是等到某份台账里混进一个 10 倍差。
//!
//! # 表结构（列序固定）
//!
//! ```text
//! value_raw,unit_raw,expected_path,expected_value_si,expected_unit_code,note
//! 3.2,cm,UnitConvert,32,mm,长度族：厘米→毫米
//! 3.2×2,cm,DirectScale,64,mm,原值词面自带倍率
//! 3.2,cm3,Composite,3.2,mL,复合量纲：量纲还原 + 单位换算两步
//! 3.2,xx,None,,,未知单位：查不到合法路径，不产出数值
//! ```
//!
//! - `value_raw` 是**报告原值词面**（`3.2` / `3.2×2`）—— 倍率是原文写明的，
//!   所以驱动入口是 [`super::path::coerce_raw`] 而不是 [`super::path::decide`]：
//!   倍率只从词面里读，**不猜倍率**。
//! - `expected_path` 取 [`super::path::CoercionPath`] 的四个值之一；
//!   表里出现表外的路径码 → **数据不对**，显式报错，不硬套一个默认路径。
//! - `expected_value_si` / `expected_unit_code` 在 `None` 行**留空** ——
//!   查不到合法路径就**不产出数值**（内核 fail-closed）。
//!
//! # 失败行报告
//!
//! 任一行不匹配即产出 [`Mismatch`]，报告里带**行号 / 输入 / 期望 / 实际**四列
//! —— 运维拿到报告能直接定位到哪一行、哪一列口径不对，不必自己推。

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use super::path::{coerce_raw, CoercionDecision, CoercionPath};

/// 对照表表头（列序固定）。
pub const HEADER: [&str; 6] = [
    "value_raw",
    "unit_raw",
    "expected_path",
    "expected_value_si",
    "expected_unit_code",
    "note",
];

/// 数值比较容差（对照表按 3 位小数记；判定内核也保留 3 位小数）。
const VALUE_TOLERANCE: f64 = 1e-9;

/// 对照表的一行。
#[derive(Clone, Debug, PartialEq)]
pub struct TableRow {
    /// **行号**（1 基，按 CSV 物理行号记 —— 报告里就是给人看的定位）。
    pub line_no: usize,
    /// 报告原值词面（`3.2` / `3.2×2`）。
    pub value_raw: String,
    /// 报告原单位词面（`cm` / `厘米` / `cm3` / `xx`）。
    pub unit_raw: String,
    /// 期望强转路径。
    pub expected_path: CoercionPath,
    /// 期望归一值（`None` 行留空）。
    pub expected_value_si: Option<f64>,
    /// 期望单位码（`None` 行留空）。
    pub expected_unit_code: Option<String>,
    /// 这一行在讲什么（口径说明，失败报告里一并打印）。
    pub note: String,
}

impl TableRow {
    /// 用这一行驱动一次判定（**与抽取层同一入口** —— 判定与换算同源）。
    pub fn drive(&self) -> CoercionDecision {
        coerce_raw(&self.value_raw, &self.unit_raw)
    }

    /// 这一行在报告里的可读输入（`3.2 cm`）。
    pub fn input_label(&self) -> String {
        format!("{} {}", self.value_raw, self.unit_raw)
    }
}

/// 一行不匹配的详情。
#[derive(Clone, Debug, PartialEq)]
pub struct Mismatch {
    /// 行号（1 基）。
    pub line_no: usize,
    /// 输入（`原值词面 + 原单位词面`）。
    pub input: String,
    /// 哪些字段对不上（`路径` / `归一值` / `单位码`）。
    pub reasons: Vec<&'static str>,
    /// 期望（路径 / 归一值 / 单位码 三件套）。
    pub expected: String,
    /// 实际（路径 / 归一值 / 单位码 三件套）。
    pub actual: String,
    /// 这一行在讲什么。
    pub note: String,
}

impl fmt::Display for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "第 {} 行 · 输入 {}", self.line_no, self.input)?;
        write!(f, "  不符：{}", self.reasons.join("、"))?;
        if !self.note.is_empty() {
            write!(f, "（{}）", self.note)?;
        }
        writeln!(f)?;
        writeln!(f, "  期望：{}", self.expected)?;
        write!(f, "  实际：{}", self.actual)
    }
}

/// 对照表加载 / 校验出错。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TableError {
    /// 表头与约定列序不一致。
    Header(String),
    /// 某一行不合法（带行号）。
    Row { line_no: usize, message: String },
    /// 文件读不到。
    Io(String),
}

impl fmt::Display for TableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TableError::Header(msg) => write!(f, "对照表表头不对：{}", msg),
            TableError::Row { line_no, message } => write!(f, "对照表第 {} 行不合法：{}", line_no, message),
            TableError::Io(msg) => write!(f, "对照表读不到：{}", msg),
        }
    }
}

impl std::error::Error for TableError {}

/// 强转路径对照表。
#[derive(Clone, Debug, Default)]
pub struct CoercionTable {
    rows: Vec<TableRow>,
}

impl CoercionTable {
    /// 从 CSV 文本解析对照表。
    ///
    /// **表头必须与 [`HEADER`] 一致**（列序固定）—— 列序错位会让人按列号读错值，
    /// 那比没有对照表更糟（Task 22 在公开码表上就撞到过列数错位）。
    pub fn parse(csv: &str) -> Result<CoercionTable, TableError> {
        let mut lines = csv.lines().enumerate();
        let (_, header) = lines
            .next()
            .ok_or_else(|| TableError::Header("表是空的".to_string()))?;
        let head: Vec<&str> = header.split(',').map(|s| s.trim()).collect();
        if head != HEADER {
            return Err(TableError::Header(format!(
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
                return Err(TableError::Row {
                    line_no,
                    message: format!(
                        "列数不对：期望 {} 列，实际 {} 列",
                        HEADER.len(),
                        f.len()
                    ),
                });
            }
            let expected_path = CoercionPath::from_code(f[2]).ok_or_else(|| TableError::Row {
                line_no,
                message: format!(
                    "未知路径码 {:?}（合法值：{}）",
                    f[2],
                    CoercionPath::ALL
                        .iter()
                        .map(|p| p.as_str())
                        .collect::<Vec<_>>()
                        .join(" / ")
                ),
            })?;
            let expected_value_si = if f[3].is_empty() {
                None
            } else {
                Some(f[3].parse::<f64>().map_err(|_| TableError::Row {
                    line_no,
                    message: format!("期望归一值 {:?} 不是数值", f[3]),
                })?)
            };
            let expected_unit_code = if f[4].is_empty() {
                None
            } else {
                Some(f[4].to_string())
            };
            rows.push(TableRow {
                line_no,
                value_raw: f[0].to_string(),
                unit_raw: f[1].to_string(),
                expected_path,
                expected_value_si,
                expected_unit_code,
                note: f[5].to_string(),
            });
        }
        Ok(CoercionTable { rows })
    }

    /// 从文件读取对照表。
    pub fn load(path: &Path) -> Result<CoercionTable, TableError> {
        let csv = std::fs::read_to_string(path)
            .map_err(|e| TableError::Io(format!("{}：{}", path.display(), e)))?;
        CoercionTable::parse(&csv)
    }

    /// 全部行。
    pub fn rows(&self) -> &[TableRow] {
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

    /// 按路径统计行数（四类路径的覆盖度一眼可见）。
    pub fn count_by_path(&self) -> BTreeMap<&'static str, usize> {
        let mut out: BTreeMap<&'static str, usize> = BTreeMap::new();
        for p in CoercionPath::ALL {
            out.insert(p.as_str(), 0);
        }
        for row in &self.rows {
            *out.entry(row.expected_path.as_str()).or_insert(0) += 1;
        }
        out
    }

    /// 逐行驱动判定内核，返回**全部不匹配的行**（空 = 全部命中）。
    pub fn check(&self) -> Vec<Mismatch> {
        let mut out = Vec::new();
        for row in &self.rows {
            if let Some(m) = check_row(row) {
                out.push(m);
            }
        }
        out
    }

    /// 逐行驱动判定内核；**任一行不匹配即返回带完整报告的 `Err`**。
    ///
    /// 这是「对照表就是判据」的落点：调用方（测试 / `selftest`）不必自己拼报告。
    pub fn assert_all(&self) -> Result<(), String> {
        let mismatches = self.check();
        if mismatches.is_empty() {
            return Ok(());
        }
        Err(report(&mismatches, self.len()))
    }
}

/// 校验一行；命中返回 `None`，不匹配返回 [`Mismatch`]。
pub fn check_row(row: &TableRow) -> Option<Mismatch> {
    let decision = row.drive();
    let mut reasons: Vec<&'static str> = Vec::new();

    if decision.path != row.expected_path {
        reasons.push("路径");
    }
    if !value_matches(decision.value_si, row.expected_value_si) {
        reasons.push("归一值");
    }
    if !unit_matches(decision.unit_code.as_deref(), row.expected_unit_code.as_deref()) {
        reasons.push("单位码");
    }
    if reasons.is_empty() {
        return None;
    }

    Some(Mismatch {
        line_no: row.line_no,
        input: row.input_label(),
        reasons,
        expected: format!(
            "路径={} 归一值={} 单位码={}",
            row.expected_path.as_str(),
            render_value(row.expected_value_si),
            row.expected_unit_code.as_deref().unwrap_or("—")
        ),
        actual: format!(
            "路径={} 归一值={} 单位码={}",
            decision.path.as_str(),
            render_value(decision.value_si),
            decision.unit_code.as_deref().unwrap_or("—")
        ),
        note: row.note.clone(),
    })
}

/// 归一值比较（两侧都空 = 一致；一侧空 = 不一致；都有值按容差比）。
fn value_matches(actual: Option<f64>, expected: Option<f64>) -> bool {
    match (actual, expected) {
        (None, None) => true,
        (Some(a), Some(e)) => (a - e).abs() <= VALUE_TOLERANCE,
        _ => false,
    }
}

/// 单位码比较（两侧都空 = 一致）。
fn unit_matches(actual: Option<&str>, expected: Option<&str>) -> bool {
    match (actual, expected) {
        (None, None) => true,
        (Some(a), Some(e)) => a == e,
        _ => false,
    }
}

/// 归一值的可读形式（空 = 「未产出数值」，**不用 0 冒充**）。
fn render_value(v: Option<f64>) -> String {
    match v {
        Some(x) => format!("{}", x),
        None => "—（未产出数值）".to_string(),
    }
}

/// 不匹配行的完整报告（行号 / 输入 / 期望 / 实际 四列齐全）。
pub fn report(mismatches: &[Mismatch], total_rows: usize) -> String {
    let mut out = format!(
        "强转路径对照表：共 {} 行，{} 行不符\n",
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

    const SAMPLE: &str = "value_raw,unit_raw,expected_path,expected_value_si,expected_unit_code,note\n\
3.2,cm,UnitConvert,32,mm,长度族：厘米→毫米\n\
3.2×2,cm,DirectScale,64,mm,原值词面自带倍率\n\
3.2,cm3,Composite,3.2,mL,复合量纲：量纲还原 + 单位换算两步\n\
3.2,xx,None,,,未知单位：查不到合法路径，不产出数值\n";

    #[test]
    fn 解析表头与数据行() {
        let table = CoercionTable::parse(SAMPLE).expect("对照表应能解析");
        assert_eq!(table.len(), 4);
        let first = &table.rows()[0];
        assert_eq!(first.line_no, 2, "行号按 CSV 物理行号记（1 基，含表头）");
        assert_eq!(first.value_raw, "3.2");
        assert_eq!(first.unit_raw, "cm");
        assert_eq!(first.expected_path, CoercionPath::UnitConvert);
        assert_eq!(first.expected_value_si, Some(32.0));
        assert_eq!(first.expected_unit_code.as_deref(), Some("mm"));
    }

    #[test]
    fn 失败行的期望值留空即不产出数值() {
        let table = CoercionTable::parse(SAMPLE).expect("对照表应能解析");
        let none_row = table
            .rows()
            .iter()
            .find(|r| r.expected_path == CoercionPath::None)
            .expect("表里应有 None 行");
        assert_eq!(none_row.expected_value_si, None);
        assert_eq!(none_row.expected_unit_code, None);
        // 驱动判定内核同样不产出数值。
        let decision = none_row.drive();
        assert_eq!(decision.path, CoercionPath::None);
        assert!(decision.value_si.is_none());
        assert!(decision.unit_code.is_none());
    }

    #[test]
    fn 表头不对报错() {
        let bad = "value_raw,unit_raw,expected_path\n1,cm,UnitConvert\n";
        let err = CoercionTable::parse(bad).expect_err("列数不对的表头应报错");
        assert!(matches!(err, TableError::Header(_)));
    }

    #[test]
    fn 列数不对报错且带行号() {
        let bad = "value_raw,unit_raw,expected_path,expected_value_si,expected_unit_code,note\n\
3.2,cm,UnitConvert,32,mm\n";
        let err = CoercionTable::parse(bad).expect_err("少一列应报错");
        match err {
            TableError::Row { line_no, message } => {
                assert_eq!(line_no, 2);
                assert!(message.contains("列数不对"));
            }
            other => panic!("应报行错误，实际 {:?}", other),
        }
    }

    #[test]
    fn 未知路径码报错不套默认路径() {
        let bad = "value_raw,unit_raw,expected_path,expected_value_si,expected_unit_code,note\n\
3.2,cm,Whatever,32,mm,\n";
        let err = CoercionTable::parse(bad).expect_err("表外路径码应报错");
        match err {
            TableError::Row { line_no, message } => {
                assert_eq!(line_no, 2);
                assert!(message.contains("未知路径码"));
            }
            other => panic!("应报行错误，实际 {:?}", other),
        }
    }

    #[test]
    fn 期望归一值不是数值报错() {
        let bad = "value_raw,unit_raw,expected_path,expected_value_si,expected_unit_code,note\n\
3.2,cm,UnitConvert,abc,mm,\n";
        let err = CoercionTable::parse(bad).expect_err("非数值期望应报错");
        assert!(matches!(err, TableError::Row { .. }));
    }

    #[test]
    fn 空行跳过但行号不跳号() {
        let csv = "value_raw,unit_raw,expected_path,expected_value_si,expected_unit_code,note\n\
3.2,cm,UnitConvert,32,mm,\n\
\n\
2.8,cm,UnitConvert,28,mm,\n";
        let table = CoercionTable::parse(csv).expect("应能解析");
        assert_eq!(table.len(), 2);
        assert_eq!(table.rows()[1].line_no, 4, "空行不占数据行，但行号按物理行");
    }

    #[test]
    fn 全部命中时不报不符() {
        let table = CoercionTable::parse(SAMPLE).expect("对照表应能解析");
        let mismatches = table.check();
        assert!(
            mismatches.is_empty(),
            "样例表应全部命中，实际不符：{}",
            report(&mismatches, table.len())
        );
        assert!(table.assert_all().is_ok());
    }

    #[test]
    fn 不匹配时报告含行号输入期望与实际() {
        let csv = "value_raw,unit_raw,expected_path,expected_value_si,expected_unit_code,note\n\
3.2,cm,UnitConvert,32,mm,长度族：厘米→毫米\n\
2.8,cm,UnitConvert,99,mm,故意写错的期望值\n";
        let table = CoercionTable::parse(csv).expect("应能解析");
        let mismatches = table.check();
        assert_eq!(mismatches.len(), 1, "只有写错的那一行不符");
        let m = &mismatches[0];
        assert_eq!(m.line_no, 3);
        assert_eq!(m.input, "2.8 cm");
        assert!(m.reasons.contains(&"归一值"));
        assert!(m.expected.contains("99"));
        assert!(m.actual.contains("28"));

        let text = report(&mismatches, table.len());
        assert!(text.contains("第 3 行"), "报告须含行号：{}", text);
        assert!(text.contains("2.8 cm"), "报告须含输入：{}", text);
        assert!(text.contains("期望"), "报告须含期望列：{}", text);
        assert!(text.contains("实际"), "报告须含实际列：{}", text);
        assert!(text.contains("共 2 行，1 行不符"));

        let err = table.assert_all().expect_err("不匹配应返回 Err");
        assert!(err.contains("第 3 行"));
    }

    #[test]
    fn 路径错了也要报出来() {
        let csv = "value_raw,unit_raw,expected_path,expected_value_si,expected_unit_code,note\n\
3.2,cm,Composite,32,mm,\n";
        let table = CoercionTable::parse(csv).expect("应能解析");
        let mismatches = table.check();
        assert_eq!(mismatches.len(), 1);
        assert!(mismatches[0].reasons.contains(&"路径"));
        assert!(mismatches[0].expected.contains("Composite"));
        assert!(mismatches[0].actual.contains("UnitConvert"));
    }

    #[test]
    fn 按路径统计覆盖度() {
        let table = CoercionTable::parse(SAMPLE).expect("对照表应能解析");
        let counts = table.count_by_path();
        assert_eq!(counts.get("UnitConvert"), Some(&1));
        assert_eq!(counts.get("DirectScale"), Some(&1));
        assert_eq!(counts.get("Composite"), Some(&1));
        assert_eq!(counts.get("None"), Some(&1));
        // 四个路径都在计数表里（空表也要有键，便于断言覆盖度）。
        assert_eq!(counts.len(), CoercionPath::ALL.len());
    }

    #[test]
    fn 路径判定与表外路径码互为逆() {
        // 表里出现的路径码一定能被内核反过来识别（表不会写出内核认不出的码）。
        for p in CoercionPath::ALL {
            assert_eq!(CoercionPath::from_code(p.as_str()), Some(p));
        }
    }
}
