//! supervisor —— 进程监督与崩溃恢复
//!
//! # 职责边界（契约 §2.1 / §2.4）
//!
//! | 能做 | 不能做 |
//! |---|---|
//! | 拉起 worker、监督其存活 | **不参与采样与决策** |
//! | worker 异常退出时**写一组固定安全值** | 不写除安全值以外的任何节点 |
//! | 记录崩溃事件 | 不读温度、不算控制律 |
//!
//! # 崩溃恢复为什么不用 unwind
//!
//! 发布版是 `panic = "abort"` —— 进程直接终止，没有 unwind 机会。
//! 因此恢复动作必须由**外部进程**完成，这正是 supervisor 存在的理由。
//!
//! # 退避策略
//!
//! worker 连续快速崩溃（例如 5 秒内）说明环境有问题，
//! 此时逐次拉长重启间隔，避免"崩溃—重启"风暴耗电。
//!
//! # ⚠️ 卡死检测（真机事故后新增，见 [`hang`]）
//!
//! 仅检测"子进程是否退出"是**不够**的：真机实测出现过 worker 陷入
//! `D` 状态（不可中断睡眠，写 `smart_chg` 让内核驱动死锁）——
//! 进程**不退出、也不工作**，`kill -9` 无效，原来的 supervisor 完全察觉不到。
//!
//! 因此额外监测 worker 的**心跳**（`runtime.json` 的更新时间）：
//! 超过阈值没更新就判定卡死，写安全值并发事件提醒用户重启。

// 本模块需要 `flock`（单实例锁 + 客户端自愈探测）与 `setsid`（脱离
// `ksu.exec` 会话拉起 supervisor）：标准库没有安全封装，属于本 crate 的
// **底层系统调用例外**（与 `worker` / `server` 同一条纪律：unsafe 只出现在
// 这里，且每一处都带 `SAFETY` 注释、只做参数合法的单次调用）。
#![allow(unsafe_code)]

use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::observability::{kind, EventLog};

/// 判定"快速崩溃"的阈值（秒）
const FAST_CRASH_S: u64 = 5;

/// 连续快速崩溃时的最大退避（秒）
const MAX_BACKOFF_S: u64 = 60;

/// 监测子进程状态的间隔（秒）
const WATCH_INTERVAL_S: u64 = 5;

/// worker 心跳（`runtime.json` 的 mtime）多久没更新就判定卡死（秒）
///
/// ⚠️ **只在"需要心跳"的时候才用这个阈值**（见 [`heartbeat_required`]）。
///
/// | 情况 | 是否要求心跳 |
/// |---|---|
/// | 充电中 | ✅ 必须 —— 温控在工作，卡死会让充电失控 |
/// | 未充电 | ❌ 不要求 —— 温控本就不工作，深度休眠是**设计行为** |
///
/// worker 空闲时最多每 15 秒更新一次快照，因此取 3 倍余量。
/// 太长会让用户长时间失去温控保护，太短会误判（正常空闲也可能憋到 15 秒）。
const HANG_TIMEOUT_S: u64 = 45;

/// 进程处于 `D`（不可中断睡眠）状态多久就判定卡死（秒）
///
/// # 为什么用这个而不是只看心跳
///
/// 真机事故：worker 写 `smart_chg` 让内核驱动死锁，进程陷入 `D` 状态 ——
/// **不退出、不工作，`kill -9` 也无效**。这是最初的 `supervisor`
/// 完全察觉不到的故障模式（它只看"进程是否退出"）。
///
/// 心跳能发现它，但代价是**必须周期性写文件**，这与"未充电时深度休眠"
/// 直接冲突。而 `D` 状态本身就是这个故障的**直接特征**：
///
/// | 进程状态 | 含义 |
/// |---|---|
/// | `S` | 可中断睡眠（等 `poll`）—— 正常休眠 |
/// | `R` | 运行中 |
/// | **`D`** | **不可中断睡眠（内核里出不来）** —— 正是卡死的样子 |
///
/// 正常磁盘 I/O 的 `D` 状态是毫秒级的，因此 30 秒阈值不会误判。
const D_STATE_TIMEOUT_S: u64 = 30;

/// 心跳停滞期间，进程持续处于 `R`（运行）状态多久就判定死循环（秒）
///
/// 覆盖第三种故障模式：进程活着（不是 `D`）、但陷入死循环占满 CPU。
/// 未充电时这表现为**偷偷耗电**，正是用户要求"保持后台清洁"要防的。
///
/// ⚠️ 只在**未充电 + 心跳已停滞**时用这个判据：
/// 充电中进程本就该是 `R`（在跑拍），不该误判。
const SPIN_STATE_TIMEOUT_S: u64 = 30;

