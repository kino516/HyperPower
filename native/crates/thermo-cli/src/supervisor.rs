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

use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration;

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

/// 拉起并监督 worker
pub fn run() -> Result<()> {
    crate::paths::ensure_dirs().context("初始化数据目录失败")?;

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

    // 记录自己的 pid（供 uninstall.sh 停止）
    if let Ok(mut f) = std::fs::File::create(crate::paths::supervisor_pidfile()) {
        use std::io::Write;
        let _ = writeln!(f, "{}", std::process::id());
    }

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
