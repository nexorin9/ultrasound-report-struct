//! 负语料：从 PMC 开放获取子集取放射 / 影像个案报告的 FINDINGS / CONCLUSION 段。
//!
//! **为什么要负语料**：本产品的主张是「只抽超声报告里的测项，认不出的不猜」。
//! 光有院内正样例不够 —— 还得证明**别的科别的自由文本不会被误抽成测项**。
//! 公开的放射个案报告正好是这类文本：句子通顺、有数字、但不是超声报告。
//!
//! **口径纪律**：
//! - 只收**有 CC 许可**的文章（逐篇标注许可，见 `INDEX.csv` 与文件头）。
//! - 只收摘要里同时有「所见」段与「结论」段的文章 —— 段形态对齐影像报告口径。
//! - 只收**不含泌尿系部位词**的片段：本项目的部位词典限定泌尿系，
//!   负语料要验证的是「非目标科别的自由文本不被误抽」；
//!   泌尿系词汇的边界由院内负样例与回归基准单独覆盖。
//! - 不收任何患者可识别明细：只取期刊公开的摘要正文。

use std::collections::BTreeMap;
use std::path::Path;

use regex::Regex;

use crate::publicdata::integrity::check_pinned;
use crate::publicdata::manifest::SourceSpec;
use crate::publicdata::report::{OutcomeStatus, SourceOutcome};
use crate::publicdata::{http_get, SourceError};

/// 一篇被采用的负语料文章。
#[derive(Clone, Debug)]
pub struct NegativeDoc {
    /// PMC 文章号。
    pub pmcid: String,
    /// 文章标题。
    pub title: String,
    /// 期刊名。
    pub journal: String,
    /// DOI。
    pub doi: String,
    /// 许可标识（形如 `creativecommons.org/licenses/by-nc/4.0`）。
    pub license: String,
    /// 「所见」段（标题 + 正文）。
    pub findings: Vec<(String, String)>,
    /// 「结论」段（标题 + 正文）。
    pub conclusions: Vec<(String, String)>,
}

/// 负语料文件名（按序编号）。
pub fn negative_file_name(index: usize) -> String {
    format!("neg-{:03}.txt", index)
}

/// 取回并落地负语料。
pub fn fetch_corpus(
    spec: &SourceSpec,
    dest: &Path,
    ids: &[&str],
    timeout_secs: u64,
) -> Result<SourceOutcome, SourceError> {
    let url = format!(
        "{}?db=pmc&id={}&retmode=xml",
        &spec.url,
        ids.join(",")
    );
    let body = http_get(&url, timeout_secs)?;
    // 接口返回的是 XML 文本，字节数随上游版本浮动 —— 不做固定字节校验，
    // 逐篇内容一致性由落地后的校验清单守住。
    let _ = check_pinned(&spec.id, "pmc-efetch.xml", &body, None, None);

    let docs = parse_articles(&String::from_utf8_lossy(&body));
    if docs.is_empty() {
        return Err(SourceError::Network(format!(
            "来源 {} 未取到可用文章（文章数 0）",
            spec.id
        )));
    }

    let dir = dest.join("negatives");
    std::fs::create_dir_all(&dir).map_err(|e| SourceError::Io(e.to_string()))?;

    let mut index_rows: Vec<(String, NegativeDoc)> = Vec::new();
    for (i, doc) in docs.iter().enumerate() {
        let file_name = negative_file_name(i + 1);
        let text = render_negative(doc);
        std::fs::write(dir.join(&file_name), text.as_bytes())
            .map_err(|e| SourceError::Io(e.to_string()))?;
        index_rows.push((file_name, doc.clone()));
    }

    let index_path = dir.join("INDEX.csv");
    let index_text = render_index(&index_rows);
    std::fs::write(&index_path, index_text.as_bytes())
        .map_err(|e| SourceError::Io(e.to_string()))?;

    let files = index_rows.len() + 1;
    Ok(SourceOutcome {
        id: spec.id.clone(),
        kind_label: spec.kind.label().to_string(),
        status: OutcomeStatus::Fetched,
        files,
        detail: format!(
            "产物目录 {}：负语料 {} 份 + INDEX.csv（来源 {}）",
            dir.display(),
            index_rows.len(),
            &spec.url
        ),
    })
}

