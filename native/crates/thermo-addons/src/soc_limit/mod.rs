//! 附加功能 ①：充电上限（`soc_limit`）
//!
//! 设定"充到指定电量即停止"。机制来自 HyperSC 逆向（见 [`logic`] 的模块文档），
//! 本模块**独立实现**，不引用其任何代码。
//!
//! # 它碰什么、不碰什么
//!
//! | 维度 | 值 |
//! |---|---|
//! | 写节点 | `smart_chg`（控制字） |
//! | 读节点 | 同目录 `soc_limit`（生效值） |
//! | **不碰** | 档位 / 电流 / 温度阈值 / 任何温控节点 |
//! | 周期 | 启用时 15 秒；未启用时 60 秒（只为响应配置变更） |
//! | 失败后果 | 最多"没限制住"，不会伤害设备 |

pub mod config;
pub mod logic;

use serde_json::json;

use crate::addon::{Addon, AddonError, AddonSpec, AddonStatus, ProbeOutcome, TickOutcome};
use crate::io::AddonIo;
pub use config::SocLimitConfig;
use config::IDLE_INTERVAL_S;

/// `smart_chg` 的候选路径（**顺序即优先级**）
///
/// ⚠️ 路径**因机型而异**，必须运行期探测 —— 硬编码单一路径会让部分机型的
/// 充电上限功能直接失效。这与温控侧的候选表**刻意分开维护**（互不依赖）。
pub const SMART_CHG_CANDIDATES: &[&str] = &[
    // 小米 HyperOS 主路径（K90 Pro Max / Android 17 命中）
    "/sys/class/xm_power/charger/smart_charge/smart_chg",
    // 面向其他机型的兼容回退
    "/sys/class/charger/smart_charge/smart_chg",
];

/// **使用前提**文案（界面与 `limit show` 共用同一份）
///
/// 用户明确要求（2026-10-16）：「加上提示：此模块需打开电池健康保护功能」。
///
/// ## 为什么这不是"多余的提醒"
///
/// `smart_charge/*` 这一组节点属于系统「电池保护」功能的实现 ——
/// 系统设置里的「智能断充」（默认 80%）改的就是同一个节点。
/// 开关关掉时驱动是否仍执行 SOC 限制**不可依赖**，因此这是**真实前提**，
/// 不是免责声明。
///
/// ## 措辞为什么把两个名字都写上
///
/// 同一个开关在不同 HyperOS 版本 / 机型上叫法不同：
///
/// | 叫法 | 出现位置 |
/// |---|---|
/// | **电池健康保护** | 部分新版设置项名称 |
/// | **电池保护** | 设置路径与系统组件名（`ChargeProtectFragment`） |
///
/// 只写一个名字，另一个版本的用户会在设置里**找不到**，然后以为
/// 提示写错了。写上两个 + 路径，任何版本都能对上。
pub const REQUIRES_BATTERY_PROTECT: &str =
    "需先在系统设置中打开「电池健康保护」（设置 → 省电与电池 → 电池保护）";

/// **系统界面不同步**的说明（界面用）
///
/// 这是 HyperSC 逆向里明确列为"必须处理的边界"的一条：
/// HyperSC 靠 hook `ChargeProtectFragment` 把界面上的 80% 改成用户设的值，
/// 而**本模块没有 hook 层**（纯 root + sysfs），改不了系统界面。
///
/// 不说清楚的话，用户设了 90% 却看到系统设置里写着 80%，
/// 会判断成"没生效" —— 而实际上限制**已经生效**了（写的是同一个节点）。
pub const NOTE_UI_NOT_SYNCED: &str =
    "系统设置里显示的百分比不会跟着变（那是系统自己的文案）。以本页的「已生效」为准。";

/// 由控制节点推出"生效值"节点（同目录的 `soc_limit`）
fn read_node_for(ctrl: &str) -> String {
    match ctrl.rfind('/') {
        Some(idx) => format!("{}/soc_limit", &ctrl[..idx]),
        None => "soc_limit".to_string(),
    }
}

