//! 配置读写层
//!
//! 契约见 `docs/04-契约-配置说明书.md`。
//!
//! # 核心设计
//!
//! | 层 | 内容 |
//! |---|---|
//! | 文件 | `/data/adb/thermo_charge/config.json`（**单一真源**，不生成任何派生文件） |
//! | 内存 | 一份**通用 JSON 对象**（`serde_json::Value`） |
//!
//! 保持"通用 JSON"而非直接反序列化成结构体，是为了满足两条硬约束：
//!
//! 1. **未知键原样保留** —— 升级、降级都不丢字段（契约 §0 规则 4）
//! 2. **分区独立解析** —— 某个分区写坏时，只有该分区的功能停用，
//!    温控与其余功能照常（见 docs/09 §6.2）
//!
//! # 写入纪律
//!
//! 原子替换：临时文件 → `fsync` → `rename`。
//! 半写的 JSON 会让下次加载直接失败，因此这是硬要求。
//!
//! # 容错分层（docs/09 §6.2）
//!
//! ```text
//! ① 读文件 + 解析成通用 JSON   ← 只有这一步失败才算"配置文件损坏"
//! ② 温控按键取名，缺失补默认   ← 不因新版本多/少键而拒绝启动
//! ③ 附加功能各自解析自己的分区 ← 某一节坏掉只影响它自己
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{json, Value};

use thermo_ipc::ErrorCode;

/// 配置结构版本
pub const CONFIG_SCHEMA: u32 = 1;

/// 配置分区名（**新增分区时必须同时登记到 [`SECTIONS`]**）
///
/// # 为什么 `curve` 也在里面
///
/// 它虽然是**数组**而不是标量键集合，但配置文件里确实是一个独立段，
/// 用户理应能 `get --section curve` 查看当前控制曲线。
///
/// 真机上曾漏登记，导致 `get --section curve` 报"未知分区"，
/// 而配置里明明有这一段 —— 属于"列表与数据不同源"的典型缺陷。
pub const SECTIONS: &[&str] = &[
    "device", "thermal", "control", "ui", "daemon", "limit", "log", "curve",
    // 充电历史（观测归档；`get --section history` 可查其开关与保留策略）
    "history",
];

// ============================================================
//  键规格表（引擎校验与前端生成表单的唯一依据）
// ============================================================

/// 键的取值类型与范围
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum KeyKind {
    /// 布尔
    Bool,
    /// 整数（含范围）
    Int {
        /// 最小值
        min: i64,
        /// 最大值
        max: i64,
    },
    /// 浮点（含范围）
    Float {
        /// 最小值
        min: f64,
        /// 最大值
        max: f64,
    },
    /// 枚举（候选值）
    Enum(&'static [&'static str]),
    /// 数组（**只读**，不参与标量读写）
    Array,
}

/// 一个配置键的规格
#[derive(Debug, Clone, Copy)]
pub struct KeySpec {
    /// 点号键名（如 `thermal.tighten_temp`）
    pub key: &'static str,
    /// 类型与范围
    pub kind: KeyKind,
    /// 是否只读（写入返回 `E_READONLY`）
    pub readonly: bool,
}

impl KeySpec {
    /// 该键的默认值（来自出厂默认配置文件的内嵌副本）
    ///
    /// ⚠️ 默认值集中在 [`default_config`]，此处只按路径取值，
    /// 避免"两处各写一份默认值"而分叉。
    pub fn default_value(&self) -> Value {
        default_config()
            .pointer(&format!("/{}", self.key.replace('.', "/")))
            .cloned()
            .unwrap_or(Value::Null)
    }
}

const fn spec(key: &'static str, kind: KeyKind) -> KeySpec {
    KeySpec {
        key,
        kind,
        readonly: false,
    }
}

const fn ro(key: &'static str, kind: KeyKind) -> KeySpec {
    KeySpec {
        key,
        kind,
        readonly: true,
    }
}

const MODES: &[&str] = &["conservative", "official", "turbo", "custom"];
const SENSORS: &[&str] = &["cell", "virt"];
const LAWS: &[&str] = &["curve", "switch", "ramp"];

/// 执行器可选值
///
/// 🔴 2026-10-14 从 `["dual","gear","curr","readonly"]` 收敛到两个 ——
/// `gear` / `curr` 在真机实测后确认**无法正确实现**，留着等于配置在撒谎。
/// 详见 [`thermo_core::Actuator`] 的文档。
const ACTUATORS: &[&str] = &["dual", "readonly"];

const PROFILES: &[&str] = &["auto", "generic"];
const LOG_LEVELS: &[&str] = &["error", "warn", "info", "debug"];

/// **按模式独立保存**的配置分区（多模式机制的边界）
///
/// 只有列在这里的分区会随模式切换而整体替换。
///
/// | 分区 | 是否随模式 | 理由 |
/// |---|---|---|
/// | `thermal` | ✅ | 温度策略，本质就是"模式"的内容 |
/// | `control` | ✅ | 功率上限/执行器/巡检间隔同属控制策略 |
/// | `limit` | ❌ **全局** | 充电上限是**独立关注点**（docs/09）——换温控模式不该动它 |
/// | `log` `ui` `device` `daemon` | ❌ **全局** | 与温控策略无关 |
///
/// ⚠️ 这个边界是刻意的：`limit.*` 属于附加功能，与温控核心互相隔离
/// （`thermo-addons` 甚至不依赖 `thermo-core`）。若把它也做进模式，
/// 就等于让温控模式的切换去改写附加功能的配置 —— 违反那条隔离纪律。
const MODE_SCOPED_SECTIONS: &[&str] = &["thermal", "control"];

/// 是否为合法模式 id
pub fn is_valid_mode(id: &str) -> bool {
    MODES.contains(&id)
}

/// 全部模式 id（界面按此顺序展示）
pub fn mode_ids() -> &'static [&'static str] {
    MODES
}

/// 模式显示名
///
/// ⚠️ `pub`：`engine.rs` 生成"模式切换"事件消息时要用它 ——
/// 事件消息是**给用户看**的，必须中文（用户反馈 2026-10-16：
/// 「最近事件内的英文显示全部转换成中文」）。
pub fn mode_label(id: &str) -> &'static str {
    match id {
        "conservative" => "保守",
        "official" => "官方",
        "turbo" => "极速",
        _ => "自定义",
    }
}

/// 模式说明（一句话，供界面展示）
fn mode_desc(id: &str) -> &'static str {
    match id {
        "conservative" => "低温慢充，最大化电池寿命",
        "official" => "完全交由系统管理，模块不插手",
        "turbo" => "尽可能快，温度阈值放宽",
        _ => "完全由你掌控",
    }
}

/// 各模式的**出厂参数覆盖**
///
/// # 为什么需要它
///
/// 没有它的话，四个模式首次切入时参数**完全相同**（都等于出厂默认）——
/// 用户点「保守」却拿到和「自定义」一样的阈值，等于这个模式没有意义。
///
/// 这里只列出**与该模式气质相符**的键，其余一律沿用出厂默认。
///
/// # 各路线的取舍
///
/// | 模式 | 收紧温度 | 收紧后降到 | 功率上限 | 思路 |
/// |---|---|---|---|---|
/// | 保守 | **40℃**（早触发） | **3 W**（降得狠） | 30 W | 宁可慢，也要冷 |
/// | 极速 | **44℃**（晚触发） | **28 W**（降得少） | 60 W | 尽量快，能承受更热 |
/// | 官方 | —— | —— | —— | **不控制**，参数无意义 |
/// | 自定义 | 出厂默认（43℃） | 出厂默认（3 W） | 不限 | 用户说了算 |
///
/// ⚠️ 「收紧后降到」是**功率（W）**，不是电流 —— 2026-10-14 用户要求
///    改成瓦数（电流不直白）。决策层用 `I = P / U` 换算成实际电流。
///    详见 `thermo_core::Config::tighten_power_w`。
///
/// ⚠️ 这些是**建议值**，不是机型边界。真正的边界仍由检测档案钳位
/// （`clamp_to_caps` 会在装载后按 `hardware_max_c` / `current.max_ua` 修正），
/// 因此任何机型上都不会越界。
///
/// ⚠️ 也刻意**不写进 `modes`**：它们只是"首次切入时的起点"，
/// 一旦用户改过，存档就以用户的值为准。
fn mode_overrides(id: &str) -> Value {
    match id {
        "conservative" => json!({
            "thermal": {
                "tighten_temp": 40.0,
                "relax_temp": 38.0,
                "ramp_span": 3.0,
                // 超温后降到 3W —— 明显慢下来（≈0.75A @ 4.0V）
                "tighten_power_w": 3
            },
            "control": { "target_power_w": 30 }
        }),
        "turbo" => json!({
            "thermal": {
                "tighten_temp": 44.0,
                "relax_temp": 43.0,
                "ramp_span": 1.0,
                // 超温后只降到 28W —— 仍保持较快（≈7A @ 4.0V）
                "tighten_power_w": 28
            },
            "control": { "target_power_w": 60 }
        }),
        // 官方模式不控制 ⇒ 参数无所谓，沿用出厂默认即可
        // 自定义 = 完全由用户掌控 ⇒ 起点就是出厂默认
        _ => json!({}),
    }
}

/// 迁移老配置时用的**名义电压**（V）
///
/// 仅用于把旧的 `thermal.tighten_curr`（电流）换算成
/// `thermal.tighten_power_w`（功率）。理由见 `ConfigStore::load` 里的迁移注释。
///
/// ⚠️ 不要拿它参与任何**实时**计算 —— 实时换算在
/// `thermo_core::decision::tighten_curr_ua`，那里用**实测电压**。
pub const NOMINAL_VOLT_FOR_MIGRATION: f64 = 4.0;

