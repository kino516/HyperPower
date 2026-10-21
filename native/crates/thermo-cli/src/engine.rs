//! 控制引擎 —— worker 的"一步"逻辑
//!
//! 把主循环里**所有有判断的部分**集中在这里，外壳（定时器、信号、进程管理）
//! 单独放在 `worker.rs`。这样做的目的是让核心逻辑**可以在 CI 上测试**：
//! 只要给一棵假节点树（`THERMO_SYSFS_ROOT`），就能跑完整的
//! 「采样 → 决策 → 钳位 → 写入 → 回读」链路，无需真机、无需 root。
//!
//! # 每拍做什么（固定顺序）
//!
//! ```text
//! ① 控制通道 poll（处理客户端请求，**绝不阻塞**）
//! ② 采样电池状态           ← 读失败则累计计数，不写节点
//! ③ 温控决策（内核纯函数）  ← 当前拍决定"要不要写、写什么"
//! ④ 写入 + 回读校验         ← 失败累计；连续失败达阈值则交回官方
//! ⑤ 附加功能调度            ← 与温控**完全独立**，各自出错各自熔断
//! ⑥ 运行快照（节流落盘）
//! ```

use serde_json::{json, Value};
use thermo_ipc::device::EffectiveProfile;

use thermo_core::config::CURR_MAX_UA;
use thermo_core::{Decision, Reason, Sample, State};

use crate::config_store::ConfigStore;
use crate::observability::{kind, EventLog, SnapshotWriter};
use crate::sysfs::{GearTable, Nodes};

/// 连续失败多少次后交回官方（fail-closed）
pub const FAIL_LIMIT: u32 = 5;

/// 档位回读是否算"写入生效"
///
/// # 判据是「**不松于目标**」，不是「等于目标」
///
/// 档位数字的含义：**越大越严**（`0` = 最松/最大电流，表尾 = 最紧）。
///
/// 官方 `MONITOR-BAT` 会在充电中**竞争写**同一节点（真机实测：
/// 模块写 `0` 后读回 `3`/`7`/`11`/`14`，见 `docs/03` §2C.4）。
/// 这种"比目标更严"的覆盖：
///
/// | 回读 | 语义 | 是否算我们的失败 |
/// |---|---|---|
/// | `≥` 目标 | 不比目标松（被官方压得更紧，或就是我们的值） | ❌ 不算 —— 更严对安全无害 |
/// | `<` 目标 | 比目标松（该收紧的没紧上去） | ✅ 算 —— 这才是真没生效 |
///
/// 旧实现用 `==`，于是官方每次覆盖都记一次写失败；连续 5 拍就
/// `release_now` 把**整场充电**交回官方（2026-10-21 真机事件
/// `1792554354~1792554369`：充电开始 34 秒后温控自废）。
fn gear_verified(read_back: i64, target: u8) -> bool {
    read_back >= i64::from(target)
}

/// 采样结果
#[derive(Debug, Clone, PartialEq)]
pub struct SampleOutcome {
    /// 采样值（全部读成功时才有）
    pub sample: Option<Sample>,
    /// 失败原因（读失败时才有）
    pub error: Option<String>,
}

/// 一拍的结果（供测试断言与日志）
#[derive(Debug, Clone, PartialEq)]
pub struct StepReport {
    /// 采样是否成功
    pub sampled: bool,
    /// 决策原因（采样成功时才有）
    pub reason: Option<Reason>,
    /// 是否执行了写入
    pub wrote: bool,
    /// 写入是否回读通过
    pub verified: bool,
    /// 本拍运行的附加功能标识
    pub addons_ran: Vec<String>,
    /// 本拍处理的命令数
    pub commands: usize,
    /// 本拍的错误（人话，供事件流）
    pub errors: Vec<String>,
}

impl StepReport {
    /// 空报告
    pub fn idle() -> Self {
        Self {
            sampled: false,
            reason: None,
            wrote: false,
            verified: false,
            addons_ran: Vec::new(),
            commands: 0,
            errors: Vec::new(),
        }
    }
}

/// 控制引擎
pub struct Engine {
    /// 探测得到的节点
    pub nodes: Nodes,
    /// 配置
    pub cfg: ConfigStore,
    /// 生效档案
    pub profile: EffectiveProfile,
    /// 档位表（设备树读取结果，可能为空）
    pub gear_table: Option<GearTable>,
    /// 温控状态机（跨拍）
    pub state: State,
    /// 附加功能调度器
    pub addons: thermo_addons::runner::AddonRunner,
    /// 附加功能的 I/O（**看门狗包装**，跨拍复用同一个 I/O 线程）
    ///
    /// ⚠️ 为什么必须跨拍复用：看门狗的"挂起"判定是**粘性**的 ——
    /// 一旦某次写入无响应，底层线程就永远卡在 D 状态，
    /// 此后所有调用都必须立即失败，不能重新起线程再试（会不断制造卡死线程）。
    pub addon_io: crate::io_watchdog::WatchedIo,
    /// 事件流
    pub events: EventLog,
    /// 运行快照
    pub snapshot: SnapshotWriter,
    /// 充电历史记录器（**纯观测**，不参与任何决策）
    ///
    /// 见 [`crate::history`] 模块文档：它与 `device.json` 完全无关，
    /// 只是把每次充电的过程曲线归档供界面展示。
    pub history: crate::history::HistoryStore,
    /// 控制通道（Unix 下才有）
    #[cfg(unix)]
    pub server: Option<crate::server::ControlServer>,
    /// 本次运行的起始时刻（单调秒）
    pub started_at_s: i64,
    /// 已完成的拍数
    pub ticks: u64,
    /// 采样连续失败次数
    pub read_failures: u32,
    /// 写入连续失败次数
    pub write_failures: u32,
    /// 是否已交回官方（因连续失败）
    pub released: bool,
    /// **学到的电流能力上限**（μA）——由"写探"得来
    ///
    /// # 为什么需要它（2026-10-20 真机实测）
    ///
    /// 标准 `power_supply` 机型（K70 Pro）上，档案里的
    /// `current.max_ua` 只有 **6 A** —— 那是 `constant_charge_current`
    /// 的**默认值**，不是能力上限。而驱动实际接受 **15.95 A**：
    ///
    /// | 写入 | 驱动实际 | USB 电压 | 电池功率 |
    /// |---|---|---|---|
    /// | 6 A | 6 A | 8.4 V | **23.6 W** |
    /// | 12 A | 12 A | 16.8 V | 31.3 W |
    /// | 20 A | **15.95 A**（钳位） | 17.4 V | **50.3 W** |
    ///
    /// 注意第三行：**电流限制还连带压低了电压协商** —— 电流给小了，
    /// 充电头就只肯给 8 V。所以这个值太小不只是"限流"，而是
    /// 直接把快充能力废掉了。
    ///
    /// # 怎么学
    ///
    /// 温度正常时先写 [`CURR_MAX_UA`]（30 A 物理上限），再读回：
    /// 驱动会把值**钳到真实能力**（实测 15.95 A），把读回值记下来，
    /// 之后就用它当目标 —— 于是每台机器自动得到自己的真实上限，
    /// 不需要任何机型数据。
    ///
    /// ⚠️ 依赖"驱动会钳位"这一前提。这是驱动的硬件保护职责，
    ///    K70 Pro 实测成立（20 A → 15.95 A）。
    pub learned_curr_max: Option<i32>,
    /// **电流通道是否已经放开**（不再限制）
    ///
    /// # 为什么光靠"值不同就写"不够（2026-10-20 真机踩坑）
    ///
    /// 电量接近上限时，驱动会**自己**小幅调整
    /// `constant_charge_current`（实测在 6.0 / 7.5 / 12.3 A 之间变）。
    /// 而 `apply_dual` 的 `write_if_different` 一看"节点值 ≠ 我们的目标"
    /// 就写回去 —— 于是模块和驱动互相追着写，**每 3 秒触发一次
    /// 充电协商重置**。
    ///
    /// 真机现象：
    ///
    /// - `curr_limit` 在 6.00 / 7.50 / 12.30 A 之间跳
    /// - USB 电压在 5 V / 9 V / 15 V / 17 V 之间跳
    /// - 电池电流在 ±0.1 A 抖动（甚至倒流）
    ///
    /// ⇒ 一旦确认"不需要限制"，就写一次放开值并把本标记置位，
    ///   之后**不再碰电流节点** —— 驱动想怎么调由它去。
    ///   需要重新限制时（收紧 / 用户设了更低的功率上限）清掉标记。
    pub curr_opened: bool,
    /// **上一次采样时的充电状态**（缓存）
    ///
    /// # 为什么需要缓存
    ///
    /// worker 主循环要靠它决定"这一轮做什么"（见 `worker` 的三态省电策略）：
    ///
    /// | 状态 | 行为 |
    /// |---|---|
    /// | 充电中 | 全拍（温控闭环） |
    /// | 未充电 + 有客户端 | 轻拍（只响应请求） |
    /// | 未充电 + 无客户端 | **深度休眠**（不跑任何拍） |
    ///
    /// 但"深度休眠"的意义就是**不做采样** —— 若为了判定状态而每轮都采样，
    /// 休眠就失去意义了。因此：**这个值在跑完拍之后更新，休眠期间用缓存**。
    ///
    /// 休眠被唤醒（uevent / 客户端）后会跑一次全拍，届时就刷新了。
    pub last_charging: bool,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("ticks", &self.ticks)
            .field("read_failures", &self.read_failures)
            .field("write_failures", &self.write_failures)
            .field("released", &self.released)
            .finish()
    }
}

/// 引擎的初始输入
///
/// 收成一个结构体而非一长串参数：字段名即语义，新增字段不会改变调用签名，
/// 也避免"第 5 个参数是电流还是电压"这类误读。
#[derive(Debug)]
pub struct EngineInit {
    /// 配置
    pub cfg: ConfigStore,
    /// 生效档案
    pub profile: EffectiveProfile,
    /// 探测得到的节点
    pub nodes: Nodes,
    /// 档位表（设备树读取结果；失败为 `None`）
    pub gear_table: Option<GearTable>,
    /// 附加功能调度器
    pub addons: thermo_addons::runner::AddonRunner,
    /// 事件流
    pub events: EventLog,
    /// 运行快照
    pub snapshot: SnapshotWriter,
    /// 充电历史归档文件的路径
    ///
    /// ⚠️ 之所以从外面传进来（而不是在 `Engine::new` 里写死），
    /// 是为了与 `events` / `snapshot` 保持一致 —— 它们是同样的观测设施，
    /// 都应当可在测试里指向假节点树内的临时文件。
    pub history_path: std::path::PathBuf,
    /// 充电历史草稿文件的路径
    pub history_draft_path: std::path::PathBuf,
    /// 启动时刻（单调秒）
    pub now_s: i64,
}

impl Engine {
    /// 构造引擎（**只做加载与探测，不写任何节点**）
    pub fn new(init: EngineInit) -> Self {
        // ⚠️ 必须在 `init.cfg` 被 move 之前取配置
        let mut history = crate::history::HistoryStore::new(
            init.history_path,
            init.history_draft_path,
            init.cfg.history_cfg(),
        );

        // 上次运行若留下未完成的会话，继续累积（worker 重启 ≠ 充电中断）
        if history.restore_draft() {
            log::info!("充电历史：已恢复上次未完成的会话");
        }

        Self {
            nodes: init.nodes,
            cfg: init.cfg,
            profile: init.profile,
            gear_table: init.gear_table,
            state: State::default(),
            addons: init.addons,
            // 附加功能的 I/O 走看门狗线程：写节点若把内核拖入 D 状态，
            // 卡住的是那个线程，温控主循环毫发无伤（详见 io_watchdog 模块文档）
            addon_io: crate::io_watchdog::WatchedIo::spawn(Box::new(crate::addon_host::SysfsIo)),
            events: init.events,
            snapshot: init.snapshot,
            history,
            #[cfg(unix)]
            server: None,
            started_at_s: init.now_s,
            ticks: 0,
            read_failures: 0,
            write_failures: 0,
            released: false,
            // `None` = 还没探测过 ⇒ 第一次温度正常时会写探一次
            learned_curr_max: None,
            curr_opened: false,
            // 初值为 `false`（未充电）⇒ 启动后第一轮会走"轻拍"，随后
            // 立刻由拍到的一次采样纠正。**刻意不用"启动即跑一拍"**：
            // worker 的启动路径已经显式跑过首拍了。
            last_charging: false,
        }
    }

    /// 内核所需的能力边界（由生效档案转换而来）
    ///
    /// # 🔴 用"学到的电流上限"覆盖档案值（2026-10-20）
    ///
    /// 档案里的 `current.max_ua` 在标准 `power_supply` 机型上只是
    /// `constant_charge_current` 的**默认值**（6 A），不是能力上限。
    /// 而决策层的 `Bounds.curr_max` 就是它 —— 于是：
    ///
    /// | 用户设置 | 结果 |
    /// |---|---|
    /// | 不设功率上限（`Normal`） | 电流取 `curr_max` = 6 A ⇒ 24 W |
    /// | 设了 120 W（`PowerLimit`） | `pcap = min(120W÷4.14V=29A, 6A)` = 6 A ⇒ 还是 24 W |
    ///
    /// 两条路都被这 6 A 卡住 —— 这也解释了为什么用户反馈
    /// 「设不设上限都只有 24W」。
    ///
    /// ⇒ 一旦通过写探学到真实上限（实测 15.95 A），就用它替换档案值，
    ///   让 `power_cap_curr` 的钳位上界变成真实能力。
    pub fn caps(&self) -> thermo_core::Capabilities {
        let mut c = crate::profile_io::to_core_caps(&self.profile.profile.capabilities);
        if let Some(learned) = self.learned_curr_max {
            c.current.max_ua = learned;
        }
        c
    }

    /// 交回官方时用的电流上限（本机实测值；拿不到则用保守值）
    pub fn release_curr_ua(&self) -> i32 {
        let c = &self.profile.profile.capabilities;
        if c.current.max_ua > 0 {
            c.current.max_ua
        } else {
            700_000
        }
    }

