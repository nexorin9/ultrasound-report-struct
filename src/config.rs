//! 配置 + 凭据（环境变量）+ 启动自检。
//!
//! **口径**（与 `spec.md` §2.3 / §7 一致）：
//!
//! | 内容 | 来源 |
//! |---|---|
//! | 投递目录、状态库路径、词典路径、RIS / EMR 端点、超时、轮询间隔 | **配置文件** `config.toml`（路径由 `--config` 指定） |
//! | Bearer token、企微 webhook | **环境变量**（`RIS_API_TOKEN` / `EMR_API_TOKEN` / `WECOM_WEBHOOK_URL` / `DAEMON_API_TOKEN`） |
//!
//! **凭据绝不落配置文件**：配置里出现密钥形状的键（token / secret / password / webhook …）
//! 直接判失败（`CONFIG_SECRET_IN_FILE`），不是「读了再忽略」，而是**启动即拒** ——
//! 配置文件常常会被人打包发到科室群里，密钥一旦进去就等于泄漏。
//!
//! **启动自检**（[`startup_checks`）把「能不能起来」这件事变成可复核的清单：
//! 每项带**稳定错误码**，缺项逐条列出，进程以非零码退出 —— 运维在值班机上
//! 看到的是「缺 RIS_API_TOKEN」而不是一段栈回溯。

mod toml_lite;

use std::fs;
use std::path::{Path, PathBuf};

use toml_lite::Value;

/// 启动自检失败时的退出码（与「用法错误」的 2 区分）。
pub const CONFIG_EXIT_CODE: u8 = 3;

/// 状态库默认路径（运行期数据，已被 `.gitignore` 排除）。
pub const DEFAULT_STATE_DB: &str = "data/state.sqlite";
/// 投递目录默认路径。
pub const DEFAULT_INBOX_DIR: &str = "inbox";
/// 词典目录默认路径。
pub const DEFAULT_DICTIONARY_DIR: &str = "assets/dictionary";
/// 部位词典文件默认路径。
pub const DEFAULT_PARTS_FILE: &str = "assets/dictionary/parts.json";
/// 测项词典文件默认路径。
pub const DEFAULT_ITEMS_FILE: &str = "assets/dictionary/items.json";
/// 目录轮询间隔默认值（毫秒）—— 事件到达延迟 ≤ 该值。
pub const DEFAULT_TICK_INTERVAL_MS: u64 = 1000;
/// 出站请求超时默认值（秒）。
pub const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 10;
/// 守护并发上限默认值。
pub const DEFAULT_MAX_CONCURRENCY: usize = 4;
/// **回写退避重试次数**默认值（`spec.md` §4 的上限就是 2）。
pub const DEFAULT_WRITEBACK_MAX_RETRIES: usize = 2;
/// **回写退避间隔基数**默认值（毫秒；第 1 次退避 200ms、第 2 次 400ms）。
pub const DEFAULT_WRITEBACK_BACKOFF_MS: u64 = 200;
/// 内置确认回执端点默认监听地址（仅内网）。
pub const DEFAULT_REVIEW_BIND: &str = "127.0.0.1:8731";

/// 出站读端点环境变量名。
pub const ENV_RIS_BASE_URL: &str = "RIS_BASE_URL";
/// 出站读凭据环境变量名。
pub const ENV_RIS_API_TOKEN: &str = "RIS_API_TOKEN";
/// 出站写端点环境变量名。
pub const ENV_EMR_BASE_URL: &str = "EMR_BASE_URL";
/// 出站写凭据环境变量名。
pub const ENV_EMR_API_TOKEN: &str = "EMR_API_TOKEN";
/// 出站推送 webhook 环境变量名。
pub const ENV_WECOM_WEBHOOK_URL: &str = "WECOM_WEBHOOK_URL";
/// 内置确认回执端点鉴权环境变量名。
pub const ENV_DAEMON_API_TOKEN: &str = "DAEMON_API_TOKEN";
/// 状态库路径环境变量名（可覆盖配置文件）。
pub const ENV_DAEMON_STATE_DB: &str = "DAEMON_STATE_DB";