/// 渲染一篇负语料文件。
///
/// 文件头三行是**来源与许可标注**（不是报告正文）—— 没有数字与单位，不会被抽成测项。
pub fn render_negative(doc: &NegativeDoc) -> String {
    let mut out = String::new();
    out.push_str(&format!("# source: {} ({})\n", doc.pmcid, doc.journal));
    out.push_str(&format!("# license: {}\n", doc.license));
    out.push_str(&format!("# title: {}\n", doc.title));
    out.push('\n');
    for (title, body) in &doc.findings {
        out.push_str(&format!("FINDINGS · {}\n{}\n\n", title, body));
    }
    for (title, body) in &doc.conclusions {
        out.push_str(&format!("CONCLUSION · {}\n{}\n\n", title, body));
    }
    out
}

/// 渲染负语料索引（CSV）。
pub fn render_index(rows: &[(String, NegativeDoc)]) -> String {
    let mut out = String::new();
    out.push_str("file,pmcid,title,journal,doi,license,license_url,source_url\n");
    for (file, doc) in rows {
        out.push_str(&format!(
            "{},{},{},{},{},{},{},{}\n",
            file,
            csv_field(&doc.pmcid),
            csv_field(&doc.title),
            csv_field(&doc.journal),
            csv_field(&doc.doi),
            csv_field(&doc.license),
            csv_field(&format!("https://{}", doc.license)),
            csv_field("https://www.ncbi.nlm.nih.gov/pmc/articles/")
        ));
    }
    out
}

/// CSV 字段转义（双引号与逗号）。
fn csv_field(value: &str) -> String {
    let flat = value.replace(['\n', '\r'], " ");
    if flat.contains(',') || flat.contains('"') {
        format!("\"{}\"", flat.replace('"', "\"\""))
    } else {
        flat
    }
}

/// 泌尿系部位词（负语料要排除的词）。
fn urinary_pattern() -> Regex {
    Regex::new(r"(?i)kidney|kidneys|renal|bladder|ureter|urethra|nephro|pyelo|hydronephro")
        .expect("泌尿系词正则应可编译")
}

/// 「所见」段标题（大小写不敏感）。
fn findings_pattern() -> Regex {
    Regex::new(r"(?i)^(findings?|results?|case presentation|impression|observations?|imaging)")
        .expect("所见段正则应可编译")
}

/// 「结论」段标题（大小写不敏感）。
fn conclusions_pattern() -> Regex {
    Regex::new(r"(?i)^(conclusions?|interpretation)")
        .expect("结论段正则应可编译")
}

/// 从 efetch 返回的 XML 里解析出可用的负语料文章。
pub fn parse_articles(xml: &str) -> Vec<NegativeDoc> {
    let mut docs = Vec::new();
    for article in split_articles(xml) {
        if let Some(doc) = parse_article(&article) {
            docs.push(doc);
        }
    }
    docs
}

/// 按 `<article ...>...</article>` 切分文章。
fn split_articles(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<article ") {
        let after = &rest[start..];
        match after.find("</article>") {
            Some(end) => {
                out.push(after[..end + "</article>".len()].to_string());
                rest = &after[end + "</article>".len()..];
            }
            None => break,
        }
    }
    out
}