    /// 跑一拍
    pub fn step(&mut self, now_s: i64) -> StepReport {
        let mut rep = StepReport::idle();
        self.ticks = self.ticks.saturating_add(1);

        // ① 控制通道（先处理请求，让 status/ctl 及时响应）
        rep.commands = self.poll_server(now_s);
        // 把上一拍产生的事件推给 watch 订阅者
        self.publish_events();

        // ①-b 按配置同步调试模式的**写入器开关**
        //
        // ⚠️ 必须在 `poll_server` **之后**：`ctl debug on` 是通过命令队列
        //    改配置的，放前面会晚一拍才生效。
        //
        // ⚠️ 也必须在**本进程**做（而不是让 CLI 直接开）：
        //    写入器是进程内的静态状态，CLI 开了 worker 也看不见。
        //    worker 是唯一的写者 ⇒ 它自己按配置决定记不记。
        self.sync_debug_mode();

        // ② 采样
        let outcome = self.sample();
        let Some(sample) = outcome.sample else {
            self.read_failures = self.read_failures.saturating_add(1);
            let msg = outcome.error.unwrap_or_else(|| "采样失败".to_string());
            if self.read_failures == 1 || self.read_failures % 10 == 0 {
                self.events.emit(
                    kind::READ_FAIL,
                    &msg,
                    json!({
                        "failures": self.read_failures
                    }),
                );
            }
            rep.errors.push(msg);

            // 采样不可用 ⇒ 仍要跑附加功能（它不依赖温控采样）
            rep.addons_ran = self.tick_addons(now_s);
            self.write_snapshot(now_s, false);
            return rep;
        };
        self.read_failures = 0;
        rep.sampled = true;

        // 记住本拍的充电状态 —— worker 靠它决定下一轮要不要深度休眠
        self.last_charging = sample.status.is_active();

        // ③ 决策（纯函数；配置非法会返回 Err）
        let cfg = match self.cfg.to_core() {
            Ok(c) => c,
            Err(e) => {
                rep.errors.push(e.to_string());
                self.events.emit(kind::CONFIG, &e.to_string(), json!({}));
                rep.addons_ran = self.tick_addons(now_s);
                self.write_snapshot(now_s, false);
                return rep;
            }
        };

        // ── ① 首次充电时做一次**写探**，学习真实电流上限 ──
        //
        // 🔴 **必须在 `caps()` 之前**（2026-10-20 真机踩坑，两版才修对）
        //
        // 第一版把写探放在 `apply()` 里，问题是 `decision` 在那之前
        // **已经用旧的 `caps` 算好了**：
        //
        // ```text
        // decision.curr_ua = 6A   （旧 cap）
        // 随后 apply 里写探 → learned = 30A → cap 变 30A
        // ⇒ need_limit = (6 < 30) = true ⇒ 又写回 6A   ← 前功尽弃
        // ```
        //
        // 所以必须先把 learned 探出来，`caps()` 才会返回真实上限，
        // 决策层算出的 `curr_ua` 才是对的。
        //
        // # 为什么学习不能挂在某个 reason 上
        //
        // 第一版放在 `Reason::Normal` 分支里，而用户的配置是
        // **功率上限 120W** ⇒ 走 `PowerLimit`，学习分支永远进不去，
        // 而 `PowerLimit` 算出的电流又被 `curr_max=6A` 钳住 ——
        // 于是"设了上限"和"没设上限"都是 24W。
        //
        // ⚠️ 依赖"驱动会钳位"（K70 Pro 实测：写 30A → 15.95A / 21.8A）。
        // ⚠️ **只在真正的 `Charging` 时探**（2026-10-20 真机踩坑）：
        //
        // 第一版用的是 `is_active()`，而它把 `Full` 也算"活跃"。
        // 于是满电时也写探了一次，驱动当然只肯给 6 A ⇒ 学到 6 A
        // 并**固定下来**，下次低电量充电时又是 24W。
        //
        // 真机日志就是这么写的：`学到电流能力上限：6 A`。
        //
        // ⇒ 只有 `Charging`（真正在充）才有参考价值：
        //   `Full` / `NotCharging` 时驱动本来就压着电流，探不出能力。
        if self.learned_curr_max.is_none()
            && sample.status == thermo_core::ChargingStatus::Charging
            && self.profile.profile.capabilities.detected
            && !cfg.observe_only
        {
            if let Some(c) = &self.nodes.curr {
                if c.write_i64(i64::from(CURR_MAX_UA)).is_ok() {
                    if let Ok(v) = c.read_i64() {
                        if let Ok(v32) = i32::try_from(v) {
                            if v32 > 0 {
                                log::info!(
                                    "[engine] 学到电流能力上限：{} A（写探 {} A，驱动钳位后）",
                                    f64::from(v32) / 1e6,
                                    f64::from(CURR_MAX_UA) / 1e6
                                );
                                self.learned_curr_max = Some(v32);
                            }
                        }
                    }
                }
            }
        }

        let caps = self.caps();

        let prev_phase = self.state.phase;
        let decision = match thermo_core::decide_with(&sample, &cfg, &self.state, &caps) {
            Ok(d) => d,
            Err(e) => {
                rep.errors.push(e.to_string());
                self.events
                    .emit(kind::CONFIG, &format!("配置校验失败：{e}"), json!({}));
                rep.addons_ran = self.tick_addons(now_s);
                self.write_snapshot(now_s, false);
                return rep;
            }
        };
        rep.reason = Some(decision.reason);

        // ④ 写入
        if decision.should_write && !self.released {
            let (wrote, verified, err) = self.apply(&sample, &decision);
            rep.wrote = wrote;
            rep.verified = verified;
            if let Some(e) = err {
                rep.errors.push(e);
            }
        }

        // 状态迁移（计拍阈值取自**配置**，见 next_state 文档）
        self.state = next_state(&self.state, &sample, &decision, cfg.tighten_temp);

        // 相位变化记为事件（稀疏，有价值）
        if decision.phase != prev_phase {
            self.events.emit(
                kind::STATE,
                // ⚠️ 用 `Phase::label()`（中文）而不是 `{:?}`（Debug）
                //
                // 用户反馈（2026-10-16）：「最近事件内的英文显示全部转换成中文」
                // 根因就是这里原先用了 `{:?}`，界面上显示成 `Normal → Tightening`。
                // 详见 `thermo_core::state::Phase::label` 的文档。
                &format!("{} → {}", prev_phase.label(), decision.phase.label()),
                json!({
                    "from": format!("{:?}", prev_phase).to_lowercase(),
                    "to": format!("{:?}", decision.phase).to_lowercase(),
                    "reason": format!("{:?}", decision.reason),
                    "temp_c": sample.temp_c,
                }),
            );
            if decision.phase == thermo_core::state::Phase::HardProtect {
                self.events.emit(
                    kind::HARD_PROTECT,
                    "触发硬保护",
                    json!({ "temp_c": sample.temp_c, "gear": decision.gear }),
                );
            }
            // 交回官方也只在**迁移进入**时记一次（否则未充电期间每拍都会记）
            if decision.release {
                self.events.emit(
                    kind::RELEASE,
                    "已交回官方控制",
                    json!({ "reason": format!("{:?}", decision.reason) }),
                );
            }
        }

        // ⑤ 附加功能（与温控完全独立）
        rep.addons_ran = self.tick_addons(now_s);

        // ⑥ 充电历史（**纯观测** —— 只读本拍结果并归档，不参与决策）
        //
        //    刻意放在 `publish_events` **之前**：会话开始/归档会产生
        //    lifecycle 事件，这样它们能在本拍就被推给 `watch` 订阅者。
        self.record_history(now_s, &sample, &decision);

        // ⑦ 把本拍产生的事件推给 watch 订阅者
        self.publish_events();

        // ⑧ 调试模式：记下**本拍的完整决策链**
        //
        // 用户要求（2026-10-17）：「开启调试模式会记录更详细的数据，
        // 方便你抓包分析」。
        //
        // 事件流只在**状态变化时**记一条，所以排查"看起来没生效"这类
        // 问题时，中间那几十拍的输入与输出全都看不到 —— 而那恰恰是
        // 判断"算错了"还是"写了没生效"的唯一依据。
        //
        // ⚠️ 放在 `publish_events` **之后**：这样 `dt_ms` 能覆盖本拍
        //    几乎全部工作（含事件推送），更接近真实的单拍耗时。
        self.trace_debug(&sample, &decision);

        // ⑨ 快照
        self.write_snapshot(now_s, false);

        rep
    }

    /// 按配置同步调试模式（每拍调用，**幂等**）
    ///
    /// # 为什么要"每拍同步"而不是"配置变更时同步一次"
    ///
    /// | 场景 | 只做"变更时"会怎样 |
    /// |---|---|
    /// | worker 重启 | 配置里开着，但新进程的写入器是关的 ⇒ **不再记录**，用户以为还在记 |
    /// | 配置被外部改（手改 `config.json`） | 同上 |
    ///
    /// 而每拍同步的成本只是一次配置读 + 一次原子比较，可忽略。
    ///
    /// ⚠️ [`crate::debug_log::enable`] 本身是幂等的（路径相同只更新上限），
    ///    所以每拍调用不会重置计数或清空文件。
    fn sync_debug_mode(&self) {
        let want = self.cfg.get_bool("debug.enabled");
        if want {
            let max_mb = u16::try_from(self.cfg.get_i64("debug.max_mb"))
                .unwrap_or(crate::debug_log::DEFAULT_MAX_MB);
            crate::debug_log::enable(crate::paths::debug_jsonl(), max_mb);
        } else if crate::debug_log::is_enabled() {
            crate::debug_log::disable();
        }
    }

    /// 未开启调试模式时是**空操作**（一次原子读）
    ///
    /// # 记什么、为什么
    ///
    /// 这一行的目标是：**只看它就能判断"为什么最后是那个电流"**。
    ///
    /// | 字段 | 为什么需要 |
    /// |---|---|
    /// | `temp` / `soc` / `volt` / `curr` / `power` | 决策的**全部输入** |
    /// | `phase` / **`reason`** | 决策结果的性质 —— 见下 |
    /// | `pcap_ua` | 功率上限单独算出的电流上限 |
    /// | `final_ua` | 与实际写入值一致（两者取严 + 钳位之后） |
    /// | `gear` | 档位（虽不参与收紧，但要能看到它是否被改） |
    /// | `should_write` / `release` | 本拍到底有没有动作 |
    /// | `peak_c` | 收紧期的峰值温度 |
    ///
    /// # 为什么没有"温度诉求"那一列
    ///
    /// 判断"降速失效"的关键是分清两种完全不同的故障：
    ///
    /// | 故障 | 症状 | 本次能看出吗 |
    /// |---|---|---|
    /// | **算错了** | 控制律本身给出的值就不对 | ✅ `reason = Tighten` |
    /// | **被盖住了** | 控制律对，但功率上限更严 | ✅ `reason = PowerLimit` |
    ///
    /// `Reason` 枚举**本身**就回答了"谁在起作用"这个问题
    /// （`Tighten` / `PowerLimit` / `Normal` / `HardProtect`…），
    /// 再配合 `pcap_ua` 与 `final_ua`，已经够定位。
    ///
    /// 要精确到"温度诉求是 20.81 A"则需要给 `thermo_core::Decision`
    /// 加字段，而那要改 **14 处构造点**、动的是决策内核的公开契约 ——
    /// 为一个诊断字段付这个代价不划算。真到需要时再加。
    fn trace_debug(&self, sample: &thermo_core::Sample, decision: &thermo_core::Decision) {
        if !crate::debug_log::is_enabled() {
            return;
        }
        // ── 外部环境快照（调试模式专用）──
        //
        // 决策链记的都是**模块自己的视角**，而排查 2026-10-17 那次
        // 「充电功率异常」时真正缺的是外部信息：充电器是不是降级了、
        // 屏幕是不是灭的、温控场景有没有切。
        //
        // ⚠️ 全部尽力而为（见 `sysfs::EnvSnapshot`）—— 读不到就是 null，
        //    绝不影响控制。这四个读操作**只在调试模式开启时**发生。
        let env = crate::sysfs::EnvSnapshot::read();

        crate::debug_log::write(
            "trace",
            json!({
                "temp": crate::observability::round_json(f64::from(sample.temp_c), 1),
                "soc": sample.soc,
                "status": format!("{:?}", sample.status),
                "volt_uv": sample.voltage_uv,
                "curr_ua": sample.current_ua,
                "power_w": crate::observability::round_json(f64::from(sample.power_w()), 2),
                "phase": format!("{:?}", decision.phase),
                "reason": format!("{:?}", decision.reason),
                "pcap_ua": decision.power_cap_ua,
                "final_ua": decision.curr_ua,
                "gear": decision.gear,
                "should_write": decision.should_write,
                "release": decision.release,
                "peak_c": self.state.peak_c,
                // ── 外部环境（2026-10-17 新增）──
                //
                // | 字段 | 用途 |
                // |---|---|
                // | `screen` | 用户怀疑「息屏降速」时要能自证/排除 |
                // | `usb_v` / `usb_a` | **一眼看出充电器是否降级**（实测 9V→5V 时功率掉到 1/6） |
                // | `usb_type` | 协议（`USB_PD` / `USB` / `Unknown`） |
                // | `sconfig` | 温控场景编号（实测 `-1` ↔ `0` 时官方策略完全不同） |
                "screen": env.screen_on.map(|on| if on { "on" } else { "off" }),
                "usb_v": env
                    .usb_voltage_uv
                    .map(|v| crate::observability::round_json(v as f64 / 1e6, 3)),
                "usb_a": env
                    .usb_current_ua
                    .map(|v| crate::observability::round_json(v as f64 / 1e6, 3)),
                "usb_type": env.usb_type,
                "sconfig": env.sconfig,
            }),
        );
    }