/// 配置加载错误（稳定错误码 + 中文说明）。
#[derive(Clone, Debug)]
pub enum ConfigError {
    /// 配置文件读不到。
    FileNotFound { path: PathBuf },
    /// 配置文件语法错误。
    Parse { line: usize, message: String },
    /// 未知节名。
    UnknownSection { section: String },
    /// 未知键名。
    UnknownKey { section: String, key: String },
    /// 取值类型不对。
    ValueType { key: String, expected: &'static str },
    /// 取值越界。
    OutOfRange { key: String, message: String },
    /// 配置文件里出现密钥形状的键。
    SecretInFile { key: String },
}

impl ConfigError {
    /// 稳定错误码（进日志与运维排查）。
    pub fn code(&self) -> &'static str {
        match self {
            ConfigError::FileNotFound { .. } => "CONFIG_FILE_NOT_FOUND",
            ConfigError::Parse { .. } => "CONFIG_PARSE_ERROR",
            ConfigError::UnknownSection { .. } => "CONFIG_UNKNOWN_SECTION",
            ConfigError::UnknownKey { .. } => "CONFIG_UNKNOWN_KEY",
            ConfigError::ValueType { .. } => "CONFIG_VALUE_TYPE",
            ConfigError::OutOfRange { .. } => "CONFIG_OUT_OF_RANGE",
            ConfigError::SecretInFile { .. } => "CONFIG_SECRET_IN_FILE",
        }
    }

    /// 中文说明。
    pub fn message(&self) -> String {
        match self {
            ConfigError::FileNotFound { path } => format!("配置文件不存在：{}", path.display()),
            ConfigError::Parse { line, message } => format!("第 {} 行：{}", line, message),
            ConfigError::UnknownSection { section } => {
                format!("未知节 [{}]（可用：inbox / state / dictionary / outbound / review）", section)
            }
            ConfigError::UnknownKey { section, key } => format!("未知键：{}.{}", section, key),
            ConfigError::ValueType { key, expected } => {
                format!("{} 取值类型不对，应为{}", key, expected)
            }
            ConfigError::OutOfRange { key, message } => format!("{}：{}", key, message),
            ConfigError::SecretInFile { key } => format!(
                "{} 是密钥形状的键，凭据只从环境变量读，配置文件内不落密钥",
                key
            ),
        }
    }
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "错误码={} 说明={}", self.code(), self.message())
    }
}

impl std::error::Error for ConfigError {}

/// 运行配置（路径与端点）。
#[derive(Clone, Debug)]
pub struct Config {
    /// 超声工作站投递目录。
    pub inbox_dir: PathBuf,
    /// 状态库路径（SQLite）。
    pub state_db: PathBuf,
    /// 词典目录。
    pub dictionary_dir: PathBuf,
    /// 部位词典文件。
    pub parts_file: PathBuf,
    /// 测项词典文件。
    pub items_file: PathBuf,
    /// RIS 端点（出站读历史报告）。
    pub ris_base_url: Option<String>,
    /// EMR 端点（出站写结构化报告）。
    pub emr_base_url: Option<String>,
    /// 出站请求超时（秒）。
    pub request_timeout_secs: u64,
    /// 目录轮询间隔（毫秒）。
    pub tick_interval_ms: u64,
    /// 守护并发上限。
    pub max_concurrency: usize,
    /// **回写退避重试次数**（额外重试次数，上限 2）。
    pub writeback_max_retries: usize,
    /// **回写退避间隔基数**（毫秒；第 n 次退避 = 基数 × 2^(n-1)；0 = 不等待）。
    pub writeback_backoff_ms: u64,
    /// 内置确认回执端点监听地址。
    pub review_bind: String,
    /// 配置文件来源路径（`None` = 用内置默认值）。
    pub source_path: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            inbox_dir: PathBuf::from(DEFAULT_INBOX_DIR),
            state_db: PathBuf::from(DEFAULT_STATE_DB),
            dictionary_dir: PathBuf::from(DEFAULT_DICTIONARY_DIR),
            parts_file: PathBuf::from(DEFAULT_PARTS_FILE),
            items_file: PathBuf::from(DEFAULT_ITEMS_FILE),
            ris_base_url: None,
            emr_base_url: None,
            request_timeout_secs: DEFAULT_REQUEST_TIMEOUT_SECS,
            tick_interval_ms: DEFAULT_TICK_INTERVAL_MS,
            max_concurrency: DEFAULT_MAX_CONCURRENCY,
            writeback_max_retries: DEFAULT_WRITEBACK_MAX_RETRIES,
            writeback_backoff_ms: DEFAULT_WRITEBACK_BACKOFF_MS,
            review_bind: DEFAULT_REVIEW_BIND.to_string(),
            source_path: None,
        }
    }
}

