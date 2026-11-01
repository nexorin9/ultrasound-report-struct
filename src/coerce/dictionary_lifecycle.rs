//! 词典生命周期：加载 → 使用 → 释放（热更新不打断在途任务）。
//!
//! **这一层解决什么**：词典（部位别名 + 测项别名）是**口径**。
//! 运维照着失败清单补一个别名，改完词典就该生效；但**不能让正在处理的那份报告改口径** ——
//! 一份报告前半段用旧词典、后半段用新词典，产出的台账行既不是旧口径也不是新口径，
//! 事后没人说得清它是哪套口径算出来的。
//!
//! **三段式生命周期**：
//!
//! | 阶段 | 动作 | 谁在做 |
//! |---|---|---|
//! | **加载** | 读词典文件 → 装成一代快照（代号 + 版本） | 启动时 / 词典文件变了 |
//! | **使用** | 任务领一份快照（引用计数 +1）→ 作用域内固定用这一代 | 每份报告 |
//! | **释放** | 任务结束 → 引用计数 -1；没人再持有这一代才回收 | 任务结束时 |
//!
//! **为什么是「代号 + 引用计数」而不是「原地改」**：词典是**口径**，口径一改，
//! 历史台账行的版本号就与新行对不上 —— 所以每一代都是**不可变**快照，
//! 换代 = 装上新快照，不是改旧的。旧快照靠 `Arc` 的引用计数活着：
//! 在途任务还握着它，它就不会被回收 —— **旧口径不会在任务中途凭空消失**。
//!
//! **机制溯源（改写，不复制源代码）**：源产品的词干器运行时用
//! `create_env` / `close_env` / `stem` 三段契约 —— 归一器**先创建**、**再使用**、
//! **最后关闭**，注册表按语言登记各语言实现。本项目把同一契约落到词典上：
//! [`Dictionaries::load`] = create（装一代快照），
//! [`DictionaryHandle::scope`] = use（领一份并固定作用域），
//! [`Dictionaries::release`] = close（引用计数 -1）。
//!
//! **进程内单例**：全项目只有**一份**生效词典（[`Dictionaries::get`]），
//! 解析内核、导出、守护进程看的是同一套口径 —— 两处各带一份必然漂移。

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::SystemTime;

use super::dictionary::{
    check_items_bases_known, default_items_path, default_parts_path, embedded_items_json,
    embedded_parts_json, DictionaryError, ItemsDictionary, PartsDictionary, SUBJECT_ITEMS,
    SUBJECT_PARTS,
};

// =====================================================================
// 快照（一代词典）
// =====================================================================

/// 一代词典快照：**不可变**。
///
/// 快照一旦装上就不再改 —— 口径是「这一代算出来的」，改了就不是同一代了。
/// 需要改口径 → 装新一代（[`Dictionaries::reload`]）。
#[derive(Clone, Debug)]
pub struct DictionarySnapshot {
    /// 代号（单调递增）：台账行留的版本号之外，再留一个「第几代」的凭据。
    generation: u64,
    /// 部位词典（这一代的）。
    parts: Arc<PartsDictionary>,
    /// 测项词典（这一代的）。
    items: Arc<ItemsDictionary>,
    /// 合并版本号（`部位版本+测项版本`）—— 台账行落的就是它。
    version: String,
    /// 装载时的词典文件指纹（修改时间 + 字节数）—— 热更新判据。
    fingerprint: (Fingerprint, Fingerprint),
}

impl DictionarySnapshot {
    /// 代号（第几代词典）。
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// 这一代的部位词典。
    pub fn parts(&self) -> &PartsDictionary {
        &self.parts
    }

    /// 这一代的部位词典（句柄形式）。
    pub fn parts_handle(&self) -> &Arc<PartsDictionary> {
        &self.parts
    }

    /// 这一代的测项词典。
    pub fn items(&self) -> &ItemsDictionary {
        &self.items
    }

