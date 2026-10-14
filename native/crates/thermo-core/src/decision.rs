//! 决策输出与评估逻辑
//!
//! 所有安全边界在此闭合：
//! - 电流钳位到 `0..=CURR_MAX_UA`
//! - 档位钳位到 `0..=GEAR_MAX`
//! - 达到硬上限必然硬保护
//! - 迟滞区不振荡

use crate::config::{Actuator, Config, ControlLaw, ABS_MAX_TEMP_C, CURR_MAX_UA, GEAR_MAX};
use crate::error::CoreError;
use crate::profile::Capabilities;
use crate::sample::Sample;
use crate::state::{Phase, State};

/// 决策原因（用于日志与可观测性）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// 模块已关闭
    Disabled,
    /// **官方模式** —— 用户选择"完全不管"，交回系统
    Official,
    /// **本机尚未跑过检测** —— 零控制（不写任何节点）
    NotDetected,
    /// 只观测模式
    ObserveOnly,
    /// 未充电
    NotCharging,
    /// 正常放行
    Normal,
    /// **受功率上限约束**（温度正常，但用户设的功率上限在起作用）
    PowerLimit,
    /// 超限降速
    Tighten,
    /// 延迟未满，暂不动作
    DelayPending,
    /// 温度回落，恢复
    Relax,
    /// 硬上限保护
    HardProtect,
    /// 虚拟温度守卫触发
    VirtGuard,
    /// 读取失败达阈值
    ReadFailure,
    /// 写入失败达阈值
    WriteFailure,
}

/// 一次决策结果
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    /// 是否要写节点
    pub should_write: bool,
    /// 目标档位
    pub gear: u8,
    /// 目标电流（μA）
    pub curr_ua: i32,
    /// 是否交回官方
    pub release: bool,
    /// 决策原因
    pub reason: Reason,
    /// 新的阶段
    pub phase: Phase,
    /// 功率闭环算出的电流上限（μA）；未启用功率限制时为 `None`
    ///
    /// 由调用方存入 [`crate::state::State::power_cap_ua`]，
    /// 供下一拍做比例校正。
    pub power_cap_ua: Option<i32>,
}

impl Decision {
    /// 构造"交回官方"决策
    ///
    /// `gear = 最松档位`（放开限速）+ `curr = 上限`（放开电流）⇒ 等价于不限速。
    /// 取值全部来自生效边界，而非写死常量。
    fn release(reason: Reason, b: Bounds) -> Self {
        Self {
            should_write: true,
            gear: b.gear_open,
            curr_ua: b.curr_max,
            release: true,
            reason,
            phase: Phase::Idle,
            power_cap_ua: None,
        }
    }
}

/// 生效钳位边界（从设备档案提取）
///
/// 把 `Capabilities` 降维成决策所需的最小集合，避免在热路径上反复查表。
#[derive(Debug, Clone, Copy)]
pub struct Bounds {
    /// 电流上限（μA）—— 来自检测档案，而非写死常量
    pub curr_max: i32,
    /// 档位上限
    pub gear_max: u8,
    /// 最松档位（放开限速时使用）
    pub gear_open: u8,
    /// 最紧档位（硬保护时使用）
    pub gear_safe: u8,
    /// 硬保护温度（℃）—— 来自检测档案的 `hardware_max_c`
    ///
    /// ⚠️ 不能用 [`ABS_MAX_TEMP_C`] 常量代替：那是 60℃ 的物理极限，
    /// 而本机实测是 48℃。用常量会让硬保护**晚 12℃ 才动作**。
    pub hardware_max_c: f32,
    /// 本机是否**跑过检测**
    ///
    /// `false` ⇒ **零控制**（决策一律 `should_write: false`）。
    /// 详见 [`crate::profile::Capabilities::detected`]。
    pub detected: bool,
}