impl Config {
    /// 从配置文件加载（缺失的键用默认值；未知键 / 越界值直接报错）。
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text = fs::read_to_string(path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => ConfigError::FileNotFound {
                path: path.to_path_buf(),
            },
            _ => ConfigError::Parse {
                line: 0,
                message: format!("读取配置文件失败：{}", e),
            },
        })?;
        let mut config = Config {
            source_path: Some(path.to_path_buf()),
            ..Config::default()
        };
        config.apply_toml(&text)?;
        Ok(config)
    }

    /// 应用配置文件内容。
    fn apply_toml(&mut self, text: &str) -> Result<(), ConfigError> {
        let parsed = toml_lite::parse(text)
            .map_err(|e| ConfigError::Parse {
                line: e.line,
                message: e.message,
            })?;

        const KNOWN_SECTIONS: [&str; 5] = ["inbox", "state", "dictionary", "outbound", "review"];

        // 未知节直接报错（打错节名不该被静默忽略）。
        for section in parsed.sections.keys() {
            if !KNOWN_SECTIONS.contains(&section.as_str()) {
                return Err(ConfigError::UnknownSection {
                    section: section.clone(),
                });
            }
        }

        for section in KNOWN_SECTIONS {
            let table = match parsed.section(section) {
                Some(table) => table,
                None => continue,
            };
            for (key, value) in table {
                // 密钥形状的键 —— 启动即拒，不静默忽略。
                if is_secret_key(key) {
                    return Err(ConfigError::SecretInFile {
                        key: format!("{}.{}", section, key),
                    });
                }
                self.apply_kv(section, key, value)?;
            }
        }
        Ok(())
    }

    /// 应用一个键值。
    fn apply_kv(&mut self, section: &str, key: &str, value: &Value) -> Result<(), ConfigError> {
        let full = format!("{}.{}", section, key);
        match (section, key) {
            ("inbox", "dir") => {
                self.inbox_dir = PathBuf::from(expect_str(&full, value)?);
            }
            ("inbox", "tick_interval_ms") => {
                let ms = expect_u64(&full, value)?;
                if ms == 0 {
                    return Err(ConfigError::OutOfRange {
                        key: full,
                        message: "轮询间隔必须大于 0 毫秒".to_string(),
                    });
                }
                self.tick_interval_ms = ms;
            }
            ("state", "db") => {
                self.state_db = PathBuf::from(expect_str(&full, value)?);
            }
            ("dictionary", "dir") => {
                self.dictionary_dir = PathBuf::from(expect_str(&full, value)?);
            }
            ("dictionary", "parts_file") => {
                self.parts_file = PathBuf::from(expect_str(&full, value)?);
            }
            ("dictionary", "items_file") => {
                self.items_file = PathBuf::from(expect_str(&full, value)?);
            }
            ("outbound", "ris_base_url") => {
                self.ris_base_url = Some(expect_str(&full, value)?);
            }
            ("outbound", "emr_base_url") => {
                self.emr_base_url = Some(expect_str(&full, value)?);
            }
            ("outbound", "request_timeout_secs") => {
                let secs = expect_u64(&full, value)?;
                if secs == 0 {
                    return Err(ConfigError::OutOfRange {
                        key: full,
                        message: "出站超时必须大于 0 秒".to_string(),
                    });
                }
                self.request_timeout_secs = secs;
            }
            ("outbound", "max_concurrency") => {
                let n = expect_u64(&full, value)?;
                if n == 0 {
                    return Err(ConfigError::OutOfRange {
                        key: full,
                        message: "并发上限必须大于 0".to_string(),
                    });
                }
                self.max_concurrency = n as usize;
            }
            ("outbound", "writeback_max_retries") => {
                let n = expect_u64(&full, value)?;
                if n as usize > crate::outbound::retry::MAX_WRITEBACK_RETRIES {
                    return Err(ConfigError::OutOfRange {
                        key: full,
                        message: format!(
                            "回写重试次数不得超过 {}（退避重试是兜底，不是无限重试）",
                            crate::outbound::retry::MAX_WRITEBACK_RETRIES
                        ),
                    });
                }
                self.writeback_max_retries = n as usize;
            }
            ("outbound", "writeback_backoff_ms") => {
                let ms = expect_u64(&full, value)?;
                self.writeback_backoff_ms = ms;
            }
            ("review", "bind") => {
                self.review_bind = expect_str(&full, value)?;
            }
            (_, _) => {
                return Err(ConfigError::UnknownKey {
                    section: section.to_string(),
                    key: key.to_string(),
                })
            }
        }
        Ok(())
    }

    /// 环境变量覆盖（端点与状态库路径）；**凭据不在这里** —— 凭据只由
    /// [`Credentials`] 从环境变量读，配置文件永远不参与。
    pub fn apply_env_overrides(&mut self) {
        if let Ok(url) = non_empty_env(ENV_RIS_BASE_URL) {
            self.ris_base_url = Some(url);
        }
        if let Ok(url) = non_empty_env(ENV_EMR_BASE_URL) {
            self.emr_base_url = Some(url);
        }
        if let Ok(path) = non_empty_env(ENV_DAEMON_STATE_DB) {
            self.state_db = PathBuf::from(path);
        }
    }

    /// 状态库路径（`--state-db` > 环境变量 > 配置文件 > 默认值）。
    pub fn state_db_path(&self, cli_override: Option<&Path>) -> PathBuf {
        match cli_override {
            Some(p) => p.to_path_buf(),
            None => self.state_db.clone(),
        }
    }
}

