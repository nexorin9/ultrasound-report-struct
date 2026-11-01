//! 词法层人工标注对照：token 序列**逐条**断言。
//!
//! 判据有两份，都在 `assets/samples/` 下：
//! - `lexer_samples/lexer-NN.txt` —— 样例正文（中文 / 英文数字 / 单位 / 全角 / 生僻字）；
//! - `lexer_tokens.csv` —— **人工标注**的 token 序列（类别 / 词面 / 原字面 / 起始字符光标 / 结束字符光标）。
//!
//! 「跑通」不等于「对」：词法层切错一个 token，后面的对拍就会拿
//! 「看起来正常却比错了对象」的数值去比历史值 —— 对照表才是判据。

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use ultrasound_report_struct::parse::lexer::{tokenize, TokenKind};

/// 一条人工标注的 token。
#[derive(Debug)]
struct Annotated {
    sample_id: String,
    token_index: usize,
    kind: String,
    text: String,
    raw: String,
    start: usize,
    end: usize,
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// 读人工标注表（无引号 CSV：字段内不含半角逗号）。
fn load_annotations() -> Vec<Annotated> {
    let path = manifest_dir().join("assets/samples/lexer_tokens.csv");
    let csv = std::fs::read_to_string(&path).expect("读人工标注表 assets/samples/lexer_tokens.csv");
    let mut out: Vec<Annotated> = Vec::new();
    for (i, line) in csv.lines().enumerate() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        if i == 0 {
            assert_eq!(
                line, "sample_id,token_index,kind,text,raw,start,end",
                "人工标注表表头变了"
            );
            continue;
        }
        let f: Vec<&str> = line.split(',').collect();
        assert_eq!(f.len(), 7, "标注表第 {} 行列数不对：{}", i + 1, line);
        out.push(Annotated {
            sample_id: f[0].to_string(),
            token_index: f[1].parse().expect("token_index 应为整数"),
            kind: f[2].to_string(),
            text: f[3].to_string(),
            raw: f[4].to_string(),
            start: f[5].parse().expect("start 应为整数"),
            end: f[6].parse().expect("end 应为整数"),
        });
    }
    assert!(!out.is_empty(), "人工标注表是空的");
    out
}

#[test]
fn 词法token序列与人工标注逐条一致() {
    let rows = load_annotations();
    let mut by_sample: BTreeMap<String, Vec<Annotated>> = BTreeMap::new();
    for row in rows {
        by_sample.entry(row.sample_id.clone()).or_default().push(row);
    }
    assert!(
        by_sample.len() >= 7,
        "人工标注表应覆盖 7 份样例，实际 {} 份",
        by_sample.len()
    );

    for (sample_id, rows) in &by_sample {
        // token_index 从 1 起且连续 —— 标注表自身不能有缺口。
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(
                row.token_index,
                i + 1,
                "{} 的 token_index 不连续（第 {} 行标成 {}）",
                sample_id,
                i + 1,
                row.token_index
            );
        }

        let path = manifest_dir().join(format!("assets/samples/lexer_samples/{}.txt", sample_id));
        let text = std::fs::read_to_string(&path).expect("读样例正文");
        let tokens = tokenize(&text);

        assert_eq!(
            tokens.len(),
            rows.len(),
            "{} 的 token 条数与人工标注不一致（实际 {} 条，标注 {} 条）",
            sample_id,
            tokens.len(),
            rows.len()
        );

        for (tok, row) in tokens.iter().zip(rows.iter()) {
            assert_eq!(
                tok.kind.as_str(),
                row.kind,
                "{} 第 {} 个 token 类别不符：期望 {}，实际 {}",
                sample_id,
                row.token_index,
                row.kind,
                tok.kind.as_str()
            );
            assert_eq!(
                tok.text, row.text,
                "{} 第 {} 个 token 归一词面不符：期望 {:?}，实际 {:?}",
                sample_id, row.token_index, row.text, tok.text
            );
            assert_eq!(
                tok.raw, row.raw,
                "{} 第 {} 个 token 原字面不符：期望 {:?}，实际 {:?}",
                sample_id, row.token_index, row.raw, tok.raw
            );
            assert_eq!(
                tok.start, row.start,
                "{} 第 {} 个 token 起始字符光标不符：期望 {}，实际 {}",
                sample_id, row.token_index, row.start, tok.start
            );
            assert_eq!(
                tok.end, row.end,
                "{} 第 {} 个 token 结束字符光标不符：期望 {}，实际 {}",
                sample_id, row.token_index, row.end, tok.end
            );
        }
    }
}

#[test]
fn 每份样例正文都有对应的人工标注() {
    let rows = load_annotations();
    let annotated: BTreeSet<String> = rows.iter().map(|r| r.sample_id.clone()).collect();

    let dir = manifest_dir().join("assets/samples/lexer_samples");
    let mut files: Vec<String> = std::fs::read_dir(&dir)
        .expect("读样例目录")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "txt").unwrap_or(false))
        .map(|e| {
            e.path()
                .file_stem()
                .expect("样例文件名应有主干")
                .to_string_lossy()
                .to_string()
        })
        .collect();
    files.sort();
    assert!(!files.is_empty(), "样例目录是空的");

    for f in &files {
        assert!(
            annotated.contains(f),
            "样例 {} 没有人工标注 —— 判据缺一条就是漏检",
            f
        );
    }
    for a in &annotated {
        assert!(
            files.contains(a),
            "人工标注里的 {} 找不到对应样例正文",
            a
        );
    }
}

#[test]
fn 人工标注覆盖全部token类别() {
    // 十类 token 都要有样例 —— 少一类就意味着该类别没有人工判据。
    let rows = load_annotations();
    let covered: BTreeSet<&str> = rows.iter().map(|r| r.kind.as_str()).collect();
    let all = [
        TokenKind::Number,
        TokenKind::Unit,
        TokenKind::Punct,
        TokenKind::Multiply,
        TokenKind::Modifier,
        TokenKind::Part,
        TokenKind::Item,
        TokenKind::Section,
        TokenKind::Term,
        TokenKind::Word,
    ];
    for kind in all {
        assert!(
            covered.contains(kind.as_str()),
            "token 类别 {} 没有人工标注样例",
            kind.as_str()
        );
    }
}