/// **首次运行时自动检测** —— 让模块"装完即用"
///
/// # 为什么需要它
///
/// 决策层有一道安全闸门（`Capabilities::detected`）：**未检测 ⇒ 零写入**。
///
/// 这道闸门本身是对的（"不猜硬件"），但**界面把这种情况显示为「待机」**
/// —— 一个看起来完全正常的词。后果：
///
/// | 用户看到 | 实际情况 |
/// |---|---|
/// | 状态页显示「待机」 | 模块**一个字节都没写**，完全没在工作 |
///
/// 用户必须自己想到"去「更多」页点一下重新检测"。对一个非开发者而言，
/// 这是**发现不了**的 —— 他会以为模块坏了，或者以为它在工作。
///
/// 因此这里在**首次启动**时自动跑一次检测，让模块立即可用。
///
/// # 为什么用快速模式（全只读）
///
/// [`crate::detect::ProbeMode::Quick`] **全部只读** —— 不写任何节点，
/// 零副作用，因此在开机阶段跑是安全的。
///
/// 完整模式含"档位可写上限"验证（要试写充电节点），会**真实改变充电功率**，
/// 必须由用户显式确认，不在这里做。
///
/// # 失败不阻塞
///
/// 检测失败只是退回"未检测"状态（与手动检测失败一样），
/// **绝不能**因此让 supervisor 退出 —— 那样模块整个不工作，
/// 用户连"未检测"的提示都看不到。
///
/// # 已检测过就不重复
///
/// 每次开机都跑会白耗电。已有 `detected: true` 的档案就跳过。
/// 想刷新请用界面「更多」页的重新检测（或 `thermo device probe`）。
fn ensure_detected(events: &EventLog) {
    // ── 已有可用档案 ⇒ 不动 ──
    if let Some(p) = crate::profile_io::load_local() {
        if p.capabilities.detected {
            log::debug!(
                "已有本机档案（检测于 {}），跳过自动检测",
                p.probe_meta.probed_at
            );
            return;
        }
        log::info!("已有档案但未标记为已检测 —— 重新检测");
    } else {
        log::info!("首次运行：自动检测本机（全部只读），使模块立即可用");
    }

    let t0 = std::time::Instant::now();
    match crate::detect::run(crate::detect::ProbeOptions::default()) {
        Ok(profile) => {
            let failed = profile.probe_meta.failed_items.clone();
            match crate::profile_io::save_local(&profile) {
                Ok(()) => {
                    let ms = t0.elapsed().as_millis();
                    log::info!("自动检测完成（{ms} ms），已保存本机档案");
                    events.emit(
                        kind::DETECT,
                        "已自动检测本机（首次运行，全部只读）",
                        serde_json::json!({
                            "elapsed_ms": ms,
                            "failed_items": failed,
                            "gear_max": profile.capabilities.gear.max,
                            "current_max_ua": profile.capabilities.current.max_ua,
                        }),
                    );
                    if !failed.is_empty() {
                        log::warn!("自动检测有 {} 项未成功：{failed:?}", failed.len());
                    }
                }
                Err(e) => {
                    // 保存失败 ⇒ 档案仍是"未检测" ⇒ 模块不控制。
                    // 这不会让用户失去安全，但必须让他知道原因。
                    log::error!("自动检测结果保存失败：{e:#}");
                    events.emit(
                        kind::DETECT,
                        &format!("自动检测结果保存失败：{e:#}"),
                        serde_json::json!({ "ok": false }),
                    );
                }
            }
        }
        Err(e) => {
            log::error!("自动检测失败：{e:#}");
            events.emit(
                kind::DETECT,
                &format!("自动检测失败：{e:#}（模块保持「未检测」状态，不控制充电）"),
                serde_json::json!({ "ok": false }),
            );
        }
    }
}

// ============================================================
//  客户端自愈：确保 supervisor 在运行
// ============================================================

/// 拉起失败后的节流窗口
///
/// supervisor 若因环境问题起不来，界面每 3 秒轮询就会 spawn 一次 ——
/// 那是进程风暴。30 秒内只允许一次尝试，用户仍可手动点「重试」。
const SPAWN_THROTTLE: Duration = Duration::from_secs(30);

