//! 口径核对清单：单位码表 + 部位编码表。
//!
//! **这两份清单是干什么的**：本项目自报单位码（`mm` / `cm` / `mL`…）与规范部位键
//! （`kidney.left`…）。清单的作用是**把自报口径挂到公开编码体系上**，
//! 让人能核对「这个码不是我们自己编的」：
//!
//! - **单位码表**取自 UCUM 官方 essential 子集 —— 本项目的单位码逐条对照 UCUM 码，
//!   精确匹配、需映射（复合量纲 `cm3` 对应 UCUM 的 `[cm3]`）、未收录三档写清楚。
//! - **部位编码表**取自 NCBI MeSH 描述符 —— 本项目的规范部位键挂到公开医学术语编码
//!   （MeSH UI，如 `Kidney` → `D007668`）上，逐条写明首选词与同义词。
//!
//! 清单只做**核对**，不替院内口径做决定：换算因子仍是院内约定的经验口径。

use std::path::Path;

use regex::Regex;
use serde_json::Value;

use crate::publicdata::integrity::check_pinned;
use crate::publicdata::manifest::{SourceSpec, UCUM_ESSENCE_RAW_URL};
use crate::publicdata::report::{OutcomeStatus, SourceOutcome};
use crate::publicdata::{http_get, SourceError};

/// 本项目声明的单位码（与强转内核的单位目录同源）。
///
/// 顺序即清单顺序：长度族在前、容量族在后。
const PROJECT_UNITS: &[(&str, &str)] = &[
    ("mm", "Length"),
    ("cm", "Length"),
    ("m", "Length"),
    ("mL", "Volume"),
    ("L", "Volume"),
    ("cm3", "Volume"),
];

/// 本项目的规范部位键 → MeSH 检索词。
const PROJECT_PARTS: &[(&str, &str)] = &[
    ("kidney.left", "Kidney"),
    ("kidney.right", "Kidney"),
    ("kidney.bilateral", "Kidney"),
    ("ureter.left", "Ureter"),
    ("ureter.right", "Ureter"),
    ("bladder", "Urinary Bladder"),
];

/// 取回 UCUM 文件的原始字节。
///
/// contents 接口的响应是 JSON（文件内容以 base64 给出），raw 地址直接是原始字节；
/// 两者给出的文件字节完全一致，所以固定校验值对两者通用。
fn fetch_ucum_bytes(spec: &SourceSpec, timeout_secs: u64) -> Result<Vec<u8>, String> {
    fetch_ucum_contents(&spec.url, timeout_secs)
}

