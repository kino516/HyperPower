//! worker —— 采样 / 决策 / **唯一常规 sysfs 写者**
//!
//! # 调度（契约 §2.3）
//!
//! | 项 | 设定 |
//! |---|---|
//! | 充电中巡检 | **1~3 秒**（须落在系统覆盖窗口内） |
//! | 空闲巡检 | 10~30 秒 |
//! | 定时器 | **`timerfd`**（不用会漂移的 sleep 循环） |
//! | 时间基准 | **`CLOCK_BOOTTIME`** —— 息屏挂起/恢复后计时不错乱 |
//! | 事件唤醒 | `power_supply` uevent（插电/拔电/状态变化）优先唤醒 |
//! | 降级 | uevent 订阅失败 → 退回纯定时轮询（**必须实现**） |
//!
//! # 单实例（契约 §2.1）
//!
//! **`flock` + pidfile**，不做进程名匹配（进程名匹配无单实例保证、易误杀）。

// ⚠️ 本模块是 crate 内**唯一**允许 `unsafe` 的地方之一（另一处是 `server.rs`）：
//    `timerfd` / `poll` / `flock` / `recv` / `sigaction` 在标准库中没有安全封装。
//    每个 `unsafe` 块都紧邻 `SAFETY` 注释说明为何安全。
#![allow(unsafe_code)]

use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use thermo_ipc::device::ProfileSource;

use crate::config_store::ConfigStore;
use crate::engine::{Engine, EngineInit};
use crate::observability::{kind, EventLog, SnapshotWriter};
use crate::sysfs::{GearTable, Nodes};

/// 终止请求标志（由信号处理器置位）
static TERM_REQUESTED: AtomicBool = AtomicBool::new(false);
/// 重载请求标志
static RELOAD_REQUESTED: AtomicBool = AtomicBool::new(false);

/// 信号处理器：**只能做 async-signal-safe 的操作**
///
/// `AtomicBool::store` 是 signal-safe 的；这里不做任何 I/O、不加锁。
extern "C" fn on_term_signal(_sig: libc::c_int) {
    TERM_REQUESTED.store(true, Ordering::Relaxed);
}

extern "C" fn on_reload_signal(_sig: libc::c_int) {
    RELOAD_REQUESTED.store(true, Ordering::Relaxed);
}

/// 注册信号处理器（SIGTERM / SIGINT → 优雅退出；SIGHUP → 重载配置）
///
/// ⚠️ `SA_RESTART` 让被信号打断的 `poll` 自动重启，避免主循环被频繁唤醒。
fn install_signal_handlers() -> Result<()> {
    // SAFETY: sigaction 是 POD；处理器函数签名正确；
    //         sigemptyset 初始化 sa_mask；null_mut() 表示不关心旧值；
    //         `as *const ()` 是函数指针转 usize 的标准两段式转换。
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_term_signal as *const () as usize;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut sa.sa_mask);
        if libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut()) != 0 {
            return Err(std::io::Error::last_os_error()).context("注册 SIGTERM 失败");
        }
        if libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut()) != 0 {
            return Err(std::io::Error::last_os_error()).context("注册 SIGINT 失败");
        }

        let mut sh: libc::sigaction = std::mem::zeroed();
        sh.sa_sigaction = on_reload_signal as *const () as usize;
        sh.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut sh.sa_mask);
        if libc::sigaction(libc::SIGHUP, &sh, std::ptr::null_mut()) != 0 {
            return Err(std::io::Error::last_os_error()).context("注册 SIGHUP 失败");
        }

        // 忽略 SIGPIPE：向已关闭的 socket 写入不应杀死进程
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    Ok(())
}

/// 充电中的巡检间隔（秒）
///
/// 温控闭环的响应速度由它决定：温度变化后最多 3 秒开始降速。
const PATROL_CHARGING_S: u64 = 3;

/// 空闲巡检间隔（秒）—— **仅在 uevent 不可用时**作为插拔的感知渠道
///
/// 正常情况下"插上充电器"由 `power_supply` uevent 立即通知，
/// 不需要靠轮询发现。因此这个间隔只用于**降级模式**（uevent 订阅失败）。
const PATROL_IDLE_S: u64 = 15;