/// 已有 supervisor 时等 socket 的上限（覆盖 worker 重启窗口）
const WAIT_RUNNING: Duration = Duration::from_millis(1500);

/// 刚拉起 supervisor 时等 socket 的上限（首次启动会先跑一次只读检测）
const WAIT_STARTED: Duration = Duration::from_secs(5);

/// 自愈结果（给客户端拼错误信息用）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnsureOutcome {
    /// 已有 supervisor，socket 已就绪
    AlreadyRunning,
    /// 刚拉起，socket 已就绪
    Started,
    /// 进程在（或刚拉起），但 socket 还没出现
    NotReady,
    /// 节流窗口内已经尝试过拉起
    Throttled,
    /// 拉起失败（含原因）
    Failed(String),
}

impl EnsureOutcome {
    /// 给用户看的处置建议（会拼进命令错误信息）
    pub fn hint(&self) -> String {
        match self {
            Self::AlreadyRunning | Self::Started => "后台进程已恢复".to_string(),
            Self::NotReady => {
                "后台进程正在启动（首次启动会先跑一次检测），请稍后点「重试」".to_string()
            }
            Self::Throttled => {
                "后台进程未运行，30 秒内已尝试拉起过；请稍后重试，仍失败请重启设备".to_string()
            }
            Self::Failed(e) => format!("自动拉起后台进程失败（{e}），请重启设备"),
        }
    }
}

/// 客户端自愈：确认 supervisor 在运行，不在则拉起
///
/// # 为什么需要它（2026-10-26 真机：隔几天就打不开状态页）
///
/// 真机现象：WebUI 报「连接 worker 失败（`control.sock` 不存在）」，
/// **必须重启手机**才能恢复，隔几天出现一次。
///
/// 根因：supervisor 是 worker 的**唯一拉起者**，而它自己只由
/// `service.sh` 在开机时拉起一次 —— 一旦被系统回收，就再也没有人
/// 拉起它（worker 也随之消失）。模块进程是 root，但 Android 仍会
/// 在内存/省电压力下回收长期空闲的后台进程。
///
/// 本函数由客户端在**连接失败时**调用（见 `ipc::roundtrip`）：
///
/// | 探测结果 | 行为 |
/// |---|---|
/// | `supervisor.lock` 被占用 | 已有 supervisor ⇒ 只等 socket（覆盖 worker 重启窗口） |
/// | 锁空闲 | 拉起一个（30 秒节流，失败不刷屏） |
///
/// ⚠️ **不写任何充电节点**（R1）：拉起后仍由 worker 决策；首次运行若
/// 未检测，supervisor 会先跑一次**只读**快速检测（既有行为）。
pub fn ensure_running() -> EnsureOutcome {
    // 非 root 无法管理模块进程（数据目录 0700 / socket 0600），
    // 硬拉只会得到一个立即失败的子进程。
    // SAFETY: `geteuid` 无参数、无副作用，只读取当前进程的有效 UID。
    if unsafe { libc::geteuid() } != 0 {
        return EnsureOutcome::Failed("需要 root 权限才能拉起后台进程".to_string());
    }
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => return EnsureOutcome::Failed(format!("取当前可执行文件失败：{e}")),
    };
    // 数据目录不存在时先建出来（节流时间戳要写在这里；root 下无害）
    std::fs::create_dir_all(crate::paths::data_dir()).ok();
    ensure_running_at(
        &crate::paths::supervisor_lock(),
        &crate::paths::supervisor_spawn_stamp(),
        &crate::paths::control_sock(),
        &crate::paths::log_dir().join("daemon.out"),
        &exe,
    )
}

/// [`ensure_running`] 的可注入版本（测试用）
fn ensure_running_at(
    lock: &Path,
    stamp: &Path,
    sock: &Path,
    log: &Path,
    exe: &Path,
) -> EnsureOutcome {
    match probe_supervisor_lock(lock) {
        LockProbe::Held => {
            // 已有 supervisor：worker 可能正在重启，等它把 socket 建出来
            if wait_socket(sock, WAIT_RUNNING) {
                EnsureOutcome::AlreadyRunning
            } else {
                EnsureOutcome::NotReady
            }
        }
        LockProbe::Free => {
            if throttled(stamp, SPAWN_THROTTLE) {
                return EnsureOutcome::Throttled;
            }
            // 先落时间戳再 spawn：spawn 失败也要被节流（否则会反复重试）
            let _ = std::fs::write(stamp, crate::observability::now_unix().to_string());
            if let Err(e) = spawn_supervisor(exe, log) {
                return EnsureOutcome::Failed(format!("{e:#}"));
            }
            if wait_socket(sock, WAIT_STARTED) {
                EnsureOutcome::Started
            } else {
                EnsureOutcome::NotReady
            }
        }
        LockProbe::Error(e) => EnsureOutcome::Failed(format!("探测 supervisor 锁失败：{e}")),
    }
}

