//! 启动自检与配置文件的集成测试。
//!
//! 覆盖三条运维关心的路径：
//! 1. **配置文件 → 配置对象 → 启动自检** 的端到端串联；
//! 2. **凭据缺失逐条列出**（缺什么就列什么，不是一句「配置有问题」）；
//! 3. **随仓库发布的样例文件本身必须可解析且不含凭据** ——
//!    样例文件一旦与支持的格式漂移，值班机上第一次启动就会踩坑。

use std::fs;
use std::path::{Path, PathBuf};

use ultrasound_report_struct::config::{
    startup_checks, Config, Credentials, Profile, CONFIG_EXIT_CODE,
};

/// 项目根目录（编译期固定）。
fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// 建一个临时工作目录（词典文件 + 状态库目录 + 配置文件都在里面）。
fn workspace(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("urs-it-{}-{}", std::process::id(), tag));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn 配置文件到自检的端到端串联() {
    let dir = workspace("e2e");

    // 词典文件（可解析的 JSON）
    fs::create_dir_all(dir.join("dictionary")).unwrap();
    fs::write(dir.join("dictionary/parts.json"), "{\"version\":\"parts-0.1.0\"}").unwrap();
    fs::write(dir.join("dictionary/items.json"), "{\"version\":\"items-0.1.0\"}").unwrap();
    fs::create_dir_all(dir.join("state")).unwrap();
    fs::create_dir_all(dir.join("inbox")).unwrap();

    let config_path = dir.join("config.toml");
    fs::write(
        &config_path,
        format!(
            r#"
[inbox]
dir = "{}"
tick_interval_ms = 1500

[state]
db = "{}"

[dictionary]
dir = "{}"
parts_file = "{}"
items_file = "{}"

[outbound]
ris_base_url = "https://ris.intranet"
emr_base_url = "https://emr.intranet"
request_timeout_secs = 15
max_concurrency = 6

[review]
bind = "127.0.0.1:9100"
"#,
            dir.join("inbox").display(),
            dir.join("state/state.sqlite").display(),
            dir.join("dictionary").display(),
            dir.join("dictionary/parts.json").display(),
            dir.join("dictionary/items.json").display(),
        ),
    )
    .unwrap();

    let config = Config::load(&config_path).unwrap();
    assert_eq!(config.tick_interval_ms, 1500);
    assert_eq!(config.request_timeout_secs, 15);
    assert_eq!(config.max_concurrency, 6);
    assert_eq!(config.review_bind, "127.0.0.1:9100");
    assert_eq!(config.ris_base_url.as_deref(), Some("https://ris.intranet"));

    // 凭据齐备 → 守护档自检全过。
    let creds = Credentials {
        ris_api_token: Some("placeholder-token-value".to_string()),
        emr_api_token: Some("placeholder-token-value".to_string()),
        wecom_webhook_url: Some("https://wecom.intranet/hook".to_string()),
        daemon_api_token: Some("placeholder-token-value".to_string()),
    };
    let report = startup_checks(&config, &creds, Profile::Daemon);
    assert!(
        !report.has_failure(),
        "自检不该失败：\n{}",
        report.render()
    );

    // 缺 RIS 令牌 → 逐条列出缺失变量名。
    let creds_missing_ris = Credentials {
        ris_api_token: None,
        ..creds.clone()
    };
    let report = startup_checks(&config, &creds_missing_ris, Profile::Daemon);
    assert!(report.has_failure());
    let rendered = report.render();
    assert!(rendered.contains("RIS_API_TOKEN"), "{}", rendered);
    assert!(
        !rendered.contains("EMR_API_TOKEN 缺少"),
        "只应列出真正缺的那一个：{}",
        rendered
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn 配置文件里出现密钥时拒绝加载() {
    let dir = workspace("secret");
    let config_path = dir.join("config.toml");
    fs::write(
        &config_path,
        "[outbound]\nris_api_token = \"real-looking-secret-value\"\n",
    )
    .unwrap();
    let err = Config::load(&config_path).unwrap_err();
    assert_eq!(err.code(), "CONFIG_SECRET_IN_FILE");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn 随仓库发布的样例配置文件可解析且不含凭据() {
    let example = project_root().join("config.example.toml");
    assert!(example.is_file(), "样例配置文件缺失：{}", example.display());

    // 样例文件必须能被自家解析器读懂（格式漂移会在值班机上第一次启动就暴露）。
    let config = Config::load(&example).expect("样例配置文件应可解析");
    assert_eq!(config.tick_interval_ms, 1000);
    assert_eq!(config.request_timeout_secs, 10);
    assert_eq!(config.max_concurrency, 4);

    // 样例文件里不得出现凭据值（只有地址与参数）。
    let text = fs::read_to_string(&example).unwrap();
    for secret_key in ["token", "secret", "password", "webhook"] {
        assert!(
            !text.to_lowercase().contains(&format!("{} =", secret_key)),
            "样例配置文件里不该出现 {} 键：{}",
            secret_key,
            text
        );
    }
}

#[test]
fn 样例环境变量文件只列变量名不含真实值() {
    let example = project_root().join(".env.example");
    assert!(example.is_file(), "样例环境变量文件缺失：{}", example.display());
    let text = fs::read_to_string(&example).unwrap();

    // 凭据位一律留空 —— 有值就说明有人在样例里留了真东西。
    for name in [
        "RIS_API_TOKEN",
        "EMR_API_TOKEN",
        "WECOM_WEBHOOK_URL",
        "DAEMON_API_TOKEN",
    ] {
        let line = text
            .lines()
            .find(|l| l.starts_with(&format!("{}=", name)))
            .unwrap_or_else(|| panic!("样例环境变量文件应列出 {}", name));
        let value = line.split_once('=').unwrap().1.trim();
        assert!(value.is_empty(), "{} 不应带真实值：{}", name, line);
    }

    // 不得出现长串疑似凭据（20 位以上连续字母数字）。
    for (idx, line) in text.lines().enumerate() {
        let mut run = 0usize;
        for ch in line.chars() {
            if ch.is_ascii_alphanumeric() {
                run += 1;
                assert!(
                    run < 20,
                    "第 {} 行疑似包含凭据值：{}",
                    idx + 1,
                    line
                );
            } else {
                run = 0;
            }
        }
    }
}

#[test]
fn 退出码常量与用法错误区分() {
    // 配置类失败用 3，与「用法错误」的 2 区分，运维脚本可分别处理。
    assert_eq!(CONFIG_EXIT_CODE, 3);
}

#[test]
fn 状态库路径优先级命令行高于配置文件() {
    let dir = workspace("priority");
    let config_path = dir.join("config.toml");
    fs::write(&config_path, "[state]\ndb = \"/from/config.sqlite\"\n").unwrap();
    let config = Config::load(&config_path).unwrap();
    let cli = Path::new("/from/cli.sqlite");
    assert_eq!(config.state_db_path(Some(cli)), cli.to_path_buf());
    assert_eq!(
        config.state_db_path(None),
        PathBuf::from("/from/config.sqlite")
    );
    let _ = fs::remove_dir_all(&dir);
}