/// **深度休眠的兜底唤醒间隔**（秒）
///
/// 未充电且无客户端时，worker 进入深度休眠：**不跑任何拍**
/// （不采样、不决策、不落盘），只在有意义的唤醒源上醒来。
///
/// | 唤醒源 | 场景 |
/// |---|---|
/// | `power_supply` uevent | **插上充电器** —— 主渠道，立即响应 |
/// | 控制通道新连接 | 打开 WebUI / 敲命令 |
/// | 信号 | 终止 / 重载配置 |
/// | **本定时器** | 兜底保险丝 |
///
/// # 为什么兜底要有，而且要长
///
/// `power_supply` uevent 是所有 Android 机型都支持的标准机制，
/// 但不能把"插电必被发现"完全押在它身上 —— 万一某机型不上报，
/// 或者订阅被 SELinux 拦掉，用户插上充电器就永远等不到温控。
///
/// 因此留一根保险丝：即使什么事件都没收到，也定期醒来看一眼。
///
/// 300 秒（5 分钟）是刻意的折中：
///
/// | 取值 | 后果 |
/// |---|---|
/// | 很短（如 15 秒） | 省电效果归零，等于没休眠 |
/// | 很长（如 30 分钟） | uevent 万一失效，用户插电后要等半小时才有温控 |
/// | **300 秒** | 正常情况永远用不到它（uevent 先醒）；异常时最坏 5 分钟兜住 |
///
/// ⚠️ 它与 supervisor 的心跳**无关** —— 未充电时 supervisor
/// **不要求** worker 有心跳（见 `supervisor::heartbeat_required`），
/// 所以这里可以放心地长。
const SLEEP_FALLBACK_S: u64 = 300;

/// 休眠相关**事件**的最小间隔（秒）—— 防止客户端轮询把事件流刷爆
///
/// 日志（`main.log`）不受此限，每次迁移都记；只有写进
/// `events.jsonl` 的那条要节流。
///
/// 取值理由见主循环里对休眠事件的处理：
/// WebUI 轮询会让"进入/退出休眠"每 3 秒抖动一次，不节流约 2400 条/小时。
///
/// 300 秒与 `SLEEP_FALLBACK_S` 对齐 —— 一个真正空闲的模块
/// 最多每 5 分钟记一条，既不丢信息也不刷屏。
const SLEEP_EVENT_MIN_GAP_S: u64 = 300;

/// 附加功能到期时间小于这个值时，缩短主循环等待以准点运行它（秒）
const ADDON_SOON_S: u32 = 5;
/// 两拍之间的**最小**间隔（秒）—— 防止 uevent 风暴把主循环变成忙循环
///
/// 实测某些机型上 uevent 持续不断（传感器等）；若无此限流，
/// 同一秒内可跑 10 拍，事件流被刷爆且 CPU 空转。
const MIN_STEP_INTERVAL_S: u64 = 2;

// ⚠️ 这里曾有一个 `CLIENT_MIN_STEP_MS = 150`（客户端唤醒时的最小拍间隔），
//    **已删除**（2026-10-14）。
//
// 它存在的前提是"客户端唤醒必须跑一拍"—— 于是 150ms 用来"限流但不完全限死"。
// 但那个前提本身是缺陷的根源：客户端只是来读一个状态，却连带触发整拍
// （采样 / 决策 / 写节点 / 历史 / 快照 fsync），真机实测每条 `ctl`
// 150~525ms，界面"卡一下"。
//
// 现在客户端由 `light_step` 立即服务、**不跑拍**，因此这个限流没有意义了。
// 拍与拍的间隔由巡检周期（充电 3s / 空闲 15s）决定，见主循环 ②。

// ============================================================
//  单实例锁
// ============================================================

/// 持有 `flock` 的进程锁（`Drop` 时自动释放）
#[derive(Debug)]
pub struct InstanceLock {
    _file: std::fs::File,
    pid_path: std::path::PathBuf,
}

impl InstanceLock {
    /// 抢占单实例锁；已被占用时返回 `Err`
    ///
    /// ⚠️ 用 `flock` 而非"检查 pidfile 里写的 PID 是否存活"——
    /// 后者有 PID 复用与竞态问题；`flock` 由内核保证独占。
    pub fn acquire(lock_path: &std::path::Path, pid_path: &std::path::Path) -> Result<Self> {
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }

        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(lock_path)
            .with_context(|| format!("打开锁文件失败: {}", lock_path.display()))?;

