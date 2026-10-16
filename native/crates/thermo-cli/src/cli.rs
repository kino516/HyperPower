//! CLI 定义与直接执行的命令
//!
//! # 两类命令
//!
//! | 类别 | 例子 | 执行方式 |
//! |---|---|---|
//! | **本地命令** | `thermo device show` · `thermo probe` | 直接在本进程完成（不依赖 worker） |
//! | **经 worker 的命令** | `thermo ctl status` | 经 UDS 交给 worker（唯一写者） |

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::ipc;

/// 温控充 —— root 常驻充电控制引擎
#[derive(Debug, Parser)]
#[command(
    name = "thermo",
    version,
    about = "ThermoCharge 充电控制引擎",
    long_about = "按电池温度闭环控制充电速度；supervisor 与 worker 由 service.sh 拉起。"
)]
pub struct Cli {
    /// 输出详细日志
    #[arg(short, long, global = true)]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Command,
}

/// 顶层子命令
#[derive(Debug, Subcommand)]
pub enum Command {
    /// 进程监督与崩溃恢复（仅写固定安全值）
    Supervisor,

    /// 采样 / 决策 / 写节点（唯一常规 sysfs 写者）
    Worker,

    /// 经 UDS 向 worker 发送命令
    Ctl {
        /// 子命令名
        command: String,
        /// 子命令参数
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
        /// 以 `key=value` 逐行输出
        #[arg(long)]
        plain: bool,
        /// JSON 缩进输出
        #[arg(long)]
        pretty: bool,
    },

    /// 实时事件流（订阅 worker 的事件推送）
    Watch,

    /// 【诊断】探测节点与档位表（**只读，零副作用**）
    Probe {
        /// 额外解码**官方充电曲线**（设备树直读，**不需要插电**）
        ///
        /// 官方自己也有一套「温度 → 电流上限」的分档策略。读出来可以
        /// 对照自己的设置是否比官方宽松，也便于把阈值设在官方**之前**，
        /// 避免两层策略互相覆盖。
        #[arg(long)]
        official: bool,

        /// 以 JSON 输出（供 WebUI / 脚本解析）
        #[arg(long)]
        json: bool,
    },

    /// 【诊断】安装后自检 / 故障排查（**只读，零副作用**）
    ///
    /// # 与其它诊断命令的分工
    ///
    /// | 命令 | 回答什么问题 |
    /// |---|---|
    /// | `probe` | 节点在哪？档位表长什么样？（**看单个组件**） |
    /// | `measure` | 现在充多快？充电器够不够好？ |
    /// | `device show` | 模块以为本机有哪些能力？ |
    /// | **`verify`** | **整套是否正常工作？哪一环断了？** |
    ///
    /// `verify` 是**端到端**的：逐项检查环境、进程、节点、配置、档案、
    /// 数据目录，并把结果汇总成"通过 / 警告 / 失败"。出问题时先跑它，
    /// 通常能直接定位到是哪一环。
    ///
    /// 借用老项目 `tools/verify.sh` 的定位（刷入后确认环境就绪 /
    /// 出问题时快速定位 / 卸载前确认已交回官方），但重写为 Rust 实现
    /// —— 与 `thermo` 同源，不会因 shell 版本差异而行为不同。
    Verify {
        /// 以 JSON 输出（供 WebUI / 脚本解析）
        #[arg(long)]
        json: bool,
    },

    /// 【诊断】读一次实际充电功率（**只读，不写档案**）
    ///
    /// 与 `device probe` 的区别：
    ///
    /// | 项 | `measure` | `device probe` |
    /// |---|---|---|
    /// | 测什么 | **此刻**的实际功率 | 机型**能力上限**（标称值） |
    /// | 用途 | 验证充电器/线材表现 | 钳位用户配置 |
    /// | 写档案 | ❌ **绝不** | ✅ |
    ///
    /// ⚠️ 结果**只展示**，不参与任何决策 —— 实测值会随电量/充电器变化，
    /// 拿它当能力上限会误钳位（见 docs/11 与 `detect.rs` 的历史说明）。
    Measure {
        /// 采样时长（毫秒），取这段时间内的**峰值**功率
        ///
        /// 默认 3000。快充会波动，单点读数容易偏低，取峰值更有代表性。
        #[arg(long, default_value_t = 3000)]
        duration_ms: u64,

        /// 采样间隔（毫秒）
        #[arg(long, default_value_t = 200)]
        interval_ms: u64,

        /// 以 JSON 输出（供 WebUI 解析）
        #[arg(long)]
        json: bool,
    },

    /// 【检测】设备信息与能力边界检测
    ///
    /// 不带子命令 = `show`（显示生效档案）
    Device {
        /// 子动作
        #[command(subcommand)]
        action: Option<DeviceAction>,
    },

    /// 输出版本信息（JSON）
    Version,
}

/// 设备档案相关的子命令
#[derive(Debug, Subcommand)]
pub enum DeviceAction {
    /// 显示生效档案（合并后的能力边界）
    Show,

    /// 执行检测
    Probe {
        /// 快速检测：**仅只读**，秒级完成
        #[arg(long)]
        quick: bool,
        /// 保存为 `device.json`
        #[arg(long)]
        save: bool,
        /// 允许写探（改变充电参数，**有物理影响**）
        #[arg(long)]
        confirm: bool,
        /// 以 JSON 输出（供 WebUI 解析）
        ///
        /// ⚠️ 与其它命令的 `--json` 语义一致：**只输出一个 JSON 对象**，
        /// 人类可读的进度/结果说明全部改为走 stderr。
        #[arg(long)]
        json: bool,
    },

    /// 导出档案（可分享给维护者）
    Export {
        /// 输出路径（默认 `/sdcard/Download/<机型>_<代号>_<日期>.json`）
        #[arg(long)]
        out: Option<String>,
        /// 面向分享：确保不含隐私字段
        #[arg(long)]
        share: bool,
    },

    /// 导入档案（成为本机档案）
    Import {
        /// 档案文件路径
        file: String,
        /// 确认导入（覆盖现有档案）
        #[arg(long)]
        confirm: bool,
    },

    /// 删除本机档案，回退出厂档案
    Reset {
        /// 确认删除
        #[arg(long)]
        confirm: bool,
    },
}

// ============================================================
//  进程命令
// ============================================================

/// `thermo supervisor`
pub fn run_supervisor() -> Result<()> {
    #[cfg(not(unix))]
    anyhow::bail!("supervisor 仅在 Unix 平台（Android / Linux）可用");

    #[cfg(unix)]
    crate::supervisor::run()
}

/// `thermo worker`
pub fn run_worker() -> Result<()> {
    #[cfg(not(unix))]
    anyhow::bail!("worker 仅在 Unix 平台（Android / Linux）可用");

    #[cfg(unix)]
    crate::worker::run()
}

/// `thermo ctl <命令> [参数…]`
pub fn run_ctl(command: Command) -> Result<()> {
    let Command::Ctl {
        command,
        args,
        plain,
        pretty,
    } = command
    else {
        anyhow::bail!("内部错误：run_ctl 收到非 Ctl 命令");
    };
    ipc::send(&command, &args, plain, pretty)
}

/// `thermo watch` —— 订阅事件流
pub fn run_watch() -> Result<()> {
    ipc::watch()
}

// ============================================================
//  诊断
// ============================================================

// 供电来源快照：已移到 `crate::sysfs::PowerSource`
//
// ⚠️ 原先在这里内联实现，后来 `commands.rs` 的 `status` 也要用
//    （界面要显示"插的是电脑还是原装充电器"）⇒ 移到 `sysfs` 共享，
//    避免两处各写一份、日后改一处漏一处。
//
// `status` 与 `measure` 两条路径都读同一个 `PowerSource::read()`。