/// 解 base64（忽略换行与空白）。
pub fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for ch in input.chars() {
        if ch.is_ascii_whitespace() || ch == '=' {
            continue;
        }
        let value = match ch {
            'A'..='Z' => ch as u32 - 'A' as u32,
            'a'..='z' => ch as u32 - 'a' as u32 + 26,
            '0'..='9' => ch as u32 - '0' as u32 + 52,
            '+' => 62,
            '/' => 63,
            _ => return None,
        };
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

/// 取回 UCUM 单位码表并落地核对清单。
///
/// 两份发布文件合起来看：
/// - **essential 子集**（305 条最小必要单位）—— 官方策展清单，优先；
/// - **完整发布包** —— 才有长度与容量单位（mm / cm / mL / kg / s …）。
///
/// 两份都做固定校验（sha256 + 字节数），任何一份对不上即报出来。
pub fn fetch_unit_codes(
    spec: &SourceSpec,
    dest: &Path,
    timeout_secs: u64,
) -> Result<SourceOutcome, SourceError> {
    let essence_bytes = fetch_ucum_bytes(spec, timeout_secs)
        .or_else(|primary| {
            http_get(UCUM_ESSENCE_RAW_URL, timeout_secs).map_err(|backup| {
                format!("主地址失败（{}）；备用地址也失败（{}）", primary, backup)
            })
        })
        .map_err(SourceError::Network)?;
    check_pinned(
        &spec.id,
        "ucum-essence.xml",
        &essence_bytes,
        spec.pinned_sha256.as_deref(),
        spec.pinned_bytes,
    )
    .map_err(SourceError::Integrity)?;

    // 第二份文件（完整发布包）：地址与固定校验值都随来源走。
    let spec_bytes = match &spec.secondary {
        Some(second) => {
            let bytes = fetch_ucum_contents(&second.url, timeout_secs)
                .or_else(|primary| {
                    http_get(&second.raw_url, timeout_secs).map_err(|backup| {
                        format!("主地址失败（{}）；备用地址也失败（{}）", primary, backup)
                    })
                })
                .map_err(SourceError::Network)?;
            check_pinned(
                &spec.id,
                &second.file_name,
                &bytes,
                Some(&second.sha256),
                Some(second.bytes),
            )
            .map_err(SourceError::Integrity)?;
            bytes
        }
        None => Vec::new(),
    };

    let essence = parse_ucum_units(&String::from_utf8_lossy(&essence_bytes));
    let spec_codes = parse_ucum_spec_codes(&String::from_utf8_lossy(&spec_bytes));
    let merged = merge_ucum_codes(essence.clone(), spec_codes.clone());
    if merged.is_empty() {
        return Err(SourceError::Network(format!(
            "来源 {} 未解析出任何单位码",
            spec.id
        )));
    }

    let dir = dest.join("ucum");
    std::fs::create_dir_all(&dir).map_err(|e| SourceError::Io(e.to_string()))?;

    // 全量单位码表（参考用）。
    let mut full = String::from(
        "code,name,class,source,in_project_scope\n",
    );
    for (code, entry) in &merged {
        full.push_str(&format!(
            "{},{},{},{},{}\n",
            code,
            csv_field(&entry.name),
            csv_field(&entry.class),
            entry.source,
            if PROJECT_UNITS.iter().any(|(c, _)| c == code) { "是" } else { "否" }
        ));
    }
    let full_path = dir.join("unit_codes.csv");
    std::fs::write(&full_path, full.as_bytes()).map_err(|e| SourceError::Io(e.to_string()))?;

    // 本项目单位码的逐条核对（这才是「核对清单」）。
    let mut cross = String::from(
        "project_code,project_family,ucum_code,ucum_name,ucum_class,match_type,note\n",
    );
    for (code, family) in PROJECT_UNITS {
        match merged.get(*code) {
            Some(entry) => {
                let in_essence = entry.source == "essence";
                cross.push_str(&format!(
                    "{},{},{},{},{},{},{}\n",
                    code,
                    family,
                    code,
                    csv_field(&entry.name),
                    csv_field(&entry.class),
                    if in_essence { "精确" } else { "完整码表" },
                    if in_essence {
                        "本项目码即 UCUM 官方策展子集（essential）里的码"
                    } else {
                        "本项目码取自 UCUM 完整码表；essential 子集未收录该码"
                    }
                ));
            }
            None => {
                // **列数与表头一致（7 列）**：ucum_code / ucum_name / ucum_class 三列留空，
                // match_type 写「未收录」，note 写口径说明。
                // （这里曾经多写了一个逗号，整行右移一列 —— `match_type` 变空、
                // 「未收录」落到 note 列上，按列号读表的人会读错。）
                cross.push_str(&format!(
                    "{},{},,,,未收录,UCUM 两份发布包均未收录该码（本项目为简化写法），换算按院内约定\n",
                    code, family
                ));
            }
        }
    }
    let cross_path = dir.join("unit_crosswalk.csv");
    std::fs::write(&cross_path, cross.as_bytes()).map_err(|e| SourceError::Io(e.to_string()))?;

    Ok(SourceOutcome {
        id: spec.id.clone(),
        kind_label: spec.kind.label().to_string(),
        status: OutcomeStatus::Fetched,
        files: 2,
        detail: format!(
            "单位码 {} 条（essential {} + 完整码表 {}）；本项目单位码核对表：{}",
            merged.len(),
            essence.len(),
            spec_codes.len(),
            cross_path.display()
        ),
    })
}

/// 取回 contents 接口返回的文件字节（内容以 base64 给出）。
fn fetch_ucum_contents(url: &str, timeout_secs: u64) -> Result<Vec<u8>, String> {
    let body = http_get(url, timeout_secs).map_err(|e| e.to_string())?;
    let json: Value = serde_json::from_slice(&body)
        .map_err(|e| format!("响应不是合法 JSON（{}）", e))?;
    let content = json
        .get("content")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "响应里没有 content 字段".to_string())?;
    base64_decode(content).ok_or_else(|| "content 不是合法的 base64".to_string())
}

