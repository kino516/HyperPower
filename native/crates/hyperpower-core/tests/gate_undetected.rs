//! R2「未检测 ⇒ 零控制」的**门禁优先级**测试
//!
//! # 为什么单独一个文件（2026-11-01 发布前审计 M-10）
//!
//! R2 是项目的**最高优先级安全边界**：
//!
//! > `Bounds.detected == false` ⇒ `should_write: false`、`release: false`，
//! > 一个节点都不写。该门禁**排在 `enabled` 之前**。
//!
//! 但审计发现 `core/tests/` 全域 **没有任何 `detected: false` 的测试**：
//!
//! ```text
//! grep 'detected' core/tests/*.rs | grep -v true   ⇒  0 命中
//! ```
//!
//! 现有测试全部硬编码 `detected: true`（`bounds.rs:26`、`properties.rs:58`，
//! 各自还都写了「否则门禁 0 直接返回」的注释）——也就是说，这条边界在
//! **内核层完全没有守护**，只靠 `engine.rs` 里一个端到端测试。
//!
//! AGENTS.md §4.1 明确要求：
//!
//! > 新门禁/优先级 ⇒ 一条「**优先级更高时不会被绕过**」的测试
//!
//! 本文件补的就是这一条：用**最极端的输入**（60℃ 硬保护 + `enabled:true`
//! + 充电中 + 功率限制）去撞 `detected:false`，证明它撞不穿。
//!
//! ⚠️ 本文件豁免 `unwrap_used` / `expect_used`：测试代码在断言失败时**应当** panic。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::field_reassign_with_default)]

use hyperpower_core::profile::{
    CurrentCapability, GearCapability, GearRow, PowerCapability, ThermalCapability,
};
use hyperpower_core::{decide_with, Capabilities, ChargingStatus, Config, Sample, State};

/// 构造一份**未检测**的能力档案（`detected: false`）
///
/// 其余字段故意填成"看起来正常"的值：真实场景里 `detected:false` 时
/// 这些是出厂保守兜底（`Capabilities::default()`），但测试要点是
/// **无论它们是多少，都不该被用来控制**。
fn caps_undetected() -> Capabilities {
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
                    gear: 15,
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
            max_w: Some(120),
            max_power_gear: Some(0),
            protocol: None,
        },
        thermal: ThermalCapability {
            hardware_max_c: 48.0,
            abs_max_c: 60.0,
        },
        // 🔴 本文件的主题
        detected: false,
        ..Capabilities::default()
    }
}

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

/// 断言"零控制"：不写、也不释放
fn assert_zero_control(d: &hyperpower_core::Decision, why: &str) {
    assert!(
        !d.should_write,
        "{why}：未检测时 should_write 必须为 false（实际 true）"
    );
    assert!(
        !d.release,
        "{why}：未检测时 release 也必须为 false（'不写' ≠ '释放'，实际 true）"
    );
}

/// **R2 核心**：未检测 + 高温硬保护（60℃）+ 已启用 + 充电中 ⇒ 仍然零写入
///
/// 这是最容易被绕过的一条路径：硬保护是"安全路径"，它在决策链里
/// 优先级很高（`decision.rs` 的 `temp >= hardware_max_c`），
/// 而且返回 `should_write: true`。必须证明 R2 排在它**之前**。
#[test]
fn 未检测_硬保护温度也不写一个节点() {
    let caps = caps_undetected();
    let cfg = Config {
        enabled: true,
        hardware_protect: true,
        ..Config::default()
    };
    // 60℃ —— 高于档案 hardware_max_c(48) 与 abs_max_c(60)
    let d = decide_with(
        &sample(60.0, ChargingStatus::Charging),
        &cfg,
        &State::default(),
        &caps,
    )
    .unwrap();

    assert_zero_control(&d, "未检测+60℃");
    assert_ne!(
        d.reason,
        hyperpower_core::Reason::HardProtect,
        "未检测时不应走到硬保护决策（那意味着会写节点）"
    );
}

/// **R2 措辞的直接要求**：门禁排在 `enabled` **之前**
///
/// `enabled: false` 的正常语义是"写一次释放值"（解除此前限制）。
/// 但在未检测状态下，模块**从未写入过任何东西**，所以也没有东西可释放 ——
/// 「零控制」必须连释放一起排除。
#[test]
fn 未检测_关闭模块也不释放() {
    let caps = caps_undetected();
    let cfg = Config {
        enabled: false,
        ..Config::default()
    };
    let d = decide_with(
        &sample(30.0, ChargingStatus::Charging),
        &cfg,
        &State::default(),
        &caps,
    )
    .unwrap();

    assert_zero_control(&d, "未检测+enabled:false");
    assert_ne!(
        d.reason,
        hyperpower_core::Reason::Disabled,
        "R2 必须排在 enabled 之前 —— 未检测时不应走到 Disabled 分支"
    );
}

