//! 并发保护：**同检查号串行、不同检查号并行**。
//!
//! ## 这层解决什么
//!
//! 同一份报告在院内会**同时**从两条路进来：
//!
//! | 现实场景 | 谁在撞 |
//! |---|---|
//! | 工作站重发 / 有人手工再点一次导出 | 守护进程的两个 worker |
//! | 定时批处理与守护进程重叠跑 | **两个进程** |
//! | 守护进程重启后续跑 | 新进程与还没退干净的旧进程 |
//!
//! 调度器里那份**进程内**的在途集合（`in_flight`）只挡得住第一种：
//! 进程重启即失，也**挡不住另一个进程**。所以这一层把「同一检查号不并发处理」
//! 落到**库里** —— 锁在 `study_locks` 表，不在进程内存里。
//!
//! ## 三层防线，各管一段
//!
//! | 层 | 落点 | 管什么 |
//! |---|---|---|
//! | **串行化** | 本模块（`study_locks` 租约锁） | 同一检查号**不同时**被处理 |
//! | **去重** | [`crate::workflow::idempotency`] | 同一份**内容**不落第二条台账行 |
//! | **唯一约束** | `reports.study_no` 唯一索引 | 同一**检查号**不落两份报告（兜底） |
//!
//! 三层缺一不可：只有去重的话，两个进程会**同时**去解析同一份报告、
//! 同时去 RIS 取历史值（白花钱，还可能把两次差值算成两次）；
//! 只有串行化的话，进程崩了锁会**永久卡住**那个检查号；
//! 只有唯一约束的话，冲突会变成一条看不懂的数据库报错。
//!
//! ## 租约为什么要过期
//!
//! 持锁的进程崩了，锁行还在库里 —— 那份报告就再也进不了台账，
//! 而「昨天少了一份报告」比「多落一行」更难解释。
//! 所以锁带**租期**：到期后可被接管（[`Repo::try_acquire_study_lock`]）。
//! 正常路径下租约到期前就释放了（[`StudyNoLease`] 的 `Drop`），
//! 过期只用来兜住「进程没了」这一种情况。
//!
//! **不依赖进程内内存锁**：重启后仍成立、跨进程也成立。

use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::sleep;
use std::time::Duration;

use crate::state::repo::{Repo, RepoError, StudyLockRow};

/// 默认租期（秒）。
///
/// 取 60 秒：一次处理（解析 → 强转 → RIS 取数 → 落库）通常在秒级，
/// 60 秒足够覆盖慢的上游；持锁进程崩掉时，这个检查号最多卡 60 秒。
pub const DEFAULT_LEASE_SECS: u64 = 60;

/// 默认重试次数（抢不到锁时退避重试几轮）。
pub const DEFAULT_ATTEMPTS: usize = 5;

/// 默认退避基数（毫秒）：第 n 轮等 `base * n` 毫秒。
pub const DEFAULT_BACKOFF_MS: u64 = 100;

/// 闸实例计数（给持有者标识编个唯一号，便于运维看出是谁占着）。
static GATE_SEQ: AtomicU64 = AtomicU64::new(0);

/// 并发错误。
#[derive(Debug)]
pub enum ConcurrencyError {
    /// 锁**被别人占着**且未过期（这一轮抢不到）。
    Held {
        /// 被锁住的检查号。
        study_no: String,
        /// 当前持有者。
        holder: String,
        /// 租约到期时间。
        expires_at: String,
    },
    /// **退避重试耗尽**：这把锁一直被别人占着，本次不处理。
    Contended {
        /// 被锁住的检查号。
        study_no: String,
        /// 重试了几轮。
        attempts: usize,
    },
    /// 状态库错误。
    Repo(RepoError),
}

