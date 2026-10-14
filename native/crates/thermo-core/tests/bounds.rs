//! 设备能力边界测试
//!
//! 覆盖「控制全部基于测出的数据」这条需求：
//! - 配置越界必须被拒（不能超过设备最大功率 / 电流上限）
//! - 决策钳位必须用**档案里的值**，而非写死的常量
//! - 加载时的钳位必须报告被改动的项（不可静默修正）
//!
//! ⚠️ 本文件豁免 `unwrap_used` / `expect_used`：测试代码在断言失败时**应当** panic。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// 测试里大量使用「先 Default 再改字段」的写法，可读性优先
#![allow(clippy::field_reassign_with_default)]

use thermo_core::profile::{
    CurrentCapability, GearCapability, GearRow, PowerCapability, SocLimitCapability,
    ThermalCapability,
};
use thermo_core::{decide_with, Capabilities, ChargingStatus, Config, CoreError, Sample, State};

/// 构造一份"本机档案"：最大功率 100W、电流上限 18.4A、档位 0~15
///
/// ⚠️ 标 `detected: true` —— 这是"允许控制"的闸门。测试要验证的是
/// **检测之后**的钳位行为，因此必须视为已检测（否则决策一律零写入）。
fn caps_100w() -> Capabilities {
    Capabilities {
        gear: GearCapability {
            min: 0,
            max: 15,
            open: 0,
            safe: 15,
            table: vec![
                GearRow {
                    gear: 0,
                    current_limit_ua: 13_500_000,
                },
                GearRow {
                    gear: 7,
                    current_limit_ua: 4_000_000,
                },
                GearRow {
                    gear: 14,
                    current_limit_ua: 500_000,
                },
            ],
        },
        current: CurrentCapability {
            min_ua: 0,
            max_ua: 18_400_000,
            safe_ua: 700_000,
        },
        power: PowerCapability {
            max_w: Some(100),
            max_power_gear: Some(0),
            protocol: Some("PD_PPS".into()),
        },
        thermal: ThermalCapability {
            hardware_max_c: 55.0,
            abs_max_c: 45.0,
        },
        soc_limit: Some(SocLimitCapability {
            supported: true,
            min: 60,
            max: 100,
        }),
        verified: true,
        // 已跑过检测 ⇒ 允许控制
        detected: true,
    }
}

fn sample(temp_c: f32) -> Sample {
    Sample {
        temp_c,
        virt_c: None,
        current_ua: 0,
        voltage_uv: 4_000_000,
        soc: 50,
        status: ChargingStatus::Charging,
    }
}

// ============================================================
//  一、配置边界校验
// ============================================================

/// 目标功率超过设备最大功率 → 必须被拒
#[test]
fn target_power_over_device_max_is_rejected() {
    let caps = caps_100w();
    let mut cfg = Config::default();

    cfg.target_power_w = 120; // 超 100W
    let err = cfg.validate_against(&caps).unwrap_err();

    match err {
        CoreError::OutOfBounds(msg) => {
            assert!(msg.contains("120W"), "错误信息应含具体值: {msg}");
            assert!(msg.contains("100W"), "错误信息应含设备上限: {msg}");
        }
        other => panic!("应返回 OutOfBounds，实得 {other:?}"),
    }
}

/// 目标功率恰好等于设备最大功率 → 允许
#[test]
fn target_power_equal_to_max_is_allowed() {
    let caps = caps_100w();
    let mut cfg = Config::default();
    cfg.target_power_w = 100;
    assert!(cfg.validate_against(&caps).is_ok());
}

/// 设备最大功率未知（未实测）→ 不因功率拒绝
#[test]
fn unknown_device_power_does_not_reject() {
    let mut caps = caps_100w();
    caps.power.max_w = None;

    let mut cfg = Config::default();
    cfg.target_power_w = 500;
    assert!(
        cfg.validate_against(&caps).is_ok(),
        "功率未知时不应因功率拒绝（由调用方另行提示）"
    );
}

/// 收紧功率超过设备最大功率 → 拒绝
#[test]
fn tighten_power_over_device_max_is_rejected() {
    let mut caps = caps_100w();
    caps.power.max_w = Some(30); // 假设某机型上限只有 30W

    let mut cfg = Config::default();
    // 用户设「降到 50W」，但本机上限 30W
    cfg.tighten_power_w = 50;
    assert!(matches!(
        cfg.validate_against(&caps),
        Err(CoreError::OutOfBounds(_))
    ));
}