    /// 这一代的测项词典（句柄形式）。
    pub fn items_handle(&self) -> &Arc<ItemsDictionary> {
        &self.items
    }

    /// 合并版本号（`部位版本+测项版本`）。
    pub fn version(&self) -> &str {
        &self.version
    }

    /// 部位词典版本号。
    pub fn parts_version(&self) -> &str {
        self.parts.version()
    }

    /// 测项词典版本号。
    pub fn items_version(&self) -> &str {
        self.items.version()
    }
}

// =====================================================================
// 文件指纹
// =====================================================================

/// 词典文件的指纹（修改时间 + 字节数）。
///
/// **为什么不用内容哈希**：词典文件几十 KB，守护进程每轮算一次 sha256 是白烧 CPU；
/// 「修改时间 + 字节数变了」已经足够触发一次重载 —— 真改了内容，时间或大小必变其一。
#[derive(Clone, Debug, PartialEq, Eq)]
struct Fingerprint {
    /// 修改时间（取不到时为 `None`）。
    mtime: Option<SystemTime>,
    /// 字节数（读不到文件时为 0）。
    len: u64,
}

impl Fingerprint {
    /// 取一个文件的指纹（文件不存在 → 空指纹）。
    fn of(path: &Path) -> Fingerprint {
        match std::fs::metadata(path) {
            Ok(meta) => Fingerprint {
                mtime: meta.modified().ok(),
                len: meta.len(),
            },
            Err(_) => Fingerprint {
                mtime: None,
                len: 0,
            },
        }
    }
}

// =====================================================================
// 注册表
// =====================================================================

/// 词典文件路径（两本词典各一个）。
#[derive(Clone, Debug)]
struct Paths {
    parts: PathBuf,
    items: PathBuf,
}

/// 注册表内部状态（进程内一份）。
#[derive(Debug)]
struct Registry {
    /// 词典文件路径（热更新时按这里的路径重读）。
    paths: RwLock<Paths>,
    /// 当前生效的一代（`None` = 还没装上）。
    current: RwLock<Option<Arc<DictionarySnapshot>>>,
    /// 各代的在途持有数（引用计数）。
    holders: Mutex<HashMap<u64, usize>>,
    /// 代号发号器。
    next_generation: AtomicU64,
}

/// 词典注册表：**进程内单例**。
///
/// 全项目只有一份生效口径 —— 解析内核、导出、守护进程看的是同一套口径。
/// 需要多份（例如测试里造临时词典）时用 [`Dictionaries::new`] 造**独立实例**，
/// 独立实例**不碰**进程单例。
#[derive(Clone, Debug)]
pub struct Dictionaries {
    inner: Arc<Registry>,
}

/// 进程内单例。
static GLOBAL: OnceLock<Dictionaries> = OnceLock::new();

impl Dictionaries {
    /// 造一个**独立**的词典注册表（不碰进程单例）。
    ///
    /// 测试与「临时词典」场景用：造出来的实例只影响持有它的人。
    pub fn new(parts_path: &Path, items_path: &Path) -> Result<Self, DictionaryError> {
        let registry = Registry {
            paths: RwLock::new(Paths {
                parts: parts_path.to_path_buf(),
                items: items_path.to_path_buf(),
            }),
            current: RwLock::new(None),
            holders: Mutex::new(HashMap::new()),
            next_generation: AtomicU64::new(1),
        };
        let dicts = Dictionaries {
            inner: Arc::new(registry),
        };
        dicts.reload()?;
        Ok(dicts)
    }