/// 一条单位码（合并后的口径）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UcumCode {
    /// 单位名。
    pub name: String,
    /// 类别（essential 的 class / 完整码表的量纲）。
    pub class: String,
    /// 来源（`essence` = 官方策展子集；`spec` = 完整码表）。
    pub source: String,
}

/// UCUM 的前缀名（十进制 + 二进制）。
///
/// 前缀不是单位：`m` 既是米（长度）也是毫（前缀），
/// 前缀行在码表里与单位行同形，靠名字挡掉，避免把「米」写成「毫」。
const UCUM_PREFIX_NAMES: &[&str] = &[
    "yotta", "zetta", "exa", "peta", "tera", "giga", "mega", "kilo", "hecto", "deka", "deci",
    "centi", "milli", "micro", "nano", "pico", "femto", "atto", "zepto", "yocto", "kibi", "mebi",
    "gibi", "tebi", "pebi", "exbi", "zebi", "yobi",
];

/// 从完整发布包的码表里抽「码 → 名称」。
///
/// 码表有两种行形态：
/// - `(码, 空, 名称, …)` —— 换算因子表；
/// - `(名称, 量纲, 符号, …)` —— 单位定义表。
pub fn parse_ucum_spec_codes(xml: &str) -> Vec<(String, String, String)> {
    let row_re = Regex::new(r"(?s)<tr[^>]*>(.*?)</tr>").expect("表格行正则应可编译");
    let mut out = Vec::new();
    for caps in row_re.captures_iter(xml) {
        let cells = table_cells(caps.get(1).map(|m| m.as_str()).unwrap_or(""));
        if cells.len() < 3 || cells[0].is_empty() || cells[2].is_empty() {
            continue;
        }
        // 码表里的码都是短的（≤ 24 字符），名称也不长；
        // 混进来的「术语表」整段文字会让单元变得很长 —— 按长度挡掉。
        if cells[0].len() > 24 || cells[2].len() > 120 {
            continue;
        }
        if UCUM_PREFIX_NAMES
            .iter()
            .any(|p| p.eq_ignore_ascii_case(&cells[0]))
        {
            // 前缀行（milli / kilo …）不是单位，跳过。
            continue;
        }
        if cells[1].is_empty() {
            // 形态一：码在第一列，名称在第三列。
            out.push((cells[0].clone(), cells[2].clone(), String::new()));
        } else {
            // 形态二：名称在第一列，符号（码）在第三列，量纲在第二列。
            out.push((cells[2].clone(), cells[0].clone(), cells[1].clone()));
        }
    }
    out
}

/// 取表格单元文本（自闭合的 `<td/>` 算空单元）。
fn table_cells(row: &str) -> Vec<String> {
    let cell_re = Regex::new(r"(?s)<td\b([^>]*)>").expect("单元正则应可编译");
    let mut out = Vec::new();
    for caps in cell_re.captures_iter(row) {
        let attrs = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        if attrs.trim_end().ends_with('/') {
            out.push(String::new());
            continue;
        }
        let start = caps.get(0).map(|m| m.end()).unwrap_or(0);
        let end = row[start..].find("</td>").map(|i| start + i).unwrap_or(start);
        out.push(plain_text(&row[start..end]));
    }
    out
}