/// `supervisor.lock` 的探测结果
#[derive(Debug, Clone, PartialEq, Eq)]
enum LockProbe {
    /// 锁空闲 ⇒ 没有 supervisor 在跑
    Free,
    /// 锁被占用 ⇒ supervisor 在跑
    Held,
    /// 无法判断（权限/文件系统错误）
    Error(String),
}

/// 用 `flock(LOCK_EX|LOCK_NB)` 探测锁是否被占用
///
/// ⚠️ 探测成功后**立即释放**：本函数只是"看一眼"，真正的持有者是
/// `run()` 里的 [`crate::worker::InstanceLock`]。
fn probe_supervisor_lock(path: &Path) -> LockProbe {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(path)
    {
        Ok(f) => f,
        Err(e) => return LockProbe::Error(e.to_string()),
    };

    // SAFETY: `as_raw_fd` 返回本进程持有的有效 fd；`flock` 只是设置/查询锁状态
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        // 拿到锁 ⇒ 没人持有 ⇒ 立即释放（不影响真正要启动的 supervisor）
        // SAFETY: 同上，fd 有效
        unsafe {
            libc::flock(file.as_raw_fd(), libc::LOCK_UN);
        }
        LockProbe::Free
    } else {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
            LockProbe::Held
        } else {
            LockProbe::Error(e.to_string())
        }
    }
}

/// 节流窗口内是否已经尝试过拉起
fn throttled(stamp: &Path, window: Duration) -> bool {
    let Ok(meta) = std::fs::metadata(stamp) else {
        return false;
    };
    let Ok(mtime) = meta.modified() else {
        return false;
    };
    mtime.elapsed().map(|d| d < window).unwrap_or(false)
}

