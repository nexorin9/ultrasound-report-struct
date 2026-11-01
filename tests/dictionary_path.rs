//! 测项词典的**配置路径真的被用上** —— 集成验收（Task 25）。
//!
//! **这一组用例守什么**：
//!
//! 1. **词典文件是真源**：配置文件里指定的 `dictionary.items_file` 必须被用上 ——
//!    日志里的 `source=` 得是那个路径，不是默认路径。运维改了词典文件却没生效、
//!    且没有任何提示，是这类系统最阴的一种「静默失败」。
//! 2. **词典坏了当场退出**：词典文件存在却结构不合法 → 进程以退出码 3 退出，
//!    错误码 `DICT_SCHEMA_INVALID` —— **不退回旧词典继续跑**（那会让口径变了
//!    却看不出来）。
//! 3. **词典版本可切换**：换一份词典文件（版本号不同）→ 生效版本跟着变，
//!    台账行落的版本随之变化 —— 口径留痕是真的在留。
//!
//! 与单元测试的差别：这里起的是**真的可执行文件**，断言的是**真的退出码**
//! 与**真的日志行**。

use std::path::{Path, PathBuf};
use std::process::Command;

/// 被测可执行文件（cargo 为集成测试注入）。
const BIN: &str = env!("CARGO_BIN_EXE_ultrasound-report-struct");

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("urs-dict-path-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 项目根目录（编译期固定）。
fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// 写一份只含 `[dictionary]` 段的配置文件（其余段走默认值）。
fn write_config(dir: &Path, items_file: &Path, parts_file: &Path) -> PathBuf {
    let path = dir.join("config.toml");
    std::fs::write(
        &path,
        format!(
            "[dictionary]\ndir = \"{}\"\nparts_file = \"{}\"\nitems_file = \"{}\"\n",
            dir.display(),
            parts_file.display(),
            items_file.display()
        ),
    )
    .unwrap();
    path
}

#[test]
fn 配置指定的测项词典路径真的被用上() {
    let dir = temp_dir("honored");
    let parts = project_root().join("assets/dictionary/parts.json");
    let items = project_root().join("assets/dictionary/items.json");
    let config = write_config(&dir, &items, &parts);

    let out = Command::new(BIN)
        .args(["--config", config.to_str().unwrap()])
        .args(["parse", "--text", "左肾前后径 2.1cm"])
        .output()
        .expect("进程应能起");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "应成功退出：\n{}\n{}",
        stdout,
        stderr
    );
    // 日志里的来源必须是**配置指定的那个路径**，不是默认路径。
    assert!(
        stdout.contains(&format!("source=文件：{}", items.display())),
        "日志应显示配置指定的测项词典路径：\n{}",
        stdout
    );
    assert!(
        stdout.contains("version=items-0.1.0"),
        "日志应显示测项词典版本：\n{}",
        stdout
    );
    // 词典真的生效了：测项码照常归一。
    assert!(
        stdout.contains("item_code=kidney.ap_diameter"),
        "测项码应照常归一：\n{}",
        stdout
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn 词典文件坏了当场退出而不是退回旧词典() {
    let dir = temp_dir("broken");
    let parts = project_root().join("assets/dictionary/parts.json");
    let bad_items = dir.join("items.json");
    // 结构不合法：缺 entries（测项词面表）。
    std::fs::write(&bad_items, "{\"version\":\"items-9.9.9\"}").unwrap();
    let config = write_config(&dir, &bad_items, &parts);

    let out = Command::new(BIN)
        .args(["--config", config.to_str().unwrap()])
        .args(["parse", "--text", "左肾前后径 2.1cm"])
        .output()
        .expect("进程应能起");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(3),
        "词典坏了应退出码 3：\n{}\n{}",
        stdout,
        stderr
    );
    assert!(
        stderr.contains("DICT_SCHEMA_INVALID"),
        "应报稳定错误码 DICT_SCHEMA_INVALID：\n{}",
        stderr
    );
    assert!(
        stderr.contains("测项词典"),
        "应说清是哪本词典坏了：\n{}",
        stderr
    );
    assert!(
        !stdout.contains("item_code="),
        "词典坏了就不该再解析出条目：\n{}",
        stdout
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn 换一份词典版本生效且未收录的测项词显式失败() {
    let dir = temp_dir("version");
    let parts = project_root().join("assets/dictionary/parts.json");
    // 一份**内容合法、版本号不同**的测项词典：多收一个别名「长度」→
    // 「前后径」都不影响，这里只验证版本号与生效词典跟着换。
    let items = dir.join("items.json");
    std::fs::write(
        &items,
        r#"{
  "version": "items-9.9.9",
  "entries": [
    {"alias": "前后径", "suffix": "ap_diameter", "bases": ["kidney", "renal_pelvis"]},
    {"alias": "残余尿量", "suffix": "residual_urine_volume", "bases": ["bladder"]}
  ]
}
"#,
    )
    .unwrap();
    let config = write_config(&dir, &items, &parts);

    let db = dir.join("state.sqlite");
    let out = Command::new(BIN)
        .args(["--config", config.to_str().unwrap()])
        .args(["--state-db", db.to_str().unwrap()])
        .args(["init-db"])
        .output()
        .expect("进程应能起");
    assert!(
        out.status.success(),
        "init-db 应成功：\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("version=items-9.9.9"),
        "生效词典版本应跟着文件换：\n{}",
        String::from_utf8_lossy(&out.stdout)
    );

    // 词典里没收录的测项词 → 显式失败条目（ITEM_DICT_MISSING），不静默丢弃。
    let out = Command::new(BIN)
        .args(["--config", config.to_str().unwrap()])
        .args(["--state-db", db.to_str().unwrap()])
        .args(["parse", "--text", "左肾长 8.5cm"])
        .output()
        .expect("进程应能起");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("ITEM_DICT_MISSING"),
        "词典没收录的测项词应显式失败：\n{}",
        stdout
    );
    assert!(
        stdout.contains("item_code=-"),
        "没收录就不该编出测项码：\n{}",
        stdout
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn 词典文件缺失时退回编译期内置的同一份() {
    // 值班机只拷可执行文件时：词典文件不在 → 退回内置副本，且**不是另一份口径**。
    let dir = temp_dir("fallback");
    let parts = project_root().join("assets/dictionary/parts.json");
    let missing = dir.join("no-such-items.json");
    let config = write_config(&dir, &missing, &parts);

    let out = Command::new(BIN)
        .args(["--config", config.to_str().unwrap()])
        .args(["parse", "--text", "左肾前后径 2.1cm"])
        .output()
        .expect("进程应能起");

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "词典文件缺失不该失败：\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("内置副本"),
        "词典文件缺失应退回内置副本：\n{}",
        stdout
    );
    assert!(
        stdout.contains("item_code=kidney.ap_diameter"),
        "退回内置副本后口径应一致：\n{}",
        stdout
    );

    let _ = std::fs::remove_dir_all(&dir);
}