/// 充电上限功能
#[derive(Debug, Clone)]
pub struct SocLimit {
    /// 配置（来自 `config.json` 的 `limit` 分区）
    config: SocLimitConfig,
    /// 命中的控制节点路径（探测后填充）
    node_ctrl: Option<String>,
    /// 对应的生效值节点路径
    node_read: Option<String>,
    /// 探测结果
    support: Option<ProbeOutcome>,
    /// 最近一次读到的生效上限
    last_seen: Option<u8>,
    /// 最近一次写入的上限
    last_written: Option<u8>,
    /// 累计写入次数
    write_count: u64,
    /// **本机支持的上限区间**（%），由宿主从检测档案注入
    ///
    /// `None` = 尚未注入（检测未跑过），此时用兜底区间
    /// `SOC_FALLBACK_MIN ~ SOC_FALLBACK_MAX`。
    ///
    /// ⚠️ 这个区间**因机型而异**：初版把它写死成 `60 ~ 100`
    /// （本机 K90PM 的值），换机型就会用错范围。
    range: Option<(u8, u8)>,
}

impl SocLimit {
    /// 用给定配置构造（尚未探测）
    pub fn new(config: SocLimitConfig) -> Self {
        Self {
            config,
            node_ctrl: None,
            node_read: None,
            support: None,
            last_seen: None,
            last_written: None,
            write_count: 0,
            range: None,
        }
    }

    /// 注入**本机支持的上限区间**（%）
    ///
    /// 由宿主在加载档案后调用（见 `addon_host::reload_config`）。
    /// 不注入时用兜底区间 —— 那种情况只在检测尚未跑过时出现。
    pub fn set_range(&mut self, min: u8, max: u8) {
        self.range = Some((min, max));
    }

    /// 当前生效的区间（本机 or 兜底）
    fn effective_range(&self) -> (u8, u8) {
        self.range
            .unwrap_or((config::SOC_FALLBACK_MIN, config::SOC_FALLBACK_MAX))
    }

    /// 当前配置
    pub fn config(&self) -> &SocLimitConfig {
        &self.config
    }

    /// 覆盖配置（**不会**自动写节点 —— 下一拍 `tick` 才会生效）
    pub fn set_config(&mut self, config: SocLimitConfig) {
        self.config = config;
    }

    /// 命中的控制节点路径
    pub fn node_ctrl(&self) -> Option<&str> {
        self.node_ctrl.as_deref()
    }

    /// 命中的读取节点路径
    pub fn node_read(&self) -> Option<&str> {
        self.node_read.as_deref()
    }

    /// 最近一次读到的生效上限
    pub fn last_seen(&self) -> Option<u8> {
        self.last_seen
    }

    /// 累计写入次数
    pub fn write_count(&self) -> u64 {
        self.write_count
    }

    /// 只读地把当前生效值刷新一次（供 `limit show` 之类的查询命令用）
    ///
    /// **只读，不写节点。**
    pub fn refresh_active(&mut self, io: &mut dyn AddonIo) {
        if let Some(read) = self.node_read.clone() {
            self.last_seen = io
                .read(&read)
                .ok()
                .and_then(|raw| logic::parse_active(&raw));
        }
    }
}

impl Addon for SocLimit {
    fn spec(&self) -> AddonSpec {
        AddonSpec {
            id: "soc_limit",
            label: "充电上限",
            summary: "充到指定电量即停止，保护电池寿命",
            origin:
                "节点机制来自 HyperSC（com.zephyr.smartcharge）逆向；本模块独立实现，不引用其代码",
            default_interval_s: 15,
            // ⚠️ **使用前提**（用户明确要求加上，2026-10-16）
            //
            // # 为什么这个前提是真的
            //
            // 本功能写的是 `smart_charge/smart_chg` —— 它属于系统
            // 「电池保护」（HyperOS 里也叫「电池健康保护」）功能的实现，
            // 与 `com.miui.securitycenter` 的 `ChargeProtectFragment`
            // 是同一套东西：系统 UI 改的是这个节点上的 80% 默认值，
            // 而我们改的是同一个节点上的自定义值。
            //
            // HyperSC 的闸门逻辑里就有一道对应判断：
            //   `IMiCharge.getMiChargePath("smart_chg")` 返回 `null`/`""`
            //   ⇒ 视为"设备不支持电池保护"，直接退出。
            //
            // ⇒ **系统开关关掉时，这个节点的行为不可靠**（驱动可能不执行
            //    SOC 限制）。所以必须让用户先打开它。
            //
            // ⚠️ 未插充电器时驱动**只记录不执行**（dmesg:
            //    `adapter is plugout, don't handle soc limit`），
            //    因此"没插电时看不到效果"是正常的，与开关无关。
            requires: Some(REQUIRES_BATTERY_PROTECT),
        }
    }