/// 凭据（**只**从环境变量读）。
#[derive(Clone, Debug, Default)]
pub struct Credentials {
    /// RIS Bearer token。
    pub ris_api_token: Option<String>,
    /// EMR Bearer token。
    pub emr_api_token: Option<String>,
    /// 企业微信群机器人 webhook。
    pub wecom_webhook_url: Option<String>,
    /// 内置确认回执端点鉴权 token。
    pub daemon_api_token: Option<String>,
}

impl Credentials {
    /// 从进程环境读凭据（空字符串视作**未提供**）。
    pub fn from_env() -> Credentials {
        Credentials {
            ris_api_token: non_empty_env(ENV_RIS_API_TOKEN).ok(),
            emr_api_token: non_empty_env(ENV_EMR_API_TOKEN).ok(),
            wecom_webhook_url: non_empty_env(ENV_WECOM_WEBHOOK_URL).ok(),
            daemon_api_token: non_empty_env(ENV_DAEMON_API_TOKEN).ok(),
        }
    }

    /// 缺失的必需环境变量名（按 profile 判定）。
    ///
    /// `Daemon` 档要求四条出站 / 回执凭据齐备；`Offline` 档（本地解析 / 导出）不要求 ——
    /// 离线跑一遍解析不该被「医院里还没配 token」拦住。
    pub fn missing_for(&self, profile: Profile) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if profile == Profile::Daemon {
            if self.ris_api_token.is_none() {
                missing.push(ENV_RIS_API_TOKEN);
            }
            if self.emr_api_token.is_none() {
                missing.push(ENV_EMR_API_TOKEN);
            }
            if self.wecom_webhook_url.is_none() {
                missing.push(ENV_WECOM_WEBHOOK_URL);
            }
            if self.daemon_api_token.is_none() {
                missing.push(ENV_DAEMON_API_TOKEN);
            }
        }
        missing
    }
}

/// 运行档位。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    /// 离线档：解析 / 导出 / 人工核对，不连院内系统，不要求凭据。
    Offline,
    /// 守护档：常驻监听 + 出站读 / 写 / 推，要求凭据齐备。
    Daemon,
}