/// 未检测 + 官方模式 ⇒ 仍零写入（official 的正常语义是"写一次释放"）
#[test]
fn 未检测_官方模式也不释放() {
    let caps = caps_undetected();
    let cfg = Config {
        enabled: true,
        mode: hyperpower_core::Mode::Official,
        ..Config::default()
    };
    let d = decide_with(
        &sample(35.0, ChargingStatus::Charging),
        &cfg,
        &State::default(),
        &caps,
    )
    .unwrap();

    assert_zero_control(&d, "未检测+official");
    assert_ne!(
        d.reason,
        hyperpower_core::Reason::Official,
        "R2 必须排在 official 之前"
    );
}

/// 未检测 + 温和温度（本该正常控制）⇒ 零写入
///
/// 覆盖"看起来一切正常"的常态：用户刚装完模块、还没跑检测、
/// 温度不高。此时**不控制**是正确行为（交给系统原有逻辑）。
#[test]
fn 未检测_常温也不控制() {
    let caps = caps_undetected();
    let cfg = Config {
        enabled: true,
        ..Config::default()
    };
    let d = decide_with(
        &sample(30.0, ChargingStatus::Charging),
        &cfg,
        &State::default(),
        &caps,
    )
    .unwrap();

    assert_zero_control(&d, "未检测+常温");
}

/// 对照实验：**同一份输入**，只把 `detected` 改成 true ⇒ 就会写
///
/// 这条用来证明上面几条不是"因为别的门禁先拦住了"的假阳性 ——
/// 差异**只来自** `detected` 这一个字段。
#[test]
fn 对照_已检测时同样的输入会写入() {
    let mut caps = caps_undetected();
    caps.detected = true;

    let cfg = Config {
        enabled: true,
        ..Config::default()
    };
    let d = decide_with(
        &sample(30.0, ChargingStatus::Charging),
        &cfg,
        &State::default(),
        &caps,
    )
    .unwrap();

    assert!(
        d.should_write || d.release,
        "已检测时应当产生实际动作（should_write 或 release），否则对照组无效"
    );
}

/// **生产路径**必须由档案构造 Bounds，且兜底档案 ⇒ `detected: false`
///
/// # 关于 `Bounds::default()`
///
/// 它刻意是 `detected: true`（见 `decision.rs:164` 的文档）：语义是
/// "假定有一份通用档案"，供 `decide()` / `evaluate()` 这类无档案参数的
/// 便捷 API 与测试使用。若它填 `false`，那个 API 就永远零写入。
///
/// 真正要守的是**生产路径**：`engine` 一律走 `Bounds::from_caps(档案)`，
/// 而 `Capabilities::default()`（= 未检测时的兜底档案）必须产出
/// `detected: false`，从而让 R2 门禁拦住所有兜底值。
///
/// 这条断言比"`Bounds::default` 必须是 false"更有意义 —— 后者会
/// 与既有设计冲突，而前者才是"忘传档案的新代码不会用兜底值控真硬件"的保证。
#[test]
fn 兜底档案构造的边界必须是未检测() {
    // 未检测时的兜底档案
    let fallback = Capabilities::default();
    assert!(
        !fallback.detected,
        "Capabilities::default() 必须 detected:false（它是未检测时的兜底档案）"
    );

    let b = hyperpower_core::Bounds::from_caps(&fallback);
    assert!(
        !b.detected,
        "Bounds::from_caps(兜底档案) 必须 detected:false —— 否则兜底值会参与控制"
    );

    // 且用兜底档案走完整决策链，必须零写入
    let cfg = Config {
        enabled: true,
        ..Config::default()
    };
    let d = decide_with(
        &sample(30.0, ChargingStatus::Charging),
        &cfg,
        &State::default(),
        &fallback,
    )
    .unwrap();
    assert_zero_control(&d, "Capabilities::default() 兜底档案");
}

/// 便捷 API `decide()` 在生产代码里**不应被调用**
///
/// 它用 `Bounds::default()`（`detected: true`），等于"假定已检测"。
/// 生产路径必须走 `decide_with` + 真实档案。这条测试以**源码扫描**的
/// 方式固化该约定，防止未来有人图省事直接 `decide()`。
#[test]
fn 便捷_decide_不得出现在生产代码里() {
    let cli_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("core crate 应有父目录")
        .join("hyperpower-cli/src");

    let mut offenders = Vec::new();
    for entry in std::fs::read_dir(&cli_src).expect("读取 cli 源码目录") {
        let path = entry.expect("目录项").path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        // 只看 `#[cfg(test)]` 之前的代码
        let prod = text.split("#[cfg(test)]").next().unwrap_or("");
        for (i, line) in prod.lines().enumerate() {
            let l = line.trim();
            if l.starts_with("//") {
                continue;
            }
            // 匹配 `decide(` 但不匹配 `decide_with(` / `evaluate_with(`
            if l.contains("hyperpower_core::decide(") || l.contains(" core::decide(") {
                offenders.push(format!("{}:{}", path.display(), i + 1));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "生产代码不得调用便捷 decide()（它假定已检测）；请改用 decide_with + 真实档案：{offenders:?}"
    );
}