    /// **加载**词典（进程内单例）。
    ///
    /// - 首次调用 → 装上第一代快照；
    /// - 再次调用 → **按新路径重载**（热更新语义）：运维改了配置里的词典路径，
    ///   重新加载即生效。
    ///
    /// 词典文件**不存在** → 退回编译期内置的同一份（值班机只拷可执行文件也能跑）；
    /// 词典文件**存在却坏了** → **硬失败**：不退回旧词典继续跑
    /// （那会让口径变了却看不出来）。
    pub fn load(parts_path: &Path, items_path: &Path) -> Result<&'static Dictionaries, DictionaryError> {
        if let Some(d) = GLOBAL.get() {
            d.set_paths(parts_path, items_path);
            d.reload()?;
            return Ok(d);
        }
        let created = Dictionaries::new(parts_path, items_path)?;
        if GLOBAL.set(created).is_err() {
            // 另一个线程抢先装上了：以它为准，按本次路径再重载一次。
            let d = GLOBAL.get().expect("进程单例已装上");
            d.set_paths(parts_path, items_path);
            d.reload()?;
            return Ok(d);
        }
        Ok(GLOBAL.get().expect("进程单例已装上"))
    }

    /// 取得进程单例（**未加载为 `None`**）。
    pub fn get() -> Option<&'static Dictionaries> {
        GLOBAL.get()
    }

    /// 确保进程单例已加载（未加载则按**默认路径**懒加载）。
    ///
    /// 懒加载是给「没人显式初始化就直接解析」的调用点兜底 ——
    /// 加载失败时**返回错误**，由调用方决定是显式失败还是退回未命中。
    pub fn ensure_loaded() -> Result<&'static Dictionaries, DictionaryError> {
        match GLOBAL.get() {
            Some(d) => Ok(d),
            None => Dictionaries::load(&default_parts_path(), &default_items_path()),
        }
    }

    /// 换掉词典文件路径（下次重载按新路径读）。
    fn set_paths(&self, parts: &Path, items: &Path) {
        let mut p = self.inner.paths.write().unwrap_or_else(|e| e.into_inner());
        p.parts = parts.to_path_buf();
        p.items = items.to_path_buf();
    }

    /// 当前生效的一代（未装载为 `None`）。
    pub fn current(&self) -> Option<Arc<DictionarySnapshot>> {
        self.inner
            .current
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// **使用**：领一份当前词典（引用计数 +1）。
    ///
    /// 领到的句柄**必须**在任务结束时释放（[`DictionaryHandle`] 的 `Drop` 自动做）——
    /// 否则这一代永远回收不掉。
    pub fn acquire(&self) -> Option<DictionaryHandle> {
        let snapshot = self.current()?;
        {
            let mut holders = self.inner.holders.lock().unwrap_or_else(|e| e.into_inner());
            *holders.entry(snapshot.generation).or_insert(0) += 1;
        }
        Some(DictionaryHandle {
            snapshot,
            registry: self.clone(),
        })
    }

    /// **释放**：引用计数 -1；没人再持有这一代时从持有表里摘掉。
    pub fn release(&self, generation: u64) {
        let mut holders = self.inner.holders.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = holders.get_mut(&generation) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                holders.remove(&generation);
            }
        }
    }

    /// 某一代的在途持有数（观测用：热更新后旧代还有多少任务在用）。
    pub fn holders_of(&self, generation: u64) -> usize {
        self.inner
            .holders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&generation)
            .copied()
            .unwrap_or(0)
    }

    /// 当前持有总数（所有代）。
    pub fn total_holders(&self) -> usize {
        self.inner
            .holders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .sum()
    }

    /// **热更新**：重读词典文件，装上新一代。
    ///
    /// **失败即不改**：词典文件坏了 → 报错，**当前这一代原样留着** ——
    /// 宁可继续用旧口径（口径留痕说得清），也不换上坏口径（口径变了却看不出来）。
    pub fn reload(&self) -> Result<ReloadOutcome, DictionaryError> {
        let paths = {
            let p = self.inner.paths.read().unwrap_or_else(|e| e.into_inner());
            p.clone()
        };
        let parts = load_parts(&paths.parts)?;
        let items = load_items(&paths.items, &parts)?;
        let version = format!("{}+{}", parts.version(), items.version());
        let fingerprint = (Fingerprint::of(&paths.parts), Fingerprint::of(&paths.items));
        let generation = self.inner.next_generation.fetch_add(1, Ordering::SeqCst);
        let snapshot = Arc::new(DictionarySnapshot {
            generation,
            parts,
            items,
            version,
            fingerprint,
        });
        let previous = {
            let mut cur = self.inner.current.write().unwrap_or_else(|e| e.into_inner());
            cur.replace(Arc::clone(&snapshot))
        };
        let (previous_generation, previous_version) = match previous.as_ref() {
            Some(p) => (p.generation, p.version().to_string()),
            None => (0, String::from("（无）")),
        };
        // 换代时旧代**仍在途**的任务数：旧口径还没被回收 ——
        // 它们继续用旧口径算完这一份报告（口径留痕说得清）。
        let retired_holders = self.holders_of(previous_generation);
        Ok(ReloadOutcome {
            generation,
            previous_generation,
            previous_version,
            current_version: snapshot.version().to_string(),
            parts_version: snapshot.parts_version().to_string(),
            items_version: snapshot.items_version().to_string(),
            retired_holders,
        })
    }

    /// 词典文件**变了**才重载（守护进程每轮调用一次）。
    ///
    /// 没变 → 返回 `None`（不重载，不换代号，不打断任何任务）。
    pub fn reload_if_changed(&self) -> Result<Option<ReloadOutcome>, DictionaryError> {
        let paths = {
            let p = self.inner.paths.read().unwrap_or_else(|e| e.into_inner());
            p.clone()
        };
        let now = (Fingerprint::of(&paths.parts), Fingerprint::of(&paths.items));
        if let Some(snapshot) = self.current() {
            if snapshot.fingerprint == now {
                return Ok(None);
            }
        }
        Ok(Some(self.reload()?))
    }
}