/// 单项自检结果。
#[derive(Clone, Debug)]
pub struct CheckOutcome {
    /// 稳定错误码（运维按码聚合）。
    pub code: &'static str,
    /// 是否通过。
    pub ok: bool,
    /// 是否致命（致命项不通过 → 进程非零退出）。
    pub fatal: bool,
    /// 中文说明。
    pub message: String,
}

/// 启动自检报告。
#[derive(Clone, Debug, Default)]
pub struct StartupReport {
    /// 逐项结果。
    pub items: Vec<CheckOutcome>,
}

impl StartupReport {
    /// 是否存在致命失败。
    pub fn has_failure(&self) -> bool {
        self.items.iter().any(|item| !item.ok && item.fatal)
    }

    /// 致命失败项。
    pub fn failures(&self) -> Vec<&CheckOutcome> {
        self.items
            .iter()
            .filter(|item| !item.ok && item.fatal)
            .collect()
    }

    /// 渲染成逐行可读的清单（一行一项，便于 grep 与粘贴到工单）。
    ///
    /// 状态三档：`通过` / `警告`（不阻断，但运维看得见）/ `失败`（致命，进程退出）。
    pub fn render(&self) -> String {
        self.items
            .iter()
            .map(|item| {
                let status = if item.ok {
                    "通过"
                } else if item.fatal {
                    "失败"
                } else {
                    "警告"
                };
                format!(
                    "启动自检 错误码={} 状态={} 说明={}",
                    item.code, status, item.message
                )
            })
            .collect::<Vec<String>>()
            .join("\n")
    }
}

/// 启动自检：词典文件存在且可解析、状态库父目录可写、必需环境变量齐备。
///
/// 缺项**逐条**列出（不是「配置有问题」），每项带稳定错误码；
/// 致命项不通过时调用方应以 [`CONFIG_EXIT_CODE`] 退出。
pub fn startup_checks(config: &Config, creds: &Credentials, profile: Profile) -> StartupReport {
    let mut report = StartupReport::default();
    let daemon = profile == Profile::Daemon;

    // 1) 配置来源
    report.items.push(CheckOutcome {
        code: "CONFIG_LOADED",
        ok: true,
        fatal: false,
        message: match &config.source_path {
            Some(path) => format!("配置已加载：{}", path.display()),
            None => "未指定配置文件，使用内置默认值".to_string(),
        },
    });

    // 2) 凭据齐备性
    let missing = creds.missing_for(profile);
    report.items.push(CheckOutcome {
        code: "ENV_REQUIRED",
        ok: missing.is_empty(),
        fatal: daemon,
        message: if missing.is_empty() {
            format!("必需环境变量齐备（{}档）", profile_name(profile))
        } else {
            format!("缺少环境变量：{}", missing.join("、"))
        },
    });

    // 3) 状态库父目录可写
    let parent = config
        .state_db
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    match ensure_writable_dir(&parent) {
        Ok(()) => report.items.push(CheckOutcome {
            code: "STATE_DB_WRITABLE",
            ok: true,
            fatal: false,
            message: format!("状态库目录可写：{}", parent.display()),
        }),
        Err(message) => report.items.push(CheckOutcome {
            code: "STATE_DB_NOT_WRITABLE",
            ok: false,
            fatal: daemon,
            message,
        }),
    }

    // 4) 词典文件存在且可解析
    for (path, label) in [
        (&config.parts_file, "部位词典"),
        (&config.items_file, "测项词典"),
    ] {
        if !path.is_file() {
            report.items.push(CheckOutcome {
                code: "DICT_MISSING",
                ok: false,
                fatal: daemon,
                message: format!("{}文件不存在：{}", label, path.display()),
            });
            continue;
        }
        match fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(_) => report.items.push(CheckOutcome {
                    code: "DICT_PARSED",
                    ok: true,
                    fatal: false,
                    message: format!("{}可解析：{}", label, path.display()),
                }),
                Err(err) => report.items.push(CheckOutcome {
                    code: "DICT_PARSE_FAILED",
                    ok: false,
                    fatal: daemon,
                    message: format!("{}解析失败（{}）：{}", label, path.display(), err),
                }),
            },
            Err(err) => report.items.push(CheckOutcome {
                code: "DICT_UNREADABLE",
                ok: false,
                fatal: daemon,
                message: format!("{}读不到（{}）：{}", label, path.display(), err),
            }),
        }
    }

    // 5) 投递目录（守护档才要求存在）
    if daemon {
        if config.inbox_dir.is_dir() {
            report.items.push(CheckOutcome {
                code: "INBOX_PRESENT",
                ok: true,
                fatal: false,
                message: format!("投递目录就绪：{}", config.inbox_dir.display()),
            });
        } else {
            report.items.push(CheckOutcome {
                code: "INBOX_MISSING",
                ok: false,
                fatal: true,
                message: format!("投递目录不存在：{}", config.inbox_dir.display()),
            });
        }
    }

    report
}