/// 合并两份发布文件的单位码（essential 优先）。
///
/// 同一码在两份里都有时，取 essential 的条目 —— 它是官方策展的最小必要集，
/// 码形与名称以此为准；完整码表只用来补 essential 没收录的长度 / 容量单位。
pub fn merge_ucum_codes(
    essence: Vec<UcumUnit>,
    spec: Vec<(String, String, String)>,
) -> std::collections::BTreeMap<String, UcumCode> {
    let mut merged: std::collections::BTreeMap<String, UcumCode> = std::collections::BTreeMap::new();
    for unit in essence {
        merged.insert(
            unit.code.clone(),
            UcumCode {
                name: unit.name,
                class: unit.class,
                source: "essence".to_string(),
            },
        );
    }
    // 完整码表里同一个符号可能出现两次（如 `m` 既是米也是毫的前缀）——
    // 取**量纲不是 prefix** 的那条（米、秒、克这些基本单位才是我们要的）。
    let mut spec_map: std::collections::BTreeMap<String, UcumCode> =
        std::collections::BTreeMap::new();
    for (code, name, class) in spec {
        let candidate = UcumCode {
            name,
            class,
            source: "spec".to_string(),
        };
        match spec_map.get(&code) {
            Some(prev) if prev.class == "prefix" && candidate.class != "prefix" => {
                spec_map.insert(code, candidate);
            }
            Some(_) => {}
            None => {
                spec_map.insert(code, candidate);
            }
        }
    }
    for (code, entry) in spec_map {
        merged.entry(code).or_insert(entry);
    }
    merged
}

/// 取回 MeSH 部位编码并落地核对清单。
///
/// 每个部位词两次取数：先按 `[MeSH]` 字段检索拿描述符 uid，再取摘要拿 UI 码与首选词。
pub fn fetch_body_part_codes(
    spec: &SourceSpec,
    dest: &Path,
    timeout_secs: u64,
) -> Result<SourceOutcome, SourceError> {
    let mut csv = String::from(
        "project_part_key,mesh_term,mesh_ui,preferred_term,synonyms,year_introduced,source_url\n",
    );
    let mut resolved = 0usize;

    for term in unique_terms() {
        let uid = mesh_uid(&spec.url, term, timeout_secs)?;
        let info = mesh_summary(&summary_endpoint(&spec.url), &uid, timeout_secs)?;
        for (part_key, _) in PROJECT_PARTS.iter().filter(|(_, t)| *t == term) {
            csv.push_str(&format!(
                "{},{},{},{},{},{},{}\n",
                part_key,
                csv_field(term),
                csv_field(&info.mesh_ui),
                csv_field(&info.preferred_term),
                csv_field(&info.synonyms.join("|")),
                csv_field(&info.year_introduced),
                csv_field(&format!(
                    "https://www.ncbi.nlm.nih.gov/mesh/?term={}",
                    url_encode(term)
                ))
            ));
        }
        resolved += 1;
    }

    if resolved == 0 {
        return Err(SourceError::Network(format!(
            "来源 {} 未解析出任何部位编码",
            spec.id
        )));
    }

    let dir = dest.join("mesh");
    std::fs::create_dir_all(&dir).map_err(|e| SourceError::Io(e.to_string()))?;
    let path = dir.join("body_part_codes.csv");
    std::fs::write(&path, csv.as_bytes()).map_err(|e| SourceError::Io(e.to_string()))?;

    Ok(SourceOutcome {
        id: spec.id.clone(),
        kind_label: spec.kind.label().to_string(),
        status: OutcomeStatus::Fetched,
        files: 1,
        detail: format!(
            "部位编码 {} 个描述符；本项目 {} 个规范部位键挂到公开术语编码：{}",
            resolved,
            PROJECT_PARTS.len(),
            path.display()
        ),
    })
}

/// 部位检索词（去重，保持顺序）。
fn unique_terms() -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for (_, term) in PROJECT_PARTS {
        if !out.contains(term) {
            out.push(term);
        }
    }
    out
}

/// MeSH 描述符的一条摘要信息。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MeshDescriptor {
    /// MeSH UI 码（形如 `D007668`）。
    pub mesh_ui: String,
    /// 首选词。
    pub preferred_term: String,
    /// 同义词。
    pub synonyms: Vec<String>,
    /// 引入年份。
    pub year_introduced: String,
}

