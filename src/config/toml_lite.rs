//! TOML **子集**解析器（配置文件格式自管，刻意不引 `toml` crate）。
//!
//! **为什么自己写**：依赖面刻意收窄到 6 个 crate（见 `plan.md` §1「依赖拉取失败」风险项），
//! 配置文件是本产品自己的格式，子集够用且完全可测；引第三方 TOML 依赖会让「拉不到依赖」
//! 这条风险重新进来。
//!
//! 支持的子集：
//! - `[节]` 节头；
//! - `键 = 值`，值支持：带引号字符串（`"..."`，转义 `\" \\ \n \t \r`）、整数、浮点、布尔；
//! - `#` 注释（整行或行尾，字符串内的 `#` 不算注释）。
//!
//! **不支持**数组、内联表、多行字符串、点号键 —— 出现即报错，**不静默忽略**
//! （运维打错一个键名却悄悄用默认值，是配置类事故的经典来源）。

use std::collections::BTreeMap;

/// 解析出的取值。
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// 带引号字符串。
    Str(String),
    /// 整数。
    Int(i64),
    /// 浮点数。
    Float(f64),
    /// 布尔值。
    Bool(bool),
}

impl Value {
    /// 取字符串（类型不符返回 `None`）。
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// 取非负整数（类型不符返回 `None`）。
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Value::Int(i) if *i >= 0 => Some(*i as u64),
            _ => None,
        }
    }
}

/// 解析结果：节 → 键 → 取值。
#[derive(Clone, Debug, Default)]
pub struct Parsed {
    /// 各节的键值表（节名 → 键值）。
    pub sections: BTreeMap<String, BTreeMap<String, Value>>,
}

impl Parsed {
    /// 取某节下的键值。
    pub fn section(&self, name: &str) -> Option<&BTreeMap<String, Value>> {
        self.sections.get(name)
    }
}

/// 解析错误（带 1 基行号）。
#[derive(Clone, Debug)]
pub struct TomlError {
    /// 1 基行号。
    pub line: usize,
    /// 错误说明。
    pub message: String,
}

/// 解析一段 TOML 子集文本。
pub fn parse(text: &str) -> Result<Parsed, TomlError> {
    let mut sections: BTreeMap<String, BTreeMap<String, Value>> = BTreeMap::new();
    let mut current = String::new();

    for (idx, raw_line) in text.lines().enumerate() {
        let line_no = idx + 1;
        let line = strip_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }

        // 节头
        if line.starts_with('[') {
            if !line.ends_with(']') {
                return Err(TomlError {
                    line: line_no,
                    message: format!("节头缺少右方括号：{}", line),
                });
            }
            let name = line[1..line.len() - 1].trim().to_string();
            if !is_valid_name(&name) {
                return Err(TomlError {
                    line: line_no,
                    message: format!("非法节名：{}", name),
                });
            }
            current = name.clone();
            sections.entry(current.clone()).or_default();
            continue;
        }

        // 键 = 值
        let eq = match line.find('=') {
            Some(pos) => pos,
            None => {
                return Err(TomlError {
                    line: line_no,
                    message: format!("缺少等号：{}", line),
                })
            }
        };
        let key = line[..eq].trim().to_string();
        let value_raw = line[eq + 1..].trim();

        if !is_valid_name(&key) {
            return Err(TomlError {
                line: line_no,
                message: format!("非法键名：{}", key),
            });
        }
        if current.is_empty() {
            return Err(TomlError {
                line: line_no,
                message: format!("键 {} 必须写在某个 [节] 下", key),
            });
        }

        let value = parse_value(value_raw).map_err(|message| TomlError {
            line: line_no,
            message,
        })?;

        let table = sections.entry(current.clone()).or_default();
        if table.contains_key(&key) {
            return Err(TomlError {
                line: line_no,
                message: format!("重复键：{}.{}", current, key),
            });
        }
        table.insert(key, value);
    }

    Ok(Parsed { sections })
}

/// 去掉行尾注释（字符串内的 `#` 不算注释）。
fn strip_comment(line: &str) -> &str {
    let mut in_string = false;
    let mut escaped = false;
    for (idx, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '#' if !in_string => return &line[..idx],
            _ => {}
        }
    }
    line
}

/// 键名 / 节名合法性：只允许字母、数字、下划线与短横线。
fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