impl Bounds {
    /// 从设备能力边界提取
    ///
    /// ⚠️ `curr_max` **对齐到 mA**（见 [`align_ma`]）：
    /// 档案里的值理论上已是整 mA，但档案可能由人工编辑或旧版本写入。
    /// 在这里对齐一次，`Decision::release` 等**不经 `clamp_curr`** 的
    /// 路径也就自动一致了。
    pub fn from_caps(caps: &Capabilities) -> Self {
        Self {
            curr_max: align_ma(caps.current.max_ua),
            gear_max: caps.gear.max,
            gear_open: caps.gear.open.min(caps.gear.max),
            gear_safe: caps.gear.safe.min(caps.gear.max),
            hardware_max_c: caps.thermal.hardware_max_c,
            detected: caps.detected,
        }
    }
}

impl Default for Bounds {
    /// **便捷 API 用的边界**（`decide()` / `evaluate()` 无档案参数时）
    ///
    /// ⚠️ 生产路径**不用**它 —— `engine` 一律走 [`Bounds::from_caps`]
    /// 按检测档案构造。
    ///
    /// # `detected: true` 的理由
    ///
    /// 这里的语义是"**假定有一份通用档案**"，而不是"未检测"：
    ///
    /// | 入口 | `detected` | 行为 |
    /// |---|---|---|
    /// | [`Bounds::from_caps`] + 真实档案 | 跟随档案 | 正常控制 |
    /// | [`Bounds::from_caps`] + 兜底档案 | `false` | **零控制** |
    /// | [`Bounds::default`] | `true` | 正常控制（测试/示例用） |
    ///
    /// 若这里填 `false`，`decide()` 就永远零写入 —— 那个 API 也就没意义了。
    fn default() -> Self {
        Self {
            curr_max: align_ma(CURR_MAX_UA),
            gear_max: GEAR_MAX,
            gear_open: 0,
            gear_safe: GEAR_MAX,
            hardware_max_c: ABS_MAX_TEMP_C,
            detected: true,
        }
    }
}

/// 电流钳位（按生效边界）+ **向下对齐到 mA**
///
/// # 为什么在这里对齐（2026-10-14 真机实测）
///
/// 驱动只接受 **mA** 精度，且是**向下截断**而非四舍五入：
///
/// | 写入（μA） | 驱动回读 | 说明 |
/// |---|---|---|
/// | `3456789` | `3456000` | -789 |
/// | `1001501` | `1001000` | -501 |
/// | `1001000` | `1001000` | 原样 |
///
/// 而电流是**唯一的连续控制量**（温度收紧 / 功率上限都靠它），
/// 决策层由浮点算出的值几乎都不是整 mA：
///
/// ```text
/// 功率闭环：I = P / U ⇒ 12000000 / 4.2 = 2857142.857… μA
/// ```
///
/// 若不对齐，会出现两个问题：
///
/// | # | 问题 |
/// |---|---|
/// | 1 | 模块记录的 `state.curr_ua` 与**节点实际值**不一致（差几百 μA） |
/// | 2 | 历史曲线里画的是"想写的值"，不是"实际生效的值" —— 属静默偏差 |
///
/// 物理影响可忽略（<1 mA），但**记录必须诚实**。
/// 在决策层一次对齐，下游（写入 / 状态 / 历史 / 回读校验）全部一致。
fn clamp_curr(v: i32, b: Bounds) -> i32 {
    align_ma(v).clamp(0, b.curr_max)
}

/// 向下对齐到整毫安（驱动的最小有效步进）
///
/// 用 `div_euclid` 而非 `/`：负数会被正确向下取整。
/// （电流在此处恒为非负，但负值输入不应产生**向上**的偏差。）
fn align_ma(ua: i32) -> i32 {
    ua.div_euclid(1_000) * 1_000
}

// 🔴 2026-10-14 删除 `clamp_gear()`
//
// 它的唯一调用点是已删除的 `gear_for_temp()`。现在决策里**所有**档位值
// 都直接取自 `Bounds` 的三个字段（`gear_open` / `gear_safe` / `gear_max`），
// 而这三个字段在 [`Bounds::from_caps`] 与 [`Bounds::default`] 构造时
// **已经**各自 `.min(caps.gear.max)` 过。
//
// ⇒ 钳位只在**构造 Bounds 时做一次**，而非在每个使用点重复。
//    若将来重新引入"按温度算档位"，记得同时恢复钳位 ——
//    驱动**不校验范围**（契约 §1.1 规则 3）。

