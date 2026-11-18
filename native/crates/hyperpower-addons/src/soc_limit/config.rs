//! 充电上限功能的配置（`config.json` 的 `limit` 分区）
//!
//! 契约见 `docs/04-契约-配置说明书.md` §8。
//!
//! # 与温控配置的关系
//!
//! **同一个文件、各解析各的**（设计 §6）：温控内核的结构里**根本没有** `limit` 字段，
//! 因此它连"充电上限有配置"这件事都看不见；反过来本功能也只读自己的分区。

use serde::{Deserialize, Serialize};

/// 配置分区名（`config.json` 里的键）
pub const SECTION: &str = "limit";

// ============================================================
//  SOC 值域
// ============================================================
//
// # ⚠️ 三层范围，不要混用
//
// | 层 | 来源 | 用途 |
// |---|---|---|
// | **物理范围** | 常量 `SOC_PHYSICAL_*` | 拒绝荒谬输入（如 5% / 200%） |
// | **兜底范围** | 常量 `SOC_FALLBACK_*` | 检测尚未跑过时的保守取值 |
// | **本机范围** | 检测档案 `capabilities.soc_limit.{min,max}` | **实际校验依据** |
//
// 初版只有一个硬编码的 `60 ~ 100`（本机 K90PM 的值），换机型就会
// 用错范围。现在实际校验走**本机范围**（由宿主注入，见
// [`SocLimit::set_range`](super::SocLimit::set_range)）。

/// 物理下限（%）：低于此值对"充电上限"没有意义
pub const SOC_PHYSICAL_MIN: u8 = 20;
/// 物理上限（%）
pub const SOC_PHYSICAL_MAX: u8 = 100;

/// 兜底下限（%）—— 检测未跑过时使用
///
/// # 55 不是随便定的（2026-10-19 真机证据）
///
/// K70 Pro 的驱动在拒绝越界写入时，dmesg 会明确打出允许区间：
///
/// ```text
/// BATTERY_CHG: battery_psy_set_charge_end_threshold:
///              Charge control end_threshold should be within [55 100]
/// ```
///
/// 也就是说标准 `power_supply` 系的**下限是 55**，不是老机型
/// （小米 `smart_chg`）那套的 60。
///
/// | 值 | 后果 |
/// |---|---|
/// | 写 50 | 被驱动拒绝（`write: Invalid argument`） |
/// | 写 0 | 同样被拒绝 —— **0 不是"不限制"，而是非法值** |
///
/// ⚠️ 取 55 而不是 60：给到 60 会把"55~59"这段本来能用的区间白丢掉，
///    用户想设 58% 会被界面直接挡住。宁可放开，越界时驱动会如实报错。
/// ⚠️ 不同机型下限可能不同（有的 50、有的 60）。这里只是**检测未跑过
///    时的兜底**；检测跑过之后用档案里的值。
pub const SOC_FALLBACK_MIN: u8 = 55;
/// 兜底上限（%）
pub const SOC_FALLBACK_MAX: u8 = 100;

/// 系统自带智能断充的默认上限（%）
///
/// 模块功能关闭时，系统就按这个值工作；开启后若不指定，也默认用它。
pub const SYS_DEFAULT_SOC: u8 = 80;

/// 校验间隔下限（秒）
pub const VERIFY_MIN_S: u32 = 5;
/// 校验间隔上限（秒）
pub const VERIFY_MAX_S: u32 = 300;

// ============================================================
//  定期满充（电池校准）
// ============================================================
//
// 用户需求（2026-11-15）：「设置充电上限后，定期充满一次」：
//   · 可设置「充入 10 个完整循环后满充一次」
//   · 可设置「每星期满充一次」
//   · **10 次必须是 10 个完整循环** —— 不能插一次电源就计一次
//
// 「完整循环」采用**累计充入电量**口径：
//
// ```text
// 一个完整循环 = 累计充入 100% 电池电量
// 20% → 80% 只算 0.6 个循环（充入 60%）
// ```
//
// 为什么不用「插拔一次」：用户一天可能插拔四五次，插拔 10 次
// 远不到 10 个完整循环；那样「10 次」会在两三天内就到了，
// 满充过于频繁，反而背离"保护电池"的初衷。
//
// 为什么不用驱动自带的 `cycle_count`：各机型节点名与更新粒度不同
// （有的按整百电量才 +1），属于机型数据，违反 R3「不写死机型数据」。
// 用 SOC 增量累计不需要任何机型节点。