/// `thermo measure` —— 读一次**实际**充电功率（只读，不写档案）
///
/// # 与 `device probe` 的区别（重要）
///
/// | 项 | `measure` | `device probe` |
/// |---|---|---|
/// | 测什么 | **此刻**的实际功率 | 机型**能力上限**（标称值） |
/// | 随什么变化 | 电量、充电器、温度 | 不变（出厂声明） |
/// | 用途 | 验证充电器/线材表现 | 钳位用户配置 |
/// | 写档案 | ❌ **绝不** | ✅ |
///
/// # 为什么要采样取峰值而不是读一个瞬时值
///
/// 快充功率**持续波动**（协议协商、温度调节、电池内阻变化）。单点读数
/// 很可能采到波谷，让用户以为"充电器不行"。取一段时间内的**峰值**
/// 更能代表这段时间的实际能力。
///
/// # ⚠️ 结果绝不参与决策
///
/// 实测值随环境变化，拿它当能力上限会误钳位用户配置 ——
/// 这正是本模块历史上踩过的坑（82% 电量测到 9W，把用户设的 43W 改掉了）。
/// 因此本命令**只输出，不落盘**。
pub fn run_measure(duration_ms: u64, interval_ms: u64, json: bool) -> Result<()> {
    // 参数防护：避免用户传 0 导致死循环 / 除零
    let duration_ms = duration_ms.clamp(100, 60_000);
    let interval_ms = interval_ms.clamp(50, 5_000);

    let nodes = crate::sysfs::Nodes::detect(None);

    let read_i64 = |n: &Option<crate::sysfs::Node>| -> Option<i64> {
        n.as_ref().and_then(|n| n.read_i64().ok())
    };

    // ① 先确认在充电 —— 不在充电时测不出任何有意义的值
    let status = nodes
        .battery_status
        .as_ref()
        .and_then(|n| n.read().ok())
        .unwrap_or_default();
    let charging = status == "Charging" || status == "Full";

    let soc = read_i64(&nodes.battery_capacity)
        .and_then(|v| u8::try_from(v.clamp(0, 100)).ok())
        .unwrap_or(0);

    // ⚠️ 弱电源识别：电脑 USB 口只能给几瓦，测出来不代表充电器能力
    let source = crate::sysfs::PowerSource::read();

    let samples = if charging {
        sample_power(&nodes, duration_ms, interval_ms)
    } else {
        Vec::new()
    };

    let peak_w = samples.iter().copied().fold(0.0_f64, f64::max);
    let avg_w = if samples.is_empty() {
        0.0
    } else {
        samples.iter().sum::<f64>() / samples.len() as f64
    };

    // ⚠️ 输出前取整 —— 否则界面会收到 `0.6525578000000001` 这种浮点尾巴
    //    （真机实测）。项目里其余浮点输出（如 `history`）都走同一套取整，
    //    保持同一纪律：**给机器的 JSON 必须是干净数字**。
    let peak_w = crate::observability::round_json(peak_w, 2);
    let avg_w = crate::observability::round_json(avg_w, 2);

    if json {
        let payload = serde_json::json!({
            "charging": charging,
            "status": status,
            "soc": soc,
            "samples": samples.len(),
            "peak_w": if charging { serde_json::json!(peak_w) } else { serde_json::Value::Null },
            "avg_w": if charging { serde_json::json!(avg_w) } else { serde_json::Value::Null },
            "duration_ms": duration_ms,
            "source": {
                "usb_type": source.usb_type,
                "protocol": source.protocol,
                "weak": source.weak,
            },
            // 供界面提示：标称值是多少（对比用）
            "nominal_w": crate::profile_io::effective_profile()
                .profile
                .capabilities
                .power
                .max_w,
        });
        println!("{payload}");
        return Ok(());
    }

    println!("=== 实际充电功率（诊断）===");
    println!("状态      : {status}");
    println!("电量      : {soc}%");
    println!(
        "供电类型  : usb/type={}  协议={}{}",
        source.usb_type.as_deref().unwrap_or("未知"),
        source.protocol.as_deref().unwrap_or("无"),
        if source.weak {
            "  ⚠️ 弱电源"
        } else {
            ""
        }
    );
    println!();

    if !charging {
        println!("未在充电 —— 请插上充电器后重试。");
        println!();
        println!("提示：本命令测的是**此刻**的实际功率，与「机型标称功率」不同。");
        println!("      标称功率请用 `thermo device probe --quick` 查看。");
        return Ok(());
    }

    println!("采样      : {} 次 / {} ms", samples.len(), duration_ms);
    println!("峰值功率  : {peak_w:.1} W");
    println!("平均功率  : {avg_w:.1} W");

    if source.weak {
        println!();
        println!(
            "⚠️ 当前是弱电源（{}）——",
            source.usb_type.as_deref().unwrap_or("?")
        );
        println!("   这个读数反映的是**充电器/线材**的能力，不是手机的。");
        println!("   想验证手机能跑多快，请用原装充电器。");
    }

    Ok(())
}

/// 在给定时间窗内采样功率，返回每次的读数（W）
fn sample_power(nodes: &crate::sysfs::Nodes, duration_ms: u64, interval_ms: u64) -> Vec<f64> {
    let mut out = Vec::new();
    let start = std::time::Instant::now();

    while start.elapsed().as_millis() < u128::from(duration_ms) {
        let curr = nodes
            .battery_current
            .as_ref()
            .and_then(|n| n.read_i64().ok())
            .map(i64::abs);
        let volt = nodes
            .battery_voltage
            .as_ref()
            .and_then(|n| n.read_i64().ok())
            .map(i64::abs);

        if let (Some(c), Some(v)) = (curr, volt) {
            if c > 0 && v > 0 {
                // μA × μV = pW；除以 1e12 得 W
                let w = (c as f64 * v as f64) / 1e12;
                // 合理性过滤：0~500W（充电电流为负，已取绝对值）
                if (0.0..=500.0).contains(&w) {
                    out.push(w);
                }
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(interval_ms));
    }

    out
}

/// 自检项的结果等级
///
/// 三档而不是"通过/失败"两档 —— 因为**警告**这一档是真实存在的：
/// 例如"未插充电器"，功能没坏，只是当前测不出真实功率。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum CheckLevel {
    /// 通过
    Ok,
    /// 警告：能用，但有需要留意的地方
    Warn,
    /// 失败：该功能不可用
    Fail,
}

impl CheckLevel {
    fn mark(self) -> &'static str {
        match self {
            Self::Ok => "✓",
            Self::Warn => "!",
            Self::Fail => "✗",
        }
    }

    /// 供 JSON 输出的稳定标识（**不要用中文** —— 脚本要解析）
    fn id(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

/// 一条自检结果
struct Check {
    level: CheckLevel,
    /// 检查项名（用户可读）
    name: String,
    /// 结论/细节
    detail: String,
}

impl Check {
    fn new(level: CheckLevel, name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            level,
            name: name.into(),
            detail: detail.into(),
        }
    }
    fn ok(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::new(CheckLevel::Ok, name, detail)
    }
    fn warn(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::new(CheckLevel::Warn, name, detail)
    }
    fn fail(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::new(CheckLevel::Fail, name, detail)
    }
}

/// 扫 `/proc` 找出本模块的指定角色进程（**只读**）
///
/// `role` 取 `"worker"` / `"supervisor"`。返回其 pid。
///
/// # 为什么不用 pidfile
///
/// pidfile 会在**安装流程**中被清理，而进程仍在运行 ——
/// 用 pidfile 判断会误报"未运行"（真机踩过，详见 `run_verify` 的注释）。
/// `/proc` 反映的是**进程是否存在**这个事实，不受文件清理影响。
///
/// # 为什么不用 `pgrep`
///
/// `pgrep` 是外部命令，依赖 busybox/toybox 的实现与 PATH。
/// `verify` 是排障工具，**不能在环境本身可疑时依赖外部命令**。
///
/// # 匹配规则
///
/// 读 `/proc/<pid>/cmdline`（NUL 分隔），要求**同时**含 `thermo` 与角色名。
/// 这样能排除：
///
/// | 干扰项 | 为什么不匹配 |
/// |---|---|
/// | 僵尸进程（`[thermo]`） | `cmdline` 为空，直接跳过；且状态为 `Z` 也跳过 |
/// | 普通的 `grep thermo` | `cmdline` 里没有角色名 |
/// | 本命令自己 | 自己是 `thermo verify`，不含 `worker`/`supervisor` |
#[cfg(unix)]
fn find_thermo_process(role: &str) -> Option<i32> {
    let entries = std::fs::read_dir("/proc").ok()?;

    for e in entries.flatten() {
        let name = e.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };

        // 跳过僵尸：状态字段是 `stat` 里最后一个 `)` 之后的第一段
        //
        // ⚠️ 不能用 `split(' ')` 取第 3 段 —— 进程名可能含空格/括号
        //    （如 `(thermo worker)`），那样会把 state 定位错。
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        let state = stat
            .rsplit(')')
            .next()
            .and_then(|rest| rest.split_whitespace().next())
            .unwrap_or("");
        if state == "Z" {
            continue;
        }

        let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        if raw.is_empty() {
            continue;
        }
        let cmd = String::from_utf8_lossy(&raw);
        if cmd.contains("thermo") && cmd.contains(role) {
            return Some(pid);
        }
    }
    None
}

