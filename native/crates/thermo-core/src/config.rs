//! 配置（类型化，对应 docs/04-契约-配置说明书.md）
//!
//! 本模块只承载**决策所需的配置子集**；其余键（日志、数据维护等）
//! 由 I/O 层负责，不进内核。

use serde::{Deserialize, Serialize};

use crate::error::CoreError;

// ============================================================
//  物理极限常量
// ============================================================
//
// # ⚠️ 这些是"物理不可能超过"的护栏，**不是**机型边界
//
// 它们只用于两件事：
//
// | 用途 | 说明 |
// |---|---|
// | 格式校验 | 拒绝明显不合理的配置（如 200℃、500A） |
// | 无档案兜底 | 检测尚未跑过时的保守取值 |
//
// **真正的机型边界一律来自检测档案**，由
// [`Config::validate_against`] 与 [`Config::clamp_to`] 执行：
//
// | 参数 | 实际边界来源 |
// |---|---|---|
// | 温度阈值 | `capabilities.thermal.hardware_max_c` |
// | 电流 | `capabilities.current.max_ua` |
// | 功率 | `capabilities.power.max_w` |
// | 档位 | `capabilities.gear.max` |
//
// # 为什么不能把常量写成本机实测值
//
// 初版这三个常量就是 K90 Pro Max 的实测值（45℃ / 15 档 / 18.4A）。
// 后果：**换成任何别的机型，边界都按 K90PM 来**——
//
// | 机型 | 真实能力 | 常量放行 | 结果 |
// |---|---|---|---|
// | 67W 机型 | 11A | 18.4A | 写入超出硬件的电流 |
// | 20 档机型 | 0~20 | 0~15 | 档位被无谓限制 |
// | 硬件 55℃ 机型 | 55℃ | 45℃ | 用户设不到应有余量 |
//
// 用户要求"以具体机型自行检测出来的数据作为调整依据"，因此这里
// 只保留**任何手机都不可能越过**的极限值。

/// 温度物理上限（℃）
///
/// 任何手机电池都不该工作在这个温度以上。实际生效的硬保护阈值
/// 由检测档案的 `capabilities.thermal.hardware_max_c` 决定。
pub const ABS_MAX_TEMP_C: f32 = 60.0;

/// 最低可行温度（用于区间校验）
pub const MIN_TEMP_C: f32 = 20.0;

/// 档位物理上限（`u8` 全范围）
///
/// 实际可写范围由检测档案的 `capabilities.gear.max` 决定
/// （= 档位表行数，因机型而异，**不得假定 15**）。
pub const GEAR_MAX: u8 = u8::MAX;

/// 电流物理上限（μA）= 30 A
///
/// 实际上限由检测档案的 `capabilities.current.max_ua` 决定。
pub const CURR_MAX_UA: i32 = 30_000_000;

/// 功率物理上限（W）= 300 W
///
/// 实际上限由检测档案的 `capabilities.power.max_w` 决定（本机 100 W）。
/// 这里只是**区间校验**的兜底 —— 防止明显越界的值进入决策层。
pub const POWER_MAX_W: u16 = 300;

/// 控制策略模式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Conservative,
    Official,
    Turbo,
    #[default]
    Custom,
}

/// 温度基准来源
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SensorSource {
    /// 电池温度（默认）
    #[default]
    Cell,
    /// 虚拟合成温度
    Virt,
}