impl std::fmt::Display for ConcurrencyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConcurrencyError::Held {
                study_no,
                holder,
                expires_at,
            } => write!(
                f,
                "检查号 {} 正在被 {} 处理（租约到 {}），本次不并发处理",
                study_no, holder, expires_at
            ),
            ConcurrencyError::Contended { study_no, attempts } => write!(
                f,
                "检查号 {} 连续 {} 轮抢不到锁，本次不并发处理",
                study_no, attempts
            ),
            ConcurrencyError::Repo(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for ConcurrencyError {}

impl From<RepoError> for ConcurrencyError {
    fn from(e: RepoError) -> Self {
        ConcurrencyError::Repo(e)
    }
}

/// **检查号租约**：持有期间这把锁归本持有者。
///
/// 释放走 `Drop` —— 处理途中出错、提前 `return`、panic 展开，
/// 锁都会还回去（**不靠记得写释放语句**）。
pub struct StudyNoLease<'a> {
    repo: &'a Repo,
    study_no: String,
    holder: String,
    released: bool,
}

impl<'a> StudyNoLease<'a> {
    /// 被锁住的检查号。
    pub fn study_no(&self) -> &str {
        &self.study_no
    }

    /// 持有者标识。
    pub fn holder(&self) -> &str {
        &self.holder
    }

    /// **显式释放**（`Drop` 里也会释放；重复释放是安全的）。
    ///
    /// 返回 `false` 表示这把锁已经不在本持有者手上了
    /// （租约到期后被别人接管了）—— 这时不许把别人的锁删掉。
    pub fn release(&mut self) -> Result<bool, RepoError> {
        if self.released {
            return Ok(false);
        }
        self.released = true;
        self.repo.release_study_lock(&self.study_no, &self.holder)
    }
}

impl Drop for StudyNoLease<'_> {
    fn drop(&mut self) {
        if !self.released {
            // 释放失败不该炸掉处理流程 —— 锁有租期兜底（到期可被接管）。
            let _ = self.repo.release_study_lock(&self.study_no, &self.holder);
        }
    }
}

impl std::fmt::Debug for StudyNoLease<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StudyNoLease")
            .field("study_no", &self.study_no)
            .field("holder", &self.holder)
            .field("released", &self.released)
            .finish()
    }
}

/// **按检查号的库级串行闸**。
///
/// 用法（处理一份报告之前）：
///
/// ```text
/// let gate = StudyNoGate::new(repo);
/// let lease = gate.acquire(&study_no, DEFAULT_LEASE_SECS, DEFAULT_ATTEMPTS)?;
/// // ……处理（解析 → 强转 → 对拍 → 落库）……
/// drop(lease);   // 锁还回去
/// ```
pub struct StudyNoGate<'a> {
    repo: &'a Repo,
    holder: String,
}

impl<'a> StudyNoGate<'a> {
    /// 建一个闸（持有者标识自动生成）。
    pub fn new(repo: &'a Repo) -> Self {
        let seq = GATE_SEQ.fetch_add(1, Ordering::SeqCst);
        StudyNoGate {
            repo,
            holder: format!("pid-{}-{}", std::process::id(), seq),
        }
    }

    /// 建一个闸，**显式指定持有者标识**（测试与运维排查用）。
    pub fn with_holder(repo: &'a Repo, holder: &str) -> Self {
        StudyNoGate {
            repo,
            holder: holder.to_string(),
        }
    }

    /// 本闸的持有者标识。
    pub fn holder(&self) -> &str {
        &self.holder
    }