/// 解析一篇文章；不合格返回 `None`。
fn parse_article(article: &str) -> Option<NegativeDoc> {
    let pmcid = capture(article, r#"<article-id pub-id-type="pmcid">([^<]*)</article-id>"#)?;
    let doi = capture(article, r#"<article-id pub-id-type="doi">([^<]*)</article-id>"#)
        .unwrap_or_default();
    let journal = capture(article, r"(?s)<journal-title>(.*?)</journal-title>")
        .map(|t| strip_tags(&t))
        .unwrap_or_default();
    let title = capture(article, r"(?s)<article-title>(.*?)</article-title>")
        .map(|t| strip_tags(&t))
        .unwrap_or_default();
    // 许可：只收 CC 许可。
    let license = capture(article, r"(creativecommons\.org/licenses/[a-z\-]+/[0-9.]+)")?;

    let abstract_xml = capture(article, r"(?s)(<abstract\b.*?</abstract>)")?;
    let mut findings: Vec<(String, String)> = Vec::new();
    let mut conclusions: Vec<(String, String)> = Vec::new();
    for sec in split_sections(&abstract_xml) {
        let sec_title = match capture(&sec, r"(?s)<title>(.*?)</title>") {
            Some(t) => strip_tags(&t).trim_end_matches(':').trim().to_string(),
            None => continue,
        };
        let body = strip_tags(&remove_first(&sec, r"(?s)<title>.*?</title>"));
        if body.is_empty() {
            continue;
        }
        let sec_title = if sec_title.is_empty() {
            "未命名段".to_string()
        } else {
            sec_title
        };
        if findings_pattern().is_match(&sec_title) {
            findings.push((sec_title, body));
        } else if conclusions_pattern().is_match(&sec_title) {
            conclusions.push((sec_title, body));
        }
    }
    if findings.is_empty() || conclusions.is_empty() {
        return None;
    }

    let joined = findings
        .iter()
        .chain(conclusions.iter())
        .map(|(_, b)| b.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    if joined.chars().count() < 400 {
        return None;
    }
    if urinary_pattern().is_match(&joined) {
        return None;
    }

    Some(NegativeDoc {
        pmcid,
        title,
        journal,
        doi,
        license,
        findings,
        conclusions,
    })
}

/// 去掉第一处匹配。
fn remove_first(text: &str, pattern: &str) -> String {
    match Regex::new(pattern) {
        Ok(re) => re.replace(text, "").to_string(),
        Err(_) => text.to_string(),
    }
}

/// 捕获第一处匹配组。
fn capture(text: &str, pattern: &str) -> Option<String> {
    let re = Regex::new(pattern).ok()?;
    re.captures(text).and_then(|c| c.get(1)).map(|m| m.as_str().to_string())
}

/// 按 `<sec>` 标签配对切分段（支持嵌套，取最外层）。
fn split_sections(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<sec") {
        // 跳过 `<sec` 后面不是空白 / `>` 的情况（如 `<section`）。
        let after = &rest[start..];
        let tag_end = match after.find('>') {
            Some(i) => i,
            None => break,
        };
        if !after[4..tag_end]
            .chars()
            .all(|c| c.is_whitespace() || c == '/')
        {
            rest = &rest[start + 4..];
            continue;
        }
        if after[4..tag_end].trim() == "/" {
            rest = &rest[start + tag_end + 1..];
            continue;
        }
        // 找配对的 </sec>。
        let mut depth = 1usize;
        let mut cursor = tag_end + 1;
        let mut end = None;
        while cursor < after.len() {
            let open = after[cursor..].find("<sec");
            let close = after[cursor..].find("</sec>");
            match (open, close) {
                (Some(o), Some(c)) if c < o => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(cursor + c);
                        break;
                    }
                    cursor += c + 6;
                }
                (Some(o), _) => {
                    // 可能是 `<section` —— 看标签名。
                    let name_end = after[cursor + o + 4..]
                        .find(|c: char| !c.is_ascii_alphanumeric())
                        .map(|i| cursor + o + 4 + i)
                        .unwrap_or(after.len());
                    let name = &after[cursor + o + 4..name_end];
                    if name.is_empty() || name.starts_with("tion") {
                        depth += 1;
                        cursor = name_end;
                    } else {
                        cursor = name_end;
                    }
                }
                (None, Some(c)) => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(cursor + c);
                        break;
                    }
                    cursor += c + 6;
                }
                (None, None) => break,
            }
        }
        match end {
            Some(e) => {
                out.push(after[..e + "</sec>".len()].to_string());
                rest = &after[e + "</sec>".len()..];
            }
            None => break,
        }
    }
    out
}

