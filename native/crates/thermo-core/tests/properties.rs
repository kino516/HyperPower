//! 属性测试 —— 证明 5 条安全命题（见 lib.rs 文档）
//!
//! 这些命题是安全边界的形式化保证，**不允许削弱或跳过**。
//!
//! ⚠️ 仅在非 Windows 平台编译（见 Cargo.toml 说明）：CI 的 ubuntu-24.04 会执行全量。

#![cfg(not(windows))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use proptest::prelude::*;
use thermo_core::config::{ABS_MAX_TEMP_C, CURR_MAX_UA, GEAR_MAX};
use thermo_core::profile::{
    Capabilities, CurrentCapability, GearCapability, PowerCapability, ThermalCapability,
};
use thermo_core::{decide, decide_with, ChargingStatus, Config, Sample, State};

/// 造一份**真实形状**的能力档案（档位上限 15，与 K90PM 实测一致）
///
/// # 为什么需要它
///
/// `decide()` 用的是 [`thermo_core::Bounds::default`]，其中
/// `gear_max = GEAR_MAX = u8::MAX` —— 于是"档位不超上限"这类断言
/// **恒为真**（`u8 <= 255`），等于什么都没测。
///
/// 而**生产路径**走的是 `decide_with(caps)`，档位上限来自设备档案的
/// `capabilities.gear.max`（= 档位表行数，因机型而异）。本测试用一份
/// 带真实边界的档案，才能验证"钳位确实按档案生效"。
///
/// ⚠️ 若用 `Bounds::default()` 写这类命题，它永远不会失败 ——
/// 那是最危险的一类"假测试"（绿着，但什么都没保证）。
fn arb_caps() -> impl Strategy<Value = Capabilities> {
    // 档位上限故意取一个**小于 u8::MAX** 的真实值，
    // 这样"不超上限"才是一个可以被违反的命题
    (1u8..=64u8, 1_000_000i32..=30_000_000i32, 30.0f32..59.0f32).prop_map(
        |(gear_max, curr_max, hw_max)| Capabilities {
            gear: GearCapability {
                min: 0,
                max: gear_max,
                open: 0,
                safe: gear_max,
                table: Vec::new(),
            },
            current: CurrentCapability {
                min_ua: 0,
                max_ua: curr_max,
                safe_ua: curr_max,
            },
            power: PowerCapability {
                max_w: None,
                max_power_gear: None,
                protocol: None,
            },
            thermal: ThermalCapability {
                hardware_max_c: hw_max,
                abs_max_c: ABS_MAX_TEMP_C,
            },
            soc_limit: None,
            detected: true,
            verified: false,
        },
    )
}

/// 生成任意采样
fn arb_sample() -> impl Strategy<Value = Sample> {
    (
        -20.0f32..80.0f32,
        proptest::option::of(-20.0f32..80.0f32),
        -30_000_000i32..30_000_000i32,
        3_000_000i32..5_000_000i32,
        0u8..=100u8,
        prop_oneof![
            Just(ChargingStatus::Charging),
            Just(ChargingStatus::Full),
            Just(ChargingStatus::Discharging),
            Just(ChargingStatus::NotCharging),
            Just(ChargingStatus::Unknown),
        ],
    )
        .prop_map(
            |(temp_c, virt_c, current_ua, voltage_uv, soc, status)| Sample {
                temp_c,
                virt_c,
                current_ua,
                voltage_uv,
                soc,
                status,
            },
        )
}

/// 生成任意状态
fn arb_state() -> impl Strategy<Value = State> {
    (
        0u8..=GEAR_MAX,
        0i32..=CURR_MAX_UA,
        proptest::option::of(-20.0f32..80.0f32),
        0u32..1000u32,
        any::<bool>(),
    )
        .prop_map(
            |(gear, curr_ua, peak_c, over_temp_ticks, tightening)| State {
                gear,
                curr_ua,
                peak_c,
                over_temp_ticks,
                tightening,
                ..State::default()
            },
        )
}