/// 档位中文名。
fn profile_name(profile: Profile) -> &'static str {
    match profile {
        Profile::Offline => "离线",
        Profile::Daemon => "守护",
    }
}

/// 确保目录存在且可写（用真实探针文件判定，不看权限位的口头承诺）。
///
/// 目录不存在时**创建**它 —— 状态库目录属于运行期数据（已 `.gitignore`），
/// 守护进程本来也要有；创建失败才判失败。
fn ensure_writable_dir(dir: &Path) -> Result<(), String> {
    if !dir.exists() {
        fs::create_dir_all(dir).map_err(|e| format!("目录不可创建（{}）：{}", dir.display(), e))?;
    }
    if !dir.is_dir() {
        return Err(format!("不是目录：{}", dir.display()));
    }
    let probe = dir.join(format!(".urs-write-probe-{}", std::process::id()));
    match fs::write(&probe, b"probe") {
        Ok(()) => {
            let _ = fs::remove_file(&probe);
            Ok(())
        }
        Err(e) => Err(format!("目录不可写（{}）：{}", dir.display(), e)),
    }
}

/// 键名是否带密钥形状（token / secret / password / webhook / credential …）。
fn is_secret_key(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(|ch| *ch != '_' && *ch != '-')
        .flat_map(|ch| ch.to_lowercase())
        .collect();
    ["token", "secret", "password", "passwd", "webhook", "apikey", "credential"]
        .iter()
        .any(|word| normalized.contains(word))
}

/// 取环境变量（空值视作未提供）。
fn non_empty_env(name: &str) -> Result<String, ()> {
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(()),
    }
}

/// 取字符串值。
fn expect_str(key: &str, value: &Value) -> Result<String, ConfigError> {
    value
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| ConfigError::ValueType {
            key: key.to_string(),
            expected: "带引号字符串",
        })
}