    /// 把本拍结果记入充电历史（**纯观测**）
    ///
    /// # 与决策的解耦
    ///
    /// 本方法**只读**决策结果，不回写任何东西。`HistoryStore` 内部
    /// 所有 I/O 失败都只记日志（同 `EventLog` 的纪律）——
    /// 观测手段的故障绝不该影响温控。
    ///
    /// # 为什么这么折算"档位/电流"
    ///
    /// | 本拍 | 取值 | 理由 |
    /// |---|---|---|
    /// | 写了节点 | 决策值 | 那就是真正写下去的值 |
    /// | 没写（只观测 / 未检测） | `state` 里的值 | 即节点上**真实存在**的值 |
    ///
    /// 这样曲线上的 `limit_ua` 始终代表"模块此刻施加的限制"，
    /// 而不是"模块想施加但没施加的"。
    fn record_history(
        &mut self,
        now_s: i64,
        sample: &thermo_core::Sample,
        decision: &thermo_core::Decision,
    ) {
        let gear = if decision.should_write {
            decision.gear
        } else {
            self.state.gear
        };
        let limit_ua = if decision.should_write {
            i64::from(decision.curr_ua)
        } else {
            i64::from(self.state.curr_ua)
        };
        let phase = crate::history::phase_str(decision.phase);

        match self.history.observe(now_s, sample, gear, limit_ua, phase) {
            crate::history::ObserveOutcome::Started => self.events.emit(
                kind::LIFECYCLE,
                "开始记录充电会话",
                json!({ "soc": sample.soc, "temp_c": sample.temp_c }),
            ),
            crate::history::ObserveOutcome::Finished(energy) => self.events.emit(
                kind::LIFECYCLE,
                "充电会话已归档",
                json!({ "energy_wh": energy }),
            ),
            crate::history::ObserveOutcome::None | crate::history::ObserveOutcome::Sampled => {}
        }
    }

    /// **轻拍**：只处理控制通道请求，不做采样 / 决策 / 落盘
    ///
    /// # 什么时候用它
    ///
    /// **未充电 + 有客户端**（用户打开着 WebUI 但没插充电器）。
    ///
    /// # 为什么这样是安全的
    ///
    /// | 常规拍的环节 | 未充电时是否需要 | 依据 |
    /// |---|---|---|
    /// | 处理客户端请求 | ✅ **必须** | 否则 `status` 都没人应答 |
    /// | 采样 | ❌ | `ctl status` **自己会实时读节点**（见 `commands::status`） |
    /// | 决策 | ❌ | 未充电时决策恒为"交回官方"，无事可做 |
    /// | 写节点 | ❌ | 同上 |
    /// | 附加功能 | ❌ | SOC 上限只在充电时需要巡检 |
    /// | 充电历史 | ❌ | 只在充电时记录 |
    /// | 快照落盘 | ❌ | 状态没变化，落盘只是白写闪存 |
    ///
    /// # 界面数据会变陈旧吗
    ///
    /// 不会。`ctl status` 是**每次调用都实时采样**的
    /// （`read_battery`），界面每 3 秒拉一次就拿到 3 秒内的真实读数 ——
    /// 与 worker 跑不跑拍无关。
    ///
    /// ⚠️ 唯一"陈旧"的是 `state.gear` / `state.curr_ua` / `ticks`：
    /// 它们停在上一次全拍的值。未充电时这三个本就该是"已交回官方"
    /// 的静止状态，不会误导用户。
    pub fn light_step(&mut self, now_s: i64) -> StepReport {
        let mut rep = StepReport::idle();

        // ① 控制通道 —— 这是轻拍的**唯一目的**
        rep.commands = self.poll_server(now_s);

        // ② 把事件推给 `watch` 订阅者
        //
        // 客户端可能正连着 `watch`（日志页），别让事件卡在缓冲里。
        self.publish_events();

        // ③ 注意：**不增加 `ticks`**
        //
        // `ticks` 的语义是"跑了多少拍控制逻辑"。轻拍没有跑控制逻辑，
        // 若也计数，界面上的"累计 N 拍"会被客户端的轮询灌水。
        rep
    }

    /// 上一次采样时的充电状态（缓存值，见字段文档）
    pub fn is_charging(&self) -> bool {
        self.last_charging
    }

    /// 把待推送事件广播给 `watch` 订阅者
    ///
    /// ⚠️ 消费过慢的客户端由 [`crate::server::ControlServer`] 按背压策略断开，
    /// **绝不阻塞控制循环**。
    #[cfg(unix)]
    pub fn publish_events(&mut self) {
        let events = self.events.drain_recent();
        if events.is_empty() {
            return;
        }
        if let Some(server) = self.server.as_mut() {
            for e in events {
                server.broadcast(&e);
            }
        }
    }

    #[cfg(not(unix))]
    pub fn publish_events(&mut self) {
        // 非 Unix 平台没有控制通道，直接丢弃（仍会落盘）
        let _ = self.events.drain_recent();
    }

    /// ② 采样：读电池观测节点
    ///
    /// ⚠️ 任一必需项读失败即视为采样失败（fail-closed）——
    /// 宁可这一拍不动作，也不基于残缺数据写节点。
    pub fn sample(&self) -> SampleOutcome {
        let Some(temp_node) = &self.nodes.battery_temp else {
            return SampleOutcome {
                sample: None,
                error: Some("电池温度节点不可用".to_string()),
            };
        };
        let Some(status_node) = &self.nodes.battery_status else {
            return SampleOutcome {
                sample: None,
                error: Some("电池状态节点不可用".to_string()),
            };
        };

        let temp_raw = match temp_node.read_i64() {
            Ok(v) => v,
            Err(e) => {
                return SampleOutcome {
                    sample: None,
                    error: Some(format!("读取电池温度失败：{e}")),
                }
            }
        };
        let status_raw = match status_node.read() {
            Ok(v) => v,
            Err(e) => {
                return SampleOutcome {
                    sample: None,
                    error: Some(format!("读取电池状态失败：{e}")),
                }
            }
        };

        // ⚠️ 取**绝对值**：内核的符号约定与直觉相反 —— 真机实测充电中
        //    `current_now` 为负数（如 -9599000），放电中为正数。
        //    统一规范化为"电流大小"，方向由 status 表达（见 Sample 文档）。
        let current_ua = self
            .nodes
            .battery_current
            .as_ref()
            .and_then(|n| n.read_i64().ok())
            .and_then(|v| i32::try_from(v).ok())
            .map(i32::abs)
            .unwrap_or(0);
        let voltage_uv = self
            .nodes
            .battery_voltage
            .as_ref()
            .and_then(|n| n.read_i64().ok())
            .and_then(|v| i32::try_from(v).ok())
            .unwrap_or(0);
        let soc = self
            .nodes
            .battery_capacity
            .as_ref()
            .and_then(|n| n.read_i64().ok())
            .and_then(|v| u8::try_from(v.clamp(0, 100)).ok())
            .unwrap_or(0);
        // 机身温度（可选）：单位兼容与范围校验见 `sysfs::board_temp_to_c`
        //
        // ⚠️ 必须与 `ctl status` 用的是**同一个函数** —— 否则状态页显示的
        //    机身温度会和历史曲线里记的对不上（这类不一致极难察觉）。
        let board_c = self
            .nodes
            .board_temp
            .as_ref()
            .and_then(|n| n.read_i64().ok())
            .and_then(crate::sysfs::board_temp_to_c);

        SampleOutcome {
            sample: Some(Sample {
                temp_c: temp_raw as f32 / 10.0, // 节点单位 0.1℃
                virt_c: board_c,
                current_ua,
                voltage_uv,
                soc,
                status: thermo_core::ChargingStatus::parse(&status_raw),
            }),
            error: None,
        }
    }

    /// ④ 写入并回读校验
    ///
    /// 返回 `(是否写了, 是否校验通过, 错误)`
    fn apply(&mut self, sample: &Sample, decision: &Decision) -> (bool, bool, Option<String>) {
        let prev = (self.state.gear, self.state.curr_ua);
        let _ = sample;

        let result = if decision.release {
            self.curr_opened = false;
            // 交回官方 ⇒ 丢掉学到的电流上限，下次充电重新探
            //
            // # 为什么要清（2026-10-20）
            //
            // 学到的值只对**当时的电量/温度/协议**成立。如果一直留着：
            //
            // · 满电时学到 6 A（驱动本来就在压电流）⇒ 下次低电量还是 24 W
            // · 拔掉换了另一个充电器 ⇒ 能力可能完全不同
            //
            // 而重探的代价只有"插电后写一次电流节点"，可以忽略。
            self.learned_curr_max = None;
            self.nodes
                .release_control(self.caps().gear.open, self.release_curr_ua())
        } else if decision.reason == Reason::HardProtect || decision.reason == Reason::VirtGuard {
            // 硬保护是**故意**收紧，标记必须清掉，否则后续放松时会被"已放开"挡住
            self.curr_opened = false;
            self.nodes
                .apply_hard_protect(decision.gear, decision.curr_ua)
        } else if decision.reason == Reason::Normal || decision.reason == Reason::PowerLimit {
            // ── 判断"到底需不需要限制电流" ──
            //
            // `decision.curr_ua` 已经是决策层算好的目标（`Normal` 时是
            // `curr_max`，`PowerLimit` 时是功率换算后的 pcap）。
            //
            // 如果它 **不小于** 学到的能力上限，说明这个目标等于"不限制"
            // —— 那就只需要写一次放开值，之后不再碰节点。
            //
            // 为什么必须这样（真机实测）：
            //
            // | 做法 | 结果 |
            // |---|---|
            // | 每拍写目标值 | 驱动会自己微调 6.0/7.5/12.3 A，我们追着写 ⇒ **每 3 秒重置协商** |
            // | 写一次放开值后撒手 | 驱动自己稳在某个值，协商稳定 |
            //
            // 而"真需要限制"（用户设了 50W 这种低于能力的上限）时，
            // 每条拍都必须把值压回去 —— 否则驱动会自己冲上去。
            // ⚠️ 基准必须用 `caps().current.max_ua`（决策层实际用的那个值），
            //    不能用 `CURR_MAX_UA`。否则首次运行时两者不一致：
            //    `curr_ua = 6A`（档案值）< `CURR_MAX_UA = 30A` ⇒ 判成
            //    "需要限制" ⇒ 永远写 6A ⇒ **永远学不到真实上限**（死锁）。
            let cap = self.caps().current.max_ua;
            let need_limit = decision.curr_ua < cap;

            if need_limit {
                // 要限制：每次都强制写精确目标
                self.curr_opened = false;
                self.nodes.apply_dual(decision.gear, decision.curr_ua, prev)
            } else if self.curr_opened {
                // 已放开：**完全不碰电流节点**，让驱动自己管
                Ok(())
            } else {
                // 首次放开：写一次物理上限，读回值由 ② 学下来
                //
                // ⚠️ 档位也要一并放到本机最松档（`gear_open`），否则
                //    官方压在 3 档时，电流放到 30A 也会被档位卡在 8A。
                let r = self
                    .nodes
                    .release_control(self.caps().gear.open, CURR_MAX_UA);
                if r.is_ok() {
                    self.curr_opened = true;
                }
                r
            }
        } else {
            self.nodes.apply_dual(decision.gear, decision.curr_ua, prev)
        };

        // ② 🔴 读回值**只用来学一次**，绝不跟着波动走（2026-10-20 真机踩坑）
        //
        // # 为什么"跟随读回值"是错的
        //
        // 上一版让 `learned_curr_max` 每拍跟随读回值，结果变成这样：
        //
        // | 拍 | 我们写 | 驱动给 | 学到 |
        // |---|---|---|---|
        // | 1 | 30 A | 6 A | 6 A |
        // | 2 | 6 A | 7.5 A | 7.5 A |
        // | 3 | 7.5 A | 6 A | 6 A |
        // | … | **来回追，每拍都写** | | |
        //
        // 电量接近上限时，驱动本来就会自己小幅调整
        // `constant_charge_current` —— 模块跟着它跑，等于每 3 秒
        // 触发一次协商重置。真机现象：`curr_limit` 在
        // 6.00 / 7.50 / 12.30 A 之间跳，USB 电压 5/9/15/17 V 乱跳，
        // 电池电流在 ±0.1 A 抖动。
        //
        // ⇒ 只在**还没学到**时记一次（写探的结果），之后固定不动。
        if result.is_ok() && self.learned_curr_max.is_none() {
            if let Some(c) = &self.nodes.curr {
                if let Ok(v) = c.read_i64() {
                    if let Ok(v32) = i32::try_from(v) {
                        if v32 > 0 {
                            log::info!(
                                "[engine] 学到电流能力上限：{} A（写探 {} A，驱动钳位后）",
                                f64::from(v32) / 1e6,
                                f64::from(CURR_MAX_UA) / 1e6
                            );
                            self.learned_curr_max = Some(v32);
                        }
                    }
                }
            }
        }

        if let Err(e) = result {
            self.write_failures = self.write_failures.saturating_add(1);
            let msg = format!("{e:#}");
            self.events.emit(
                kind::WRITE_FAIL,
                &msg,
                json!({ "failures": self.write_failures, "gear": decision.gear }),
            );

            if self.write_failures >= FAIL_LIMIT && !self.released {
                self.release_now("连续写入失败达阈值");
            }
            return (false, false, Some(msg));
        }

        // 回读校验（只校验档位：电流在未充电时读的是能力上限，不可作为判据）
        //
        // ⚠️ 判据见 [`gear_verified`]：**不松于目标**即通过。
        //    官方 `MONITOR-BAT` 会把档位竞争写成更严的值（3/7/11/14），
        //    用 `==` 会把这种常态覆盖误判成写入失败 ⇒ 连续 5 拍
        //    `release_now` ⇒ 整场充电交回官方（真机事故）。
        let read_back = self.nodes.ctrl.as_ref().and_then(|n| n.read_i64().ok());
        let verified = read_back.is_none_or(|v| gear_verified(v, decision.gear));

        if verified {
            self.write_failures = 0;
            // ⚠️ `release` 事件**不在这里**记 —— 见 `step` 里的状态迁移处理。
            //    未充电时每拍都会交回官方（决策优先级 ③），若每拍都记一条，
            //    事件流会被同一件事刷爆（实测空闲两分钟写上百条重复记录）。
        } else {
            self.write_failures = self.write_failures.saturating_add(1);
            // `verified == false` ⇒ `read_back` 必为 `Some`（`None` 走通过）
            let actual = read_back.unwrap_or(-1);
            let msg = format!(
                "写入后档位比目标更松（期望 ≥{}，实得 {}）",
                decision.gear, actual
            );
            self.events.emit(
                kind::WRITE_FAIL,
                &msg,
                json!({
                    "failures": self.write_failures,
                    "gear": decision.gear,
                    "read_back": actual,
                }),
            );
            if self.write_failures >= FAIL_LIMIT && !self.released {
                self.release_now("档位回读连续更松达阈值");
            }
        }

        (true, verified, None)
    }