/// 曲线电流超过上限 → 拒绝
#[test]
fn curve_current_over_device_max_is_rejected() {
    let mut caps = caps_100w();
    caps.current.max_ua = 10_000_000;

    let cfg = Config::default();
    // 默认曲线首段是 18_400_000，必然超界
    assert!(matches!(
        cfg.validate_against(&caps),
        Err(CoreError::OutOfBounds(_))
    ));
}

/// 温度阈值超过硬件保护温度 → 拒绝
#[test]
fn temp_threshold_over_hardware_max_is_rejected() {
    let mut caps = caps_100w();
    caps.thermal.hardware_max_c = 42.0;

    let mut cfg = Config::default();
    cfg.tighten_temp = 44.0;
    assert!(matches!(
        cfg.validate_against(&caps),
        Err(CoreError::OutOfBounds(_))
    ));
}

// ============================================================
//  二、加载时钳位（不可静默修正）
// ============================================================

/// 钳位必须报告被改动的项
#[test]
fn clamp_reports_each_change() {
    let mut caps = caps_100w();
    caps.current.max_ua = 5_000_000;

    let mut cfg = Config::default();
    cfg.target_power_w = 200; // 超功率上限
    cfg.tighten_power_w = 200; // 超收紧功率上限

    let changes = cfg.clamp_to(&caps);

    assert!(!changes.is_empty(), "钳位必须报告改动");
    assert!(
        changes.iter().any(|c| c.contains("target_power_w")),
        "应报告功率上限改动: {changes:?}"
    );
    assert!(
        changes.iter().any(|c| c.contains("tighten_power_w")),
        "应报告收紧功率改动: {changes:?}"
    );

    // 钳位后必须合法
    assert_eq!(cfg.target_power_w, 100);
    assert!(cfg.validate_against(&caps).is_ok());
}

/// 无需钳位时返回空列表
#[test]
fn clamp_returns_empty_when_within_bounds() {
    let caps = caps_100w();
    let mut cfg = Config::default();
    assert!(cfg.clamp_to(&caps).is_empty());
}

// ============================================================
//  三、决策钳位使用档案边界
// ============================================================

/// 放开限速时，电流取**档案上界**而非写死常量
#[test]
fn release_uses_profile_current_ceiling() {
    let mut caps = caps_100w();
    caps.current.max_ua = 12_000_000; // 把上界降到 12A

    let cfg = Config::default();
    let st = State::default();
    // 温度远低于收紧阈值 → 放开
    let d = decide_with(&sample(30.0), &cfg, &st, &caps).unwrap();

    assert_eq!(
        d.curr_ua, 12_000_000,
        "放开时应写档案上界 12A，而不是写死的 18.4A"
    );
    assert_eq!(d.gear, caps.gear.open, "放开时档位应取最松");
}

