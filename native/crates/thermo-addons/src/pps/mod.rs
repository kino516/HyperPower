//! 附加功能 ②：PPS 解锁（绕过 PD 认证 + 可选伪造金标动画）
//!
//! 移植自 FreePPS（GPL-3.0），机制与写入纪律**照抄真机结论**。
//! 方案见 `ThermoCharge-PPS移植方案.md`。
//!
//! # 一句话
//!
//! 有些第三方 PD 充电头支持 PPS（可编程电压），但手机厂商的 PD 认证
//! 会把它们挡在快充之外。本功能把 `pd_verifed` 置 1，让驱动跳过认证，
//! 于是这些头也能触发快充。
//!
//! # 三个塌陷点（都踩过，别再踩）
//!
//! | # | 坑 | 处理 |
//! |---|---|---|
//! | 1 | 充电中每拍重复写 `1` ⇒ 干扰 MIPPS 握手 | 用 `engaged` 标记，只在**状态跳变**时写（见 [`state`]） |
//! | 2 | 广播同步发 ⇒ 阻塞温控采样近 1 秒 | 子进程 `spawn_detached`（见 [`Pps::tick`]） |
//! | 3 | 对小米原装头发金标 ⇒ 与系统动画打架 | 门控要求 `adapter_svid == 0000`（见 [`forge`]） |
//!
//! # 隔离
//!
//! 本模块**不依赖 `thermo-core`**：只认识自己的几个节点，不碰档位/电流/温度。

pub mod config;
pub mod forge;
pub mod nodes;
pub mod state;

use serde_json::json;

use crate::addon::{Addon, AddonError, AddonStatus, ProbeOutcome, TickOutcome};
use crate::io::AddonIo;

pub use config::PpsConfig;
pub use nodes::Platform;

use config::PpsMode;
use state::{Action, State};

/// 未启用、且**没有节点可还原**时的巡检间隔（秒）
///
/// 场景：用户没开功能、本机探测不到认证位 —— 这时没什么可做，
/// 低频看一眼即可（等配置变更会由宿主 `reload_config` 唤醒）。
const IDLE_INTERVAL_S: u32 = 60;

/// 每拍读的只读节点缓存（供 `status` 与门控共用，避免重复 I/O）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Snapshot {
    /// `pd_verifed` 原文
    pd_verified: Option<String>,
    /// `real_type` 原文
    real_type: Option<String>,
    /// `adapter_svid` 原文
    adapter_svid: Option<String>,
    /// USB 电压（μV）
    voltage_uv: Option<i64>,
    /// `apdo_max`（W）
    apdo_max: Option<u16>,
}

/// PPS 解锁功能
#[derive(Debug, Clone)]
pub struct Pps {
    /// 配置（来自 `config.json` 的 `pps` 分区）
    config: PpsConfig,
    /// 探测到的节点集合（`None` = 尚未探测或本机不支持）
    nodes: Option<nodes::Nodes>,
    /// 探测结果
    support: Option<ProbeOutcome>,
    /// 会话状态机
    state: State,
    /// 本拍读到的只读节点快照
    snap: Snapshot,
    /// **本次插电是否已经广播过**
    ///
    /// 目标：同一次插电只放一次动画。拔出后清零，
    /// 下次插上再放 —— 否则动画不会重新播（SystemUI 会去重）。
    burst_done: bool,
    /// 累计写入次数
    write_count: u64,
    /// 最近一次门控判定结果（供界面解释"为什么没广播"）
    gate_msg: Option<String>,
}

impl Pps {
    /// 用给定配置构造（尚未探测）
    pub fn new(config: PpsConfig) -> Self {
        Self {
            config,
            nodes: None,
            support: None,
            state: State::default(),
            snap: Snapshot::default(),
            burst_done: false,
            write_count: 0,
            gate_msg: None,
        }
    }

    /// 换配置（宿主在配置变更时调用）
    pub fn set_config(&mut self, config: PpsConfig) {
        self.config = config;
    }

    /// 当前配置
    pub fn config(&self) -> &PpsConfig {
        &self.config
    }

    /// 命中的认证位路径（供 `addon show` 与排障）
    pub fn node_pd_verified(&self) -> Option<&str> {
        self.nodes.as_ref().map(|n| n.pd_verified.as_str())
    }

    /// 探测到的平台
    pub fn platform(&self) -> Option<Platform> {
        self.nodes.as_ref().map(|n| n.platform)
    }