    fn probe(&mut self, io: &mut dyn AddonIo) -> ProbeOutcome {
        for cand in SMART_CHG_CANDIDATES {
            if io.writable(cand) {
                let read = read_node_for(cand);
                self.node_ctrl = Some((*cand).to_string());
                self.node_read = Some(read);
                let outcome = ProbeOutcome::Supported {
                    detail: format!("命中 {cand}"),
                };
                self.support = Some(outcome.clone());
                return outcome;
            }
        }

        self.node_ctrl = None;
        self.node_read = None;
        // ⚠️ 找不到节点时，**先怀疑系统开关、再怀疑机型**
        //
        // # 为什么把"电池保护没开"放在第一位
        //
        // 这两种原因的**表象完全相同**（都表现为节点不存在或不可写），
        // 但处理成本差了一个数量级：
        //
        // | 原因 | 用户要做什么 | 成本 |
        // |---|---|---|
        // | **电池保护开关没开** | 去设置里打开 | 10 秒 |
        // | 机型真的不支持 | 无解，放弃该功能 | —— |
        //
        // 若只写"本机型不支持"，绝大多数用户会直接放弃 ——
        // 而他们很可能只是**少开了一个开关**。
        //
        // 反过来写（先提示查开关）不会误导：真不支持的机型用户
        // 查完开关发现本来就是开的，自然得出"确实不支持"的结论。
        let outcome = ProbeOutcome::Unsupported {
            reason: format!(
                "未找到可写的 smart_chg 节点。请先确认系统设置里已打开「电池健康保护」\
                 （设置 → 省电与电池 → 电池保护）；若本来就开着，则本机型可能不支持充电上限\
                 （已尝试 {} 条候选路径）",
                SMART_CHG_CANDIDATES.len()
            ),
        };
        self.support = Some(outcome.clone());
        outcome
    }

    fn tick(&mut self, io: &mut dyn AddonIo, _now_s: i64) -> TickOutcome {
        let interval = self.config.interval_s();

        // ① 未启用 → **零写入**（决策 B：停止干预，一个字都不写）
        if !self.config.enabled {
            return TickOutcome::idle(interval);
        }

        // ② ⚠️ **插着充电器才巡检**
        //
        // # 为什么要加这个判据
        //
        // 限制值只可能在**充电时**被改写 —— 没插电时驱动根本不会去充，
        // 也就不存在"限制失效"的风险。而每次巡检都是一次 sysfs 读取，
        // 会打断 CPU 深度睡眠。
        //
        // 宿主（worker）据同一判据把"未充电 + 无客户端"整段进入深度阻塞，
        // 真正做到空闲时零周期性唤醒。详见 docs/13-检测功能与后台策略.md §5.3。
        //
        // ⚠️ 必须**每拍现查**（而不是缓存成字段）：缓存会在插拔瞬间过期，
        //    导致"插了电却仍在跳过"或反之。
        //
        // # 边界：插电瞬间
        //
        // 深度阻塞由 `power_supply` uevent 唤醒，插上充电器会**立即**跑一拍，
        // 此时 `is_plugged()` 为真，本函数正常工作。因此不存在
        // "插了电但限制值没同步"的窗口。
        //
        // # 边界：拔电后限制值被系统改写
        //
        // 拔电后不再巡检，若此时驱动自行改写了限制值，要等到**下次插电**
        // 才会被发现并修回。这是可接受的：
        //   · 没插电时限制值是多少都无所谓（不充电）
        //   · 下次插电的第一件事就是校验并修正（见上面"插电瞬间"）
        if !io.is_plugged() {
            return TickOutcome::idle(interval);
        }

        // ③ 配置自检（fail-closed：坏配置绝不拿去写节点）
        //
        // ⚠️ 用**本机检测出的区间**校验，而非写死的 60~100
        let (range_min, range_max) = self.effective_range();
        if let Err(e) = self.config.validate_within(range_min, range_max) {
            return TickOutcome::failed(AddonError::out_of_range(e), interval);
        }

        // ④ 节点必须已探测到
        let (Some(ctrl), Some(read)) = (self.node_ctrl.clone(), self.node_read.clone()) else {
            return TickOutcome::failed(
                AddonError::node_missing("smart_chg 节点不可用（本机型可能不支持充电上限）"),
                interval,
            );
        };

        // ⑤ 读"生效值"——**永远读 soc_limit，不读 smart_chg**
        let actual = match io.read(&read) {
            Ok(raw) => logic::parse_active(&raw),
            Err(e) => {
                return TickOutcome::failed(
                    AddonError::bad_cond(format!("读取 {read} 失败：{e}")),
                    interval,
                );
            }
        };
        self.last_seen = actual;

        // ⑥ 已一致 → 不写（这是常态：绝大多数拍什么都不做）
        if !logic::needs_rewrite(self.config.soc, actual) {
            return TickOutcome::idle(interval);
        }

        // ⑦ 写控制字 `(上限 << 16) | 0x11`
        let text = logic::format_word(logic::encode(self.config.soc));
        if let Err(e) = io.write(&ctrl, &text) {
            return TickOutcome::failed(
                AddonError::bad_cond(format!("写入 {ctrl} 失败：{e}")),
                interval,
            );
        }
        self.last_written = Some(self.config.soc);
        self.write_count = self.write_count.saturating_add(1);

        // ⑧ 回读校验（读 soc_limit）：写得下去 ≠ 生效
        let verify = io
            .read(&read)
            .ok()
            .and_then(|raw| logic::parse_active(&raw));
        self.last_seen = verify;
        if logic::needs_rewrite(self.config.soc, verify) {
            let seen = verify.map_or_else(|| "读不到".to_string(), |v| format!("{v}%"));
            return TickOutcome {
                wrote: true,
                changed: true,
                error: Some(AddonError::bad_cond(format!(
                    "写入后回读不符：期望 {}%，实得 {seen}",
                    self.config.soc
                ))),
                next_interval_s: interval,
            };
        }

        TickOutcome::wrote(interval)
    }