/// **实测**温度收紧时档位与电流的取值
///
/// # 这个测试锁住的行为（2026-10-14 修正）
///
/// 温度收紧**只走电流通道**，档位**始终保持放开**。
///
/// ## 为什么改成这样
///
/// 与老项目（`ThermoCharge`）对齐 —— 它的执行器
/// `act_apply_dual` **无条件把档位写成 `GEAR_OPEN`**，只让电流随温度变化：
///
/// ```sh
/// act_apply_dual() {
///   _curr=$(_clamp_curr "$1")            # ← 只接收电流
///   write_checked "$N_CTRL" "$GEAR_OPEN" # ← 档位恒为最松
///   write_checked "$N_CC1"  "$_curr"     # ← 电流动态
/// }
/// # 且 act_apply 的 dual 分支**丢弃** gear 参数：
/// case "$ACT" in dual) act_apply_dual "$_curr" ;; esac
/// ```
///
/// 本项目曾用 `gear_for_temp()` 把温度线性映射到档位。真机实测证明
/// 那样做有害（见 `docs/03` §2A）：
///
/// | 通道 | 可取值 | 相邻跳变 |
/// |---|---|---|
/// | 档位 | **16 个**（0~15） | 最高区 **2500 mA** 一跳 |
/// | 电流 | **18401 个**（0~18400 mA） | **1 mA** |
///
/// 用台阶表达连续的温度渐变必然跳变；更要紧的是**档位 14/15 会切断快充**
/// （实测总线电压 16V → 8.9V 腰斩，功率 44W → 1.9W，三轮复现）。
///
/// ⚠️ **硬保护仍然两层收紧**（见 `hard_protect_uses_profile_safe_gear`）
/// —— 那里档位取最紧档是**主动切断快充**的手段，实测有效，保留。
#[test]
fn 温度收紧时只走电流不改档位() {
    let caps = caps_100w(); // gear.max=15, gear.open=0, open档电流 13.5A
    let cfg = Config::default(); // tighten=43, ramp_span=2 ⇒ ramp_end=45

    // 处于收紧态（跳过延迟门禁），温度逐点观察
    let st = State {
        tightening: true,
        ..State::default()
    };

    for t in [43.0_f32, 44.0, 44.5, 45.0] {
        let d = decide_with(&sample(t), &cfg, &st, &caps).unwrap();
        println!(
            "  温度 {t:.1}℃ ⇒ 档位={}（放开档是 {}）  电流={}μA",
            d.gear, caps.gear.open, d.curr_ua
        );
    }

    let d43 = decide_with(&sample(43.0), &cfg, &st, &caps).unwrap();
    let d44 = decide_with(&sample(44.0), &cfg, &st, &caps).unwrap();
    let d45 = decide_with(&sample(45.0), &cfg, &st, &caps).unwrap();

    // ── 档位在整段收紧区间内**恒定放开** ──
    for (t, d) in [(43.0, &d43), (44.0, &d44), (45.0, &d45)] {
        assert_eq!(
            d.gear, caps.gear.open,
            "{t}℃ 收紧时档位必须保持放开（= {}），实际 {}",
            caps.gear.open, d.gear
        );
    }

    // ── 电流**确实**随温度收紧（否则等于没控温）──
    assert!(
        d45.curr_ua < d43.curr_ua,
        "电流必须随温度下降：43℃={}μA 应大于 45℃={}μA",
        d43.curr_ua,
        d45.curr_ua
    );

    // ── 收紧时**必然**改变电流（与"温度正常"路径区分开）──
    let idle = decide_with(&sample(30.0), &cfg, &State::default(), &caps).unwrap();
    assert_ne!(
        d45.curr_ua, idle.curr_ua,
        "45℃ 收紧时的电流应与 30℃ 正常时有明显区别"
    );
}

/// 🔴 **默认配置**下，用户设的「降到多少」必须生效
///
/// # 这个测试为什么必须存在（它抓的是一个真实缺陷）
///
/// 下面那个 `用户设定的收紧电流在高温区仍精确生效` 测试**显式设了**
/// `control_law = Switch` —— 于是它绕开了默认的 `curve` 律，
/// **没能发现**界面上的「降到多少」滑块在默认配置下完全失效。
///
/// 真实情况（2026-10-14 发现）：
///
/// | 律 | 用 `tighten_curr`？ |
/// |---|---|
/// | `curve`（旧默认） | ❌ **完全不用** —— 终点由 `curve` 数组末段决定 |
/// | `ramp`（现默认） | ✅ 是过渡终点 |
///
/// ⇒ 旧默认下，用户设「降到 2A」到 45℃ 实际仍降到 **0.7A**（曲线末值）；
/// 设「0（停止充电）」也降不到 0。
///
/// 本测试**不设 `control_law`**，用的就是出厂默认 —— 因此能抓到这类
/// "默认值让设置失效"的问题。**改任何默认值时都应先看这条测试**。
#[test]
fn 默认配置下降低的电流设定必须生效() {
    let caps = caps_100w();

    // ⚠️ 刻意不碰 `control_law` —— 要测的就是**出厂默认**的行为
    let mut cfg = Config::default();
    assert_eq!(
        cfg.control_law,
        thermo_core::ControlLaw::Ramp,
        "出厂默认应为 ramp —— 否则界面上的「降到多少」会失效（见本测试文档）"
    );

    // 8 W 在 4.0 V 下 = 2 A（本文件的 `sample()` 用 4_000_000 μV）
    cfg.tighten_power_w = 8;
    cfg.tighten_temp = 43.0;
    cfg.ramp_span = 2.0;

    let st = State {
        tightening: true,
        ..State::default()
    };

    // 过渡终点（43 + 2 = 45℃）：应**正好**停在用户设定的 2A
    let d45 = decide_with(&sample(45.0), &cfg, &st, &caps).unwrap();
    assert_eq!(
        d45.curr_ua, 2_000_000,
        "45℃（过渡终点）应精确停在「8W ÷ 4.0V = 2A」，实得 {}",
        d45.curr_ua
    );

    // 超过终点后保持该值（不能继续往下掉）
    let d47 = decide_with(&sample(47.0), &cfg, &st, &caps).unwrap();
    assert_eq!(d47.curr_ua, 2_000_000, "超过终点后应保持在用户设定值");

    // 起点（43℃）应仍是满流 —— 过渡是"从满到目标"的斜坡
    let d43 = decide_with(&sample(43.0), &cfg, &st, &caps).unwrap();
    assert!(
        d43.curr_ua > 2_000_000,
        "43℃（过渡起点）应高于目标值，实得 {}",
        d43.curr_ua
    );

    // 中点应落在两者之间（验证是"线性过渡"而非跳变）
    let d44 = decide_with(&sample(44.0), &cfg, &st, &caps).unwrap();
    assert!(
        d44.curr_ua > 2_000_000 && d44.curr_ua < d43.curr_ua,
        "44℃ 应处于两者之间（线性过渡），实得 {}",
        d44.curr_ua
    );
}