/// 功率闭环：由**目标功率**算出当前的电流上限
///
/// # 语义
///
/// 用户设「最大充电功率 50W」⇒ **充电功率不超过 50W**。
/// 实现方式是**限制电流**（功率 = 电流 × 电压），档位不受影响
/// （档位保持放开 —— 它是台阶、且会切断快充，不适合做连续控制）。
///
/// # 为什么用闭环而不是纯公式
///
/// 理论上 `I = P / U` 就够，但实际有多个不确定因素：
///
/// | 因素 | 影响 |
/// |---|---|
/// | 电荷泵比例 | 部分机型是 2:1，写的电流与实际流入电池的不同 |
/// | 电池内阻 | 大电流下压降明显，`U` 会变 |
/// | 转换损耗 | 约 10% 的能量以热的形式损失 |
///
/// 这些**因机型而异**，无法靠公式算准。因此：
///
/// 1. **前馈**：首拍用 `I = P / U` 起步（`U` 是实时读的电池电压）
/// 2. **闭环**：之后每拍用实测功率按比例校正 —— 误差会自愈
///
/// 这满足"以机型自行检测出的数据为依据"：公式里的电压是实时值，
/// 电流上界来自检测档案，**没有任何机型相关的硬编码**。
///
/// # 返回值
///
/// `(电流上限 μA, 是否受功率限制)`
///
/// 第二个值为 `false` 表示"不限功率"，调用方应直接用 `b.curr_max`。
///
/// # 边界处理
///
/// | 情况 | 处理 | 理由 |
/// |---|---|---|
/// | `target_power_w == 0` | 不限制 | 约定：0 = 用户未启用功率上限 |
/// | 电压读不到（≤0.5V） | 不限制 | 无法换算时宁可放开，避免因读数缺失卡在低速 |
/// | 功率反馈缺失（≤0.1W） | 用前馈公式 | 刚插入/未充电时无功率可测 |
/// | 误差在目标 ±5% 内 | 保持不动 | 死区，避免在目标附近来回抖 |
fn power_cap_curr(sample: &Sample, config: &Config, state: &State, b: Bounds) -> (i32, bool) {
    let target = f64::from(config.target_power_w);
    if target <= 0.0 {
        return (b.curr_max, false);
    }

    // ---------- 前馈：I = P / U ----------
    //
    // 单位：P[W] / U[V] = I[A]；乘以 1e6 得 μA。
    let u_v = f64::from(sample.voltage_uv) / 1e6;
    let feedforward = if u_v > 0.5 {
        ((target / u_v) * 1e6) as i32
    } else {
        // 电压不可信 → 不限制（保守放行，而非盲目收紧）
        return (b.curr_max, false);
    };
    let feedforward = feedforward.clamp(0, b.curr_max);

    // ---------- 闭环：按实测功率比例校正 ----------
    let actual = f64::from(sample.power_w());

    // 无功率反馈 → 用前馈起步
    if actual <= 0.1 {
        return (feedforward, true);
    }

    // 基准：上一拍算的值；首拍用前馈
    let base = state
        .power_cap_ua
        .unwrap_or(feedforward)
        .clamp(0, b.curr_max);

    // 比例：>1 说明实测低于目标（该放松），<1 说明超标（该收紧）
    let ratio = target / actual;

    // 死区 ±5%：已在目标附近就不折腾
    if (0.95..=1.05).contains(&ratio) {
        return (base, true);
    }

    // 单拍变化幅度限 ±25%：防止测量噪声导致大幅摆动
    let bounded = ratio.clamp(0.75, 1.25);
    let next = (f64::from(base) * bounded) as i32;
    (next.clamp(0, b.curr_max), true)
}

/// 核心评估（纯函数）
///
/// 使用默认（保守）能力边界。需要按设备档案约束时用 [`evaluate_with`]。
pub fn evaluate(sample: &Sample, config: &Config, state: &State) -> Result<Decision, CoreError> {
    evaluate_with(sample, config, state, &Bounds::default())
}

