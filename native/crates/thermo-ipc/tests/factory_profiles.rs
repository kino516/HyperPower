//! 出厂档案（`module/src/profiles/*.json`）的加载回归测试
//!
//! # 为什么必须有这个测试
//!
//! 出厂档案是**随模块分发、设备端直接加载**的文件，一旦结构与代码不一致，
//! 设备端会**静默跳过**该档案（`load_factory_profiles` 只记 warning 不报错），
//! 表现为"莫名其妙用上了保守默认值"——线上极难排查。
//!
//! 本项目历史上就踩过这个坑：档案文件是**旧结构**而代码已改新结构，
//! 编译与测试全绿，但设备端加载失败。本测试在 **CI 阶段**就用真实文件把这条路径走一遍。
//!
//! # 当前状态：目录为空（这是**预期**的）
//!
//! 本项目**不预置任何机型档案** —— 各机型的档位、电流上限、最大功率档位、
//! 硬件保护温度都可能不同，一律以本机 `device probe` 的实测结果为准
//! （用户明确要求"不要写死档位等信息"）。
//!
//! 因此本测试的角色是**守护回流通道**：工厂档案目录是"用户测完导出 → 维护者核对 →
//! 放进来"的落点，一旦有文件进来，下面这些断言立即生效。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::{Path, PathBuf};

use thermo_ipc::device::{DeviceProfile, PROFILE_KIND, PROFILE_SCHEMA};

/// 出厂档案目录（`native/crates/thermo-ipc` → 仓库根 → `module/src/profiles`）
fn profiles_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
        .join("module")
        .join("src")
        .join("profiles")
}

/// 收集目录下全部 `.json`
fn profile_files() -> Vec<PathBuf> {
    let dir = profiles_dir();
    let mut out: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("出厂档案目录不存在（{}）：{e}", dir.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
        .collect();
    out.sort();
    out
}

#[test]
fn 出厂档案目录存在() {
    // ⚠️ 目录**允许为空**：出厂档案目录是"用户回流数据的落点"。
    //    本项目当前**不含任何机型档案** —— 一切能力数据（档位表、电流上限、
    //    最大功率、硬件保护温度）都以本机 `device probe` 的实测结果为准，
    //    见 docs/07-契约-检测与设备档案.md §1.0。
    //
    //    空目录下，下面几个"遍历档案"的测试会自然通过（循环体不执行）；
    //    一旦有档案回流进来，它们会立刻开始守护。
    assert!(
        profiles_dir().is_dir(),
        "出厂档案目录不存在：{}",
        profiles_dir().display()
    );
}

#[test]
fn 每个出厂档案都能按新结构加载并通过校验() {
    for path in profile_files() {
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("读取失败 {}：{e}", path.display()));

        // 这一步就是设备端的加载路径：结构不匹配会在这里失败
        let profile: DeviceProfile = serde_json::from_str(&text).unwrap_or_else(|e| {
            panic!(
                "出厂档案 {} 结构不符合 DeviceProfile（设备端会静默跳过它）：{e}",
                path.display()
            )
        });

        profile
            .validate()
            .unwrap_or_else(|e| panic!("出厂档案 {} 校验失败：{e}", path.display()));

        assert_eq!(
            profile.schema,
            PROFILE_SCHEMA,
            "{} 的 schema 应为 {PROFILE_SCHEMA}",
            path.display()
        );
        assert_eq!(
            profile.kind,
            PROFILE_KIND,
            "{} 的 kind 应为 {PROFILE_KIND}",
            path.display()
        );
    }
}

#[test]
fn 出厂档案必须具备机型标识() {
    // `validate()` 要求 device / model 至少有一个非空；若为空则该文件永远加载不了，
    // 只会被静默跳过 —— 这正是本项目历史上踩过的坑（旧结构档案 + 新代码）。
    //
    // ⚠️ 由此可得一条设计结论：「未匹配机型的兜底档案」**不能**作为出厂档案文件存在，
    // 它由代码生成（`profile_io::generic_profile()`），因此本目录里不得再放 generic.json。
    for path in profile_files() {
        let text = fs::read_to_string(&path).unwrap();
        let p: DeviceProfile = serde_json::from_str(&text).unwrap();
        let name = path.file_name().unwrap_or_default().to_string_lossy();

        assert!(
            !p.identity.device.is_empty() || !p.identity.model.is_empty(),
            "{name} 缺少机型标识 ⇒ 设备端永远加载不了它（兜底档案应由代码生成，而非放进本目录）"
        );
    }
}