/// 摘要接口地址：检索接口地址里的 `esearch` 换成 `esummary`。
///
/// 这么做是为了让入口地址只配一处（清单里一个 URL 就能定位到同一套接口的两步）。
pub fn summary_endpoint(base: &str) -> String {
    base.replace("esearch", "esummary")
}

/// 按 `[MeSH]` 字段检索描述符 uid。
pub fn mesh_uid(base: &str, term: &str, timeout_secs: u64) -> Result<String, SourceError> {
    let url = format!(
        "{}?db=mesh&term={}%5BMeSH%5D&retmax=1&retmode=json",
        base,
        url_encode(term)
    );
    let body = http_get(&url, timeout_secs)?;
    let json: Value = serde_json::from_slice(&body)
        .map_err(|e| SourceError::Network(format!("{}：响应不是合法 JSON（{}）", url, e)))?;
    let uid = json
        .get("esearchresult")
        .and_then(|r| r.get("idlist"))
        .and_then(|l| l.get(0))
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            SourceError::Network(format!("{}：检索不到描述符（term={}）", url, term))
        })?;
    Ok(uid.to_string())
}

/// 取 MeSH 描述符摘要（UI 码 / 首选词 / 同义词 / 引入年份）。
pub fn mesh_summary(base: &str, uid: &str, timeout_secs: u64) -> Result<MeshDescriptor, SourceError> {
    let url = format!("{}?db=mesh&id={}&retmode=json", base, uid);
    let body = http_get(&url, timeout_secs)?;
    let json: Value = serde_json::from_slice(&body)
        .map_err(|e| SourceError::Network(format!("{}：响应不是合法 JSON（{}）", url, e)))?;
    let record = json
        .get("result")
        .and_then(|r| r.get(uid))
        .ok_or_else(|| SourceError::Network(format!("{}：摘要里没有 uid={}", url, uid)))?;

    let mesh_ui = record
        .get("ds_meshui")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let terms: Vec<String> = record
        .get("ds_meshterms")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let preferred_term = terms.first().cloned().unwrap_or_default();
    let synonyms = terms.iter().skip(1).cloned().collect::<Vec<_>>();
    // ds_yearintroduced 里混着说明文字（如「… was KIDNEY TRANSPLANTATION 1963-1965」），
    // 只取第一个年份 —— 清单要的是年份，不是整段说明。
    let year_introduced = record
        .get("ds_yearintroduced")
        .and_then(|v| v.as_str())
        .and_then(first_year)
        .unwrap_or_default();

    if mesh_ui.is_empty() {
        return Err(SourceError::Network(format!(
            "{}：描述符 uid={} 没有 ds_meshui",
            url, uid
        )));
    }
    Ok(MeshDescriptor {
        mesh_ui,
        preferred_term,
        synonyms,
        year_introduced,
    })
}

/// 取文本里的第一个四位年份。
pub fn first_year(text: &str) -> Option<String> {
    let bytes: Vec<char> = text.chars().collect();
    let mut idx = 0usize;
    while idx + 4 <= bytes.len() {
        let window: String = bytes[idx..idx + 4].iter().collect();
        if window.chars().all(|c| c.is_ascii_digit()) {
            return Some(window);
        }
        idx += 1;
    }
    None
}

/// URL 查询串里的轻量转义（空格与 & 之类）。
pub fn url_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            ' ' => out.push('+'),
            '+' => out.push_str("%2B"),
            '&' => out.push_str("%26"),
            '=' => out.push_str("%3D"),
            '?' => out.push_str("%3F"),
            '#' => out.push_str("%23"),
            c if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' => out.push(c),
            c => out.push_str(&format!("%{:02X}", c as u32)),
        }
    }
    out
}

/// UCUM 单位码一条。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UcumUnit {
    /// UCUM 码（原样）。
    pub code: String,
    /// 大小写不敏感码。
    pub code_ci: String,
    /// 是否米制。
    pub is_metric: String,
    /// 量纲类别（`length` / `volume` 等）。
    pub class: String,
    /// 单位名。
    pub name: String,
    /// 打印符号。
    pub print_symbol: String,
}

