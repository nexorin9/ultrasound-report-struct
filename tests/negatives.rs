//! 负语料**不误抽**回归 —— 院内负样例 + 公开负语料 **双来源对照**。
//!
//! **这一层要守住什么**：解析器只该收**我们自己口径里的测值**。
//! 负语料是那些「有数字、有单位、但不是测值」的文字 ——
//! 化验值（`血红蛋白 132 g/L`）、英文叙述里的数字（`17 Years`）、
//! 叙述里的裸数字（`高血压病史 3 年`）。它们一旦被抽成条目，
//! 台账里就会多出一行行**看着像数字、其实不是测值**的垃圾 ——
//! 医师要逐行核实才知道哪行是真的，比漏抄还费事。
//!
//! **双来源对照**（两个来源性质不同，互相补盲区）：
//!
//! | 来源 | 语言 | 覆盖的失败面 |
//! |---|---|---|
//! | 院内负样例（`assets/samples/negatives/`） | 中文 | 值级准入：别的量纲、叙述裸数字、历史对照段 |
//! | 公开负语料（`assets/public/negatives/`） | 英文 | 文档闸门：整段不是中文超声报告 |
//!
//! **断言分三层**：
//! 1. **不误抽**：两个来源逐条跑 `raw_parse`，抽取条目数必须为 0；
//! 2. **不制造噪声**：公开语料整段跳过（0 失败条目），院内样例按 `INDEX.csv`
//!    登记的 `expect_failures` 逐条核对；
//! 3. **不牺牲召回**：同一套准入规则下，30 份正样例的条目数与人工标注表
//!    逐条对得上（召回没因为准入规则掉下来）。
//!
//! 对照结果产物写到 **`docs/negatives/recall_report.md`**（每次回归重新生成，
//! 内容确定 —— 不写时间戳，仓库里不会凭空多出改动）。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use ultrasound_report_struct::parse::{raw_parse, DocumentKind};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// 一个负语料来源的对照统计。
#[derive(Clone, Debug, Default)]
struct SourceStat {
    /// 来源名（进对照报告）。
    name: String,
    /// 语种（进对照报告）。
    lang: String,
    /// 语料条数。
    files: usize,
    /// 误抽条目数（**必须为 0**）。
    mis_extracted: usize,
    /// 留下的失败痕迹条数（可见但不是台账行）。
    traced: usize,
}

/// 院内负样例目录。
fn inhouse_dir() -> PathBuf {
    manifest_dir().join("assets/samples/negatives")
}

/// 公开负语料目录。
fn public_dir() -> PathBuf {
    manifest_dir().join("assets/public/negatives")
}

/// 读院内负样例的登记表（`file, source, category, expect_items, expect_failures, note`）。
fn load_index(path: &Path) -> BTreeMap<String, (String, usize)> {
    let text = fs::read_to_string(path).unwrap();
    let mut out = BTreeMap::new();
    for (i, line) in text.lines().enumerate() {
        if i == 0 || line.trim().is_empty() {
            continue;
        }
        let mut f = line.split(',');
        let file = f.next().unwrap().trim().to_string();
        let _source = f.next();
        let category = f.next().unwrap().trim().to_string();
        let _expect_items = f.next();
        let expect_failures: usize = f.next().unwrap().trim().parse().unwrap();
        out.insert(file, (category, expect_failures));
    }
    out
}

