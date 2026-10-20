//! sysfs I/O 看门狗：**让附加功能的写入永远无法拖死温控**
//!
//! # 为什么需要这个模块（真机事故）
//!
//! 2026-10-13 真机实测：写 `smart_chg` 时内核驱动 `mca_smart_charge` 死锁，
//! 调用者进程陷入 `D` 状态（**不可中断睡眠**）：
//!
//! ```text
//! __flush_work
//! cancel_delayed_work_sync
//! smart_charge_set_soc_limit_cb [mca_smart_charge]   ← 写 smart_chg 触发的回调
//! mca_vote [mca_voter]
//! smart_charge_sysfs_store
//! vfs_write                                          ← 我们的 write()
//! ```
//!
//! 后果非常严重：
//!
//! | # | 后果 |
//! |---|---|
//! | 1 | worker 永久卡死 —— **温控彻底停摆**（模块最核心的安全功能失效） |
//! | 2 | `D` 状态**无法被杀**（`kill -9` 无效），只能重启设备 |
//! | 3 | 熔断器**救不了** —— 它针对的是"函数返回错误"，而这里函数根本不返回 |
//!
//! 这违背了用户的硬性要求：
//! 「附加功能不要与充电控制的逻辑搅在一起」—— 代码层做到了（crate 隔离），
//! **运行期没有**：一次附加功能写入能把温控主循环一起拖死。
//!
//! # 修法：把 I/O 挪到专用线程 + 超时
//!
//! ```text
//! 主线程（温控）                     I/O 线程
//!   │                                  │
//!   ├─ AddonIo::write() ──请求──▶      ├─ 真正 open/write（可能卡死在 D 状态）
//!   │                                  │
//!   ├─ recv_timeout(3s) ◀──回复────────┤ 正常：立即回复
//!   │                                  │
//!   └─ 超时 ⇒ 判定"节点已挂起"
//!        · 标记 stuck，**此后永不再发请求**
//!        · 返回错误 ⇒ 附加功能熔断、停用
//!        · 主循环毫发无伤，继续做温控
//! ```
//!
//! ⚠️ 关键：超时后**绝不 `join` 那个线程** —— 它处于不可中断睡眠，
//! `join` 会让主线程也一起卡死，正是我们要避免的事。线程就让它挂着，
//! 反正每个进程只会有一次（因为此后不再发请求）。

use std::sync::mpsc::{channel, RecvTimeoutError, Sender, SyncSender};
use std::thread;
use std::time::Duration;

use thermo_addons::io::AddonIo;

/// 单次 I/O 的超时时长
///
/// sysfs 读写正常都在微秒级；即使触发 I2C 通信也在毫秒级。
/// 3 秒足够宽裕（不会误判），又短到用户几乎无感 ——
/// 且附加功能周期是 60 秒，这点开销完全可忽略。
pub const IO_TIMEOUT: Duration = Duration::from_secs(3);

/// 发给 I/O 线程的请求
///
/// 请求侧用 [`SyncSender`]（有界队列，防积压），回复侧用 [`Sender`]
/// （容量无限 —— 每个请求对应恰好一个回复，无需背压）。
enum Req {
    Exists {
        path: String,
        reply: Sender<bool>,
    },
    Writable {
        path: String,
        reply: Sender<bool>,
    },
    Read {
        path: String,
        reply: Sender<Result<String, String>>,
    },
    Write {
        path: String,
        value: String,
        reply: Sender<Result<(), String>>,
    },
}

/// 看门狗包装的 [`AddonIo`]
///
/// 主线程（温控）用它；真正的 sysfs 操作在专用线程里执行。
pub struct WatchedIo {
    tx: SyncSender<Req>,
    /// 已判定底层线程挂起 —— 此后所有调用立即失败（**永不再入队**）
    stuck: bool,
    /// 挂起处的描述（供宿主记事件）
    stuck_detail: Option<String>,
}