/// 控制律
///
/// # 🔴 默认是 `Ramp`（2026-10-14 修正，此前误为 `Curve`）
///
/// 三种律对「用户设定的收紧功率」（`tighten_power_w`）的**使用方式不同**，
/// 这决定了界面上那些滑块是否真正生效：
///
/// | 律 | 用 `tighten_power_w`？ | 用 `curve` 数组？ | 适用 |
/// |---|---|---|---|
/// | **`Ramp`** | ✅ **是终点值** | ❌ | **默认** —— 界面三个滑块正好对应它的起点/长度/终点 |
/// | `Curve` | ❌ **完全不用** | ✅ | 高级：需要多段精细曲线时（`curve` 只读，需改配置文件） |
/// | `Switch` | ✅ 是开关目标 | ❌ | 简单二段：到温就降到位，无过渡 |
///
/// ## `Curve` 为什么不能当默认
///
/// 界面「温度控制」卡片暴露的三个滑块是
/// 「收紧温度 / （过渡区间）/ **降到多少**」，其中「降到多少」写的是
/// `thermal.tighten_power_w` —— 而 `Curve` 律**根本不读这个键**。
///
/// ⇒ 默认 `Curve` 时，用户设的「降到多少」被**静默忽略**：
///
/// | 用户设 | `Curve` 律实际降到 | `Ramp` 律实际降到 |
/// |---|---|---|
/// | 10 W | **2.8 W**（曲线末段值） | 10 W ✓ |
/// | 0（期望停止充电） | **2.8 W** | 0 W ✓ |
///
/// 连带使四种模式的 `tighten_power_w` 差异（保守 3W / 极速 28W）全部失效 ——
/// 「极速」模式"降得少"这个设计根本没体现出来。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ControlLaw {
    /// 按温度曲线分段映射
    ///
    /// ⚠️ **不使用 `tighten_power_w`** —— 终点值由 `curve` 数组末段决定。
    /// 选它意味着放弃界面上「降到多少」那个滑块。
    Curve,
    /// 二段开关（到温即降到 `tighten_power_w`，无过渡段）
    Switch,
    /// 线性平滑降速（**默认**）：从 `tighten_temp` 起，经 `ramp_span` 度
    /// 线性降到 `tighten_power_w`
    #[default]
    Ramp,
}

/// 执行器选择
///
/// # 🔴 为什么只有两个值（2026-10-14 真机实测后收敛）
///
/// 曾有四个值 `dual` / `gear` / `curr` / `readonly`，但实测证明
/// **中间两个无法正确实现**，留着就是"配置在撒谎"：
///
/// | 值 | 状态 | 理由 |
/// |---|---|---|
/// | `dual` | ✅ 唯一正确 | 两个通道**都要写**，见下 |
/// | `readonly` | ✅ 保留 | 只观测（与 `observe_only` 同义） |
/// | `gear` | ❌ 删除 | 仅档位 ⇒ **放弃连续控制**。档位只有 16 个台阶，而电流 18401 个；且功率上限只走电流通道 ⇒ 功能残缺 |
/// | `curr` | ❌ 删除 | 仅电流 ⇒ **不写档位放开** ⇒ 官方写下的收紧档位（实测 34.3℃→7、37.5℃→11、40.5℃→14）会把我们设的电流**静默卡住** |
///
/// ## 为什么 `dual` 是必需的而非可选
///
/// 两个通道**各有不可替代的作用**（契约 §1.1：并联取最严）：
///
/// | 通道 | 不可替代的作用 | 实测依据 |
/// |---|---|---|
/// | **电流** | 唯一的**连续控制量**（温控渐变 + 功率上限） | 1 mA 步进，18401 个取值 |
/// | **档位** | **压制官方**的收紧档位，否则电流控制被卡 | 官方会写 7/11/12/14（实测回读捕获） |
///
/// ⇒ 少写任一个都产生真实缺陷，因此不提供"只写一个"的选项。
///
/// ## 旧配置里的 `gear` / `curr` 怎么处理
///
/// 见 `config_store::to_core()`：会**记警告**并退回 `dual`，
/// 不会静默按别的语义执行。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Actuator {
    /// 档位 + 电流双写（**唯一完整可用的模式**）
    #[default]
    Dual,
    /// 只读（等同只观测）
    Readonly,
}

/// 温度曲线的一个分段点
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CurvePoint {
    /// 该段适用的最高温度（℃）
    pub t_max: f32,
    /// 对应的目标电流（μA）
    pub curr: i32,
}