/// `thermo verify` —— 安装后自检 / 故障排查（**只读，零副作用**）
///
/// # 设计原则
///
/// | # | 原则 | 理由 |
/// |---|---|---|
/// | 1 | **只读** | 自检本身不能改变任何状态，否则"排查问题"变成了"制造问题" |
/// | 2 | **端到端** | 逐层检查（环境 → 进程 → 节点 → 配置 → 档案 → 数据），断在哪一环一目了然 |
/// | 3 | **警告 ≠ 失败** | "未插充电器"不是故障；把两者混起来会让用户白白紧张 |
/// | 4 | **退出码可判** | 有 `Fail` 才返回非零，供脚本/界面自动判断 |
pub fn run_verify(json: bool) -> Result<()> {
    let mut checks: Vec<Check> = Vec::new();

    // ─────────── ① 数据目录与模块目录 ───────────
    let data = crate::paths::data_dir();
    if data.is_dir() {
        checks.push(Check::ok("数据目录", data.display().to_string()));
    } else {
        checks.push(Check::fail(
            "数据目录",
            format!("{} 不存在 —— 模块可能未完成安装", data.display()),
        ));
    }

    let module = std::path::PathBuf::from(crate::paths::MODULE_DIR);
    let bin = module.join("bin/thermo");
    if bin.is_file() {
        checks.push(Check::ok("模块目录", module.display().to_string()));
    } else {
        checks.push(Check::fail(
            "模块目录",
            format!("{} 不存在（二进制缺失？）", bin.display()),
        ));
    }

    // ─────────── ② 进程（**扫进程表，不依赖 pidfile**）───────────
    //
    // # 🔴 为什么不能用 pidfile 判断（真机踩过，2026-10-14）
    //
    // 初版读 `worker.pid` 判断，结果在**刚装完模块**时误报：
    //
    // ```text
    // ✗ worker 进程：读不到 /data/adb/thermo_charge/worker.pid —— worker 未运行？
    // ✗ 控制通道：/data/adb/thermo_charge/ipc/control.sock 不存在
    // ```
    //
    // 而**进程其实活得好好的**：
    //
    // | 事实 | 说明 |
    // |---|---|
    // | `ksud module install` 只删临时文件 | 它**不杀进程** —— 旧版本进程仍在跑 |
    // | 但 pidfile 与 socket 被清掉了 | 那是安装流程/`service.sh` 的清理动作 |
    // | ⇒ pidfile 缺失 ≠ 进程不在 | **判据错了** |
    //
    // 后果很严重：用户以为模块挂了，去"重启服务"，结果拉起**第二份**
    // supervisor，它的 worker 抢不到 `worker.lock`（被旧 worker 持有），
    // 当场变成僵尸进程 —— 排查动作本身制造了新故障。
    //
    // ⇒ 判据改为**直接扫 `/proc`**（进程存在即事实，不受文件清理影响）。
    //    pidfile 仍会被读取，但**只作为补充信息**，不作为判据。
    #[cfg(unix)]
    {
        let worker_pid = find_thermo_process("worker");
        let supervisor_pid = find_thermo_process("supervisor");

        match (worker_pid, supervisor_pid) {
            (Some(w), Some(s)) => {
                checks.push(Check::ok(
                    "进程",
                    format!("supervisor pid={s} · worker pid={w}"),
                ));
            }
            (Some(w), None) => {
                // worker 在但没人监督 ⇒ 崩溃后不会自动重启
                checks.push(Check::warn(
                    "进程",
                    format!("worker pid={w} 在运行，但**没有 supervisor** —— 崩溃后不会自动恢复"),
                ));
            }
            (None, Some(s)) => {
                // supervisor 在但 worker 起不来 —— 常见原因是 worker.lock 被占
                checks.push(Check::fail(
                    "进程",
                    format!(
                        "supervisor pid={s} 在运行，但 **worker 没起来** \
                         （多为 worker.lock 被残留进程占用）"
                    ),
                ));
            }
            (None, None) => {
                checks.push(Check::fail(
                    "进程",
                    "没有找到 thermo 进程 —— 模块未运行。可执行 service.sh 启动".to_string(),
                ));
            }
        }

        // 补充信息：pidfile 是否与事实一致（不一致只提示，不算失败）
        let pidfile = crate::paths::pidfile();
        let pf_pid: Option<i32> = std::fs::read_to_string(&pidfile)
            .ok()
            .and_then(|s| s.trim().parse().ok());
        match (pf_pid, worker_pid) {
            (Some(pf), Some(real)) if pf != real => checks.push(Check::warn(
                "pidfile",
                format!("记录 {pf} 但实际 worker 是 {real}（残留文件，不影响运行）"),
            )),
            (None, Some(real)) => checks.push(Check::ok(
                "pidfile",
                format!("缺失（安装流程会清理）但 worker {real} 在运行"),
            )),
            _ => {}
        }
    }
    #[cfg(not(unix))]
    checks.push(Check::warn(
        "进程",
        "本平台不支持（仅 Android / Linux）".to_string(),
    ));

    // ─────────── ③ 控制通道 ───────────
    //
    // ⚠️ 这段只在 Unix 上编译 —— `std::os::unix` 在 Windows 上不存在，
    //    而本项目的主机（Windows）也要能 `cargo check`。
    #[cfg(unix)]
    {
        let sock = crate::paths::control_sock();
        if sock.exists() {
            // 能连上才说明 worker 真的在监听（文件可能是残留）
            match std::os::unix::net::UnixStream::connect(&sock) {
                Ok(_) => checks.push(Check::ok("控制通道", "可连接".to_string())),
                Err(e) => checks.push(Check::fail(
                    "控制通道",
                    format!("socket 存在但连不上：{e}（worker 卡死？）"),
                )),
            }
        } else {
            checks.push(Check::fail(
                "控制通道",
                format!("{} 不存在", sock.display()),
            ));
        }
    }
    #[cfg(not(unix))]
    checks.push(Check::warn(
        "控制通道",
        "本平台不支持（仅 Android / Linux）".to_string(),
    ));

    // ─────────── ④ 节点与能力 ───────────
    let nodes = crate::sysfs::Nodes::detect(None);

    // 控速节点：这是模块的核心能力，缺失就是失败
    if nodes.can_control() {
        checks.push(Check::ok(
            "控速节点",
            format!(
                "{} / {}",
                // `path_str()` 返回 `String`（不是 `&str`）⇒ 用 `map_or_else`
                nodes
                    .ctrl
                    .as_ref()
                    .map_or_else(|| "-".into(), |n| n.path_str()),
                nodes
                    .curr
                    .as_ref()
                    .map_or_else(|| "-".into(), |n| n.path_str()),
            ),
        ));
    } else {
        checks.push(Check::fail(
            "控速节点",
            "档位或电流节点不可用 —— 本机型可能不支持模块控速".to_string(),
        ));
    }

    // 观测节点：缺了只是看不到数据，算警告
    if nodes.can_observe() {
        checks.push(Check::ok("观测节点", "温度/电量/状态齐备".to_string()));
    } else {
        checks.push(Check::warn(
            "观测节点",
            "电池观测节点不全 —— 实时数据可能缺失".to_string(),
        ));
    }

    // 电荷泵温度：可选观测，缺了不算问题（很多机型没有）
    match &nodes.cp_die_temp {
        Some(n) => {
            let v = n
                .read_i64()
                .map(|x| format!("{:.1}℃", x as f64 / 10.0))
                .unwrap_or_else(|_| "读取失败".to_string());
            checks.push(Check::ok("电荷泵温度", format!("{v}（仅观测）")));
        }
        None => checks.push(Check::ok(
            "电荷泵温度",
            "本机未上报（可选观测项，不影响控制）".to_string(),
        )),
    }

    // SOC 上限：可选功能
    if nodes.can_limit_soc() {
        checks.push(Check::ok("充电上限节点", "可用".to_string()));
    } else {
        checks.push(Check::warn(
            "充电上限节点",
            "不可用 —— 「充电上限」功能在本机不可用".to_string(),
        ));
    }

    // ─────────── ⑤ 档位表 ───────────
    match crate::sysfs::GearTable::detect() {
        Ok((t, _p)) => checks.push(Check::ok("档位表", format!("{} 档（设备树直读）", t.len()))),
        Err(e) => checks.push(Check::warn(
            "档位表",
            format!("读取失败，将退回档案边界：{e:#}"),
        )),
    }

    // ─────────── ⑥ 配置 ───────────
    let cfg = crate::config_store::ConfigStore::load(crate::paths::config_json());
    match cfg.to_core() {
        Ok(c) => {
            // 配置能加载 ≠ 能通过设备边界校验。后者要用真实档案，
            // 且需经 `to_core_caps` 转换 —— IPC 层与核心层各有一份
            // `Capabilities`，字段相同但**类型不同**（直接传会编译失败）。
            let profile = crate::profile_io::effective_profile();
            let core_caps = crate::profile_io::to_core_caps(&profile.profile.capabilities);
            match c.validate_against(&core_caps) {
                Ok(()) => checks.push(Check::ok("配置", "合法且在本机边界内".to_string())),
                Err(e) => checks.push(Check::fail(
                    "配置",
                    format!("超出本机能力边界：{e}（控制会被拒绝）"),
                )),
            }
        }
        Err(e) => checks.push(Check::fail("配置", format!("加载失败：{e}"))),
    }

    // ─────────── ⑦ 设备档案（**关键闸门**）───────────
    let profile = crate::profile_io::effective_profile();
    if profile.profile.capabilities.detected {
        let dt = profile.profile.probe_meta.probed_at;
        checks.push(Check::ok(
            "设备档案",
            format!("已检测（来源 {}，时间 {dt}）", profile.source.label()),
        ));
    } else {
        // 这条是**失败**而非警告：未检测 ⇒ 模块零写入，等于没工作
        checks.push(Check::fail(
            "设备档案",
            "未检测 —— 模块当前**不控制充电**。请跑 `device probe`".to_string(),
        ));
    }

    // ─────────── ⑧ 供电与充电状态 ───────────
    let src = crate::sysfs::PowerSource::read();
    let battery_status = nodes
        .battery_status
        .as_ref()
        .and_then(|n| n.read().ok())
        .unwrap_or_else(|| "Unknown".to_string());
    let charging = matches!(battery_status.trim(), "Charging" | "Full");

    if charging {
        if src.weak {
            checks.push(Check::warn(
                "供电来源",
                format!(
                    "{} —— 弱电源（功率读数反映的是充电器能力）",
                    src.label().unwrap_or("未知")
                ),
            ));
        } else {
            checks.push(Check::ok(
                "供电来源",
                format!(
                    "{}{}",
                    src.label().unwrap_or("未知"),
                    src.power_max_w
                        .map(|w| format!("（协商 {w}W）"))
                        .unwrap_or_default()
                ),
            ));
        }
    } else {
        checks.push(Check::ok(
            "供电来源",
            format!("未充电（{battery_status}）—— 功率相关项此刻无意义"),
        ));
    }

    // ─────────── 汇总 ───────────
    let failed: Vec<&Check> = checks
        .iter()
        .filter(|c| c.level == CheckLevel::Fail)
        .collect();
    let warned: Vec<&Check> = checks
        .iter()
        .filter(|c| c.level == CheckLevel::Warn)
        .collect();

    let overall = if !failed.is_empty() {
        CheckLevel::Fail
    } else if !warned.is_empty() {
        CheckLevel::Warn
    } else {
        CheckLevel::Ok
    };

    if json {
        let items: Vec<serde_json::Value> = checks
            .iter()
            .map(|c| {
                serde_json::json!({
                    "level": c.level.id(),
                    "name": c.name,
                    "detail": c.detail,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "overall": overall.id(),
                "ok": checks.iter().filter(|c| c.level == CheckLevel::Ok).count(),
                "warn": warned.len(),
                "fail": failed.len(),
                "checks": items,
            })
        );
    } else {
        println!("=== 自检 ===");
        println!();
        for c in &checks {
            println!("  {} {:<14} {}", c.level.mark(), c.name, c.detail);
        }
        println!();
        match overall {
            CheckLevel::Ok => println!("结论：✓ 一切正常"),
            CheckLevel::Warn => {
                println!("结论：! 可用，但有 {} 项需要留意", warned.len());
            }
            CheckLevel::Fail => {
                println!("结论：✗ 有 {} 项失败 —— 模块可能不工作", failed.len());
                for c in &failed {
                    println!("    ✗ {}：{}", c.name, c.detail);
                }
            }
        }
    }

    // 有失败项 ⇒ 非零退出码（供脚本与界面自动判断）
    if overall == CheckLevel::Fail {
        anyhow::bail!("自检发现 {} 项失败", failed.len());
    }
    Ok(())
}

/// `thermo probe` —— 诊断：节点探测 + 档位表（**只读，零副作用**）
///
/// `official = true` 时额外解码官方充电曲线（设备树直读，不需插电）。
pub fn run_probe(official: bool, json: bool) -> Result<()> {
    // ── JSON 模式：只输出一个对象，人类可读内容走 stderr ──
    //
    // ⚠️ 与项目其它命令的 `--json` 语义一致（见 `docs/05`）：
    //    stdout 必须是**恰好一个 JSON**，否则前端解析会失败。
    if json {
        let bands = crate::sysfs::read_official_bands();
        let items: Vec<serde_json::Value> = bands
            .iter()
            .map(|b| {
                let stages: Vec<serde_json::Value> = b
                    .stages
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "voltage_mv": t.voltage_mv,
                            "curr_max_ma": t.curr_max_ma,
                            "curr_min_ma": t.curr_min_ma,
                        })
                    })
                    .collect();
                serde_json::json!({
                    "lo_c": b.lo_c,
                    "hi_c": b.hi_c,
                    "kind": b.kind,
                    "start_curr_ma": b.start_curr_ma(),
                    "stages": stages,
                })
            })
            .collect();

        let (gear_count, gear_path) = match crate::sysfs::GearTable::detect() {
            Ok((t, p)) => (Some(t.len()), Some(p)),
            Err(_) => (None, None),
        };

        println!(
            "{}",
            serde_json::json!({
                "gear_count": gear_count,
                "gear_path": gear_path,
                "official": items,
            })
        );
        return Ok(());
    }

    println!("=== 档位表（设备树直读）===");
    match crate::sysfs::GearTable::detect() {
        Ok((t, path)) => {
            println!("路径    : {path}");
            println!("档位数  : {}（**因机型而异**）", t.len());
            println!();
            println!("档位   电流上限(mA)   电流上限(μA)");
            for (gear, ua) in t.current_limits() {
                println!("  {gear:2}     {:>8}     {ua:>10}", ua / 1000);
            }
            if let Some(g) = t.max_power_gear() {
                println!();
                println!("最大功率档位: {g}（电流上限最大的档）");
            }
        }
        Err(e) => {
            // 候选路径的逐条探测统一在 `print_candidate_tables()` 输出
            // （这里只说明失败本身，避免重复刷屏）
            println!("读取失败（将退回档案边界）: {e:#}");
            println!("（候选路径的逐条探测见下方「候选路径探测总表」）");
        }
    }

    println!();
    // ⚠️ SOC 上限节点的逐条候选探测**已移到** `print_candidate_tables()`
    //
    // 这里原本有一份 `{mark:12}` 的旧式对齐输出，问题是：
    //
    // | 问题 | 说明 |
    // |---|---|
    // | **重复** | 候选表已在"充电上限控制字"分组里逐条打过 |
    // | 对齐不准 | `{:12}` 按 `char` 数补空格，而中文占 2 列 ⇒ 路径参差 |
    //
    // 现在统一走 `print_candidate_tables()`（宽度运行时计算）。

    println!();
    println!("=== 节点映射自检（运行期探测）===");
    let nodes = crate::sysfs::Nodes::detect(None);
    println!("  控速可用  : {}", yes_no(nodes.can_control()));
    println!("  观测可用  : {}", yes_no(nodes.can_observe()));
    println!("  SOC 上限  : {}", yes_no(nodes.can_limit_soc()));
    let missing = nodes.missing_required();
    if missing.is_empty() {
        println!("  必需节点  : ✓ 齐备");
    } else {
        println!("  缺失节点  : {}", missing.join(" / "));
    }

    println!();
    println!("=== 温控节点实际路径 ===");
    for (name, n) in [
        ("档位", &nodes.ctrl),
        ("电流", &nodes.curr),
        ("第二路电流", &nodes.curr2),
        ("移除标记", &nodes.remove),
        ("SOC 控制", &nodes.soc_ctrl),
        ("SOC 生效值", &nodes.soc_read),
        ("电池温度", &nodes.battery_temp),
        ("电池电流", &nodes.battery_current),
        ("电池电压", &nodes.battery_voltage),
        ("电池状态", &nodes.battery_status),
        ("电池电量", &nodes.battery_capacity),
        ("机身温度", &nodes.board_temp),
        ("电荷泵温度", &nodes.cp_die_temp),
        ("档位表", &nodes.gear_table),
    ] {
        match n {
            Some(node) => println!("  {name:12}: {}", node.path_str()),
            None => println!("  {name:12}: （不可用）"),
        }
    }

    // ⚠️ 这里原有「场景」单独一行（只读节点，标注"仅用于显示"），
    //    已随该字段一起删除（2026-10-17 用户要求：「只能看，没用处」）。

    // ── 候选路径总览（**换机型时最有用的部分**）──
    print_candidate_tables();

    // ── 官方充电曲线（可选）──
    if official {
        print_official_curves();
    }

    Ok(())
}

