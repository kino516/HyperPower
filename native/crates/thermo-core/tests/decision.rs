//! 决策内核单元测试
//!
//! ⚠️ 本文件豁免 `unwrap_used` / `expect_used`：测试代码在断言失败时**应当**
//! panic（这是测试的失败信号），与生产代码禁止 panic 的约束不同。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use thermo_core::config::{Config, ABS_MAX_TEMP_C, CURR_MAX_UA, GEAR_MAX};
use thermo_core::state::Phase;
use thermo_core::{decide, ChargingStatus, Reason, Sample, State};

fn sample(temp: f32, status: ChargingStatus) -> Sample {
    Sample {
        temp_c: temp,
        virt_c: None,
        current_ua: 3_000_000,
        voltage_uv: 4_000_000,
        soc: 50,
        status,
    }
}

#[test]
fn disabled_releases_control() {
    let cfg = Config {
        enabled: false,
        ..Config::default()
    };
    let d = decide(
        &sample(50.0, ChargingStatus::Charging),
        &cfg,
        &State::default(),
    )
    .unwrap();
    assert!(d.release);
    assert_eq!(d.reason, Reason::Disabled);
}

#[test]
fn observe_only_never_writes_control() {
    let cfg = Config {
        observe_only: true,
        ..Config::default()
    };
    let d = decide(
        &sample(43.0, ChargingStatus::Charging),
        &cfg,
        &State::default(),
    )
    .unwrap();

    // ⚠️ 这里必须断言 `should_write == false`
    //
    // 初版断言的是 `d.release == true` —— 但 `release` 的语义是
    // "**要写**入放开值"（`Decision::release` 里 `should_write: true`）。
    // 于是"只观测"实际上也在写节点，与模式名相悖。
    //
    // 现在 `observe_only` 返回 `should_write: false`：真正零写入。
    assert!(!d.should_write, "只观测模式必须零写入");
    assert!(!d.release, "只观测不是「交回官方」，不应触发释放写入");
    assert_eq!(d.reason, Reason::ObserveOnly);
}

/// 模块关闭（`enabled: false`）**要**写 —— 语义是"解除此前的限制"
///
/// 与 `observe_only` 的区别：那个是"别动"，这个是"把我的手拿开"。
/// （同名断言已在本文件下方 `disabled_releases_control` 中，此处不重复。）

#[test]
fn discharging_releases_control() {
    let cfg = Config::default();
    let d = decide(
        &sample(38.0, ChargingStatus::Discharging),
        &cfg,
        &State::default(),
    )
    .unwrap();
    assert!(d.release);
    assert_eq!(d.reason, Reason::NotCharging);
}

#[test]
fn below_tighten_is_normal() {
    let cfg = Config::default();
    let d = decide(
        &sample(37.0, ChargingStatus::Charging),
        &cfg,
        &State::default(),
    )
    .unwrap();
    assert!(!d.release);
    assert_eq!(d.reason, Reason::Normal);
    assert_eq!(d.gear, 0);
    assert_eq!(d.curr_ua, CURR_MAX_UA);
}

#[test]
fn delay_defers_action() {
    let cfg = Config {
        delay_s: 5,
        ..Config::default()
    };
    let st = State {
        over_temp_ticks: 0,
        ..State::default()
    };
    let d = decide(&sample(44.0, ChargingStatus::Charging), &cfg, &st).unwrap();
    assert_eq!(d.reason, Reason::DelayPending);
    assert!(!d.should_write);
}

#[test]
fn delay_elapsed_triggers_tighten() {
    let cfg = Config {
        delay_s: 5,
        ..Config::default()
    };
    let st = State {
        over_temp_ticks: 5,
        ..State::default()
    };
    let d = decide(&sample(44.0, ChargingStatus::Charging), &cfg, &st).unwrap();
    assert_eq!(d.reason, Reason::Tighten);
    assert!(d.should_write);
    assert!(d.curr_ua < CURR_MAX_UA);
}

#[test]
fn hard_limit_has_top_priority() {
    let cfg = Config::default();
    // 即使延迟未满、即使配置了拖延，硬上限也必须立即动作
    let st = State {
        over_temp_ticks: 0,
        ..State::default()
    };
    let d = decide(&sample(ABS_MAX_TEMP_C, ChargingStatus::Charging), &cfg, &st).unwrap();
    assert_eq!(d.reason, Reason::HardProtect);
    assert_eq!(d.phase, Phase::HardProtect);
    assert_eq!(d.gear, GEAR_MAX);
}

#[test]
fn hysteresis_holds_until_relax() {
    let cfg = Config::default(); // tighten=43, relax=42
    let st = State {
        tightening: true,
        over_temp_ticks: 100,
        ..State::default()
    };

    // 落在迟滞区（42.5）→ 保持收紧
    let d = decide(&sample(42.5, ChargingStatus::Charging), &cfg, &st).unwrap();
    assert_ne!(d.reason, Reason::Relax);

    // 回落到 41.5（< relax）→ 恢复
    let d = decide(&sample(41.5, ChargingStatus::Charging), &cfg, &st).unwrap();
    assert_eq!(d.reason, Reason::Relax);
}

#[test]
fn invalid_config_is_rejected() {
    let cfg = Config {
        relax_temp: 44.0,
        tighten_temp: 43.0,
        ..Config::default()
    };
    assert!(decide(
        &sample(40.0, ChargingStatus::Charging),
        &cfg,
        &State::default()
    )
    .is_err());
}

#[test]
fn ramp_end_is_clamped_to_physical_ceiling() {
    // tighten=58 + span=5 = 63，超出物理上限 60，必须被截断
    let cfg = Config {
        tighten_temp: 58.0,
        ramp_span: 5.0,
        relax_temp: 40.0,
        ..Config::default()
    };
    assert_eq!(cfg.ramp_end_temp(), ABS_MAX_TEMP_C);
}

/// 降速终点必须被**档案的硬保护温度**截断（而不是物理常量）
///
/// 这是"以检测数据为准"的一个落点：硬件 48℃ 的机型，
/// 降速区间不该越过 48℃。
#[test]
fn ramp_end_respects_profile_hardware_ceiling() {
    use thermo_core::decision::{evaluate_with, Bounds};
    use thermo_core::{Capabilities, ChargingStatus, Sample, State};

    // 档案：硬件保护 48℃
    let mut caps = Capabilities::default();
    caps.thermal.hardware_max_c = 48.0;
    caps.current.max_ua = 18_400_000;
    // ⚠️ 标"已检测" —— 否则决策层门禁 0 会直接返回零写入
    caps.detected = true;
    let b = Bounds::from_caps(&caps);

    // 配置：47℃ 开始收紧 + 2℃ 降速跨度 ⇒ 算出 49℃，越过 48℃
    let cfg = Config {
        tighten_temp: 47.0,
        ramp_span: 2.0,
        relax_temp: 46.0,
        ..Config::default()
    };

    // 48℃ 应触发硬保护（而不是被算成"降速区间内的某一点"）
    let sample = Sample {
        temp_c: 48.0,
        virt_c: None,
        current_ua: 1_000_000,
        voltage_uv: 4_000_000,
        soc: 50,
        status: ChargingStatus::Charging,
    };
    let d = evaluate_with(&sample, &cfg, &State::default(), &b).unwrap();
    assert_eq!(
        d.phase,
        thermo_core::state::Phase::HardProtect,
        "48℃ 达到档案硬上限，必须硬保护"
    );
}