/// 取非负整数值。
fn expect_u64(key: &str, value: &Value) -> Result<u64, ConfigError> {
    value.as_u64().ok_or_else(|| ConfigError::ValueType {
        key: key.to_string(),
        expected: "非负整数",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn write_temp(name: &str, body: &str) -> PathBuf {
        let path = env::temp_dir().join(format!("urs-cfg-{}-{}", std::process::id(), name));
        fs::write(&path, body).unwrap();
        path
    }

    /// 自检用配置：状态库指向临时目录 —— 自检会确保目录存在，
    /// 别让单元测试在项目目录里留下 `data/`。
    fn cfg_for_checks(tag: &str) -> Config {
        let dir = env::temp_dir().join(format!("urs-checks-{}-{}", std::process::id(), tag));
        Config {
            state_db: dir.join("state.sqlite"),
            ..Config::default()
        }
    }

    #[test]
    fn config_默认值覆盖默认路径与间隔() {
        let cfg = Config::default();
        assert_eq!(cfg.inbox_dir, PathBuf::from(DEFAULT_INBOX_DIR));
        assert_eq!(cfg.state_db, PathBuf::from(DEFAULT_STATE_DB));
        assert_eq!(cfg.tick_interval_ms, DEFAULT_TICK_INTERVAL_MS);
        assert_eq!(cfg.request_timeout_secs, DEFAULT_REQUEST_TIMEOUT_SECS);
        assert_eq!(cfg.max_concurrency, DEFAULT_MAX_CONCURRENCY);
        assert!(cfg.ris_base_url.is_none());
        assert!(cfg.source_path.is_none());
    }

    #[test]
    fn config_配置文件覆盖默认值() {
        let path = write_temp(
            "override.toml",
            r#"
[inbox]
dir = "/data/urs/inbox"
tick_interval_ms = 2000

[state]
db = "/var/lib/urs/state.sqlite"

[dictionary]
parts_file = "/opt/urs/parts.json"
items_file = "/opt/urs/items.json"

[outbound]
ris_base_url = "https://ris.intranet"
emr_base_url = "https://emr.intranet"
request_timeout_secs = 25
max_concurrency = 8

[review]
bind = "127.0.0.1:9000"
"#,
        );
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.inbox_dir, PathBuf::from("/data/urs/inbox"));
        assert_eq!(cfg.tick_interval_ms, 2000);
        assert_eq!(cfg.state_db, PathBuf::from("/var/lib/urs/state.sqlite"));
        assert_eq!(cfg.parts_file, PathBuf::from("/opt/urs/parts.json"));
        assert_eq!(cfg.items_file, PathBuf::from("/opt/urs/items.json"));
        assert_eq!(cfg.ris_base_url.as_deref(), Some("https://ris.intranet"));
        assert_eq!(cfg.emr_base_url.as_deref(), Some("https://emr.intranet"));
        assert_eq!(cfg.request_timeout_secs, 25);
        assert_eq!(cfg.max_concurrency, 8);
        assert_eq!(cfg.review_bind, "127.0.0.1:9000");
        assert_eq!(cfg.source_path.as_deref(), Some(path.as_path()));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn config_未写的键走默认值() {
        let path = write_temp("partial.toml", "[inbox]\ndir = \"/tmp/inbox\"\n");
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.inbox_dir, PathBuf::from("/tmp/inbox"));
        assert_eq!(cfg.tick_interval_ms, DEFAULT_TICK_INTERVAL_MS);
        assert_eq!(cfg.state_db, PathBuf::from(DEFAULT_STATE_DB));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn config_未知键报错不静默忽略() {
        let path = write_temp("unknown.toml", "[inbox]\ndirr = \"/tmp/inbox\"\n");
        let err = Config::load(&path).unwrap_err();
        assert_eq!(err.code(), "CONFIG_UNKNOWN_KEY");
        assert!(err.message().contains("dirr"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn config_未知节报错() {
        let path = write_temp("unknown-section.toml", "[unknown]\nx = 1\n");
        let err = Config::load(&path).unwrap_err();
        assert_eq!(err.code(), "CONFIG_UNKNOWN_SECTION");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn config_配置文件出现密钥形状的键判失败() {
        let path = write_temp("secret.toml", "[outbound]\nris_api_token = \"abcdefghijklmnopqrst\"\n");
        let err = Config::load(&path).unwrap_err();
        assert_eq!(err.code(), "CONFIG_SECRET_IN_FILE");
        assert!(err.message().contains("环境变量"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn config_类型不对与越界值报错() {
        let path = write_temp("type.toml", "[inbox]\ntick_interval_ms = \"fast\"\n");
        let err = Config::load(&path).unwrap_err();
        assert_eq!(err.code(), "CONFIG_VALUE_TYPE");
        let _ = fs::remove_file(&path);

        let path2 = write_temp("range.toml", "[inbox]\ntick_interval_ms = 0\n");
        let err2 = Config::load(&path2).unwrap_err();
        assert_eq!(err2.code(), "CONFIG_OUT_OF_RANGE");
        let _ = fs::remove_file(&path2);
    }

    #[test]
    fn config_文件不存在报稳定错误码() {
        let err = Config::load(Path::new("/definitely/not/here/config.toml")).unwrap_err();
        assert_eq!(err.code(), "CONFIG_FILE_NOT_FOUND");
    }

    #[test]
    fn config_端点可被环境变量覆盖() {
        let mut cfg = Config::default();
        env::set_var(ENV_RIS_BASE_URL, "https://ris.from-env");
        env::set_var(ENV_DAEMON_STATE_DB, "/tmp/from-env.sqlite");
        cfg.apply_env_overrides();
        env::remove_var(ENV_RIS_BASE_URL);
        env::remove_var(ENV_DAEMON_STATE_DB);
        assert_eq!(cfg.ris_base_url.as_deref(), Some("https://ris.from-env"));
        assert_eq!(cfg.state_db, PathBuf::from("/tmp/from-env.sqlite"));
    }

    #[test]
    fn startup_缺出站读令牌时自检失败并列出变量名() {
        let cfg = cfg_for_checks("missing-token");
        let creds = Credentials {
            ris_api_token: None,
            emr_api_token: Some("emr-token-value".to_string()),
            wecom_webhook_url: Some("https://wecom.local/hook".to_string()),
            daemon_api_token: Some("daemon-token-value".to_string()),
        };
        let report = startup_checks(&cfg, &creds, Profile::Daemon);
        assert!(report.has_failure(), "缺 RIS 令牌时必须判失败");
        let rendered = report.render();
        assert!(rendered.contains("RIS_API_TOKEN"), "报告须列出缺失变量名：{}", rendered);
        assert!(rendered.contains("ENV_REQUIRED"));
        let failures = report.failures();
        assert!(failures.iter().any(|f| f.code == "ENV_REQUIRED"));
    }

    #[test]
    fn startup_离线档不要求凭据() {
        let cfg = cfg_for_checks("offline");
        let creds = Credentials::default();
        let report = startup_checks(&cfg, &creds, Profile::Offline);
        let env_item = report
            .items
            .iter()
            .find(|i| i.code == "ENV_REQUIRED")
            .unwrap();
        assert!(env_item.ok, "离线档不该因缺凭据失败");
        assert!(!report.has_failure());
    }

    #[test]
    fn startup_词典文件缺失时守护档判失败() {
        let cfg = Config {
            parts_file: PathBuf::from("/definitely/not/here/parts.json"),
            items_file: PathBuf::from("/definitely/not/here/items.json"),
            ..cfg_for_checks("dict-missing")
        };
        let creds = Credentials {
            ris_api_token: Some("a".into()),
            emr_api_token: Some("b".into()),
            wecom_webhook_url: Some("c".into()),
            daemon_api_token: Some("d".into()),
        };
        let report = startup_checks(&cfg, &creds, Profile::Daemon);
        assert!(report.has_failure());
        let rendered = report.render();
        assert!(rendered.contains("DICT_MISSING"), "{}", rendered);
    }

    #[test]
    fn startup_词典文件存在但解析失败时报错() {
        let dir = env::temp_dir().join(format!("urs-dict-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("parts.json");
        fs::write(&bad, "{ not json ").unwrap();
        let cfg = Config {
            parts_file: bad.clone(),
            items_file: dir.join("items.json"),
            ..cfg_for_checks("dict-parse")
        };
        fs::write(&cfg.items_file, "{}").unwrap();
        let report = startup_checks(&cfg, &Credentials::default(), Profile::Offline);
        let rendered = report.render();
        assert!(rendered.contains("DICT_PARSE_FAILED"), "{}", rendered);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn startup_状态库目录不可写时判失败() {
        // 指向一个「文件」而不是目录 —— 建目录必然失败。
        let blocker = env::temp_dir().join(format!("urs-blocker-{}.txt", std::process::id()));
        fs::write(&blocker, b"x").unwrap();
        let cfg = Config {
            state_db: blocker.join("state.sqlite"),
            ..Config::default()
        };
        let report = startup_checks(&cfg, &Credentials::default(), Profile::Offline);
        let rendered = report.render();
        assert!(rendered.contains("STATE_DB_NOT_WRITABLE"), "{}", rendered);
        let _ = fs::remove_file(&blocker);
    }

    #[test]
    fn startup_凭据齐备时守护档不因凭据失败() {
        let cfg = cfg_for_checks("creds-ok");
        let creds = Credentials {
            ris_api_token: Some("a".into()),
            emr_api_token: Some("b".into()),
            wecom_webhook_url: Some("c".into()),
            daemon_api_token: Some("d".into()),
        };
        let report = startup_checks(&cfg, &creds, Profile::Daemon);
        let env_item = report.items.iter().find(|i| i.code == "ENV_REQUIRED").unwrap();
        assert!(env_item.ok);
    }
}