/// 把路径补空格到指定宽度（**ASCII 补空格是准确的**）
///
/// # 为什么标记在行末、而不是行首
///
/// Rust 的 `{:width}` 按 **`char` 数量**补空格，而中文在终端占 **2 列**。
/// 若写成 `{mark:14} {path}`，标记得按**显示列**对齐才准：
///
/// | 标记 | char 数 | 显示列 |
/// |---|---|---|
/// | `✓ 命中` | 4 | **5** |
/// | `不存在` | 3 | **6** |
///
/// 两者 char 数不同、显示列也不同 ⇒ 后面的路径必然参差
/// （真机输出过这种不整齐的表）。
///
/// ⇒ 把标记挪到**行末**，只对**纯 ASCII 的路径**补空格 ——
/// 此时 `char` 数与显示列数一致，对齐必然准确，且与标记本身无关。
///
/// # 为什么宽度是**运行时算出来的**
///
/// 初版写死 68，结果两条 71~73 字符的设备树路径仍然不齐
/// —— 而这两条恰好是**换机型时最需要看清**的（硬件保护温度）。
/// 硬编码任何常数都会在某个机型上失效。
///
/// ⇒ 由调用方先扫一遍所有候选、取最大长度（见 `print_candidate_tables`）。
///
/// 超长路径**不截断**（宁可列不齐，也不能显示一个被截断的假路径 ——
/// 用户会照着它复制去排查）。
fn pad_path(p: &str, width: usize) -> String {
    let n = p.chars().count();
    if n >= width {
        p.to_string()
    } else {
        format!("{p}{}", " ".repeat(width - n))
    }
}