/// 🔴 默认配置下，**设 0 = 停止充电**必须真的能做得到
///
/// 用户明确要求过「**达到此温度停止充电**」这个选项（2026-10-14）。
/// 而旧的 `curve` 默认律下，无论怎么设都降不到 0（曲线末段是 0.7A）。
#[test]
fn 默认配置下可以设成停止充电() {
    let caps = caps_100w();
    let mut cfg = Config::default();
    cfg.tighten_power_w = 0; // 用户设「停止充电」
    cfg.tighten_temp = 43.0;
    cfg.ramp_span = 2.0;

    let st = State {
        tightening: true,
        ..State::default()
    };

    let d45 = decide_with(&sample(45.0), &cfg, &st, &caps).unwrap();
    assert_eq!(
        d45.curr_ua, 0,
        "设 0 应真的写到 0（停止充电），实得 {}",
        d45.curr_ua
    );
}

/// 用户设的「收紧后降到多少」在**任何温度**都精确生效
///
/// # 这条性质为什么重要
///
/// 用户明确要求：「**收紧后降到多少由我定**」（2026-10-13）。
///
/// 修正前：档位随温度收紧 ⇒ 45℃ 时档位取最紧（对应 0.5A）
/// ⇒ 两通道并联取 min ⇒ 用户设的 2A 被**静默压到 0.5A**。
///
/// 修正后：档位恒放开 ⇒ 电流是唯一约束 ⇒ 用户设定精确生效。
///
/// 真实档案的档位表（`docs/03` 交叉验证过）：
///
/// | 档位 | 电流上限 |
/// |---|---|
/// | 0 | 13.5 A |
/// | 8 | 3.5 A |
/// | 15 | 0.5 A（驱动额外接受的最慢档） |
#[test]
fn 用户设定的收紧电流在高温区仍精确生效() {
    let mut caps = caps_100w();
    // 给一份接近真机的档位表（含最慢档 15 ⇒ 0.5A）
    caps.gear.table = vec![
        GearRow {
            gear: 0,
            current_limit_ua: 13_500_000,
        },
        GearRow {
            gear: 8,
            current_limit_ua: 3_500_000,
        },
        GearRow {
            gear: 15,
            current_limit_ua: 500_000,
        },
    ];

    // 用户希望"收紧后降到 2A"（= 8W @ 4.0V），用开关律
    let mut cfg = Config::default();
    cfg.tighten_power_w = 8;
    cfg.control_law = thermo_core::ControlLaw::Switch;
    cfg.tighten_temp = 43.0;

    let st = State {
        tightening: true,
        ..State::default()
    };

    // ── 整个收紧区间内，电流恒为用户设定值且档位放开 ──
    //
    // ⚠️ 45℃ 是关键点：修正前此处档位变成 15（0.5A），会把 2A 压掉。
    for t in [44.0_f32, 44.5, 45.0, 46.0] {
        let d = decide_with(&sample(t), &cfg, &st, &caps).unwrap();
        println!(
            "  {t:.1}℃: 电流={}μA（用户设 2A）  档位={}",
            d.curr_ua, d.gear
        );
        assert_eq!(d.curr_ua, 2_000_000, "{t}℃ 时用户设定的 2A 必须原样生效");
        assert_eq!(
            d.gear, caps.gear.open,
            "{t}℃ 时档位必须放开，否则档位层会卡住用户设定"
        );
    }
}