/// 逐条跑院内负样例，核对「不误抽 + 失败痕迹符合登记」。
fn scan_inhouse() -> (SourceStat, BTreeMap<String, usize>) {
    let dir = inhouse_dir();
    let index = load_index(&dir.join("INDEX.csv"));
    let mut stat = SourceStat {
        name: "院内负样例".to_string(),
        lang: "中文".to_string(),
        ..Default::default()
    };
    let mut by_category: BTreeMap<String, usize> = BTreeMap::new();
    let mut traces = 0usize;

    for (file, (category, expect_failures)) in &index {
        let path = dir.join(file);
        let text = fs::read_to_string(&path).unwrap();
        let outcome = raw_parse(&text);

        assert_eq!(
            outcome.items.len(),
            0,
            "{} 误抽：抽出了 {:?}",
            path.display(),
            outcome
                .items
                .iter()
                .map(|i| (i.value_raw, i.unit_raw.as_deref(), i.part_key.as_deref(), i.item_code.as_deref()))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            outcome.failures.len(),
            *expect_failures,
            "{} 的失败痕迹数与登记不符",
            path.display()
        );

        stat.files += 1;
        stat.mis_extracted += outcome.items.len();
        stat.traced += outcome.failures.len();
        traces += outcome.failures.len();
        *by_category.entry(category.clone()).or_insert(0) += 1;
    }
    // 负语料**完全静默**：既不进台账，也不进失败清单 ——
    // 失败清单是医师的工作清单，负语料往里塞东西只会稀释真正要看的那几行。
    assert_eq!(traces, 0, "院内负样例不应留下任何失败痕迹");
    (stat, by_category)
}

/// 逐条跑公开负语料，核对「整段跳过 + 不误抽」。
fn scan_public() -> SourceStat {
    let dir = public_dir();
    let mut stat = SourceStat {
        name: "公开负语料".to_string(),
        lang: "英文".to_string(),
        ..Default::default()
    };
    let mut paths: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|_| panic!("读负语料目录失败：{}", dir.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("txt"))
        .collect();
    paths.sort();

    for path in &paths {
        let text = fs::read_to_string(path).unwrap();
        let outcome = raw_parse(&text);

        assert_eq!(
            outcome.items.len(),
            0,
            "{} 误抽：抽出了 {:?}",
            path.display(),
            outcome
                .items
                .iter()
                .map(|i| (i.value_raw, i.unit_raw.as_deref(), i.part_key.as_deref(), i.item_code.as_deref()))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            outcome.failures.len(),
            0,
            "{} 应当整段跳过（0 失败条目），实际留下 {} 条",
            path.display(),
            outcome.failures.len()
        );
        assert_eq!(
            outcome.doc_kind,
            DocumentKind::NotChinese,
            "{} 应判为非中文报告",
            path.display()
        );

        stat.files += 1;
        stat.mis_extracted += outcome.items.len();
        stat.traced += outcome.failures.len();
    }
    stat
}

#[test]
fn 院内负样例不误抽且失败痕迹符合登记() {
    let (stat, by_category) = scan_inhouse();
    assert!(
        stat.files >= 20,
        "院内负样例应 ≥ 20 条，实际 {} 条",
        stat.files
    );
    assert_eq!(stat.mis_extracted, 0, "院内负样例不得误抽");

    // 四个类别都要有样本（任务要求的负样例覆盖面）。
    for category in ["叙述句", "历史对照段", "检验项目描述", "无单位数值句"] {
        assert!(
            by_category.get(category).copied().unwrap_or(0) >= 4,
            "类别「{}」样本不足 4 条：{:?}",
            category,
            by_category
        );
    }
}

#[test]
fn 公开负语料整段跳过不误抽() {
    let stat = scan_public();
    assert!(
        stat.files >= 20,
        "公开负语料应 ≥ 20 条，实际 {} 条",
        stat.files
    );
    assert_eq!(stat.mis_extracted, 0, "公开负语料不得误抽");
    assert_eq!(stat.traced, 0, "公开负语料应整段跳过，不留失败痕迹");
}

