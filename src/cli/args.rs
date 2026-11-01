//! 全局参数与子命令参数解析。
//!
//! **全局参数只有一份语义**（[`Globals`]）——
//! 散在各个子命令里各写一遍，早晚写出两个版本（有的认 `--dry-run`、
//! 有的不认；运维敲错一个参数，报错还说不清是哪个子命令的）。

use std::path::PathBuf;

/// 全局参数（子命令无关）。
#[derive(Debug, Default)]
pub struct Globals {
    /// 配置文件路径。
    pub config: Option<PathBuf>,
    /// 状态库路径（命令行覆盖）。
    pub state_db: Option<PathBuf>,
    /// **演练模式**：不落状态库、不写出站报文。
    pub dry_run: bool,
    /// 是否按守护档要求凭据齐备。
    pub require_credentials: bool,
}

/// 拆出全局参数（其余参数留给子命令）。
///
/// **用法错误在这里就报错**（缺取值 / 拼错），不留给子命令去猜 ——
/// 子命令拿到的参数一定是干净的。
pub fn split_globals(args: &[String]) -> Result<(Globals, Vec<String>), String> {
    let mut globals = Globals::default();
    let mut rest: Vec<String> = Vec::new();
    let mut idx = 0usize;
    while idx < args.len() {
        match args[idx].as_str() {
            "--config" | "--state-db" => {
                let key = args[idx].clone();
                idx += 1;
                let value = args
                    .get(idx)
                    .cloned()
                    .ok_or_else(|| format!("{} 缺少取值", key))?;
                if key == "--config" {
                    globals.config = Some(PathBuf::from(value));
                } else {
                    globals.state_db = Some(PathBuf::from(value));
                }
            }
            "--dry-run" => globals.dry_run = true,
            "--require-credentials" => globals.require_credentials = true,
            other => rest.push(other.to_string()),
        }
        idx += 1;
    }
    Ok((globals, rest))
}

/// 取 `--<name> <值>` 形式的参数值。
pub fn opt_value(args: &[String], name: &str) -> Result<Option<String>, String> {
    let mut idx = 0usize;
    while idx < args.len() {
        if args[idx] == name {
            idx += 1;
            return match args.get(idx) {
                Some(v) => Ok(Some(v.clone())),
                None => Err(format!("{} 缺少取值", name)),
            };
        }
        idx += 1;
    }
    Ok(None)
}

/// 判断参数里有没有 `--help` / `-h`。
pub fn wants_help(args: &[String]) -> bool {
    args.iter().any(|a| a == "--help" || a == "-h")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn 全局参数被拆出来() {
        let (globals, rest) = split_globals(&args(&[
            "--config",
            "c.toml",
            "--state-db",
            "d.sqlite",
            "--dry-run",
            "parse",
            "--text",
            "左肾 3.2cm",
        ]))
        .unwrap();
        assert_eq!(globals.config, Some(PathBuf::from("c.toml")));
        assert_eq!(globals.state_db, Some(PathBuf::from("d.sqlite")));
        assert!(globals.dry_run);
        assert!(!globals.require_credentials);
        assert_eq!(rest, vec!["parse", "--text", "左肾 3.2cm"]);
    }

    #[test]
    fn 全局参数缺取值即报错() {
        assert!(split_globals(&args(&["--config"])).is_err());
        assert!(split_globals(&args(&["--state-db"])).is_err());
    }

    #[test]
    fn 子命令参数取不到就是空() {
        assert_eq!(opt_value(&args(&["--text", "abc"]), "--text").unwrap(), Some("abc".to_string()));
        assert_eq!(opt_value(&args(&[]), "--text").unwrap(), None);
        assert!(opt_value(&args(&["--text"]), "--text").is_err());
    }
}