    /// 立即交回官方（安全出口）
    pub fn release_now(&mut self, reason: &str) {
        if self.released {
            return;
        }
        match self
            .nodes
            .release_control(self.caps().gear.open, self.release_curr_ua())
        {
            Ok(()) => {
                self.released = true;
                self.state.reset();
                self.events.emit(
                    kind::RELEASE,
                    &format!("交回官方：{reason}"),
                    json!({ "reason": reason }),
                );
                log::warn!("已交回官方控制：{reason}");
            }
            Err(e) => {
                self.events.emit(
                    kind::WRITE_FAIL,
                    &format!("交回官方失败：{e:#}"),
                    json!({ "reason": reason }),
                );
            }
        }
    }

    /// ⑤ 附加功能调度
    ///
    /// # ⚠️ I/O 必走看门狗
    ///
    /// 这里**不能**直接 `let mut io = SysfsIo;` —— 真机实测写 `smart_chg`
    /// 会让内核驱动死锁，把调用线程拖进 `D` 状态（不可中断）。
    /// 那样整个温控主循环会一起停摆，违背"附加功能与温控不搅在一起"。
    /// 详见 [`crate::io_watchdog`]。
    fn tick_addons(&mut self, now_s: i64) -> Vec<String> {
        let ran = self.addons.tick_due(&mut self.addon_io, now_s);

        // I/O 被判定挂起 → 记一条醒目事件，并让用户知道该功能已停用
        if let Some(detail) = self.addon_io.take_stuck_detail() {
            self.events.emit(
                kind::ADDON_TRIP,
                "附加功能的节点 I/O 已停用（写入无响应）",
                json!({ "detail": detail, "io_timeout_s": crate::io_watchdog::IO_TIMEOUT.as_secs() }),
            );
            log::error!("{detail}；已停用附加功能的节点写入，温控不受影响");
        }

        // 熔断状态变化 → 记事件（只记一次：由 slot 自己维持 tripped 标志）
        for id in &ran {
            if let Some(slot) = self.addons.slot(id) {
                if slot.is_tripped() {
                    let label = slot.status().label;
                    self.events.emit(
                        kind::ADDON_TRIP,
                        &format!("附加功能「{label}」连续失败已自动停用"),
                        json!({ "id": id, "failures": slot.consecutive_failures() }),
                    );
                }
            }
        }
        ran.iter().map(|s| (*s).to_string()).collect()
    }

    /// ⑥ 运行快照（节流落盘）
    pub fn write_snapshot(&mut self, now_s: i64, force: bool) -> bool {
        let snap = self.snapshot_json(now_s);
        self.snapshot.maybe_write(&snap, now_s, force)
    }

    /// 构造快照 JSON
    ///
    /// # ⚠️ 浮点必须取整（真机踩到过）
    ///
    /// `s.temp_c` / `s.power_w()` 是 `f32`，直接塞进 `json!` 会变成
    /// **`f64` 展开后的长尾**，例如真机实测落盘的：
    ///
    /// ```json
    /// "power_w": 3.582210063934326,
    /// "temp_c": 32.29999923706055
    /// ```
    ///
    /// 这类尾巴有三个害处：
    ///
    /// | 害处 | 说明 |
    /// |---|---|
    /// | 文件变大 | 每个数多 10~15 字符，快照每 5 秒写一次 |
    /// | 界面显示脏 | 谁读这个文件都会看到一串无意义尾数 |
    /// | **浮点相等判断失效** | 用 `temp_c == 32.3` 比较永远为假 |
    ///
    /// 项目统一用 [`observability::round_json_f32`] 取整 ——
    /// `ctl status` 早就这么做了，**但快照漏了**（本函数曾直接输出原值）。
    /// 同一份数据走两条输出路径、只修了一条，是很容易漏的形态。
    pub fn snapshot_json(&self, now_s: i64) -> Value {
        let sample = self.sample().sample;
        let battery = sample.as_ref().map_or_else(
            || json!({ "temp_c": 0.0, "status": "Unknown" }),
            |s| {
                json!({
                    // 温度：0.1℃ 精度足够（与 `ctl status` 保持一致）
                    "temp_c": crate::observability::round_json_f32(s.temp_c, 1),
                    "soc": s.soc,
                    // 功率：0.01W 精度足够（`power_w()` 返回 f32 ⇒ 用 f32 版包装）
                    "power_w": crate::observability::round_json_f32(s.power_w(), 2),
                    "status": format!("{:?}", s.status),
                })
            },
        );

        json!({
            "ts": now_s,
            "state": format!("{:?}", self.state.phase).to_lowercase(),
            "daemon": "running",
            "battery": battery,
            "control": {
                "gear_now": self.state.gear,
                "curr_now": self.state.curr_ua,
            },
            "uptime_s": now_s.saturating_sub(self.started_at_s).max(0),
            "ticks": self.ticks,
            "released": self.released,
        })
    }

    /// ① 控制通道轮询（返回处理的命令数）
    #[cfg(unix)]
    fn poll_server(&mut self, now_s: i64) -> usize {
        // 借用拆分：先把 server 取出来，让 handler 可以借用 self 的其余字段
        let mut server = match self.server.take() {
            Some(s) => s,
            None => return 0,
        };

        let (handled, pending) = {
            let ctx = crate::commands::Ctx {
                cfg: &self.cfg,
                profile: &self.profile,
                nodes: &self.nodes,
                gear_table: self.gear_table.as_ref(),
                state: &self.state,
                ticks: self.ticks,
                started_at_s: self.started_at_s,
                released: self.released,
                addons: &self.addons,
                // 把 worker 内存里的实时历史给命令面 —— 否则
                // `history list` 只能读到最多滞后 10 分钟的草稿快照
                history: &self.history,
                pending: std::cell::RefCell::new(Vec::new()),
                now_s,
            };
            let mut handler = |cmd: &str, args: &Value, stream: bool| {
                crate::commands::dispatch(&ctx, cmd, args, stream)
            };
            let n = server.poll(now_s, &mut handler).len();
            let p = ctx.pending.into_inner();
            (n, p)
        };

        self.server = Some(server);
        self.apply_pending(pending, now_s);
        handled
    }

    #[cfg(not(unix))]
    fn poll_server(&mut self, _now_s: i64) -> usize {
        0
    }

    /// 应用命令面收集到的写操作（**配置只由 worker 写**的单写者纪律）
    pub fn apply_pending(&mut self, pending: Vec<crate::commands::Pending>, now_s: i64) {
        use crate::commands::Pending;

        let mut config_dirty = false;
        for p in pending {
            match p {
                Pending::SetKey { key, value } => match self.cfg.set(&key, &value) {
                    Ok(()) => config_dirty = true,
                    Err(e) => self.events.emit(
                        kind::CONFIG,
                        &format!("配置写入被拒：{e}"),
                        json!({ "key": key, "value": value }),
                    ),
                },
                Pending::SetBatch(pairs) => match self.cfg.set_batch(&pairs) {
                    Ok(()) => config_dirty = true,
                    Err(e) => self.events.emit(
                        kind::CONFIG,
                        &format!("批量写入被拒（整批未生效）：{e}"),
                        json!({ "count": pairs.len() }),
                    ),
                },
                Pending::Reload => {
                    let path = self.cfg.path().to_path_buf();
                    let fresh = crate::config_store::ConfigStore::load(&path);
                    self.cfg = fresh;
                    config_dirty = true;
                }
                Pending::AddonRecover(id) => {
                    if let Some(slot) = self.addons.slot_mut(&id) {
                        let was = slot.recover();
                        self.events.emit(
                            kind::ADDON,
                            &format!("附加功能 {id} 熔断已解除"),
                            json!({ "id": id, "was_tripped": was }),
                        );
                    }
                }
                Pending::SwitchMode(id) => match self.cfg.switch_mode(&id) {
                    Ok((from, to)) => {
                        config_dirty = true;
                        // ⚠️ 消息里用**中文模式名**，不能用 id
                        //
                        // 用户看到的原先是「模式切换：custom → conservative」——
                        // 那两个词对用户毫无意义（用户反馈 2026-10-16：
                        // 「最近事件内的英文显示全部转换成中文」）。
                        // `data` 里仍保留 id，供程序化解析。
                        self.events.emit(
                            kind::CONFIG,
                            &format!(
                                "模式切换：{} → {}",
                                crate::config_store::mode_label(&from),
                                crate::config_store::mode_label(&to)
                            ),
                            json!({ "from": from, "to": to }),
                        );
                    }
                    Err(e) => self.events.emit(
                        kind::CONFIG,
                        &format!("模式切换被拒：{e}"),
                        json!({ "mode": id }),
                    ),
                },
                Pending::HistoryClear => match self.history.clear() {
                    Ok(n) => self.events.emit(
                        kind::CONFIG,
                        &format!("充电历史已清空（{n} 条记录）"),
                        json!({ "removed": n }),
                    ),
                    Err(e) => self.events.emit(
                        kind::CONFIG,
                        &format!("清空充电历史失败：{e:#}"),
                        json!({}),
                    ),
                },
                // ── 清空全部记录 ──
                //
                // 🔴 顺序很重要，见下方注释。
                Pending::RecordsClear => {
                    // ① 充电历史 —— 复用 `HistoryStore::clear()`
                    //
                    // ⚠️ 它**刻意保留**正在进行的会话草稿：用户点"清空"是清理
                    //    旧记录，不该顺手毁掉他此刻正在观察的这一条。
                    let (hist_n, hist_err) = match self.history.clear() {
                        Ok(n) => (n, None),
                        Err(e) => (0, Some(format!("{e:#}"))),
                    };

                    // ② 其余记录文件（诊断报告 / 打点 / 开机记录）
                    //
                    // 清单来自 `paths::clearable_records()` —— **唯一真源**，
                    // 界面弹窗里展示的文案由同一份数据生成，不可能不一致。
                    let mut files_removed = 0_usize;
                    for rec in crate::paths::clearable_records() {
                        if std::fs::remove_file(&rec.path).is_ok() {
                            files_removed += 1;
                        }
                    }

                    // ③ 事件流 —— **必须放最后**
                    //
                    // 因为它会连同磁盘上的事件一起清掉；而下面第 ④ 步要
                    // **紧接着补记一条**，所以这里不能提前 emit 任何东西
                    // （否则那条也会被自己清掉）。
                    let ev_removed = self.events.clear();

                    // ④ 补记一条"已清空"
                    //
                    // 不补的话，用户点完只看一片空白，无法确认操作**是否发生过**、
                    // 也无法知道"清了多少"。这条会写进刚清空的文件，成为新起点。
                    self.events.emit(
                        kind::CONFIG,
                        &format!(
                            "已清空全部记录（历史 {hist_n} 条 · 事件 {ev_removed} 个文件 · \
                             其它 {files_removed} 个文件）"
                        ),
                        json!({
                            "history_sessions": hist_n,
                            "event_files": ev_removed,
                            "other_files": files_removed,
                        }),
                    );

                    // ⑤ 历史清空失败要**明确报出来**
                    //
                    // 其余几步都是"尽力而为"，唯独历史失败会让用户以为
                    // 全清干净了、实际旧记录还在。再补一条错误事件。
                    if let Some(e) = hist_err {
                        self.events.emit(
                            kind::CONFIG,
                            &format!("清空充电历史失败（其余记录已清）：{e}"),
                            json!({}),
                        );
                    }
                }
            }
        }

        if config_dirty {
            // ① 落盘（原子写）
            if let Err(e) = self.cfg.save() {
                self.events
                    .emit(kind::CONFIG, &format!("配置落盘失败：{e:#}"), json!({}));
            }
            // ② 把新配置推给附加功能实例（下一拍生效）
            crate::addon_host::reload_config(&mut self.addons, &self.cfg);
            // ③ 记录版本号，供前端检测"另一端改了配置"
            self.events.emit(
                kind::CONFIG,
                "配置已更新",
                json!({ "cfg_ver": self.cfg.ver }),
            );
        }

        let _ = now_s;
    }
}