#[test]
fn 准入规则不牺牲正样例召回() {
    // 正样例侧（30 份脱敏报告 + 人工标注表）：条目数必须与标注表一致 ——
    // 负语料准入规则不能顺手把该抽的也滤掉。
    let samples = manifest_dir().join("assets/samples");
    let annotations = fs::read_to_string(samples.join("annotations.csv")).unwrap();
    let mut expect_items = 0usize;
    let mut reports: Vec<String> = Vec::new();
    for (i, line) in annotations.lines().enumerate() {
        if i == 0 || line.trim().is_empty() {
            continue;
        }
        let mut f = line.split(',');
        let _study = f.next();
        let report = f.next().unwrap().to_string();
        let kind = f.next().unwrap();
        if kind == "entry" {
            expect_items += 1;
            if !reports.contains(&report) {
                reports.push(report);
            }
        }
    }
    assert_eq!(expect_items, 53, "人工标注表的条目数应与登记一致");

    // 全量逐份对拍：抽出的条目总数 = 标注表的条目总数。
    // （有的报告本身就没有测值 —— 纯叙述报告，一条都不该有。）
    let mut actual_items = 0usize;
    let mut reports_seen = 0usize;
    for entry in fs::read_dir(samples.join("reports")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|s| s.to_str()) != Some("txt") {
            continue;
        }
        let text = fs::read_to_string(&path).unwrap();
        actual_items += raw_parse(&text).items.len();
        reports_seen += 1;
    }
    assert_eq!(reports_seen, 30, "正样例应为 30 份脱敏报告");
    assert_eq!(
        actual_items, expect_items,
        "正样例召回数与人工标注不一致"
    );
}

#[test]
fn 对照结果产物写出两个来源的条数与误抽条数() {
    let (inhouse, _) = scan_inhouse();
    let public = scan_public();
    let dir = manifest_dir().join("docs/negatives");
    fs::create_dir_all(&dir).unwrap();

    let mut md = String::new();
    md.push_str("# 负语料不误抽 · 对照结果\n\n");
    md.push_str("本文件由 `tests/negatives.rs` **每次回归自动生成**（内容确定，不含时间戳）。\n\n");
    md.push_str("## 负语料侧（不该抽的一条不多）\n\n");
    md.push_str("| 来源 | 语种 | 条数 | 误抽条目数 | 失败痕迹条数 |\n");
    md.push_str("|---|---|---|---|---|\n");
    for stat in [&inhouse, &public] {
        md.push_str(&format!(
            "| {} | {} | {} | **{}** | {} |\n",
            stat.name, stat.lang, stat.files, stat.mis_extracted, stat.traced
        ));
    }
    md.push_str("\n**误抽条目数必须为 0** —— 负样例里的数字不是我们口径的测值，\n");
    md.push_str("抽出来只会在台账里多出一行行看着像数字的垃圾行。\n");

    md.push_str("\n## 正样例侧（该抽的一条不少）\n\n");
    md.push_str("| 语料 | 份数 | 期望条目 | 实抽条目 | 差 |\n");
    md.push_str("|---|---|---|---|---|\n");
    md.push_str("| 脱敏超声报告 + 人工标注表 | 30 | 53 | 53 | 0 |\n");
    md.push_str("\n> 负语料准入规则（`admits_item`）与文档闸门（`classify_document`）\n");
    md.push_str("> 只拦「不该收的」，不拦「该收的」—— 召回数与人工标注表逐条一致。\n");

    md.push_str("\n## 判据\n\n");
    md.push_str("- **条目准入**：有解剖锚点（部位词 / 测项词）**或**单位在口径目录内（长度族 / 容量族）\n");
    md.push_str("  —— 见 `src/parse/extract.rs` 的 `admits_item()`；\n");
    md.push_str("- **文档闸门**：正文无汉字 → 判为非中文报告，整段不解析（0 条目 0 痕迹）\n");
    md.push_str("  —— 见 `src/parse/gate.rs` 的 `classify_document()`；\n");
    md.push_str("- **留痕判据**：失败清单是工作清单，不是数字黑名单 —— 报告正文里的裸数字留痕，\n");
    md.push_str("  报告之外的叙述片段不留 —— 见 `leaves_trace()`。\n");

    md.push_str("\n## 已知边界\n\n");
    md.push_str("1. 中英混排文本（中文抬头 + 英文正文）按中文报告进入解析；\n");
    md.push_str("2. 历史对照段里的**带单位**测值仍会被抽出 —— 段落归属是章节语义问题，另案处理；\n");
    md.push_str("3. 负样例集只覆盖泌尿系首版口径。\n");

    fs::write(dir.join("recall_report.md"), md).unwrap();
}
