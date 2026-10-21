//! 命令面实现
//!
//! 契约见 `docs/05-契约-接口输出规范.md`（**45 条命令**）。
//!
//! # 分工
//!
//! | 类别 | 在哪执行 | 命令 |
//! |---|---|---|
//! | 查询 / 配置 / 附加功能 | **worker**（经 UDS） | `info` `status` `get` `set` `limit` `addon` `logs` `data` `test` `probe` `device` `preset` `restore` |
//! | 进程控制 | **客户端侧** | `start` `stop` `restart` `heartbeat` |
//!
//! # 写配置的特殊之处
//!
//! `set` / `limit set` 等命令要改配置，但 handler 只拿到只读上下文。
//! 因此它们把改动**记入 [`Ctx::pending`]**（一个 `RefCell` 收集箱），
//! 由 worker 主循环在本拍结束时统一落盘 —— 这样既保持了
//! "配置只由 worker 写"的单写者纪律，也避免了重复写 flash。

use std::cell::RefCell;

use serde_json::{json, Value};
use thermo_ipc::device::EffectiveProfile;
use thermo_ipc::{ErrorCode, Response};

use crate::config_store::{self, ConfigStore};
use crate::observability;
use crate::sysfs::{GearTable, Nodes};

/// 延迟执行的写操作
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pending {
    /// 写一个配置键
    SetKey {
        /// 键名
        key: String,
        /// 原始字符串值
        value: String,
    },
    /// 批量写
    SetBatch(Vec<(String, String)>),
    /// 重载配置（从磁盘重读）
    Reload,
    /// 解除某附加功能的熔断
    AddonRecover(String),
    /// **切换模式**（多配置：保守 / 官方 / 极速 / 自定义）
    ///
    /// 为什么必须是一个独立动作、而不是展开成 `SetBatch`：
    ///
    /// | 步骤 | 展开成 SetBatch 会怎样 |
    /// |---|---|
    /// | ① 存**旧**模式的值 | ❌ 丢失（没人做这件事） |
    /// | ② 装载**新**模式的值 | ✅ |
    /// | ③ 落盘时同步存档 | ❌ 会把**新**值存进**新模式**，旧模式存档永远是初始值 |
    ///
    /// ⇒ 必须由引擎调用 `ConfigStore::switch_mode()`，它按正确顺序
    ///   完成"先存旧、再载新"，随后 `save()` 统一落盘。
    SwitchMode(String),
    /// 清空充电历史（已完成会话；**进行中的会话保留**）
    ///
    /// 为什么保留进行中的：用户点"清空"是清理旧记录，
    /// 不该顺手毁掉他此刻正在观察的这一条。
    HistoryClear,
    /// **清空全部记录**：事件 + 充电历史 + 诊断报告 + 日志文件
    ///
    /// # 为什么需要一个新动作，而不是让 CLI 自己删文件
    ///
    /// | 原因 | 说明 |
    /// |---|---|
    /// | **事件必须由 worker 清** | 清完要**立刻补记一条"已清空"**，否则用户只看到一片空白，无法确认操作发生过。而事件文件由 worker 独占写入 |
    /// | 与充电历史共用一套逻辑 | 复用 [`HistoryStore::clear`]，它已处理"草稿被当前会话占用时不删"的边界 |
    /// | 避免写者竞争 | CLI 直接删文件时，worker 可能正好在写同一批文件 |
    ///
    /// ⚠️ **不碰**：`config.json`（用户设置）、`device.json`（设备档案 ——
    ///    清了会让模块进入"未检测不控制"）、`presets.json`、
    ///    `*.pid` / `*.lock`（单实例保护）、`ipc/`（控制通道）。
    RecordsClear,
}

/// 命令上下文
pub struct Ctx<'a> {
    /// 配置
    pub cfg: &'a ConfigStore,
    /// 生效档案
    pub profile: &'a EffectiveProfile,
    /// 节点
    pub nodes: &'a Nodes,
    /// 档位表
    pub gear_table: Option<&'a GearTable>,
    /// 温控状态
    pub state: &'a thermo_core::State,
    /// 已完成拍数
    pub ticks: u64,
    /// 启动时刻
    pub started_at_s: i64,
    /// 是否已交回官方
    pub released: bool,
    /// 附加功能调度器
    pub addons: &'a thermo_addons::runner::AddonRunner,
    /// 充电历史记录器（**worker 内存中的实时副本**）
    ///
    /// # 为什么必须用它，而不是从文件读
    ///
    /// 进行中会话的草稿**每 10 分钟才落盘一次**（省闪存，见
    /// [`crate::history`] 的模块文档）。若命令面只读文件，用户在界面上
    /// 看到的"正在记录"数据**最多滞后 10 分钟** —— 表现为
    /// 「充电 20 分钟了，界面还显示 2 个采样点」。
    ///
    /// 真机踩到过（2026-10-13）：`history list` 报 `points: 2`，
    /// 而实际上 worker 内存里已有十几个点。
    ///
    /// ⇒ 直接用 worker 内存里的这个实例。文件只在
    ///   「worker 没在跑」时才作为降级数据源（见 `history_cmd`）。
    pub history: &'a crate::history::HistoryStore,
    /// 待执行的写操作（由主循环消费）
    pub pending: RefCell<Vec<Pending>>,
    /// 当前单调秒
    pub now_s: i64,
}

impl Ctx<'_> {
    fn push(&self, p: Pending) {
        self.pending.borrow_mut().push(p);
    }

    fn data(&self, v: Value) -> Response {
        Response::success_with(v)
    }

    fn err(code: ErrorCode, msg: impl Into<String>) -> Response {
        Response::failure(code.as_str(), msg)
    }
}