        // LOCK_EX | LOCK_NB：独占且不等待
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            anyhow::bail!(
                "已有 worker 在运行（{} 被占用）—— 单实例保护",
                lock_path.display()
            );
        }

        // 记下自己的 PID（供外部诊断；**不作为**单实例判据）
        let pid = std::process::id();
        if let Ok(mut f) = std::fs::File::create(pid_path) {
            let _ = writeln!(f, "{pid}");
        }

        Ok(Self {
            _file: file,
            pid_path: pid_path.to_path_buf(),
        })
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.pid_path);
        // flock 随 fd 关闭自动释放
    }
}

// ============================================================
//  定时器（timerfd）
// ============================================================

/// `timerfd` 封装（使用 `CLOCK_BOOTTIME`）
#[derive(Debug)]
struct Timer {
    fd: OwnedFd,
}

impl Timer {
    fn new() -> Result<Self> {
        // SAFETY: `timerfd_create` 只返回一个 fd；参数为合法常量组合。
        // 失败时返回负值，下面立即检查。
        let raw = unsafe {
            libc::timerfd_create(libc::CLOCK_BOOTTIME, libc::TFD_NONBLOCK | libc::TFD_CLOEXEC)
        };
        if raw < 0 {
            return Err(std::io::Error::last_os_error()).context("创建 timerfd 失败");
        }
        // SAFETY: raw 是本进程新建且未被其他代码持有的有效 fd
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        Ok(Self { fd })
    }

    /// 设定下一次到期时间（一次性；到期后重新设定）
    fn arm(&self, after: Duration) -> Result<()> {
        let mut spec: libc::itimerspec = unsafe { std::mem::zeroed() };
        let secs = after.as_secs().max(1);
        spec.it_value.tv_sec = secs as libc::time_t;
        spec.it_value.tv_nsec = 0;
        // it_interval 保持 0 = 一次性
        // SAFETY: fd 有效、spec 已初始化、old_value 可为空指针
        let rc =
            unsafe { libc::timerfd_settime(self.fd.as_raw_fd(), 0, &spec, std::ptr::null_mut()) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error()).context("设置 timerfd 失败");
        }
        Ok(())
    }

    /// 排空到期计数（不清会被 poll 一直报可读）
    fn drain(&self) {
        let mut buf = [0u8; 8];
        // SAFETY: 缓冲区大小与 timerfd 的读取要求一致（u64）
        unsafe {
            libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len());
        }
    }
}

// ============================================================
//  uevent 订阅（power_supply 优先唤醒）
// ============================================================

/// 内核 uevent 订阅器（`NETLINK_KOBJECT_UEVENT`）
///
/// ⚠️ 订阅失败**不致命**：契约要求"退回纯定时轮询"，因此所有错误都被降级处理。
#[derive(Debug)]
struct UeventSubscriber {
    fd: OwnedFd,
}