/// 硬保护时档位取档案的 safe 值
#[test]
fn hard_protect_uses_profile_safe_gear() {
    let mut caps = caps_100w();
    caps.gear.max = 15;
    caps.gear.safe = 12; // 若某机型安全档是 12

    let cfg = Config::default();
    let st = State::default();

    // ⚠️ 温度取 56℃，因为假档案的 `hardware_max_c` 是 55℃。
    //
    // 这里曾写 46.0 —— 当时硬保护阈值是常量 `ABS_MAX_TEMP_C = 45.0`。
    // 现在改用档案的 `hardware_max_c`（**这是修复**：常量 45℃ 会让
    // 硬件允许 55℃ 的机型白白少 10℃ 余量）。
    let d = decide_with(&sample(56.0), &cfg, &st, &caps).unwrap();

    assert_eq!(d.gear, 12, "硬保护应写档案的 safe 档位");
}

/// 硬保护阈值必须跟随档案，而不是常量
#[test]
fn hard_protect_threshold_follows_profile() {
    // 假档案 hardware_max_c = 55℃，因此 50℃ **不该**触发硬保护
    let caps = caps_100w();
    let cfg = Config::default();
    let st = State::default();

    let d = decide_with(&sample(50.0), &cfg, &st, &caps).unwrap();
    assert_ne!(
        d.phase,
        thermo_core::state::Phase::HardProtect,
        "50℃ 未达档案的 55℃ 硬上限，不该硬保护（用常量 45℃ 就会误触发）"
    );

    // 而 56℃ 必须触发
    let d = decide_with(&sample(56.0), &cfg, &st, &caps).unwrap();
    assert_eq!(
        d.phase,
        thermo_core::state::Phase::HardProtect,
        "56℃ 已超档案的 55℃ 硬上限，必须硬保护"
    );
}

/// 功率上限：设了就不许超过，且不影响档位
#[test]
fn power_limit_caps_current_but_not_gear() {
    let caps = caps_100w();
    let mut cfg = Config::default();

    // 设 50W 上限。假档案电压 4.0V ⇒ 前馈 = 50/4.0 = 12.5A
    cfg.target_power_w = 50;

    let st = State::default();
    // 温度远低于收紧线 ⇒ 走"功率限制"分支
    let d = decide_with(&sample(30.0), &cfg, &st, &caps).unwrap();

    assert_eq!(d.reason, thermo_core::Reason::PowerLimit);
    assert!(
        d.curr_ua <= 12_500_000,
        "50W ÷ 4.0V = 12.5A，实得 {} μA",
        d.curr_ua
    );
    assert!(d.power_cap_ua.is_some(), "功率闭环状态应被记录");

    // ⚠️ 档位必须仍取最松 —— 功率限制只走电流通道
    assert_eq!(d.gear, caps.gear.open, "功率限制不应改变档位");
}

/// 功率上限为 0 = 不限制
#[test]
fn zero_power_target_means_unlimited() {
    let caps = caps_100w();
    let mut cfg = Config::default();
    cfg.target_power_w = 0;

    let st = State::default();
    let d = decide_with(&sample(30.0), &cfg, &st, &caps).unwrap();

    assert_eq!(d.curr_ua, caps.current.max_ua, "0 = 不限，应放到档案上界");
    assert_eq!(d.reason, thermo_core::Reason::Normal);
    assert!(d.power_cap_ua.is_none());
}

/// 功率闭环会按实测功率校正
#[test]
fn power_loop_corrects_by_measured_power() {
    let caps = caps_100w();
    let mut cfg = Config::default();
    cfg.target_power_w = 50;

    // 状态里记住上一拍的限制值 12.5A
    let mut st = State::default();
    st.power_cap_ua = Some(12_500_000);

    // 实测功率 100W（远超目标 50W）⇒ 应显著收紧
    let mut s = sample(30.0);
    s.current_ua = 25_000_000; // 25A × 4.0V = 100W
    s.voltage_uv = 4_000_000;

    let d = decide_with(&s, &cfg, &st, &caps).unwrap();
    let cap = d.power_cap_ua.expect("应记录闭环状态");

    assert!(
        cap < 12_500_000,
        "实测 100W 超目标 50W，限制值必须收紧（12.5A → {} μA）",
        cap
    );
    assert!(cap > 0, "不应收紧到 0");
}