/// 取参数数组的第 n 个（字符串形式）
fn arg(args: &Value, n: usize) -> Option<String> {
    let a = args.as_array()?;
    let v = a.get(n)?;
    match v {
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// 取布尔标志（`--xxx` 形式）
fn flag(args: &Value, name: &str) -> bool {
    args.as_array().is_some_and(|a| {
        a.iter().any(|v| {
            v.as_str().is_some_and(|s| {
                s == name || s.trim_start_matches("--") == name.trim_start_matches("--")
            })
        })
    })
}

/// 取 `--key=value` 或 `--key value` 形式的值
fn opt_value(args: &Value, key: &str) -> Option<String> {
    let a = args.as_array()?;
    let long = format!("--{key}");
    for (i, v) in a.iter().enumerate() {
        let Some(s) = v.as_str() else { continue };
        if let Some(rest) = s.strip_prefix(&format!("{long}=")) {
            return Some(rest.to_string());
        }
        if s == long {
            if let Some(next) = a.get(i + 1).and_then(Value::as_str) {
                return Some(next.to_string());
            }
        }
    }
    None
}

/// 命令分发
///
/// 返回 `(响应, 是否切换为流式客户端)`
pub fn dispatch(ctx: &Ctx<'_>, cmd: &str, args: &Value, stream: bool) -> (Response, bool) {
    match cmd {
        // ── 状态查询 ──
        "info" => (info(ctx), false),
        "status" => (status(ctx), false),
        "device" => (device(ctx, args), false),
        // ── 配置读写 ──
        "get" => (get(ctx, args), false),
        "set" => (set(ctx, args), false),
        "reload" => {
            ctx.push(Pending::Reload);
            (Response::success(), false)
        }
        // ── 预设（模板：套用一次，覆盖当前值）──
        "preset" => (preset(ctx, args), false),
        // ── 多模式配置（4 套**互相独立**的参数，切走再切回仍在）──
        "mode" => (mode_cmd(ctx, args), false),
        // ── 安全 ──
        "restore" => (restore(ctx, args), false),
        // ── 日志 ──
        "logs" => (logs(args), false),
        // ── 充电历史（逐次充电的过程曲线）──
        "history" => (history_cmd(ctx, args), false),
        // ── 清空全部记录（事件 / 历史 / 报告 / 日志）──
        //
        // ⚠️ 命令名叫 `records` 而不是挂到 `data` 下：
        //    `data clear` 读起来像"清空整个数据目录"（含配置与设备档案），
        //    而那会造成**模块停止控制**（未检测不控制）—— 歧义太危险。
        "records" => (records(ctx, args), false),
        // ── 充电上限（附加功能的便捷别名）──
        "limit" => (limit(ctx, args), false),
        // ── 附加功能通用入口 ──
        "addon" => (addon(ctx, args), false),
        // ── 数据 ──
        "data" => (data(args), false),
        // ── 调试模式（排障用的详细记录）──
        "debug" => (debug_cmd(ctx, args), false),
        // ── 诊断 ──
        "probe" => (probe(ctx), false),
        "test" => (test(ctx, args), false),
        "help" => (help(), false),
        "watch" => (watch(ctx, stream), stream),
        // ── 进程控制（worker 无法自管，交由客户端/管理器）──
        "start" | "stop" | "restart" => (
            Ctx::err(
                ErrorCode::BadCond,
                "进程控制请经 KernelSU/APatch 管理器，或由 supervisor 处理",
            ),
            false,
        ),
        "heartbeat" => (heartbeat(ctx), false),
        other => (
            Ctx::err(ErrorCode::Type, format!("未知命令：{other}")),
            false,
        ),
    }
}

// ============================================================
//  状态查询
// ============================================================

/// `info` —— 模块与能力信息
fn info(ctx: &Ctx<'_>) -> Response {
    let p = &ctx.profile.profile;
    let caps = &p.capabilities;
    let counts = config_store::section_key_counts();

    ctx.data(json!({
        "api_version": thermo_ipc::PROTOCOL_VERSION,
        // 模块 id —— 必须与 module.prop 的 `id` 一致（KernelSU 用其作目录名）
        //
        // 前缀 `a_` 是为了在 KernelSU 模块列表里**排序靠前**
        // （列表按 id 字母序升序）。数据目录仍是 `hyperpower`，见 `paths.rs`。
        "module_id": "a_hyperpower",
        "module_name": "澎湃充控 HyperPower",
        "version": env!("CARGO_PKG_VERSION"),
        "data_dir": crate::paths::DATA_DIR,
        "module_dir": crate::paths::MODULE_DIR,
        "android_api": p.identity.android_api,
        "feature_gear_loop": ctx.nodes.ctrl.is_some(),
        "feature_current_loop": ctx.nodes.curr.is_some(),
        "feature_soc_limit": ctx.nodes.can_limit_soc(),
        "device_profile": if p.identity.device.is_empty() { "generic".to_string() } else { p.identity.device.clone() },
        "device_source": ctx.profile.source.label(),
        "device_unverified": ctx.profile.is_unverified(),
        "gear_max": caps.gear.max,
        "gear_table_rows": caps.gear.table.len(),
        "current_max_ua": caps.current.max_ua,
        "power_max_w": caps.power.max_w,
        "config_keys": config_store::key_count(),
        "config_sections": counts,
        "addon_count": ctx.addons.slots().len(),
        "observe_only": ctx.cfg.get_bool("observe_only"),
        "enabled": ctx.cfg.get_bool("enabled"),
    }))
}

/// `status` —— 实时状态
/// 把 sysfs 的 `0.1℃` 整数读数转成摄氏度
///
/// # 为什么要抽成函数
///
/// 单位换算错了**不会报错**，只会静默显示错误的值：
///
/// | 写法 | `425` 显示为 | 后果 |
/// |---|---|---|
/// | `/ 10.0`（正确） | 42.5℃ | — |
/// | `/ 100.0`（错误） | 4.3℃ | 看不出电荷泵在发热 |
///
/// 抽出来是为了让"单位正确"这件事能被**测试固定**。
fn tenths_to_celsius(raw: i64) -> f64 {
    observability::round_json(raw as f64 / 10.0, 1)
}

/// 取整到两位小数（**JSON 输出前必须**）
///
/// 电流/功率这类由整数换算来的浮点数会带出长尾
/// （如 `10.523999999999999`），直接塞进 JSON 会让界面显示
/// 一串无意义的尾数。项目里统一走 [`observability::round_json`]。
fn round2(v: f64) -> f64 {
    observability::round_json(v, 2)
}

fn status(ctx: &Ctx<'_>) -> Response {
    let sample = read_battery(ctx.nodes);
    let phase = format!("{:?}", ctx.state.phase).to_lowercase();

    let battery = match &sample {
        Some(s) => json!({
            "temp_c": (s.temp_dc as f64) / 10.0,
            "soc": s.soc,
            "power_w": s.power_w,
            "status": s.status,
            // 🔴 **实际**电流（A），与 `control.curr_now`（模块**写入**的值）不同
            //
            // 两者经常对不上 —— 真机实测：
            //
            // | 模块写入 | 实际 | 原因 |
            // |---|---|---|
            // | 8 A | 9.06 A | 系统响应滞后（前一拍的值还在生效） |
            // | 8 A | 6.51 A | 同上，方向相反 |
            //
            // 展示实际值才反映"真正在充多快"；展示写入值只反映"模块的意图"。
            // 界面上两个都要有，用户才能看出系统是否跟随。
            "current_a": round2(s.curr_ua.unsigned_abs() as f64 / 1e6),
            // 电池电压（V）
            //
            // ⚠️ 这个值**一直在读**（`read_battery` 用它算功率），原先只是
            //    算完就丢掉。现在一并输出 —— **零额外 I/O** 就补上了一个
            //    充电三要素（电压/电流/功率）里唯一缺的量。
            //
            // 界面上它有用：充电时电压会从 ~3.9V 一路升到 ~4.5V，
            // 远高于标称 3.87V —— 这是正常的恒流→恒压过程，
            // 用户看到"电压这么高"时能对着这一行确认没出问题。
            "voltage_v": round2(s.volt_uv.unsigned_abs() as f64 / 1e6),
        }),
        None => json!({
            "temp_c": 0.0,
            "soc": null,
            "power_w": 0.0,
            "status": "Unknown",
            "current_a": null,
            "voltage_v": null,
        }),
    };

    // 供电来源（诊断展示：解释"为什么只有 0.7W"）
    //
    // ⚠️ **不参与任何决策** —— 纯粹是为了让用户看懂当前读数。
    let src = crate::sysfs::PowerSource::read();
    let charger = json!({
        // 协议优先、USB 类型兜底；两者都读不到时为 null
        "label": src.label(),
        "protocol": src.protocol,
        "usb_type": src.usb_type,
        // 协商出的功率上限（W）；未协商出快充时为 null
        "power_max_w": src.power_max_w,
        // 弱电源（电脑 USB 口等）—— 界面据此提示读数的局限
        "weak": src.weak,
    });

    // ⚠️ 这里原有「系统场景」的读取与归一化（`nodes.scene` + `describe_scene`），
    //    已删除（2026-10-17 用户要求：「只能看，没用处」）。
    //
    // 删除理由（已查证）：它**从不参与决策**（`engine.rs` / `thermo-core`
    // 里一次都没出现），也翻译不出模式名 —— 映射表
    // `/vendor/etc/thermal-map.conf` 是加密的（真机拉取验证）。
    // 一个永远只能显示裸数字的字段，放在界面上只会制造疑问。

    // ⚠️ `abs_max` 取**检测到的硬件保护温度**，不是常量
    //
    // 初版输出 `thermo_core::config::ABS_MAX_TEMP_C`（本机写死 45.0）。
    // 那个常量现在是 60.0 的物理极限，直接输出会让界面显示错误的值。
    // 真正生效的硬保护阈值就是档案里的 `hardware_max_c`。
    let abs_max = ctx.profile.profile.capabilities.thermal.hardware_max_c;

    // 电荷泵芯片温度（观测用）
    //
    // ⚠️ 与电池温度**分开**输出，不做合并或取最大值 ——
    //    两者物理位置不同、上限也不同，混在一起会让界面无法解释
    //    "为什么电池 34℃ 却显示 42℃"。
    //
    // 读不到时为 `null`（机型无此节点 / 驱动未上报），前端需按可空处理。
    let cp_die = ctx
        .nodes
        .cp_die_temp
        .as_ref()
        .and_then(|n| n.read_i64().ok())
        .map(tenths_to_celsius);

    // 机身温度（℃；`null` = 本机无此节点或读数不合理）
    //
    // ⚠️ 与 `engine::sample` 用**同一个**换算函数 —— 否则状态页显示的
    //    机身温度会和历史曲线里记的对不上（这类不一致极难察觉）。
    //
    // 为什么要显示它：`thermal.sensor = virt` 时，模块**按机身温度收紧**，
    // 而状态页主卡的大字是**电池温度** —— 两者能差好几度。
    // 不显示的话，用户会看到"电池才 38℃ 怎么就降速了"。
    let board_c = ctx
        .nodes
        .board_temp
        .as_ref()
        .and_then(|n| n.read_i64().ok())
        .and_then(crate::sysfs::board_temp_to_c)
        // ⚠️ 必须取整后再进 JSON
        //
        // 首次部署漏了这一步，真机直接输出 `27.2000007629395` ——
        // f32 的十进制展开长尾（见 `observability::round_json` 的文档）。
        // 界面上就是一个难看且无意义的长数字。
        //
        // 项目里 `temp_c` / `cp_die_temp_c` 都走了这一步，这里也不例外。
        .map(|c| crate::observability::round_json(f64::from(c), 1));

    ctx.data(json!({
        "ts": ctx.now_s,
        "state": phase,
        // 当前生效的**模式**（保守 / 官方 / 极速 / 自定义）
        //
        // 用户要求（2026-10-17）：状态页三格要显示"当前控制页面里的充电模式"。
        //
        // 同时给 id 与中文标签：
        //   · `mode` 供程序判断（与 `ctl mode` 的 id 一致）
        //   · `mode_label` 供界面直接显示 —— 让**后端**负责 id→中文的映射，
        //     前端不必再维护一份（两处各一份必然有一天不一致，
        //     而对齐的正是用户在「控制」页看到的那几个词）
        "mode": ctx.cfg.current_mode(),
        "mode_label": crate::config_store::mode_label(ctx.cfg.current_mode()),
        "daemon": "running",
        // 🔴 是否已跑过检测 —— 界面**必须**据此提示，不能只靠 `state`
        //
        // 未检测时决策层零写入（安全闸门），但 `state` 会是 `idle`，
        // 而界面把 `idle` 译作「待机」—— 一个看起来完全正常的词。
        //
        // 用户装完模块看到「待机」会以为在工作，实际什么都没干。
        // 因此单独给出这个字段，让界面能显示明确的「未检测」提示。
        //
        // 正常情况下 supervisor 首次启动会自动检测（见 `supervisor::ensure_detected`），
        // 所以 `false` 意味着自动检测也失败了 —— 更需要提示。
        "detected": ctx.profile.profile.capabilities.detected,
        "battery": battery,
        "charger": charger,
        // ⚠️ 这里原有 `"scene"` 字段，已删除（2026-10-17 用户要求）。
        //    前端 `types.ts` 的 `StatusView.scene` 与状态页那一行也一并去掉了。
        "control": {
            "gear_now": ctx.state.gear,
            "curr_now": ctx.state.curr_ua,
            // 用户设定的功率上限（0 = 不限功率）
            "power_limit_w": ctx.cfg.get_u16("control.target_power_w"),
        },
        "thermal": {
            "peak_c": ctx.state.peak_c,
            "basis": ctx.cfg.get_str("thermal.sensor"),
            "tighten_temp": ctx.cfg.get_f64("thermal.tighten_temp"),
            "abs_max": abs_max,
            // 电荷泵芯片温度（℃；null = 本机无此节点）
            "cp_die_temp_c": cp_die,
            // 机身温度（℃；null = 本机无此节点）
            "board_temp_c": board_c,
        },
        "session": { "duration_s": ctx.now_s.saturating_sub(ctx.started_at_s).max(0) },
        "uptime_s": ctx.now_s.saturating_sub(ctx.started_at_s).max(0),
        "ticks": ctx.ticks,
        "released": ctx.released,
    }))
}

/// 一次电池读数（`ctl status` 用）
///
/// # ⚠️ 为什么是结构体而不是元组
///
/// 原先是 `(i64, i64, String, u8, f64)`，消费方靠 `s.0` / `s.3` 取值。
/// 加进电压与机身温度后就是 7 个字段 —— 而这几项**单位不同、含义相近**：
///
/// | 字段 | 单位 |
/// |---|---|
/// | 温度 | 0.1℃ |
/// | 电流 | μA |
/// | **电压** | **μV** |
/// | 电量 | % |
///
/// 数错一个索引就是**数量级错误**（μV 当 μA 用会差 1000 倍），
/// 而元组索引没有任何提示。⇒ 字段名承担这份记忆。
struct BatteryReading {
    /// 温度（0.1℃）
    temp_dc: i64,
    /// 电流（μA，**原始带符号**：本机实测充电为负、放电为正）
    curr_ua: i64,
    /// 电压（μV）
    volt_uv: i64,
    /// 充电状态原文
    status: String,
    /// 电量（%）
    soc: u8,
    /// 功率（W，由电流与电压推算）
    power_w: f64,
}

/// 读电池
///
/// ⚠️ `curr_ua` 是**原始带符号值**：本机实测**充电为负、放电为正**
/// （如充电 `-10870000`、放电 `253000`）。展示时取绝对值即可
/// —— 方向由 `status` 字段表达，不需要靠符号。
///
/// ⚠️ `volt_uv` 原先**读了却丢掉**（只用来算功率）。现在一并返回：
///    这是"零额外 I/O"就能补上的一个实时读数。
fn read_battery(nodes: &Nodes) -> Option<BatteryReading> {
    let temp = nodes.battery_temp.as_ref()?.read_i64().ok()?;
    let status = nodes.battery_status.as_ref()?.read().ok()?;
    let curr = nodes
        .battery_current
        .as_ref()
        .and_then(|n| n.read_i64().ok())
        .unwrap_or(0);
    let volt = nodes
        .battery_voltage
        .as_ref()
        .and_then(|n| n.read_i64().ok())
        .unwrap_or(0);
    let soc = nodes
        .battery_capacity
        .as_ref()
        .and_then(|n| n.read_i64().ok())
        .and_then(|v| u8::try_from(v.clamp(0, 100)).ok())
        .unwrap_or(0);
    let power = (curr.unsigned_abs() as f64) * (volt.unsigned_abs() as f64) / 1e12;
    Some(BatteryReading {
        temp_dc: temp,
        curr_ua: curr,
        volt_uv: volt,
        status,
        soc,
        power_w: (power * 100.0).round() / 100.0,
    })
}

/// `device` —— 设备档案
fn device(ctx: &Ctx<'_>, args: &Value) -> Response {
    let sub = arg(args, 0).unwrap_or_else(|| "show".to_string());
    match sub.as_str() {
        "show" => {
            let p = &ctx.profile.profile;
            ctx.data(json!({
                "source": ctx.profile.source.label(),
                "verified": p.verified,
                "unverified": ctx.profile.is_unverified(),
                "identity": {
                    "device": p.identity.device,
                    "model": p.identity.model,
                    "platform": p.identity.platform,
                    "marketname": p.identity.marketname,
                    "android_api": p.identity.android_api,
                    "android_release": p.identity.android_release,
                    "page_size": p.identity.page_size,
                },
                "battery": {
                    "model_name": p.battery.model_name,
                    "technology": p.battery.technology,
                    "health_percent": p.battery.health_percent(),
                    "cycle_count": p.battery.cycle_count,
                },
                "capabilities": {
                    "gear_min": p.capabilities.gear.min,
                    "gear_max": p.capabilities.gear.max,
                    "gear_open": p.capabilities.gear.open,
                    "gear_safe": p.capabilities.gear.safe,
                    "gear_table_rows": p.capabilities.gear.table.len(),
                    "current_max_ua": p.capabilities.current.max_ua,
                    "power_max_w": p.capabilities.power.max_w,
                    "power_max_gear": p.capabilities.power.max_power_gear,
                    "protocol": p.capabilities.power.protocol,
                    "hardware_max_c": p.capabilities.thermal.hardware_max_c,
                    "soc_limit_supported": p.capabilities.soc_limit.is_some(),
                    // ⚠️ 本机支持的电量上限区间 —— 界面滑块据此设范围
                    //
                    // 初版界面把 60~100 写死了（本机 K90PM 的值）。
                    // 不同机型的驱动支持范围不同，必须由检测给出。
                    "soc_limit_min": p.capabilities.soc_limit.as_ref().map(|s| s.min),
                    "soc_limit_max": p.capabilities.soc_limit.as_ref().map(|s| s.max),
                    // 值域来源标记（界面显示可信度徽标）
                    "sources": {
                        "gear_max": p.capabilities.sources.gear_max,
                        "gear_table": p.capabilities.sources.gear_table,
                        "current_max": p.capabilities.sources.current_max,
                        "power_max": p.capabilities.sources.power_max,
                        "hardware_max_temp": p.capabilities.sources.hardware_max_temp,
                    },
                },
                "probe_meta": {
                    "method": p.probe_meta.method,
                    "needs_charge_for": p.probe_meta.needs_charge_for,
                    "failed_items": p.probe_meta.failed_items,
                    // 待补测（因插电状态不满足而主动跳过的项）——
                    // 与 `failed_items` 分开，界面用中性语气显示
                    "pending_items": p.probe_meta.pending_items,
                    "notes": p.probe_meta.notes,
                },
            }))
        }
        "probe" => {
            // 检测需读设备树与节点：这里给出**只读**的快速结果
            let (rows, path) = match GearTable::detect() {
                Ok((t, p)) => (t.current_limits(), Some(p)),
                Err(_) => (Vec::new(), None),
            };
            ctx.data(json!({
                "mode": if flag(args, "quick") { "quick" } else { "full" },
                "gear_table_path": path,
                "gear_count": rows.len(),
                "gear_table": rows.iter().map(|(g, ua)| json!({"gear": g, "current_limit_ua": ua})).collect::<Vec<_>>(),
                "max_power_gear": ctx.gear_table.and_then(GearTable::max_power_gear),
                "soc_node": ctx.nodes.soc_ctrl.as_ref().map(|n| n.path_str()),
                "node_missing": ctx.nodes.missing_required(),
                "note": "本命令只读；完整检测（含最大功率实测）需充电中执行 device probe --save",
            }))
        }
        "export" => {
            let p = &ctx.profile.profile;
            let text = match serde_json::to_string_pretty(p) {
                Ok(t) => t,
                Err(e) => return Ctx::err(ErrorCode::Internal, format!("序列化失败：{e}")),
            };
            let name = p.export_filename(&today_compact());
            let out = opt_value(args, "out")
                .unwrap_or_else(|| format!("{}/{}", crate::paths::EXPORT_DIR, name));
            match std::fs::write(&out, text.as_bytes()) {
                Ok(()) => ctx.data(json!({
                    "path": out,
                    "share": flag(args, "share"),
                    "privacy": "不含任何设备唯一标识（无 IMEI / 序列号 / 账号）",
                })),
                Err(e) => Ctx::err(ErrorCode::Internal, format!("写入失败：{e}")),
            }
        }
        "import" => {
            let Some(file) = arg(args, 1) else {
                return Ctx::err(ErrorCode::Type, "用法：device import <文件> --confirm");
            };
            if !flag(args, "confirm") {
                return Ctx::err(ErrorCode::Confirm, "导入将覆盖现有本机档案，请加 --confirm");
            }
            let text = match std::fs::read_to_string(&file) {
                Ok(t) => t,
                Err(e) => return Ctx::err(ErrorCode::Internal, format!("读取失败：{e}")),
            };
            let incoming: thermo_ipc::device::DeviceProfile = match serde_json::from_str(&text) {
                Ok(p) => p,
                Err(e) => return Ctx::err(ErrorCode::Type, format!("档案格式非法：{e}")),
            };
            if let Err(e) = incoming.validate() {
                return Ctx::err(ErrorCode::Range, format!("档案校验失败：{e}"));
            }
            if let Ok(bak) = crate::profile_io::backup_local() {
                log::info!("导入前已备份：{bak:?}");
            }
            match crate::profile_io::save_local(&incoming) {
                Ok(()) => ctx.data(json!({"imported": file, "target": crate::paths::device_json().display().to_string()})),
                Err(e) => Ctx::err(ErrorCode::Internal, format!("保存失败：{e:#}")),
            }
        }
        "reset" => {
            if !flag(args, "confirm") {
                return Ctx::err(ErrorCode::Confirm, "将删除本机档案，请加 --confirm");
            }
            match crate::profile_io::remove_local() {
                Ok(true) => ctx.data(json!({"removed": true})),
                Ok(false) => ctx.data(json!({"removed": false, "note": "本机档案本就不存在"})),
                Err(e) => Ctx::err(ErrorCode::Internal, format!("删除失败：{e:#}")),
            }
        }
        other => Ctx::err(ErrorCode::Type, format!("未知的 device 子命令：{other}")),
    }
}

/// `get` —— 读配置
fn get(ctx: &Ctx<'_>, args: &Value) -> Response {
    if flag(args, "all") {
        return ctx.data(ctx.cfg.raw().clone());
    }
    if let Some(section) = opt_value(args, "section") {
        // 先按契约声明的分区名校验（比"查不到就说未知键"更精确、更易排错）
        if !config_store::SECTIONS.contains(&section.as_str()) {
            return Ctx::err(
                ErrorCode::UnknownKey,
                format!(
                    "未知分区：{section}（可用：{}）",
                    config_store::SECTIONS.join(" / ")
                ),
            );
        }
        return match ctx.cfg.section(&section) {
            Some(v) => ctx.data(v.clone()),
            None => Ctx::err(ErrorCode::UnknownKey, format!("分区缺失：{section}")),
        };
    }
    let Some(key) = arg(args, 0) else {
        return Ctx::err(
            ErrorCode::Type,
            "用法：get <键> | get --all | get --section <分区>",
        );
    };
    match ctx.cfg.get(&key) {
        Some(v) => ctx.data(json!({ key: v })),
        None => {
            if config_store::lookup(&key).is_some() {
                // 键合法但暂无值（理论上不会发生，因为加载时会补默认）
                ctx.data(json!({ key: Value::Null }))
            } else {
                Ctx::err(ErrorCode::UnknownKey, format!("未知配置键：{key}"))
            }
        }
    }
}

/// `set` —— 写配置
fn set(ctx: &Ctx<'_>, args: &Value) -> Response {
    if flag(args, "batch") {
        // set --batch k1=v1 k2=v2
        let mut pairs = Vec::new();
        if let Some(a) = args.as_array() {
            for v in a {
                let Some(s) = v.as_str() else { continue };
                if s.starts_with("--") {
                    continue;
                }
                if let Some((k, val)) = s.split_once('=') {
                    pairs.push((k.to_string(), val.to_string()));
                }
            }
        }
        if pairs.is_empty() {
            return Ctx::err(ErrorCode::Type, "用法：set --batch k1=v1 k2=v2 …");
        }
        // 整批一次性做两层校验（这正是"同时改一组相关键"的正当途径）
        if let Err(e) = ctx.cfg.dry_run(&pairs) {
            return Ctx::err(e.code, e.msg);
        }
        ctx.push(Pending::SetBatch(pairs.clone()));
        return ctx.data(json!({"batch": pairs.len(), "applied": "本拍结束时统一落盘"}));
    }

    let (Some(key), Some(value)) = (arg(args, 0), arg(args, 1)) else {
        return Ctx::err(ErrorCode::Type, "用法：set <键> <值>");
    };

    // ⚠️ 用 ConfigStore 的 dry_run 做**两层**校验（单键范围 + 跨键关系）：
    //    只做单键检查会让用户先拿到 ok:true、随后配置被静默拒绝。
    if let Err(e) = ctx.cfg.dry_run(&[(key.clone(), value.clone())]) {
        return Ctx::err(e.code, e.msg);
    }

    ctx.push(Pending::SetKey {
        key: key.clone(),
        value: value.clone(),
    });
    ctx.data(json!({"key": key, "value": value}))
}

/// `mode` —— 多模式配置（保守 / 官方 / 极速 / 自定义）
///
/// # 用户需求（2026-10-13）
///
/// > 「当前只有 1 种配置，增加多配置选项，设置 4 种预设：
/// >   保守 官方 极速 自定义。4 种模式全部可以自定义相关温度」
///
/// # 与 `preset` 的区别（**关键**）
///
/// | | `preset`（旧） | `mode`（本命令） |
/// |---|---|---|
/// | 语义 | 套用**模板**，覆盖当前值 | 切到**另一套独立配置** |
/// | 各自留存 | ❌ 切走即丢 | ✅ 每套独立保存 |
/// | 反复切换 | ❌ 值被来回覆盖 | ✅ 互不影响 |
///
/// 例：用户切到「极速」→ 改了几个值 → 切回「保守」，
/// 再切回「极速」时**之前改的值还在**（`preset` 做不到这点）。
///
/// # 官方模式的语义
///
/// 「官方」= **模块完全不插手**，由系统全权管理。此时决策层会写一次
/// 释放值（放开档位与电流）然后不再干预 —— 见 `thermo_core::decision`。
fn mode_cmd(ctx: &Ctx<'_>, args: &Value) -> Response {
    let sub = arg(args, 0).unwrap_or_else(|| "list".to_string());
    match sub.as_str() {
        // 列出全部模式：含各自参数与"哪个是当前生效"
        "list" => ctx.data(ctx.cfg.list_modes()),

        // 查看某模式的参数（省略 id 则看当前的）
        "show" => {
            let id = arg(args, 1).unwrap_or_else(|| ctx.cfg.current_mode().to_string());
            if !config_store::is_valid_mode(&id) {
                return Ctx::err(
                    ErrorCode::Range,
                    format!(
                        "未知模式：{id}（可选：{}）",
                        config_store::mode_ids().join(" / ")
                    ),
                );
            }
            let is_current = ctx.cfg.current_mode() == id;
            match ctx.cfg.mode_snapshot(&id) {
                Some(v) => ctx.data(json!({"id": id, "current": is_current, "values": v})),
                None => ctx.data(json!({
                    "id": id,
                    "current": is_current,
                    "values": Value::Null,
                    "note": "该模式尚未自定义过，首次切入时使用出厂默认",
                })),
            }
        }

        // 切换模式
        "switch" | "set" => {
            let Some(id) = arg(args, 1) else {
                return Ctx::err(
                    ErrorCode::Type,
                    format!("用法：mode switch <{}>", config_store::mode_ids().join("|")),
                );
            };
            if !config_store::is_valid_mode(&id) {
                return Ctx::err(
                    ErrorCode::Range,
                    format!(
                        "未知模式：{id}（可选：{}）",
                        config_store::mode_ids().join(" / ")
                    ),
                );
            }
            let from = ctx.cfg.current_mode().to_string();
            if from == id {
                return ctx.data(json!({
                    "mode": id,
                    "changed": false,
                    "note": "已是当前模式",
                }));
            }
            ctx.push(Pending::SwitchMode(id.clone()));
            ctx.data(json!({ "from": from, "mode": id, "changed": true }))
        }

        other => Ctx::err(ErrorCode::Type, format!("未知的 mode 子命令：{other}")),
    }
}

/// `preset` —— 预设
fn preset(ctx: &Ctx<'_>, args: &Value) -> Response {
    let presets = match crate::paths::presets_json().exists() {
        true => std::fs::read_to_string(crate::paths::presets_json())
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v.get("presets").cloned())
            .unwrap_or(Value::Null),
        false => Value::Null,
    };

    let sub = arg(args, 0).unwrap_or_else(|| "list".to_string());
    match sub.as_str() {
        "list" => ctx.data(json!({ "presets": presets })),
        "show" => {
            let Some(id) = arg(args, 1) else {
                return Ctx::err(ErrorCode::Type, "用法：preset show <id>");
            };
            let found = presets
                .as_array()
                .and_then(|a| {
                    a.iter()
                        .find(|p| p.get("id").and_then(Value::as_str) == Some(&id))
                })
                .cloned();
            match found {
                Some(p) => ctx.data(p),
                None => Ctx::err(ErrorCode::Range, format!("未知预设：{id}")),
            }
        }
        "apply" => {
            let Some(id) = arg(args, 1) else {
                return Ctx::err(ErrorCode::Type, "用法：preset apply <id>");
            };
            let Some(values) = presets
                .as_array()
                .and_then(|a| {
                    a.iter()
                        .find(|p| p.get("id").and_then(Value::as_str) == Some(&id))
                })
                .and_then(|p| p.get("values"))
                .and_then(Value::as_object)
                .cloned()
            else {
                return Ctx::err(ErrorCode::Range, format!("未知预设：{id}"));
            };
            let pairs: Vec<(String, String)> = values
                .iter()
                .map(|(k, v)| {
                    let s = v.as_str().map_or_else(|| v.to_string(), str::to_string);
                    (k.clone(), s)
                })
                .collect();
            // 整批一次性做两层校验（预设本就是"一次改一整套"）
            if let Err(e) = ctx.cfg.dry_run(&pairs) {
                return Ctx::err(e.code, e.msg);
            }
            ctx.push(Pending::SetBatch(pairs.clone()));
            ctx.data(json!({"applied": id, "keys": pairs.len()}))
        }
        "refresh-official" => ctx.data(json!({
            "note": "官方策略探测需充电中执行，当前版本未实现自动读取",
        })),
        other => Ctx::err(ErrorCode::Type, format!("未知的 preset 子命令：{other}")),
    }
}

/// `restore` —— 交回官方
fn restore(ctx: &Ctx<'_>, args: &Value) -> Response {
    let hard = flag(args, "hard");
    let caps = &ctx.profile.profile.capabilities;

    if hard {
        let gear_safe = caps.gear.safe;
        let curr_safe = caps.current.safe_ua;
        match ctx.nodes.apply_hard_protect(gear_safe, curr_safe) {
            Ok(()) => ctx.data(json!({
                "mode": "hard",
                "gear": gear_safe,
                "curr_ua": curr_safe,
                "note": "已收紧到最慢档",
            })),
            Err(e) => Ctx::err(ErrorCode::BadCond, format!("硬保护失败：{e:#}")),
        }
    } else {
        let curr_max = if caps.current.max_ua > 0 {
            caps.current.max_ua
        } else {
            700_000
        };
        match ctx.nodes.release_control(caps.gear.open, curr_max) {
            Ok(()) => ctx.data(json!({
                "mode": "release",
                "curr_ua": curr_max,
                "note": "已交回官方控制",
            })),
            Err(e) => Ctx::err(ErrorCode::BadCond, format!("交回失败：{e:#}")),
        }
    }
}

/// `history` —— 充电历史（逐次充电的过程曲线）
///
/// # 两个数据源（**必须都用对**）
///
/// | 数据 | 来源 | 理由 |
/// |---|---|---|
/// | **进行中会话** | `ctx.history`（worker 内存） | 草稿每 10 分钟才落盘，读文件会滞后最多 10 分钟 |
/// | 已归档会话 | `history.json` 文件 | 它们是落盘后的事实，且可能由**别的进程**（命令面独立调用）读取 |
///
/// ⚠️ 命令既可能经 worker（UDS）执行，也可能是命令行直接起进程 ——
/// 后者拿不到 worker 内存。此时退化为「只读文件」，进行中会话会滞后，
/// 但**已归档数据始终准确**。
///
/// # 它是观测数据，不是能力数据
///
/// 见 [`crate::history`] 模块文档：这些值**绝不参与决策**，
/// 与 `device.json` 的能力边界毫无关系。
fn history_cmd(ctx: &Ctx<'_>, args: &Value) -> Response {
    let sub = arg(args, 0).unwrap_or_else(|| "list".to_string());
    match sub.as_str() {
        // 会话列表（**不含曲线点**，控件用好几百个点太重）
        "list" => {
            let sessions: Vec<Value> = ctx
                .history
                .load_sessions()
                .iter()
                .map(|s| s.to_list_item(ctx.now_s))
                .collect();
            ctx.data(json!({
                "sessions": sessions,
                "stats": ctx.history.stats(),
                // ⭐ 用 worker 内存里的实时会话（不是滞后的草稿文件）
                "current": ctx.history.current_session(ctx.now_s),
            }))
        }

        // 单次会话的完整曲线
        "show" => {
            let id = arg(args, 1);
            let found: Option<Value> = match id.as_deref() {
                // 未指定 id ⇒ **优先给"正在记录的这一条"**（用户最关心），
                // 没有的话再给最近一次已归档的
                None => ctx
                    .history
                    .current_session_full()
                    .map(|s| s.to_json_clean(ctx.now_s))
                    .or_else(|| {
                        ctx.history
                            .load_sessions()
                            .into_iter()
                            .next()
                            .map(|s| s.to_json_clean(ctx.now_s))
                    }),
                // 指定 id ⇒ 先查归档，再查"正在记录的"（它还没归档）
                Some(i) => ctx
                    .history
                    .load_session(i)
                    .map(|s| s.to_json_clean(ctx.now_s))
                    .or_else(|| {
                        ctx.history
                            .current_session_full()
                            .filter(|s| s.id == i)
                            .map(|s| s.to_json_clean(ctx.now_s))
                    }),
            };
            match found {
                Some(v) => ctx.data(v),
                None => Ctx::err(
                    ErrorCode::Range,
                    match id {
                        Some(i) => format!("找不到该次充电记录：{i}"),
                        None => "还没有任何充电记录".to_string(),
                    },
                ),
            }
        }

        // 汇总
        "stats" => ctx.data(ctx.history.stats()),

        // 清空（走 worker —— 单一写者纪律）
        "clear" => {
            ctx.push(Pending::HistoryClear);
            ctx.data(json!({
                "cleared": true,
                "note": "已清空历史记录；正在充电的这一次会继续记录",
            }))
        }

        other => Ctx::err(ErrorCode::Type, format!("未知的 history 子命令：{other}")),
    }
}

/// `debug` —— 调试模式（记录更详细的排障数据）
///
/// 用户要求（2026-10-17）：「在反馈问题中增加一个调试模式按钮，
/// 如果开启调试模式会记录更详细的数据，方便你抓包分析」。
///
/// # 子命令
///
/// | 命令 | 作用 |
/// |---|---|
/// | `debug on` | 打开（写配置键，worker 下一拍生效） |
/// | `debug off` | 关闭 |
/// | `debug status`（默认） | 看当前状态与已记录的大小 |
///
/// ⚠️ **只改配置、不直接开写入器**：
///    写入器是进程内的静态状态，CLI 开了 worker 也看不见。
///    worker 每拍读配置自行决定（见 `engine::sync_debug_mode`），
///    这样保持"worker 是唯一写者"的架构纪律。
fn debug_cmd(ctx: &Ctx<'_>, args: &Value) -> Response {
    let sub = arg(args, 0).unwrap_or_else(|| "status".to_string());
    match sub.as_str() {
        "on" => {
            ctx.push(Pending::SetKey {
                key: "debug.enabled".to_string(),
                value: "true".to_string(),
            });
            ctx.data(json!({
                "enabled": true,
                "note": "调试模式已开启，下一拍开始记录。\
                         它会记录每拍决策链、节点原始读写与运行日志 —— \
                         复现完问题后请记得关掉。",
            }))
        }
        "off" => {
            ctx.push(Pending::SetKey {
                key: "debug.enabled".to_string(),
                value: "false".to_string(),
            });
            ctx.data(json!({ "enabled": false, "note": "调试模式已关闭" }))
        }
        "status" => {
            let enabled = ctx.cfg.get_bool("debug.enabled");
            let max_mb = ctx.cfg.get_i64("debug.max_mb");
            let path = crate::paths::debug_jsonl();
            // 大小从文件本身的 metadata 取（快），**不数行数** ——
            // 数一遍 20 MB 要读整个文件，而界面每 3 秒就会查一次状态。
            let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            ctx.data(json!({
                "enabled": enabled,
                "path": path.display().to_string(),
                "bytes": bytes,
                "max_bytes": max_mb.max(0) as u64 * 1024 * 1024,
                "max_mb": max_mb,
                "note": if enabled {
                    "正在记录。复现完问题后记得关掉。"
                } else {
                    "未开启。开启后会记录每拍决策链、节点原始读写与运行日志。"
                },
            }))
        }
        other => Ctx::err(ErrorCode::Type, format!("未知的 debug 子命令：{other}")),
    }
}

/// `records` —— **清空全部记录**
/// 覆盖：最近事件 · 充电历史 · 诊断报告 · 界面打点日志 · 开机记录日志。
///
/// ⚠️ **不含**：`config.json`（你的设置）、`device.json`（设备档案 ——
///    清了模块会进入"未检测不控制"）、进程锁与控制通道。
///
/// # 为什么走 `Pending` 而不是在这里直接删
///
/// 见 [`Pending::RecordsClear`] 的文档 —— 主要因为**清完要补记一条事件**，
/// 而事件文件由 worker 独占写入。
fn records(ctx: &Ctx<'_>, args: &Value) -> Response {
    let sub = arg(args, 0).unwrap_or_else(|| "clear".to_string());
    match sub.as_str() {
        "clear" => {
            ctx.push(Pending::RecordsClear);
            ctx.data(json!({
                "cleared": true,
                "scope": records_scope(),
                "kept": RECORDS_KEPT,
                "note": "已清空全部记录；正在充电的这一次会继续记录。\
                         配置与设备档案未受影响。",
            }))
        }
        // 只列出会清什么，不真删 —— 供界面在确认弹窗里展示
        "list" => ctx.data(json!({
            "scope": records_scope(),
            "kept": RECORDS_KEPT,
        })),
        other => Ctx::err(ErrorCode::Type, format!("未知的 records 子命令：{other}")),
    }
}

/// 「清空全部记录」**明确不碰**的东西（给用户看的文案）
///
/// 必须比"清什么"更醒目 —— 用户最怕的是"清记录把我的设置也清了"。
/// 特别是 `device.json`：清掉它模块会**停止控制充电**。
const RECORDS_KEPT: &[&str] = &[
    "config.json（你的全部设置）",
    "device.json（设备档案 —— 清了模块会停止控制充电）",
    "presets.json（预设模板）",
];

/// 「清空全部记录」覆盖的范围（给用户看的文案）
/// # ⚠️ 不在这里硬编码文件清单
///
/// 前两项是**特殊处理**的（不是简单删文件，见下方原因），所以写在这里；
/// **其余全部来自 [`crate::paths::clearable_records`]** —— 与
/// `engine.rs` 真正删除的那批同源，**结构上不可能不一致**。
///
/// 硬编码两份的后果是最坏的一类不一致：弹窗说清了 A、B、C，
/// 实际只清了 A、B —— 用户据此以为清干净了，而事实不是。
fn records_scope() -> Vec<String> {
    let mut scope = vec![
        // 事件：还要清**轮转归档**（`.1` / `.2`）与内存缓冲，
        // 不是删一个文件那么简单
        "最近事件（含轮转归档）".to_string(),
        // 历史：**刻意保住正在进行的会话** ——
        // 用户点"清空"是清理旧记录，不该毁掉他此刻正在观察的这一条
        "充电历史（已完成会话）".to_string(),
    ];
    scope.extend(
        crate::paths::clearable_records()
            .into_iter()
            .map(|r| r.label.to_string()),
    );
    scope
}

/// `logs` —— 日志查看
fn logs(args: &Value) -> Response {
    let sub = arg(args, 0).unwrap_or_else(|| "tail".to_string());
    let dir = crate::paths::log_dir();

    match sub.as_str() {
        "list" => {
            let mut files = Vec::new();
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten() {
                    let p = e.path();
                    let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                    files.push(json!({
                        "name": p.file_name().map(|n| n.to_string_lossy().to_string()),
                        "size_kb": size / 1024,
                    }));
                }
            }
            // 数据目录下的非 logs/ 文件也列出来（事件流、运行快照、CSV）
            for path in [
                crate::paths::events_jsonl(),
                crate::paths::runtime_json(),
                crate::paths::session_csv(),
            ] {
                if path.exists() {
                    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    files.push(json!({
                        "name": path.file_name().map(|n| n.to_string_lossy().to_string()),
                        "size_kb": size / 1024,
                    }));
                }
            }
            Response::success_with(json!({
                "dir": dir.display().to_string(),
                "files": files,
            }))
        }
        "tail" => {
            let n = arg(args, 1)
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(50);
            // ⚠️ 契约 §4 细节 8：`tail` 的 `file` 字段给**完整路径**
            let path = crate::paths::main_log();
            Response::success_with(json!({
                "file": path.display().to_string(),
                "lines": observability::tail_lines(&path, n),
            }))
        }
        "show" => {
            let Some(name) = arg(args, 1) else {
                return Response::failure(
                    ErrorCode::Type.as_str(),
                    "用法：logs show <文件名> [行数]",
                );
            };
            if name.contains('/') {
                return Response::failure(ErrorCode::Type.as_str(), "文件名不得含路径分隔符");
            }
            let allowed = [
                "main.log",
                "main",
                "service.log",
                "service",
                "daemon.out",
                "daemon",
                "detect.log",
                "detect",
                "events.jsonl",
                "events",
                "session.csv",
                "csv",
            ];
            if !allowed.contains(&name.as_str()) {
                return Response::failure(
                    ErrorCode::Range.as_str(),
                    format!("未知日志文件：{name}"),
                );
            }
            let short = name
                .trim_end_matches(".log")
                .trim_end_matches(".jsonl")
                .trim_end_matches(".csv");
            let path = match short {
                "main" => crate::paths::main_log(),
                "service" => dir.join("service.log"),
                "daemon" => dir.join("daemon.out"),
                "detect" => dir.join("detect.log"),
                "events" => crate::paths::events_jsonl(),
                "csv" => crate::paths::session_csv(),
                _ => crate::paths::main_log(),
            };
            let n = arg(args, 2)
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(50);
            Response::success_with(json!({
                // ⚠️ 契约 §4 细节 8：show 给**文件名**，tail 给**完整路径**
                "file": path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
                "lines": observability::tail_lines(&path, n),
            }))
        }
        "events" => {
            let n = arg(args, 1)
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(50);
            Response::success_with(json!({
                "file": "events.jsonl",
                "lines": observability::tail_lines(&crate::paths::events_jsonl(), n)
            }))
        }
        "csv" => {
            let n = arg(args, 1)
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(50);
            Response::success_with(json!({
                "file": "session.csv",
                "lines": observability::tail_lines(&crate::paths::session_csv(), n)
            }))
        }
        "clear" => {
            let mut cleared = Vec::new();
            for p in [
                crate::paths::main_log(),
                dir.join("service.log"),
                dir.join("daemon.out"),
            ] {
                if std::fs::remove_file(&p).is_ok() {
                    cleared.push(p.display().to_string());
                }
            }
            Response::success_with(json!({ "cleared": cleared }))
        }
        other => Response::failure(
            ErrorCode::Type.as_str(),
            format!("未知的 logs 子命令：{other}"),
        ),
    }
}