proptest! {
    /// 命题 1：任意输入下，输出电流不超过安全上限
    #[test]
    fn prop_current_never_exceeds_max(s in arb_sample(), st in arb_state()) {
        let cfg = Config::default();
        if let Ok(d) = decide(&s, &cfg, &st) {
            prop_assert!(d.curr_ua >= 0, "电流不得为负: {}", d.curr_ua);
            prop_assert!(d.curr_ua <= CURR_MAX_UA, "电流超上限: {}", d.curr_ua);
        }
    }

    /// 命题 2：任意输入下，输出档位**不超过档案边界**
    ///
    /// ⚠️ 必须走 `decide_with(caps)` 而不是 `decide()`：
    ///    后者的 `gear_max` 是 `u8::MAX`，断言恒真、**测不出任何东西**。
    ///    生产路径用的正是 `decide_with`，所以这里测的才是真路径。
    #[test]
    fn prop_gear_within_range(s in arb_sample(), st in arb_state(), caps in arb_caps()) {
        let cfg = Config::default();
        if let Ok(d) = decide_with(&s, &cfg, &st, &caps) {
            prop_assert!(
                d.gear <= caps.gear.max,
                "档位超档案上限: gear={} max={}",
                d.gear,
                caps.gear.max
            );
        }
    }

    /// 命题 2b：任意输入下，输出电流**不超过档案边界**
    ///
    /// 与命题 1（物理上限）互补：这里测的是**机型边界**。
    #[test]
    fn prop_current_within_caps(s in arb_sample(), st in arb_state(), caps in arb_caps()) {
        let cfg = Config::default();
        if let Ok(d) = decide_with(&s, &cfg, &st, &caps) {
            prop_assert!(d.curr_ua >= 0, "电流不得为负: {}", d.curr_ua);
            prop_assert!(
                d.curr_ua <= caps.current.max_ua,
                "电流超档案上限: curr={} max={}",
                d.curr_ua,
                caps.current.max_ua
            );
        }
    }

    /// 命题 3：达到硬上限温度必然进入硬保护
    #[test]
    fn prop_hard_limit_forces_protection(
        mut s in arb_sample(),
        st in arb_state(),
        extra in 0.0f32..10.0f32,
    ) {
        let cfg = Config::default();
        s.status = ChargingStatus::Charging;
        s.temp_c = ABS_MAX_TEMP_C + extra;
        s.virt_c = None; // 排除虚拟温度干扰
        let d = decide(&s, &cfg, &st).expect("决策应当成功");
        prop_assert_eq!(d.phase, thermo_core::state::Phase::HardProtect,
            "达到硬上限必须硬保护: temp={}", s.temp_c);
        prop_assert!(!d.release, "硬保护不得交回官方");
    }

    /// 命题 4：迟滞区不会产生每拍振荡
    ///
    /// 构造：收紧态下温度处于 [relax, tighten) 区间，应保持收紧（不恢复）。
    #[test]
    fn prop_hysteresis_no_oscillation(
        s in arb_sample(),
        gear in 0u8..=GEAR_MAX,
        curr in 0i32..=CURR_MAX_UA,
        delta in 0.1f32..0.9f32,
    ) {
        let cfg = Config::default();
        // 温度落在迟滞区内：relax_temp <= t < tighten_temp
        let t = cfg.relax_temp + delta * (cfg.tighten_temp - cfg.relax_temp);
        let sample = Sample {
            temp_c: t,
            virt_c: None,
            status: ChargingStatus::Charging,
            ..s
        };
        let tightening_state = State {
            gear,
            curr_ua: curr,
            tightening: true,
            ..State::default()
        };
        let d = decide(&sample, &cfg, &tightening_state).expect("决策应当成功");
        prop_assert_ne!(
            d.reason,
            thermo_core::Reason::Relax,
            "迟滞区内不应恢复（会导致振荡）: temp={}", t
        );
        prop_assert_eq!(d.phase, thermo_core::state::Phase::Tightening);
    }

    /// 命题 5：配置损坏时不会产生越界写值
    ///
    /// ⚠️ 断言同样必须对着**档案边界**（不是 `u8::MAX` / `CURR_MAX_UA`），
    ///    否则又是恒真。用 `decide_with` 才能证明"非法配置 + 真实机型边界"
    ///    这个组合下仍然安全 —— 这才是生产路径的真实处境。
    #[test]
    fn prop_invalid_config_never_produces_out_of_range(
        s in arb_sample(),
        st in arb_state(),
        caps in arb_caps(),
        bad_tighten in -100.0f32..0.0f32,
    ) {
        // 用结构体字面量构造（而非先 Default 再改字段）：
        // 后者会被 clippy 判为 field_reassign_with_default
        let cfg = Config {
            tighten_temp: bad_tighten, // 非法配置
            ..Config::default()
        };
        match decide_with(&s, &cfg, &st, &caps) {
            // 非法配置必须被拒绝（fail-closed），而不是产生越界值
            Err(_) => {}
            Ok(d) => {
                prop_assert!(d.curr_ua <= caps.current.max_ua, "电流越界: {}", d.curr_ua);
                prop_assert!(d.gear <= caps.gear.max, "档位越界: {}", d.gear);
            }
        }
    }
}