/// 解析一个取值。
fn parse_value(raw: &str) -> Result<Value, String> {
    if raw.is_empty() {
        return Err("取值为空".to_string());
    }
    if raw.starts_with('"') {
        let (text, rest) = parse_quoted(raw)?;
        if !rest.trim().is_empty() {
            return Err(format!("字符串后有多余内容：{}", rest.trim()));
        }
        return Ok(Value::Str(text));
    }
    if raw.starts_with('[') || raw.starts_with('{') {
        return Err(format!("不支持数组或内联表：{}", raw));
    }
    match raw {
        "true" => return Ok(Value::Bool(true)),
        "false" => return Ok(Value::Bool(false)),
        _ => {}
    }
    if let Ok(i) = raw.parse::<i64>() {
        return Ok(Value::Int(i));
    }
    if let Ok(f) = raw.parse::<f64>() {
        if f.is_finite() {
            return Ok(Value::Float(f));
        }
        return Err(format!("非法数值：{}", raw));
    }
    Err(format!("无法识别的取值（字符串需加引号）：{}", raw))
}

/// 解析带引号字符串，返回（字符串内容, 剩余未消费部分）。
fn parse_quoted(raw: &str) -> Result<(String, &str), String> {
    let bytes = raw.as_bytes();
    let mut idx = 1usize; // 跳过起始引号
    let mut out = String::new();
    while idx < bytes.len() {
        match bytes[idx] {
            b'"' => return Ok((out, &raw[idx + 1..])),
            b'\\' => {
                idx += 1;
                let esc = *bytes.get(idx).ok_or_else(|| "字符串未闭合".to_string())?;
                match esc {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'n' => out.push('\n'),
                    b't' => out.push('\t'),
                    b'r' => out.push('\r'),
                    other => {
                        return Err(format!("不支持的转义：\\{}", other as char));
                    }
                }
                idx += 1;
            }
            _ => {
                let ch = raw[idx..]
                    .chars()
                    .next()
                    .ok_or_else(|| "字符串未闭合".to_string())?;
                out.push(ch);
                idx += ch.len_utf8();
            }
        }
    }
    Err("字符串未闭合".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 解析节与键值() {
        let text = r#"
[inbox]
dir = "inbox"
tick_interval_ms = 1000

[outbound]
ris_base_url = "https://ris.example.internal"
max_concurrency = 4
"#;
        let parsed = parse(text).unwrap();
        let inbox = parsed.section("inbox").unwrap();
        assert_eq!(inbox.get("dir"), Some(&Value::Str("inbox".to_string())));
        assert_eq!(inbox.get("tick_interval_ms"), Some(&Value::Int(1000)));
        let outbound = parsed.section("outbound").unwrap();
        assert_eq!(
            outbound.get("ris_base_url"),
            Some(&Value::Str("https://ris.example.internal".to_string()))
        );
        assert_eq!(outbound.get("max_concurrency"), Some(&Value::Int(4)));
    }

    #[test]
    fn 注释被剥掉且字符串内的井号保留() {
        let text = r#"
[inbox]  # 投递目录
dir = "in#box" # 行尾注释
"#;
        let parsed = parse(text).unwrap();
        let inbox = parsed.section("inbox").unwrap();
        assert_eq!(inbox.get("dir"), Some(&Value::Str("in#box".to_string())));
    }

    #[test]
    fn 布尔与浮点可解析() {
        let parsed = parse("[a]\nflag = true\nratio = 0.25\n").unwrap();
        let sec = parsed.section("a").unwrap();
        assert_eq!(sec.get("flag"), Some(&Value::Bool(true)));
        assert_eq!(sec.get("ratio"), Some(&Value::Float(0.25)));
    }

    #[test]
    fn 缺等号报错并带行号() {
        let err = parse("[a]\nkey_without_value\n").unwrap_err();
        assert_eq!(err.line, 2);
        assert!(err.message.contains("等号"));
    }

    #[test]
    fn 字符串未加引号报错() {
        let err = parse("[a]\nkey = inbox\n").unwrap_err();
        assert_eq!(err.line, 2);
        assert!(err.message.contains("引号"));
    }

    #[test]
    fn 重复键报错() {
        let err = parse("[a]\nk = 1\nk = 2\n").unwrap_err();
        assert_eq!(err.line, 3);
        assert!(err.message.contains("重复键"));
    }

    #[test]
    fn 未归属节的键报错() {
        let err = parse("k = 1\n").unwrap_err();
        assert_eq!(err.line, 1);
        assert!(err.message.contains("节"));
    }

    #[test]
    fn 数组与内联表不被静默忽略() {
        let err = parse("[a]\nk = [1, 2]\n").unwrap_err();
        assert!(err.message.contains("数组"));
        let err2 = parse("[a]\nk = {x = 1}\n").unwrap_err();
        assert!(err2.message.contains("内联表"));
    }
}