/// `limit` —— 充电上限（附加功能的**便捷别名**）
fn limit(ctx: &Ctx<'_>, args: &Value) -> Response {
    let sub = arg(args, 0).unwrap_or_else(|| "show".to_string());
    match sub.as_str() {
        "show" => addon_show(ctx, "soc_limit"),
        "probe" => {
            // 「写探」在真机上会真的写节点（可从回读判断是否真生效），
            // 而写 smart_chg 有可能把内核驱动拖入 D 状态 —— 因此
            // `limit probe` 走**只读探测**，不做写探。
            // 若是历史遗留的写探路径，必须也用看门狗包装（此处仅读，无需）。
            let mut io = crate::io_watchdog::WatchedIo::spawn(Box::new(crate::addon_host::SysfsIo));
            let mut probe = thermo_addons::soc_limit::SocLimit::new(Default::default());
            let outcome = {
                use thermo_addons::addon::Addon;
                probe.probe(&mut io)
            };
            ctx.data(json!({
                "supported": outcome.is_supported(),
                "detail": outcome.message(),
                "candidates": thermo_addons::soc_limit::SMART_CHG_CANDIDATES,
            }))
        }
        "set" => {
            let Some(v) = arg(args, 1) else {
                return Ctx::err(ErrorCode::Type, "用法：limit set <60~100>");
            };
            // `limit set` **同时开启**该功能（符合直觉），因此是一次两键写入
            let pairs = vec![
                ("limit.soc".to_string(), v.clone()),
                ("limit.enabled".to_string(), "true".to_string()),
            ];
            if let Err(e) = ctx.cfg.dry_run(&pairs) {
                return Ctx::err(e.code, e.msg);
            }
            for (k, val) in pairs {
                ctx.push(Pending::SetKey { key: k, value: val });
            }
            ctx.data(json!({"soc": v, "enabled": true}))
        }
        "on" => {
            ctx.push(Pending::SetKey {
                key: "limit.enabled".into(),
                value: "true".into(),
            });
            ctx.data(json!({"enabled": true}))
        }
        "off" => {
            ctx.push(Pending::SetKey {
                key: "limit.enabled".into(),
                value: "false".into(),
            });
            ctx.data(json!({
                "enabled": false,
                "note": "已停止干预（零写入）；系统自带的智能断充照常工作",
            }))
        }
        other => Ctx::err(ErrorCode::Type, format!("未知的 limit 子命令：{other}")),
    }
}