/// 打印**全部**候选路径的探测结果
///
/// # 为什么需要它（这是给"换机型"用的）
///
/// 本项目的节点路径**全部**走候选表（因为路径因机型而异）。
/// 但这个设计有个盲点：某一项**没探到**时，用户只看到"（不可用）"，
/// 不知道：
///
/// | 问题 | 本函数回答 |
/// |---|---|
/// | 是路径不对？还是节点根本不存在？ | 逐条列出"存在 / 不存在" |
/// | 它试过哪些候选？ | 全部列出（含未尝试的） |
/// | 要不要补一条候选？ | 看到真实的目录结构后再决定 |
///
/// ⇒ 把**每一张候选表**逐条打出来，标出命中项。
///
/// ⚠️ 只读：`exists()` / `is_writable()` 都只读元信息，不碰节点内容。
fn print_candidate_tables() {
    use crate::sysfs;

    println!();
    println!("=== 候选路径探测总表（路径因机型而异，换机型时看这里）===");

    // ── ① 纯路径候选表（逐条判断存在/可写）──
    //
    // 这些常量都是 `&[&str]`，语义统一，可以一起处理。
    let groups: &[(&str, &[&str], bool)] = &[
        // (名称, 候选表, 是否要求可写)
        ("充电上限控制字", sysfs::SMART_CHG_CANDIDATES, true),
        ("档位表（设备树）", sysfs::GEAR_TABLE_CANDIDATES, false),
        ("电荷泵温度", sysfs::CP_DIE_TEMP_CANDIDATES, false),
        ("硬件保护温度", sysfs::HW_MAX_TEMP_CANDIDATES, false),
        // ⚠️ 这里原有「系统场景」（`SCENE_CANDIDATES`），已删除
        //    （2026-10-17 用户要求）。它既不参与决策、又翻译不出模式名，
        //    留在"换机型排查表"里只是占一行。
    ];

    // 先算出**所有**候选路径的最大宽度（而不是写死一个常数）
    //
    // 写死过 68，结果两条 71~73 字符的设备树路径仍不齐 ——
    // 而那两条恰好是换机型时最需要看清的（硬件保护温度）。
    // 硬编码任何宽度都会在某个机型上失效。
    let mut w = sysfs::DECRYPT_TXT.chars().count();
    for (_, cands, _) in groups {
        for c in cands.iter() {
            w = w.max(c.chars().count());
        }
    }
    for d in sysfs::CTRL_DIRS
        .iter()
        .chain(sysfs::BATTERY_DIRS)
        .chain(sysfs::OFFICIAL_PARA_DIRS)
    {
        w = w.max(d.chars().count());
    }
    // 路径与标记之间留 2 列间隔
    let w = w + 2;

    for (name, cands, need_write) in groups {
        println!();
        println!("  [{name}]");
        if cands.is_empty() {
            println!("    （候选表为空 —— 说明代码里根本没有可选路径）");
            continue;
        }
        for c in cands.iter() {
            let n = sysfs::Node::new(*c);
            let mark = if !n.exists() {
                "不存在"
            } else if *need_write && !n.is_writable() {
                "存在但不可写"
            } else {
                "✓ 命中"
            };
            println!("    {}{mark}", pad_path(c, w));
        }
    }

    // ── ② 目录型候选（`xm_power` / `charger` / `power_supply` 三种命名）──
    //
    // 这些是**目录**，实际节点由 `目录/固定名` 拼出来，因此单独处理。
    println!();
    println!("  [控制节点目录] 实际节点 = 目录 + 固定文件名");
    for d in sysfs::CTRL_DIRS {
        let n = sysfs::Node::new(format!("{d}/wired_ctrl_limit"));
        let mark = if n.exists() {
            "✓ 目录有效"
        } else {
            "不存在"
        };
        println!("    {}{mark}", pad_path(d, w));
    }

    println!();
    println!("  [电池节点目录]");
    for d in sysfs::BATTERY_DIRS {
        let n = sysfs::Node::new(format!("{d}/temp"));
        let mark = if n.exists() {
            "✓ 目录有效"
        } else {
            "不存在"
        };
        println!("    {}{mark}", pad_path(d, w));
    }

    // ── ③ 官方热控表（单文件）──
    println!();
    println!("  [官方温控表]（单文件，不是候选表）");
    let f = std::path::Path::new(sysfs::DECRYPT_TXT);
    let mark = if f.is_file() {
        "✓ 命中"
    } else {
        "不存在"
    };
    println!("    {}{mark}", pad_path(sysfs::DECRYPT_TXT, w));

    // ── ④ 官方充电曲线目录 ──
    println!();
    println!("  [官方充电曲线目录]");
    for d in sysfs::OFFICIAL_PARA_DIRS {
        let mark = if std::path::Path::new(d).is_dir() {
            "✓ 目录有效"
        } else {
            "不存在"
        };
        println!("    {}{mark}", pad_path(d, w));
    }

    // ── ⑤ 汇总：哪几项没探到 ──
    //
    // 这一节是给"扫一眼就知道缺什么"用的 —— 上面细节太多。
    println!();
    println!("  ── 汇总：本机**没探到**的功能 ──");
    let mut missing: Vec<String> = Vec::new();
    if sysfs::SMART_CHG_CANDIDATES
        .iter()
        .all(|c| !sysfs::Node::new(*c).is_writable())
    {
        missing.push("充电上限（SOC 上限）".to_string());
    }
    if sysfs::GEAR_TABLE_CANDIDATES
        .iter()
        .all(|c| !sysfs::Node::new(*c).exists())
    {
        missing.push("档位表（档位边界将退回兜底）".to_string());
    }
    if sysfs::CP_DIE_TEMP_CANDIDATES
        .iter()
        .all(|c| !sysfs::Node::new(*c).exists())
    {
        missing.push("电荷泵温度（少一项观测，不影响控制）".to_string());
    }
    if sysfs::HW_MAX_TEMP_CANDIDATES
        .iter()
        .all(|c| !sysfs::Node::new(*c).exists())
    {
        missing.push("硬件保护温度（将改用官方热控表兜底）".to_string());
    }
    if !std::path::Path::new(sysfs::DECRYPT_TXT).is_file() {
        missing.push("官方温控表（官方策略不可见，但控制不受影响）".to_string());
    }

    if missing.is_empty() {
        println!("    ✓ 全部探到");
    } else {
        for m in &missing {
            println!("    · {m}");
        }
    }
}