/// 满充目标电量（%）：解除上限、充到满
///
/// 取物理上限 100。若本机检测出的上限区间上界更低，实际写入会用
/// 「区间上界」（见 `SocLimit::full_target`）—— 驱动不支持 100 时
/// 强行写只会被拒绝。
pub const FULL_SOC: u8 = 100;

/// 一个完整循环的充入电量（%）
pub const CYCLE_PERCENT: u32 = 100;

/// 累计充入电量的封顶（%）—— 200 个完整循环
///
/// 只是防溢出的物理护栏，远大于任何有意义的阈值。
pub const CYCLE_CAP_PCT: u32 = 20_000;

/// 一天有多少秒（周满充的换算）
pub const DAY_S: i64 = 86_400;

/// `full_after_cycles` 的取值范围（0 = 关闭该条件）
pub const FULL_CYCLES_MIN: u32 = 0;
/// `full_after_cycles` 上限
pub const FULL_CYCLES_MAX: u32 = 999;

/// `full_interval_days` 的取值范围（0 = 关闭该条件）
pub const FULL_DAYS_MIN: u32 = 0;
/// `full_interval_days` 上限（一年）
pub const FULL_DAYS_MAX: u32 = 365;

/// 状态持久化的标识（宿主用它定位状态文件）
///
/// 与配置分区名 [`SECTION`] 相同，便于一眼对应；文件路径由宿主决定，
/// 附加功能本身**不碰文件系统**（R4）。
pub const STATE_ID: &str = SECTION;

/// 状态落盘的最小间隔（秒）
///
/// 充电中每拍（15 秒）SOC 都会涨，若每拍都写状态文件，一次两小时的
/// 充电就是 480 次小文件写入。节流到每分钟一次足够 —— 即使进程被杀，
/// 最多丢 1 分钟的充入量（< 2% 电量），对 10 次循环的计数没有影响。
pub const STATE_SAVE_MIN_GAP_S: i64 = 60;