impl std::fmt::Debug for WatchedIo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatchedIo")
            .field("stuck", &self.stuck)
            .field("stuck_detail", &self.stuck_detail)
            .finish()
    }
}

impl WatchedIo {
    /// 启动 I/O 线程并返回代理
    ///
    /// `inner` 是真正的 I/O 实现（真机上是 [`crate::addon_host::SysfsIo`]，
    /// 测试里可以是任何假实现）。
    pub fn spawn(mut inner: Box<dyn AddonIo + Send>) -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Req>(8);

        // 线程名便于排查（`ps -T` 可见）
        let built = thread::Builder::new()
            .name("thermo-io".to_string())
            .spawn(move || {
                // ⚠️ 本循环一旦卡在某次 I/O 上就永远出不来 —— 这正是设计意图：
                //    卡住的是这个线程，不是温控主循环。
                while let Ok(req) = rx.recv() {
                    match req {
                        Req::Exists { path, reply } => {
                            let _ = reply.send(inner.exists(&path));
                        }
                        Req::Writable { path, reply } => {
                            let _ = reply.send(inner.writable(&path));
                        }
                        Req::Read { path, reply } => {
                            let _ = reply.send(inner.read(&path));
                        }
                        Req::Write { path, value, reply } => {
                            let _ = reply.send(inner.write(&path, &value));
                        }
                    }
                }
            });

        match built {
            Ok(_) => Self {
                tx,
                stuck: false,
                stuck_detail: None,
            },
            Err(e) => {
                // 线程都起不来 → 直接标记挂起，所有 I/O 失败（fail-closed）
                log::error!("I/O 看门狗线程创建失败：{e}；附加功能将不可用");
                let (tx, _rx) = std::sync::mpsc::sync_channel::<Req>(1);
                Self {
                    tx,
                    stuck: true,
                    stuck_detail: Some(format!("I/O 线程创建失败：{e}")),
                }
            }
        }
    }

    /// 是否已判定挂起
    ///
    /// 宿主用它决定要不要提示用户（`tick_addons` 通过 `take_stuck_detail`
    /// 记事件，本方法主要供测试与将来诊断使用）。
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_stuck(&self) -> bool {
        self.stuck
    }

    /// 取出挂起描述（取走后清空，避免重复记事件）
    pub fn take_stuck_detail(&mut self) -> Option<String> {
        self.stuck_detail.take()
    }

    /// 统一的"发请求 + 等回复"逻辑
    fn call<T: Send + 'static>(
        &mut self,
        what: &str,
        path: &str,
        make: impl FnOnce(String, Sender<T>) -> Req,
    ) -> Result<T, String> {
        if self.stuck {
            return Err(format!(
                "节点 I/O 已停用（{}）",
                self.stuck_detail.as_deref().unwrap_or("此前有写入挂起")
            ));
        }

        let (rtx, rrx) = channel::<T>();
        if let Err(e) = self.tx.try_send(make(path.to_string(), rtx)) {
            // 队列异常（线程已退出或积压）→ 同样按挂起处理，fail-closed
            let detail = format!("{what} {path} 无法入队：{e}");
            self.stuck = true;
            self.stuck_detail = Some(detail.clone());
            return Err(detail);
        }

        match rrx.recv_timeout(IO_TIMEOUT) {
            Ok(v) => Ok(v),
            Err(RecvTimeoutError::Timeout) => {
                // ⚠️ 底层线程已卡在不可中断的内核调用里 —— **绝不 join**。
                let detail = format!(
                    "{what} {path} 超过 {} 秒无响应（节点可能已挂起内核线程）",
                    IO_TIMEOUT.as_secs()
                );
                self.stuck = true;
                self.stuck_detail = Some(detail.clone());
                Err(detail)
            }
            Err(RecvTimeoutError::Disconnected) => {
                let detail = format!("{what} {path} 失败：I/O 线程已退出");
                self.stuck = true;
                self.stuck_detail = Some(detail.clone());
                Err(detail)
            }
        }
    }
}