    fn release(&mut self, _io: &mut dyn AddonIo) -> TickOutcome {
        // 决策 B：停止干预 = **零写入**。
        // 系统自带的智能断充（默认 80%）照常工作，无需我们恢复任何东西。
        self.last_written = None;
        TickOutcome::idle(IDLE_INTERVAL_S)
    }

    fn status(&self) -> AddonStatus {
        let spec = self.spec();
        // 「已生效」的判据：**读 soc_limit**（驱动报告的生效值）
        //
        // ⚠️ 不能用 `smart_chg` 的读值 —— 它在 16/17/80 之间漂移，
        //    与写入值无稳定对应关系（真机实测确证）。详见 [`logic`] 的文档。
        //
        // 未插充电器时驱动只记录不执行，此时 `soc_limit` 可能读到一个
        // 与目标不符的值 —— 这**不是失败**，插电后自会生效。
        // 因此这里只如实报告"实际读到多少"，不在后端做"是否生效"的判决。
        let active = self.last_seen;
        let matches_target = active == Some(self.config.soc);
        AddonStatus {
            id: spec.id.to_string(),
            label: spec.label.to_string(),
            summary: spec.summary.to_string(),
            origin: spec.origin.to_string(),
            enabled: self.config.enabled,
            supported: self.supported(),
            tripped: false, // 熔断状态由 runner 覆盖
            interval_s: self.config.interval_s(),
            probe: self
                .support
                .as_ref()
                .map_or_else(|| "尚未探测".to_string(), |p| p.message().to_string()),
            // ⚠️ 与 `spec().requires` **同源** —— 界面与 CLI 拿同一份文案
            requires: spec.requires.map(str::to_string),
            detail: json!({
                "soc_target": self.config.soc,
                "soc_active": active,
                // 目标与生效是否一致（界面据此决定要不要提示"尚未生效"）
                "soc_in_effect": matches_target,
                "verify_s": self.config.verify_s,
                "keep_on_restore": self.config.keep_on_restore,
                "warn_on_hypersc": self.config.warn_on_hypersc,
                "node_ctrl": self.node_ctrl,
                "node_read": self.node_read,
                "last_written": self.last_written,
                "write_count": self.write_count,
                // 系统界面不会同步 —— 必须主动说明，否则用户看到系统的
                // 80% 会以为没生效
                "note_ui_not_synced": NOTE_UI_NOT_SYNCED,
            }),
        }
    }

    fn enabled(&self) -> bool {
        self.config.enabled
    }

    fn supported(&self) -> bool {
        self.support
            .as_ref()
            .is_some_and(ProbeOutcome::is_supported)
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