/// 充电上限功能的配置
///
/// 字段缺失时一律取 [`Default`]（升级/降级不因缺键而拒绝启动）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SocLimitConfig {
    /// 是否启用该功能
    ///
    /// **出厂默认 `false`**：模块对该节点**零写入**，系统自带的智能断充照常工作。
    pub enabled: bool,
    /// 目标上限（%）
    pub soc: u8,
    /// 校验间隔（秒）：对抗系统"事件驱动式"改写
    pub verify_s: u32,
    /// 温控"交回官方"时是否保留本上限
    pub keep_on_restore: bool,
    /// 检测到**其它也在写同一节点的充电控制模块**时是否提示用户
    ///
    /// # 为什么需要这个提示
    ///
    /// `smart_chg` 是一个**全局节点** —— 任何写它的程序都在改同一个开关。
    /// 两个程序同时启用时，谁都可能在后，于是**互相覆盖**：
    /// 用户会看到"设置过一会儿自己变回去了"这种莫名现象，
    /// 却不知道是有两个程序在抢。
    ///
    /// 因此检测到就明确提示，让用户**二选一**。
    ///
    /// ⚠️ 字段名保持中性（`conflict` 而不是指名道姓）—— 会写这个节点的
    ///    程序不止一个，写死某个名字将来必然过时。
    ///
    /// ⚠️ 缺省值由**容器级** `#[serde(default)]` 提供（走 `Default` = `true`），
    ///    这里不要再写字段级 `default` —— 那会覆盖成 `false`，
    ///    于是"老配置缺这个键"变成"关闭提示"，与 `Default` 不一致。
    pub warn_on_conflict: bool,
    /// 是否把**系统显示电量同步成驱动口径的 SOC**（写 `fake_soc`）
    ///
    /// # 为什么要做成开关（2026-11-18 用户要求）
    ///
    /// 采用标准 Linux `power_supply` 的机型上，系统显示的电量
    /// （`battery/capacity`）与电量计内部的 SOC（`fg1_rsoc`）可以差
    /// 6~20%，而充电截止阈值按**后者**比较：
    ///
    /// ```text
    /// 设 80%  →  驱动按 rsoc=80 停充  →  图标那时才 74%（虚低）
    /// 反过来（本机 K70 Pro）capacity 虚高 ⇒ 图标 80 时 rsoc 才 72，还得继续充
    /// ```
    ///
    /// 开启同步后每拍写 `fake_soc = rsoc`，电池图标与驱动**同一口径**，
    /// 用户看到的数就是停止充电的那个数。
    ///
    /// # 什么时候该关掉它
    ///
    /// | 场景 | 说明 |
    /// |---|---|
    /// | 不想让系统电量被改写 | 有人更信任系统自己的算法（含校准/平滑） |
    /// | 机型没有 `fake_soc` | 写不进去，本字段无意义（`sync_available=false`） |
    ///
    /// ⚠️ 出厂默认 **`true`**：与 2026-10-20 起的既有行为一致
    ///    （那时是"只要启用就同步"）。改成 `false` 会让老用户升级后
    ///    突然发现图标又虚高了 —— 那不是用户要的。
    pub sync_display: bool,
    /// 是否启用**定期满充**（电池校准）
    ///
    /// 出厂默认 `false`：不启用时行为与旧版完全一致（零额外写入）。
    ///
    /// 开启后，满足 [`Self::full_after_cycles`] 或
    /// [`Self::full_interval_days`] 任一条件时，临时把上限放到 100%
    /// 充满一次；充满后恢复原上限并把循环计数清零。
    pub full_enabled: bool,
    /// 充入累计多少个**完整循环**后满充一次
    ///
    /// `0` = 关闭该条件。完整循环按「累计充入 100% 电量」计，
    /// **不是插拔次数**（见模块文档）。
    pub full_after_cycles: u32,
    /// 每多少天满充一次
    ///
    /// `0` = 关闭该条件。计时起点是「上次满充完成」；
    /// 从未满充过时用「功能启用时刻」，因此首次满充在启用后满 N 天。
    pub full_interval_days: u32,
}

impl Default for SocLimitConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            soc: SYS_DEFAULT_SOC,
            verify_s: 15,
            keep_on_restore: true,
            warn_on_conflict: true,
            // 与既有行为一致：启用上限时把系统显示校正成驱动口径
            sync_display: true,
            // 定期满充默认关闭：出厂行为与旧版完全一致
            full_enabled: false,
            // 用户需求里的两个默认值：10 个完整循环 / 7 天
            full_after_cycles: 10,
            full_interval_days: 7,
        }
    }
}

impl SocLimitConfig {
    /// 从 `config.json` 的 `limit` 分区解析（**独立容错**）
    ///
    /// # 规则
    ///
    /// | 情况 | 行为 |
    /// |---|---|
    /// | 分区缺失 / 为 `null` | 取默认值（即功能关闭），**不算错误** |
    /// | 字段缺失 | 取该字段默认值 |
    /// | 类型错误 | 返回 `Err` —— 调用方应**停用**该功能（fail-closed），不要拿坏配置去写节点 |
    pub fn from_section_opt(section: Option<&serde_json::Value>) -> Result<Self, String> {
        match section {
            None | Some(serde_json::Value::Null) => Ok(Self::default()),
            Some(v) => serde_json::from_value::<Self>(v.clone())
                .map_err(|e| format!("`{SECTION}` 分区解析失败：{e}")),
        }
    }

    /// 校验取值是否合法（fail-closed：不合法则拒绝应用）
    ///
    /// 用**兜底范围**（检测未跑过时的保守取值）。
    /// 已知本机范围时请用 [`validate_within`](Self::validate_within)。
    pub fn validate(&self) -> Result<(), String> {
        self.validate_within(SOC_FALLBACK_MIN, SOC_FALLBACK_MAX)
    }