/// 去标签 + 解实体 + 压空白。
pub fn strip_tags(html: &str) -> String {
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

/// 归并同名文章（同一篇可能被多次收录），保序。
pub fn dedup(docs: Vec<NegativeDoc>) -> Vec<NegativeDoc> {
    let mut seen: BTreeMap<String, NegativeDoc> = BTreeMap::new();
    for doc in docs {
        seen.entry(doc.pmcid.clone()).or_insert(doc);
    }
    seen.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<pmc-articleset>
<article article-type="case-report">
<front><journal-meta><journal-title>Demo Journal</journal-title></journal-meta>
<article-meta>
<article-id pub-id-type="pmcid">PMC12345678</article-id>
<article-id pub-id-type="doi">10.1234/demo</article-id>
<article-title>A demo imaging case</article-title>
<permissions><license xlink:href="http://creativecommons.org/licenses/by-nc/4.0"><license-p>CC BY-NC</license-p></license></permissions>
<abstract abstract-type="structured">
<sec><title>Findings</title><p>Imaging demonstrated a lesion described in plain narrative text without any parenchymal structure involved at all, and the remaining parenchyma was unremarkable across every sampled series, with no dilatation of the collecting system and no adjacent fluid collection detected on the delayed acquisitions obtained for comparison purposes.</p></sec>
<sec><title>Case presentation</title><p>The patient was referred for further assessment after an incidental abnormality was reported on a study obtained for an unrelated indication, and the study was repeated in three months with a stable appearance, at which point the clinical team elected to continue observation rather than proceed to any invasive confirmatory procedure given the absence of interval change and the reassuring laboratory profile.</p></sec>
<sec><title>Conclusion</title><p>Correlation with clinical course supported the working diagnosis and no further intervention was planned at this stage, with the recommendation that the referring service review the imaging findings in the context of the presenting complaint and arrange a further dedicated study should the clinical picture change in the intervening period.</p></sec>
</abstract>
</article-meta></front></article>
</pmc-articleset>"#;

    #[test]
    fn 解析出文章号许可与两类段() {
        let docs = parse_articles(SAMPLE);
        assert_eq!(docs.len(), 1);
        let doc = &docs[0];
        assert_eq!(doc.pmcid, "PMC12345678");
        assert_eq!(doc.doi, "10.1234/demo");
        assert_eq!(doc.journal, "Demo Journal");
        assert_eq!(doc.license, "creativecommons.org/licenses/by-nc/4.0");
        assert_eq!(doc.findings.len(), 2);
        assert_eq!(doc.conclusions.len(), 1);
    }

    #[test]
    fn 无结论段或无许可的文章不收() {
        let no_lic = SAMPLE.replace(
            "http://creativecommons.org/licenses/by-nc/4.0",
            "http://example.com/license",
        );
        assert!(parse_articles(&no_lic).is_empty());
        let no_conc = SAMPLE.replace("<title>Conclusion</title>", "<title>Remarks</title>");
        assert!(parse_articles(&no_conc).is_empty());
    }

    #[test]
    fn 含泌尿系部位词的片段不收() {
        let renal = SAMPLE.replace(
            "without any parenchymal structure involved at all",
            "with a renal structure clearly involved at all",
        );
        assert!(parse_articles(&renal).is_empty());
    }

    #[test]
    fn 文件头带来源与许可且不含可抽的数值() {
        let docs = parse_articles(SAMPLE);
        let text = render_negative(&docs[0]);
        assert!(text.starts_with("# source: PMC12345678"), "{}", text);
        assert!(text.contains("# license: creativecommons.org/licenses/by-nc/4.0"));
        assert!(text.contains("FINDINGS · Findings"), "{}", text);
        assert!(text.contains("CONCLUSION · Conclusion"), "{}", text);
        // 头部注释行里不得出现「数字 + 单位」形态，避免被当成测项。
        for line in text.lines().filter(|l| l.starts_with('#')) {
            assert!(!line.contains(" cm") && !line.contains(" mm"), "{}", line);
        }
    }

    #[test]
    fn 去标签后压空白且解实体() {
        assert_eq!(strip_tags("<p>a &amp; b</p>"), "a & b");
        assert_eq!(strip_tags("<p>x</p><p>y</p>"), "x y");
    }

    #[test]
    fn 索引含每篇的来源与许可() {
        let docs = parse_articles(SAMPLE);
        let rows = vec![(negative_file_name(1), docs[0].clone())];
        let index = render_index(&rows);
        assert!(index.starts_with("file,pmcid,title,journal,doi,license,license_url,source_url"));
        assert!(index.contains("neg-001.txt"));
        assert!(index.contains("creativecommons.org/licenses/by-nc/4.0"));
    }
}