/// 实测功率低于目标时放松
#[test]
fn power_loop_relaxes_when_below_target() {
    let caps = caps_100w();
    let mut cfg = Config::default();
    cfg.target_power_w = 50;

    let mut st = State::default();
    st.power_cap_ua = Some(5_000_000); // 5A

    // 实测只有 9W（5A × 4.0V = 20W？不，这里给 2.25A × 4V = 9W）
    let mut s = sample(30.0);
    s.current_ua = 2_250_000;
    s.voltage_uv = 4_000_000;

    let d = decide_with(&s, &cfg, &st, &caps).unwrap();
    let cap = d.power_cap_ua.expect("应记录闭环状态");

    assert!(
        cap > 5_000_000,
        "实测 9W 远低于目标 50W，限制值应放松（5A → {} μA）",
        cap
    );
    assert!(cap <= caps.current.max_ua, "放松不得超出档案上界");
}

/// 死区：误差在 ±5% 内不动
#[test]
fn power_loop_has_deadband() {
    let caps = caps_100w();
    let mut cfg = Config::default();
    cfg.target_power_w = 50;

    let mut st = State::default();
    st.power_cap_ua = Some(12_500_000);

    // 实测 51W（目标 50W，误差 2%，在死区内）
    let mut s = sample(30.0);
    s.current_ua = 12_750_000; // 12.75A × 4.0V = 51W
    s.voltage_uv = 4_000_000;

    let d = decide_with(&s, &cfg, &st, &caps).unwrap();
    assert_eq!(
        d.power_cap_ua,
        Some(12_500_000),
        "误差 2% 在死区内，限制值应保持不变（防抖）"
    );
}

/// 温度限制更严时，温度优先
#[test]
fn temperature_wins_when_stricter() {
    let caps = caps_100w();
    let mut cfg = Config::default();
    cfg.target_power_w = 50; // 功率允许较宽

    let mut st = State::default();
    // 越过延迟门禁（默认 delay_s = 5，需连续超温计拍够）
    st.over_temp_ticks = cfg.delay_s;
    // 温度在收紧区（默认 tighten_temp = 43℃），44℃ 是过渡区中点
    let d = decide_with(&sample(44.0), &cfg, &st, &caps).unwrap();

    assert_eq!(d.reason, thermo_core::Reason::Tighten);

    // ── 两条约束各要求多少 ──
    //
    // | 来源 | 算法 | 结果 |
    // |---|---|---|
    // | 功率 | `I = P/U = 50W / 4.0V` | **12.5 A** |
    // | 温度 | `ramp` 律在 44℃ 的中点：`18400000 - (18400000-l)×0.5`，其中 `l` = 默认 `tighten_power_w` 换算后的电流 | **9.575 A** |
    //
    // `l` 的算法（默认 `tighten_power_w = 3`）：`3W / 4.0V × 1e6 = 750_000 μA`
    // ⇒ `18400000 - (18400000 - 750000) × 0.5 = 9_575_000`
    //
    // ⇒ 温度更严 ⇒ 最终应取 9.575 A
    //
    // ⚠️ 此处曾断言 `<= 2_500_000`（旧 `curve` 律的段值）。默认律改成 `ramp`
    //    后该断言不再成立 —— 见 `默认配置下降低的电流设定必须生效` 的说明。
    //
    // ⚠️ 也曾经是 `9_550_000`（那时"降到多少"是 **0.7 A** 的电流默认值）。
    //    改成功率后默认值是 **3 W**，在 4.0 V 下等于 0.75 A ⇒ 结果随之变化。
    //    **这是预期的**，不是回归。
    assert_eq!(
        d.curr_ua, 9_575_000,
        "温度更严时应取温度诉求值（功率允许 12.5A，温度只要 9.575A），实得 {}",
        d.curr_ua
    );
    assert!(
        d.curr_ua < 12_500_000,
        "结果必须严于功率单独允许的量，实得 {}",
        d.curr_ua
    );
}