/// **全部 43 个配置键**（契约 §1~§9）
pub const KEYS: &[KeySpec] = &[
    // ── §1 全局开关（3）──
    spec("enabled", KeyKind::Bool),
    spec("mode", KeyKind::Enum(MODES)),
    spec("observe_only", KeyKind::Bool),
    // ── §2 设备分区（6）──
    spec("device.profile", KeyKind::Enum(PROFILES)),
    spec("device.max_power_w", KeyKind::Int { min: 0, max: 500 }),
    // ⚠️ 已移除 `device.power_limit_w`
    //
    // 它与 `control.target_power_w` **语义完全重复**（都是"用户设的功率上限"），
    // 历史上一个是显示用、一个是控制用 —— 结果 `ctl status` 报的值
    // 与实际生效的值可以不一致（真机踩过：界面显示 43W，控制用的是另一个数）。
    //
    // 现在统一到 `control.target_power_w`：
    //   · 它是唯一的功率上限来源
    //   · `ctl status` 的 `power_limit_w` 字段也读它（保持前端契约不变）
    //
    // 旧配置里残留的 `device.power_limit_w` 会被原样保留但不参与任何逻辑
    // （`ConfigStore` 保留未知键），下次保存时自然消失。
    spec("device.allow_generic", KeyKind::Bool),
    spec("device.generic_ack", KeyKind::Bool),
    spec(
        "device.probe_ceiling",
        KeyKind::Int {
            min: 0,
            max: 50_000_000,
        },
    ),
    spec("device.auto_probe", KeyKind::Bool),
    // ── §3 温度控制分区（10，含只读 abs_max）──
    //
    // ⚠️ 这里的 min/max 是**格式性护栏**（拒绝明显不合理的输入），
    //    不是机型边界。真正的机型边界由 `validate_against()` 按
    //    检测档案执行 —— 例如 `tighten_temp` 必须 ≤ `hardware_max_c`。
    //
    // 初版把 max 写成了本机实测值（45.0℃ / 18.4A），后果是：
    //
    // | 机型 | 硬件允许 | 常量放行 | 结果 |
    // |---|---|---|---|
    // | 本机 K90PM | 48℃ | 45℃ | 用户设不到 46~47℃ |
    // | 硬件 55℃ 机型 | 55℃ | 45℃ | **白白少了 10℃ 余量** |
    spec("thermal.sensor", KeyKind::Enum(SENSORS)),
    spec(
        "thermal.tighten_temp",
        KeyKind::Float {
            min: 20.0,
            // 物理上限（任何手机电池都不该工作在此之上）
            max: 59.0,
        },
    ),
    spec(
        "thermal.relax_temp",
        KeyKind::Float {
            min: 20.0,
            // 须比 tighten 低 1℃，故与 tighten 同高
            max: 58.0,
        },
    ),
    spec("thermal.delay_s", KeyKind::Int { min: 0, max: 120 }),
    // ⚠️ 0 = **停止充电**（用户把「降到多少」滑块拖到最左端）
    //
    // 与下面的 `control.target_power_w` **语义相反** —— 那个 0 是"不限制"。
    // 用户拖到最左端时期望的是"别充了"，而不是"随便充"（后者用最右端
    // 的「不减速」表达）。见 `thermo_core::Config::tighten_power_w` 的文档。
    //
    // 上限 300 W 只是兜底；真正生效的上限是本机档案的
    // `capabilities.power.max_w`（由 `validate_against` / `clamp_to` 管）。
    spec("thermal.tighten_power_w", KeyKind::Int { min: 0, max: 300 }),
    spec("thermal.ramp_span", KeyKind::Float { min: 0.5, max: 5.0 }),
    spec("thermal.control_law", KeyKind::Enum(LAWS)),
    spec("thermal.virt_guard", KeyKind::Bool),
    spec(
        "thermal.virt_hard",
        KeyKind::Float {
            min: 35.0,
            max: 60.0,
        },
    ),
    // ⚠️ 只读。语义：**模块级硬保护阈值**
    //
    // | 值 | 含义 |
    // |---|---|
    // | `0.0` | 未设置 ⇒ **跟随检测到的 `hardware_max_c`**（默认） |
    // | 其它 | 显式阈值（须 ≤ `hardware_max_c`，由 `validate_against` 保证） |
    //
    // 初版这里写死 `45.0 ~ 45.0`（本机 K90PM 的值），导致换机型后
    // 硬保护阈值不跟随硬件。现在默认 0，实际值由检测档案决定。
    ro(
        "thermal.abs_max",
        KeyKind::Float {
            min: 0.0,
            max: 59.0,
        },
    ),
    // ── §4 温度曲线（1，只读数组）──
    ro("curve", KeyKind::Array),
    // ── §5 控制器分区（8）──
    spec("control.actuator", KeyKind::Enum(ACTUATORS)),
    spec("control.closed_loop", KeyKind::Bool),
    // ⚠️ 0 = **不限制功率**（默认）
    //
    // 这不是"值为 0 的功率限制"，而是"用户没启用功率上限"。
    // 决策层据此直接放行（见 `decision::power_cap_curr` 的开头判断）。
    spec("control.target_power_w", KeyKind::Int { min: 0, max: 500 }),
    spec("control.patrol_s", KeyKind::Int { min: 1, max: 60 }),
    spec("control.shift_s", KeyKind::Int { min: 0, max: 300 }),
    spec("control.feedforward", KeyKind::Float { min: 0.0, max: 2.0 }),
    spec("control.hardware_protect", KeyKind::Bool),
    spec("control.stabilize_s", KeyKind::Int { min: 0, max: 60 }),
    // ── §6 界面分区（1）──
    spec("ui.module_desc", KeyKind::Bool),
    // ── §7 守护进程分区（3）──
    spec("daemon.on_demand", KeyKind::Bool),
    spec("daemon.standby_s", KeyKind::Int { min: 5, max: 300 }),
    spec("daemon.hb_timeout_s", KeyKind::Int { min: 30, max: 300 }),
    // ── §8 充电上限分区（5，**附加功能**）──
    spec("limit.enabled", KeyKind::Bool),
    spec("limit.soc", KeyKind::Int { min: 60, max: 100 }),
    spec("limit.verify_s", KeyKind::Int { min: 5, max: 300 }),
    spec("limit.keep_on_restore", KeyKind::Bool),
    spec("limit.warn_on_hypersc", KeyKind::Bool),
    // ── §9 日志分区（3）──
    spec("log.level", KeyKind::Enum(LOG_LEVELS)),
    spec("log.csv", KeyKind::Bool),
    spec("log.max_size_kb", KeyKind::Int { min: 64, max: 4096 }),
    // ── §10 充电历史分区（3，**观测归档**，不参与控制）──
    //
    // 见 `crate::history` 模块文档：这是**独立观测数据**，
    // 与 `device.json` 无关，**绝不参与任何决策**。
    spec("history.enabled", KeyKind::Bool),
    // 保留次数：一次充电约 10KB ⇒ 30 次约 300KB。上限 500 次约 5MB。
    spec("history.keep", KeyKind::Int { min: 1, max: 500 }),
    // 采样间隔：10s（细腻）~ 300s（省地方）。默认 30s。
    spec("history.sample_s", KeyKind::Int { min: 10, max: 300 }),
];

/// 按点号键查规格
pub fn lookup(key: &str) -> Option<&'static KeySpec> {
    KEYS.iter().find(|k| k.key == key)
}

/// 键总数（自检用）
pub fn key_count() -> usize {
    KEYS.len()
}

// ============================================================
//  出厂默认配置
// ============================================================

/// 出厂默认配置（内嵌，**不依赖文件**）
///
/// ⚠️ 与 `module/src/defaults/config.json` 必须一致；
/// 由 `tests/defaults.rs` 的契约测试守护。
pub fn default_config() -> Value {
    json!({
        "schema": CONFIG_SCHEMA,
        "cfg_ver": 0,
        "updated_at": 0,
        "enabled": true,
        "mode": "custom",
        "observe_only": false,
        "device": {
            "profile": "auto",
            "max_power_w": 0,
            "allow_generic": true,
            "generic_ack": false,
            "probe_ceiling": 0,
            "auto_probe": true
        },
        "thermal": {
            "sensor": "cell",
            "tighten_temp": 43.0,
            "relax_temp": 42.0,
            "delay_s": 5,
            // 3 W ≈ 旧的 0.7 A（按 4.0 V 换算）
            "tighten_power_w": 3,
            "ramp_span": 2.0,
            // 🔴 默认是 `ramp`（线性过渡），**不是** `curve`（2026-10-14 修正）
            //
            // # 为什么必须改（这是一个真实缺陷的修复）
            //
            // 界面「温度控制」卡片暴露的三个滑块，正好就是 ramp 律的三个要素：
            //
            // | 界面上写的 | 配置键 | ramp 律里的角色 |
            // |---|---|---|
            // | 收紧温度 | `thermal.tighten_temp` | 起点 |
            // | （不暴露） | `thermal.ramp_span` | 过渡长度（默认 2℃） |
            // | **降到多少** | `thermal.tighten_power_w` | **终点** |
            //
            // 而 `curve` 律**完全不读 `tighten_power_w`**（它只用 `curve` 数组）：
            //
            // ```rust
            // ControlLaw::Curve => curve_target(…),   // ← 不看 tighten_power_w
            // ControlLaw::Ramp  => ramp_target(…),    // ← 用 tighten_power_w
            // ```
            //
            // ⇒ 默认 `curve` 时，用户在界面上设的「降到多少」**被静默忽略**：
            //
            // | 用户设 | 到 45℃ 实际降到 | 说明 |
            // |---|---|---|
            // | 10 W | **2.8 W** | 走的是 `curve` 末段，用户设定没生效 |
            // | 0（停止充电） | **2.8 W** | 同上，连"停止充电"都做不到 |
            //
            // 连带影响：四种模式的 `tighten_power_w` 差异（保守 3W / 极速 28W）**全部失效**
            // —— 「极速」模式"降得少"这个设计根本没体现出来。
            //
            // 改成 `ramp` 后，界面设的每一个值都真正生效，且
            // `curve` 仍保留（`thermal.control_law` 可显式切回，供高级用户使用）。
            "control_law": "ramp",
            "virt_guard": true,
            "virt_hard": 46.0,
            // 0 = 跟随检测到的 hardware_max_c（见 §3 的 spec 说明）
            "abs_max": 0.0
        },
        "control": {
            "actuator": "dual",
            "closed_loop": true,
            // 0 = **不限制功率**（模块核心职责是温控，限功率是可选能力）
            "target_power_w": 0,
            "patrol_s": 3,
            "shift_s": 6,
            "feedforward": 0,
            "hardware_protect": true,
            "stabilize_s": 15
        },
        "ui": { "module_desc": true },
        "daemon": { "on_demand": true, "standby_s": 30, "hb_timeout_s": 45 },
        "limit": {
            "enabled": false,
            "soc": 80,
            "verify_s": 15,
            "keep_on_restore": true,
            "warn_on_hypersc": true
        },
        "curve": [
            {"t_max": 38.0, "curr": 18400000},
            {"t_max": 40.0, "curr": 12000000},
            {"t_max": 41.0, "curr": 9000000},
            {"t_max": 42.0, "curr": 7000000},
            {"t_max": 43.0, "curr": 5000000},
            {"t_max": 44.0, "curr": 2500000},
            {"t_max": 45.0, "curr": 700000}
        ],
        "log": { "level": "info", "csv": true, "max_size_kb": 512 },
        // 充电历史（逐次充电的过程曲线）。见 `crate::history` 模块文档。
        "history": {
            "enabled": true,
            // 保留最近多少次会话（更老的自动删除，防止无限增长）
            "keep": 30,
            // 曲线采样间隔（秒）
            "sample_s": 30
        },
    })
}