/// `addon` —— 附加功能通用入口
fn addon(ctx: &Ctx<'_>, args: &Value) -> Response {
    let sub = arg(args, 0).unwrap_or_else(|| "list".to_string());
    match sub.as_str() {
        "list" => {
            let list: Vec<Value> = ctx
                .addons
                .slots()
                .iter()
                .map(|s| {
                    let st = s.status();
                    json!({
                        "id": st.id,
                        "label": st.label,
                        "enabled": st.enabled,
                        "supported": st.supported,
                        "tripped": st.tripped,
                        "interval_s": st.interval_s,
                        "writes": s.writes(),
                        "errors": s.errors(),
                    })
                })
                .collect();
            ctx.data(json!({
                "addon_api_version": thermo_addons::ADDON_API_VERSION,
                "count": list.len(),
                "addons": list,
            }))
        }
        "show" => {
            let Some(id) = arg(args, 1) else {
                return Ctx::err(ErrorCode::Type, "用法：addon show <id>");
            };
            addon_show(ctx, &id)
        }
        "on" | "off" => {
            let Some(id) = arg(args, 1) else {
                return Ctx::err(ErrorCode::Type, format!("用法：addon {sub} <id>"));
            };
            let key = addon_enable_key(&id);
            let Some(key) = key else {
                return Ctx::err(ErrorCode::Range, format!("未知附加功能：{id}"));
            };
            ctx.push(Pending::SetKey {
                key: key.to_string(),
                value: (sub == "on").to_string(),
            });
            ctx.data(json!({"id": id, "enabled": sub == "on"}))
        }
        "reset" => {
            let Some(id) = arg(args, 1) else {
                return Ctx::err(ErrorCode::Type, "用法：addon reset <id>");
            };
            ctx.push(Pending::AddonRecover(id.clone()));
            ctx.data(json!({"id": id, "note": "已请求解除熔断"}))
        }
        other => Ctx::err(ErrorCode::Type, format!("未知的 addon 子命令：{other}")),
    }
}