    /// **抢一把锁**（不等待）。
    ///
    /// - 抢到 → 返回租约（`Drop` 时自动释放）。
    /// - 别人占着且未过期 → [`ConcurrencyError::Held`]。
    ///
    /// 抢不到**不算失败**：调用方可以退避后重试（[`StudyNoGate::acquire`]）。
    pub fn try_acquire(
        &self,
        study_no: &str,
        lease_secs: u64,
    ) -> Result<StudyNoLease<'a>, ConcurrencyError> {
        let expires_at = crate::state::datetime_from_unix(
            crate::state::utc_now_secs().saturating_add(lease_secs),
        );
        if self
            .repo
            .try_acquire_study_lock(study_no, &self.holder, &expires_at)?
        {
            Ok(StudyNoLease {
                repo: self.repo,
                study_no: study_no.to_string(),
                holder: self.holder.clone(),
                released: false,
            })
        } else {
            // 抢不到：把「谁占着、占到什么时候」带出去（排查要看这个）。
            let held = self.repo.study_lock_of(study_no)?;
            Err(ConcurrencyError::Held {
                study_no: study_no.to_string(),
                holder: held
                    .as_ref()
                    .map(|row| row.holder.clone())
                    .unwrap_or_else(|| "unknown".to_string()),
                expires_at: held
                    .map(|row| row.expires_at)
                    .unwrap_or_else(|| "unknown".to_string()),
            })
        }
    }

    /// **抢锁 + 退避重试**（最多 `attempts` 轮）。
    ///
    /// 退避间隔逐轮拉长（`base * n`）：抢不到多半是**对方正在处理**，
    /// 稍等一会儿对方就放锁了；一直等不到就是真在忙，交给上层重试。
    pub fn acquire(
        &self,
        study_no: &str,
        lease_secs: u64,
        attempts: usize,
    ) -> Result<StudyNoLease<'a>, ConcurrencyError> {
        let attempts = attempts.max(1);
        for round in 0..attempts {
            match self.try_acquire(study_no, lease_secs) {
                Ok(lease) => return Ok(lease),
                Err(ConcurrencyError::Held { .. }) => {
                    if round + 1 < attempts {
                        sleep(Duration::from_millis(
                            DEFAULT_BACKOFF_MS * (round as u64 + 1),
                        ));
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Err(ConcurrencyError::Contended {
            study_no: study_no.to_string(),
            attempts,
        })
    }

    /// 这把检查号锁现在归谁（`None` = 没人占）。
    pub fn holder_of(&self, study_no: &str) -> Result<Option<StudyLockRow>, RepoError> {
        self.repo.study_lock_of(study_no)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> Repo {
        Repo::open_in_memory().unwrap()
    }

    #[test]
    fn 同一检查号第二个抢不到() {
        let repo = repo();
        let gate = StudyNoGate::with_holder(&repo, "holder-A");
        let _lease = gate.try_acquire("S-A", DEFAULT_LEASE_SECS).unwrap();

        let other = StudyNoGate::with_holder(&repo, "holder-B");
        match other.try_acquire("S-A", DEFAULT_LEASE_SECS) {
            Err(ConcurrencyError::Held {
                study_no, holder, ..
            }) => {
                assert_eq!(study_no, "S-A");
                assert_eq!(holder, "holder-A", "报错要说得清是谁占着");
            }
            other => panic!("应报被占着，实际：{:?}", other),
        };
    }

    #[test]
    fn 不同检查号互不阻塞() {
        let repo = repo();
        let gate = StudyNoGate::with_holder(&repo, "holder-A");
        let _a = gate.try_acquire("S-A", DEFAULT_LEASE_SECS).unwrap();
        // 另一个检查号照常抢得到 —— **不同检查号要并行**。
        let _b = gate.try_acquire("S-B", DEFAULT_LEASE_SECS).unwrap();
        assert_eq!(repo.count_study_locks().unwrap(), 2);
    }

    #[test]
    fn 释放后可以再抢到() {
        let repo = repo();
        let gate = StudyNoGate::with_holder(&repo, "holder-A");
        {
            let _lease = gate.try_acquire("S-A", DEFAULT_LEASE_SECS).unwrap();
            assert_eq!(repo.count_study_locks().unwrap(), 1);
        }
        // 租约析构即释放 —— 库里的锁行不该留下。
        assert_eq!(repo.count_study_locks().unwrap(), 0);
        let again = gate.try_acquire("S-A", DEFAULT_LEASE_SECS).unwrap();
        assert_eq!(again.study_no(), "S-A");
    }

    #[test]
    fn 持有者对不上时释放不动别人的锁() {
        let repo = repo();
        let gate = StudyNoGate::with_holder(&repo, "holder-A");
        let _lease = gate.try_acquire("S-A", DEFAULT_LEASE_SECS).unwrap();
        // 别人（租约到期后接管的持有者）来释放：不该把这把锁删掉。
        let released = repo.release_study_lock("S-A", "holder-B").unwrap();
        assert!(!released, "持有者对不上不得释放");
        assert_eq!(repo.count_study_locks().unwrap(), 1, "锁还在");
    }

    #[test]
    fn 过期锁可被接管() {
        let repo = repo();
        // 造一条**已过期**的锁（模拟持锁进程崩了）。
        repo.connection()
            .execute(
                "INSERT INTO study_locks (study_no, holder, acquired_at, expires_at) \
                 VALUES ('S-STALE','holder-crashed','2000-01-01 00:00:00','2000-01-01 00:01:00')",
                [],
            )
            .unwrap();
        let gate = StudyNoGate::with_holder(&repo, "holder-B");
        let lease = gate
            .try_acquire("S-STALE", DEFAULT_LEASE_SECS)
            .expect("过期锁应可被接管");
        assert_eq!(lease.holder(), "holder-B");
    }

    #[test]
    fn 未过期的锁不被接管() {
        let repo = repo();
        let gate = StudyNoGate::with_holder(&repo, "holder-A");
        let _lease = gate.try_acquire("S-A", DEFAULT_LEASE_SECS).unwrap();
        let other = StudyNoGate::with_holder(&repo, "holder-B");
        assert!(
            other.try_acquire("S-A", DEFAULT_LEASE_SECS).is_err(),
            "未过期的锁不得被接管"
        );
        // 持有者仍是原主。
        assert_eq!(gate.holder_of("S-A").unwrap().unwrap().holder, "holder-A");
    }

    #[test]
    fn 退避重试耗尽后报争用() {
        let repo = repo();
        let holder = StudyNoGate::with_holder(&repo, "holder-A");
        let _lease = holder.try_acquire("S-BUSY", DEFAULT_LEASE_SECS).unwrap();

        let other = StudyNoGate::with_holder(&repo, "holder-B");
        match other.acquire("S-BUSY", DEFAULT_LEASE_SECS, 2) {
            Err(ConcurrencyError::Contended { study_no, attempts }) => {
                assert_eq!(study_no, "S-BUSY");
                assert_eq!(attempts, 2);
            }
            other => panic!("应报争用，实际：{:?}", other),
        };
    }

    #[test]
    fn 退避重试期间对方放锁就能抢到() {
        let dir = std::env::temp_dir().join(format!("urs-lock-retry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("state.db");

        let repo = Repo::open(&db).unwrap();
        let holder = StudyNoGate::with_holder(&repo, "holder-A");
        let _lease = holder.try_acquire("S-RETRY", DEFAULT_LEASE_SECS).unwrap();

        // **另一条连接**（相当于另一个进程）在稍后放锁。
        let releaser_db = db.clone();
        let releaser = std::thread::spawn(move || {
            sleep(Duration::from_millis(120));
            let repo = Repo::open(&releaser_db).unwrap();
            repo.release_study_lock("S-RETRY", "holder-A").unwrap();
        });

        let other = StudyNoGate::with_holder(&repo, "holder-B");
        let got = other.acquire("S-RETRY", DEFAULT_LEASE_SECS, 8);
        releaser.join().unwrap();
        assert!(got.is_ok(), "对方放锁后应抢得到：{:?}", got.err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 同一检查号并发抢锁只有一个抢到() {
        let dir = std::env::temp_dir().join(format!("urs-lock-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("state.db");

        let workers = 6;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(workers));
        let mut handles = Vec::new();
        // 连接**先开好**再分给线程：万一开连接失败，线程会 panic 在 barrier 之前，
        // 其余线程就永远等在 barrier 上（整个测试挂死，而不是干脆地失败）。
        let repos: Vec<Repo> = (0..workers)
            .map(|_| Repo::open(&db).unwrap())
            .collect();
        for (i, repo) in repos.into_iter().enumerate() {
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                let gate = StudyNoGate::with_holder(&repo, &format!("holder-{}", i));
                barrier.wait();
                // **租约必须握在手里**：析构即释放锁，
                // 随手就丢的话锁根本没人占，测不出串行。
                let lease = gate.try_acquire("S-RACE", DEFAULT_LEASE_SECS);
                let won = lease.is_ok();
                // 持锁不放一会儿，让并发窗口真的打开。
                sleep(Duration::from_millis(80));
                drop(lease);
                won
            }));
        }
        let winners = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|won| *won)
            .count();
        assert_eq!(winners, 1, "同一检查号并发抢锁只能有一个抢到");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