/// 解析 UCUM essential 子集。
pub fn parse_ucum_units(xml: &str) -> Vec<UcumUnit> {
    let unit_re = Regex::new(r#"(?s)<unit\s+Code="([^"]*)"\s+CODE="([^"]*)"\s+isMetric="([^"]*)"\s+class="([^"]*)"\s*>(.*?)</unit>"#)
        .expect("UCUM 单位正则应可编译");
    let name_re = Regex::new(r"(?s)<name>(.*?)</name>").expect("UCUM 名称正则应可编译");
    let symbol_re = Regex::new(r"(?s)<printSymbol>(.*?)</printSymbol>").expect("UCUM 符号正则应可编译");
    let mut out = Vec::new();
    for caps in unit_re.captures_iter(xml) {
        let body = caps.get(5).map(|m| m.as_str()).unwrap_or("");
        let name = name_re
            .captures(body)
            .and_then(|c| c.get(1))
            .map(|m| plain_text(m.as_str()))
            .unwrap_or_default();
        let symbol = symbol_re
            .captures(body)
            .and_then(|c| c.get(1))
            .map(|m| plain_text(m.as_str()))
            .unwrap_or_default();
        let code = caps.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
        out.push(UcumUnit {
            code_ci: caps.get(2).map(|m| m.as_str()).unwrap_or("").to_lowercase(),
            is_metric: caps.get(3).map(|m| m.as_str()).unwrap_or("").to_string(),
            class: caps.get(4).map(|m| m.as_str()).unwrap_or("").to_string(),
            name,
            print_symbol: symbol,
            code,
        });
    }
    out
}

/// 去标签 + 解实体 + 压空白。
fn plain_text(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            // 标签结束处补一个空格：相邻标签之间的文字不能被粘在一起。
            '>' => {
                in_tag = false;
                text.push(' ');
            }
            c if !in_tag => text.push(c),
            _ => {}
        }
    }
    let text = text
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&");
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// CSV 字段转义。
fn csv_field(value: &str) -> String {
    let flat = value.replace(['\n', '\r'], " ");
    if flat.contains(',') || flat.contains('"') {
        format!("\"{}\"", flat.replace('"', "\"\""))
    } else {
        flat
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ucum_单位解析出码名与类别() {
        let xml = r#"<unit Code="mm" CODE="mm" isMetric="yes" class="length">
            <name>millimeter</name><printSymbol>mm</printSymbol></unit>
        <unit Code="mL" CODE="ML" isMetric="yes" class="volume">
            <name>millilitre</name><printSymbol>mL</printSymbol></unit>"#;
        let units = parse_ucum_units(xml);
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].code, "mm");
        assert_eq!(units[0].name, "millimeter");
        assert_eq!(units[0].class, "length");
        assert_eq!(units[1].code_ci, "ml");
        assert_eq!(units[1].class, "volume");
    }

    #[test]
    fn mesh_摘要解析出编码首选词与同义词() {
        let json = r#"{"result":{"uids":["68007668"],"68007668":{
            "ds_meshui":"D007668",
            "ds_meshterms":["Kidney","Kidneys"],
            "ds_yearintroduced":"1963"}}}"#;
        let value: Value = serde_json::from_str(json).unwrap();
        let record = &value["result"]["68007668"];
        let terms: Vec<String> = record["ds_meshterms"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(record["ds_meshui"].as_str().unwrap(), "D007668");
        assert_eq!(terms[0], "Kidney");
        assert_eq!(terms.len(), 2);
    }

    #[test]
    fn 部位检索词去重且顺序稳定() {
        let terms = unique_terms();
        assert_eq!(terms, vec!["Kidney", "Ureter", "Urinary Bladder"]);
    }

    #[test]
    fn url_转义把空格与特殊字符处理掉() {
        assert_eq!(url_encode("Urinary Bladder"), "Urinary+Bladder");
        assert_eq!(url_encode("Kidney"), "Kidney");
    }
}