    /// 累计写入次数
    pub fn write_count(&self) -> u64 {
        self.write_count
    }

    /// 读一个可能的整数节点（失败 → `None`）
    fn read_i64(io: &mut dyn AddonIo, path: Option<&str>) -> Option<i64> {
        let p = path?;
        io.read(p).ok()?.trim().parse().ok()
    }

    /// 读一个可能的字符串节点（失败 → `None`）
    fn read_str(io: &mut dyn AddonIo, path: Option<&str>) -> Option<String> {
        let p = path?;
        let s = io.read(p).ok()?;
        let t = s.trim();
        (!t.is_empty()).then(|| t.to_string())
    }

    /// 抓一份只读快照（**不写任何节点**）
    fn snapshot(&mut self, io: &mut dyn AddonIo) -> Snapshot {
        let Some(nd) = self.nodes.clone() else {
            return Snapshot::default();
        };

        let pd = Self::read_str(io, Some(&nd.pd_verified));
        let rt = Self::read_str(io, nd.real_type.as_deref());
        let svid = Self::read_str(io, nd.adapter_svid.as_deref());
        let v = Self::read_i64(io, nd.usb_voltage.as_deref());
        let apdo = Self::read_i64(io, nd.apdo_max.as_deref())
            .and_then(|x| u16::try_from(x).ok())
            .filter(|w| *w > 0);

        Snapshot {
            pd_verified: pd,
            real_type: rt,
            adapter_svid: svid,
            voltage_uv: v,
            apdo_max: apdo,
        }
    }

    /// `auto` 模式下是否应当解锁
    ///
    /// | 模式 | 判据 |
    /// |---|---|
    /// | `lock` | 总是 `true` |
    /// | `auto` | 仅在 `real_type` 已含 `PPS` 时才解锁 |
    ///
    /// ⚠️ `auto` 的语义是"不主动改变协议路径"：如果充电头本来就是
    ///    PPS，那解锁只是把认证这一关推开；如果本来不是，就不去碰它。
    fn should_engage(&self) -> bool {
        match self.config.mode {
            PpsMode::Lock => true,
            PpsMode::Auto => self
                .snap
                .real_type
                .as_deref()
                .is_some_and(|s| s.to_ascii_uppercase().contains("PPS")),
        }
    }

    /// 门控判定（决定要不要放金标动画）
    fn gate(&self) -> forge::GateResult {
        forge::evaluate(
            &forge::Gate {
                real_type: self.snap.real_type.clone(),
                pd_verified: self.snap.pd_verified.clone(),
                adapter_svid: self.snap.adapter_svid.clone(),
                voltage_uv: self.snap.voltage_uv,
                apdo_max: self.snap.apdo_max,
            },
            self.config.min_vbus_uv,
            self.config.power_display_w,
        )
    }
}

impl Addon for Pps {
    fn spec(&self) -> crate::addon::AddonSpec {
        config::spec(&self.config)
    }

    fn probe(&mut self, io: &mut dyn AddonIo) -> ProbeOutcome {
        match nodes::Nodes::detect(io) {
            Some(nd) => {
                let detail = format!("命中 {}（平台 {}）", nd.pd_verified, nd.platform.label());
                self.nodes = Some(nd);
                // ⚠️ 探测成功后立刻抓一份快照
                //
                // `status()` 是 `&self`（不能做 I/O），它只能读 `snap`。
                // 若不在这里抓，`addon show pps` 在第一次 tick 之前会
                // 把所有只读节点显示成 null —— 用户无法判断
                // "本机到底能不能广播"，而那是排障的第一手信息。
                self.snap = self.snapshot(io);
                let outcome = ProbeOutcome::Supported { detail };
                self.support = Some(outcome.clone());
                outcome
            }
            None => {
                self.nodes = None;
                self.snap = Snapshot::default();
                let outcome = ProbeOutcome::Unsupported {
                    reason: format!(
                        "未找到可写的 PD 认证位（已尝试 {} 条候选：qcom 与 mtk 两套平台）",
                        nodes::PD_VERIFIED.len()
                    ),
                };
                self.support = Some(outcome.clone());
                outcome
            }
        }
    }