/// 热更新结果（进日志 / CLI 输出）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReloadOutcome {
    /// 新一代的代号。
    pub generation: u64,
    /// 上一代的代号（首次装载为 0）。
    pub previous_generation: u64,
    /// 上一代的合并版本号。
    pub previous_version: String,
    /// 新一代的合并版本号。
    pub current_version: String,
    /// 新一代的部位词典版本号。
    pub parts_version: String,
    /// 新一代的测项词典版本号。
    pub items_version: String,
    /// 换代时**仍在途**的任务数（旧代还没被回收）。
    pub retired_holders: usize,
}

impl ReloadOutcome {
    /// 一行中文说明（CLI 输出与日志共用）。
    pub fn render(&self) -> String {
        format!(
            "词典已换代：第 {} 代 → 第 {} 代，版本 {} → {}，换代时仍有 {} 个在途任务用旧口径",
            self.previous_generation,
            self.generation,
            self.previous_version,
            self.current_version,
            self.retired_holders,
        )
    }
}

// =====================================================================
// 句柄与作用域
// =====================================================================

/// 词典句柄：**持有**一代词典（引用计数 +1）。
///
/// 句柄析构时自动**释放**（引用计数 -1）—— 任务结束时把它丢掉即可，
/// 不用手写「释放」调用（手写容易漏，漏了就永远回收不掉旧代）。
pub struct DictionaryHandle {
    snapshot: Arc<DictionarySnapshot>,
    registry: Dictionaries,
}

impl DictionaryHandle {
    /// 领到的这一代快照。
    pub fn snapshot(&self) -> &DictionarySnapshot {
        &self.snapshot
    }

    /// 这一代的合并版本号。
    pub fn version(&self) -> &str {
        self.snapshot.version()
    }

    /// **进入作用域**：本次任务内固定用这一代词典。
    ///
    /// 作用域内发生热更新时，任务**仍然用这一代** ——
    /// 一份报告不会前半段旧口径、后半段新口径。
    pub fn scope(self) -> ScopeGuard {
        let snapshot = Arc::clone(&self.snapshot);
        SCOPED.with(|s| s.borrow_mut().push(Arc::clone(&snapshot)));
        ScopeGuard {
            snapshot,
            handle: Some(self),
        }
    }
}