/// 打印官方**温控策略表**（`decrypt.txt` 直读）
///
/// # 这是整个诊断里最有价值的一张表
///
/// 官方与模块是**两层并联**策略。所以"我设了 42℃ 为什么 34℃ 就变慢了"
/// 这类疑问，答案就在这张表里 —— 官方在**更低的温度**就动手了。
///
/// # ⚠️ 两个通道的作用方式**不同**（容易搞错）
///
/// | 通道 | 官方做什么 | 与模块的关系 |
/// |---|---|---|
/// | **档位** `wired_ctrl_limit` | `MONITOR-BAT` 按温度写档位 | **竞争写**：双方都在写，后写覆盖先写 |
/// | **电流** `wired_chg_curr` | `SIC-BAT` 按温度算 PID 上限 | 驱动把两者**取较小值** |
///
/// ⇒ 不能说"两层取更严的那个" —— 档位是**竞争**，不是取 min。
/// 实测证据：模块写档位 0（放开），随后读回的是官方写的 3/7/11/14。
///
/// `user_tighten` 用于标出用户的阈值落在官方表的哪一档。
fn print_official_thermal(user_tighten: f64) {
    println!();
    println!("=== 官方温控策略（系统自带）===");

    let (monitor, sic) = match crate::sysfs::read_official_rules() {
        Ok(v) => v,
        Err(e) => {
            // ⚠️ 用**具体原因**，不能笼统说"读不到文件"
            //
            // 真机实测：场景切到 11（录像）后系统重写该文件，
            // 节名全变 `VIDEO-` 前缀 —— 此时文件**明明存在**。
            // 笼统提示会把排查引向错误方向（查权限/查机型）。
            println!("  {}", e.message());
            // 把真实节名列出来 —— 这正是排查"节名不同"的场合
            print_section_names();
            return;
        }
    };

    println!();
    println!("  ⚠️ 官方与模块是两层并联，但两个通道的作用方式不同：");
    println!("     · 档位：**双方都在写**，后写的覆盖先写的（竞争，不是取更严）");
    println!("     · 电流：驱动把模块写入与官方 SIC-BAT **取较小值**");

    // ── MONITOR-BAT：官方按电池温度写档位 ──
    println!();
    println!("  [MONITOR-BAT] 官方写入的限速档位");
    println!("    触发温度    解除温度    官方档位");
    for (i, t) in monitor.trig.iter().enumerate() {
        let clr = monitor.clr.get(i).copied().unwrap_or(0);
        // target 单位 = 档位 × 100（`0300` = 档位 3）
        let gear = monitor.target.get(i).map_or(0, |v| v / 100);
        println!(
            "    {:>6.1}℃   {:>6.1}℃    {:>6}",
            *t as f64 / 1000.0,
            clr as f64 / 1000.0,
            gear
        );
    }

    // ── SIC-BAT：官方 PID 连续限流（"18.4A" 的来源）──
    if sic.is_usable() && !sic.max_ma.is_empty() {
        println!();
        println!("  [SIC-BAT] 官方连续限流（PID）");
        println!("    触发温度    官方电流上限    电流下限");
        for (i, t) in sic.trig.iter().enumerate() {
            let max = sic.max_ma.get(i).copied().unwrap_or(0);
            let min = sic.min_ma.get(i).copied().unwrap_or(0);
            println!(
                "    {:>6.1}℃   {:>10.2} A   {:>6.2} A",
                *t as f64 / 1000.0,
                max as f64 / 1000.0,
                min as f64 / 1000.0
            );
        }
        println!();
        println!(
            "  ⚠️ 第一档的 {:.1} A 就是「写 0 也会读回 18400000」的原因 ——",
            sic.max_ma.first().copied().unwrap_or(0) as f64 / 1000.0
        );
        println!("     那是官方策略在写，不是节点不让你写。");
    }

    // ── 与用户阈值对照 ──
    println!();
    println!("  ── 你的收紧阈值（{user_tighten:.1}℃）在官方表的哪一档 ──");
    let mut first_trigger: Option<f64> = None;
    for (i, t) in monitor.trig.iter().enumerate() {
        let tc = *t as f64 / 1000.0;
        let gear = monitor.target.get(i).map_or(0, |v| v / 100);
        let mark = if tc <= user_tighten {
            "← 此档及之前官方已在降档"
        } else {
            "（尚未触发）"
        };
        // 只打印用户阈值上下各 2 档，避免刷屏
        if (tc - user_tighten).abs() < 4.0 {
            println!("    {tc:>6.1}℃ → 档位 {gear:<3} {mark}");
        }
        if first_trigger.is_none() {
            first_trigger = Some(tc);
        }
    }
    if let Some(first) = first_trigger {
        if user_tighten > first {
            println!();
            println!("  🔴 官方从 {first:.1}℃ 就开始写档位，而你的阈值是 {user_tighten:.1}℃。");
            println!("     也就是说 {first:.1}~{user_tighten:.1}℃ 这段**官方先动手**：");
            println!("     它会往档位节点写 3/7/11/12/14，把模块写的\"放开\"覆盖掉。");
            println!();
            println!("     ⚠️ 把收紧温度设到 {first:.1}℃ 以下**并不能阻止官方**");
            println!("        （它会照自己的表写），但能让模块在官方之前就先降流，");
            println!("        从而避免\"官方一脚下去掉得太狠\"。");
        } else {
            println!();
            println!("  ✓ 你的阈值（{user_tighten:.1}℃）早于官方的首次触发（{first:.1}℃）——");
            println!("    这段温区由模块先降，降得更平缓；官方要到 {first:.1}℃ 才介入。");
        }
    }

    // 最后列出文件里**全部**节名（换机型时的诊断依据）
    print_section_names();
}

/// 列出 `decrypt.txt` 里全部节名（**换机型的诊断依据**）
///
/// # 为什么需要它
///
/// 本项目对 `decrypt.txt` 的解析**写死了节名** `MONITOR-BAT` / `SIC-BAT`：
///
/// | 情况 | 后果 |
/// |---|---|
/// | 节名一致（本机 K90PM） | ✅ 正常读到 |
/// | **节名不同**（他机型） | 静默读不到，只是少一项信息 |
///
/// 而**猜节名是危险的** —— 读到语义不同的表比读不到更糟
/// （同类错误刚在硬件温度候选表上修过一次：把"主板温度"当成"电池温度"，
/// 数值合理但语义错，`parse_temp_celsius` 拦不住）。
///
/// ⇒ **不猜**，而是把真实存在的节名全部列出来，
/// 让维护者遇到新机型时一眼看到该用什么名字，再决定是否补候选。
fn print_section_names() {
    let Some(names) = crate::sysfs::read_section_names() else {
        return;
    };
    if names.is_empty() {
        return;
    }

    println!();
    println!(
        "  ── 文件里的全部节名（{} 个，供换机型时核对）──",
        names.len()
    );

    // 标出本模块**实际使用**的两个
    const USED: &[&str] = &["MONITOR-BAT", "SIC-BAT"];
    let mut line = String::from("    ");
    for (i, n) in names.iter().enumerate() {
        let mark = if USED.contains(&n.as_str()) {
            "★"
        } else {
            " "
        };
        let item = format!("{mark}{n}");
        // 每行最多约 76 字符，避免超宽
        if line.chars().count() + item.chars().count() + 2 > 76 {
            println!("{line}");
            line = String::from("    ");
        }
        line.push_str(&item);
        line.push_str("  ");
        // 末项后换行
        if i + 1 == names.len() {
            println!("{line}");
        }
    }

    println!();
    println!("    ★ = 本模块当前使用的节名（MONITOR-BAT 档位 / SIC-BAT 限流）");
    println!("    若本机没有这两个节名，说明官方温控表的位置或命名不同 ——");
    println!("    **不要猜**：从上面找一个语义相符的，确认后再改代码。");
}