// ============================================================
//  配置存储
// ============================================================

/// 配置存储（内存中的通用 JSON + 原子落盘）
#[derive(Debug, Clone)]
pub struct ConfigStore {
    path: PathBuf,
    /// 完整配置（**含未知键**，原样保留）
    value: Value,
    /// 修订号（每次落盘 +1，供前端检测"另一端改了配置"）
    pub ver: u64,
}

impl ConfigStore {
    /// 从指定路径加载；文件不存在或损坏时回退出厂默认
    ///
    /// # 容错
    ///
    /// | 情况 | 行为 |
    /// |---|---|
    /// | 文件不存在 | 用出厂默认（首次安装的正常情形） |
    /// | 文件损坏（非法 JSON） | 用出厂默认 + 记警告；**不覆盖用户的坏文件**（留待人工检查） |
    /// | 键缺失 | 补默认值（不因版本差异拒绝启动） |
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut value = match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Value>(&text) {
                Ok(v) if v.is_object() => v,
                Ok(_) => {
                    log::warn!("配置文件不是 JSON 对象，改用出厂默认：{}", path.display());
                    default_config()
                }
                Err(e) => {
                    log::warn!("配置文件解析失败，改用出厂默认：{e}");
                    default_config()
                }
            },
            Err(_) => default_config(),
        };

        // ── 一次性迁移：`thermal.tighten_curr`（μA）→ `thermal.tighten_power_w`（W）──
        //
        // 🔴 位置很关键：**必须在"逐键补默认"之前**。
        //
        // 补默认值会把缺失的 `tighten_power_w` 填成出厂默认 —— 一旦填上，
        // 迁移条件（"新键缺失"）就永远不成立，用户的旧设定会被静默丢掉。
        // （首次实现在补默认之后，单测 `老配置的收紧电流会迁移成功率`
        //   立刻抓到：期望 8 W，实得默认的 3 W。）
        //
        // 🔴 背景（2026-10-14）：用户反馈「控制页的『降到多少』是电流，
        //    不够直白，改成控制功率瓦数」。详见 `thermo_core::Config::tighten_power_w`。
        //
        // 迁移的必要性：老存档里只有 `tighten_curr`。若什么都不做，
        // "补默认值"会给 `tighten_power_w` 填出厂默认 **3 W** ——
        // **用户自己调过的值被静默丢掉**。这违反项目规则
        // 「不可静默修正」（见 `clamp_to` 的文档）。
        //
        // 做法：仅当**老键存在且新键缺失**时才换算。
        //
        // | 场景 | 行为 |
        // |---|---|
        // | 全新安装（两个键都没有） | 不迁移，走出厂默认 3 W |
        // | 老存档升级（只有老键） | 按 4.0 V 换算（见下）并**记日志** |
        // | 已迁移过（只有新键） | 不碰 |
        // | 两个键都有（迁移后又手改过老键） | **以新键为准**，不覆盖 |
        //
        // ⚠️ 换算用**固定的 4.0 V**，不读 sysfs —— 两个原因：
        //    1. `ConfigStore` 是**纯文件 IO**，不碰硬件（职责单一，也便于单测）
        //    2. 迁移只发生一次，而"当时电池电压"是个瞬时值，本来也不该采信
        //
        // 4.0 V 是锂电的典型平台电压；用旧默认值验证：
        // `0.7 A × 4.0 V = 2.8 W` —— 与新的出厂默认 3 W 基本一致，
        // 说明这个换算是自洽的。
        //
        // # ⚠️ 迁移必须**落盘**，不能只改内存
        //
        // 真机验证时发现的：迁移只改了内存里的 `value`，而 `save()` 才写盘。
        // 升级后如果不做任何会触发 `save()` 的操作，**磁盘上一直只有旧键** ——
        // 于是每次 `load()` 都要重新迁移一遍：
        //
        // | 问题 | 说明 |
        // |---|---|
        // | 依赖旧键存在 | 将来若删掉这段迁移代码，值就**静默变成出厂默认** |
        // | 状态难排查 | `cat config.json` 看到的是"没迁移"的样子，与运行值不符 |
        //
        // ⇒ 迁移发生后就写盘一次（**尽力而为**：写失败只记警告，
        //   不能让一次只读的 `ctl get` 因为磁盘满而整体失败 ——
        //   内存里的值仍然是对的）。
        let mut migrated = false;
        if value.pointer("/thermal/tighten_curr").is_some()
            && value.pointer("/thermal/tighten_power_w").is_none()
        {
            let old_ua = value
                .pointer("/thermal/tighten_curr")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                .max(0);
            // 四舍五入到整瓦；上限交给 clamp_to 按本机档案收紧
            let watts = ((old_ua as f64 / 1_000_000.0) * NOMINAL_VOLT_FOR_MIGRATION).round() as u16;
            set_path(&mut value, "thermal.tighten_power_w", json!(watts));
            log::info!(
                "配置迁移：thermal.tighten_curr {old_ua} μA → thermal.tighten_power_w {watts} W\
                 （按 {NOMINAL_VOLT_FOR_MIGRATION} V 换算；原键保留但不再参与决策）"
            );
            migrated = true;
        }

        // 🔴 同时也迁移 **`modes` 里每个模式的存档**
        //
        // 这一条是**必需的**，不是"顺手补全" —— `switch_mode` 会用该模式的
        // 存档**整体替换** `thermal` 节（见 `switch_mode` 的 ② 步）。
        //
        // 只迁顶层的后果（真机升级前核查时发现）：
        //
        // | 时刻 | `thermal.tighten_power_w` | 行为 |
        // |---|---|---|
        // | 升级后（顶层已迁移） | 3 W | 正常 |
        // | **用户切一下模式** | 存档里没有这个键 ⇒ **缺失** | `get_u16` 读成 **0** |
        // | 于是 | | **0 表示"停止充电"** ⇒ 一超温就停充 🔴 |
        // | 再重启一次 | 被补默认值填成 3 W | 又变回去（前后不一致） |
        //
        // ⇒ 不迁移模式存档，用户会在"切模式"这个普通操作上踩到静默的
        //    行为突变。这类"只在某个操作后才出现"的缺陷最难排查，必须在这里堵住。
        let mode_ids: Vec<String> = value
            .get("modes")
            .and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        for id in mode_ids {
            // ⚠️ 用**点分隔**路径给 `set_path`，用 **JSON Pointer** 给 `pointer()`
            //
            // 这两个别搞混：`set_path` 按 `.` 切分，`pointer` 要 `/a/b`。
            // 把 `/modes/x/thermal/y` 传给 `set_path` 会插入一个字面键名
            // （`set_path` 切完只有一个片段）—— 迁移静默失效、键却没少。
            // 首次实现正是这样，被单测 `模式存档的收紧电流也会迁移` 抓到。
            let old_ptr = format!("/modes/{id}/thermal/tighten_curr");
            let new_dot = format!("modes.{id}.thermal.tighten_power_w");
            if value.pointer(&old_ptr).is_some()
                && value
                    .pointer(&format!("/modes/{id}/thermal/tighten_power_w"))
                    .is_none()
            {
                let old_ua = value
                    .pointer(&old_ptr)
                    .and_then(Value::as_i64)
                    .unwrap_or(0)
                    .max(0);
                let watts =
                    ((old_ua as f64 / 1_000_000.0) * NOMINAL_VOLT_FOR_MIGRATION).round() as u16;
                set_path(&mut value, &new_dot, json!(watts));
                log::info!("配置迁移（模式 {id}）：tighten_curr {old_ua} μA → {watts} W");
                migrated = true;
            }
        }

        // 逐键补默认（缺失才补，已有不动）
        for spec in KEYS {
            if value.pointer(&ptr(spec.key)).is_none() {
                let dv = spec.default_value();
                set_path(&mut value, spec.key, dv);
            }
        }
        // 只读键强制回正确值（防止被手改后带偏控制）
        for spec in KEYS.iter().filter(|k| k.readonly) {
            if spec.kind != KeyKind::Array {
                set_path(&mut value, spec.key, spec.default_value());
            }
        }

        let ver = value.get("cfg_ver").and_then(Value::as_u64).unwrap_or(0);

        let mut store = Self { path, value, ver };

        // 迁移发生后**立即落盘**（理由见上方 `migrated` 的文档）
        if migrated {
            if let Err(e) = store.save() {
                // 尽力而为：内存值已正确，写盘失败不该让本次加载整体失败
                // （`ctl get` 是只读命令，不能因为磁盘满就报错）
                log::warn!("配置迁移落盘失败（内存值仍有效）：{e}");
            }
        }

        store
    }

    /// 使用出厂默认（不落盘）
    ///
    /// 生产路径上 `load()` 已覆盖此需求（文件缺失时内部就走默认）；
    /// 本方法主要供测试构造隔离的配置实例。
    #[cfg(test)]
    pub fn with_defaults(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            value: default_config(),
            ver: 0,
        }
    }

    /// 配置文件路径
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 完整配置（只读访问）
    pub fn raw(&self) -> &Value {
        &self.value
    }

    /// 取某个分区（**附加功能专用**）
    ///
    /// 返回 `None` 表示该分区不存在（调用方应取该功能的默认值，而不是报错）。
    pub fn section(&self, name: &str) -> Option<&Value> {
        self.value.get(name)
    }

    /// 读单个键
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.value.pointer(&ptr(key))
    }

    /// 读布尔
    pub fn get_bool(&self, key: &str) -> bool {
        self.get(key).and_then(Value::as_bool).unwrap_or(false)
    }

    /// 读整数
    pub fn get_i64(&self, key: &str) -> i64 {
        self.get(key).and_then(Value::as_i64).unwrap_or(0)
    }

    /// 读浮点
    pub fn get_f64(&self, key: &str) -> f64 {
        self.get(key).and_then(Value::as_f64).unwrap_or(0.0)
    }

    /// 读无符号整数（带上限截断）
    pub fn get_u32(&self, key: &str) -> u32 {
        let v = self.get_i64(key);
        u32::try_from(v.max(0)).unwrap_or(u32::MAX)
    }

    /// 读 `u16`（带上限截断）
    pub fn get_u16(&self, key: &str) -> u16 {
        let v = self.get_i64(key);
        u16::try_from(v.clamp(0, i64::from(u16::MAX))).unwrap_or(u16::MAX)
    }

    /// 读枚举字符串
    pub fn get_str(&self, key: &str) -> &str {
        self.get(key).and_then(Value::as_str).unwrap_or("")
    }

    /// **校验并写入单键**（fail-closed：不合法则不改）
    ///
    /// # 两层校验
    ///
    /// | 层 | 内容 | 例 |
    /// |---|---|---|
    /// | ① 单键 | 类型 + 取值区间 | `tighten_temp` 必须落在 20~45 |
    /// | ② **跨键** | 键与键之间的关系 | `relax_temp` 必须比 `tighten_temp` 低至少 1℃ |
    ///
    /// ⚠️ 第 ② 层是必须的：内核是 fail-closed 的，一旦组合非法就**每拍都拒绝决策**
    /// （表现为"模块活着但完全不工作"）。真机实测踩过这个坑 ——
    /// 单键合法（41 在区间内）而组合非法（放松 42 > 收紧 41），
    /// 结果引擎整条决策链路停摆。
    ///
    /// 需要同时改多个键时，用 [`Self::set_batch`] 一次性提交。
    pub fn set(&mut self, key: &str, raw: &str) -> Result<(), ConfigError> {
        let spec = lookup(key).ok_or_else(|| ConfigError {
            code: ErrorCode::UnknownKey,
            msg: format!("未知配置键：{key}"),
            field: Some(key.to_string()),
        })?;

        if spec.readonly {
            return Err(ConfigError {
                code: ErrorCode::Readonly,
                msg: format!("{key} 是只读键，不可修改"),
                field: Some(key.to_string()),
            });
        }

        let coerced = coerce(spec, raw)?;

        // 跨键校验：先模拟应用，再看整体是否合法
        let mut candidate = self.value.clone();
        set_path(&mut candidate, key, coerced.clone());
        self.check_combined(&candidate, key)?;

        set_path(&mut self.value, key, coerced);
        Ok(())
    }

    /// 批量写入（**任一失败则整批拒绝**）
    ///
    /// 批量提交同样做跨键校验 —— 这正是一次性改一整套相关键的正当途径
    /// （例如同时下调 `tighten_temp` 与 `relax_temp`）。
    pub fn set_batch(&mut self, pairs: &[(String, String)]) -> Result<(), ConfigError> {
        // 先全部校验（不改任何值）
        let mut staged: Vec<(String, Value)> = Vec::with_capacity(pairs.len());
        for (k, v) in pairs {
            let spec = lookup(k).ok_or_else(|| ConfigError {
                code: ErrorCode::UnknownKey,
                msg: format!("未知配置键：{k}"),
                field: Some(k.clone()),
            })?;
            if spec.readonly {
                return Err(ConfigError {
                    code: ErrorCode::Readonly,
                    msg: format!("{k} 是只读键，不可修改"),
                    field: Some(k.clone()),
                });
            }
            staged.push((k.clone(), coerce(spec, v)?));
        }

        // 跨键校验：整批应用后再看整体是否合法
        let mut candidate = self.value.clone();
        for (k, v) in &staged {
            set_path(&mut candidate, k, v.clone());
        }
        let first_key = pairs.first().map(|(k, _)| k.clone()).unwrap_or_default();
        self.check_combined(&candidate, &first_key)?;

        // 全部合法 → 一次性应用
        self.value = candidate;
        Ok(())
    }

    /// 用候选值做**跨键关系校验**（不改动自身）
    ///
    /// 走的是内核**完全相同的**校验逻辑（`thermo_core::Config::validate`），
    /// 因此不会出现"配置层放行、内核拒绝"的不一致。
    fn check_combined(&self, candidate: &Value, key: &str) -> Result<(), ConfigError> {
        let probe = Self {
            path: self.path.clone(),
            value: candidate.clone(),
            ver: self.ver,
        };

        // 先确保能转换成内核配置
        probe.to_core().map_err(|e| ConfigError {
            code: ErrorCode::Type,
            msg: format!("配置结构异常：{}", e.msg),
            field: Some(key.to_string()),
        })?;

        // 再跑内核的跨键校验
        if let Ok(core) = probe.to_core() {
            if let Err(e) = core.validate() {
                return Err(ConfigError {
                    code: ErrorCode::Collision,
                    msg: format!("{e}（若需同时修改多个键，请用 set --batch k1=v1 k2=v2）"),
                    field: Some(key.to_string()),
                });
            }
        }
        Ok(())
    }

    /// **试算**：按候选改动把两层校验都跑一遍，但不落盘、也不改动内存
    ///
    /// # 为什么命令层需要它
    ///
    /// 命令面（`commands.rs`）的职责是"尽快告诉用户行不行"。
    /// 若它只做单键范围检查、而跨键校验留到 worker 真正写入时才做，
    /// 用户会先拿到 `{"ok":true}`，随后配置却被静默拒绝 —— **前后不一致**，
    /// 且现场表现为"命令说成功但没有任何效果"。
    ///
    /// 用本方法在命令层就把两层校验都跑一遍，让失败**当场可见**。
    pub fn dry_run(&self, pairs: &[(String, String)]) -> Result<(), ConfigError> {
        let mut candidate = self.value.clone();
        for (k, v) in pairs {
            let spec = lookup(k).ok_or_else(|| ConfigError {
                code: ErrorCode::UnknownKey,
                msg: format!("未知配置键：{k}"),
                field: Some(k.clone()),
            })?;
            if spec.readonly {
                return Err(ConfigError {
                    code: ErrorCode::Readonly,
                    msg: format!("{k} 是只读键，不可修改"),
                    field: Some(k.clone()),
                });
            }
            let coerced = coerce(spec, v)?;
            set_path(&mut candidate, k, coerced);
        }
        let first = pairs.first().map(|(k, _)| k.clone()).unwrap_or_default();
        self.check_combined(&candidate, &first)
    }

    /// **原子落盘**（临时文件 → `fsync` → `rename`）
    pub fn save(&mut self) -> Result<()> {
        // ⚠️ 落盘前先把"当前生效参数"同步回本模式的存档。
        //
        //    否则：用户改成 41℃ → 没切模式就重启 → 改动丢失
        //    （因为顶层值与 modes[当前] 不一致，而切换时以存档为准）。
        self.sync_mode_snapshot();

        self.ver = self.ver.saturating_add(1);
        self.value["cfg_ver"] = json!(self.ver);
        self.value["updated_at"] = json!(now_unix());

        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建目录失败: {}", parent.display()))?;
        }

        let text = serde_json::to_string_pretty(&self.value).context("序列化配置失败")?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, text.as_bytes())
            .with_context(|| format!("写入临时文件失败: {}", tmp.display()))?;

        // fsync 后再 rename，避免掉电导致空文件
        if let Ok(f) = std::fs::File::open(&tmp) {
            f.sync_all().ok();
        }
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("替换配置失败: {}", self.path.display()))?;
        Ok(())
    }

    // ============================================================
    //  多模式配置（每模式一套独立参数）
    // ============================================================
    //
    // # 用户需求（2026-10-13）
    //
    // > 「当前只有 1 种配置，增加多配置选项，设置 4 种预设：
    // >   保守 官方 极速 自定义。4 种模式全部可以自定义相关温度」
    //
    // # 与旧 `presets.json` 的区别（**本次改动的要点**）
    //
    // | | 旧 preset（模板） | 新 mode（配置） |
    // |---|---|---|
    // | 语义 | 套用一次，**覆盖**当前值 | 切换到**另一套独立配置** |
    // | 各自留存 | ❌ 切走即丢 | ✅ 每套独立保存 |
    // | 改完再切 | ❌ 模板被"污染" | ✅ 改的是当前那套，互不影响 |
    //
    // 旧机制下：用户切到「极速」→ 改了几个值 → 切回「保守」，
    // 此时保守的值**已被极速覆盖过**。`modes` 让四套彻底独立。
    //
    // # 存储结构
    //
    // 顶层始终是"**当前生效**的参数" ⇒ 对外契约**完全不变**
    // （`ctl get thermal.tighten_temp` 读到的仍是生效值）。
    // 另用 `modes.<id>` 存每个模式的完整快照：
    //
    // ```jsonc
    // {
    //   "mode": "turbo",                 // 当前生效的是哪套
    //   "thermal": { ... },              // ← 生效值（顶层，契约不变）
    //   "modes": {                       // ← 各模式的独立存档
    //     "conservative": { "thermal": {...}, "control": {...} },
    //     "turbo":        { "thermal": {...}, "control": {...} }
    //   }
    // }
    // ```
    //
    // ⚠️ `modes` **不是配置键**（不在 [`KEYS`] 里）—— 它是存储结构，
    //    因此键表、`config_keys` 计数与配置契约都不受影响。
    //
    // # 只同步"模式相关"的分区
    //
    // 见 [`MODE_SCOPED_SECTIONS`]：只搬 `thermal` / `control` 两节。
    // `limit.*`（充电上限）与 `log.*` 是**全局**的 —— 充电上限不该
    // 因为换了个温控模式就被改掉（它们是彼此独立的关注点）。
    //
    // # 自动同步（用户不需要手动"保存"）
    //
    // [`Self::save`] 会把当前 scoped 分区同步回 `modes[当前模式]`，
    // 所以 `ctl set thermal.tighten_temp=41` 改的就是"当前模式的值"，
    // 切走再切回来依然在。

    /// 把顶层 scoped 分区同步进 `modes[当前模式]`
    fn sync_mode_snapshot(&mut self) {
        let mode = self.current_mode().to_string();

        let mut snap = serde_json::Map::new();
        for sec in MODE_SCOPED_SECTIONS {
            if let Some(v) = self.value.get(*sec) {
                snap.insert((*sec).to_string(), v.clone());
            }
        }

        if !self.value.get("modes").is_some_and(Value::is_object) {
            self.value["modes"] = Value::Object(serde_json::Map::new());
        }
        if let Some(modes) = self.value.get_mut("modes").and_then(Value::as_object_mut) {
            modes.insert(mode, Value::Object(snap));
        }
    }

    /// 切换模式：**先存旧的、再加载新的**
    ///
    /// 返回 `(旧模式, 新模式)`。
    ///
    /// ⚠️ 只改内存，**不落盘** —— 由调用方决定何时 `save()`
    /// （命令面会在切完后立即保存，见 `commands::preset`）。
    pub fn switch_mode(&mut self, target: &str) -> Result<(String, String), ConfigError> {
        if !MODES.contains(&target) {
            return Err(ConfigError {
                code: ErrorCode::Range,
                msg: format!("未知模式：{target}（可选：{}）", MODES.join(" / ")),
                field: Some("mode".to_string()),
            });
        }

        let from = self.current_mode().to_string();

        // ① 把当前生效参数存回**旧模式**的存档
        self.sync_mode_snapshot();

        // ② 取**新模式**的存档；缺失则用「出厂默认 + 该模式的出厂覆盖」
        //
        //    后者是让「保守」真的是保守、「极速」真的是极速 ——
        //    否则四个模式首次切入时参数一模一样，模式就形同虚设。
        let base = default_config();
        let over = mode_overrides(target);
        let snap = self
            .value
            .get("modes")
            .and_then(|m| m.get(target))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();

        for sec in MODE_SCOPED_SECTIONS {
            // 优先用用户存档；没有则"出厂默认 ← 被该模式的覆盖修正"
            let v = snap.get(*sec).cloned().unwrap_or_else(|| {
                let mut v = base.get(*sec).cloned().unwrap_or_else(|| json!({}));
                if let (Some(v), Some(o)) =
                    (v.as_object_mut(), over.get(*sec).and_then(Value::as_object))
                {
                    for (k, val) in o {
                        v.insert(k.clone(), val.clone());
                    }
                }
                v
            });
            if let Some(obj) = self.value.as_object_mut() {
                obj.insert((*sec).to_string(), v);
            }
        }

        // ③ 切 `mode` 键
        if let Some(obj) = self.value.as_object_mut() {
            obj.insert("mode".to_string(), json!(target));
        }
        Ok((from, target.to_string()))
    }

    /// 某模式的存档快照
    pub fn mode_snapshot(&self, id: &str) -> Option<Value> {
        self.value.get("modes").and_then(|m| m.get(id)).cloned()
    }

    /// 当前模式 id
    pub fn current_mode(&self) -> &str {
        self.value
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("custom")
    }

    /// 充电历史的运行配置（见 `crate::history`）
    pub fn history_cfg(&self) -> crate::history::HistoryCfg {
        crate::history::HistoryCfg {
            enabled: self.get_bool("history.enabled"),
            // 至少留 1 次 —— 否则归档完会立刻把刚写的那次删掉
            keep: self.get_i64("history.keep").max(1) as usize,
            sample_s: self.get_i64("history.sample_s").max(10),
        }
    }

    /// 列出全部模式（含"是否当前生效" + 各自参数）
    pub fn list_modes(&self) -> Value {
        let cur = self.current_mode();
        let items: Vec<Value> = MODES
            .iter()
            .map(|id| {
                json!({
                    "id": id,
                    "label": mode_label(id),
                    "description": mode_desc(id),
                    "current": *id == cur,
                    "values": self.mode_snapshot(id).unwrap_or(Value::Null),
                })
            })
            .collect();
        json!({ "current": cur, "modes": items })
    }

    /// 转成决策内核的配置类型
    ///
    /// ⚠️ 这是**温控核心与配置层之间唯一的桥**：内核看不见 `limit.*`，
    /// 也看不见任何附加功能的分区（docs/09 §6.1）。
    pub fn to_core(&self) -> Result<thermo_core::Config, ConfigError> {
        let bad = |k: &str, why: &str| ConfigError {
            code: ErrorCode::Type,
            msg: format!("{k} 取值异常（{why}）"),
            field: Some(k.to_string()),
        };

        let mode = match self.get_str("mode") {
            "conservative" => thermo_core::Mode::Conservative,
            "official" => thermo_core::Mode::Official,
            "turbo" => thermo_core::Mode::Turbo,
            _ => thermo_core::Mode::Custom,
        };
        let sensor = match self.get_str("thermal.sensor") {
            "virt" => thermo_core::SensorSource::Virt,
            _ => thermo_core::SensorSource::Cell,
        };
        let law = match self.get_str("thermal.control_law") {
            "switch" => thermo_core::ControlLaw::Switch,
            "ramp" => thermo_core::ControlLaw::Ramp,
            _ => thermo_core::ControlLaw::Curve,
        };
        // 执行器：`gear` / `curr` 已删除（无法正确实现，见 `Actuator` 文档）
        //
        // ⚠️ 旧配置里可能仍存着这两个值（`load()` **不校验枚举**，只补缺失键）。
        //    这里**记警告**再退回 `dual` —— 不能静默，否则用户以为
        //    "仅电流"生效了，实际行为却是双写（或反之）。
        let actuator = match self.get_str("control.actuator") {
            "readonly" => thermo_core::Actuator::Readonly,
            "dual" => thermo_core::Actuator::Dual,
            other => {
                log::warn!(
                    "配置里的 control.actuator = {other:?} 已不再支持\
                     （无法正确实现，见文档）；本次按 dual 执行。\
                     可用 `thermo ctl set control.actuator dual` 消除本警告"
                );
                thermo_core::Actuator::Dual
            }
        };

        let curve = self
            .get("curve")
            .and_then(Value::as_array)
            .ok_or_else(|| bad("curve", "不是数组"))?
            .iter()
            .map(|p| thermo_core::CurvePoint {
                t_max: p.get("t_max").and_then(Value::as_f64).unwrap_or(0.0) as f32,
                curr: p.get("curr").and_then(Value::as_i64).unwrap_or(0) as i32,
            })
            .collect();

        let cfg = thermo_core::Config {
            enabled: self.get_bool("enabled"),
            observe_only: self.get_bool("observe_only"),
            mode,
            sensor,
            tighten_temp: self.get_f64("thermal.tighten_temp") as f32,
            relax_temp: self.get_f64("thermal.relax_temp") as f32,
            delay_s: self.get_u32("thermal.delay_s"),
            tighten_power_w: self.get_u16("thermal.tighten_power_w"),
            ramp_span: self.get_f64("thermal.ramp_span") as f32,
            control_law: law,
            virt_guard: self.get_bool("thermal.virt_guard"),
            virt_hard: self.get_f64("thermal.virt_hard") as f32,
            actuator,
            closed_loop: self.get_bool("control.closed_loop"),
            target_power_w: self.get_u16("control.target_power_w"),
            patrol_s: self.get_u32("control.patrol_s").max(1),
            shift_s: self.get_u32("control.shift_s"),
            hardware_protect: self.get_bool("control.hardware_protect"),
            curve,
        };
        Ok(cfg)
    }

    /// 按**设备档案的能力边界**钳位温控参数（契约 §0.1）
    ///
    /// # 何时用
    ///
    /// 档案变更（例如用户跑了检测、换了一份档案）后，既有配置可能越界。
    /// 此时**加载即钳位**，而不是等控制时才发现。
    ///
    /// ⚠️ 返回被钳位的项列表；调用方**必须记事件**（`config_clamped`），
    /// **不可静默修正**。
    ///
    /// # 注意
    ///
    /// 只钳位**温控相关**的键；附加功能的分区由各自的功能自行校验
    /// （例如 `limit.soc` 由充电上限功能检查值域）。
    pub fn clamp_to_caps(&mut self, caps: &thermo_core::Capabilities) -> Vec<String> {
        let mut clamped: Vec<String> = Vec::new();
        // 收集待写入的修正，最后统一应用 —— 避免闭包与 `self.value` 的可变借用冲突
        let mut fixes: Vec<(String, Value)> = Vec::new();

        // ① 目标功率 ≤ 设备最大功率（用户最关心的那条）
        //
        // ⚠️ 0 = 用户**未启用**功率上限，不是"限制到 0W"，不要钳它
        if let Some(max_w) = caps.power.max_w {
            let cur = self.get_u16("control.target_power_w");
            if cur > 0 && cur > max_w {
                clamped.push(format!("control.target_power_w: {cur} → {max_w}"));
                fixes.push(("control.target_power_w".into(), json!(max_w)));
            }
        }

        // ② 收紧功率 ≤ 本机功率上限
        //
        // ⚠️ 这里**要**钳 0：`thermal.tighten_power_w` 的 0 表示"停止充电"，
        //    是**有意义的用户选择**，不是"未设置"。只有当它**大于本机上限**
        //    时才需要修正。
        if let Some(max_w) = caps.power.max_w {
            let cur = self.get_u16("thermal.tighten_power_w");
            if cur > max_w {
                clamped.push(format!("thermal.tighten_power_w: {cur} → {max_w}"));
                fixes.push(("thermal.tighten_power_w".into(), json!(max_w)));
            }
        }

        // ③ 温度曲线：**整体等比缩放**（与 `thermo-core::Config::clamp_to` 同语义）
        //
        // ⚠️ 不能"逐项钳到上限" —— 那会把曲线压平、形状失真：
        //
        // | 做法 | K90PM（18.4A） | 67W 机型（11A） |
        // |---|---|---|
        // | ❌ 逐项钳 | 不变 | 首段 18.4→11，**其余段不动** ⇒ 曲线走形 |
        // | ✅ 等比缩放 | ×1.00 | ×0.598 ⇒ 形状完整保留 |
        //
        // 真机踩过：逐项钳把出厂曲线变成了
        // `[5A, 5A, 5A, 5A, 5A, 2.5A, 0.7A]`（前五段全被压平）。
        //
        // ⚠️ 注意：`curve` 仍然以**电流**（μA）表达，不跟随
        //    `tighten_power_w` 改成功率。理由：曲线是"温度 → 速度占比"的
        //    **形状**，而 `Config::curve` 的类型是 `Vec<CurvePoint{ curr }>`；
        //    把它也改成功率需要动核心层的类型与全部单测，收益仅是
        //    "配置文件内部一致"。它对用户不可见（界面不暴露曲线编辑）。
        let max_ua = caps.current.max_ua;
        if let Some(arr) = self.value.get("curve").and_then(Value::as_array).cloned() {
            let peak = arr
                .iter()
                .filter_map(|p| p.get("curr").and_then(Value::as_i64))
                .max()
                .unwrap_or(0);

            if peak > i64::from(max_ua) && peak > 0 {
                let scale = f64::from(max_ua) / peak as f64;
                let mut new_arr = arr;
                for p in new_arr.iter_mut() {
                    let Some(cur) = p.get("curr").and_then(Value::as_i64) else {
                        continue;
                    };
                    let scaled = (cur as f64 * scale).round() as i64;
                    if let Some(obj) = p.as_object_mut() {
                        obj.insert("curr".into(), json!(scaled));
                    }
                }
                clamped.push(format!(
                    "curve: 整体缩放 ×{scale:.3}（峰值 {peak} → {max_ua} μA，适配本机电流上限）"
                ));
                fixes.push(("curve".into(), Value::Array(new_arr)));
            }
        }

        // ④ 收紧温度 ≤ 硬件保护温度
        let hw_max = f64::from(caps.thermal.hardware_max_c);
        let cur = self.get_f64("thermal.tighten_temp");
        if cur > hw_max {
            clamped.push(format!("thermal.tighten_temp: {cur} → {hw_max}"));
            fixes.push(("thermal.tighten_temp".into(), json!(hw_max)));
        }

        for (key, value) in fixes {
            set_path(&mut self.value, &key, value);
        }
        clamped
    }
}