/// 核心评估（指定生效边界）
///
/// `bounds` 来自检测得到的设备档案 —— **所有钳位都按它执行**，
/// 而不是按写死的常量。这是"控制全部基于测出的数据"这条需求的落点。
pub fn evaluate_with(
    sample: &Sample,
    config: &Config,
    state: &State,
    bounds: &Bounds,
) -> Result<Decision, CoreError> {
    let b = *bounds;
    config.validate()?;

    // ---------- 门禁 0：未检测 ⇒ 零控制 ----------
    //
    // 用户要求："在使用模块之前未进行测试的，不要进行任何控制行为"。
    //
    // 这是**最高优先级**的门禁 —— 排在 `enabled` 之前，因为：
    // 用户装完模块、还没跑检测时，配置里的默认值不能成为"控制依据"。
    //
    // 为什么不能"用兜底值放行"：兜底值必然出错（真机两种都踩过）
    //
    // | 兜底值 | 后果 |
    // |---|---|
    // | 18.4A（K90PM 实测） | 67W 机型**超限** |
    // | 5A（保守猜测） | 用户什么都没设却被**限到约 22W** |
    //
    // ⇒ 宁可不控制（交给系统原有逻辑），也不要用猜的边界动真实硬件。
    if !b.detected {
        return Ok(Decision {
            should_write: false,
            gear: state.gear,
            curr_ua: state.curr_ua,
            release: false,
            reason: Reason::NotDetected,
            phase: Phase::Idle,
            power_cap_ua: None,
        });
    }

    // ---------- 前置门禁 ----------
    if !config.enabled {
        return Ok(Decision::release(Reason::Disabled, b));
    }
    if config.observe_only || config.actuator == Actuator::Readonly {
        // ⚠️ 只观测 / 只读执行器 ⇒ **零写入**
        //
        // 这两种模式的语义是"**只看不动**"。初版复用了 `Decision::release`
        // （它 `should_write: true`），于是"只观测"也会去写节点 ——
        // 真机表现为：开了只观测，充电电流仍被改动。
        //
        // 与 `Disabled` 的区别：
        //
        // | 模式 | 是否写 | 理由 |
        // |---|---|---|
        // | `enabled: false`（模块关闭） | ✅ 写（释放） | 要解除模块此前加的限制 |
        // | `observe_only`（只观测） | ❌ **不写** | 用户明确要求"不要动" |
        return Ok(Decision {
            should_write: false,
            gear: state.gear,
            curr_ua: state.curr_ua,
            release: false,
            reason: Reason::ObserveOnly,
            phase: Phase::Idle,
            power_cap_ua: None,
        });
    }
    // ---------- 官方模式：完全放手 ----------
    //
    // 用户语义（2026-10-13 确认）：**「官方」= 模块完全不插手**。
    //
    // ⚠️ 与 `observe_only` 的区别（两者都在上面返回了，这里是第三种"不管"）：
    //
    // | 模式 | 是否写 | 理由 |
    // |---|---|---|
    // | `enabled: false` | ✅ 写（释放） | 要解除模块此前加的限制 |
    // | `observe_only` | ❌ **不写** | 用户明确要求"不要动" |
    // | **`mode: official`** | ✅ **写一次（释放）** | 同 `enabled: false` —— 必须先解除自己的限制，才算真的"不管" |
    //
    // 🔴 为什么不能直接零写入：
    //    模块此前可能已把档位/电流压紧。若"官方"只是自己不写，
    //    那些限制会**残留到系统覆盖之前**（实测 5~21 秒），
    //    用户看到的是"选了官方却还在被限速"。
    //    ⇒ 必须主动写回 `gear_open` + `curr_max` 才算交还控制权。
    //
    // 位置在 `observe_only` **之后**：用户若同时开了只观测，
    // 那是更强的"不要动"意愿，应优先（此时不释放）。
    if config.mode == crate::config::Mode::Official {
        return Ok(Decision::release(Reason::Official, b));
    }

    if !sample.status.is_active() {
        return Ok(Decision::release(Reason::NotCharging, b));
    }

    let temp = match config.sensor {
        crate::config::SensorSource::Cell => sample.temp_c,
        crate::config::SensorSource::Virt => sample.virt_c.unwrap_or(sample.temp_c),
    };
    if !temp.is_finite() {
        return Ok(Decision::release(Reason::ReadFailure, b));
    }

    // ---------- 硬保护（最高优先级） ----------
    //
    // ⚠️ 阈值取自 `b.hardware_max_c`（检测档案），**不是** `ABS_MAX_TEMP_C`
    //    常量。常量是 60℃ 的物理极限，本机实测硬件保护温度是 48℃ ——
    //    用常量会让硬保护晚 12℃ 才动作，等于没有保护。
    if config.hardware_protect && temp >= b.hardware_max_c {
        return Ok(Decision {
            should_write: true,
            gear: b.gear_safe,
            curr_ua: clamp_curr(tighten_curr_ua(sample, config, b), b),
            release: false,
            reason: Reason::HardProtect,
            phase: Phase::HardProtect,
            // 硬保护期间不更新功率闭环：电流已被温度压到最紧，
            // 此时按功率误差调整会污染闭环状态
            power_cap_ua: state.power_cap_ua,
        });
    }

    // ---------- 虚拟温度守卫 ----------
    if config.virt_guard {
        if let Some(v) = sample.virt_c {
            if v.is_finite() && v >= config.virt_hard {
                return Ok(Decision {
                    should_write: true,
                    gear: b.gear_safe,
                    curr_ua: clamp_curr(tighten_curr_ua(sample, config, b), b),
                    release: false,
                    reason: Reason::VirtGuard,
                    phase: Phase::HardProtect,
                    power_cap_ua: state.power_cap_ua,
                });
            }
        }
    }

    // ---------- 迟滞判定 ----------
    // 已收紧：需回落到 relax_temp 以下才恢复
    // 未收紧：需超过 tighten_temp 才收紧
    let should_tighten = if state.tightening {
        temp >= config.relax_temp
    } else {
        temp >= config.tighten_temp
    };

    if !should_tighten {
        let recovering = state.tightening;

        // ---------- 功率上限（温度正常时的唯一约束） ----------
        //
        // 温度未达收紧线时，电流不再无脑取 `b.curr_max`，而要过一道
        // 功率闸门 —— 这正是"用户设 50W 就不许超过 50W"的落点。
        //
        // ⚠️ 档位仍取最松（`b.gear_open`）：功率限制只走电流通道。
        //    档位表是离散的（13.5A / 11A / 9A…），用它凑功率太粗糙，
        //    而电流是连续的，能精确控功率。
        let (pcap, limited) = power_cap_curr(sample, config, state, b);

        return Ok(Decision {
            should_write: true,
            gear: b.gear_open,
            curr_ua: if limited { pcap } else { b.curr_max },
            release: false,
            reason: if recovering {
                Reason::Relax
            } else if limited {
                Reason::PowerLimit
            } else {
                Reason::Normal
            },
            phase: Phase::Normal,
            power_cap_ua: if limited { Some(pcap) } else { None },
        });
    }

    // ---------- 延迟门禁（防误触发） ----------
    if !state.tightening && state.over_temp_ticks < config.delay_s {
        return Ok(Decision {
            should_write: false,
            gear: state.gear,
            curr_ua: state.curr_ua,
            release: false,
            reason: Reason::DelayPending,
            phase: state.phase,
            power_cap_ua: state.power_cap_ua,
        });
    }

    // ---------- 按控制律计算目标电流 ----------
    let target_curr = match config.control_law {
        ControlLaw::Curve => curve_target(sample, temp, config, b),
        ControlLaw::Switch => tighten_curr_ua(sample, config, b),
        ControlLaw::Ramp => ramp_target(sample, temp, config, b),
    };

    // ---------- 与功率上限取更严 ----------
    //
    // 两者都是"上限"，因此取 min：
    //
    // | 场景 | 结果 |
    // |---|---|
    // | 功率限制更严（50W ⇒ 11A，温度只要求 ≤15A） | 11A ← 功率生效 |
    // | 温度限制更严（温度要求 ≤2.5A，功率允许 11A） | 2.5A ← 温度生效 |
    //
    // 温度优先是安全的，且"不超过 N W"这个约束**始终成立**。
    let (pcap, power_limited) = power_cap_curr(sample, config, state, b);
    let final_curr = clamp_curr(target_curr.min(pcap), b);

    // ⚠️ 档位**保持放开**，温度收紧只走电流通道
    //
    // # 为什么不用档位参与温度收紧（2026-10-14 真机实测结论）
    //
    // 档位是**台阶**，电流是**无极**，两者精度差约 1100 倍：
    //
    // | 通道 | 可取值 | 相邻跳变 |
    // |---|---|---|
    // | 档位 | **16 个**（0~15） | 最高区 **2500 mA** 一跳 |
    // | 电流 | **18401 个**（0~18400 mA） | **1 mA** |
    //
    // 用台阶去表达连续的温度渐变，必然产生**跳变**；
    // 更要紧的是**档位 14/15 会切断快充**（实测：总线电压 16V → 8.9V 腰斩，
    // 功率 44W → 1.9W，分压比 4.05 → 2.26，三轮复现）。
    //
    // ⇒ 若档位随温度收紧，用户在 44℃ 以上设的"降到多少"会被**静默压掉**：
    //
    // | 温度 | 档位 | 档位层限流 | 用户设 2A 的实际结果 |
    // |---|---|---|---|
    // | 44℃ | 8 | 3.5 A | 2 A ✓ 生效 |
    // | **45℃** | **15** | **0.5 A** | **0.5 A ✗ 被卡** |
    //
    // 而"收紧后降到多少由用户定"是明确需求（2026-10-13）。
    //
    // # 硬保护**仍然**两层收紧
    //
    // 见本函数上方 `Reason::HardProtect` 分支：那里档位取 `b.gear_safe`
    // （最紧档），是**主动切断快充**的最后手段 —— 这一点实测有效，保留。
    //
    // 分工因此变得清晰：
    //
    // | 通道 | 角色 |
    // |---|---|
    // | 电流 | **唯一的连续控制量**（温控 / 功率上限） |
    // | 档位 | **开关**（硬保护时切断快充） |
    let _ = temp; // 档位不再依赖温度
    Ok(Decision {
        should_write: true,
        gear: b.gear_open,
        curr_ua: final_curr,
        release: false,
        reason: Reason::Tighten,
        phase: Phase::Tightening,
        power_cap_ua: if power_limited {
            Some(pcap)
        } else {
            state.power_cap_ua
        },
    })
}