    /// 用**指定的本机范围**校验
    ///
    /// `range` 来自检测档案的 `capabilities.soc_limit.{min,max}` ——
    /// 不同机型驱动的支持范围不同，因此必须按检测值校验。
    pub fn validate_within(&self, min: u8, max: u8) -> Result<(), String> {
        // 先做物理护栏（防注入的 range 本身荒谬）
        let (min, max) = (
            min.clamp(SOC_PHYSICAL_MIN, SOC_PHYSICAL_MAX),
            max.clamp(SOC_PHYSICAL_MIN, SOC_PHYSICAL_MAX),
        );
        // range 反了就退回兜底，而不是拒绝一切
        let (min, max) = if min <= max {
            (min, max)
        } else {
            (SOC_FALLBACK_MIN, SOC_FALLBACK_MAX)
        };

        if !(min..=max).contains(&self.soc) {
            return Err(format!(
                "上限 {}% 超出本机支持范围（{min}~{max}%）",
                self.soc
            ));
        }
        if !(VERIFY_MIN_S..=VERIFY_MAX_S).contains(&self.verify_s) {
            return Err(format!(
                "校验间隔 {}s 超出支持范围（{VERIFY_MIN_S}~{VERIFY_MAX_S}s）",
                self.verify_s
            ));
        }
        // 满充条件：0 = 关闭该条件；两个都 0 时即使总开关打开也永不触发
        if !(FULL_CYCLES_MIN..=FULL_CYCLES_MAX).contains(&self.full_after_cycles) {
            return Err(format!(
                "满充循环数 {} 超出支持范围（{FULL_CYCLES_MIN}~{FULL_CYCLES_MAX}）",
                self.full_after_cycles
            ));
        }
        if !(FULL_DAYS_MIN..=FULL_DAYS_MAX).contains(&self.full_interval_days) {
            return Err(format!(
                "满充间隔 {} 天超出支持范围（{FULL_DAYS_MIN}~{FULL_DAYS_MAX} 天）",
                self.full_interval_days
            ));
        }
        Ok(())
    }

    /// 每拍间隔（秒）：启用时按 `verify_s`，未启用时放慢（只需响应配置变更）
    pub fn interval_s(&self) -> u32 {
        if self.enabled {
            self.verify_s
        } else {
            IDLE_INTERVAL_S
        }
    }
}

/// 未启用时的巡检间隔（秒）
///
/// 不做事不代表不用醒来 —— 用户可能随时改配置（或经 UDS 通知），
/// 放慢到 60 秒看一眼即可，开销可忽略。
pub const IDLE_INTERVAL_S: u32 = 60;

/// 定期满充的**运行状态**（跨重启持久化）
///
/// # 为什么与配置分开
///
/// 这里记的是「已经充入了多少电量、上次满充在什么时候」—— 是**记账**，
/// 不是用户意图。写进 `config.json` 会有两个问题：
///
/// | 问题 | 后果 |
/// |---|---|
/// | 污染用户配置 | 备份/同步配置时把运行进度一起带走 |
/// | 干扰配置变更通知 | 每 15 秒改一次配置，WebUI 会不断弹"配置已变" |
///
/// ⇒ 单独落一个状态文件（路径由宿主决定，见 [`STATE_ID`]）。
///
/// 文件损坏 / 缺失时一律退回 [`Default`]：最多从头计数，
/// 绝不能让坏状态阻止充电上限正常工作。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FullChargeState {
    /// 累计充入电量（%）
    ///
    /// 每 100 = 1 个完整循环。按 SOC 正增量累计：
    /// 20% → 80% 记 60，而不是记 1 次。
    pub charged_pct: u32,
    /// 上次满充完成时间（Unix 秒）；`0` = 从未满充
    pub last_full_epoch_s: i64,
    /// 上一拍读到的 SOC（跨拍/跨会话累计的基准）
    pub last_soc: Option<u8>,
    /// 是否正在执行满充（上限临时放到 100%）
    pub pending: bool,
    /// 功能启用时刻（Unix 秒）—— 周计时在"从未满充"时的起点
    pub enabled_since_s: i64,
    /// 上次落盘时间（Unix 秒，仅用于节流）
    pub saved_at_s: i64,
}

impl FullChargeState {
    /// 已完成的**完整循环**数（整数部分）
    pub fn cycles_done(&self) -> u32 {
        self.charged_pct / CYCLE_PERCENT
    }

    /// 当前循环的进度（%）
    pub fn cycle_progress_pct(&self) -> u32 {
        self.charged_pct % CYCLE_PERCENT
    }
}