    fn tick(&mut self, io: &mut dyn AddonIo, now_s: i64) -> TickOutcome {
        let interval = self.config.interval_s();

        // ① 先抓快照（只读）
        //
        // ⚠️ 必须在"未启用"判断**之前**抓 —— 否则用户关着功能时
        //    `addon show pps` 的 real_type / adapter_svid / 门控原因
        //    全是 null，"为什么不能广播"就无从判断。
        //
        // 代价：5 次只读 sysfs 访问。未启用时周期是 30~60 秒，
        // 这点开销远小于"用户看不到诊断信息"的代价。
        if self.nodes.is_some() {
            self.snap = self.snapshot(io);
        }

        // ② 未启用 → 走 release 语义（可能需要在拔电时还原）
        //
        // ⚠️ 不能直接 `idle`：用户关掉功能后，节点里可能还留着我们写的 1。
        //    但也不该在**充电中**还原（会打断协议协商）——
        //    这个判断交给状态机（见 `State::on_release`）。
        if !self.config.enabled {
            return self.release(io);
        }

        // ③ 节点必须已探测到
        if self.nodes.is_none() {
            return TickOutcome::failed(
                AddonError::node_missing("PD 认证位不可用（本机型可能不支持 PPS 解锁）"),
                interval,
            );
        }

        // ④ 插电状态：以 `is_plugged()` 为准（与充电上限同一口径）
        let charging = io.is_plugged();

        // 记录"刚插电"这一跳变 —— 拔出时把广播标记清掉，
        // 下次插上才能再放一次动画
        let was_charging = self.state.is_charging();
        if was_charging && !charging {
            self.burst_done = false;
        }

        // ④-1 🔴 **回读同步**：节点可能被系统重置（2026-10-20 真机 bug）
        //
        // # 为什么必须有这一步
        //
        // 真机实测（K70 Pro，插着 100W PPS 头）：
        //
        // | 观察 | 值 |
        // |---|---|
        // | 我们写过 | `1`（`write_count: 1`） |
        // | 节点当前 | **`0`** ← 被驱动/系统改回去了 |
        // | 内核 dmesg | `pd_verifed:= 1`（内部状态又是 1） |
        //
        // 也就是说：**节点值会被外部改写**。而状态机只看自己的
        // `engaged` 标记 —— 它认为"已写过，不必重复写"，于是
        // **再也不会把 1 写回去** ⇒ 解锁实际失效。
        //
        // ⇒ 每拍回读一次；发现"我们认为已解锁、实际却是 0"就同步标记，
        //   于是本拍/下一拍的状态机会重新写一次。
        //
        // ⚠️ 只在**实际值不是 1** 时同步 —— 不要反过来把"实际是 1"
        //    当作已解锁（那会掩盖"我们没写过"的事实，`auto` 模式会误判）。
        {
            let actual_one = self.snap.pd_verified.as_deref() == Some("1");
            if self.state.is_engaged() && !actual_one {
                self.state.note_actual(false);
            }
        }

        // ⑤ `auto` 模式下不满足条件 ⇒ 不写节点
        //
        // ⚠️ **绝对不能**调 `state.on_tick()`（2026-10-20 真机 bug）：
        //    它会把 `engaged` 置 true，而节点根本没写。之后即使切回
        //    「锁定」模式，状态机也认为"已解锁"而不再写 ⇒
        //    用户看到的就是「开启 PPS 解锁无效」。
        //
        // ⇒ 只更新"是否在充电"，不动 `engaged`。
        if !self.should_engage() {
            self.state.note_charging(charging);

            // 但**之前**写过 1（比如从「锁定」切过来）的，要还原成 0 ——
            // 否则「自动」模式成了"进去就不管"，与它的语义相反。
            //
            // ⚠️ 充电中不还原（会打断正在进行的协商），等拔电后下一拍再还。
            if self.state.is_engaged() && !charging {
                if let Some(nd) = self.nodes.clone() {
                    if io.write(&nd.pd_verified, "0").is_ok() {
                        self.write_count = self.write_count.saturating_add(1);
                        self.state.note_actual(false);
                        self.snap.pd_verified = Some("0".to_string());
                        return TickOutcome::wrote(interval);
                    }
                }
            }
            return TickOutcome::idle(interval);
        }

        // ⑥ 会话状态机决定本拍动作
        let action = self.state.on_tick(charging, now_s);

        let mut wrote = false;
        if let Action::Engage | Action::Release = action {
            let Some(nd) = self.nodes.clone() else {
                return TickOutcome::failed(AddonError::node_missing("节点在探测后消失"), interval);
            };
            let value = action.value().unwrap_or("0");

            if let Err(e) = io.write(&nd.pd_verified, value) {
                return TickOutcome::failed(
                    AddonError::bad_cond(format!("写入 {} 失败：{e}", nd.pd_verified)),
                    interval,
                );
            }
            self.write_count = self.write_count.saturating_add(1);
            wrote = true;

            // 回读校验：`pd_verifed` 的写入是**诚实**的（真机实测），
            // 所以这里能真正验证 —— 不像 charge_control_end_threshold 那样会谎报。
            let back = Self::read_str(io, Some(&nd.pd_verified));
            if back.as_deref() != Some(value) {
                let seen = back.unwrap_or_else(|| "读不到".to_string());
                return TickOutcome {
                    wrote: true,
                    changed: true,
                    error: Some(AddonError::bad_cond(format!(
                        "写入后回读不符：期望 {value}，实得 {seen}"
                    ))),
                    next_interval_s: interval,
                };
            }
            self.snap.pd_verified = back;
        }

        // ⑦ 广播（只在**刚插电**且门控满足时放一次）
        //
        // ⚠️ 三个前提缺一不可：
        //    · 配置打开 `forge_broadcast`
        //    · 正在充电（拔电时放动画没有意义）
        //    · 本次插电还没放过（`burst_done`）
        if self.config.forge_broadcast && charging && !self.burst_done {
            let g = self.gate();
            self.gate_msg = Some(g.message());

            if let forge::GateResult::Ready { power_w } = g {
                // ⚠️ 交给子进程：同步发 5 条 am 会阻塞主循环近 1 秒
                let power = power_w.to_string();
                let spawned = io.spawn_detached(&["pps", "burst", "--power", &power]);

                match spawned {
                    Ok(()) => {
                        self.state.mark_burst(now_s);
                        self.burst_done = true;
                    }
                    Err(e) => {
                        // 广播失败**不是致命错误**：节点已经写好了，
                        // 只是动画没放。记下来供界面显示，下一拍会再试。
                        self.gate_msg = Some(format!("广播启动失败：{e}"));
                    }
                }
            }
        }

        let mut out = if wrote {
            TickOutcome::wrote(interval)
        } else {
            TickOutcome::idle(interval)
        };
        // 状态可能变了（如刚插电），让宿主记事件
        out.changed = wrote;
        out
    }