/// 配置层错误（**复用契约既有的错误码**，不新增退出码）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    /// 错误码
    pub code: ErrorCode,
    /// 人话说明
    pub msg: String,
    /// 相关键（可选）
    pub field: Option<String>,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code.as_str(), self.msg)
    }
}

impl std::error::Error for ConfigError {}

/// 把字符串按规格强制转成 JSON 值（含范围校验）
fn coerce(spec: &KeySpec, raw: &str) -> Result<Value, ConfigError> {
    let raw = raw.trim();
    let err = |msg: String| ConfigError {
        code: ErrorCode::Range,
        msg,
        field: Some(spec.key.to_string()),
    };
    let type_err = |msg: String| ConfigError {
        code: ErrorCode::Type,
        msg,
        field: Some(spec.key.to_string()),
    };

    match spec.kind {
        KeyKind::Bool => match raw {
            "true" | "1" | "on" | "yes" => Ok(json!(true)),
            "false" | "0" | "off" | "no" => Ok(json!(false)),
            _ => Err(type_err(format!(
                "{} 需要布尔值（true/false），实得 {raw:?}",
                spec.key
            ))),
        },
        KeyKind::Int { min, max } => {
            let v: i64 = raw
                .parse()
                .map_err(|_| type_err(format!("{} 需要整数，实得 {raw:?}", spec.key)))?;
            if v < min || v > max {
                return Err(err(format!("{} = {v} 超出范围（{min} ~ {max}）", spec.key)));
            }
            Ok(json!(v))
        }
        KeyKind::Float { min, max } => {
            let v: f64 = raw
                .parse()
                .map_err(|_| type_err(format!("{} 需要小数，实得 {raw:?}", spec.key)))?;
            if !v.is_finite() || v < min || v > max {
                return Err(err(format!("{} = {v} 超出范围（{min} ~ {max}）", spec.key)));
            }
            Ok(json!(v))
        }
        KeyKind::Enum(allowed) => {
            if allowed.contains(&raw) {
                Ok(json!(raw))
            } else {
                Err(err(format!(
                    "{} 只接受 {}，实得 {raw:?}",
                    spec.key,
                    allowed.join(" / ")
                )))
            }
        }
        KeyKind::Array => Err(ConfigError {
            code: ErrorCode::Readonly,
            msg: format!("{} 是数组类型，不支持标量写入", spec.key),
            field: Some(spec.key.to_string()),
        }),
    }
}