#[test]
fn 出厂档案不含机型实测能力数据() {
    // 设计约定（docs/09-设计-附加功能架构.md）：出厂档案只保留机型识别信息与
    // 节点路径**候选**；档位表 / 电流上限 / 最大功率 / 硬件保护温度一律以本机
    // `device probe` 的实测结果为准，不写死在出厂档案里。
    for path in profile_files() {
        let text = fs::read_to_string(&path).unwrap();
        let p: DeviceProfile = serde_json::from_str(&text).unwrap();
        let name = path.file_name().unwrap_or_default().to_string_lossy();

        assert!(
            p.capabilities.gear.table.is_empty(),
            "{name} 不该预置档位表（各机型不同，须由本机检测得出）"
        );
        assert!(
            p.capabilities.power.max_w.is_none(),
            "{name} 不该预置最大功率（须充电中实测）"
        );
        assert!(
            p.capabilities.power.max_power_gear.is_none(),
            "{name} 不该预置最大功率档位"
        );
        assert!(
            p.capabilities.soc_limit.is_none(),
            "{name} 不该预置 SOC 上限能力（须本机探测）"
        );
        assert!(!p.verified, "{name} 未经人工核对，verified 必须为 false");
    }
}

#[test]
fn generic兜底档案不出现在出厂档案目录() {
    // generic 的 identity 为空 ⇒ 通不过 validate() ⇒ 永远加载不了。
    // 兜底由 `profile_io::generic_profile()` 在代码里生成，本目录不应有它。
    assert!(
        !profiles_dir().join("generic.json").exists(),
        "generic.json 不属于出厂档案目录（它永远加载不了，兜底应由代码生成）"
    );
}

#[test]
fn 每个出厂档案的身份可自我匹配() {
    // 出厂档案的**唯一价值**是机型识别 + 节点路径候选，因此其 identity
    // 必须能被匹配规则认出（device / model / platform 任一命中）。
    // 这条规则对"将来回流进来的任何机型"都成立，不针对具体型号。
    for path in profile_files() {
        let text = fs::read_to_string(&path).unwrap();
        let p: DeviceProfile = serde_json::from_str(&text).unwrap();
        let name = path.file_name().unwrap_or_default().to_string_lossy();

        assert!(
            p.identity.matches(&p.identity),
            "{name} 的身份无法自我匹配 ⇒ 该档案永远不会被选中"
        );

        // 反向：全空的身份不得匹配任何东西（避免"谁都匹配"的档案污染匹配结果）
        let empty = thermo_ipc::device::Identity::default();
        assert!(
            !empty.matches(&empty),
            "空身份不得互相匹配（否则会抢走本该匹配给具体机型的档案）"
        );
        assert!(!empty.matches(&p.identity), "空身份不得匹配到任何真实机型");
    }
}

#[test]
fn 出厂档案的档位范围是保守的() {
    // 未实测前不应把可写范围放宽到超出通用安全区间
    for path in profile_files() {
        let text = fs::read_to_string(&path).unwrap();
        let p: DeviceProfile = serde_json::from_str(&text).unwrap();
        let g = &p.capabilities.gear;
        let name = path.file_name().unwrap_or_default().to_string_lossy();

        assert!(g.max >= g.min, "{name} 档位范围非法");
        assert!(g.open <= g.max, "{name} 最松档位超出范围");
        assert!(g.safe <= g.max, "{name} 最紧档位超出范围");
        assert!(p.capabilities.current.max_ua > 0, "{name} 电流上限必须为正");
        assert!(
            p.capabilities.thermal.abs_max_c <= 45.0,
            "{name} 模块硬上限不得超过 45℃"
        );
    }
}