/// 附加功能 id → 启用键
fn addon_enable_key(id: &str) -> Option<&'static str> {
    match id {
        "soc_limit" => Some("limit.enabled"),
        "pps" => Some("pps.enabled"),
        _ => None,
    }
}

/// 展示单个附加功能
fn addon_show(ctx: &Ctx<'_>, id: &str) -> Response {
    let Some(slot) = ctx.addons.slot(id) else {
        return Ctx::err(ErrorCode::Range, format!("未知附加功能：{id}"));
    };
    let st = slot.status();
    // ⚠️ 这里**手工重建** JSON（而不是直接序列化 `AddonStatus`），
    //    因为还要并入 5 个来自 `slot` 的运行期字段。
    //
    //    ⚠️ **代价**：`AddonStatus` 每加一个字段，这里都要跟一次 ——
    //    2026-10-16 就漏过一次（新增 `requires` 后 `limit show` 里查不到，
    //    而界面上有 —— 同一份信息两个出口不一致）。
    //    为此加了 `附加功能状态字段不会漏进命令行输出` 测试守着。
    ctx.data(json!({
        "addon_api_version": thermo_addons::ADDON_API_VERSION,
        "id": st.id,
        "label": st.label,
        "summary": st.summary,
        "origin": st.origin,
        "enabled": st.enabled,
        "supported": st.supported,
        "tripped": st.tripped,
        "interval_s": st.interval_s,
        "probe": st.probe,
        // 使用前提（用户要求 2026-10-16）
        // 界面与 CLI 必须是**同一份文案**，否则用户看到两个说法会以为哪个是错的
        "requires": st.requires,
        "consecutive_failures": slot.consecutive_failures(),
        "last_error": slot.last_error().map(|e| e.to_string()),
        "writes": slot.writes(),
        "errors": slot.errors(),
        "ticks": slot.ticks(),
        "detail": st.detail,
    }))
}