/// 由本拍结果推出下一拍的状态
///
/// ⚠️ 内核是纯函数、**不修改**传入的 `State`；跨拍状态的维护是 I/O 层的职责
/// （见 `thermo-core/src/state.rs` 文档）。
///
/// # 参数 `tighten_temp`（**必须来自配置**）
///
/// `over_temp_ticks` 是内核 `delay_s` 门禁的输入：只有它累积到 `delay_s`
/// 才会真正收紧。**判据必须是用户配置的收紧温度**。
///
/// 真机踩过的坑：这里曾硬编码 `43.0`，导致
///
/// | 用户设的阈值 | 后果 |
/// |---|---|
/// | **低于** 43.0（如 38） | 计拍永不累加 ⇒ 永远停在 `DelayPending`，**永不收紧** |
/// | **高于** 43.0 | 计拍提前开始累加 ⇒ 实际延迟比配置的短 |
///
/// 因此阈值必须作为参数传入，不得写死。
fn next_state(prev: &State, sample: &Sample, d: &Decision, tighten_temp: f32) -> State {
    let mut s = prev.clone();
    s.phase = d.phase;
    s.gear = d.gear;
    s.curr_ua = d.curr_ua;
    s.tightening = matches!(
        d.phase,
        thermo_core::state::Phase::Tightening | thermo_core::state::Phase::HardProtect
    );

    // 功率闭环状态：由决策算好、这里落盘
    //
    // ⚠️ 必须跨拍保留 —— 功率闭环是**比例校正**，每拍基于上一拍的
    //    设定值按实测误差调整。丢了它就永远只能靠前馈公式，
    //    误差无法收敛（详见 `decision::power_cap_curr` 的说明）。
    s.power_cap_ua = d.power_cap_ua;

    // 超温计拍：用于 delay_s 门禁（阈值为**配置值**，不得写死）
    if sample.status.is_active() && sample.temp_c >= tighten_temp {
        s.over_temp_ticks = s.over_temp_ticks.saturating_add(1);
    } else {
        s.over_temp_ticks = 0;
    }

    // 峰值温度：仅收紧期内记录
    if s.tightening {
        s.peak_c = Some(match s.peak_c {
            Some(p) if p > sample.temp_c => p,
            _ => sample.temp_c,
        });
    } else {
        s.peak_c = None;
    }

    s
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn 下一拍状态跟随决策() {
        let prev = State::default();
        let sample = Sample {
            temp_c: 44.0,
            virt_c: None,
            current_ua: 1_000_000,
            voltage_uv: 4_000_000,
            soc: 50,
            status: thermo_core::ChargingStatus::Charging,
        };
        let d = Decision {
            should_write: true,
            gear: 12,
            curr_ua: 500_000,
            release: false,
            reason: Reason::Tighten,
            phase: thermo_core::state::Phase::Tightening,
            power_cap_ua: None,
        };
        let next = next_state(&prev, &sample, &d, 43.0);
        assert_eq!(next.gear, 12);
        assert_eq!(next.curr_ua, 500_000);
        assert!(next.tightening);
        assert_eq!(next.peak_c, Some(44.0));
        assert!(next.over_temp_ticks > 0);
    }

    // ────────────────────────────────────────────────────────────
    //  计拍阈值必须跟随配置
    //
    //  真机踩过的坑：这里曾硬编码 43.0，于是当用户把阈值设成 38 时，
    //  40.5℃ 的温度永远累加不到 delay_s，**永远卡在延迟门禁、永不收紧**。
    // ────────────────────────────────────────────────────────────

    #[test]
    fn 计拍阈值低于四十三时仍能累加() {
        let prev = State::default();
        let sample = Sample {
            temp_c: 40.5, // 高于 38 的配置阈值，但低于旧的硬编码 43
            virt_c: None,
            current_ua: 5_000_000,
            voltage_uv: 4_000_000,
            soc: 50,
            status: thermo_core::ChargingStatus::Charging,
        };
        let d = Decision {
            should_write: false,
            gear: 0,
            curr_ua: 5_000_000,
            release: false,
            reason: Reason::DelayPending,
            phase: thermo_core::state::Phase::Normal,
            power_cap_ua: None,
        };

        // 配置阈值 38 ⇒ 40.5 属于"超温"，应当计拍
        let next = next_state(&prev, &sample, &d, 38.0);
        assert_eq!(
            next.over_temp_ticks, 1,
            "阈值 38 时 40.5℃ 应被计入超温拍数（否则永远触发不了收紧）"
        );

        // 配置阈值 45 ⇒ 40.5 不超温，不应计拍
        let next = next_state(&prev, &sample, &d, 45.0);
        assert_eq!(next.over_temp_ticks, 0, "阈值 45 时 40.5℃ 不算超温");
    }

    #[test]
    fn 计拍阈值高于四十三时不会提前累加() {
        let prev = State::default();
        let sample = Sample {
            temp_c: 44.0, // 高于旧硬编码的 43，但低于配置的 45
            virt_c: None,
            current_ua: 5_000_000,
            voltage_uv: 4_000_000,
            soc: 50,
            status: thermo_core::ChargingStatus::Charging,
        };
        let d = Decision {
            should_write: false,
            gear: 0,
            curr_ua: 5_000_000,
            release: false,
            reason: Reason::DelayPending,
            phase: thermo_core::state::Phase::Normal,
            power_cap_ua: None,
        };
        let next = next_state(&prev, &sample, &d, 45.0);
        assert_eq!(
            next.over_temp_ticks, 0,
            "阈值 45 时 44℃ 不该计拍（否则实际延迟会短于配置）"
        );
    }

    #[test]
    fn 未收紧时不记峰值() {
        let prev = State::default();
        let sample = Sample {
            temp_c: 30.0,
            virt_c: None,
            current_ua: 0,
            voltage_uv: 4_000_000,
            soc: 50,
            status: thermo_core::ChargingStatus::Charging,
        };
        let d = Decision {
            should_write: true,
            gear: 0,
            curr_ua: 18_400_000,
            release: false,
            reason: Reason::Normal,
            phase: thermo_core::state::Phase::Normal,
            power_cap_ua: None,
        };
        let next = next_state(&prev, &sample, &d, 43.0);
        assert_eq!(next.peak_c, None, "非收紧期不该有峰值");
        assert_eq!(next.over_temp_ticks, 0);
    }

    #[test]
    fn 未充电时超温计拍归零() {
        let prev = State {
            over_temp_ticks: 10,
            ..State::default()
        };
        let sample = Sample {
            temp_c: 50.0,
            virt_c: None,
            current_ua: 0,
            voltage_uv: 4_000_000,
            soc: 50,
            status: thermo_core::ChargingStatus::Discharging,
        };
        let d = Decision {
            should_write: true,
            gear: 0,
            curr_ua: 18_400_000,
            release: true,
            reason: Reason::NotCharging,
            phase: thermo_core::state::Phase::Idle,
            power_cap_ua: None,
        };
        assert_eq!(next_state(&prev, &sample, &d, 43.0).over_temp_ticks, 0);
    }

    #[test]
    fn 观测模块可用性判断() {
        let nodes = Nodes::default();
        assert!(!nodes.can_control(), "空节点集不可控制");
        assert!(!nodes.can_observe(), "空节点集不可观测");
        assert!(!nodes.can_limit_soc(), "空节点集不支持 SOC 上限");
        assert_eq!(nodes.missing_required().len(), 4, "应报出 4 项必需节点缺失");
    }

    // ============================================================
    //  端到端：假节点树上的完整链路
    //
    //  这一组测试跑的是"真代码 + 假文件"，覆盖：
    //    运行期节点探测 → 档位表解析 → 采样 → 决策 → 钳位 → 写入 → 回读
    //  因此不需要真机、不需要 root，在任意平台（含 Windows）都能跑。
    //
    //  ⚠️ 必须**串行执行**：`THERMO_SYSFS_ROOT` 是进程级环境变量，
    //     并行跑会让几个测试互相覆盖对方的假节点树
    //     （cargo test 默认多线程）。因此用一把互斥锁把整组锁起来。
    // ============================================================

    /// 端到端测试的串行锁
    static E2E_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 持锁运行，并保证退出时清掉环境变量（含 panic 路径）
    fn with_fake_root<T>(root: &std::path::Path, f: impl FnOnce() -> T) -> T {
        let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("THERMO_SYSFS_ROOT", root);

        struct Cleanup;
        impl Drop for Cleanup {
            fn drop(&mut self) {
                std::env::remove_var("THERMO_SYSFS_ROOT");
            }
        }
        let _cleanup = Cleanup;

        f()
    }

    /// 一棵可控的假节点树
    struct FakeSysfs {
        root: std::path::PathBuf,
    }

    impl FakeSysfs {
        /// 造出与真机同构的节点树（路径与 `sysfs.rs` 的候选表一致）
        fn new(tag: &str) -> Self {
            let mut root = std::env::temp_dir();
            root.push(format!("thermo_e2e_{tag}_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);

            let dirs = [
                // 温控控制节点组（必须同目录）
                "sys/class/xm_power/charger/charger_thermal",
                // SOC 上限节点（独立目录）
                "sys/class/xm_power/charger/smart_charge",
                // 电池观测
                "sys/class/power_supply/battery",
                // 档位表（设备树）
                "sys/firmware/devicetree/base/soc/mca_charger_thermal",
                // 场景
                "sys/class/thermal/thermal_message",
            ];
            for d in dirs {
                std::fs::create_dir_all(root.join(d)).unwrap();
            }

            let s = Self { root };
            // 初始状态：未限速、充电中、温度 30℃
            s.write(
                "sys/class/xm_power/charger/charger_thermal/wired_ctrl_limit",
                "0",
            );
            s.write(
                "sys/class/xm_power/charger/charger_thermal/wired_chg_curr",
                "18400000",
            );
            s.write(
                "sys/class/xm_power/charger/charger_thermal/wired_chg_curr2",
                "18400000",
            );
            s.write(
                "sys/class/xm_power/charger/charger_thermal/wired_thermal_remove",
                "0",
            );
            s.write("sys/class/xm_power/charger/smart_charge/smart_chg", "80");
            s.write("sys/class/xm_power/charger/smart_charge/soc_limit", "80");
            s.write("sys/class/power_supply/battery/temp", "300");
            s.write("sys/class/power_supply/battery/status", "Charging");
            s.write("sys/class/power_supply/battery/capacity", "50");
            s.write("sys/class/power_supply/battery/current_now", "2000000");
            s.write("sys/class/power_supply/battery/voltage_now", "4000000");
            s.write("sys/class/thermal/thermal_message/sconfig", "0");
            s.write_gear_table();
            s
        }

        fn path(&self, rel: &str) -> std::path::PathBuf {
            self.root.join(rel)
        }

        fn write(&self, rel: &str, value: &str) {
            std::fs::write(self.path(rel), value.as_bytes()).unwrap();
        }

        fn read(&self, rel: &str) -> String {
            std::fs::read_to_string(self.path(rel))
                .unwrap()
                .trim()
                .to_string()
        }

        /// 造一张 15 档的档位表（每档 10 个 32 位大端整数，第 5 个字段为电流上限 mA）
        fn write_gear_table(&self) {
            const LIMITS_MA: [u32; 15] = [
                13500, 11000, 9000, 8000, 7000, 6000, 5000, 4000, 3500, 3000, 2500, 2000, 1500,
                1000, 500,
            ];
            let mut bytes = Vec::new();
            for ma in LIMITS_MA {
                for field in 0..10usize {
                    let v = if field == 4 { ma } else { 0 };
                    bytes.extend_from_slice(&v.to_be_bytes());
                }
            }
            std::fs::write(
                self.path("sys/firmware/devicetree/base/soc/mca_charger_thermal/wired_thermal"),
                bytes,
            )
            .unwrap();
        }
    }

    impl Drop for FakeSysfs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// 在假节点树上组装引擎
    ///
    /// ⚠️ 调用方必须已持有 [`E2E_LOCK`]（内部通过 [`with_fake_root`] 保证）。
    fn engine_on(fake: &FakeSysfs) -> Engine {
        engine_on_with_curr(fake, None)
    }

    /// 构造引擎，可指定档案里的**电流能力上限**（μA）
    ///
    /// `None` = 用通用档案的默认值（`CURR_MAX_UA`）。
    ///
    /// # 为什么需要可调（2026-10-20）
    ///
    /// 真机（标准 `power_supply` 机型）上档案的 `current.max_ua` 只有
    /// **6A**（`constant_charge_current` 的默认值），而驱动实际允许 12A+。
    /// "只放宽不收紧"这条行为**只有在 `driven > 档案值` 时才看得出来** ——
    /// 用默认的 30A 档案测不出来。
    fn engine_on_with_curr(fake: &FakeSysfs, curr_max: Option<i32>) -> Engine {
        let nodes = Nodes::detect(None);
        let (table, _path) = GearTable::detect().expect("假档位表应当可解析");

        let events = EventLog::new(fake.path("events.jsonl"), 512);
        let snapshot = SnapshotWriter::new(fake.path("runtime.json"));

        let cfg = ConfigStore::load(fake.path("config.json"));

        // ⚠️ 标记"已检测"
        //
        // 决策层的门禁 0 会拒绝一切"未检测"状态的写入（用户要求：
        // 未跑检测不得有任何控制行为）。这些 E2E 测试要验证的是
        // **检测之后**的控制逻辑，因此必须把测试档案标为已检测。
        //
        // 专门验证门禁本身的测试见 `端到端_未检测时零写入`。
        let mut p = crate::profile_io::generic_profile(Default::default());
        p.capabilities.detected = true;
        if let Some(c) = curr_max {
            p.capabilities.current.max_ua = c;
        }
        let profile = thermo_ipc::device::EffectiveProfile {
            profile: p,
            source: thermo_ipc::device::ProfileSource::Generic,
        };

        let (slots, _) = thermo_addons::registry::build_all(cfg.raw());
        let addons = thermo_addons::runner::AddonRunner::new(slots);

        Engine::new(EngineInit {
            cfg,
            profile,
            nodes,
            gear_table: Some(table),
            addons,
            events,
            snapshot,
            history_path: fake.path("history.json"),
            history_draft_path: fake.path("history.current.jsonl"),
            now_s: 1000,
        })
    }

    #[test]
    fn 端到端_运行期探测能命中假节点树() {
        let fake = FakeSysfs::new("detect");
        with_fake_root(&fake.root, || {
            let nodes = Nodes::detect(None);
            assert!(nodes.can_control(), "档位与电流节点应被探测到");
            assert!(nodes.can_observe(), "电池观测节点应被探测到");
            assert!(nodes.can_limit_soc(), "SOC 上限节点应被探测到");
            assert!(nodes.missing_required().is_empty());

            // SOC 读取节点应取"控制节点同目录"的 soc_limit
            let read = nodes
                .soc_read
                .as_ref()
                .map(crate::sysfs::Node::path_str)
                .unwrap_or_default();
            assert!(
                read.ends_with("smart_charge/soc_limit"),
                "读取节点应取同目录的 soc_limit，实得 {read}"
            );

            // 档位数由表长度决定（此处 15），且最大功率档位是电流上限最大者
            let (table, _) = GearTable::detect().unwrap();
            assert_eq!(table.len(), 15);
            assert_eq!(table.max_power_gear(), Some(0));
            assert_eq!(table.current_limit_ua(0), Some(13_500_000));
        });
    }

    /// 标准 `power_supply` 框架的假节点树（2026-10-19 真机那一类）
    ///
    /// 与 [`FakeSysfs`] 的关键区别：**没有** `xm_power` 封装层，
    /// **没有**档位表，**没有** `soc_limit`。全部能力都由电池节点自己提供：
    ///
    /// | 能力 | 节点 |
    /// |---|---|
    /// | 档位 | `battery/charge_control_limit`（0~16） |
    /// | 档位上限 | `battery/charge_control_limit_max` |
    /// | 电流 | `battery/constant_charge_current`（μA） |
    /// | SOC 上限 | `battery/charge_control_end_threshold`（百分比） |
    fn make_standard_power_supply_tree(tag: &str) -> std::path::PathBuf {
        let mut root = std::env::temp_dir();
        root.push(format!("thermo_std_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        const DIR: &str = "sys/class/power_supply/battery";
        std::fs::create_dir_all(root.join(DIR)).unwrap();
        let w = |name: &str, v: &str| {
            std::fs::write(root.join(format!("{DIR}/{name}")), v.as_bytes()).unwrap();
        };
        w("charge_control_limit", "0");
        w("charge_control_limit_max", "16");
        w("constant_charge_current", "18400000");
        w("constant_charge_current_max", "18400000");
        w("charge_control_end_threshold", "100");
        w("temp", "300");
        w("status", "Charging");
        w("capacity", "50");
        w("current_now", "2000000");
        w("voltage_now", "4000000");
        root
    }

    /// 标准框架的节点必须被认作"可控制 / 可限制 SOC"
    ///
    /// # 这一条守的是 2026-10-19 的两个改动
    ///
    /// | # | 曾经的问题 | 表现 |
    /// |---|---|---|
    /// | 1 | 控制节点组只认 `wired_ctrl_limit` / `wired_chg_curr` | 标准框架机型整个"控速节点不可用"，档位/电流全测不到 |
    /// | 2 | `soc_read` 无条件拼同目录的 `soc_limit` | 拿到一条**不存在的路径**；`ctl nodes` 显示已映射，实际每次读都失败 |
    #[test]
    fn 标准power_supply框架的节点也能被探测到() {
        let root = make_standard_power_supply_tree("nodes");
        with_fake_root(&root, || {
            let nodes = Nodes::detect(None);

            assert!(
                nodes.can_control(),
                "标准框架的 charge_control_limit / constant_charge_current 应被认作控制节点组"
            );
            assert!(
                nodes
                    .ctrl
                    .as_ref()
                    .is_some_and(|n| n.path_str().ends_with("charge_control_limit")),
                "档位节点应命中 charge_control_limit"
            );
            assert!(
                nodes
                    .curr
                    .as_ref()
                    .is_some_and(|n| n.path_str().ends_with("constant_charge_current")),
                "电流节点应命中 constant_charge_current"
            );

            // SOC 上限：标准接口下**控制节点自己就是生效值**
            assert!(nodes.can_limit_soc(), "标准框架应判定为支持 SOC 上限");
            let read = nodes
                .soc_read
                .as_ref()
                .map(crate::sysfs::Node::path_str)
                .unwrap_or_default();
            assert!(
                read.ends_with("charge_control_end_threshold"),
                "标准框架的生效值节点应是控制节点自身（而不是不存在的 soc_limit），实得 {read}"
            );
        });
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn 端到端_延迟门禁期间不写节点() {
        let fake = FakeSysfs::new("delay");
        // 把温度抬到收紧阈值（默认 43.0℃）之上
        fake.write("sys/class/power_supply/battery/temp", "430");

        with_fake_root(&fake.root, || {
            let mut engine = engine_on(&fake);

            // delay_s 默认 5 秒：前 5 拍应当"保持现状、不写"
            for i in 0..5 {
                let rep = engine.step(1000 + i);
                assert!(rep.sampled, "第 {i} 拍应采样成功");
                assert!(!rep.wrote, "第 {i} 拍处于延迟门禁期，不该写节点");
            }
            assert_eq!(
                fake.read("sys/class/xm_power/charger/charger_thermal/wired_ctrl_limit"),
                "0",
                "延迟期内档位不得被改动"
            );
        });
    }

    #[test]
    fn 端到端_超过延迟后按线性过渡收紧并双写() {
        let fake = FakeSysfs::new("tighten");
        // 44.0℃ —— 落在过渡区中点（默认 tighten=43、ramp_span=2 ⇒ 43~45）
        fake.write("sys/class/power_supply/battery/temp", "440");

        with_fake_root(&fake.root, || {
            let mut engine = engine_on(&fake);

            let mut wrote = false;
            for i in 0..12 {
                if engine.step(1000 + i).wrote {
                    wrote = true;
                    break;
                }
            }
            assert!(wrote, "延迟过后应当执行写入");

            // 契约：档位与电流是两层并联，**必须同时写**
            let gear = fake.read("sys/class/xm_power/charger/charger_thermal/wired_ctrl_limit");
            let curr = fake.read("sys/class/xm_power/charger/charger_thermal/wired_chg_curr");
            // ⚠️ 与**档案上限**比较，不能写死 18400000
            //    （测试档案的 `current.max_ua` = CURR_MAX_UA = 30000000；
            //     写死旧值会让这条断言恒真、失去意义 —— 真机踩过同类问题）
            assert_ne!(curr, "30000000", "电流层应被收紧（不能只改档位）");
            assert!(gear.parse::<u32>().is_ok(), "档位必须是整数，实得 {gear}");

            // 默认律是 `ramp`（线性过渡），44.0℃ 处于 43~45 区间的中点：
            //
            //   ratio = (44 - 43) / (45 - 43) = 0.5
            //   上限  = 30000000（测试档案的 `current.max_ua` = CURR_MAX_UA）
            //   目标  = 出厂默认 `tighten_power_w` 换算后的电流
            //         = 3 W ÷ 4.0 V = 0.75 A ⇒ 750000 μA
            //   v = 30000000 - (30000000 - 750000) × 0.5 = 15375000
            //
            // ⚠️ 此处曾断言 `5000000`（旧 `curve` 律的段值）。改成 `ramp` 后
            //    界面上「降到多少」那个滑块才真正生效 —— 见 `ControlLaw` 文档。
            //
            // ⚠️ 也曾是 `15350000` —— 那时「降到多少」是 **0.7 A** 的电流默认值。
            //    改成功率后默认值是 **3 W**，在 4.0 V 下等于 0.75 A ⇒ 结果随之变化。
            //    **这是预期的**，不是回归。
            assert_eq!(
                curr, "15375000",
                "44.0℃ 应取线性过渡的中点值（上限与目标的中间）"
            );
        });
    }

    /// 🔴 **回归守卫**：温度正常时写**物理上限**，让驱动自己钳位
    ///
    /// # 真机证据（2026-10-20，K70 Pro + 第三方 PPS 头）
    ///
    /// | 写入电流 | 驱动实际接受 | USB 电压 | 电池功率 |
    /// |---|---|---|---|
    /// | 6 A（档案值） | 6 A | 8.4 V | **23.6 W** |
    /// | 12 A | 12 A | 16.8 V | 31.3 W |
    /// | 20 A | **15.95 A**（钳位） | 17.4 V | **50.3 W** |
    ///
    /// 档案里的 `curr_max`（6 A）是 `constant_charge_current` 的
    /// **默认值**，不是能力上限。照着写会把功率钉死在 24W —— 而且
    /// **电流给小了，充电头连电压都不肯上**（8.4 V vs 17.4 V）。
    ///
    /// ⇒ 温度正常时写 `CURR_MAX_UA`（30 A 物理上限），驱动会钳到
    ///   真实能力，模块再读回记住（见 `Engine::learned_curr_max`）。
    #[test]
    fn 端到端_温度正常时写物理上限让驱动钳位() {
        let fake = FakeSysfs::new("relax_only");

        with_fake_root(&fake.root, || {
            // 复现真机：档案里只有 6A（那只是个默认值）
            let mut engine = engine_on_with_curr(&fake, Some(6_000_000));

            let rep = engine.step(1000);
            assert!(rep.sampled, "应采样成功");

            let curr = fake.read("sys/class/xm_power/charger/charger_thermal/wired_chg_curr");
            assert_eq!(
                curr, "30000000",
                "温度正常时应写物理上限让驱动钳位，而不是照抄档案里的 6A —— \
                 否则功率会被钉死在 24W（真机实测 50W → 24W）"
            );
        });
    }

    /// 学到的上限会被记住，后续不再反复写物理上限
    ///
    /// 假节点树没有真实驱动，写入即生效，所以"驱动钳位后"读回就是 30A
    /// —— 这正好用来验证"学到之后走学到的值"这条路径不再变。
    #[test]
    fn 端到端_学到上限后保持一致() {
        let fake = FakeSysfs::new("learned");

        with_fake_root(&fake.root, || {
            let mut engine = engine_on_with_curr(&fake, Some(6_000_000));

            engine.step(1000);
            let first = fake.read("sys/class/xm_power/charger/charger_thermal/wired_chg_curr");
            assert_eq!(first, "30000000");

            // 再跑几拍：不应被改回档案值
            for i in 1..4 {
                engine.step(1000 + i);
            }
            let later = fake.read("sys/class/xm_power/charger/charger_thermal/wired_chg_curr");
            assert_eq!(later, "30000000", "学到的上限必须保持，不能退回 6A");
        });
    }

    /// 🔴 **回归守卫**：已放开之后，值一致就**绝不重复写**
    ///
    /// # 真机踩坑（2026-10-20，K70 Pro）
    ///
    /// 上一版每拍都按"值不同就写"处理，而电量接近上限时驱动会**自己**
    /// 小幅调整 `constant_charge_current`（实测 6.0 / 7.5 / 12.3 A 之间变）
    /// —— 模块跟着追，就变成每 3 秒写一次，每次都触发充电协商重置。
    ///
    /// 真机现象：
    ///
    /// - `curr_limit` 在 6.00 / 7.50 / 12.30 A 之间跳
    /// - USB 电压在 5 V / 9 V / 15 V / 17 V 之间跳
    /// - 电池电流在 ±0.1 A 抖动（甚至倒流）
    ///
    /// ⇒ 放开一次之后就不再碰电流节点，让驱动自己管。
    #[test]
    fn 端到端_已放开后不再重复写() {
        let fake = FakeSysfs::new("opened");

        with_fake_root(&fake.root, || {
            let mut engine = engine_on_with_curr(&fake, Some(6_000_000));

            // 模拟"已放开 + 已学到真机实测的 12.3A"
            engine.learned_curr_max = Some(12_300_000);
            engine.curr_opened = true;
            fake.write(
                "sys/class/xm_power/charger/charger_thermal/wired_chg_curr",
                "12300000",
            );

            // 多跑几拍：任何一次重写都会触发协商重置
            for i in 0..5 {
                engine.step(1000 + i);
                assert_eq!(
                    fake.read("sys/class/xm_power/charger/charger_thermal/wired_chg_curr"),
                    "12300000",
                    "第 {i} 拍：已放开后不该再写电流节点"
                );
            }
        });
    }

    #[test]
    fn 端到端_硬保护收紧到最紧档() {
        let fake = FakeSysfs::new("hard");
        // 45.0℃ = 模块硬上限
        fake.write("sys/class/power_supply/battery/temp", "450");

        with_fake_root(&fake.root, || {
            let mut engine = engine_on(&fake);
            let rep = engine.step(1000);
            assert!(rep.wrote, "达硬上限应立即动作（不受延迟门禁影响）");

            assert_eq!(
                fake.read("sys/class/xm_power/charger/charger_thermal/wired_ctrl_limit"),
                "15",
                "应使用档案里的最紧档位"
            );
            assert_eq!(
                fake.read("sys/class/xm_power/charger/charger_thermal/wired_chg_curr"),
                // 出厂默认 `tighten_power_w = 3` W ÷ 4.0 V（测试档案电压）= 0.75 A
                "750000",
                "应使用「降到多少」换算出的电流（3 W ÷ 4.0 V = 0.75 A）"
            );
        });
    }

    // ────────────────────────────────────────────────────────────
    //  快照文件的浮点必须取整（真机踩坑回归测试）
    //
    //  真机落盘的原始内容：
    //    "power_w": 3.582210063934326,
    //    "temp_c": 32.29999923706055
    //
    //  根因：`ctl status` 走了 `observability::round_json`，
    //  而 `snapshot_json`（写 runtime.json 的那条路）**漏了** ——
    //  同一份数据两条输出路径、只修了一条。
    // ────────────────────────────────────────────────────────────

    /// 快照里的温度/功率不得带浮点长尾
    #[test]
    fn 快照浮点已取整() {
        let fake = FakeSysfs::new("snap_round");
        fake.write("sys/class/power_supply/battery/temp", "323");
        fake.write("sys/class/power_supply/battery/current_now", "-11082000");
        fake.write("sys/class/power_supply/battery/voltage_now", "3234000");

        with_fake_root(&fake.root, || {
            let mut engine = engine_on(&fake);
            engine.step(1000);
            let snap = engine.snapshot_json(1000);

            let t = snap["battery"]["temp_c"].as_f64().unwrap_or_default();
            // 32.3 的 f32 展开是 32.29999923706055 ⇒ 取整后必须**恰好**是 32.3
            assert_eq!(
                t, 32.3,
                "温度应取整到 0.1℃，实得 {t}（未取整时会带 23706055 尾巴）"
            );

            let p = snap["battery"]["power_w"].as_f64().unwrap_or_default();
            // 小数位不得超过 2 位（`p * 100` 必须接近整数）
            let scaled = p * 100.0;
            assert!(
                (scaled - scaled.round()).abs() < 1e-6,
                "功率应取整到 0.01W，实得 {p}"
            );

            // 反向断言：JSON 文本里不得出现长尾数字
            let text = snap.to_string();
            assert!(
                !text.contains("29999923706055"),
                "快照文本里仍出现 f32 长尾：{text}"
            );
        });
    }

    // ────────────────────────────────────────────────────────────
    //  配置阈值必须贯穿到闭环（真机踩坑回归测试）
    //
    //  真机现象：把收紧阈值设成 38℃ 后，温度稳定在 40.5℃（已超阈值），
    //  模块却始终停在 normal 一动不动。
    //
    //  根因：延迟门禁的"超温计拍"硬编码了 43.0 ——
    //  阈值低于 43 时计拍永不累加 ⇒ 永远卡在 DelayPending ⇒ 永不收紧。
    // ────────────────────────────────────────────────────────────

    #[test]
    fn 端到端_低于四十三度的阈值仍然会触发收紧() {
        let fake = FakeSysfs::new("low_threshold");
        // 40.5℃：高于 38 的配置阈值，但**低于**旧的硬编码 43
        fake.write("sys/class/power_supply/battery/temp", "405");

        with_fake_root(&fake.root, || {
            let mut engine = engine_on(&fake);
            engine
                .cfg
                .set_batch(&[
                    ("thermal.tighten_temp".into(), "38".into()),
                    ("thermal.relax_temp".into(), "37".into()),
                ])
                .expect("38/37 组合应合法");

            // 跑到超过 delay_s（默认 5）+ 余量
            let mut wrote = false;
            for i in 0..12 {
                if engine.step(1000 + i).wrote {
                    wrote = true;
                    break;
                }
            }

            assert!(
                wrote,
                "阈值 38℃ 而温度 40.5℃ —— 必须触发收紧（真机曾在同一场景下永久卡住）"
            );

            // 38℃ 落在曲线「t_max=38.0 → 18_400_000」那一段
            let curr = fake.read("sys/class/xm_power/charger/charger_thermal/wired_chg_curr");
            assert_ne!(
                curr, "18400000",
                "闭环应当真的改变电流，而不只是写了个相同的值"
            );
        });
    }

    #[test]
    fn 端到端_高于四十三度的阈值不会过早收紧() {
        let fake = FakeSysfs::new("high_threshold");
        // 44.0℃：高于旧硬编码的 43，但**低于**配置的 45
        fake.write("sys/class/power_supply/battery/temp", "440");
        // 先把电流节点设成一个"收紧态"值，便于观察它是否被改回放开值
        fake.write(
            "sys/class/xm_power/charger/charger_thermal/wired_chg_curr",
            "1000000",
        );

        with_fake_root(&fake.root, || {
            let mut engine = engine_on(&fake);
            engine
                .cfg
                .set_batch(&[
                    ("thermal.tighten_temp".into(), "45".into()),
                    ("thermal.relax_temp".into(), "44".into()),
                ])
                .expect("45/44 组合应合法");

            for i in 0..8 {
                let rep = engine.step(1000 + i);
                assert_eq!(
                    rep.reason,
                    Some(Reason::Normal),
                    "第 {i} 拍：44℃ 未达阈值 45℃，应判为「正常放行」而非收紧"
                );
            }

            // ⚠️ normal 状态**仍会写**（契约要求周期写回以抵抗系统覆盖），
            //    但写的是**放开值**，而不是曲线里的收紧值。
            //    44℃ 若被误判为"超温"，曲线段 t_max=44 会给出 2_500_000。
            assert_ne!(
                fake.read("sys/class/xm_power/charger/charger_thermal/wired_chg_curr"),
                "2500000",
                "44℃ 不该套用曲线（那说明阈值被当成了 43）"
            );
            assert_eq!(
                fake.read("sys/class/xm_power/charger/charger_thermal/wired_ctrl_limit"),
                "0",
                "未收紧时档位应保持最松（0）"
            );
        });
    }

    /// 官方 `MONITOR-BAT` 会把档位**竞争写**成更严的值（真机实测：写 0 后读回 3/7/11/14）
    ///
    /// 这类"比目标更严"的覆盖**不是**模块的写入失败：
    ///
    /// | 回读值 | 语义 | 处理 |
    /// |---|---|---|
    /// | `≥` 目标 | 不比目标松（可能被官方压得更紧） | 接受（更严对安全无害） |
    /// | `<` 目标 | 比目标松（该紧的没紧上去） | 才算失败 |
    ///
    /// 旧实现只要"不等于目标"就计一次写失败，于是官方把 0 覆盖成 3 后
    /// 连续 5 拍误判 ⇒ `release_now` ⇒ **整个充电会话交回官方**（真机事故）。
    #[test]
    fn 官方把档位覆盖得更严时不算写入失败() {
        let fake = FakeSysfs::new("gear_official_override");
        let ctrl = "sys/class/xm_power/charger/charger_thermal/wired_ctrl_limit";

        with_fake_root(&fake.root, || {
            let mut engine = engine_on(&fake);

            // 第 1 拍：正常放行，模块写档位 0（最松）
            let rep = engine.step(1000);
            assert!(rep.wrote, "正常放行应当写节点");
            assert_eq!(fake.read(ctrl), "0", "档位应被写成最松档 0");

            // 模拟官方 `MONITOR-BAT` 在模块写完之后覆盖成更严的 3 档
            fake.write(ctrl, "3");

            // 之后连续 6 拍（超过 FAIL_LIMIT=5）：官方覆盖不该累计写失败
            for i in 0..6 {
                engine.step(1004 + i * 4);
            }

            assert_eq!(
                engine.write_failures, 0,
                "官方把档位压得更严不是写入失败，不该累计（实得 {}）",
                engine.write_failures
            );
            assert!(
                !engine.released,
                "不得因官方竞争写档位而交回官方（那会让整个充电会话失去温控）"
            );
        });
    }

    /// 档位回读判据：**更严通过、更松失败**
    ///
    /// 单独锁死纯函数的两个方向，避免以后又改回 `==`。
    #[test]
    fn 档位回读判据_更严通过_更松失败() {
        assert!(gear_verified(0, 0), "等于目标应通过");
        assert!(gear_verified(3, 0), "官方把最松档 0 覆盖成 3（更严）应通过");
        assert!(gear_verified(15, 3), "比目标更严应通过");
        assert!(!gear_verified(2, 3), "比目标更松必须判失败");
        assert!(
            !gear_verified(0, 15),
            "硬保护档位被放松（15 → 0）必须判失败"
        );
    }

    // ────────────────────────────────────────────────────────────
    //  负电流（真机符号约定）回归测试
    //
    //  真机实测：**充电中 `current_now` 为负数**（如 -9599000），与直觉相反。
    //  曾经 detect.rs 因 `curr <= 0` 判定失败，报"明明在充电却未实测"。
    // ────────────────────────────────────────────────────────────
    #[test]
    fn 端到端_负电流仍能算出正功率() {
        let fake = FakeSysfs::new("neg_current");
        fake.write("sys/class/power_supply/battery/temp", "390");
        fake.write("sys/class/power_supply/battery/status", "Charging");
        // 真机实测值：充电时电流为负
        fake.write("sys/class/power_supply/battery/current_now", "-9599000");
        fake.write("sys/class/power_supply/battery/voltage_now", "4434000");

        with_fake_root(&fake.root, || {
            let engine = engine_on(&fake);
            let out = engine.sample();
            let s = out
                .sample
                .unwrap_or_else(|| panic!("应能采样，实得错误：{:?}", out.error));

            assert_eq!(
                s.current_ua, 9_599_000,
                "电流应被规范化为**非负**大小（真机充电时原始值为负）"
            );

            let w = s.power_w();
            assert!(
                (42.0..43.0).contains(&w),
                "9.599A × 4.434V ≈ 42.6W，实得 {w}W —— 负数未取绝对值会算成负功率"
            );
        });
    }

    /// **未检测 ⇒ 零控制**
    ///
    /// 用户要求："在使用模块之前未进行测试的，不要进行任何控制行为"。
    ///
    /// # 这个测试防的是什么
    ///
    /// 未检测时档案是**兜底值**，用它控制必然出错（真机两种都踩过）：
    ///
    /// | 兜底值 | 后果 |
    /// |---|---|
    /// | 18.4A（K90PM 实测值） | 67W 机型**超限** |
    /// | 5A（保守猜测） | 用户什么都没设却被**限到约 22W** |
    ///
    /// 正确行为：**一个节点都不写**，完全交给系统原有逻辑。
    #[test]
    fn 端到端_未检测时零写入() {
        let fake = FakeSysfs::new("undetected");
        // 造一个"高温 + 充电中"的场景：若允许控制，它必然会写节点
        fake.write("sys/class/power_supply/battery/temp", "460");
        fake.write("sys/class/power_supply/battery/status", "Charging");

        with_fake_root(&fake.root, || {
            // 与 `engine_on` 相同，但**不**标 detected
            let nodes = Nodes::detect(None);
            let (table, _) = GearTable::detect().expect("假档位表应当可解析");
            let events = EventLog::new(fake.path("events.jsonl"), 512);
            let snapshot = SnapshotWriter::new(fake.path("runtime.json"));
            let cfg = ConfigStore::load(fake.path("config.json"));
            let profile = thermo_ipc::device::EffectiveProfile {
                profile: crate::profile_io::generic_profile(Default::default()),
                source: thermo_ipc::device::ProfileSource::Generic,
            };
            let (slots, _) = thermo_addons::registry::build_all(cfg.raw());
            let addons = thermo_addons::runner::AddonRunner::new(slots);

            let mut engine = Engine::new(EngineInit {
                cfg,
                profile,
                nodes,
                gear_table: Some(table),
                addons,
                events,
                snapshot,
                history_path: fake.path("history.json"),
                history_draft_path: fake.path("history.current.jsonl"),
                now_s: 2000,
            });

            let before_ctrl =
                fake.read("sys/class/xm_power/charger/charger_thermal/wired_ctrl_limit");
            let before_curr =
                fake.read("sys/class/xm_power/charger/charger_thermal/wired_chg_curr");

            for i in 0..5 {
                let rep = engine.step(2000 + i);
                assert!(rep.sampled, "第 {i} 拍应采样成功（采样不受门禁影响）");
                assert!(
                    !rep.wrote,
                    "第 {i} 拍：未检测时**不得写任何节点**（实际写了）"
                );
            }

            assert_eq!(
                fake.read("sys/class/xm_power/charger/charger_thermal/wired_ctrl_limit"),
                before_ctrl,
                "档位节点不得被改动"
            );
            assert_eq!(
                fake.read("sys/class/xm_power/charger/charger_thermal/wired_chg_curr"),
                before_curr,
                "电流节点不得被改动"
            );
        });
    }

    /// 只观测模式下零写入
    #[test]
    fn 端到端_只观测模式零写入() {
        let fake = FakeSysfs::new("observe");
        fake.write("sys/class/power_supply/battery/temp", "450"); // 即使硬保护也不该写

        with_fake_root(&fake.root, || {
            let mut engine = engine_on(&fake);
            engine.cfg.set("observe_only", "true").unwrap();

            for i in 0..5 {
                let rep = engine.step(1000 + i);
                assert!(rep.sampled, "第 {i} 拍应采样成功");
            }

            assert_eq!(
                fake.read("sys/class/xm_power/charger/charger_thermal/wired_ctrl_limit"),
                "0",
                "只观测模式下不得写入任何节点"
            );
            assert_eq!(
                fake.read("sys/class/xm_power/charger/charger_thermal/wired_chg_curr"),
                "18400000",
                "只观测模式下电流也不得被改动"
            );
        });
    }

    /// 「交回官方」必须**只放宽、不收紧**
    ///
    /// # 这个测试防的是什么
    ///
    /// 档案来自**保守兜底**时（用户尚未跑检测），`curr_max` 会小于
    /// 节点当前值。若释放时照写，就成了"**交回官方反而限速**"：
    ///
    /// ```text
    /// 驱动默认 18400000 → 模块"释放"写 5000000
    /// ⇒ 用户装完模块什么都没设，充电却从 100W 掉到约 22W
    /// ```
    ///
    /// 上面那个测试（`端到端_只观测模式零写入`）正是这个场景 ——
    /// 假节点的 18400000 未被 5000000 覆盖。
    #[test]
    fn 端到端_释放不会收紧电流() {
        let fake = FakeSysfs::new("release");
        // 造出"模块之前收紧过"的状态：节点值为很小的电流
        fake.write(
            "sys/class/xm_power/charger/charger_thermal/wired_chg_curr",
            "700000",
        );
        // 未充电 ⇒ 决策为 release
        fake.write("sys/class/power_supply/battery/status", "Discharging");

        with_fake_root(&fake.root, || {
            let mut engine = engine_on(&fake);
            engine.step(1000);

            let curr = fake.read("sys/class/xm_power/charger/charger_thermal/wired_chg_curr");
            let v: i64 = curr.trim().parse().expect("电流节点应是数字");
            assert!(v > 700_000, "释放时应把 700000 的收紧值放开来，实得 {curr}");
        });
    }

    #[test]
    fn 端到端_未充电时交回官方() {
        let fake = FakeSysfs::new("idle");
        fake.write("sys/class/power_supply/battery/status", "Discharging");
        // 先制造一个"被限速"的现场
        fake.write(
            "sys/class/xm_power/charger/charger_thermal/wired_ctrl_limit",
            "10",
        );
        fake.write(
            "sys/class/xm_power/charger/charger_thermal/wired_chg_curr",
            "1000000",
        );

        with_fake_root(&fake.root, || {
            let mut engine = engine_on(&fake);
            let rep = engine.step(1000);
            assert!(rep.sampled);
            assert!(rep.wrote, "未充电应执行「交回官方」");

            assert_eq!(
                fake.read("sys/class/xm_power/charger/charger_thermal/wired_ctrl_limit"),
                "0",
                "档位应归零"
            );
            assert_ne!(
                fake.read("sys/class/xm_power/charger/charger_thermal/wired_chg_curr"),
                "1000000",
                "电流应被放开，不能停留在旧的限速值"
            );
        });
    }

    // ────────────────────────────────────────────────────────────
    //  充电历史（**纯观测** —— 不得影响温控）
    // ────────────────────────────────────────────────────────────

    #[test]
    fn 端到端_充电时记录历史拔掉后归档() {
        let fake = FakeSysfs::new("hist");
        with_fake_root(&fake.root, || {
            let mut engine = engine_on(&fake);

            // ① 充电中 ⇒ 开始记录
            let mut t = 1000;
            for _ in 0..5 {
                engine.step(t);
                t += 30;
            }
            assert!(engine.history.is_recording(), "充电中应正在记录");

            // ② 拔掉充电器 ⇒ 归档
            fake.write("sys/class/power_supply/battery/status", "Discharging");
            engine.step(t);

            assert!(!engine.history.is_recording(), "拔掉后应归档");
            let sessions = engine.history.load_sessions();
            assert_eq!(sessions.len(), 1, "应归档出一条记录");
            assert_eq!(sessions[0].started_at, 1000);
            assert!(sessions[0].points.len() >= 2, "曲线应有多个采样点");
            assert!(sessions[0].ended_at > 0, "应记下结束时刻");
        });
    }

    #[test]
    fn 端到端_历史记录不影响控制行为() {
        // 关掉历史记录后，温控的节点写入必须**完全一样** ——
        // 证明历史是纯观测、不参与决策
        let run = |enable: bool| -> (String, String) {
            let fake = FakeSysfs::new(if enable { "hist_on" } else { "hist_off" });
            with_fake_root(&fake.root, || {
                let mut engine = engine_on(&fake);
                engine
                    .cfg
                    .set("history.enabled", if enable { "true" } else { "false" })
                    .unwrap();
                engine.history.configure(engine.cfg.history_cfg());

                for i in 0..5 {
                    engine.step(1000 + i * 30);
                }
                (
                    fake.read("sys/class/xm_power/charger/charger_thermal/wired_ctrl_limit"),
                    fake.read("sys/class/xm_power/charger/charger_thermal/wired_chg_curr"),
                )
            })
        };

        let (ctrl_on, curr_on) = run(true);
        let (ctrl_off, curr_off) = run(false);
        assert_eq!(ctrl_on, ctrl_off, "开关历史不得改变档位写入");
        assert_eq!(curr_on, curr_off, "开关历史不得改变电流写入");
    }

    #[test]
    fn 端到端_关闭历史时不记录() {
        let fake = FakeSysfs::new("hist_disabled");
        with_fake_root(&fake.root, || {
            let mut engine = engine_on(&fake);
            engine.cfg.set("history.enabled", "false").unwrap();
            engine.history.configure(engine.cfg.history_cfg());

            for i in 0..5 {
                engine.step(1000 + i * 30);
            }
            assert!(!engine.history.is_recording(), "关闭时不得记录");
            assert!(engine.history.load_sessions().is_empty());
        });
    }

    #[test]
    fn 端到端_进行中会话的点数实时可用() {
        // 🔴 真机暴露：命令面若只读**草稿文件**，而草稿每 10 分钟才落盘，
        //    界面会显示「充电 20 分钟只有 2 个采样点」。
        //
        //    修法是命令面直接用 worker 内存里的 `engine.history`。
        //    本测试守住"内存里的点数随拍数实时增长"。
        let fake = FakeSysfs::new("hist_live");
        with_fake_root(&fake.root, || {
            let mut engine = engine_on(&fake);

            let mut t = 1000;
            for _ in 0..10 {
                engine.step(t);
                t += 30;
            }

            let cur = engine
                .history
                .current_session_full()
                .expect("应有进行中会话");
            assert_eq!(
                cur.points.len(),
                10,
                "内存里的点数必须与拍数一致（1 首点 + 9 采样）"
            );

            // 快照接口也要报出实时数量与"正在记录"标志
            let snap = engine
                .history
                .current_session(t)
                .expect("快照应有进行中会话");
            assert_eq!(snap["points"], 10, "快照里的点数也必须是实时的");

            let st = engine.history.stats();
            assert_eq!(st["recording"], true, "stats 应报出正在记录");
        });
    }

    #[test]
    fn 端到端_历史里的限流值是模块真正写下去的值() {
        // 这条性质是充电历史**最有价值的用途**：把「温度」与「模块施加的限流」
        // 放在一起看，才能回答"温度上来后模块到底有没有降速"。
        // 若曲线里记的是别的值（比如档案上限），这个用途就不成立。
        let fake = FakeSysfs::new("hist_limit");
        with_fake_root(&fake.root, || {
            let mut engine = engine_on(&fake);

            // 造一个"必须收紧"的现场：电池温度超过硬保护（档案 48℃）
            fake.write("sys/class/power_supply/battery/temp", "500");

            let mut t = 1000;
            for _ in 0..4 {
                engine.step(t);
                t += 30;
            }

            // 从草稿文件读回最后一点
            //
            // ⚠️ 草稿是 **JSONL**（追加写）：第 1 行是头部，其余每行一个点。
            //    这样每个点立即落盘，worker 重启不丢数据。
            let text = std::fs::read_to_string(fake.path("history.current.jsonl"))
                .expect("进行中的会话应已落草稿");
            // 取最后一行（跳过头部）—— 用 rev + find，clippy 更认这个写法
            let last = text
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty() && !l.contains("\"h\":1"))
                .expect("草稿里应至少有一个点");
            let v: serde_json::Value = serde_json::from_str(last).unwrap();
            let limit = v["limit_ua"].as_i64().expect("曲线点里应有 limit_ua");

            let node_curr = fake
                .read("sys/class/xm_power/charger/charger_thermal/wired_chg_curr")
                .parse::<i64>()
                .expect("节点值应是整数");

            assert_eq!(limit, node_curr, "曲线里的限流值必须等于节点上真实写入的值");
        });
    }

    #[test]
    fn 端到端_附加功能与温控互不干扰() {
        let fake = FakeSysfs::new("addon");
        // ⚠️ 必须是**充电中**（2026-10-20 修正）
        //
        // 充电上限有一条设计纪律：**未充电时不巡检**（省电，见
        // `AddonIo::is_plugged` 的文档）。原先这里写 `Discharging`
        // 却期望 SOC 控制字被写入 —— 那个断言之所以一直能过，是因为
        // `WatchedIo` 漏实现了 `is_plugged()`，落到 trait 默认值
        // （永远 `true`）。补上实现后本测试立刻失败，正好暴露了
        // "测试依赖 bug" 这件事。
        fake.write("sys/class/power_supply/battery/status", "Charging");
        // 附加功能侧：初始生效值 80
        fake.write("sys/class/xm_power/charger/smart_charge/soc_limit", "80");

        with_fake_root(&fake.root, || {
            let mut engine = engine_on(&fake);
            engine.cfg.set("limit.enabled", "true").unwrap();
            engine.cfg.set("limit.soc", "85").unwrap();
            crate::addon_host::reload_config(&mut engine.addons, &engine.cfg);

            // 附加功能需要先探测（worker 启动时会做）；走引擎自己的看门狗 I/O
            engine.addons.probe_all(&mut engine.addon_io);

            let rep = engine.step(1000);

            // 两个循环各自干活：温控写了档位/电流，附加功能写了 SOC 控制字
            assert!(rep.wrote, "温控侧应当写入（交回官方）");
            assert!(
                rep.addons_ran.iter().any(|s| s == "soc_limit"),
                "附加功能应当被调度，实得 {:?}",
                rep.addons_ran
            );

            // 附加功能写的是**控制字**到 smart_chg（而不是直接写 soc_limit）
            let word = fake.read("sys/class/xm_power/charger/smart_charge/smart_chg");
            assert_eq!(
                word, "0x550011",
                "(85 << 16) | 0x11 的十六进制形式，实得 {word}"
            );

            // 温控侧的节点只被温控侧改，附加功能没碰
            assert_eq!(
                fake.read("sys/class/xm_power/charger/charger_thermal/wired_ctrl_limit"),
                "0"
            );

            // ⚠️ 假节点树**没有驱动**，所以 smart_chg 的写入不会让 soc_limit 跟着变。
            //    这恰好验证了契约里的关键点：**判断生效永远读 soc_limit** ——
            //    模块发现回读不符后必须报错（而不是盲目相信自己写成功了）。
            let slot = engine.addons.slot("soc_limit").expect("插槽应存在");
            assert_eq!(
                slot.consecutive_failures(),
                1,
                "回读不符必须计入失败（证明模块确实在读 soc_limit 校验）"
            );
            let err = slot.last_error().map(|e| e.to_string()).unwrap_or_default();
            assert!(
                err.contains("回读不符"),
                "错误信息应说明回读不符，实得：{err}"
            );
        });
    }

    #[test]
    fn 端到端_附加功能未启用时不写它的节点() {
        let fake = FakeSysfs::new("addon_off");
        fake.write("sys/class/xm_power/charger/smart_charge/soc_limit", "80");

        with_fake_root(&fake.root, || {
            let mut engine = engine_on(&fake);
            // 出厂默认就是关闭
            assert!(!engine.cfg.get_bool("limit.enabled"));
            engine.addons.probe_all(&mut engine.addon_io);

            for i in 0..3 {
                engine.step(1000 + i);
            }

            assert_eq!(
                fake.read("sys/class/xm_power/charger/smart_charge/soc_limit"),
                "80",
                "决策 B：功能关闭时对该节点**零写入**"
            );
        });
    }

    // ────────────────────────────────────────────────────────────
    //  ★ 核心隔离命题：附加功能写入卡死时，温控必须照常工作
    //
    //  真机事故（2026-10-13）：写 smart_chg 让内核驱动 mca_smart_charge
    //  死锁在 cancel_delayed_work_sync → __flush_work，worker 进程陷入
    //  D 状态（不可中断），**整个温控停摆**且 kill -9 无效。
    //
    //  这违背了"附加功能不要与充电控制搅在一起"的要求 ——
    //  代码层隔离（crate 不依赖）做到了，运行期没有。
    //
    //  本测试用一个"写入即永久阻塞"的假 I/O 复现该场景，
    //  断言温控侧仍然正常决策与写入。
    // ────────────────────────────────────────────────────────────

    /// 写入指定路径就**永久阻塞**的假 I/O（等价于真机的 D 状态）
    #[derive(Debug)]
    struct HangingIo {
        inner: crate::addon_host::SysfsIo,
        hang_path: String,
    }

    impl thermo_addons::io::AddonIo for HangingIo {
        fn exists(&mut self, p: &str) -> bool {
            self.inner.exists(p)
        }
        fn writable(&mut self, p: &str) -> bool {
            self.inner.writable(p)
        }
        fn read(&mut self, p: &str) -> Result<String, String> {
            self.inner.read(p)
        }
        fn write(&mut self, p: &str, v: &str) -> Result<(), String> {
            if p == self.hang_path {
                // 永久阻塞 —— 与真机内核 D 状态等价
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(3600));
                }
            }
            self.inner.write(p, v)
        }
    }

    #[test]
    fn 端到端_附加功能写入卡死时温控不受影响() {
        let fake = FakeSysfs::new("hang_isolation");
        fake.write("sys/class/xm_power/charger/smart_charge/soc_limit", "80");
        // 温度超过阈值 → 温控侧本来就要动作，便于观察它是否还活着
        fake.write("sys/class/power_supply/battery/temp", "450");

        with_fake_root(&fake.root, || {
            let mut engine = engine_on(&fake);

            // 打开充电上限，让它去写 smart_chg（会卡死）
            engine.cfg.set("limit.enabled", "true").unwrap();
            engine.cfg.set("limit.soc", "85").unwrap();
            crate::addon_host::reload_config(&mut engine.addons, &engine.cfg);
            engine.addons.probe_all(&mut engine.addon_io);

            // ⚠️ 换成一个"写 smart_chg 即永久阻塞"的 I/O，模拟真机事故
            engine.addon_io = crate::io_watchdog::WatchedIo::spawn(Box::new(HangingIo {
                inner: crate::addon_host::SysfsIo,
                hang_path: "/sys/class/xm_power/charger/smart_charge/smart_chg".to_string(),
            }));

            // ── 跑到附加功能那一拍（会卡 3 秒然后超时）──
            let t0 = std::time::Instant::now();
            let rep = engine.step(1000);
            let elapsed = t0.elapsed();

            // 1) 调用方**返回了**（而不是永久挂起）—— 这是最关键的一条
            assert!(
                elapsed < std::time::Duration::from_secs(10),
                "附加功能写入卡死不得拖住主循环（耗时 {elapsed:?}）"
            );

            // 2) 温控侧照常写入（硬保护：档位收到最紧 + 电流收紧）
            assert!(rep.wrote, "温控必须照常动作");
            //    档位先放开到 0，再在下一拍收紧；此处只需确认它确实在动

            // 3) I/O 被判定挂起，并留下可诊断的描述
            //
            //    注意 `take_stuck_detail()` 已在 `tick_addons` 里取走过一次
            //    （用于记事件），因此这里改从**事件文件**里读证据 ——
            //    顺便验证"挂起确实被写进了用户可见的事件流"。
            assert!(engine.addon_io.is_stuck(), "I/O 应被标记为挂起");
            let events_text =
                std::fs::read_to_string(fake.path("events.jsonl")).unwrap_or_default();
            assert!(
                events_text.contains("io_timeout_s"),
                "应在事件流里记录 I/O 挂起（含超时秒数），实得：{events_text}"
            );
            assert!(
                events_text.contains("无响应") || events_text.contains("停用"),
                "事件应说明节点无响应/已停用，实得：{events_text}"
            );

            // 4) **温控继续工作**：后续拍仍然正常决策与写入
            for i in 1..4 {
                let rep = engine.step(1000 + i);
                assert!(rep.wrote, "第 {i} 拍：附加功能已停用，温控必须继续正常工作");
            }

            // 5) 温控最后把档位收到最紧（硬保护的预期行为）
            assert_eq!(
                fake.read("sys/class/xm_power/charger/charger_thermal/wired_ctrl_limit"),
                "15",
                "45℃ 达硬上限 → 档位应收到最紧，证明温控链路完好"
            );

            // 6) 附加功能侧：卡死导致回读失败 → 计入失败（最终会熔断）
            let slot = engine.addons.slot("soc_limit").expect("插槽应存在");
            assert!(
                slot.consecutive_failures() > 0,
                "卡死必须被计为失败，让它最终熔断停用"
            );
        });
    }
}