impl std::fmt::Debug for DictionaryHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DictionaryHandle")
            .field("generation", &self.snapshot.generation)
            .field("version", &self.snapshot.version())
            .finish()
    }
}

impl Drop for DictionaryHandle {
    fn drop(&mut self) {
        self.registry.release(self.snapshot.generation);
    }
}

/// 词典作用域守卫：作用域内固定用**进入时**的那一代。
///
/// 析构时弹出作用域并释放持有（引用计数 -1）。
pub struct ScopeGuard {
    snapshot: Arc<DictionarySnapshot>,
    handle: Option<DictionaryHandle>,
}

impl ScopeGuard {
    /// 作用域内固定用的这一代快照。
    pub fn snapshot(&self) -> &DictionarySnapshot {
        &self.snapshot
    }

    /// 作用域内固定用的这一代的合并版本号。
    pub fn version(&self) -> &str {
        self.snapshot.version()
    }
}

impl std::fmt::Debug for ScopeGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScopeGuard")
            .field("generation", &self.snapshot.generation)
            .field("version", &self.snapshot.version())
            .finish()
    }
}

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        SCOPED.with(|s| {
            let _ = s.borrow_mut().pop();
        });
        // 句柄随本结构析构 → 引用计数 -1。
        self.handle.take();
    }
}

// =====================================================================
// 作用域（线程内）
// =====================================================================

thread_local! {
    /// 当前线程的词典作用域栈（栈顶 = 生效的那一代）。
    static SCOPED: RefCell<Vec<Arc<DictionarySnapshot>>> = const { RefCell::new(Vec::new()) };
}

/// 作用域内生效的快照（不在作用域内 → `None`）。
pub fn scoped_snapshot() -> Option<Arc<DictionarySnapshot>> {
    SCOPED.with(|s| s.borrow().last().cloned())
}

/// 当前生效的词典快照：**作用域优先 → 进程单例 → 懒加载默认词典**。
///
/// 作用域优先是关键：热更新换了当前代，但在途任务仍应看到自己领到的那一代。
pub fn current_snapshot() -> Option<Arc<DictionarySnapshot>> {
    if let Some(snapshot) = scoped_snapshot() {
        return Some(snapshot);
    }
    let dicts = Dictionaries::ensure_loaded().ok()?;
    dicts.current()
}

/// 进入**进程当前生效词典**的作用域（每份报告一次）。
///
/// **已经有作用域时不叠加**：外层已经固定了口径（守护进程每份报告一层、
/// 解析入口又一层），内层再叠一层只会把口径换成进程单例 ——
/// 外层那一代（比如运维刚热更新的那一代）就被悄悄顶掉了。
///
/// 词典不可用时返回 `None` —— 调用点据此判断「没有固定口径」，
/// 词典查不到的地方照旧产出显式失败条目（不猜口径）。
pub fn scope_current() -> Option<ScopeGuard> {
    if scoped_snapshot().is_some() {
        return None;
    }
    let dicts = Dictionaries::ensure_loaded().ok()?;
    dicts.acquire().map(|handle| handle.scope())
}

/// 进程当前生效词典**变了就重载**（守护进程每轮调用一次）。
pub fn reload_current_if_changed() -> Result<Option<ReloadOutcome>, DictionaryError> {
    let dicts = Dictionaries::ensure_loaded()?;
    dicts.reload_if_changed()
}

// =====================================================================
// 装载细节
// =====================================================================

/// 读部位词典：文件不存在 → 编译期内置的同一份；文件坏了 → 硬失败。
fn load_parts(path: &Path) -> Result<Arc<PartsDictionary>, DictionaryError> {
    match PartsDictionary::load(path) {
        Ok(dict) => Ok(Arc::new(dict)),
        Err(DictionaryError::FileNotFound { .. }) => {
            let mut dict = PartsDictionary::from_json(embedded_parts_json())?;
            dict.set_source("内置副本（词典文件不存在）");
            Ok(Arc::new(dict))
        }
        Err(err) => Err(err),
    }
}

