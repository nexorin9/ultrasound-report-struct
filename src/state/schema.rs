//! 状态库 schema 与幂等迁移。
//!
//! 六张表承载**台账 / 条目 / 对拍 / 审计 / 幂等 / 检查号锁**：
//!
//! | 表 | 承载什么 | 关键约束 |
//! |---|---|---|
//! | `reports` | 报告主表（检查号 / 病历号 / 状态 / 词典版本） | `idempotency_key` 唯一索引 + **`study_no` 唯一索引** |
//! | `items` | 结构化条目（部位 / 测项 / 原值原单位 / 归一值 / 强转路径 / 行列） | 外键指向 `reports`，级联删除 |
//! | `comparisons` | 对拍差值（有历史 / 无历史 / 歧义 / 上游不可达） | 外键指向 `reports` |
//! | `audit_log` | 逐动作留痕；**失败条目也落这里**（带错误码 + 行列 + 原文片段） | 外键指向 `reports` |
//! | `idempotency` | 幂等键登记表（键唯一，重复投递在此拦下） | `(idempotency_key, scope)` 唯一索引 |
//! | `study_locks` | **库级检查号锁**（串行化：同一检查号不并发处理） | `study_no` 主键 |
//!
//! 迁移按 `PRAGMA user_version` **递增**执行：重复执行不报错、不重复建表，
//! 进程重启后从库里的版本号续跑。

use rusqlite::Connection;
use std::thread::sleep;
use std::time::Duration;

/// 当前 schema 版本（新增迁移时 +1 并把脚本追加到 [`MIGRATIONS`]）。
pub const SCHEMA_VERSION: i64 = 3;

/// 迁移脚本列表：下标 + 1 即版本号。
const MIGRATIONS: &[&str] = &[MIGRATION_V1, MIGRATION_V2, MIGRATION_V3];

/// **忙等待上限（毫秒）**。
///
/// 守护进程是**多连接**（每个工作线程一条）；「定时批处理与守护进程重叠跑」
/// 还会让**两个进程**同时开同一个库。WAL 下并发写会撞上写锁，
/// 不设忙等待就直接返回 `SQLITE_BUSY` —— 报告被误判成「处理失败」，
/// 守护进程甚至起不来。
const BUSY_TIMEOUT_MS: u64 = 5_000;

/// 撞上写锁时的**重试轮数**与**退避基数（毫秒）**：第 n 轮等 `base * n`。
///
/// `busy_timeout` 已经覆盖了大部分等待；这几轮是兜底 ——
/// 迁移要在**写事务里**做，排队可能比单条语句久。
const BUSY_ATTEMPTS: usize = 5;
const BUSY_BACKOFF_MS: u64 = 40;

/// 是不是「库被别处占着」（`BUSY` / `LOCKED`）。
///
/// 这两种错**重试就能过**；别的错重试没有意义，直接报出去。
fn is_busy(err: &rusqlite::Error) -> bool {
    matches!(
        err.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy) | Some(rusqlite::ErrorCode::DatabaseLocked)
    )
}