/// 打印官方策略（`probe --official` 的全部额外输出）
///
/// # 两块内容，重要性差很多
///
/// | # | 内容 | 来源 | 价值 |
/// |---|---|---|---|
/// | ① | **温控策略表** | `decrypt.txt` | ⭐⭐⭐ 直接决定"官方何时先动手" |
/// | ② | 充电曲线（CC/CV 阶梯） | 设备树 | ⭐ 说明各温度档的充电阶段 |
///
/// 两者都**只读、不参与决策**，且都**不需要插充电器**。
fn print_official_curves() {
    // 读用户当前的收紧设置，用于对照
    let cfg = crate::config_store::ConfigStore::load(crate::paths::config_json());
    let user_tighten = cfg.get_f64("thermal.tighten_temp");
    let user_power = cfg.get_i64("thermal.tighten_power_w");

    // ── ① 官方温控策略表（最重要）──
    print_official_thermal(user_tighten);

    // ── ② 官方充电曲线（CC/CV 阶梯）──
    println!();
    println!("=== 官方充电曲线（设备树直读）===");

    let bands = crate::sysfs::read_official_bands();
    if bands.is_empty() {
        println!("  未找到官方曲线属性（候选目录均无温度档属性）");
        println!("  候选目录:");
        for d in crate::sysfs::OFFICIAL_PARA_DIRS {
            println!("    {d}");
        }
        return;
    }

    for kind in ["normal", "ffc"] {
        let rows: Vec<&crate::sysfs::OfficialBand> =
            bands.iter().filter(|b| b.kind == kind).collect();
        if rows.is_empty() {
            continue;
        }
        println!();
        println!(
            "  [{}]",
            if kind == "normal" {
                "普通充电"
            } else {
                "快充（FFC）"
            }
        );
        println!("    温度档      官方允许的最大电流   阶段数");
        for b in rows {
            // 展示**第一阶段**的 `curr_max` —— 即"该温度档下官方允许充多快"
            let start = b.start_curr_ma().map_or_else(
                || "—".to_string(),
                |ma| format!("{:.2} A", ma as f64 / 1000.0),
            );
            println!(
                "    {:>3}~{:<3}℃  {:>14}   {:>5}",
                b.lo_c,
                b.hi_c,
                start,
                b.stages.len()
            );
        }
    }

    // 明细：各档完整阶梯（电压 / 电流上限 / 电流下限）
    //
    // 上层表只给"允许的最大电流"，这里给出完整曲线 ——
    // 排障时能看出官方是**分几级降**的（CV 阶段：先大电流、后小电流）。
    println!();
    println!("  ── 各档完整阶梯（电压mV / 上限mA / 下限mA）──");
    for b in &bands {
        let stages: Vec<String> = b
            .stages
            .iter()
            .map(|t| format!("{}/{}/{}", t.voltage_mv, t.curr_max_ma, t.curr_min_ma))
            .collect();
        println!(
            "    {} {:>3}~{:<3}℃  {}",
            if b.kind == "normal" { "普" } else { "快" },
            b.lo_c,
            b.hi_c,
            stages.join("  ")
        );
    }

    println!();
    println!("  ── 与你的设置对照 ──");
    println!("    你的收紧温度 : {user_tighten:.1}℃");
    println!("    你的收紧功率 : {user_power} W");
}

fn yes_no(b: bool) -> &'static str {
    if b {
        "✓ 是"
    } else {
        "✗ 否"
    }
}

// ============================================================
//  版本
// ============================================================

/// 版本信息（JSON）
pub fn version_json() -> String {
    format!(
        r#"{{"version":"{}","protocol_version":{},"addon_api_version":{}}}"#,
        env!("CARGO_PKG_VERSION"),
        thermo_ipc::PROTOCOL_VERSION,
        thermo_addons::ADDON_API_VERSION
    )
}

// ============================================================
//  设备档案（本地执行，不经 worker）
// ============================================================

/// `thermo device …`
pub fn run_device(action: Option<DeviceAction>) -> Result<()> {
    match action.unwrap_or(DeviceAction::Show) {
        DeviceAction::Show => device_show(),
        DeviceAction::Probe {
            quick,
            save,
            confirm,
            json,
        } => device_probe(quick, save, confirm, json),
        DeviceAction::Export { out, share } => device_export(out, share),
        DeviceAction::Import { file, confirm } => device_import(&file, confirm),
        DeviceAction::Reset { confirm } => device_reset(confirm),
    }
}

/// 显示生效档案
fn device_show() -> Result<()> {
    let eff = crate::profile_io::effective_profile();
    let p = &eff.profile;
    let caps = &p.capabilities;

    println!("=== 生效档案 ===");
    println!(
        "来源      : {}（{}）",
        eff.source.label(),
        if eff.source.is_exact() {
            "精确匹配"
        } else {
            "未验证"
        }
    );
    if eff.is_unverified() {
        println!("⚠️  未验证机型 —— 控制前需确认（device.generic_ack）");
    }
    println!();

    println!("=== 机型 ===");
    println!("市场名    : {}", p.identity.display());
    println!("设备代号  : {}", or_dash(&p.identity.device));
    println!("机型编号  : {}", or_dash(&p.identity.model));
    println!("平台      : {}", or_dash(&p.identity.platform));
    println!(
        "Android   : {} (API {})",
        or_dash(&p.identity.android_release),
        p.identity.android_api
    );
    println!("页大小    : {} 字节", p.identity.page_size);
    println!();

    println!("=== 电池 ===");
    println!("型号      : {}", or_dash(&p.battery.model_name));
    if let Some(h) = p.battery.health_percent() {
        println!(
            "健康度    : {h:.1}%（{}/{} μAh）",
            p.battery.charge_full_uah, p.battery.charge_full_design_uah
        );
    }
    println!("循环次数  : {}", p.battery.cycle_count);
    println!();

    println!("=== 能力边界（控制依据）===");
    println!(
        "档位      : {} ~ {}（最松 {} / 最紧 {}）",
        caps.gear.min, caps.gear.max, caps.gear.open, caps.gear.safe
    );
    if caps.gear.table.is_empty() {
        println!("档位表    : 未检测（请跑 device probe --save）");
    } else {
        println!("档位表    : {} 档", caps.gear.table.len());
        for r in &caps.gear.table {
            if r.gear % 3 == 0 || r.gear == caps.gear.max {
                println!(
                    "            档位 {:>2} → {:>6.2} A",
                    r.gear,
                    r.current_limit_ua as f64 / 1e6
                );
            }
        }
    }
    println!(
        "电流上限  : {:.2} A（{}）",
        caps.current.max_ua as f64 / 1e6,
        caps.sources.current_max.label()
    );
    match caps.power.max_w {
        Some(w) => println!("功率上限  : {w} W（{}）", caps.sources.power_max.label()),
        None => println!("功率上限  : 未读出（不做功率钳位，请跑 device probe --save）"),
    }
    match caps.power.max_power_gear {
        Some(g) => println!("功率最大档: {g}"),
        None => println!("功率最大档: 未检测"),
    }
    println!(
        "硬件保护  : {}℃（{}）",
        caps.thermal.hardware_max_c,
        caps.sources.hardware_max_temp.label()
    );
    println!("模块硬上限: {}℃", caps.thermal.abs_max_c);
    match caps.soc_limit {
        Some(s) if s.supported => println!("SOC 上限  : 支持（{}~{}%）", s.min, s.max),
        _ => println!("SOC 上限  : 不支持"),
    }

    // 来源汇总：让用户一眼看出哪些数是实测的、哪些是推断的
    println!();
    println!("=== 数据来源 ===");
    println!("  档位上限  : {}", caps.sources.gear_max.label());
    println!("  档位表    : {}", caps.sources.gear_table.label());
    println!("  电流上限  : {}", caps.sources.current_max.label());
    println!("  功率上限  : {}", caps.sources.power_max.label());
    println!("  硬件温度  : {}", caps.sources.hardware_max_temp.label());

    // ⚠️ 只对**非本机**来源告警
    //
    // 设备树算本机数据（档位表与硬件温度本来就只能从设备树读，且已实测验证），
    // 不算"推断/兜底" —— 把它算进去会让用户误以为数据有问题。
    let non_local: Vec<&str> = [
        ("档位上限", caps.sources.gear_max),
        ("档位表", caps.sources.gear_table),
        ("电流上限", caps.sources.current_max),
        ("功率上限", caps.sources.power_max),
        ("硬件温度", caps.sources.hardware_max_temp),
    ]
    .into_iter()
    .filter(|(_, s)| !s.is_local())
    .map(|(name, _)| name)
    .collect();

    if !non_local.is_empty() {
        println!(
            "  ⚠️ {} 来自他人档案或默认值 —— 建议在本机重跑检测确认真实能力",
            non_local.join("、")
        );
    }

    if !p.probe_meta.notes.is_empty() {
        println!();
        println!("=== 探测备注 ===");
        for n in &p.probe_meta.notes {
            println!("  · {n}");
        }
    }
    if !p.probe_meta.failed_items.is_empty() {
        println!();
        println!(
            "⚠️  未成功的检测项: {}",
            p.probe_meta.failed_items.join(", ")
        );
    }
    // `needs_charge_for` 现在应当始终为空（检测已全只读）。
    // 保留这段是为了兼容**旧版档案**（那些版本会把 power.max_w 记进来）。
    if !p.probe_meta.needs_charge_for.is_empty() {
        println!();
        println!(
            "⚡ 旧版档案记录的「需充电项」: {}（新版检测已不需要插电，建议重跑）",
            p.probe_meta.needs_charge_for.join(", ")
        );
    }

    Ok(())
}