/// 读测项词典：文件不存在 → 编译期内置的同一份；文件坏了 → 硬失败。
///
/// 装载后做**口径同源检查**：测项词典引用的部位基名必须在部位词典里存在 ——
/// 否则拼出来的测项码永远匹配不到历史值，对拍会静悄悄地一条都不比上。
fn load_items(path: &Path, parts: &PartsDictionary) -> Result<Arc<ItemsDictionary>, DictionaryError> {
    let dict = match ItemsDictionary::load(path) {
        Ok(dict) => dict,
        Err(DictionaryError::FileNotFound { .. }) => {
            let mut dict = ItemsDictionary::from_json(embedded_items_json())?;
            dict.set_source("内置副本（词典文件不存在）");
            dict
        }
        Err(err) => return Err(err),
    };
    check_items_bases_known(&dict, parts)?;
    Ok(Arc::new(dict))
}

/// 词典类别常量（供本模块构造错误时保持文案一致）。
const _SUBJECT_PARTS: &str = SUBJECT_PARTS;
const _SUBJECT_ITEMS: &str = SUBJECT_ITEMS;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coerce::dictionary::dictionary_version;
    use crate::parse::parse_report;
    use crate::state::idempotency_key;
    use crate::state::repo::{IngestRecord, Repo};

    /// 临时目录（按用例名隔离）。
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "urs-dict-life-{}-{}-{:?}",
            tag,
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 部位词典（最小可用：肾左右 + 膀胱）。
    fn parts_json(version: &str, extra_alias: &str) -> String {
        format!(
            r#"{{"version":"{}",
                "bases":[
                  {{"base":"kidney","word_faces":["肾","肾脏"]}},
                  {{"base":"bladder","word_faces":["膀胱"]}}
                ],
                "entries":[
                  {{"part_key":"kidney.left","base":"kidney","side":"left","aliases":["左肾"]}},
                  {{"part_key":"kidney.right","base":"kidney","side":"right","aliases":["右肾"]}},
                  {{"part_key":"kidney.bilateral","base":"kidney","side":"bilateral","aliases":["双肾"]}},
                  {{"part_key":"bladder","base":"bladder","side":"none","aliases":["膀胱","{}"]}}
                ]}}"#,
            version, extra_alias
        )
    }

    /// 测项词典（最小可用：前后径 + 长度）。
    fn items_json(version: &str) -> String {
        format!(
            r#"{{"version":"{}",
                "entries":[
                  {{"alias":"前后径","suffix":"ap_diameter","bases":["kidney"]}},
                  {{"alias":"长","suffix":"length","bases":["kidney"]}}
                ]}}"#,
            version
        )
    }

    /// 造一套词典文件（返回目录与两个路径）。
    fn write_dicts(
        dir: &Path,
        parts_version: &str,
        items_version: &str,
        extra_alias: &str,
    ) -> (PathBuf, PathBuf) {
        let parts = dir.join("parts.json");
        let items = dir.join("items.json");
        std::fs::write(&parts, parts_json(parts_version, extra_alias)).unwrap();
        std::fs::write(&items, items_json(items_version)).unwrap();
        (parts, items)
    }

    #[test]
    fn 加载装上第一代且引用计数正确() {
        let dir = temp_dir("load");
        let (parts, items) = write_dicts(&dir, "parts-1.0.0", "items-1.0.0", "膀胱腔");
        let dicts = Dictionaries::new(&parts, &items).unwrap();

        let snapshot = dicts.current().expect("应装上一代");
        assert_eq!(snapshot.generation(), 1);
        assert_eq!(snapshot.version(), "parts-1.0.0+items-1.0.0");
        assert_eq!(dicts.total_holders(), 0, "刚装上时没有持有者");

        let handle = dicts.acquire().expect("应能领到词典");
        assert_eq!(dicts.holders_of(1), 1, "领一份 → 引用计数 +1");
        drop(handle);
        assert_eq!(dicts.holders_of(1), 0, "释放 → 引用计数 -1");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 热更新换代后新任务用新版本() {
        let dir = temp_dir("reload");
        let (parts, items) = write_dicts(&dir, "parts-1.0.0", "items-1.0.0", "膀胱腔");
        let dicts = Dictionaries::new(&parts, &items).unwrap();

        // 第一代：解析出的部位键用旧词典。
        let guard = dicts.acquire().unwrap().scope();
        let first = parse_report("左肾前后径 2.1cm");
        assert_eq!(first.items[0].part_key.as_deref(), Some("kidney.left"));
        assert_eq!(first.items[0].dictionary_version, "parts-1.0.0+items-1.0.0");
        drop(guard);

        // 改词典文件（版本号 + 新增别名）→ 热更新。
        let (parts2, items2) = write_dicts(&dir, "parts-2.0.0", "items-2.0.0", "膀胱腔");
        assert_eq!(parts2, parts);
        assert_eq!(items2, items);
        let outcome = dicts.reload().unwrap();
        assert_eq!(outcome.previous_generation, 1);
        assert_eq!(outcome.generation, 2);
        assert_eq!(outcome.current_version, "parts-2.0.0+items-2.0.0");
        assert_eq!(outcome.previous_version, "parts-1.0.0+items-1.0.0");

        // 第二代：新任务用新版本。
        let guard = dicts.acquire().unwrap().scope();
        let second = parse_report("左肾前后径 2.1cm");
        assert_eq!(
            second.items[0].dictionary_version,
            "parts-2.0.0+items-2.0.0",
            "热更新后新任务应使用新版本"
        );
        drop(guard);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 在途任务中途热更新不影响该任务结果() {
        let dir = temp_dir("midflight");
        let (parts, items) = write_dicts(&dir, "parts-1.0.0", "items-1.0.0", "膀胱腔");
        let dicts = Dictionaries::new(&parts, &items).unwrap();

        // 任务领第 1 代并进入作用域。
        let guard = dicts.acquire().unwrap().scope();
        assert_eq!(guard.snapshot().generation(), 1);

        // 任务处理到一半：词典文件改了并热更新。
        let _ = write_dicts(&dir, "parts-2.0.0", "items-2.0.0", "膀胱腔");
        let outcome = dicts.reload().unwrap();
        assert_eq!(outcome.generation, 2);
        assert_eq!(outcome.retired_holders, 1, "换代时旧代仍有 1 个在途任务");

        // 任务继续解析 —— 用的仍是第 1 代（口径不中途变）。
        let mid = parse_report("左肾前后径 2.1cm");
        assert_eq!(
            mid.items[0].dictionary_version,
            "parts-1.0.0+items-1.0.0",
            "在途任务必须继续用自己领到的那一代"
        );
        assert_eq!(mid.items[0].part_key.as_deref(), Some("kidney.left"));

        // 旧代此刻仍在持有表里（任务还没结束）。
        assert_eq!(dicts.holders_of(1), 1);
        drop(guard);
        assert_eq!(dicts.holders_of(1), 0, "任务结束 → 旧代释放");
        assert_eq!(dicts.total_holders(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 旧代释放后新任务看到新版本() {
        let dir = temp_dir("release");
        let (parts, items) = write_dicts(&dir, "parts-1.0.0", "items-1.0.0", "膀胱腔");
        let dicts = Dictionaries::new(&parts, &items).unwrap();
        {
            let _guard = dicts.acquire().unwrap().scope();
        }
        let _ = write_dicts(&dir, "parts-2.0.0", "items-2.0.0", "膀胱腔");
        dicts.reload().unwrap();

        let guard = dicts.acquire().unwrap().scope();
        assert_eq!(guard.snapshot().generation(), 2);
        assert_eq!(guard.version(), "parts-2.0.0+items-2.0.0");
        drop(guard);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 词典文件坏了重载失败且当前代不变() {
        let dir = temp_dir("broken");
        let (parts, items) = write_dicts(&dir, "parts-1.0.0", "items-1.0.0", "膀胱腔");
        let dicts = Dictionaries::new(&parts, &items).unwrap();

        std::fs::write(&parts, "{\"version\":\"parts-9.9.9\",\"bases\":[],\"entries\":[]}").unwrap();
        let err = dicts.reload().unwrap_err();
        assert_eq!(err.code(), "DICT_SCHEMA_INVALID");
        // 失败即不改：当前这一代原样留着（宁可继续用旧口径，也不换上坏口径）。
        let current = dicts.current().unwrap();
        assert_eq!(current.generation(), 1);
        assert_eq!(current.version(), "parts-1.0.0+items-1.0.0");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 文件没变不重载_变了才重载() {
        let dir = temp_dir("fingerprint");
        let (parts, items) = write_dicts(&dir, "parts-1.0.0", "items-1.0.0", "膀胱腔");
        let dicts = Dictionaries::new(&parts, &items).unwrap();

        assert!(
            dicts.reload_if_changed().unwrap().is_none(),
            "文件没变不该重载"
        );
        assert_eq!(dicts.current().unwrap().generation(), 1);

        let _ = write_dicts(&dir, "parts-2.0.0", "items-2.0.0", "膀胱腔");
        let outcome = dicts.reload_if_changed().unwrap().expect("文件变了应重载");
        assert_eq!(outcome.generation, 2);
        assert_eq!(outcome.current_version, "parts-2.0.0+items-2.0.0");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 热更新后旧台账行仍留旧版本() {
        let dir = temp_dir("ledger");
        let (parts, items) = write_dicts(&dir, "parts-1.0.0", "items-1.0.0", "膀胱腔");
        let dicts = Dictionaries::new(&parts, &items).unwrap();

        // 第一代解析并落库。
        let repo = Repo::open_in_memory().unwrap();
        let body = "左肾前后径 2.1cm";
        let outcome = {
            let _guard = dicts.acquire().unwrap().scope();
            parse_report(body)
        };
        assert_eq!(outcome.items[0].dictionary_version, "parts-1.0.0+items-1.0.0");
        let key = idempotency_key("SAMPLE-LIFE", body);
        repo.commit_ingestion(&IngestRecord {
            study_no: "SAMPLE-LIFE",
            patient_id: Some("PID-EXAMPLE"),
            source_path: None,
            idempotency_key: &key,
            dictionary_version: outcome.items[0].dictionary_version.clone(),
            items: &outcome.items,
            failures: &outcome.failures,
        })
        .unwrap();

        // 热更新到第二代。
        let _ = write_dicts(&dir, "parts-2.0.0", "items-2.0.0", "膀胱腔");
        dicts.reload().unwrap();

        // 旧台账行仍是旧版本（口径留痕说的是「这条数据是哪套口径算出来的」）。
        let row_version: String = repo
            .connection()
            .query_row("SELECT dictionary_version FROM items LIMIT 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(row_version, "parts-1.0.0+items-1.0.0");

        // 新报告用新版本。
        let new_outcome = {
            let _guard = dicts.acquire().unwrap().scope();
            parse_report(body)
        };
        assert_eq!(new_outcome.items[0].dictionary_version, "parts-2.0.0+items-2.0.0");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 作用域外解析走进程单例的当前口径() {
        // 不进作用域时：解析内核看的是进程单例的当前代（与词典文件一致）。
        let version = dictionary_version();
        assert!(version.starts_with("parts-"), "版本号应来自词典文件：{}", version);
        let outcome = parse_report("左肾前后径 2.1cm");
        assert_eq!(outcome.items[0].dictionary_version, version);
    }
}