/// 版本 1：五张表 + 索引。
const MIGRATION_V1: &str = r#"
CREATE TABLE IF NOT EXISTS reports (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    study_no          TEXT    NOT NULL,
    patient_id        TEXT,
    status            TEXT    NOT NULL,
    source_path       TEXT,
    dictionary_version TEXT   NOT NULL,
    idempotency_key   TEXT    NOT NULL,
    created_at        TEXT    NOT NULL,
    updated_at        TEXT    NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_reports_idempotency ON reports(idempotency_key);
CREATE INDEX IF NOT EXISTS idx_reports_study ON reports(study_no);
CREATE INDEX IF NOT EXISTS idx_reports_status ON reports(status);

CREATE TABLE IF NOT EXISTS items (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    report_id         INTEGER NOT NULL REFERENCES reports(id) ON DELETE CASCADE,
    part_key          TEXT,
    item_code         TEXT,
    value_raw         REAL,
    unit_raw          TEXT,
    value_si          REAL,
    unit_code         TEXT,
    coercion_path     TEXT    NOT NULL,
    line              INTEGER NOT NULL DEFAULT 0,
    column            INTEGER NOT NULL DEFAULT 0,
    char_start        INTEGER NOT NULL DEFAULT 0,
    notes             TEXT,
    dictionary_version TEXT   NOT NULL,
    created_at        TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_items_report ON items(report_id);

CREATE TABLE IF NOT EXISTS comparisons (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    report_id      INTEGER NOT NULL REFERENCES reports(id) ON DELETE CASCADE,
    item_id        INTEGER REFERENCES items(id) ON DELETE SET NULL,
    part_key       TEXT,
    item_code      TEXT,
    previous_value REAL,
    current_value  REAL,
    delta          REAL,
    direction      TEXT,
    unit_code      TEXT,
    state          TEXT    NOT NULL,
    created_at     TEXT    NOT NULL,
    updated_at     TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_comparisons_report ON comparisons(report_id);

CREATE TABLE IF NOT EXISTS audit_log (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    report_id         INTEGER REFERENCES reports(id) ON DELETE CASCADE,
    study_no          TEXT,
    action            TEXT    NOT NULL,
    actor             TEXT,
    error_code        TEXT,
    line              INTEGER,
    column            INTEGER,
    raw_snippet       TEXT,
    input_digest      TEXT,
    dictionary_version TEXT,
    detail            TEXT,
    created_at        TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_audit_report ON audit_log(report_id);
CREATE INDEX IF NOT EXISTS idx_audit_action ON audit_log(action);

CREATE TABLE IF NOT EXISTS idempotency (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    idempotency_key TEXT NOT NULL,
    study_no        TEXT NOT NULL,
    scope           TEXT NOT NULL,
    state           TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_idempotency_key ON idempotency(idempotency_key);
"#;

/// 版本 2：幂等登记的唯一索引改为 **(幂等键, 登记用途)**。
///
/// **为什么改**：同一个幂等键（`sha256(study_no‖报告内容)`）会出现在**两个用途**里 ——
/// `ingest`（这份报告已入库）与 `writeback`（这份报告已回写）。
/// 旧的单列唯一索引会让第二次登记撞唯一约束，
/// 回写通道被误判成「重复投递」而**永远写不出去**。
///
/// 单列索引还有一个更隐蔽的毛病：它把「同一份内容在不同用途下各登记一次」
/// 与「同一份内容在**同一用途**下登记两次」混为一谈 ——
/// 前者合法，后者才是重复投递。索引必须表达这个区别。
const MIGRATION_V2: &str = r#"
DROP INDEX IF EXISTS idx_idempotency_key;
CREATE UNIQUE INDEX IF NOT EXISTS idx_idempotency_key_scope
    ON idempotency(idempotency_key, scope);
CREATE INDEX IF NOT EXISTS idx_idempotency_scope ON idempotency(scope);
"#;

/// 版本 3：**检查号唯一约束** + **库级检查号锁表**。
///
/// **为什么加检查号唯一约束**：
/// `idempotency_key`（`sha256(study_no‖报告内容)`）只挡「**同一份内容**」。
/// 同一检查号但**内容不同**（工作站改过又重导、两个来源推了同一份检查），
/// 幂等键不同 → 挡不住 → 台账里会出现**同一个检查号的两行测值**。
/// 而台账是要跟 RIS 对账、要按「本月积水病例数」统计的东西：
/// 多出来的一行会被当成「这台机器多测了一次」—— 比漏测更难解释。
/// 业务上「一个检查号一份报告」本来就成立（检查号就是这次检查的编号）。
///
/// **为什么加锁表**：
/// 串行化要**跨进程**成立 —— 守护进程与定时批处理可能同时在跑，
/// 进程内那份在途集合（调度器的 `in_flight`）重启即失、也挡不住另一个进程。
/// 库级**租约锁**两头都管住：抢不到就等，等不到就退避重试；
/// 持锁进程崩了，租约到期后锁自动可被接管（不会把检查号永久卡死）。
const MIGRATION_V3: &str = r#"
DROP INDEX IF EXISTS idx_reports_study;
CREATE UNIQUE INDEX IF NOT EXISTS idx_reports_study_no ON reports(study_no);
CREATE TABLE IF NOT EXISTS study_locks (
    study_no    TEXT PRIMARY KEY,
    holder      TEXT NOT NULL,
    acquired_at TEXT NOT NULL,
    expires_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_study_locks_expires ON study_locks(expires_at);
"#;

/// 库里的当前 schema 版本。
pub fn current_version(conn: &Connection) -> Result<i64, rusqlite::Error> {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
}

/// 幂等迁移：只跑版本号大于库内版本的那些脚本。
///
/// 重复执行是**安全**的：版本号已是最新时直接返回，`applied` 为 0。
///
/// **并发迁移**（多个连接同时打开同一个新库）：迁移在 **`BEGIN IMMEDIATE`**
/// 事务里做 —— 立刻拿写锁，而不是「先读后写」再升级锁。
/// 后者在并发下会撞上 SQLite 的**锁升级死锁**，而 `busy_timeout` 对那种情况
/// **不起作用**（直接返回 `BUSY`）；`BEGIN IMMEDIATE` 让并发迁移者排队，
/// 忙等待才正常生效。版本号在**拿到写锁之后**才读 —— 别的连接可能刚迁完，
/// 那时它已经能看到最新版本号，于是直接跳过（不会重复建表）。
pub fn migrate(conn: &Connection) -> Result<usize, rusqlite::Error> {
    let mut last_busy = None;
    for attempt in 0..BUSY_ATTEMPTS {
        match migrate_once(conn) {
            Ok(applied) => return Ok(applied),
            Err(e) if is_busy(&e) => {
                // 别的连接正占着写锁：退避后重抢。
                last_busy = Some(e);
                sleep(Duration::from_millis(BUSY_BACKOFF_MS * (attempt as u64 + 1)));
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_busy.expect("至少跑过一轮"))
}

/// 迁移一轮（外层 [`migrate`] 负责重试）。
fn migrate_once(conn: &Connection) -> Result<usize, rusqlite::Error> {
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let outcome = (|| -> Result<usize, rusqlite::Error> {
        // 版本号要在**拿到写锁之后**再读。
        let current = current_version(conn)?;
        let mut applied = 0usize;
        for (idx, sql) in MIGRATIONS.iter().enumerate() {
            let version = idx as i64 + 1;
            if version <= current {
                continue;
            }
            conn.execute_batch(sql)?;
            conn.execute_batch(&format!("PRAGMA user_version = {};", version))?;
            applied += 1;
        }
        Ok(applied)
    })();
    match outcome {
        Ok(applied) => {
            conn.execute_batch("COMMIT;")?;
            Ok(applied)
        }
        Err(e) => {
            // 失败整体回滚 —— 不留「建了一半的表」。
            let _ = conn.execute_batch("ROLLBACK;");
            Err(e)
        }
    }
}

/// 打开状态库前的初始化：忙等待 + WAL 日志 + 外键约束 + 迁移。
///
/// **顺序有讲究**：忙等待必须**最先**设上 —— 后面每一条语句都可能撞上
/// 别的连接的写锁（多线程开连接、批处理与守护进程重叠跑）。
/// 忙等待设晚了，前面那几条就 unprotected，撞锁直接失败。
pub fn init(conn: &Connection) -> Result<usize, rusqlite::Error> {
    // **第一步**：忙等待（守护进程多连接 + 批处理与守护进程重叠跑都会撞写锁）。
    // 5 秒足够覆盖一次事务（幂等查重 → 写报告 → 写条目 → 写审计）。
    conn.execute_batch(&format!("PRAGMA busy_timeout = {};", BUSY_TIMEOUT_MS))?;

    // **第二步**：WAL 日志。journal_mode 会回一行结果，用 query_row 取而不是 execute。
    // 已是 WAL 的库这条是空操作；新库转换要排他锁，撞上了就退避重试。
    set_journal_mode_wal(conn)?;

    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    migrate(conn)
}

/// 把库切成 WAL 模式；撞上别处的锁就退避重试。
///
/// 已是 WAL 时这条是**空操作**（只读 `journal_mode`，不占锁）；
/// 只有「从别的模式转换」才需要排他锁 —— 那时可能正有别的连接开着它。
fn set_journal_mode_wal(conn: &Connection) -> Result<(), rusqlite::Error> {
    let mut last_busy = None;
    for attempt in 0..BUSY_ATTEMPTS {
        match conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(())) {
            Ok(()) => return Ok(()),
            Err(e) if is_busy(&e) => {
                last_busy = Some(e);
                sleep(Duration::from_millis(BUSY_BACKOFF_MS * (attempt as u64 + 1)));
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_busy.expect("至少跑过一轮"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 首次迁移建出五张表并置版本号() {
        let conn = Connection::open_in_memory().unwrap();
        let applied = init(&conn).unwrap();
        assert_eq!(applied, SCHEMA_VERSION as usize, "首次应跑完全部迁移");
        assert_eq!(current_version(&conn).unwrap(), SCHEMA_VERSION);

        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='table' AND name IN \
                 ('reports','items','comparisons','audit_log','idempotency') ORDER BY name",
            )
            .unwrap();
        let names: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            names,
            vec!["audit_log", "comparisons", "idempotency", "items", "reports"]
        );
    }

    #[test]
    fn 重复执行迁移不报错也不重复建表() {
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        let second = migrate(&conn).unwrap();
        assert_eq!(second, 0, "已是最新版本时不应再执行任何脚本");
        let third = init(&conn).unwrap();
        assert_eq!(third, 0);

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='reports'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn 幂等键唯一索引按键与用途组合() {
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        let idx: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' \
                 AND name='idx_idempotency_key_scope'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(idx, 1, "组合唯一索引应存在");

        // 旧的单列唯一索引必须**已经去掉** ——
        // 留着它，同一把钥匙在 `ingest` 与 `writeback` 两个用途下登记时
        // 第二次会撞唯一约束，回写通道就永远写不出去。
        let old: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_idempotency_key'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(old, 0, "旧单列唯一索引应已删除");

        // 组合索引的列顺序：先键、后用途。
        let cols: String = conn
            .query_row(
                "SELECT group_concat(name, ',') FROM pragma_index_info('idx_idempotency_key_scope')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cols, "idempotency_key,scope");
    }

    #[test]
    fn 版本三建出检查号唯一索引与锁表() {
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), SCHEMA_VERSION);

        // 检查号唯一索引：同一检查号不得落两份报告。
        let idx: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_reports_study_no'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(idx, 1, "检查号唯一索引应存在");

        // 旧的非唯一索引要去掉（同一列两个索引，白白多一份维护成本）。
        let old: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_reports_study'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(old, 0, "旧的非唯一检查号索引应已删除");

        // 锁表：按检查号主键。
        let locks: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='study_locks'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(locks, 1, "库级检查号锁表应存在");
        let cols: String = conn
            .query_row(
                "SELECT group_concat(name, ',') FROM pragma_table_info('study_locks')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cols, "study_no,holder,acquired_at,expires_at");
    }
}