    fn release(&mut self, io: &mut dyn AddonIo) -> TickOutcome {
        let interval = self.config.interval_s();
        let Some(nd) = self.nodes.clone() else {
            // 没有节点 ⇒ 无事可做，低频等待配置变更
            return TickOutcome::idle(IDLE_INTERVAL_S);
        };

        let charging = io.is_plugged();
        let action = self.state.on_release(charging);

        if action != Action::Release {
            // 要么从未解锁过，要么正在充电（不打断协商）
            return TickOutcome::idle(interval);
        }

        if let Err(e) = io.write(&nd.pd_verified, "0") {
            return TickOutcome::failed(
                AddonError::bad_cond(format!("还原 {} 失败：{e}", nd.pd_verified)),
                interval,
            );
        }
        self.write_count = self.write_count.saturating_add(1);

        // 还原后本次插电的广播标记也该清掉
        self.burst_done = false;
        TickOutcome::wrote(interval)
    }

    fn status(&self) -> AddonStatus {
        let spec = self.spec();
        let gate = self.gate();

        AddonStatus {
            id: spec.id.to_string(),
            label: spec.label.to_string(),
            summary: spec.summary.to_string(),
            origin: spec.origin.to_string(),
            enabled: self.config.enabled,
            supported: self.supported(),
            tripped: false, // 熔断状态由 runner 覆盖
            interval_s: interval_or(&self.config),
            probe: self
                .support
                .as_ref()
                .map_or_else(|| "尚未探测".to_string(), |p| p.message().to_string()),
            requires: spec.requires.map(str::to_string),
            detail: json!({
                "platform": self.platform().map(Platform::label),
                "node": self.node_pd_verified(),
                "node_value": self.snap.pd_verified,
                "mode": self.config.mode.label(),
                "session": self.state.session_label(),
                "engaged": self.state.is_engaged(),
                "real_type": self.snap.real_type,
                "adapter_svid": self.snap.adapter_svid,
                "voltage_uv": self.snap.voltage_uv,
                "apdo_max": self.snap.apdo_max,
                "broadcast_ready": gate.is_ready(),
                "broadcast_reason": self.gate_msg.clone().unwrap_or_else(|| gate.message()),
                "last_burst_at": self.state.last_burst_at,
                "write_count": self.write_count,
            }),
        }
    }