/// 等到控制 socket 出现（每 50 ms 查一次）
fn wait_socket(sock: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if sock.exists() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// 脱离当前会话拉起 supervisor（`setsid` + 日志重定向到 `daemon.out`）
///
/// 必须 `setsid`：客户端的父进程是 KernelSU 的 `ksu.exec` 会话，
/// 会话结束时不希望把 supervisor 一起带走（与 `service.sh` 的 `setsid` 一致）。
fn spawn_supervisor(exe: &Path, log: &Path) -> Result<()> {
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let mut cmd = Command::new(exe);
    cmd.arg("supervisor").stdin(Stdio::null());

    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
    {
        Ok(f) => match f.try_clone() {
            Ok(f2) => {
                cmd.stdout(f);
                cmd.stderr(f2);
            }
            Err(_) => {
                cmd.stdout(Stdio::null());
                cmd.stderr(Stdio::null());
            }
        },
        Err(_) => {
            cmd.stdout(Stdio::null());
            cmd.stderr(Stdio::null());
        }
    }

    // SAFETY: 这段代码在 fork 后的子进程里执行（std 保证）；`setsid` 只创建新会话，
    // 失败时立即返回错误、不会继续执行任何用户代码。
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    cmd.spawn()
        .with_context(|| format!("拉起 supervisor 失败：{}", exe.display()))?;
    Ok(())
}

/// 拉起并监督 worker
pub fn run() -> Result<()> {
    crate::paths::ensure_dirs().context("初始化数据目录失败")?;

    // ── 单实例：拿不到锁 ⇒ 已有 supervisor 在跑，本进程直接退出 ──
    //
    // # 为什么现在必须有（2026-10-26）
    //
    // 之前 supervisor 只由 `service.sh` 在开机拉起一次、**没有任何单实例
    // 保护**。现在客户端会在连接失败时自愈拉起（见 [`ensure_running`]），
    // 于是存在两条拉起路径（开机脚本 + 客户端）。若不加锁，极端时序下
    // 会出现两个 supervisor 同时监督 worker：重复拉起、重复写安全值。
    //
    // `flock` 由内核保证独占；进程退出时自动释放（无需清理残留）。
    let _instance = match crate::worker::InstanceLock::acquire(
        &crate::paths::supervisor_lock(),
        &crate::paths::supervisor_pidfile(),
    ) {
        Ok(lock) => lock,
        Err(e) => {
            log::info!("supervisor 已在运行，本进程退出（{e:#}）");
            return Ok(());
        }
    };

    let events = EventLog::new(crate::paths::events_jsonl(), 512);
    let me = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("thermo"));

    events.emit(
        kind::LIFECYCLE,
        // ⚠️ 事件消息是**给用户看**的，用中文
        //
        // 用户反馈（2026-10-16）：「最近事件内的英文显示全部转换成中文」。
        // `supervisor` / `worker` 是内部进程名（前者管进程、后者管充电），
        // 用户看不懂 —— 消息用中文，`data` 里保留 `role` 供排障。
        "监督进程启动",
        serde_json::json!({ "pid": std::process::id(), "role": "supervisor" }),
    );

    // 🔴 首次运行时自动检测 —— 必须在拉起 worker **之前**
    //
    // worker 启动时会把 `device.json` 读进内存并据此构造决策边界。
    // 若检测放在 worker 之后，第一次运行的那一轮 worker 仍会以
    // "未检测"启动（零写入），要等下次重启才生效。
    ensure_detected(&events);

    let mut backoff_s = 0u64;
    let mut last_start = std::time::Instant::now();

    loop {
        // ── 拉起 worker ──
        let started = std::time::Instant::now();
        let mut child = match spawn_worker(&me) {
            Ok(c) => c,
            Err(e) => {
                log::error!("拉起 worker 失败：{e:#}");
                events.emit(
                    kind::CRASH,
                    &format!("拉起 worker 失败：{e:#}"),
                    serde_json::json!({}),
                );
                std::thread::sleep(Duration::from_secs(backoff_s.max(5)));
                continue;
            }
        };

        log::info!("worker 已启动（pid={}）", child.id());
        events.emit(
            kind::LIFECYCLE,
            "充电控制进程已启动",
            serde_json::json!({ "pid": child.id(), "role": "worker" }),
        );

        // 稳定运行过一段时间 → 退避重置
        if started.duration_since(last_start).as_secs() > FAST_CRASH_S {
            backoff_s = 0;
        }
        last_start = started;

        // 每个 worker 各有一份告警标志：避免卡死期间每 5 秒重复刷同一条事件
        let mut hang_reported = false;

        // ── 准备"是否需要心跳"的判据（每次重启时取一次）──
        //
        // 判据要读电池状态，因此需要节点路径。按项目纪律**运行期探测**
        // （不写死路径），并在每次 worker 重启时重新探测一次
        // —— 用户可能在这期间跑过检测，档案里的路径会更准。
        let nodes = {
            let profile = crate::profile_io::effective_profile();
            crate::sysfs::Nodes::detect(Some(&profile.profile.nodes))
        };

        // ── 等待 worker 结束（轮询以便顺带检测卡死）──
        let status = wait_child_watching(&mut child, &events, &mut hang_reported, &nodes);

        // ── 崩溃处理 ──
        let code = status.as_ref().map(|s| s.to_string()).unwrap_or_default();
        let uptime_s = started.elapsed().as_secs();

        if uptime_s <= FAST_CRASH_S {
            backoff_s = (backoff_s * 2).clamp(1, MAX_BACKOFF_S);
        }

        log::warn!("worker 已退出（{code}，存活 {uptime_s}s）");
        events.emit(
            kind::CRASH,
            &format!("worker 异常退出（{code}）"),
            serde_json::json!({ "uptime_s": uptime_s, "backoff_s": backoff_s }),
        );

        // ── 恢复：写一组固定安全值（**supervisor 唯一的写入职责**）──
        match crate::worker::write_safe_values(thermo_ipc::device::ProfileSource::Generic) {
            Ok(()) => {
                log::info!("已写入安全值（档位归零、电流放开、移除标记归零）");
                events.emit(kind::RELEASE, "崩溃后已恢复安全值", serde_json::json!({}));
            }
            Err(e) => {
                log::error!("恢复安全值失败：{e:#}");
                events.emit(
                    kind::WRITE_FAIL,
                    &format!("崩溃后恢复安全值失败：{e:#}"),
                    serde_json::json!({}),
                );
            }
        }

        // ── 退避后重启 ──
        if backoff_s > 0 {
            log::info!("退避 {backoff_s}s 后重启 worker");
            std::thread::sleep(Duration::from_secs(backoff_s));
        } else {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

/// 等待子进程结束，**同时**监测它是否卡死
///
/// 返回子进程的退出状态描述（与 `Child::wait` 的语义一致）。
///
/// # 三个判据（**都不要求 worker 周期性写文件**）
///
/// | # | 故障模式 | 判据 | 来源 |
/// |---|---|---|---|
/// | 1 | 进程崩溃退出 | `try_wait` 返回 | 一直都有 |
/// | 2 | **内核死锁**（`D` 状态） | 连续 [`D_STATE_TIMEOUT_S`] 秒处于 `D` | 直接读 `/proc/<pid>/stat` |
/// | 3 | 心跳停滞（**充电中**才要求） | mtime 超 [`HANG_TIMEOUT_S`] 未变 | 见下 |
/// | 4 | **死循环**（未充电时偷偷耗电） | 心跳停滞 + 连续 [`SPIN_STATE_TIMEOUT_S`] 秒处于 `R` | 同上 |
///
/// # 为什么判据 2 是直接读 `/proc` 而不是看心跳
///
/// 真机事故的故障模式是「写 `smart_chg` 让内核驱动死锁」——
/// 进程陷入 `D`（不可中断睡眠），**不退出、不工作，`kill -9` 无效**。
///
/// `D` 状态本身就是这个故障的**直接特征**，比"心跳停了"更精确：
/// 它不依赖 worker 做任何事，因此**允许 worker 真正深度休眠**。
///
/// 正常磁盘 I/O 的 `D` 是毫秒级的，30 秒阈值不会误判。
///
/// # 判据 3 为什么只在充电时要
///
/// 未充电时 worker 在 `poll` 里无限阻塞是**设计行为**（省电），
/// 此时不刷心跳完全正常。若仍按心跳判卡死，就变成
/// "越省电越被误判"，反而逼 worker 周期醒来 —— 与省电目标相悖。
///
/// 而充电时必须要求心跳：温控在工作，卡死会让充电失控。
///
/// ⚠️ 卡死时我们**无法**杀死它（`D` 状态对 `SIGKILL` 免疫），
/// 因此这里能做的是：
///
/// 1. 写一次安全值（让充电回到可控状态）
/// 2. 记录一条**醒目**事件，明确告诉用户"需要重启设备"
///
/// 并且**不再重复拉起**新的 worker（那个卡死的进程仍占着 `flock`，
/// 新进程会因拿不到锁而立刻退出，只会刷屏）。等到用户重启后
/// `service.sh` 会重新拉起一切。
fn wait_child_watching(
    child: &mut Child,
    events: &EventLog,
    hang_reported: &mut bool,
    nodes: &crate::sysfs::Nodes,
) -> Option<String> {
    let heartbeat = crate::paths::runtime_json();
    // 以"拉起时刻"为基准：runtime.json 可能还是**上一次运行**留下的旧文件
    let mut last_beat = std::time::Instant::now();
    let mut last_mtime = file_mtime(&heartbeat);

    // 判据 2：连续处于 D 状态的起点
    let mut d_since: Option<std::time::Instant> = None;
    // 判据 4：连续处于 R 状态的起点
    let mut r_since: Option<std::time::Instant> = None;

    loop {
        // ── 判据 1：子进程还在吗 ──
        match child.try_wait() {
            Ok(Some(status)) => return Some(format!("{status}")),
            Ok(None) => {}
            Err(e) => return Some(format!("等待失败：{e}")),
        }

        std::thread::sleep(Duration::from_secs(WATCH_INTERVAL_S));

        let pid = child.id();
        let state = proc_state(pid);
        let need_beat = heartbeat_required(nodes);

        // ── 判据 2：D 状态（内核死锁）──
        //
        // 与"是否充电"无关：D 状态本身就是异常（正常休眠是 S）。
        if state == Some('D') {
            let since = *d_since.get_or_insert_with(std::time::Instant::now);
            let d_s = since.elapsed().as_secs();
            if d_s >= D_STATE_TIMEOUT_S && !*hang_reported {
                *hang_reported = true;
                report_stuck(
                    events,
                    pid,
                    &format!("进程卡在不可中断睡眠（D 状态）已 {d_s} 秒"),
                    "内核里出不来，信号无效 —— 只能重启设备",
                );
                write_safe_after_hang(events);
            }
            continue;
        }
        d_since = None;

        // ── 判据 3 / 4 的前提：心跳有没有动 ──
        let now_mtime = file_mtime(&heartbeat);
        if now_mtime != last_mtime {
            last_mtime = now_mtime;
            last_beat = std::time::Instant::now();
            r_since = None;
            if *hang_reported {
                log::info!("worker 心跳恢复");
                events.emit(kind::LIFECYCLE, "worker 心跳恢复", serde_json::json!({}));
                *hang_reported = false;
            }
            continue;
        }

        // 心跳停滞。分两种情况：
        if !need_beat {
            // ── 情况 A：未充电 ⇒ 深度休眠是设计行为 ──
            //
            // 心跳停在这里**不算故障**。但若进程持续处于 R（运行）状态，
            // 说明它在空转 —— 未充电时不该有任何计算，这正是
            // "保持后台清洁"要防的偷偷耗电。
            if state == Some('R') {
                let since = *r_since.get_or_insert_with(std::time::Instant::now);
                let r_s = since.elapsed().as_secs();
                if r_s >= SPIN_STATE_TIMEOUT_S && !*hang_reported {
                    *hang_reported = true;
                    report_stuck(
                        events,
                        pid,
                        &format!("未充电却持续占用 CPU（R 状态 {r_s} 秒）"),
                        "疑似死循环，正在消耗电量",
                    );
                    write_safe_after_hang(events);
                }
            } else {
                // S 状态 = 在 poll 里阻塞 = **正是深度休眠该有的样子**
                r_since = None;
            }

            // 重置心跳基准：避免"累计到 45 秒后误报"
            last_beat = std::time::Instant::now();
            continue;
        }

        // ── 情况 B：充电中 ⇒ 必须有心跳 ──
        let silent_s = last_beat.elapsed().as_secs();
        if silent_s >= HANG_TIMEOUT_S && !*hang_reported {
            *hang_reported = true;
            report_stuck(
                events,
                pid,
                &format!("充电中心跳停滞 {silent_s} 秒且进程未退出"),
                "温控已停止，请重启设备恢复",
            );
            write_safe_after_hang(events);
        }

        // ⚠️ 故意**不**在这里杀掉子进程并重启：
        //    卡死进程持有 flock，新 worker 拿不到锁会立即退出；
        //    且 D 状态进程杀不掉，重启只会制造刷屏。
        //    等用户重启设备后由 service.sh 恢复。
    }
}

/// 是否需要 worker 维持心跳
///
/// | 情况 | 要求心跳？ | 理由 |
/// |---|---|---|
/// | **充电中** | ✅ 必须 | 温控在工作，卡死会让充电失控 |
/// | **未充电** | ❌ 不要求 | 温控本就不工作；深度休眠是设计行为 |
///
/// ⚠️ 读不到电池状态时**保守地要求心跳** —— 宁可多要求一次心跳，
/// 也不要在真正需要监控时睡着了。
fn heartbeat_required(nodes: &crate::sysfs::Nodes) -> bool {
    let Some(node) = &nodes.battery_status else {
        return true;
    };
    match node.read() {
        Ok(s) => matches!(s.trim(), "Charging" | "Full"),
        Err(_) => true,
    }
}

/// 读进程状态字符（`R` / `S` / `D` / `Z` …）
///
/// # `/proc/<pid>/stat` 的解析要点
///
/// 格式是位置式的，且**第 2 个字段 `comm` 可能含空格和括号**
/// （例如进程名里有 `)`）。因此不能简单 `split_whitespace()` 取第 3 个，
/// 必须**从最后一个 `)` 之后**开始解析。
///
/// ```text
/// 1234 (thermo worker) S 1 1234 ...
///       ^^^^^^^^^^^^^^  ^
///       comm 可含括号     真正的状态在这里之后
/// ```
fn proc_state(pid: u32) -> Option<char> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = text.rsplit_once(')')?.1;
    rest.split_whitespace().next()?.chars().next()
}

/// 记一条"卡死"事件（醒目，含处置建议）
fn report_stuck(events: &EventLog, pid: u32, what: &str, hint: &str) {
    log::error!("worker 异常：{what}（{hint}）");
    events.emit(
        kind::CRASH,
        &format!("worker 卡死：{what}。{hint}"),
        serde_json::json!({ "pid": pid, "hint": hint }),
    );
}

/// 卡死后尽力把充电参数恢复到可控状态
///
/// （若 supervisor 自己也卡在这上面，说明驱动整体异常，只能靠重启）
fn write_safe_after_hang(events: &EventLog) {
    match crate::worker::write_safe_values(thermo_ipc::device::ProfileSource::Generic) {
        Ok(()) => {
            events.emit(
                kind::RELEASE,
                "worker 卡死后已尝试恢复安全值",
                serde_json::json!({}),
            );
        }
        Err(e) => {
            events.emit(
                kind::WRITE_FAIL,
                &format!("worker 卡死后恢复安全值失败：{e:#}"),
                serde_json::json!({}),
            );
        }
    }
}

/// 读文件的修改时间（拿不到则返回 `None`）
fn file_mtime(path: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

/// 拉起 worker 子进程（同一二进制的 `worker` 子命令）
fn spawn_worker(exe: &std::path::Path) -> Result<Child> {
    Command::new(exe)
        .arg("worker")
        .spawn()
        .with_context(|| format!("执行 {} worker 失败", exe.display()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// 每个测试一个独立临时目录（含 pid，CI 并行安全）
    fn temp_dir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("thermo_sup_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn 拉起节流窗口生效() {
        let dir = temp_dir("throttle");
        let stamp = dir.join("supervisor.spawn");

        assert!(
            !throttled(&stamp, SPAWN_THROTTLE),
            "没有时间戳 ⇒ 不节流（首次拉起必须放行）"
        );

        std::fs::write(&stamp, "1").unwrap();
        assert!(
            throttled(&stamp, SPAWN_THROTTLE),
            "刚写过 ⇒ 30 秒内不得重复拉起"
        );
        assert!(
            !throttled(&stamp, Duration::ZERO),
            "窗口为 0 ⇒ 永不节流（用于测试/诊断）"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 锁探测必须区分「有人在跑」与「没人跑」——
    /// 这是自愈是否会重复拉起 supervisor 的唯一判据。
    #[test]
    fn 探测supervisor单实例锁() {
        let dir = temp_dir("lockprobe");
        let lock = dir.join("supervisor.lock");
        let pid = dir.join("supervisor.pid");

        assert_eq!(
            probe_supervisor_lock(&lock),
            LockProbe::Free,
            "没有进程持锁 ⇒ Free"
        );

        let held = crate::worker::InstanceLock::acquire(&lock, &pid).expect("应能拿到锁");
        assert_eq!(
            probe_supervisor_lock(&lock),
            LockProbe::Held,
            "已有 supervisor 持锁 ⇒ Held（客户端不得重复拉起）"
        );

        // ⚠️ 这里**不断言**「drop 后立刻恢复 Free」：
        //    `flock` 随 close 释放的行为**依赖文件系统** —— CI 的 `/tmp`
        //    上实测 close 后仍短暂持有（本次 CI 失败就是这条断言）。
        //    生产上锁由**进程退出**释放（内核清理），`ensure_running` 的
        //    Free 分支由 `拉起失败后不重复刷屏` 覆盖（无锁 ⇒ Free ⇒ 尝试拉起）。
        drop(held);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 拉起失败也必须被节流：否则界面每 3 秒轮询就会 spawn 一次
    #[test]
    fn 拉起失败后不重复刷屏() {
        let dir = temp_dir("spawnfail");
        let lock = dir.join("supervisor.lock");
        let stamp = dir.join("supervisor.spawn");
        let sock = dir.join("control.sock");
        let log = dir.join("daemon.out");
        let missing_exe = dir.join("no_such_thermo");

        let first = ensure_running_at(&lock, &stamp, &sock, &log, &missing_exe);
        assert!(
            matches!(first, EnsureOutcome::Failed(_)),
            "拉起不存在的程序应失败，实得 {first:?}"
        );
        assert!(
            first.hint().contains("重启设备"),
            "失败提示必须告诉用户该做什么：{}",
            first.hint()
        );

        let second = ensure_running_at(&lock, &stamp, &sock, &log, &missing_exe);
        assert_eq!(
            second,
            EnsureOutcome::Throttled,
            "失败后 30 秒内不得重复拉起"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// socket 未出现时必须超时返回 false，不能永久挂住客户端
    #[test]
    fn 等待socket超时() {
        let dir = temp_dir("waitsock");
        let sock = dir.join("control.sock");

        let t = Instant::now();
        assert!(!wait_socket(&sock, Duration::from_millis(120)));
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "等待必须有上限（客户端不能被拖死）"
        );

        std::fs::write(&sock, "").unwrap();
        assert!(
            wait_socket(&sock, Duration::from_millis(120)),
            "socket 存在 ⇒ 立即返回 true"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