/// 曲线控制律：取第一个满足 `temp <= t_max` 的分段
///
/// `sample` 只用于"曲线为空"这个兜底分支（那时退回用户的功率目标）。
fn curve_target(sample: &Sample, temp: f32, config: &Config, b: Bounds) -> i32 {
    for p in &config.curve {
        if temp <= p.t_max {
            return clamp_curr(p.curr, b);
        }
    }
    // 超出所有分段 → 最慢档
    //
    // ⚠️ `curve` 为空时退回用户的「降到多少」——`Config::validate` 会拒绝
    //    空曲线，所以这条分支在正常流程下到不了；留着是为了**不 panic**。
    config.curve.last().map_or_else(
        || tighten_curr_ua(sample, config, b),
        |p| clamp_curr(p.curr, b),
    )
}

/// 降速终点温度（℃，受**档案的硬保护温度**截断）
///
/// # 为什么要按档案截断
///
/// `tighten_temp + ramp_span` 可能算到硬保护点之外。例如本机
/// `hardware_max_c = 48℃`、`tighten_temp = 47℃`、`ramp_span = 2.0`
/// ⇒ 算出 49℃。降速区间越过 48℃ 没有意义 —— 到那里已经触发硬保护了。
///
/// ⚠️ 不能用 [`ABS_MAX_TEMP_C`] 常量代替：那是 60℃ 的物理极限，
/// 按它截断等于不截断。
fn ramp_end(config: &Config, b: Bounds) -> f32 {
    config.ramp_end_temp().min(b.hardware_max_c)
}