/// 决策所需配置
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// 总开关
    pub enabled: bool,
    /// 只观测（不写任何节点）
    pub observe_only: bool,
    /// 模式
    pub mode: Mode,
    /// 温度基准
    pub sensor: SensorSource,

    /// 收紧温度（℃）
    pub tighten_temp: f32,
    /// 放松温度（℃）
    pub relax_temp: f32,
    /// 超限持续秒数（防误触发）
    pub delay_s: u32,

    /// **收紧后降到多少**（W）—— 用户界面的「降到多少」滑块
    ///
    /// # 为什么是功率而不是电流
    ///
    /// 初版这里是 `tighten_curr`（μA），用户看到的是「0.7 A」。
    /// 真机反馈：**电流不直白** —— 用户关心的是"充得多快"，而
    /// 直观的量是瓦数（充电器、快充协议、电源适配器全都标瓦）。
    ///
    /// 换算到功率还有个实在的好处：**功率是"充电速度"的正确度量**。
    /// 同样的 2 A，在 3.5 V 时是 7 W、在 4.4 V 时是 8.8 W ——
    /// 而电池电压会随电量与温度变化。用户设"降到 5 W"比设"降到 1.2 A"
    /// 更接近他真正想要的效果。
    ///
    /// # 决策层怎么用
    ///
    /// 决策层**仍然只能写电流**（驱动节点是电流节点），因此在
    /// [`crate::decision`] 里用**前馈公式**换算：
    ///
    /// ```text
    /// I = P / U        （U 取当前实测电池电压）
    /// ```
    ///
    /// 换算误差的方向是**安全**的：电流降低后电池电压会略微升高
    /// （内阻压降减小），于是 `I = P/U` 算出的电流**更小** —— 偏保守。
    ///
    /// # ⚠️ `0` 的语义：**停止充电**
    ///
    /// 与 [`Self::target_power_w`] 的 `0` 是**相反**的语义 ——
    /// 那个 `0` 表示"不限制功率"。
    ///
    /// | 键 | `0` 的含义 |
    /// |---|---|
    /// | `thermal.tighten_power_w`（本键） | **停止充电**（收紧时写 0 电流） |
    /// | `control.target_power_w` | **不限制**（等于关闭这个功能） |
    ///
    /// 这么定是因为用户把滑块拖到最左端时期望的是"别充了"，
    /// 而不是"随便充"（那用最右端的"不减速"表达）。
    pub tighten_power_w: u16,

    /// 降速区间跨度（℃）
    pub ramp_span: f32,
    /// 控制律
    pub control_law: ControlLaw,
    /// 虚拟温度守卫
    pub virt_guard: bool,
    /// 虚拟温度硬阈值（℃）
    pub virt_hard: f32,

    /// 执行器
    pub actuator: Actuator,
    /// 功率闭环
    pub closed_loop: bool,
    /// 目标功率（W）
    pub target_power_w: u16,
    /// 巡检间隔（秒）
    pub patrol_s: u32,
    /// 调档最小间隔（秒）
    pub shift_s: u32,
    /// 硬件保护
    pub hardware_protect: bool,

    /// 温度曲线
    pub curve: Vec<CurvePoint>,
}

impl Default for Config {
    /// 出厂默认配置
    ///
    /// ⚠️ **必须与 `thermo-cli::config_store::default_config()` 保持一致** ——
    /// 那个是写入 `config.json` 的真源，本函数供核心层单测/示例使用。
    /// 两者不一致会让"默认行为"在测试与真机上不同（本文件就踩过一次：
    /// `control_law` 在此处漏改成 `ramp`，被 `默认配置下降低的电流设定必须生效` 抓到）。
    fn default() -> Self {
        Self {
            enabled: true,
            observe_only: false,
            mode: Mode::Custom,
            sensor: SensorSource::Cell,
            tighten_temp: 43.0,
            relax_temp: 42.0,
            delay_s: 5,
            // 3 W ≈ 旧版的 0.7 A（按 4.0 V 换算）。详见字段文档。
            tighten_power_w: 3,
            ramp_span: 2.0,
            // 🔴 `ramp` 而非 `curve` —— 见 [`ControlLaw`] 文档：
            //    `curve` 不读 `tighten_power_w`，会让界面上的「降到多少」失效
            control_law: ControlLaw::Ramp,
            virt_guard: true,
            virt_hard: 48.0,
            actuator: Actuator::Dual,
            closed_loop: true,
            // ⚠️ 0 = **不限制功率**（默认）
            //
            // 初版默认 43（继承自老项目）。那会让模块**莫名其妙地限速**：
            // 用户没要求限功率，插上 100W 充电器却只跑 43W。
            //
            // 模块的核心职责是**温度控制**；功率上限是用户主动开启的
            // 附加能力。默认不介入才是对的。
            target_power_w: 0,
            patrol_s: 3,
            shift_s: 6,
            hardware_protect: true,
            curve: vec![
                CurvePoint {
                    t_max: 38.0,
                    curr: 18_400_000,
                },
                CurvePoint {
                    t_max: 40.0,
                    curr: 12_000_000,
                },
                CurvePoint {
                    t_max: 41.0,
                    curr: 9_000_000,
                },
                CurvePoint {
                    t_max: 42.0,
                    curr: 7_000_000,
                },
                CurvePoint {
                    t_max: 43.0,
                    curr: 5_000_000,
                },
                CurvePoint {
                    t_max: 44.0,
                    curr: 2_500_000,
                },
                CurvePoint {
                    t_max: 45.0,
                    curr: 700_000,
                },
            ],
        }
    }
}