/// `data` —— 数据目录操作
fn data(args: &Value) -> Response {
    let sub = arg(args, 0).unwrap_or_else(|| "stats".to_string());
    let dir = crate::paths::data_dir();

    match sub.as_str() {
        "stats" => {
            let mut files = Vec::new();
            let mut total = 0u64;
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten() {
                    let Ok(meta) = e.metadata() else { continue };
                    if meta.is_file() {
                        total += meta.len();
                        files.push(json!({
                            "name": e.file_name().to_string_lossy(),
                            "size_kb": meta.len() / 1024,
                        }));
                    }
                }
            }
            Response::success_with(json!({
                "dir": dir.display().to_string(),
                "total_kb": total / 1024,
                "files": files,
            }))
        }
        "export" => {
            let files = [
                ("config.json", crate::paths::config_json()),
                ("presets.json", crate::paths::presets_json()),
            ];
            let with_device = flag(args, "with-device");
            let mut exported = Vec::new();

            if let Some(out_dir) = opt_value(args, "dir") {
                if let Err(e) = std::fs::create_dir_all(&out_dir) {
                    return Response::failure(
                        ErrorCode::Internal.as_str(),
                        format!("创建目录失败：{e}"),
                    );
                }
                for (name, src) in files {
                    if src.exists() {
                        let dst = format!("{out_dir}/{name}");
                        if std::fs::copy(&src, &dst).is_ok() {
                            exported.push(dst);
                        }
                    }
                }
                if with_device && crate::paths::device_json().exists() {
                    let dst = format!("{out_dir}/device.json");
                    if std::fs::copy(crate::paths::device_json(), &dst).is_ok() {
                        exported.push(dst);
                    }
                }
            } else {
                // `--out <文件>`：把内容拼成一份 JSON（**纯数据，无 ok/data 包裹**）
                let mut bundle = serde_json::Map::new();
                for (name, src) in files {
                    if let Ok(t) = std::fs::read_to_string(&src) {
                        if let Ok(v) = serde_json::from_str::<Value>(&t) {
                            bundle.insert(name.to_string(), v);
                        }
                    }
                }
                if with_device {
                    if let Ok(t) = std::fs::read_to_string(crate::paths::device_json()) {
                        if let Ok(v) = serde_json::from_str::<Value>(&t) {
                            bundle.insert("device.json".into(), v);
                        }
                    }
                }
                let value = Value::Object(bundle);
                if let Some(out) = opt_value(args, "out") {
                    let text = match serde_json::to_string_pretty(&value) {
                        Ok(t) => t,
                        Err(e) => {
                            return Response::failure(
                                ErrorCode::Internal.as_str(),
                                format!("序列化失败：{e}"),
                            )
                        }
                    };
                    if let Err(e) = std::fs::write(&out, text.as_bytes()) {
                        return Response::failure(
                            ErrorCode::Internal.as_str(),
                            format!("写入失败：{e}"),
                        );
                    }
                    exported.push(out);
                }
                return Response::success_with(json!({
                    "files": value,
                    "written": exported,
                }));
            }

            Response::success_with(json!({"exported": exported, "count": exported.len()}))
        }
        "import" => {
            if !flag(args, "confirm") {
                return Response::failure(
                    ErrorCode::Confirm.as_str(),
                    "导入将**覆盖**现有配置，请加 --confirm",
                );
            }
            let mut imported = Vec::new();
            if let Some(from_dir) = opt_value(args, "dir") {
                for (name, dst) in [
                    ("config.json", crate::paths::config_json()),
                    ("presets.json", crate::paths::presets_json()),
                ] {
                    let src = format!("{from_dir}/{name}");
                    if std::path::Path::new(&src).exists() {
                        if let Err(e) = std::fs::copy(&src, &dst) {
                            return Response::failure(
                                ErrorCode::Internal.as_str(),
                                format!("导入 {name} 失败：{e}"),
                            );
                        }
                        imported.push(name);
                    }
                }
            } else if let Some(file) = arg(args, 1) {
                let Ok(text) = std::fs::read_to_string(&file) else {
                    return Response::failure(
                        ErrorCode::Internal.as_str(),
                        format!("读取失败：{file}"),
                    );
                };
                // 契约：必须是**多行格式 JSON 对象**
                if !text.contains('\n') {
                    return Response::failure(
                        ErrorCode::Type.as_str(),
                        "需为多行格式 JSON 对象（单行 JSON 无法解析）",
                    );
                }
                let Ok(v) = serde_json::from_str::<Value>(&text) else {
                    return Response::failure(ErrorCode::Type.as_str(), "不是合法 JSON");
                };
                let Some(obj) = v.as_object() else {
                    return Response::failure(ErrorCode::Type.as_str(), "顶层必须是 JSON 对象");
                };
                if let Some(schema) = obj.get("schema").and_then(Value::as_u64) {
                    if schema > u64::from(config_store::CONFIG_SCHEMA) {
                        return Response::failure(
                            ErrorCode::Range.as_str(),
                            format!("备份来自更新版本（schema {schema}），拒绝导入"),
                        );
                    }
                }
                let text = match serde_json::to_string_pretty(&v) {
                    Ok(t) => t,
                    Err(e) => {
                        return Response::failure(
                            ErrorCode::Internal.as_str(),
                            format!("序列化失败：{e}"),
                        )
                    }
                };
                if let Err(e) = std::fs::write(crate::paths::config_json(), text.as_bytes()) {
                    return Response::failure(
                        ErrorCode::Internal.as_str(),
                        format!("写入失败：{e}"),
                    );
                }
                imported.push("config.json");
            } else {
                return Response::failure(
                    ErrorCode::Type.as_str(),
                    "用法：data import <文件> --confirm ｜ data import --dir <目录> --confirm",
                );
            }
            Response::success_with(json!({"imported": imported, "note": "需 reload 后生效"}))
        }
        "purge" => {
            if !flag(args, "force") {
                return Response::failure(
                    ErrorCode::Confirm.as_str(),
                    "将彻底清除配置与日志，请加 --force",
                );
            }
            // 路径白名单（防 rm -rf "" 类灾难）
            if dir.display().to_string() != crate::paths::DATA_DIR {
                return Response::failure(
                    ErrorCode::BadCond.as_str(),
                    "数据目录路径异常，拒绝删除",
                );
            }
            let _ = std::fs::remove_dir_all(&dir);
            Response::success_with(json!({"purged": crate::paths::DATA_DIR}))
        }
        other => Response::failure(
            ErrorCode::Type.as_str(),
            format!("未知的 data 子命令：{other}"),
        ),
    }
}

/// `probe` —— 诊断（**只读，零副作用**）
fn probe(ctx: &Ctx<'_>) -> Response {
    let gear = match GearTable::detect() {
        Ok((t, path)) => json!({
            "path": path,
            "count": t.len(),
            "table": t.current_limits().iter().map(|(g, ua)| json!({"gear": g, "current_limit_ua": ua})).collect::<Vec<_>>(),
            "max_power_gear": t.max_power_gear(),
            // 档位表里最大的电流上限：控制侧的上界依据
            "max_current_ua": t.max_current_ua(),
        }),
        Err(e) => json!({ "error": format!("{e:#}") }),
    };

    let mut soc = Vec::new();
    for c in thermo_addons::soc_limit::SMART_CHG_CANDIDATES {
        let n = crate::sysfs::Node::new(*c);
        soc.push(json!({
            "path": c,
            "exists": n.exists(),
            "writable": n.is_writable(),
        }));
    }

    let ctrl = [
        ("ctrl", &ctx.nodes.ctrl),
        ("curr", &ctx.nodes.curr),
        ("curr2", &ctx.nodes.curr2),
        ("remove", &ctx.nodes.remove),
    ]
    .iter()
    .map(|(name, n)| {
        json!({
            "name": name,
            "path": n.as_ref().map(|x| x.path_str()),
            "ok": n.is_some(),
        })
    })
    .collect::<Vec<_>>();

    ctx.data(json!({
        "gear_table": gear,
        "soc_candidates": soc,
        "control_nodes": ctrl,
        "missing": ctx.nodes.missing_required(),
        "can_control": ctx.nodes.can_control(),
        "can_observe": ctx.nodes.can_observe(),
        "can_limit_soc": ctx.nodes.can_limit_soc(),
    }))
}

/// `test` —— 环境自检
fn test(ctx: &Ctx<'_>, args: &Value) -> Response {
    let sub = arg(args, 0);

    if sub.as_deref() == Some("node-rw") {
        if !flag(args, "confirm") {
            return Ctx::err(
                ErrorCode::Confirm,
                "节点读写测试会临时写入节点，请加 --confirm",
            );
        }
        let Some(ctrl) = &ctx.nodes.ctrl else {
            return Ctx::err(ErrorCode::NoNode, "档位节点不可用");
        };
        let orig = ctrl.read_i64().unwrap_or(0);
        let write_ok = ctrl.write_and_verify(&orig.to_string()).unwrap_or(false);
        return ctx.data(json!({
            "node": ctrl.path_str(),
            "original": orig,
            "write_ok": write_ok,
            "note": "写入的是原值（零副作用验证）",
        }));
    }

    // 默认：环境自检
    ctx.data(json!({
        "config_file": ctx.cfg.path().display().to_string(),
        "config_exists": ctx.cfg.path().exists(),
        "config_keys": config_store::key_count(),
        "data_dir": crate::paths::DATA_DIR,
        "data_dir_writable": std::fs::OpenOptions::new()
            .write(true).create(true).truncate(false).open(crate::paths::data_dir().join(".wtest")).is_ok(),
        "gear_table": ctx.gear_table.map(|t| t.len()),
        "can_control": ctx.nodes.can_control(),
        "can_observe": ctx.nodes.can_observe(),
        "can_limit_soc": ctx.nodes.can_limit_soc(),
        "missing_nodes": ctx.nodes.missing_required(),
        "unverified_device": ctx.profile.is_unverified(),
    }))
}

/// `watch` —— 订阅事件流
fn watch(_ctx: &Ctx<'_>, stream: bool) -> Response {
    if !stream {
        return Ctx::err(ErrorCode::Type, "watch 需带 stream=true 请求");
    }
    Response::success_with(json!({
        "subscribed": true,
        "note": "后续事件将以 JSON Lines 形式推送",
    }))
}

/// `heartbeat` —— 前端心跳
fn heartbeat(ctx: &Ctx<'_>) -> Response {
    ctx.data(json!({
        "daemon": "running",
        "ts": ctx.now_s,
    }))
}

/// `help` —— 命令帮助
fn help() -> Response {
    Response::success_with(json!({
        "commands": [
            "info | status | watch",
            "device show|probe|export|import|reset",
            "get <键> | get --all | get --section <分区>",
            "set <键> <值> | set --batch k=v … | reload",
            // 多模式：4 套**互相独立**的配置（切走再切回，值仍在）
            "mode list|show|switch <保守|官方|极速|自定义>",
            "preset list|show|apply|refresh-official",
            "restore [--hard]",
            "logs tail|show|list|events|csv|clear",
            // 充电历史：逐次充电的过程曲线（纯观测，不参与决策）
            "history list|show|stats|clear",
            "limit show|probe|set|on|off",
            "addon list|show|on|off|reset",
            "data stats|export|import|purge",
            "probe | test [node-rw --confirm] | help",
        ],
    }))
}