/// 把「收紧功率目标」（W）换算成电流上限（μA）
///
/// # 为什么需要换算
///
/// 用户界面上的「降到多少」是**功率**（W）—— 那是用户能理解的量
/// （充电器、快充协议都标瓦）。而驱动节点**只接受电流**。
/// 因此决策层负责这一步换算。
///
/// # 用前馈，不用闭环
///
/// ```text
/// I = P / U        U = 当前实测电池电压
/// ```
///
/// 这里刻意**不**做闭环校正（与 [`power_cap_curr`] 不同），原因：
///
/// | 项 | 说明 |
/// |---|---|
/// | 误差方向安全 | 电流降低后电池电压会略微**升高**（内阻压降减小）⇒ `P/U` 算出的 I 偏小 ⇒ 偏保守 |
/// | 无状态 | 闭环要往 `State` 里存一个"上一拍的值"，而温控收紧是个**跟随温度连续变化**的区间，存了反而抖 |
/// | 不干扰功率上限 | `State::power_cap_ua` 已被 [`power_cap_curr`] 占用；两套闭环共享一个字段会互相污染 |
///
/// # 边界
///
/// | 情况 | 处理 | 理由 |
/// |---|---|---|
/// | `w == 0` | 返回 `0` | **停止充电**（用户把滑块拖到最左端就是这个意图） |
/// | 电压读不到（≤0.5V） | 返回 `b.curr_max` | 无法换算时宁可**放开**，避免因读数缺失卡在低速 |
/// | 算出的电流超上限 | 钳到 `b.curr_max` | 功率目标高于本机能力时不该越界 |
fn tighten_curr_ua(sample: &Sample, config: &Config, b: Bounds) -> i32 {
    // ⚠️ `0` = 停止充电（与 `target_power_w` 的 0 相反，见字段文档）
    if config.tighten_power_w == 0 {
        return 0;
    }

    let u_v = f64::from(sample.voltage_uv) / 1e6;
    if u_v <= 0.5 {
        // 电压不可信 → 不收紧（保守放行，而不是盲目压到最低）
        return b.curr_max;
    }

    let curr = (f64::from(config.tighten_power_w) / u_v) * 1e6;
    // `as i32` 前先钳到合法区间：f64 → i32 在越界时是**饱和**转换，
    // 但显式钳更清楚，也避免负数进入后续 `clamp_curr`
    curr.clamp(0.0, f64::from(b.curr_max)) as i32
}