impl Config {
    /// 校验配置合法性（fail-closed：不合法则整体拒绝）
    pub fn validate(&self) -> Result<(), CoreError> {
        let bad = |m: &str| Err(CoreError::InvalidConfig(m.to_string()));

        if !(MIN_TEMP_C..=ABS_MAX_TEMP_C).contains(&self.tighten_temp) {
            return bad("收紧温度超出合法范围");
        }
        if !(MIN_TEMP_C..=ABS_MAX_TEMP_C).contains(&self.relax_temp) {
            return bad("放松温度超出合法范围");
        }
        // 迟滞区：放松温度必须至少比收紧温度低 1℃
        if self.relax_temp > self.tighten_temp - 1.0 {
            return bad("放松温度须至少比收紧温度低 1℃");
        }
        if !(0.5..=5.0).contains(&self.ramp_span) {
            return bad("降速区间超出合法范围");
        }
        if self.tighten_power_w > POWER_MAX_W {
            return bad("收紧功率超出合法范围");
        }
        if self.patrol_s == 0 || self.patrol_s > 60 {
            return bad("巡检间隔超出合法范围");
        }
        if self.curve.is_empty() {
            return bad("温度曲线为空");
        }
        for p in &self.curve {
            if !p.t_max.is_finite() || p.t_max <= 0.0 || p.t_max > ABS_MAX_TEMP_C + 1.0 {
                return bad("温度曲线分段点非法");
            }
            if !(0..=CURR_MAX_UA).contains(&p.curr) {
                return bad("温度曲线电流超出合法范围");
            }
        }
        Ok(())
    }

    /// 实际生效的降速终点（仅受**物理上限**截断）
    ///
    /// ⚠️ 这只是兜底。**真正的截断在决策层**：降速区间不得超过
    /// 本机的硬保护温度（`capabilities.thermal.hardware_max_c`），
    /// 见 `decision::ramp_end`。
    ///
    /// 例如本机 `hardware_max_c = 48℃`，而 `tighten_temp + ramp_span`
    /// 若算出 52℃，决策层会截到 48℃ —— 降速区间没必要越过硬保护点。
    pub fn ramp_end_temp(&self) -> f32 {
        let end = self.tighten_temp + self.ramp_span;
        end.min(ABS_MAX_TEMP_C)
    }