impl UeventSubscriber {
    fn open() -> Result<Self> {
        // SAFETY: socket(2) 返回 fd 或负值；参数为合法常量组合
        let raw = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_DGRAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                libc::NETLINK_KOBJECT_UEVENT,
            )
        };
        if raw < 0 {
            return Err(std::io::Error::last_os_error()).context("创建 netlink socket 失败");
        }
        // SAFETY: raw 是本进程新建的有效 fd
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };

        // SAFETY: sockaddr_nl 是 POD，全零后再填字段是标准做法
        let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        addr.nl_family = libc::AF_NETLINK as u16;
        addr.nl_groups = 1; // 内核广播组
        addr.nl_pid = 0; // 由内核分配

        // SAFETY: 指针与长度匹配 sockaddr_nl 的实际大小
        let rc = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                (&addr as *const libc::sockaddr_nl).cast::<libc::sockaddr>(),
                std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error()).context("绑定 netlink 失败");
        }

        Ok(Self { fd })
    }

    /// 排空所有待处理事件；返回是否看到 `power_supply` 相关事件
    fn drain(&self) -> bool {
        let mut buf = [0u8; 4096];
        let mut saw_power_supply = false;

        loop {
            // SAFETY: buf 有效且长度正确；MSG_DONTWAIT 保证不阻塞
            let n = unsafe {
                libc::recv(
                    self.fd.as_raw_fd(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if n <= 0 {
                break;
            }
            let slice = &buf[..n as usize];
            if slice
                .windows(b"power_supply".len())
                .any(|w| w == b"power_supply")
            {
                saw_power_supply = true;
            }
        }
        saw_power_supply
    }
}

// ============================================================
//  主循环
// ============================================================

/// worker 入口
pub fn run() -> Result<()> {
    crate::paths::ensure_dirs().context("初始化数据目录失败")?;

    // ── 单实例 ──
    let _lock = InstanceLock::acquire(&crate::paths::lockfile(), &crate::paths::pidfile())?;

    // ── 加载配置与档案 ──
    let mut cfg = ConfigStore::load(crate::paths::config_json());
    let profile = crate::profile_io::effective_profile();

    let events = EventLog::new(
        crate::paths::events_jsonl(),
        u64::try_from(cfg.get_i64("log.max_size_kb"))
            .unwrap_or(crate::observability::DEFAULT_MAX_KB),
    );
    let snapshot = SnapshotWriter::new(crate::paths::runtime_json());

    // ── 按设备档案钳位温控参数（契约 §0.1；**必须留痕，不可静默**）──
    //
    // ⚠️ 钳位**只在运行期生效，绝不落盘**。
    //
    // 为什么不能写回：钳位的依据是 `device.json` 里的**检测结果**，
    // 而检测可能不准。真机踩过的坑：在电脑 USB（弱电源）上跑到 9W 时
    // 检测记为 `max_w = 9`，于是把用户设的 43W 目标**永久改写**成 9W ——
    // 即使换回原装 100W 充电器，配置也回不来了。
    //
    // 运行期钳位同样能保证安全（决策用的就是这份内存里的配置），
    // 而检测一旦修正，行为立即恢复正确。
    let caps = crate::profile_io::to_core_caps(&profile.profile.capabilities);
    let clamped = cfg.clamp_to_caps(&caps);
    if !clamped.is_empty() {
        for c in &clamped {
            log::warn!("配置越界已钳位（仅本次运行）：{c}");
        }
        events.emit(
            kind::CONFIG_CLAMPED,
            &format!(
                "{} 项配置因超出设备边界被钳位（仅在本次运行生效，未改动配置文件）",
                clamped.len()
            ),
            serde_json::json!({
                "items": clamped,
                "persisted": false,
                "hint": "若认为档位/功率检测有误，请重新执行 device probe",
            }),
        );
    }

    events.emit(
        kind::LIFECYCLE,
        "worker 启动",
        serde_json::json!({
            "pid": std::process::id(),
            "version": env!("CARGO_PKG_VERSION"),
            "config_ver": cfg.ver,
            "profile_source": profile.source.label(),
        }),
    );

    // ── 运行期探测节点（**不写死机型**） ──
    let nodes = Nodes::detect(Some(&profile.profile.nodes));
    for m in nodes.missing_required() {
        log::warn!("缺少必需节点：{m}");
        events.emit(
            kind::DETECT,
            &format!("缺少必需节点：{m}"),
            serde_json::json!({}),
        );
    }
    log::info!(
        "节点探测完成：控速={} 观测={} SOC上限={}",
        nodes.can_control(),
        nodes.can_observe(),
        nodes.can_limit_soc()
    );

    let gear_table = match GearTable::detect() {
        Ok((t, path)) => {
            log::info!("档位表读取成功（{} 档，{}）", t.len(), path);
            Some(t)
        }
        Err(e) => {
            log::warn!("档位表不可用（将退回档案边界）：{e:#}");
            events.emit(
                kind::DETECT,
                &format!("档位表不可用：{e:#}"),
                serde_json::json!({}),
            );
            None
        }
    };

    // ── 附加功能（**与温控独立**） ──
    let addon_setup = crate::addon_host::setup(&cfg);
    for p in &addon_setup.problems {
        log::warn!("{p}");
        events.emit(kind::ADDON, p, serde_json::json!({}));
    }

    // ── 组装引擎 ──
    let mut engine = Engine::new(EngineInit {
        cfg,
        profile,
        nodes,
        gear_table,
        addons: addon_setup.runner,
        events: events.clone(),
        snapshot,
        history_path: crate::paths::history_json(),
        history_draft_path: crate::paths::history_draft_jsonl(),
        now_s: crate::observability::now_unix(),
    });

    // ── 控制通道 ──
    let mut started_server = false;
    #[cfg(unix)]
    {
        match crate::server::ControlServer::bind(crate::paths::control_sock()) {
            Ok(s) => {
                engine.server = Some(s);
                started_server = true;
            }
            Err(e) => {
                log::error!("控制通道启动失败（将无 ctl 能力）：{e:#}");
                events.emit(
                    kind::DENY,
                    &format!("控制通道启动失败：{e:#}"),
                    serde_json::json!({}),
                );
            }
        }
    }

    // ── 附加功能探测（只读） ──
    {
        // 探测也走看门狗：某些节点的**读取**同样可能挂住内核线程
        let mut io = crate::io_watchdog::WatchedIo::spawn(Box::new(crate::addon_host::SysfsIo));
        for (id, supported, detail) in engine.addons.probe_all(&mut io) {
            log::info!(
                "附加功能 {id}：{} —— {detail}",
                if supported { "支持" } else { "不支持" }
            );
            events.emit(
                kind::ADDON,
                &format!("{id} 探测：{detail}"),
                serde_json::json!({ "id": id, "supported": supported }),
            );
        }
    }

    if !started_server {
        log::warn!("worker 以无控制通道模式运行（仍会正常控制充电）");
    }

    // ── 信号处理 ──
    install_signal_handlers()?;

    // ── 定时器与事件源 ──
    let timer = Timer::new()?;
    let uevent = match UeventSubscriber::open() {
        Ok(s) => {
            log::info!("uevent 订阅成功（power_supply 事件将优先唤醒）");
            events.emit(kind::LIFECYCLE, "uevent 订阅成功", serde_json::json!({}));
            Some(s)
        }
        Err(e) => {
            // 契约要求：订阅失败必须能降级为纯定时轮询
            log::warn!("uevent 订阅失败（退回纯定时轮询）：{e:#}");
            events.emit(
                kind::LIFECYCLE,
                &format!("uevent 订阅失败，已降级为纯定时轮询：{e:#}"),
                serde_json::json!({}),
            );
            None
        }
    };

    // ── 主循环 ──
    //
    // # 节奏控制（**关键**）
    //
    // 有两个信号源会"催促"主循环，但二者都**不能**无限制地驱动一拍：
    //
    // | 源 | 期望行为 |
    // |---|---|
    // | `timerfd` 到期 | 按巡检间隔跑一拍（充电 3s / 空闲 15s） |
    // | `power_supply` uevent | **提前**唤醒，让插拔被尽快感知 |
    //
    // ⚠️ 若"收到 uevent 就跑一拍"，主循环会变成**忙循环** —— 实测本机
    //    uevent 持续不断（传感器等），同一秒内曾跑 10 拍：事件流被刷爆、
    //    CPU 空转。
    //
    // 因此限流方式是：**没到点就不跑拍**（而不是"先睡够再跑"）。
    //
    // 🔴 限流**只作用于控制拍**，绝不作用于"响应客户端"。
    //
    //     这两件事曾被绑在一起：只要在充电，`need_full` 就恒为真，
    //     于是每条 `ctl` 都要 (1) 先等限流、(2) 再等一整拍跑完
    //     （采样 / 决策 / 写节点 / 附加功能 / 历史 / 快照 fsync）
    //     才拿到响应。真机实测每条命令 150~525ms，界面表现为"卡一下"。
    //
    //     现在客户端由 `light_step` **立即**服务，与拍的节奏无关。
    let mut last_step: Option<std::time::Instant> = None;
    // 上一轮 poll 是否因"控制通道有新连接"而唤醒
    let mut client_wake = false;
    // 是否正处于深度休眠（仅用于日志去重，避免刷屏）
    let mut was_sleeping = false;
    // 上次发"进入深度休眠"事件的时刻（**节流用**，见下）
    let mut last_sleep_event: Option<std::time::Instant> = None;
    // 本轮是否"刚从休眠/启动中醒来"
    //
    // 🔴 **初值必须是 `true`** —— 启动时也要先跑一拍确认充电状态，
    //    否则第一轮会因缓存里是"未充电"而直接进入休眠。
    let mut woke_up = true;

    loop {
        if TERM_REQUESTED.load(Ordering::Relaxed) {
            log::info!("收到终止信号，交回官方后退出");
            engine.release_now("worker 退出");
            engine.write_snapshot(crate::observability::now_unix(), true);
            events.emit(kind::LIFECYCLE, "worker 退出", serde_json::json!({}));
            return Ok(());
        }

        if RELOAD_REQUESTED.swap(false, Ordering::Relaxed) {
            log::info!("收到 SIGHUP，重新加载配置与档案");

            let fresh = ConfigStore::load(crate::paths::config_json());
            engine.cfg = fresh;
            crate::addon_host::reload_config(&mut engine.addons, &engine.cfg);
            // 充电历史的开关/保留次数/采样间隔也随配置更新
            engine.history.configure(engine.cfg.history_cfg());

            // ⚠️ **同时重载设备档案**（而不只是配置）
            //
            // 起因是一个真实缺陷：用户跑完 `device probe --save` 后，
            // `ctl device show` 仍显示**旧数据** —— 因为 `engine.profile`
            // 是 worker 启动时加载的副本，检测结果写进了文件，worker 不知道。
            //
            // 影响不只是显示：
            //   · 新检测出的档位上限 / 标称功率不生效
            //   · 配置钳位仍按旧边界算（可能无谓限制用户设定值）
            //
            // 因此 SIGHUP 一并重载档案。检测命令会在保存后自动发 SIGHUP
            // （见 `cli.rs` 的 `device_probe`）。
            let fresh_profile = crate::profile_io::effective_profile();
            let source_before = engine.profile.source.label().to_string();
            let source_after = fresh_profile.source.label().to_string();

            if source_before != source_after {
                log::info!("档案来源变化：{source_before} → {source_after}");
            }
            engine.profile = fresh_profile;

            // 档案换了 ⇒ 边界可能变宽或变窄，重新钳位
            let caps = crate::profile_io::to_core_caps(&engine.profile.profile.capabilities);
            let clamped = engine.cfg.clamp_to_caps(&caps);
            if !clamped.is_empty() {
                for c in &clamped {
                    log::warn!("重载后配置越界已钳位（仅本次运行）：{c}");
                }
                events.emit(
                    kind::CONFIG_CLAMPED,
                    &format!("{} 项配置因新档案边界被钳位", clamped.len()),
                    serde_json::json!({ "items": clamped, "persisted": false }),
                );
            }

            events.emit(
                kind::CONFIG,
                "配置与设备档案已重新加载",
                serde_json::json!({ "profile_source": source_after }),
            );
        }

        // ① 服务客户端 —— **立即，不等限流、不跑控制拍**
        //
        // `light_step` 只做两件事：`poll_server`（内含 `apply_pending`，
        // 因此**写命令也会当场生效**）+ 把事件推给 `watch` 订阅者。
        //
        // ⚠️ `clients_now > 0` 这个分支是给**长连接**留的：`client_wake`
        //    只在"有新连接进 backlog"时置位，而一个已连上的客户端
        //    （如 `watch`）后续发来的请求不会再次触发它。
        let now = crate::observability::now_unix();
        let clients_now = engine
            .server
            .as_ref()
            .map_or(0, crate::server::ControlServer::client_count);

        if client_wake || clients_now > 0 {
            engine.light_step(now);
        }

        // ② 控制拍：由**时间**驱动，与客户端的来去无关
        //
        // # 什么时候跑
        //
        // | # | 条件 | 理由 |
        // |---|---|---|
        // | a | 距上一拍 ≥ 巡检周期 | 正常闭环节奏（充电 3s / 空闲 15s） |
        // | b | **刚从休眠中被唤醒** | 可能是刚插电；休眠期不采样，缓存值不可信 |
        //
        // # 为什么不再看"有没有客户端"
        //
        // 旧实现把三态写死在拍里（充电必全拍 / 有客户端就跑轻拍 /
        // 其余休眠），后果是**充电时每条客户端请求都连带触发一整拍** ——
        // WebUI 每 3 秒轮询一次，于是控制拍被灌到 ~2 倍频率，
        // 且客户端必须排队等这一拍跑完。
        //
        // 现在客户端在 ① 就被服务完了，拍的节奏只由时间决定。
        //
        // ⚠️ 条件 b 的 `MIN_STEP_INTERVAL_S` 下限防的是 uevent 风暴：
        //    风暴只会让 poll 空转，不会刷拍。
        let charging_now = engine.is_charging();
        let interval_s = if charging_now {
            PATROL_CHARGING_S
        } else {
            PATROL_IDLE_S
        };
        let since_step = last_step.map_or(interval_s, |t| t.elapsed().as_secs());
        let due = since_step >= interval_s;
        let woke_from_sleep = woke_up && since_step >= MIN_STEP_INTERVAL_S;

        if due || woke_from_sleep {
            let report = engine.step(now);
            for e in &report.errors {
                log::warn!("{e}");
            }
            last_step = Some(std::time::Instant::now());
        }

        woke_up = false;

        // ③ 决定下一轮等多久
        //
        // 判据与上面一致，但**必须重新取一次**：刚跑完的拍可能
        // 改变了充电状态（比如这次拍发现充电器已插上）。
        let charging = engine.is_charging();
        let clients = engine
            .server
            .as_ref()
            .map_or(0, crate::server::ControlServer::client_count);
        let sleeping = !charging && clients == 0;

        // 附加功能即将到期 → 缩短等待，保证它按时运行
        let addon_due_in = engine.addons.next_due_in_s(now);

        let wait_ms: libc::c_int = if sleeping {
            // ── 深度休眠 ──
            //
            // **不跑任何拍**，只在有意义的唤醒源上醒来：
            //
            // | 唤醒源 | 场景 |
            // |---|---|
            // | `power_supply` uevent | 插上/拔下充电器（主渠道，立即响应） |
            // | 控制通道新连接 | 打开 WebUI / 敲命令 |
            // | 信号 | 终止 / 重载配置 |
            // | 本定时器 | **兜底保险丝**（见下） |
            //
            // 兜底间隔取 [`SLEEP_FALLBACK_S`]（5 分钟）：
            // 正常情况**永远用不到它**（uevent 会先醒）；万一某机型
            // uevent 不上报或订阅失败，最坏 5 分钟也能兜住。
            //
            // ⚠️ 与旧版的**本质区别**：旧版每 30 秒醒来跑一整拍
            //    （因为 supervisor 强制要求心跳，不刷就误报卡死）。
            //    现在 supervisor 只在**充电时**要求心跳，未充电时
            //    改用进程状态判定卡死 ⇒ 这里可以真正地"什么都不做"。
            (SLEEP_FALLBACK_S * 1000) as libc::c_int
        } else {
            let base = if charging {
                PATROL_CHARGING_S
            } else {
                PATROL_IDLE_S
            };
            let wait_s = if addon_due_in <= ADDON_SOON_S {
                1
            } else {
                // ⚠️ 取"**距下一拍还差多久**"，而不是固定再等一个周期。
                //
                // 旧写法每次都等满 `base`，于是**每次客户端唤醒都会把
                // 定时器往后推**：WebUI 每 3 秒轮询一次、充电巡检也是
                // 3 秒 —— 控制拍会被无限推迟，只有客户端唤醒时才会随机跑一拍。
                // 现在按 `last_step` 算剩余时间，控制节奏与客户端完全解耦。
                let elapsed = last_step.map_or(base, |t| t.elapsed().as_secs());
                base.saturating_sub(elapsed).max(1)
            };
            (wait_s * 1000) as libc::c_int
        };

        // 休眠的进入/退出只在**状态迁移时**记一次
        //
        // # ⚠️ 但事件必须**节流**（真机实测踩到）
        //
        // WebUI 每 3 秒轮询一次，而每次 `ctl` 调用都是**新连接**：
        //
        // ```text
        // 轮询 → 连上（退出休眠）→ 应答 → 断开（进入休眠）→ 3 秒后轮询 …
        // ```
        //
        // 若每次都记事件，界面开着期间会产出约 **2400 条/小时**的
        // "进入/退出休眠"噪音，把真正有价值的事件（写入失败、
        // 状态迁移）挤出 `events.jsonl`，日志 1~2 小时就轮转一次。
        //
        // 因此：日志（`main.log`）每次都记，**事件**按
        // [`SLEEP_EVENT_MIN_GAP_S`] 节流。
        if sleeping && !was_sleeping {
            log::info!("未充电且无客户端 —— 进入深度休眠（插电或打开界面即恢复）");
            let due =
                last_sleep_event.is_none_or(|t| t.elapsed().as_secs() >= SLEEP_EVENT_MIN_GAP_S);
            if due {
                events.emit(
                    kind::LIFECYCLE,
                    "进入深度休眠（不跑循环，零 CPU）",
                    serde_json::json!({ "fallback_wake_s": SLEEP_FALLBACK_S }),
                );
                last_sleep_event = Some(std::time::Instant::now());
            }
            was_sleeping = true;
        } else if !sleeping && was_sleeping {
            log::info!("退出深度休眠（充电={charging} 客户端={clients}）");
            // 退出事件同样节流 —— 它与进入是一对，一起节流才对称
            if last_sleep_event.is_some_and(|t| t.elapsed().as_secs() < SLEEP_EVENT_MIN_GAP_S) {
                // 刚记过"进入"就立刻"退出" ⇒ 这是轮询抖动，不记
            } else {
                events.emit(
                    kind::LIFECYCLE,
                    "退出深度休眠",
                    serde_json::json!({ "charging": charging, "clients": clients }),
                );
                last_sleep_event = Some(std::time::Instant::now());
            }
            was_sleeping = false;
        }

        // ④ 等待：定时器 / uevent / 控制通道 / 信号
        //
        // ⚠️ 定时器必须 arm —— 它是心跳的来源（见上方 `idle_block` 的说明）。
        timer.arm(Duration::from_millis(wait_ms as u64))?;

        // 控制通道的两个层次**都要监听**（曾经的缺陷见下）：
        //
        // | fd | 何时可读 |
        // |---|---|
        // | **监听 fd** | 有新连接进入 backlog |
        // | **客户端 fd** | 已连上的客户端发来了数据 |
        //
        // 🔴 只监听前者是不够的：`connect()` 的握手在客户端**还没写请求**时
        //    就完成了，于是 `accept()` 能成功、而紧接着的 `read_in()` 读不到东西
        //    ⇒ 本轮什么也不做，之后**没有任何 fd 会再叫醒我们** ——
        //    请求明明已躺在 socket 缓冲里，worker 却在睡觉。
        //
        //    真机实测每条命令固定多 ~130ms，正是充电时 uevent 的唤醒周期。
        //    把客户端 fd 一起 poll 进来后，数据一到立即唤醒。
        let (sock_fd, client_fds) = engine
            .server
            .as_ref()
            .map_or((Vec::new(), Vec::new()), |s| {
                (vec![s.listener_fd()], s.client_fds())
            });

        let mut fds: Vec<libc::pollfd> = Vec::with_capacity(3 + client_fds.len());
        fds.push(libc::pollfd {
            fd: timer.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        fds.push(libc::pollfd {
            fd: uevent.as_ref().map_or(-1, |u| u.fd.as_raw_fd()),
            events: libc::POLLIN,
            revents: 0,
        });
        fds.push(libc::pollfd {
            fd: sock_fd.first().copied().unwrap_or(-1),
            events: libc::POLLIN,
            revents: 0,
        });
        for fd in &client_fds {
            fds.push(libc::pollfd {
                fd: *fd,
                events: libc::POLLIN,
                revents: 0,
            });
        }

        // SAFETY: fds 是本地 Vec、长度由 `nfds` 给出；timeout 单位为毫秒
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, wait_ms) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::Interrupted {
                log::warn!("poll 失败：{err}");
                std::thread::sleep(Duration::from_millis(500));
            }
        }

        if fds[0].revents & libc::POLLIN != 0 {
            timer.drain();
        }
        if fds[1].revents & libc::POLLIN != 0 {
            if let Some(u) = &uevent {
                // ⚠️ 必须**排空**缓冲：否则 fd 一直可读，poll 立即返回造成忙循环。
                //    排空后进入下一轮循环。
                if u.drain() {
                    log::debug!("收到 power_supply uevent");
                }
            }
        }
        // 两个层次任一可读都算"控制通道有事做"
        client_wake = fds[2].revents & libc::POLLIN != 0;
        if !client_wake {
            // 已连接的客户端发来了数据（见上方关于"只监听监听 fd"的说明）
            client_wake = fds[3..].iter().any(|f| f.revents & libc::POLLIN != 0);
        }
        if client_wake {
            log::debug!("控制通道有事做，尽快进入下一轮处理");
        }

        // poll 返回 ⇒ 被某个唤醒源叫醒了。
        //
        // ⚠️ 若刚才处于休眠，下一轮**必须跑一次全拍**确认状态 ——
        //    唤醒源里最可能就是"插上了充电器"，而缓存里的充电状态
        //    还是休眠前的旧值（休眠期间刻意不采样）。
        if sleeping {
            woke_up = true;
        }
    }
}

/// 供 supervisor 使用的"崩溃恢复"：写一组固定安全值
///
/// ⚠️ supervisor **仅**在 worker 崩溃时调用本函数，
/// 不得参与决策、不得写其他节点（契约 §2.4）。
pub fn write_safe_values(profile_source: ProfileSource) -> Result<()> {
    let profile = crate::profile_io::effective_profile();
    let nodes = Nodes::detect(Some(&profile.profile.nodes));
    let caps = &profile.profile.capabilities;
    let curr_max = if caps.current.max_ua > 0 {
        caps.current.max_ua
    } else {
        700_000
    };
    let _ = profile_source;
    nodes.release_control(curr_max)
}