/// 线性插值控制律
///
/// `sample` 是必需的：终点值要把用户的**功率**目标换算成电流
/// （见 [`tighten_curr_ua`]）。
fn ramp_target(sample: &Sample, temp: f32, config: &Config, b: Bounds) -> i32 {
    let start = config.tighten_temp;
    let end = ramp_end(config, b);
    if temp <= start {
        return b.curr_max;
    }
    // 终点：用户设的「降到多少」（功率 → 电流）
    let lo_curr = tighten_curr_ua(sample, config, b);
    if temp >= end || (end - start).abs() < f32::EPSILON {
        return clamp_curr(lo_curr, b);
    }
    let ratio = (temp - start) / (end - start);
    let hi = b.curr_max as f32;
    let lo = lo_curr as f32;
    let v = hi - (hi - lo) * ratio;
    clamp_curr(v as i32, b)
}

// 🔴 2026-10-14 删除 `gear_for_temp()`
//
// 它把温度线性映射到档位（0~15）参与收紧。删除理由见 `decide_with` 的
// 收紧分支：档位是 16 个台阶、电流是 18401 个台阶，用台阶表达连续渐变
// 必然跳变；且档位 14/15 会**切断快充**，导致用户在高温区设的
// "降到多少"被静默压掉。
//
// ⚠️ 删除后若还需要"按温度收紧档位"，注意先解决上述问题 ——
//    真机实测数据见 `docs/03-契约-节点契约表.md` §2A。