impl AddonIo for WatchedIo {
    fn exists(&mut self, path: &str) -> bool {
        // 探测类调用：失败一律当作"不存在"（fail-closed，不误判为可用）
        self.call("探测", path, |path, reply| Req::Exists { path, reply })
            .unwrap_or(false)
    }

    fn writable(&mut self, path: &str) -> bool {
        self.call("试写", path, |path, reply| Req::Writable { path, reply })
            .unwrap_or(false)
    }

    fn read(&mut self, path: &str) -> Result<String, String> {
        self.call("读取", path, |path, reply| Req::Read { path, reply })?
    }

    fn write(&mut self, path: &str, value: &str) -> Result<(), String> {
        self.call("写入", path, |path, reply| Req::Write {
            path,
            value: value.to_string(),
            reply,
        })?
    }

    /// 是否插着充电器
    ///
    /// # 🔴 这个覆写是必须的（2026-10-20 真机 bug）
    ///
    /// 漏掉它就会落到 trait 的**默认实现（永远 `true`）**，而 worker
    /// 主循环用的正是 `WatchedIo` —— 于是真机上：
    ///
    /// | 功能 | 症状 |
    /// |---|---|
    /// | PPS 解锁 | 拔电时永不还原 `pd_verifed`；`auto` 模式的还原分支永不执行 |
    /// | 充电上限 | "未充电跳过巡检"的省电逻辑从未生效 |
    ///
    /// # 为什么不走 watchdog 队列
    ///
    /// 它只读一个几字节的 `status` 文件，且与温控主循环自己的采样同级
    /// （那边也是直接读 sysfs）。走队列反而要新增 `Req` 变体、多一次
    /// 线程往返 —— 得不偿失。
    fn is_plugged(&mut self) -> bool {
        crate::addon_host::read_is_plugged()
    }