    /// **设备边界校验**（v2 新增）
    ///
    /// 检查所有控制参数是否落在**生效设备档案**的能力边界内。
    ///
    /// # 与 `validate()` 的区别
    ///
    /// | 方法 | 检查什么 | 依据 |
    /// |---|---|---|
    /// | `validate()` | 配置**自身**是否合法（类型、范围、内部一致性） | 契约 §0 规则 1-6 |
    /// | `validate_against()` | 配置是否**超出本机能力**（客观上限） | 契约 §0.1 + 检测档案 |
    ///
    /// 两者**都必须通过**才允许应用配置。
    ///
    /// # 示例
    ///
    /// ```
    /// # use thermo_core::{Config, Capabilities};
    /// let caps = Capabilities::default();
    /// let mut cfg = Config::default();
    /// // 目标功率超过设备最大功率 → 拒绝
    /// cfg.target_power_w = 200;
    /// assert!(cfg.validate_against(&caps).is_ok());   // 默认档案 max_w 未知 → 放行
    /// ```
    pub fn validate_against(&self, caps: &crate::profile::Capabilities) -> Result<(), CoreError> {
        use crate::profile::BoundaryViolation as V;

        // ① 目标功率 ≤ 设备最大功率（**用户最关心的那条**）
        if let Some(max_w) = caps.power.max_w {
            if self.target_power_w > max_w {
                return Err(V::Power {
                    target: self.target_power_w,
                    max: max_w,
                }
                .into());
            }
        }

        // ② 收紧功率 ≤ 设备最大功率
        //
        // ⚠️ 与 ① 一样按功率比对（不再是电流）—— 这样用户设「降到 120 W」
        //    而本机上限 100 W 时会被明确拒绝，而不是换算成电流后再报一个
        //    用户看不懂的电流越界。
        if let Some(max_w) = caps.power.max_w {
            if self.tighten_power_w > max_w {
                return Err(V::Power {
                    target: self.tighten_power_w,
                    max: max_w,
                }
                .into());
            }
        }

        // ③ 曲线每段电流 ≤ 电流上限
        for p in &self.curve {
            if p.curr > caps.current.max_ua {
                return Err(V::Current {
                    target: p.curr,
                    max: caps.current.max_ua,
                }
                .into());
            }
        }

        // ④ 温度阈值 ≤ 硬件保护温度
        if self.tighten_temp > caps.thermal.hardware_max_c {
            return Err(V::Temp {
                target: self.tighten_temp,
                max: caps.thermal.hardware_max_c,
            }
            .into());
        }

        Ok(())
    }

    /// 按设备边界**钳位**（用于加载时修正越界的既有配置）
    ///
    /// 返回被钳位的项目列表（空表示无需修正）。
    /// ⚠️ 调用方**必须**记录事件，不可静默修正。
    pub fn clamp_to(&mut self, caps: &crate::profile::Capabilities) -> Vec<String> {
        let mut clamped = Vec::new();

        if let Some(max_w) = caps.power.max_w {
            if self.target_power_w > max_w {
                clamped.push(format!(
                    "control.target_power_w: {} → {}",
                    self.target_power_w, max_w
                ));
                self.target_power_w = max_w;
            }
        }

        // 收紧功率同样按功率钳位（与 `validate_against` 的第 ② 条对称）
        if let Some(max_w) = caps.power.max_w {
            if self.tighten_power_w > max_w {
                clamped.push(format!(
                    "thermal.tighten_power_w: {} → {}",
                    self.tighten_power_w, max_w
                ));
                self.tighten_power_w = max_w;
            }
        }

        // ---------- 温度曲线：整体等比缩放 ----------
        //
        // # 为什么是"缩放"而不是"逐项钳到上限"
        //
        // 出厂曲线写的是**满刻度参考值**（以 18.4A 为首段），它表达的是
        // 「温度 → 速度占比」这条曲线**形状**，而不是某个机型的绝对电流。
        //
        // | 做法 | K90PM（18.4A） | 67W 机型（11A） |
        // |---|---|---|
        // | ❌ 逐项钳到上限 | 不变 | 首段 18.4→11，其余段**不动** ⇒ 曲线被压平，形状失真 |
        // | ✅ 整体等比缩放 | ×1.00，不变 | ×0.598 ⇒ **形状完整保留**，只改幅度 |
        //
        // 这样"不预置任何机型数据"与"出厂即有可用曲线"两个目标同时满足：
        // 曲线形状是通用的（越热越慢），幅度由本机检测的电流上限决定。
        let curve_peak = self.curve.iter().map(|p| p.curr).max().unwrap_or(0);
        if curve_peak > caps.current.max_ua && curve_peak > 0 {
            let scale = f64::from(caps.current.max_ua) / f64::from(curve_peak);
            for p in &mut self.curve {
                p.curr = (f64::from(p.curr) * scale).round() as i32;
            }
            clamped.push(format!(
                "温度曲线整体缩放 ×{scale:.3}（峰值 {} → {} μA，适配本机电流上限）",
                curve_peak, caps.current.max_ua
            ));
        }
        if self.tighten_temp > caps.thermal.hardware_max_c {
            clamped.push(format!(
                "thermal.tighten_temp: {} → {}",
                self.tighten_temp, caps.thermal.hardware_max_c
            ));
            self.tighten_temp = caps.thermal.hardware_max_c;
        }

        clamped
    }
}