/// 点号键 → JSON Pointer
fn ptr(key: &str) -> String {
    format!("/{}", key.replace('.', "/"))
}

/// 按点号键写入（自动创建中间对象）
fn set_path(root: &mut Value, key: &str, v: Value) {
    let parts: Vec<&str> = key.split('.').collect();
    let (last, parents) = match parts.split_last() {
        Some((l, p)) => (*l, p),
        None => return,
    };

    let mut cur = root;
    for p in parents {
        if !cur.is_object() {
            *cur = json!({});
        }
        // 先确保是对象，再取可变引用 —— 分两步避免可变/不可变借用冲突
        let Some(obj) = cur.as_object_mut() else {
            return;
        };
        cur = obj.entry((*p).to_string()).or_insert_with(|| json!({}));
    }

    if !cur.is_object() {
        *cur = json!({});
    }
    if let Some(obj) = cur.as_object_mut() {
        obj.insert(last.to_string(), v);
    }
}

/// 当前 Unix 秒
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 分区键统计（供 `data stats` 与自检）
pub fn section_key_counts() -> BTreeMap<&'static str, usize> {
    let mut m = BTreeMap::new();
    for spec in KEYS {
        let section = spec.key.split('.').next().unwrap_or(spec.key);
        let name = if spec.key.contains('.') {
            section
        } else {
            "(全局)"
        };
        *m.entry(name).or_insert(0) += 1;
    }
    m
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn 键总数为四十三() {
        // 43 = 40（原 41，移除重复的 `device.power_limit_w`）+ 3（充电历史分区）
        //
        // 移除理由见 KEYS 里的注释：它与 `control.target_power_w`
        // 语义完全重复，且曾导致"界面显示的值 ≠ 实际生效的值"。
        //
        // 新增的 3 个见 §10：`history.enabled` / `history.keep` / `history.sample_s`。
        assert_eq!(
            key_count(),
            43,
            "移除 device.power_limit_w 并加入充电历史后合计 43 个键"
        );
    }

    // ────────────────────────────────────────────────────────────
    //  执行器（2026-10-14 收敛为 dual / readonly）
    // ────────────────────────────────────────────────────────────

    /// `gear` / `curr` 已不再支持，但旧配置不能**静默**改变含义
    ///
    /// `load()` 不校验枚举值（只补缺失键），因此老 `config.json` 里可能
    /// 仍存着 `"curr"`。若不处理，旧配置会**悄悄**按 dual 跑，
    /// 而用户以为"仅电流"生效了。
    ///
    /// 这里验证的是**行为**：能正常构造出 Config 且执行器为 Dual
    /// （警告由 `to_core()` 的 `log::warn!` 输出，不在此断言）。
    #[test]
    fn 旧的执行器值退回dual而非静默报错() {
        for legacy in ["gear", "curr"] {
            let mut s = ConfigStore::with_defaults("x.json");
            // 直接改内部 JSON：模拟旧配置（`set` 现在会拒绝这两个值）
            set_path(&mut s.value, "control.actuator", json!(legacy));

            let cfg = s
                .to_core()
                .unwrap_or_else(|e| panic!("旧配置 {legacy:?} 应能加载，实际：{e}"));
            assert_eq!(
                cfg.actuator,
                thermo_core::Actuator::Dual,
                "旧值 {legacy:?} 应退回 dual（唯一完整可用的模式）"
            );
        }
    }

    /// 新增写入必须被拒绝（不能再设成已删除的值）
    #[test]
    fn 已删除的执行器值被拒绝写入() {
        let mut s = ConfigStore::with_defaults("x.json");
        for bad in ["gear", "curr", "nonsense"] {
            let r = s.set("control.actuator", bad);
            assert!(
                r.is_err(),
                "control.actuator = {bad:?} 应被拒绝（已不支持）"
            );
        }
        // 合法值仍可写
        assert!(s.set("control.actuator", "readonly").is_ok());
        assert!(s.set("control.actuator", "dual").is_ok());
    }

    // ────────────────────────────────────────────────────────────
    //  多模式（4 套互相独立的配置）
    //
    //  用户需求（2026-10-13）：「增加多配置选项，设置 4 种预设：
    //  保守 官方 极速 自定义」。要点是**互相独立** ——
    //  切走再切回，之前改的值必须还在（旧的 preset 机制做不到）。
    // ────────────────────────────────────────────────────────────

    #[test]
    fn 模式切换会保存旧模式的值() {
        let mut s = ConfigStore::with_defaults("x.json");
        s.set("thermal.tighten_temp", "44").unwrap();
        assert_eq!(s.get_f64("thermal.tighten_temp"), 44.0);

        let (from, to) = s.switch_mode("turbo").expect("切到 turbo 应成功");
        assert_eq!(from, "custom");
        assert_eq!(to, "turbo");
        assert_eq!(s.current_mode(), "turbo");

        // custom 的 44℃ 必须被存进它的存档
        let snap = s.mode_snapshot("custom").expect("custom 应有存档");
        assert_eq!(
            snap["thermal"]["tighten_temp"].as_f64(),
            Some(44.0),
            "切走时必须先把旧模式的值存下来"
        );
    }

    #[test]
    fn 切走再切回值仍在() {
        let mut s = ConfigStore::with_defaults("x.json");
        s.set("thermal.tighten_temp", "44").unwrap();
        s.switch_mode("turbo").unwrap();
        s.switch_mode("custom").unwrap();
        assert_eq!(
            s.get_f64("thermal.tighten_temp"),
            44.0,
            "切回自定义后，之前改的 44℃ 必须还在（这正是与旧 preset 的关键区别）"
        );
    }

    #[test]
    fn 各模式参数互不干扰() {
        let mut s = ConfigStore::with_defaults("x.json");
        s.set("thermal.tighten_temp", "44").unwrap();
        s.switch_mode("turbo").unwrap();
        s.set("thermal.tighten_temp", "45").unwrap();

        s.switch_mode("custom").unwrap();
        assert_eq!(s.get_f64("thermal.tighten_temp"), 44.0, "自定义应保持 44");
        s.switch_mode("turbo").unwrap();
        assert_eq!(s.get_f64("thermal.tighten_temp"), 45.0, "极速应保持 45");
    }

    #[test]
    fn 未知模式被拒绝且不改变现状() {
        let mut s = ConfigStore::with_defaults("x.json");
        assert!(s.switch_mode("nonsense").is_err(), "未知模式必须被拒");
        assert_eq!(s.current_mode(), "custom", "被拒后不得改变当前模式");
    }

    #[test]
    fn 首次切入无存档的模式用该模式的出厂参数() {
        let mut s = ConfigStore::with_defaults("x.json");
        s.set("thermal.tighten_temp", "44").unwrap();

        // conservative 从未自定义过 ⇒ 应拿到它的**出厂参数**（40℃），
        // 而不是上一个模式的值（44℃），也不是出厂默认（43℃）
        s.switch_mode("conservative").unwrap();
        assert_eq!(
            s.get_f64("thermal.tighten_temp"),
            40.0,
            "首次切入保守模式应拿到保守的出厂阈值"
        );
        assert_eq!(s.get_f64("thermal.relax_temp"), 38.0);
        assert_eq!(
            s.get_i64("control.target_power_w"),
            30,
            "保守模式的功率上限应为 30W"
        );
    }

    #[test]
    fn 四个模式的出厂参数互不相同() {
        // 守住"每个模式有自己的气质" —— 若四个模式的出厂参数相同，
        // 模式机制就形同虚设（用户切过去发现什么都没变）
        let mut s = ConfigStore::with_defaults("x.json");

        s.switch_mode("conservative").unwrap();
        let cons = s.get_f64("thermal.tighten_temp");

        s.switch_mode("turbo").unwrap();
        let turbo = s.get_f64("thermal.tighten_temp");

        s.switch_mode("custom").unwrap();
        let custom = s.get_f64("thermal.tighten_temp");

        assert!(
            cons < custom,
            "保守的阈值应比自定义低（{cons} vs {custom}）"
        );
        assert!(
            turbo > custom,
            "极速的阈值应比自定义高（{turbo} vs {custom}）"
        );
    }

    #[test]
    fn 收紧后降到多少在各模式下不同() {
        let mut s = ConfigStore::with_defaults("x.json");

        s.switch_mode("conservative").unwrap();
        let cons = s.get_i64("thermal.tighten_power_w");

        s.switch_mode("turbo").unwrap();
        let turbo = s.get_i64("thermal.tighten_power_w");

        assert!(
            cons < turbo,
            "保守模式超温后应降得更狠（{cons} W < {turbo} W）"
        );
    }

    #[test]
    fn 模式出厂参数同样受跨键校验约束() {
        // 保守模式收紧 40 / 放宽 38 —— 必须自洽（relax < tighten）
        let mut s = ConfigStore::with_defaults("x.json");
        s.switch_mode("conservative").unwrap();
        let pairs: Vec<(String, String)> = vec![
            ("thermal.tighten_temp".into(), "40".into()),
            ("thermal.relax_temp".into(), "38".into()),
        ];
        assert!(
            s.dry_run(&pairs).is_ok(),
            "保守模式的出厂参数必须通过内核的跨键校验"
        );
    }

    /// 🔴 老存档的 `tighten_curr` 必须被迁移成 `tighten_power_w`
    ///
    /// 不做迁移的后果：老用户自定义的「降到多少」被**静默重置**为出厂默认
    /// （3 W）—— 违反项目规则「不可静默修正」。
    #[test]
    fn 老配置的收紧电流会迁移成功率() {
        let dir = std::env::temp_dir().join(format!("thermo_mig_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        // 构造一份"老版本"存档：有 tighten_curr、没有 tighten_power_w
        let mut old = default_config();
        old["thermal"]
            .as_object_mut()
            .unwrap()
            .remove("tighten_power_w");
        // 2 A × 4.0 V = 8 W
        old["thermal"]["tighten_curr"] = json!(2_000_000);
        std::fs::write(&path, serde_json::to_string_pretty(&old).unwrap()).unwrap();

        let s = ConfigStore::load(&path);

        assert_eq!(
            s.get_i64("thermal.tighten_power_w"),
            8,
            "2 A 应按名义 4.0 V 迁移成 8 W"
        );

        // 🔴 迁移必须**落盘** —— 不能只改内存
        //
        // 否则磁盘上一直只有旧键，每次 `load()` 都要重新迁移：
        // 一旦将来删掉迁移代码，用户的值就**静默变成出厂默认**。
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            after
                .pointer("/thermal/tighten_power_w")
                .and_then(Value::as_i64),
            Some(8),
            "迁移结果应已写入磁盘（否则下次加载还得重来一遍）"
        );

        // 老键保留（不删用户数据），但不再出现在契约键表里
        assert!(
            !KEYS.iter().any(|k| k.key == "thermal.tighten_curr"),
            "老键不应留在契约键表中"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 落盘只该发生**一次** —— 第二次加载不应再写
    ///
    /// 这条防的是"每次 `load()` 都写盘"：`ctl` 每个命令都会 `load()`
    /// 一次配置，若每次都写，一天能写几万次（闪存寿命无关，但毫无必要）。
    #[test]
    fn 迁移落盘只发生一次() {
        let dir = std::env::temp_dir().join(format!("thermo_mig4_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        let mut old = default_config();
        old["thermal"]
            .as_object_mut()
            .unwrap()
            .remove("tighten_power_w");
        old["thermal"]["tighten_curr"] = json!(2_000_000);
        std::fs::write(&path, serde_json::to_string_pretty(&old).unwrap()).unwrap();

        // 第一次：迁移 + 落盘（`cfg_ver` 会自增 1）
        let first = ConfigStore::load(&path);
        let ver1 = first.ver;

        // 第二次：磁盘上已有新键 ⇒ 不应再迁移
        let second = ConfigStore::load(&path);
        assert_eq!(second.ver, ver1, "第二次加载不应再写盘（cfg_ver 不该变）");
        assert_eq!(second.get_i64("thermal.tighten_power_w"), 8);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 已经迁移过的存档**不得**被再次覆盖
    #[test]
    fn 已有功率键时迁移不覆盖() {
        let dir = std::env::temp_dir().join(format!("thermo_mig2_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        let mut both = default_config();
        both["thermal"]["tighten_curr"] = json!(2_000_000); // 陈旧的旧键
        both["thermal"]["tighten_power_w"] = json!(45); // 用户已按功率设过
        std::fs::write(&path, serde_json::to_string_pretty(&both).unwrap()).unwrap();

        let s = ConfigStore::load(&path);

        assert_eq!(
            s.get_i64("thermal.tighten_power_w"),
            45,
            "新键已存在时应以它为准，不能被旧键换算覆盖"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 🔴 **模式存档**也必须迁移 —— 否则切模式会让「降到多少」变成 0（停止充电）
    ///
    /// # 这个测试锁的是一个真实的设计陷阱
    ///
    /// `switch_mode` 会用该模式的存档**整体替换** `thermal` 节。
    /// 若模式存档里没有 `tighten_power_w`：
    ///
    /// | 时刻 | 值 | 行为 |
    /// |---|---|---|
    /// | 升级后 | 3 W | 正常 |
    /// | **切模式后** | **缺失** ⇒ 读成 0 | **一超温就停充** 🔴 |
    /// | 再重启 | 补默认 3 W | 又变回去 |
    ///
    /// 即"只在用户点了切模式之后才出现"的行为突变 —— 最难排查的一类。
    #[test]
    fn 模式存档的收紧电流也会迁移() {
        let dir = std::env::temp_dir().join(format!("thermo_mig3_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        // 模拟老存档：顶层与各模式存档都只有 tighten_curr
        let mut old = default_config();
        old["thermal"]
            .as_object_mut()
            .unwrap()
            .remove("tighten_power_w");
        old["thermal"]["tighten_curr"] = json!(2_000_000); // 存 2 A ⇒ 8 W

        let mut modes = serde_json::Map::new();
        for (id, ua) in [
            ("conservative", 700_000_i64), // ⇒ 3 W
            ("custom", 2_000_000),         // ⇒ 8 W
            ("official", 0),               // ⇒ 0 W（停止充电，要保持 0）
            ("turbo", 7_000_000),          // ⇒ 28 W
        ] {
            modes.insert(
                id.to_string(),
                json!({
                    "thermal": {
                        "tighten_temp": 43.0,
                        "relax_temp": 42.0,
                        "tighten_curr": ua,
                    }
                }),
            );
        }
        old["modes"] = Value::Object(modes);
        std::fs::write(&path, serde_json::to_string_pretty(&old).unwrap()).unwrap();

        let mut s = ConfigStore::load(&path);

        // 逐个模式切过去，检查读到的功率是否正确
        for (id, want) in [
            ("conservative", 3_i64),
            ("custom", 8),
            ("official", 0),
            ("turbo", 28),
        ] {
            s.switch_mode(id).unwrap();
            assert_eq!(
                s.get_i64("thermal.tighten_power_w"),
                want,
                "切到 {id} 后「降到多少」应为 {want} W —— \
                 若为 0 说明模式存档没迁移，超温会直接停止充电"
            );
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 充电上限不随模式切换而改变() {
        // limit.* 属附加功能，刻意**不做进模式**（见 MODE_SCOPED_SECTIONS 的说明）
        let mut s = ConfigStore::with_defaults("x.json");
        s.set("limit.enabled", "true").unwrap();
        s.set("limit.soc", "85").unwrap();

        s.switch_mode("turbo").unwrap();
        assert!(
            s.get_bool("limit.enabled"),
            "充电上限开关不得被模式切换影响"
        );
        assert_eq!(s.get_i64("limit.soc"), 85, "充电上限值不得被模式切换影响");
    }

    #[test]
    fn 模式存档可跨进程持久化() {
        let dir = std::env::temp_dir().join(format!(
            "thermo_mode_persist_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        {
            let mut s = ConfigStore::load(&path);
            s.set("thermal.tighten_temp", "44").unwrap();
            s.switch_mode("turbo").unwrap();
            s.save().unwrap();
        }

        // 重新加载 ⇒ 当前模式与各模式存档都必须还在
        let s = ConfigStore::load(&path);
        assert_eq!(s.current_mode(), "turbo");
        let snap = s.mode_snapshot("custom").expect("custom 存档应已持久化");
        assert_eq!(snap["thermal"]["tighten_temp"].as_f64(), Some(44.0));

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn 模式列表含四种且只有一个当前() {
        let s = ConfigStore::with_defaults("x.json");
        let list = s.list_modes();
        let arr = list["modes"].as_array().expect("modes 应是数组");
        assert_eq!(arr.len(), 4, "应为 保守/官方/极速/自定义 四种");
        assert_eq!(list["current"], "custom");

        let cur: Vec<_> = arr.iter().filter(|m| m["current"] == true).collect();
        assert_eq!(cur.len(), 1, "有且只有一个模式是当前生效的");

        // 四个 id 必须齐全
        let ids: Vec<&str> = arr.iter().filter_map(|m| m["id"].as_str()).collect();
        for want in ["conservative", "official", "turbo", "custom"] {
            assert!(ids.contains(&want), "缺少模式：{want}");
        }
    }

    #[test]
    fn 未知模式id可被判断() {
        assert!(is_valid_mode("turbo"));
        assert!(is_valid_mode("official"));
        assert!(!is_valid_mode("turbo2"));
        assert_eq!(mode_ids().len(), 4);
    }

    #[test]
    fn 已废弃的功率键不再存在() {
        // 防止有人"顺手加回来"
        assert!(
            !KEYS.iter().any(|k| k.key == "device.power_limit_w"),
            "device.power_limit_w 已废弃 —— 功率上限统一走 control.target_power_w"
        );
    }

    #[test]
    fn 键名不重复() {
        let mut seen = std::collections::BTreeSet::new();
        for k in KEYS {
            assert!(seen.insert(k.key), "重复键：{}", k.key);
        }
    }

    #[test]
    fn 每个键在默认配置里都有值() {
        for k in KEYS {
            let v = k.default_value();
            assert!(!v.is_null(), "键 {} 在出厂默认配置里缺失", k.key);
        }
    }

    #[test]
    fn 只读键不可写() {
        let mut s = ConfigStore::with_defaults("x.json");
        let e = s.set("thermal.abs_max", "50").unwrap_err();
        assert_eq!(e.code, ErrorCode::Readonly);
        let e = s.set("curve", "[]").unwrap_err();
        assert_eq!(e.code, ErrorCode::Readonly);
    }

    #[test]
    fn 未知键报未知键错误() {
        let mut s = ConfigStore::with_defaults("x.json");
        let e = s.set("nonexistent.key", "1").unwrap_err();
        assert_eq!(e.code, ErrorCode::UnknownKey);
    }

    #[test]
    fn 类型不符被拒绝() {
        let mut s = ConfigStore::with_defaults("x.json");
        let e = s.set("thermal.tighten_temp", "abc").unwrap_err();
        assert_eq!(e.code, ErrorCode::Type);
    }

    #[test]
    fn 越界被拒绝且原值不变() {
        let mut s = ConfigStore::with_defaults("x.json");
        let before = s.get_f64("thermal.tighten_temp");
        let e = s.set("thermal.tighten_temp", "99").unwrap_err();
        assert_eq!(e.code, ErrorCode::Range);
        assert_eq!(s.get_f64("thermal.tighten_temp"), before, "拒绝时不得改动");
    }

    // ────────────────────────────────────────────────────────────
    //  跨键关系校验
    //
    //  真机实测踩到的坑：单键合法（41 在 20~45 内）而组合非法
    //  （放松 42 > 收紧 41，违反内核"放松须比收紧低 1℃"的规则）——
    //  结果是内核 fail-closed 每拍拒绝决策，整个引擎"活着但不工作"。
    // ────────────────────────────────────────────────────────────

    #[test]
    fn 跨键关系非法时单键写入被拒() {
        let mut s = ConfigStore::with_defaults("x.json");
        // 出厂默认：tighten=43.0, relax=42.0（合法：relax ≤ tighten - 1）
        let before = s.get_f64("thermal.tighten_temp");

        // 只把收紧温度降到 41 → relax(42) 比 tighten(41) 还高 ⇒ 非法
        let e = s.set("thermal.tighten_temp", "41").unwrap_err();
        assert_eq!(e.code, ErrorCode::Collision, "应报配置校验失败");
        assert!(
            e.msg.contains("放松温度"),
            "错误信息应指明是哪个关系冲突，实得：{}",
            e.msg
        );
        assert_eq!(
            s.get_f64("thermal.tighten_temp"),
            before,
            "拒绝时不得改动原值"
        );
    }

    #[test]
    fn 批量写入可以一次性调整相关键() {
        let mut s = ConfigStore::with_defaults("x.json");

        // 同时下调两个键 → 组合仍然合法 ⇒ 允许
        s.set_batch(&[
            ("thermal.tighten_temp".into(), "41".into()),
            ("thermal.relax_temp".into(), "40".into()),
        ])
        .expect("整批提交应通过跨键校验");

        assert_eq!(s.get_f64("thermal.tighten_temp"), 41.0);
        assert_eq!(s.get_f64("thermal.relax_temp"), 40.0);
    }

    #[test]
    fn 批量写入若组合非法则整批拒绝() {
        let mut s = ConfigStore::with_defaults("x.json");
        let t0 = s.get_f64("thermal.tighten_temp");
        let r0 = s.get_f64("thermal.relax_temp");

        let e = s
            .set_batch(&[
                ("thermal.tighten_temp".into(), "40".into()),
                ("thermal.relax_temp".into(), "42".into()), // 仍高于收紧
            ])
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::Collision);
        assert_eq!(s.get_f64("thermal.tighten_temp"), t0, "整批拒绝，全部不变");
        assert_eq!(s.get_f64("thermal.relax_temp"), r0);
    }

    #[test]
    fn 出厂默认配置本身通过跨键校验() {
        // 出厂默认必须是自洽的，否则首次启动就会每拍失败
        let s = ConfigStore::with_defaults("x.json");
        s.to_core()
            .expect("出厂默认应能转成内核配置")
            .validate()
            .expect("出厂默认应通过内核的全部校验");
    }

    #[test]
    fn 批量写入任一失败则整批拒绝() {
        let mut s = ConfigStore::with_defaults("x.json");
        let before_enabled = s.get_bool("enabled");
        let r = s.set_batch(&[
            ("enabled".into(), "false".into()),
            ("thermal.tighten_temp".into(), "999".into()), // 越界
        ]);
        assert!(r.is_err());
        assert_eq!(
            s.get_bool("enabled"),
            before_enabled,
            "整批拒绝时前面那项也不得生效"
        );
    }

    #[test]
    fn 枚举只接受候选值() {
        let mut s = ConfigStore::with_defaults("x.json");
        assert!(s.set("mode", "turbo").is_ok());
        assert!(s.set("mode", "nonsense").is_err());
    }

    #[test]
    fn 未知键在加载时被保留() {
        let text = r#"{"enabled": false, "我的自定义键": {"a": 1}, "limit": {"enabled": true, "soc": 85}}"#;
        let v: Value = serde_json::from_str(text).unwrap();
        // 模拟 load 的补默认过程
        let mut value = v;
        for spec in KEYS {
            if value.pointer(&ptr(spec.key)).is_none() {
                set_path(&mut value, spec.key, spec.default_value());
            }
        }
        assert!(value.get("我的自定义键").is_some(), "未知键必须保留");
        assert_eq!(value["limit"]["soc"], json!(85), "已知键不被覆盖");
        assert_eq!(value["enabled"], json!(false), "已知键不被覆盖");
    }

    #[test]
    fn 分区独立读取() {
        let s = ConfigStore::with_defaults("x.json");
        assert!(s.section("limit").is_some(), "附加功能分区应可独立读取");
        assert!(s.section("不存在").is_none(), "缺失分区返回 None 而非报错");
    }

    #[test]
    fn 转内核配置不包含附加功能分区() {
        let s = ConfigStore::with_defaults("x.json");
        let c = s.to_core().unwrap();
        // 内核结构里根本没有 limit 字段 —— 这是**编译期**保证，
        // 此处只确认转换本身可用且取值正确。
        assert!(c.enabled);
        assert_eq!(c.tighten_temp, 43.0);
        assert_eq!(c.patrol_s, 3);
    }

    #[test]
    fn 出厂默认的充电上限为八十且关闭() {
        let s = ConfigStore::with_defaults("x.json");
        assert!(!s.get_bool("limit.enabled"), "出厂默认必须关闭");
        assert_eq!(s.get_i64("limit.soc"), 80, "默认上限与系统一致（80%）");
    }

    // ── 默认值一致性（内嵌 vs 出厂文件）──────────────────────────
    //
    // 默认值有**两份**：
    //   ① `module/src/defaults/config.json` —— 安装时由 customize.sh 拷贝
    //   ② 本模块的 `default_config()`    —— 文件缺失/损坏时的兜底
    //
    // 两份不一致会导致极难排查的行为差异（新装用户一套、恢复用户另一套）。
    // 下面几个测试在 CI 阶段把这类分叉挡在发布之前。

    /// 出厂默认配置文件路径（`native/crates/thermo-cli` → 仓库根 → `module/src/defaults`）
    fn defaults_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("..")
            .join("module")
            .join("src")
            .join("defaults")
            .join("config.json")
    }

    #[test]
    fn 出厂默认配置文件存在() {
        let p = defaults_path();
        assert!(
            p.is_file(),
            "出厂默认配置不存在：{}（customize.sh 依赖它写入初始配置）",
            p.display()
        );
    }

    // ────────────────────────────────────────────────────────────
    //  分区白名单必须与真实数据同源
    //
    //  真机踩坑：配置里有 `curve` 段，但 `SECTIONS` 漏登记它，
    //  于是 `get --section curve` 报"未知分区" —— 数据在、却查不到。
    // ────────────────────────────────────────────────────────────

    #[test]
    fn 分区白名单覆盖默认配置里的每一个顶层对象() {
        let default = default_config();
        let obj = default.as_object().expect("默认配置应是对象");

        let mut missing = Vec::new();
        for (name, value) in obj {
            // 只关心对象型分区（标量顶层键如 schema/enabled 不参与）
            if !value.is_object() {
                continue;
            }
            if !SECTIONS.contains(&name.as_str()) {
                missing.push(name.clone());
            }
        }

        assert!(
            missing.is_empty(),
            "这些分区在默认配置里存在、却不在 SECTIONS 白名单里（会导致 get --section 失败）：{missing:?}\n\
             当前白名单：{SECTIONS:?}"
        );
    }

    #[test]
    fn 分区白名单里的每一项都真实存在() {
        let default = default_config();
        let mut phantom = Vec::new();
        for name in SECTIONS {
            if default.get(*name).is_none() {
                phantom.push(*name);
            }
        }
        assert!(
            phantom.is_empty(),
            "白名单里有默认配置中不存在的分区：{phantom:?}"
        );
    }

    #[test]
    fn curve分区可读() {
        // 真机回归：曾漏登记 curve，导致 get --section curve 报 E_UNKNOWN_KEY
        let s = ConfigStore::with_defaults("x.json");
        assert!(SECTIONS.contains(&"curve"), "curve 必须在白名单里");
        let sec = s.section("curve").expect("应能取到 curve 分区");
        assert!(sec.is_array(), "curve 是数组型分区");
        assert!(!sec.as_array().unwrap().is_empty(), "默认曲线不该为空");
    }

    #[test]
    fn 内嵌默认与出厂文件逐键一致() {
        let text = std::fs::read_to_string(defaults_path()).expect("应能读取出厂默认配置");
        let file: Value = serde_json::from_str(&text).expect("出厂默认配置必须是合法 JSON");
        let embedded = default_config();

        let mut diffs = Vec::new();
        for spec in KEYS {
            let pointer = ptr(spec.key);
            let from_file = file.pointer(&pointer);
            let from_code = embedded.pointer(&pointer);
            if from_file != from_code {
                diffs.push(format!(
                    "{}: 文件={:?} 代码={:?}",
                    spec.key,
                    from_file.cloned().unwrap_or(Value::Null),
                    from_code.cloned().unwrap_or(Value::Null)
                ));
            }
        }

        assert!(
            diffs.is_empty(),
            "出厂默认配置与代码内嵌默认值不一致（会导致新装用户与恢复用户行为不同）：\n{}",
            diffs.join("\n")
        );
    }

    #[test]
    fn 出厂配置包含全部契约键() {
        let text = std::fs::read_to_string(defaults_path()).unwrap();
        let file: Value = serde_json::from_str(&text).unwrap();

        let mut missing = Vec::new();
        for spec in KEYS {
            if file.pointer(&ptr(spec.key)).is_none() {
                missing.push(spec.key);
            }
        }
        assert!(
            missing.is_empty(),
            "出厂默认配置缺少契约键（首次安装后这些键会缺失）：{}",
            missing.join(", ")
        );
    }

    #[test]
    fn 出厂配置不预置机型实测数据() {
        let text = std::fs::read_to_string(defaults_path()).unwrap();
        let file: Value = serde_json::from_str(&text).unwrap();

        // 用户明确要求：档位 / 配置表 / 最大功率等一律靠设备检测
        assert_eq!(
            file.pointer("/device/max_power_w").and_then(Value::as_i64),
            Some(0),
            "最大功率必须为 0（= 自动探测），不得预置机型数值"
        );
        assert_eq!(
            file.pointer("/device/probe_ceiling")
                .and_then(Value::as_i64),
            Some(0),
            "写探默认必须禁用（开箱零风险）"
        );
    }
}