    fn enabled(&self) -> bool {
        self.config.enabled
    }

    fn supported(&self) -> bool {
        self.nodes.is_some()
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// 巡检间隔（配置越界时钳位）
fn interval_or(c: &PpsConfig) -> u32 {
    c.interval_s()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::addon::Addon;

    /// 极简假节点树
    #[derive(Debug, Default)]
    struct FakeIo {
        files: std::collections::HashMap<String, String>,
        writable: Vec<String>,
        plugged: bool,
        write_log: Vec<(String, String)>,
        spawned: Vec<Vec<String>>,
    }

    impl FakeIo {
        fn qcom() -> Self {
            let mut files = std::collections::HashMap::new();
            for (k, v) in [
                ("/sys/class/qcom-battery/pd_verifed", "0"),
                ("/sys/class/qcom-battery/real_type", "PD_PPS"),
                ("/sys/class/qcom-battery/adapter_svid", "0000"),
                ("/sys/class/power_supply/usb/voltage_now", "20000000"),
                ("/sys/class/qcom-battery/apdo_max", "100"),
                ("/sys/class/power_supply/battery/status", "Charging"),
            ] {
                files.insert(k.to_string(), v.to_string());
            }
            Self {
                files,
                writable: vec!["/sys/class/qcom-battery/pd_verifed".to_string()],
                plugged: true,
                ..Self::default()
            }
        }
    }

    impl AddonIo for FakeIo {
        fn exists(&mut self, p: &str) -> bool {
            self.files.contains_key(p)
        }
        fn writable(&mut self, p: &str) -> bool {
            self.writable.iter().any(|x| x == p)
        }
        fn read(&mut self, p: &str) -> Result<String, String> {
            self.files
                .get(p)
                .cloned()
                .ok_or_else(|| "不存在".to_string())
        }
        fn write(&mut self, p: &str, v: &str) -> Result<(), String> {
            if !self.writable.iter().any(|x| x == p) {
                return Err("不可写".to_string());
            }
            self.write_log.push((p.to_string(), v.to_string()));
            // 模拟真实驱动：只接受 0/1
            if v != "0" && v != "1" {
                return Err("Invalid argument".to_string());
            }
            self.files.insert(p.to_string(), v.to_string());
            Ok(())
        }
        fn is_plugged(&mut self) -> bool {
            self.plugged
        }
        fn spawn_detached(&mut self, args: &[&str]) -> Result<(), String> {
            self.spawned
                .push(args.iter().map(|s| (*s).to_string()).collect());
            Ok(())
        }
    }

    fn enabled() -> PpsConfig {
        PpsConfig {
            enabled: true,
            ..PpsConfig::default()
        }
    }

    fn probed(cfg: PpsConfig, io: &mut FakeIo) -> Pps {
        let mut p = Pps::new(cfg);
        assert!(p.probe(io).is_supported(), "假树应当支持");
        p
    }

    #[test]
    fn 探测命中qcom节点() {
        let mut io = FakeIo::qcom();
        let mut p = Pps::new(PpsConfig::default());
        let o = p.probe(&mut io);
        assert!(o.is_supported());
        assert_eq!(p.platform(), Some(Platform::Qcom));
        assert_eq!(
            p.node_pd_verified(),
            Some("/sys/class/qcom-battery/pd_verifed")
        );
    }

    #[test]
    fn 节点全无时判定不支持() {
        let mut io = FakeIo::default();
        let mut p = Pps::new(PpsConfig::default());
        let o = p.probe(&mut io);
        assert!(!o.is_supported());
        assert!(o.message().contains("PD 认证位"));
    }

    /// 启用后首拍就写 1（充电中）
    #[test]
    fn 启用后写入解锁() {
        let mut io = FakeIo::qcom();
        let mut p = probed(enabled(), &mut io);

        let out = p.tick(&mut io, 0);
        assert!(out.is_ok(), "{:?}", out.error);
        assert_eq!(
            io.files.get("/sys/class/qcom-battery/pd_verifed"),
            Some(&"1".to_string())
        );
    }

    /// 🔴 核心纪律：充电中不重复写
    #[test]
    fn 充电中不重复写节点() {
        let mut io = FakeIo::qcom();
        let mut p = probed(enabled(), &mut io);

        p.tick(&mut io, 0);
        let after_first = io.write_log.len();

        for t in 1..8 {
            p.tick(&mut io, t);
        }
        assert_eq!(io.write_log.len(), after_first, "后续拍不该再写 pd_verifed");
    }

    /// 未启用 → 不写 1
    #[test]
    fn 未启用时不写解锁() {
        let mut io = FakeIo::qcom();
        let mut p = probed(PpsConfig::default(), &mut io);
        p.tick(&mut io, 0);
        assert!(
            !io.write_log.iter().any(|(_, v)| v == "1"),
            "未启用不该解锁：{:?}",
            io.write_log
        );
    }

    /// 关闭功能且拔电 ⇒ 写 0 还原
    #[test]
    fn 关闭且拔电时还原() {
        let mut io = FakeIo::qcom();
        let mut p = probed(enabled(), &mut io);
        p.tick(&mut io, 0); // 解锁

        io.plugged = false;
        let out = p.release(&mut io);
        assert!(out.wrote, "应当还原");
        assert_eq!(
            io.files.get("/sys/class/qcom-battery/pd_verifed"),
            Some(&"0".to_string())
        );
    }

    /// 关闭但充电中 ⇒ 不动（避免打断协商）
    #[test]
    fn 关闭但充电中不还原() {
        let mut io = FakeIo::qcom();
        let mut p = probed(enabled(), &mut io);
        p.tick(&mut io, 0);

        let before = io.write_log.len();
        let out = p.release(&mut io);
        assert!(!out.wrote, "充电中不该还原");
        assert_eq!(io.write_log.len(), before);
    }

    /// 门控满足时触发一次广播子进程
    #[test]
    fn 门控满足时广播一次() {
        let mut io = FakeIo::qcom();
        let mut p = probed(enabled(), &mut io);

        p.tick(&mut io, 0);
        assert_eq!(io.spawned.len(), 1, "应启动一次 burst 子进程");
        assert_eq!(io.spawned[0][0], "pps");
        assert_eq!(io.spawned[0][1], "burst");

        // 后续拍不重复广播
        p.tick(&mut io, 1);
        p.tick(&mut io, 2);
        assert_eq!(io.spawned.len(), 1, "同一次插电只放一次动画");
    }

    /// 门控不满足（非公版头）⇒ 不广播
    #[test]
    fn 非公版头不广播() {
        let mut io = FakeIo::qcom();
        io.files.insert(
            "/sys/class/qcom-battery/adapter_svid".to_string(),
            "1234".to_string(),
        );
        let mut p = probed(enabled(), &mut io);
        p.tick(&mut io, 0);

        assert!(io.spawned.is_empty(), "非公版头不该伪造动画");
        let d = p.status().detail;
        assert_eq!(d["broadcast_ready"], false);
    }

    /// 关闭广播开关 ⇒ 不广播但仍解锁
    #[test]
    fn 关掉广播开关只解锁不广播() {
        let mut io = FakeIo::qcom();
        let cfg = PpsConfig {
            forge_broadcast: false,
            ..enabled()
        };
        let mut p = probed(cfg, &mut io);
        p.tick(&mut io, 0);

        assert!(io.spawned.is_empty());
        assert_eq!(
            io.files.get("/sys/class/qcom-battery/pd_verifed"),
            Some(&"1".to_string()),
            "节点仍要解锁"
        );
    }

    /// `auto` 模式：非 PPS 头不解锁
    #[test]
    fn auto模式非pps不解锁() {
        let mut io = FakeIo::qcom();
        io.files.insert(
            "/sys/class/qcom-battery/real_type".to_string(),
            "USB".to_string(),
        );
        let cfg = PpsConfig {
            mode: PpsMode::Auto,
            ..enabled()
        };
        let mut p = probed(cfg, &mut io);
        p.tick(&mut io, 0);

        assert!(
            !io.write_log.iter().any(|(_, v)| v == "1"),
            "auto 模式不该动非 PPS 头"
        );
    }

    /// 🔴 **回归守卫**：`auto` 模式空转之后，切回「锁定」必须能写
    ///
    /// # 这个 bug 真机踩过（2026-10-20，K70 Pro）
    ///
    /// 当时的 `auto` 分支调了 `state.on_tick()` 却丢弃返回值 ⇒
    /// `engaged` 被置 `true`，节点却没写。用户随后切到「锁定」，
    /// 状态机认为"已解锁"而**永远不写** —— 表现就是
    /// **「开启 PPS 解锁无效」**，且 `write_count` 一直是 0。
    #[test]
    fn auto空转后切回锁定仍能解锁() {
        let mut io = FakeIo::qcom();
        // 先让 real_type 不是 PPS，模拟"自动模式下不动"
        io.files.insert(
            "/sys/class/qcom-battery/real_type".to_string(),
            "USB".to_string(),
        );
        let cfg_auto = PpsConfig {
            mode: PpsMode::Auto,
            ..enabled()
        };
        let mut p = probed(cfg_auto, &mut io);

        // auto 模式跑几拍（不写）
        for t in 0..3 {
            p.tick(&mut io, t);
        }
        assert!(io.write_log.is_empty(), "auto 不满足条件时不该写");
        assert!(
            !p.status().detail["engaged"].as_bool().unwrap_or(true),
            "auto 空转后 engaged 必须是 false（否则后面的锁定就废了）"
        );

        // 切回「锁定」
        p.set_config(enabled());
        let out = p.tick(&mut io, 10);

        assert!(out.is_ok(), "{:?}", out.error);
        assert_eq!(
            io.files.get("/sys/class/qcom-battery/pd_verifed"),
            Some(&"1".to_string()),
            "切回锁定后必须真的写进去"
        );
        assert_eq!(p.write_count(), 1, "write_count 必须从 0 变成 1");
    }

    /// `auto` 模式之前写过 1、之后不满足了 ⇒ 拔电时应当还原成 0
    #[test]
    fn auto模式由满足变不满足时拔电还原() {
        let mut io = FakeIo::qcom(); // real_type = PD_PPS
        let cfg_auto = PpsConfig {
            mode: PpsMode::Auto,
            ..enabled()
        };
        let mut p = probed(cfg_auto, &mut io);

        p.tick(&mut io, 0); // PPS + auto ⇒ 解锁
        assert_eq!(
            io.files.get("/sys/class/qcom-battery/pd_verifed"),
            Some(&"1".to_string())
        );

        // 换个非 PPS 头（real_type 变了），并拔电
        io.files.insert(
            "/sys/class/qcom-battery/real_type".to_string(),
            "USB".to_string(),
        );
        io.plugged = false;

        p.tick(&mut io, 1);

        assert_eq!(
            io.files.get("/sys/class/qcom-battery/pd_verifed"),
            Some(&"0".to_string()),
            "不再满足 auto 条件时应当还原，而不是留着 1"
        );
    }

    /// `auto` 模式：PPS 头照常解锁
    #[test]
    fn auto模式pps解锁() {
        let mut io = FakeIo::qcom();
        let cfg = PpsConfig {
            mode: PpsMode::Auto,
            ..enabled()
        };
        let mut p = probed(cfg, &mut io);
        p.tick(&mut io, 0);
        assert_eq!(
            io.files.get("/sys/class/qcom-battery/pd_verifed"),
            Some(&"1".to_string())
        );
    }

    /// 拔出后再插上 ⇒ 可以再广播一次
    #[test]
    fn 重新插电可再广播() {
        let mut io = FakeIo::qcom();
        let mut p = probed(enabled(), &mut io);

        p.tick(&mut io, 0);
        assert_eq!(io.spawned.len(), 1);

        // 拔出
        io.plugged = false;
        p.tick(&mut io, 1);
        // 再插上
        io.plugged = true;
        p.tick(&mut io, 2);

        assert_eq!(io.spawned.len(), 2, "新一次插电应重新放动画");
    }

    #[test]
    fn 状态快照含关键字段() {
        let mut io = FakeIo::qcom();
        let mut p = probed(enabled(), &mut io);
        p.tick(&mut io, 0);

        let s = p.status();
        assert_eq!(s.id, "pps");
        assert!(s.supported, "本机应判定为支持");
        assert!(s.enabled, "已启用");
        assert_eq!(s.detail["platform"], "qcom");
        assert_eq!(s.detail["mode"], "锁定");
        assert!(s.detail["session"].as_str().unwrap().contains("充电"));
    }

    #[test]
    fn 使用前提包含mipps警示() {
        let p = Pps::new(PpsConfig::default());
        let req = p.spec().requires.expect("必须有使用前提");
        assert!(req.contains("MIPPS"), "必须警示原装头冲突：{req}");
        assert!(req.contains("自动"), "要给出保守替代方案：{req}");
    }
}
