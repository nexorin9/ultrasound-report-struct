//! 优雅退出：SIGTERM / SIGINT → 停止标志。
//!
//! **为什么不新增依赖**：本项目**刻意**把依赖面收窄到 6 个 crate（见 `plan.md`），
//! 信号处理是唯一需要 libc 的地方，而 std 在 unix 目标上本就链接 libc ——
//! 直接声明 `signal(2)` 即可，依赖面不变。
//!
//! **信号处理器里只做 async-signal-safe 的事**：置一个原子布尔。
//! 不加锁、不打日志、不碰数据库 —— 那些动作留给调度线程在**安全上下文**里做。
//!
//! **停机语义**（`spec.md` §7「进程常驻」）：收到 SIGTERM / SIGINT 后
//! **停止取新任务 → 等在途任务结束 → 落库 → 退出码 0**。
//! 值班机上 `systemctl stop` 与 `Ctrl-C` 走同一条路径。

use std::sync::atomic::{AtomicBool, Ordering};

/// 停止标志（信号处理器写，调度线程读）。
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

/// SIGINT（`Ctrl-C`）。
const SIGINT: i32 = 2;
/// SIGTERM（`kill` 默认信号、`systemctl stop`）。
const SIGTERM: i32 = 15;

extern "C" {
    /// libc 的 `signal(2)`。
    ///
    /// 第二个参数是 `sighandler_t`（`void (*)(int)`），用 Rust 的
    /// `extern "C" fn(i32)` 表达；返回值是**上一个处理器**，
    /// `SIG_ERR` 不是合法函数指针 —— 用 `Option<extern "C" fn(i32)>` 承接
    /// （函数指针的 `Option` 有保证的 niche 优化，布局与裸指针一致）。
    fn signal(signum: i32, handler: extern "C" fn(i32)) -> Option<extern "C" fn(i32)>;
}

/// 信号处理器：只置原子标志。
extern "C" fn on_stop_signal(_signum: i32) {
    // SAFETY: 只写一个原子布尔 —— 这是 async-signal-safe 的最小集合。
    STOP_REQUESTED.store(true, Ordering::SeqCst);
}

/// 安装停止信号处理器（幂等：重复安装无副作用）。
pub fn install_stop_handlers() {
    // SAFETY: `signal` 是 libc 标准函数（std 在 unix 目标上已链接 libc）；
    // 传入的处理器签名与 `sighandler_t` 一致；返回值（上一个处理器）此处用不到。
    unsafe {
        let _ = signal(SIGINT, on_stop_signal);
        let _ = signal(SIGTERM, on_stop_signal);
    }
}

/// 是否已收到停止信号。
pub fn stop_requested() -> bool {
    STOP_REQUESTED.load(Ordering::SeqCst)
}

/// 请求停止（与信号处理器同一条路径；供内部与测试调用）。
pub fn request_stop() {
    STOP_REQUESTED.store(true, Ordering::SeqCst);
}

/// 复位停止标志（**仅测试用**：进程级全局状态，测试之间要能清干净）。
pub fn reset_stop_flag() {
    STOP_REQUESTED.store(false, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 停止标志可置可复位() {
        reset_stop_flag();
        assert!(!stop_requested());
        request_stop();
        assert!(stop_requested());
        reset_stop_flag();
        assert!(!stop_requested());
    }

    #[test]
    fn 安装处理器后进程仍可正常运行() {
        // 只验证安装本身不崩、不改变停止标志语义（真正发信号由集成测试做）。
        reset_stop_flag();
        install_stop_handlers();
        install_stop_handlers();
        assert!(!stop_requested());
        reset_stop_flag();
    }
}