/// 档案电流上限更低时，曲线输出被钳到该上限
#[test]
fn curve_output_is_clamped_to_profile_ceiling() {
    let mut caps = caps_100w();
    caps.current.max_ua = 3_000_000; // 上界 3A

    let mut cfg = Config::default();
    // 收紧功率合法（3 W 在 4.0 V 下 = 0.75 A，不超 3A）
    cfg.tighten_power_w = 3;
    // 曲线全部压到 3A 以内，避免 validate_against 拒绝
    for p in cfg.curve.iter_mut() {
        p.curr = p.curr.min(3_000_000);
    }

    // 温度落在曲线首段（38℃ 以下）→ 目标 3A
    let st = State::default();
    let d = decide_with(&sample(37.0), &cfg, &st, &caps).unwrap();
    assert!(d.curr_ua <= 3_000_000, "输出不得超过档案上界");
}

/// 电流必须**对齐到整毫安**（驱动的最小有效步进）
///
/// # 为什么必须有这个测试
///
/// 真机实测（2026-10-14，模块停掉后测）证明驱动只接受 **mA** 精度，
/// 且是**向下截断**：
///
/// | 写入（μA） | 驱动回读 | 差 |
/// |---|---|---|
/// | `3456789` | `3456000` | -789 |
/// | `1001501` | `1001000` | -501 |
/// | `1001000` | `1001000` | 原样 |
///
/// 而决策层由浮点算出的值**几乎都不是整 mA**：
///
/// ```text
/// 功率闭环：I = P / U ⇒ 12000000 / 4.2 = 2857142.857… μA
/// ```
///
/// 若不对齐，模块记录的 `state.curr_ua` 会与节点**实际值**不一致，
/// 历史曲线画的也是"想写的值"而非"生效的值" —— 属静默偏差。
#[test]
fn 电流对齐到整毫安() {
    let caps = caps_100w();
    let cfg = Config::default();
    let st = State::default();

    // 一路验证：任何决策给出的电流都应是 1000 的整数倍
    for t in [25.0_f32, 35.0, 40.0, 42.0, 43.0, 44.0, 45.0, 46.0, 47.0] {
        let s = sample(t);
        let d = decide_with(&s, &cfg, &st, &caps).unwrap();
        assert_eq!(
            d.curr_ua % 1_000,
            0,
            "{t}℃ 时电流 {}μA 未对齐到 mA —— 记录值会与实际值不符",
            d.curr_ua
        );
    }

    // 另一路验证：B 档（`Bounds::default`）同样对齐
    let idle = decide_with(&sample(30.0), &cfg, &st, &caps).unwrap();
    assert_eq!(idle.curr_ua % 1_000, 0, "放开时也应是对齐值");

    // 释放路径（`Decision::release`）不经 `clamp_curr`，单独验证
    let mut off = Config::default();
    off.enabled = false;
    let d = decide_with(&sample(30.0), &off, &st, &caps).unwrap();
    assert!(d.release, "关闭开关应返回释放决策");
    assert_eq!(d.curr_ua % 1_000, 0, "释放值 {}μA 未对齐到 mA", d.curr_ua);
}

/// 档案给出的电流上限若不是整 mA，也要对齐
///
/// （档案可能被人工编辑或由旧版本写入，不能假定它一定是整 mA。）
#[test]
fn 档案电流上限也会被对齐() {
    let mut caps = caps_100w();
    caps.current.max_ua = 18_400_999; // 特意给一个非整 mA 的值

    let cfg = Config::default();
    let st = State::default();
    let d = decide_with(&sample(30.0), &cfg, &st, &caps).unwrap();

    assert_eq!(
        d.curr_ua, 18_400_000,
        "上限应向下对齐到 18400000，实得 {}",
        d.curr_ua
    );
}

/// 真实场景：目标功率合法但有档案 → 决策正常
#[test]
fn decision_works_with_real_profile() {
    let caps = caps_100w();
    let cfg = Config::default();
    let st = State::default();

    // 正常温度：放开
    let d = decide_with(&sample(30.0), &cfg, &st, &caps).unwrap();
    assert!(!d.release);

    // 超温持续（模拟已过延迟）：收紧
    let mut st2 = State::default();
    st2.over_temp_ticks = 10;
    let d2 = decide_with(&sample(44.0), &cfg, &st2, &caps).unwrap();
    assert!(d2.curr_ua < caps.current.max_ua, "收紧后电流应低于上限");
}