/// 通知 worker 重新加载配置与档案（发 SIGHUP）
///
/// # 为什么需要它
///
/// worker 在启动时把 `device.json` 读进内存。用户跑完检测后文件变了，
/// 但 worker 不知道 —— 表现为：
///
/// | 症状 | 原因 |
/// |---|---|
/// | `ctl device show` 显示旧数据 | 它读到的是 worker 内存里的副本 |
/// | 新检测的档位/功率不生效 | 决策用的是旧边界 |
///
/// # 为什么不新加一条 IPC 命令
///
/// | 理由 | 说明 |
/// |---|---|
/// | 复用已有通道 | 信号处理器在 worker 启动时就装好了 |
/// | 不依赖 socket | 检测是本地命令，worker 可能没跑或 socket 不可达 |
/// | 语义一致 | SIGHUP 本就是"外部状态变了，重新读一遍" |
///
/// # 为什么走外部 `kill` 命令而不是 `libc::kill`
///
/// 本 crate 的 `#![deny(unsafe_code)]` **只对 `worker` / `server` 开例外**
/// （它们的 `timerfd` / `poll` / `getsockopt` 在标准库中没有安全封装）。
/// 检测功能不属于例外范围，因此不能直接调 `libc::kill`。
///
/// 调 `kill(1)` 命令同样可靠（本模块运行在 root 且有完整 busybox/toybox 环境），
/// 且不必为一次信号投递打破 unsafe 纪律。
///
/// # 失败时静默
///
/// worker 没在运行是**正常情况**（用户可能只跑检测不常驻）。
/// 因此找不到进程时直接返回，不报错、不打扰用户。
fn notify_worker_reload() {
    #[cfg(unix)]
    {
        // 用 pidfile 里的 PID 精确投递。
        //
        // ⚠️ 不用 `pkill -f "thermo worker"`：它按命令行匹配，别的进程
        //    （甚至编辑器、grep 自身）只要命令行里含这个字符串就会被误伤。
        let pid_path = crate::paths::pidfile();
        let Ok(text) = std::fs::read_to_string(&pid_path) else {
            return;
        };
        let Ok(pid) = text.trim().parse::<u32>() else {
            return;
        };

        // 投递失败（进程不存在 / 无权限）不是错误 —— 静默忽略
        let ok = std::process::Command::new("kill")
            .args(["-HUP", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if ok {
            // ⚠️ 走 **stderr**，不走 stdout
            //
            // 这是"过程性提示"，不是"命令结果"。而 `device probe --json`
            // 的 stdout 必须**只有一行 JSON**（WebUI 直接 `JSON.parse`）。
            //
            // 真机踩过：这行 `println!` 排在 JSON 之前，于是
            //
            // ```text
            // 已通知 worker 重载新档案          ← 污染
            // {"elapsed_ms":61,...}            ← 真正的 JSON
            // ```
            //
            // WebUI 报「响应不是合法 json」。stderr 在终端照常可见，
            // 因此两种输出模式都正确。
            eprintln!("已通知 worker 重载新档案");
        }
    }
}

/// 执行检测
///
/// # `json` 模式（供 WebUI）
///
/// 只往 **stdout** 输出一个 JSON 对象，人类可读的说明全部走 **stderr** ——
/// 这样界面可以安全地把 stdout 当纯数据解析，不必担心混入提示文字。
fn device_probe(quick: bool, save: bool, confirm: bool, json: bool) -> Result<()> {
    let started = std::time::Instant::now();
    let mode = if quick {
        crate::detect::ProbeMode::Quick
    } else {
        crate::detect::ProbeMode::Full
    };

    // 人类可读的说明统一走 stderr（json 模式下 stdout 必须干净）
    let hint = |s: &str| {
        if json {
            eprintln!("{s}");
        } else {
            println!("{s}");
        }
    };

    if !quick && !confirm {
        hint("提示：完整检测含**写探**（会临时改变充电参数）。");
        hint("      确认执行请加 --confirm；或改用 --quick 做零副作用检测。");
        hint("");
    }

    hint(&format!("=== 开始检测（{mode:?} 模式）==="));
    let profile = crate::detect::run(crate::detect::ProbeOptions {
        mode,
        allow_write_probe: confirm,
    })?;

    // 落盘（无论哪种输出模式，行为一致）
    if save {
        crate::profile_io::save_local(&profile)?;

        // ⚠️ 保存后**通知 worker 重载档案**
        //
        // 否则 worker 仍用启动时加载的旧档案：`ctl device show` 显示旧数据、
        // 新检测出的边界不生效。见 `worker.rs` 里 SIGHUP 分支的说明。
        //
        // 用 SIGHUP 而不是新加一条 IPC 命令：
        //   · 复用已有的信号通道（信号处理器早就装好了）
        //   · 本地命令不必依赖 socket 是否可达
        //   · 语义一致 —— 都是"外部状态变了，重新读一遍"
        //
        // 发不出去也不报错：worker 可能没在跑（用户只跑检测），
        // 那不是错误。
        notify_worker_reload();
    }

    if json {
        // 界面要用的全部信息打包成一个对象
        let payload = serde_json::json!({
            "saved": save,
            "elapsed_ms": started.elapsed().as_millis() as u64,
            "mode": if quick { "quick" } else { "full" },
            "profile": profile,
        });
        println!("{payload}");
        return Ok(());
    }

    println!();
    println!("=== 检测结果 ===");
    println!("机型      : {}", profile.identity.display());
    println!(
        "档位数    : {}（因机型而异）",
        profile.capabilities.gear.table.len()
    );
    println!(
        "电流上限  : {:.2} A",
        profile.capabilities.current.max_ua as f64 / 1e6
    );
    match profile.capabilities.power.max_w {
        Some(w) => println!(
            "功率上限  : {w} W（来源：{}）",
            profile.capabilities.sources.power_max.label()
        ),
        None => println!("功率上限  : 未读出（不做功率钳位）"),
    }
    match profile.capabilities.power.max_power_gear {
        Some(g) => println!("功率最大档: {g}"),
        None => println!("功率最大档: 未检测"),
    }
    if !profile.probe_meta.failed_items.is_empty() {
        println!("未成功    : {}", profile.probe_meta.failed_items.join(", "));
    }

    println!();
    if save {
        println!("✓ 已保存到 {}", crate::paths::device_json().display());
    } else {
        println!("（未保存 —— 加 --save 写入 device.json）");
    }

    Ok(())
}

/// 导出档案
fn device_export(out: Option<String>, share: bool) -> Result<()> {
    let eff = crate::profile_io::effective_profile();
    let p = &eff.profile;

    let out_name = out.unwrap_or_else(|| {
        let name = p.export_filename(&crate::profile_io::today_compact());
        format!("{}/{name}", crate::paths::EXPORT_DIR)
    });
    let path = std::path::PathBuf::from(&out_name);

    crate::profile_io::save(&path, p)?;

    println!("✓ 已导出: {}", path.display());
    if share {
        println!();
        println!("该文件可直接分享给维护者，用于把本机型加入出厂档案。");
        println!("内容不含任何设备唯一标识（无 IMEI / 序列号 / 账号）。");
    }
    println!();
    println!("档案来源: {}", eff.source.label());
    println!("机型      : {}", p.identity.display());

    Ok(())
}

/// 导入档案
fn device_import(file: &str, confirm: bool) -> Result<()> {
    let src = std::path::Path::new(file);
    if !src.exists() {
        anyhow::bail!("文件不存在: {file}");
    }

    let incoming = crate::profile_io::load_file(src)?;

    let local_id = crate::profile_io::read_identity();
    if !incoming.identity.matches(&local_id) {
        println!("⚠️  档案机型与本机不匹配");
        println!("   档案: {}", incoming.identity.display());
        println!("   本机: {}", local_id.display());
        if !confirm {
            anyhow::bail!("机型不匹配，拒绝导入（如确认无误请重试并加 --confirm）");
        }
        println!("   已通过 --confirm 强制导入");
    }

    if !confirm {
        println!("导入将**覆盖**现有本机档案。确认请加 --confirm。");
        return Ok(());
    }

    if let Some(bak) = crate::profile_io::backup_local()? {
        println!("已备份原档案: {}", bak.display());
    }

    crate::profile_io::save_local(&incoming)?;
    println!(
        "✓ 已导入为 本机档案（{}）",
        crate::paths::device_json().display()
    );

    Ok(())
}

/// 删除本机档案
fn device_reset(confirm: bool) -> Result<()> {
    if !confirm {
        println!("将删除本机档案并回退出厂档案。确认请加 --confirm。");
        return Ok(());
    }
    if crate::profile_io::remove_local()? {
        println!("✓ 已删除本机档案，现使用出厂档案");
    } else {
        println!("本机档案不存在（已在使用出厂档案）");
    }
    Ok(())
}

/// 空字符串显示为 `-`
fn or_dash(s: &str) -> &str {
    if s.is_empty() {
        "-"
    } else {
        s
    }
}