/// 今天的日期（`YYYYMMDD`）
fn today_compact() -> String {
    // 不引入 chrono：用系统 date 命令；失败则退化为时间戳
    std::process::Command::new("date")
        .arg("+%Y%m%d")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("{}", observability::now_unix()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn ctx_with(
        cfg: ConfigStore,
    ) -> (
        Ctx<'static>,
        ConfigStore,
        EffectiveProfile,
        Nodes,
        thermo_addons::runner::AddonRunner,
    ) {
        ctx_with_detected(cfg, false)
    }

    /// 同 [`ctx_with`]，但可指定**是否已检测**
    ///
    /// `detected` 是允许控制的总闸门：`false` ⇒ 决策层零写入。
    /// 界面必须能区分这两种情况（否则会把「不控制」误显示成「待机」）。
    fn ctx_with_detected(
        cfg: ConfigStore,
        detected: bool,
    ) -> (
        Ctx<'static>,
        ConfigStore,
        EffectiveProfile,
        Nodes,
        thermo_addons::runner::AddonRunner,
    ) {
        // 用 Box::leak 造出 'static 生命周期，仅供测试使用
        let cfg: &'static ConfigStore = Box::leak(Box::new(cfg));
        let mut generic = crate::profile_io::generic_profile(Default::default());
        generic.capabilities.detected = detected;
        let profile: &'static EffectiveProfile = Box::leak(Box::new(EffectiveProfile {
            profile: generic,
            source: thermo_ipc::device::ProfileSource::Generic,
        }));
        let nodes: &'static Nodes = Box::leak(Box::new(Nodes::default()));
        let state: &'static thermo_core::State = Box::leak(Box::new(thermo_core::State::default()));
        let (slots, _) = thermo_addons::registry::build_all(cfg.raw());
        let addons: &'static thermo_addons::runner::AddonRunner =
            Box::leak(Box::new(thermo_addons::runner::AddonRunner::new(slots)));
        // 测试用一个指向临时目录的历史记录器（空历史，不参与断言）
        let history: &'static crate::history::HistoryStore =
            Box::leak(Box::new(crate::history::HistoryStore::new(
                std::env::temp_dir().join("thermo_cmd_test_history.json"),
                std::env::temp_dir().join("thermo_cmd_test_history.current.json"),
                crate::history::HistoryCfg::default(),
            )));

        let ctx = Ctx {
            cfg,
            profile,
            nodes,
            gear_table: None,
            state,
            ticks: 0,
            started_at_s: 0,
            released: false,
            addons,
            history,
            pending: RefCell::new(Vec::new()),
            now_s: 1000,
        };
        (
            ctx,
            ConfigStore::with_defaults("x"),
            EffectiveProfile {
                profile: crate::profile_io::generic_profile(Default::default()),
                source: thermo_ipc::device::ProfileSource::Generic,
            },
            Nodes::default(),
            thermo_addons::runner::AddonRunner::new(Vec::new()),
        )
    }

    // ────────────────────────────────────────────────────────────
    //  电荷泵芯片温度（观测）
    // ────────────────────────────────────────────────────────────

    /// 单位换算必须是 `0.1℃`（写成 `/100` 会静默显示错误的温度）
    #[test]
    fn 电荷泵温度按零点一摄氏度换算() {
        // 真机实测值：`tdie = 425` ⇒ 42.5℃
        assert_eq!(format!("{}", tenths_to_celsius(425)), "42.5");
        // 未插充电器时的实测值
        assert_eq!(format!("{}", tenths_to_celsius(460)), "46");
        // 整数边界不产生浮点尾巴
        assert_eq!(format!("{}", tenths_to_celsius(0)), "0");
        assert_eq!(format!("{}", tenths_to_celsius(500)), "50");
    }

    /// 无此节点时必须输出 `null`，**不能**退化成 `0`
    #[test]
    fn 无电荷泵节点时输出null而非零() {
        let (ctx, _c, _p, _n, _a) = ctx_with(ConfigStore::with_defaults("x.json"));
        // `ctx_with` 用的是 `Nodes::default()` ⇒ 所有观测节点都是 None
        let (resp, _) = dispatch(&ctx, "status", &json!([]), false);
        match resp {
            Response::Ok { data: Some(d), .. } => {
                assert!(
                    d["thermal"]["cp_die_temp_c"].is_null(),
                    "无节点时应为 null —— 输出 0 会被误读为\"电荷泵 0℃\""
                );
            }
            other => panic!("status 应返回数据，实际：{other:?}"),
        }
    }

    // ────────────────────────────────────────────────────────────
    //  检测状态（`status` 的 `detected` 字段）
    // ────────────────────────────────────────────────────────────

    /// 未检测时必须明确报告 —— 界面据此提示「不控制充电」
    ///
    /// # 为什么这条重要
    ///
    /// 未检测时决策层零写入（安全闸门「不猜硬件」），
    /// 但 `state` 会是 `idle` ⇒ 界面把 `idle` 译作「待机」，
    /// **一个看起来完全正常的词**。
    ///
    /// 用户装完模块看到「待机」会以为在工作，实际什么都没干。
    /// 因此 `status` 必须单独给出 `detected`，让界面能显示明确提示。
    #[test]
    fn 未检测时status报告detected为false() {
        let (ctx, _c, _p, _n, _a) = ctx_with_detected(ConfigStore::with_defaults("x.json"), false);
        let (resp, _) = dispatch(&ctx, "status", &json!([]), false);
        match resp {
            Response::Ok { data: Some(d), .. } => {
                assert_eq!(
                    d["detected"], false,
                    "未检测必须报 false —— 否则界面会把「不控制」显示成「待机」"
                );
                // 同时确认 `state` 确实是 `idle`（这正是需要额外字段的原因）
                assert_eq!(
                    d["state"], "idle",
                    "未检测时 state 应为 idle —— 若哪天变了，本条注释需更新"
                );
            }
            other => panic!("status 应返回数据，实际：{other:?}"),
        }
    }

    /// 已检测时报告 `true`（界面不再显示警告）
    #[test]
    fn 已检测时status报告detected为true() {
        let (ctx, _c, _p, _n, _a) = ctx_with_detected(ConfigStore::with_defaults("x.json"), true);
        let (resp, _) = dispatch(&ctx, "status", &json!([]), false);
        match resp {
            Response::Ok { data: Some(d), .. } => {
                assert_eq!(d["detected"], true, "已检测应报 true");
            }
            other => panic!("status 应返回数据，实际：{other:?}"),
        }
    }

    #[test]
    fn 未知命令返回类型错误() {
        let (ctx, _c, _p, _n, _a) = ctx_with(ConfigStore::with_defaults("x.json"));
        let (resp, _) = dispatch(&ctx, "不存在的命令", &json!([]), false);
        match resp {
            Response::Err { code, .. } => assert_eq!(code, "E_TYPE"),
            _ => panic!("应返回错误"),
        }
    }

    #[test]
    fn 未知配置键被拒绝() {
        let (ctx, _c, _p, _n, _a) = ctx_with(ConfigStore::with_defaults("x.json"));
        let (resp, _) = dispatch(&ctx, "set", &json!(["nope.key", "1"]), false);
        match resp {
            Response::Err { code, msg, .. } => {
                assert_eq!(code, "E_UNKNOWN_KEY");
                assert!(msg.contains("未知配置键"));
            }
            _ => panic!("应返回错误"),
        }
    }

    #[test]
    fn 只读键被拒绝() {
        let (ctx, _c, _p, _n, _a) = ctx_with(ConfigStore::with_defaults("x.json"));
        let (resp, _) = dispatch(&ctx, "set", &json!(["thermal.abs_max", "50"]), false);
        match resp {
            Response::Err { code, .. } => assert_eq!(code, "E_READONLY"),
            _ => panic!("应返回错误"),
        }
    }

    #[test]
    fn 越界值被拒绝() {
        let (ctx, _c, _p, _n, _a) = ctx_with(ConfigStore::with_defaults("x.json"));
        let (resp, _) = dispatch(&ctx, "set", &json!(["thermal.tighten_temp", "99"]), false);
        match resp {
            Response::Err { code, .. } => assert_eq!(code, "E_RANGE"),
            _ => panic!("应返回错误"),
        }
    }

    #[test]
    fn 合法写入进入待办队列() {
        let (ctx, _c, _p, _n, _a) = ctx_with(ConfigStore::with_defaults("x.json"));
        // ⚠️ 取值必须同时满足**单键**与**跨键**校验：
        //    出厂 relax=42.0，内核要求 relax ≤ tighten - 1 ⇒ tighten ≥ 43
        let (resp, _) = dispatch(&ctx, "set", &json!(["thermal.tighten_temp", "44"]), false);
        assert!(matches!(resp, Response::Ok { ok: true, .. }));
        let pending = ctx.pending.borrow();
        assert_eq!(pending.len(), 1);
        assert!(matches!(pending[0], Pending::SetKey { .. }));
    }

    #[test]
    fn 跨键关系非法时命令当场被拒() {
        let (ctx, _c, _p, _n, _a) = ctx_with(ConfigStore::with_defaults("x.json"));

        // 41 单键合法（在 20~45 内），但会让 relax(42) 高于 tighten ⇒ 组合非法。
        // 命令层必须在**当场**拒绝，而不是返回 ok 后让主循环静默丢弃。
        let (resp, _) = dispatch(&ctx, "set", &json!(["thermal.tighten_temp", "41"]), false);
        match resp {
            Response::Err { code, msg, .. } => {
                assert_eq!(code, "E_COLLISION", "应报配置校验未通过");
                assert!(
                    msg.contains("放松温度"),
                    "错误信息应指明冲突关系，实得：{msg}"
                );
            }
            _ => panic!("跨键非法必须当场拒绝，实得 {resp:?}"),
        }
        assert!(ctx.pending.borrow().is_empty(), "被拒时不得进入待办队列");
    }

    #[test]
    fn 批量写入可一次性调整相关键() {
        let (ctx, _c, _p, _n, _a) = ctx_with(ConfigStore::with_defaults("x.json"));
        let (resp, _) = dispatch(
            &ctx,
            "set",
            &json!([
                "--batch",
                "thermal.tighten_temp=41",
                "thermal.relax_temp=40"
            ]),
            false,
        );
        assert!(
            matches!(resp, Response::Ok { ok: true, .. }),
            "同时下调两键应被接受，实得 {resp:?}"
        );
        assert_eq!(ctx.pending.borrow().len(), 1);
    }

    #[test]
    fn limit_set同时开启功能() {
        let (ctx, _c, _p, _n, _a) = ctx_with(ConfigStore::with_defaults("x.json"));
        let (resp, _) = dispatch(&ctx, "limit", &json!(["set", "85"]), false);
        assert!(matches!(resp, Response::Ok { ok: true, .. }));
        let pending = ctx.pending.borrow();
        assert_eq!(pending.len(), 2, "应同时写入 soc 与 enabled");
    }

    #[test]
    fn limit_off不产生任何节点写入() {
        let (ctx, _c, _p, _n, _a) = ctx_with(ConfigStore::with_defaults("x.json"));
        let (resp, _) = dispatch(&ctx, "limit", &json!(["off"]), false);
        assert!(matches!(resp, Response::Ok { ok: true, .. }));
        let pending = ctx.pending.borrow();
        assert_eq!(pending.len(), 1, "只改配置，不写节点");
        assert!(matches!(&pending[0], Pending::SetKey { key, value }
            if key == "limit.enabled" && value == "false"));
    }

    #[test]
    fn 进程控制命令被明确拒绝() {
        let (ctx, _c, _p, _n, _a) = ctx_with(ConfigStore::with_defaults("x.json"));
        for cmd in ["start", "stop", "restart"] {
            let (resp, _) = dispatch(&ctx, cmd, &json!([]), false);
            match resp {
                Response::Err { code, .. } => assert_eq!(code, "E_BAD_COND"),
                _ => panic!("{cmd} 应明确拒绝"),
            }
        }
    }

    #[test]
    fn 批量写入的预校验拦截未知键() {
        let (ctx, _c, _p, _n, _a) = ctx_with(ConfigStore::with_defaults("x.json"));
        let (resp, _) = dispatch(
            &ctx,
            "set",
            &json!(["--batch", "enabled=false", "bad.key=1"]),
            false,
        );
        match resp {
            Response::Err { code, .. } => assert_eq!(code, "E_UNKNOWN_KEY"),
            _ => panic!("应拦截"),
        }
    }

    #[test]
    fn addon_list列出全部附加功能() {
        let (ctx, _c, _p, _n, _a) = ctx_with(ConfigStore::with_defaults("x.json"));
        let (resp, _) = dispatch(&ctx, "addon", &json!(["list"]), false);
        match resp {
            Response::Ok { data: Some(d), .. } => {
                // ⚠️ 不按数组下标断言（顺序会随新增功能变化），按 id 查
                let list = d["addons"].as_array().expect("应是数组");
                assert_eq!(d["count"], list.len(), "count 必须与实际条数一致");

                for id in ["soc_limit", "pps"] {
                    let item = list
                        .iter()
                        .find(|a| a["id"] == id)
                        .unwrap_or_else(|| panic!("列表里应有 {id}"));
                    assert_eq!(item["enabled"], false, "{id} 出厂默认关闭");
                }
            }
            _ => panic!("应返回列表"),
        }
    }

    #[test]
    fn 未知附加功能返回范围错误() {
        let (ctx, _c, _p, _n, _a) = ctx_with(ConfigStore::with_defaults("x.json"));
        let (resp, _) = dispatch(&ctx, "addon", &json!(["show", "没有这个"]), false);
        match resp {
            Response::Err { code, .. } => assert_eq!(code, "E_RANGE"),
            _ => panic!("应返回错误"),
        }
    }

    #[test]
    fn logs_show拒绝路径穿越() {
        let (resp, _) = dispatch_logs_helper("../../etc/passwd");
        match resp {
            Response::Err { code, .. } => assert_eq!(code, "E_TYPE"),
            _ => panic!("应拒绝含路径分隔符的文件名"),
        }
    }

    fn dispatch_logs_helper(name: &str) -> (Response, bool) {
        let (ctx, _c, _p, _n, _a) = ctx_with(ConfigStore::with_defaults("x.json"));
        dispatch(&ctx, "logs", &json!(["show", name]), false)
    }

    #[test]
    fn logs_show拒绝未知文件名() {
        let (resp, _) = dispatch_logs_helper("hack.log");
        match resp {
            Response::Err { code, .. } => assert_eq!(code, "E_RANGE"),
            _ => panic!("应拒绝未知日志文件"),
        }
    }

    #[test]
    fn watch不带头流标志时拒绝() {
        let (ctx, _c, _p, _n, _a) = ctx_with(ConfigStore::with_defaults("x.json"));
        let (resp, _) = dispatch(&ctx, "watch", &json!([]), false);
        match resp {
            Response::Err { code, .. } => assert_eq!(code, "E_TYPE"),
            _ => panic!("应拒绝"),
        }
    }

    #[test]
    fn 返回的响应可序列化为契约格式() {
        let (ctx, _c, _p, _n, _a) = ctx_with(ConfigStore::with_defaults("x.json"));
        let (resp, _) = dispatch(&ctx, "info", &json!([]), false);
        let text = resp.to_line().unwrap();
        assert!(text.ends_with('\n'));
        let v: Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["data"]["module_id"], "a_hyperpower");

        // 🔴 防漏改：模块 id 与模块目录**必须同名**
        //
        // KernelSU 用 `module.prop` 的 `id` 作模块目录名，因此这两处
        // 天然耦合。它们分散在不同文件里（`paths.rs` 与 `commands.rs`），
        // 改 id 时**很容易漏掉一处** —— 漏掉则 `module_dir` 指向不存在的
        // 目录，用户看到假的路径。
        //
        // 这里把"必须一致"固化成断言（同时也就覆盖了 `module.prop`
        // 与 `paths.rs` 的一致性：前者改了而后者没改时，本断言会失败）。
        let dir_name = crate::paths::MODULE_DIR
            .rsplit('/')
            .next()
            .expect("MODULE_DIR 应至少含一个 /");
        assert_eq!(
            v["data"]["module_id"], dir_name,
            "module_id 与 MODULE_DIR 的目录名不一致 —— 改 id 时漏改了其中一处"
        );
    }

    /// 模块 id 必须与 `module.prop` 一致（跨文件一致性）
    ///
    /// 这是上一条测试的**补充**：上一条只保证两个 Rust 常量互相一致，
    /// 本条直接读 `module.prop` 原文，保证 Rust 侧与打包侧不脱节。
    ///
    /// 路径相对 `CARGO_MANIFEST_DIR` 往上两级到仓库根 —— 该文件由
    /// `cargo xtask pack` 打进 zip 的 `module.prop`，是**唯一真源**。
    #[test]
    fn 模块id与module_prop一致() {
        // `CARGO_MANIFEST_DIR` = `<仓库根>/native/crates/thermo-cli`
        // ⇒ 需上溯三级才到仓库根（初版少写了一级，测试报"找不到文件"）
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .expect("应能定位到仓库根");
        let prop = root.join("module/src/module.prop");
        assert!(
            prop.exists(),
            "module.prop 不存在：{}（路径推导有误？）",
            prop.display()
        );
        let text = std::fs::read_to_string(&prop)
            .unwrap_or_else(|e| panic!("读取 {} 失败：{e}", prop.display()));

        let id = text
            .lines()
            .find_map(|l| l.strip_prefix("id="))
            .map(str::trim)
            .expect("module.prop 应有 id= 行");

        assert_eq!(
            id, "a_hyperpower",
            "module.prop 的 id 变了 —— 需同步 paths.rs 的 MODULE_DIR 与 commands.rs"
        );
        assert_eq!(
            crate::paths::MODULE_DIR,
            format!("/data/adb/modules/{id}"),
            "MODULE_DIR 与 module.prop 的 id 不一致"
        );
    }

    /// Cargo 包版本必须与 `module.prop` 的 `version` 一致
    ///
    /// `ctl info` 的 `version` 用 `CARGO_PKG_VERSION`，而 KernelSU 模块列表
    /// 读 `module.prop` —— 两处不同步时用户会看到两个版本号，无法判断
    /// 模块是否已更新（0.5.1 就漏了这处，故固化成测试）。
    #[test]
    fn 版本号与module_prop一致() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .expect("应能定位到仓库根");
        let prop = root.join("module/src/module.prop");
        let text = std::fs::read_to_string(&prop)
            .unwrap_or_else(|e| panic!("读取 {} 失败：{e}", prop.display()));
        let version = text
            .lines()
            .find_map(|l| l.strip_prefix("version="))
            .map(str::trim)
            .expect("module.prop 应有 version= 行");

        assert_eq!(
            env!("CARGO_PKG_VERSION"),
            version,
            "Cargo 版本与 module.prop 的 version 不一致 —— 发版时需同步两处"
        );
    }

    /// WebUI 里硬编码的模块路径必须与 `module.prop` 的 id 一致
    ///
    /// # 为什么需要这条
    ///
    /// WebUI 通过 `ksu.exec("sh /data/adb/modules/<id>/bin/ctl …")` 调用后端，
    /// 这个路径在 TypeScript 里是**字符串常量**，编译器管不到。
    ///
    /// 改 id 时若漏改这里，后果是**整个界面失效** ——
    /// 用户点任何按钮都没反应（`ctl` 找不到），而且界面上看不到明显错误。
    /// 这属于"最难排查"的一类：后端全好，只有前端静默失效。
    ///
    /// 因此用 Rust 测试去**读 TypeScript 源码**并核对。
    #[test]
    fn webui路径与module_prop一致() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .expect("应能定位到仓库根");
        let api = root.join("webui/src/api.ts");
        assert!(
            api.exists(),
            "api.ts 不存在：{}（路径推导有误？）",
            api.display()
        );
        let text = std::fs::read_to_string(&api)
            .unwrap_or_else(|e| panic!("读取 {} 失败：{e}", api.display()));

        // 找 `const MODULE_DIR = "…";`
        let dir = text
            .lines()
            .find_map(|l| {
                let t = l.trim();
                t.strip_prefix("const MODULE_DIR = \"")
                    .and_then(|r| r.split('"').next())
            })
            .expect("api.ts 里应有 `const MODULE_DIR = \"…\";`");

        assert_eq!(
            dir,
            crate::paths::MODULE_DIR,
            "WebUI 的 MODULE_DIR 与 Rust 的 MODULE_DIR 不一致 —— \
             改 id 时漏改了 api.ts，界面会**整个失效**"
        );
    }

    // ────────────────────────────────────────────────────────────
    //  附加功能输出完整性
    // ────────────────────────────────────────────────────────────

    /// `AddonStatus` 的每个字段都必须出现在 `limit show` 的输出里
    ///
    /// # 这个测试是为一次真实事故加的（2026-10-16）
    ///
    /// 用户要求「加上提示：此模块需打开电池健康保护功能」时，
    /// 我给 `AddonStatus` 加了 `requires` 字段 —— 界面**立刻**显示了，
    /// 但 `addon_show` 是**手工重建 JSON** 的（它还要并入 5 个来自
    /// `slot` 的运行期字段，不能直接序列化），那行没跟 ⇒
    /// **`ctl limit show` 里查不到这条提示**。
    ///
    /// 同一份信息两个出口不一致，而且不报错 —— 属于最难发现的一类。
    ///
    /// # 为什么用"比对"而不是"手抄字段名"
    ///
    /// 手抄的清单本身就是**会漏的那一份**（它就是 `addon_show` 里
    /// 那张手工清单的副本）。这里改成**从真实 `AddonStatus` 序列化出键集**
    /// 再与实际输出比对 ⇒ 以后谁给 `AddonStatus` 加了字段，
    /// 这个测试会直接失败，并指出漏了哪个键。
    #[test]
    fn 附加功能状态字段不会漏进命令行输出() {
        let (ctx, ..) = ctx_with(ConfigStore::with_defaults("x"));

        // "应当输出的全部字段" —— 来自类型本身，不是手抄
        let st = ctx
            .addons
            .slot("soc_limit")
            .expect("soc_limit 应当已注册")
            .status();
        let expected = serde_json::to_value(&st).expect("AddonStatus 应当可序列化");
        let expected_obj = expected
            .as_object()
            .expect("AddonStatus 序列化后应当是对象");

        // "实际输出了什么"
        let r = addon_show(&ctx, "soc_limit");
        let Response::Ok { data: Some(d), .. } = r else {
            panic!("limit show 应当返回数据，实际：{r:?}");
        };
        let actual = d.as_object().expect("输出应当是 JSON 对象");

        let missing: Vec<&String> = expected_obj
            .keys()
            .filter(|k| !actual.contains_key(*k))
            .collect();
        assert!(
            missing.is_empty(),
            "这些字段在 `AddonStatus` 里有，但 `addon_show` 没输出：{missing:?} —— \
             `ctl limit show` 会缺这些信息（界面上却看得到）"
        );
    }

    /// 使用前提必须同时出现在 `spec` 与 CLI 输出里（用户要求 2026-10-16）
    ///
    /// 这条与上一条的差别：上一条防**漏字段**，这条防**字段是空的**——
    /// 建了字段却忘记赋值，比对测试照样通过。
    #[test]
    fn 使用前提必须真的输出到命令行() {
        let (ctx, ..) = ctx_with(ConfigStore::with_defaults("x"));

        let r = addon_show(&ctx, "soc_limit");
        let Response::Ok { data: Some(d), .. } = r else {
            panic!("limit show 应当返回数据");
        };

        let requires = d["requires"]
            .as_str()
            .expect("`limit show` 必须输出使用前提（不能是 null）");
        assert!(
            requires.contains("电池保护") || requires.contains("电池健康保护"),
            "使用前提要点名那个系统开关，实得：{requires}"
        );
    }

    // ────────────────────────────────────────────────────────────
    //  「清空全部记录」的安全护栏
    // ────────────────────────────────────────────────────────────

    /// 🔴 **绝不能**删配置与设备档案
    ///
    /// 这两样一旦被清，后果都是用户意料之外的：
    ///
    /// | 文件 | 清掉的后果 |
    /// |---|---|
    /// | `config.json` | 用户**全部设置**回到出厂（模式、温度阈值、功率上限、充电上限…） |
    /// | `device.json` | 模块进入「**未检测不控制**」⇒ **完全不管充电**，必须重跑检测才恢复 |
    ///
    /// 而"清空记录"这个操作的字面意思，**完全不会让用户预期到这两件事**。
    /// 所以用测试把它钉死：以后谁往 `clearable_records()` 里加路径，
    /// 只要碰到这两样就会立刻失败。
    #[test]
    fn 清空记录绝不能碰配置与设备档案() {
        let paths: Vec<String> = crate::paths::clearable_records()
            .into_iter()
            .map(|r| r.path.display().to_string())
            .collect();

        // ⚠️ 用 `contains` 而不是全路径相等 —— 挡住"换个目录放同名文件"
        for forbidden in [
            "config.json",
            "device.json",
            "presets.json",
            "runtime.json",
            "/ipc/",
            ".pid",
            ".lock",
        ] {
            assert!(
                !paths.iter().any(|p| p.contains(forbidden)),
                "「清空全部记录」绝不能包含 `{forbidden}`，实得：{paths:?}"
            );
        }
    }

    /// 界面弹窗展示的清单必须**覆盖**真正要删的每一项
    ///
    /// 反过来说：不许出现"弹窗说会清 A，实际没清 A"或"实际清了 B，
    /// 弹窗没提 B"。后者更危险 —— 用户会以为某项数据还在。
    #[test]
    fn 清空记录的界面清单与实删清单一致() {
        let scope = records_scope();
        let real = crate::paths::clearable_records();

        // 每一项真删的，都必须在界面清单里有同名条目
        for rec in &real {
            assert!(
                scope.iter().any(|s| s == rec.label),
                "`{}` 会被真删，但界面清单里没有它 —— \
                 用户看到的「会清什么」就是假的",
                rec.label
            );
        }

        // 反向：界面清单里除"事件/历史"两条特殊项外，其余都必须真的会被删
        let labels: Vec<&str> = real.iter().map(|r| r.label).collect();
        for s in &scope {
            if s.contains("最近事件") || s.contains("充电历史") {
                continue; // 这两条走 EventLog / HistoryStore，不在本表
            }
            assert!(
                labels.contains(&s.as_str()),
                "界面清单里的 `{s}` 不在实删清单里 —— 用户以为清了，实际没清"
            );
        }
    }

    /// 「不碰」的清单里必须**点名** `device.json` 并说明后果
    ///
    /// 它是唯一一个"清掉会导致功能静默停止"的文件，光写文件名不够。
    #[test]
    fn 不碰清单要点明设备档案的后果() {
        let kept: String = RECORDS_KEPT.join(" · ");
        assert!(kept.contains("config.json"), "必须点名配置：{kept}");
        assert!(kept.contains("device.json"), "必须点名设备档案：{kept}");
        assert!(
            kept.contains("停止控制") || kept.contains("重跑检测"),
            "光写文件名不够 —— 要说明「清了会停止控制充电」这个后果：{kept}"
        );
    }
}