    /// 启动子进程**不走 watchdog 线程**
    ///
    /// # 为什么可以直接在主线程做
    ///
    /// watchdog 存在的意义是"sysfs 写可能把线程卡进内核 D 状态"。
    /// 而 `spawn` 只是 `fork` + `exec`，**不碰 sysfs** —— 它不会因为
    /// 节点卡住而挂起，因此没有理由让它经过那个队列。
    ///
    /// 反过来说，如果把它塞进 watchdog 队列，会引入一个新问题：
    /// 子进程启动需要几十毫秒，而 watchdog 的调用是**同步等回复**的，
    /// 期间主线程照样阻塞 —— 达不到"不阻塞温控"的目的。
    ///
    /// ⇒ 直接委托给 [`crate::addon_host::spawn_child`]：
    ///   它只做 `Command::spawn` + 立刻返回，不等子进程结束。
    fn spawn_detached(&mut self, args: &[&str]) -> Result<(), String> {
        crate::addon_host::spawn_child(args)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// 可控的假 I/O：可指定某次写入永久卡住
    #[derive(Default)]
    struct FakeIo {
        files: HashMap<String, String>,
        /// 命中这个路径的写入会**永久阻塞**（模拟内核 D 状态）
        hang_on: Option<String>,
        /// 是否已发生过挂起（供断言之用）
        hung: Arc<AtomicBool>,
    }

    impl AddonIo for FakeIo {
        fn exists(&mut self, p: &str) -> bool {
            self.files.contains_key(p)
        }
        fn writable(&mut self, _p: &str) -> bool {
            true
        }
        fn read(&mut self, p: &str) -> Result<String, String> {
            self.files.get(p).cloned().ok_or_else(|| "不存在".into())
        }
        fn write(&mut self, p: &str, v: &str) -> Result<(), String> {
            if self.hang_on.as_deref() == Some(p) {
                self.hung.store(true, Ordering::SeqCst);
                // 永久阻塞 —— 与真机的 D 状态等价
                loop {
                    thread::sleep(Duration::from_secs(3600));
                }
            }
            self.files.insert(p.to_string(), v.to_string());
            Ok(())
        }
    }

    fn fake(
        files: &[(&str, &str)],
        hang_on: Option<&str>,
    ) -> (Box<dyn AddonIo + Send>, Arc<AtomicBool>) {
        let hung = Arc::new(AtomicBool::new(false));
        let io = FakeIo {
            files: files
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            hang_on: hang_on.map(str::to_string),
            hung: Arc::clone(&hung),
        };
        (Box::new(io), hung)
    }

    #[test]
    fn 正常读写照常工作() {
        let (inner, _) = fake(&[("/a", "1")], None);
        let mut io = WatchedIo::spawn(inner);

        assert!(io.exists("/a"));
        assert_eq!(io.read("/a").unwrap(), "1");
        assert!(io.write("/a", "2").is_ok());
        assert_eq!(io.read("/a").unwrap(), "2");
        assert!(!io.is_stuck());
    }

    // ────────────────────────────────────────────────────────────
    //  核心命题：**写入卡死不得拖死调用方**
    //
    //  这是真机事故的回归测试：写 smart_chg 让内核驱动死锁，
    //  整个 worker 进程陷入 D 状态，温控彻底停摆。
    // ────────────────────────────────────────────────────────────

    #[test]
    fn 写入卡死时调用方仍能返回() {
        let (inner, hung) = fake(&[], Some("/hang"));
        let mut io = WatchedIo::spawn(inner);

        // 这次写入会永久阻塞底层线程；调用方必须在超时后返回错误
        let start = std::time::Instant::now();
        let r = io.write("/hang", "x");
        let elapsed = start.elapsed();

        assert!(r.is_err(), "挂起的写入必须以错误返回，而不是永久阻塞");
        assert!(
            elapsed >= IO_TIMEOUT,
            "应当在超时后才返回（实得 {elapsed:?}）"
        );
        assert!(
            elapsed < IO_TIMEOUT + Duration::from_secs(2),
            "不应无谓地多等（实得 {elapsed:?}）"
        );
        assert!(io.is_stuck(), "应标记为挂起");
        assert!(hung.load(Ordering::SeqCst), "底层确实收到了那次写入");
    }

    #[test]
    fn 挂起后后续调用立即失败且不再入队() {
        let (inner, _) = fake(&[("/ok", "1")], Some("/hang"));
        let mut io = WatchedIo::spawn(inner);

        assert!(io.write("/hang", "x").is_err());
        assert!(io.is_stuck());

        // 此后即使是完全正常的路径也必须快速失败 —— 绝不再次入队
        let start = std::time::Instant::now();
        assert!(io.write("/ok", "2").is_err());
        assert!(io.read("/ok").is_err());
        assert!(!io.exists("/ok"));
        assert!(start.elapsed() < Duration::from_millis(200), "应立即失败");
    }

    #[test]
    fn 挂起详情可取出且只取一次() {
        let (inner, _) = fake(&[], Some("/hang"));
        let mut io = WatchedIo::spawn(inner);

        assert!(io.take_stuck_detail().is_none(), "尚未挂起时为空");
        assert!(io.write("/hang", "x").is_err());

        let d = io.take_stuck_detail().expect("应能取出挂起描述");
        assert!(d.contains("/hang"), "描述应包含出问题的路径，实得：{d}");
        assert!(
            io.take_stuck_detail().is_none(),
            "取走后应清空（避免重复记事件）"
        );
    }

    #[test]
    fn 读取卡死同样被拦截() {
        // 只有写会挂：读走正常路径，验证"同一线程被占住后读也会超时"
        let (inner, _) = fake(&[], Some("/hang"));
        let mut io = WatchedIo::spawn(inner);
        assert!(io.write("/hang", "x").is_err(), "先让线程卡住");
        // 线程已卡在写里，后续读会因 stuck 而立即失败
        assert!(io.read("/whatever").is_err());
    }
}
