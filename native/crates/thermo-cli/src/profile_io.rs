//! 设备档案的读写与合并
//!
//! 契约见 `docs/07-契约-检测与设备档案.md`。
//!
//! # 三层数据模型
//!
//! | 层 | 位置 | 优先级 |
//! |---|---|---|
//! | 本机档案 | `/data/adb/hyperpower/device.json` | **最高** |
//! | 出厂档案 | `module/profiles/*.json` | 中 |
//! | 通用默认 | 代码内置 | 最低 |
//!
//! # 只读 vs 实时
//!
//! 本模块**只处理持久化的档案**。实时数据（温度 / 电流 / 电压 / 电量 /
//! 充电状态）**永不经过这里** —— 它们由采样层直接从节点读入内存。

// ⚠️ 检测功能尚未全部接线（M2 阶段），档案读写的调用方在 worker / ctl 中。
#![allow(dead_code)]

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use thermo_ipc::device::{
    Capabilities, DeviceProfile, EffectiveProfile, GearRow, Identity, NodePaths, ProfileSource,
};

use crate::paths;

// ============================================================
//  读取
// ============================================================

/// 读取任意档案文件
pub fn load_file(path: &Path) -> Result<DeviceProfile> {
    let text =
        fs::read_to_string(path).with_context(|| format!("读取档案失败: {}", path.display()))?;
    let profile: DeviceProfile =
        serde_json::from_str(&text).with_context(|| format!("解析档案失败: {}", path.display()))?;
    profile
        .validate()
        .map_err(|e| anyhow::anyhow!("档案非法（{}）: {e}", path.display()))?;
    Ok(profile)
}

/// 读取本机档案（不存在则返回 `None`）
pub fn load_local() -> Option<DeviceProfile> {
    let path = paths::device_json();
    if !path.exists() {
        return None;
    }
    match load_file(&path) {
        Ok(p) => Some(p),
        Err(e) => {
            log::warn!("本机档案加载失败，将回退: {e:#}");
            None
        }
    }
}

/// 读取本机身份（用于匹配出厂档案）
///
/// 只读系统属性，**不写任何节点**。
pub fn read_identity() -> Identity {
    let get = |key: &str| -> String {
        std::process::Command::new("getprop")
            .arg(key)
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };

    let api: u32 = get("ro.build.version.sdk").parse().unwrap_or(0);

    Identity {
        device: get("ro.product.device"),
        model: get("ro.product.model"),
        platform: get("ro.board.platform"),
        marketname: get("ro.product.marketname"),
        android_api: api,
        android_release: get("ro.build.version.release"),
        kernel: read_kernel_version(),
        page_size: read_page_size(),
    }
}

/// 读内核版本（只读）
fn read_kernel_version() -> String {
    fs::read_to_string("/proc/version")
        .ok()
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// 读内存页大小（只读）
fn read_page_size() -> u32 {
    // getconf 不一定存在，直接读辅助向量更可靠；此处用 getconf 并回退
    std::process::Command::new("getconf")
        .arg("PAGESIZE")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(4096)
}

// ============================================================
//  出厂档案匹配
// ============================================================

/// 扫描出厂档案目录，返回所有可加载的档案
pub fn load_factory_profiles() -> Vec<DeviceProfile> {
    let dir = paths::profiles_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        log::debug!("出厂档案目录不存在: {}", dir.display());
        return Vec::new();
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match load_file(&path) {
            Ok(p) => out.push(p),
            Err(e) => log::warn!("跳过出厂档案 {}: {e:#}", path.display()),
        }
    }
    out
}

/// 按身份匹配出厂档案
///
/// 匹配依据：`device` / `model` / `platform` 任一命中（契约 §1.1 规则 3）。
/// `generic` 档案（无匹配字段）不会命中，仅作为最终兜底。
pub fn match_factory(identity: &Identity) -> Option<DeviceProfile> {
    let mut best: Option<(u8, DeviceProfile)> = None;

    for p in load_factory_profiles() {
        // generic 档案的 match 字段全为空 → 不参与匹配
        let score = match_score(identity, &p.identity);
        if score == 0 {
            continue;
        }
        // 分值高者优先（device 命中权重最高）
        if best.as_ref().is_none_or(|(s, _)| score > *s) {
            best = Some((score, p));
        }
    }

    best.map(|(_, p)| p)
}

/// 匹配打分（0 = 不匹配）
fn match_score(a: &Identity, b: &Identity) -> u8 {
    let eq = |x: &str, y: &str| !x.is_empty() && !y.is_empty() && x == y;

    if eq(&a.device, &b.device) {
        return 3; // 设备代号最精确
    }
    if eq(&a.model, &b.model) {
        return 2; // 机型编号次之
    }
    if eq(&a.platform, &b.platform) {
        return 1; // 平台最宽泛
    }
    0
}

// ============================================================
//  生效档案（三层合并）
// ============================================================

/// 取得**生效档案**：本机 → 出厂 → 通用默认
///
/// 这是控制逻辑的唯一入口 —— 所有边界都从这里来。
pub fn effective_profile() -> EffectiveProfile {
    // ① 本机档案（优先级最高）
    if let Some(local) = load_local() {
        log::debug!("使用本机档案（{}）", local.identity.display());
        return EffectiveProfile {
            profile: local,
            source: ProfileSource::Local,
        };
    }

    // ② 出厂档案（按机型匹配）
    let identity = read_identity();
    if let Some(factory) = match_factory(&identity) {
        log::debug!("使用出厂档案（{}）", factory.identity.display());
        return EffectiveProfile {
            profile: factory,
            source: ProfileSource::Factory,
        };
    }

    // ③ 通用默认
    log::warn!("未匹配到任何档案，使用通用保守默认");
    EffectiveProfile {
        profile: generic_profile(identity),
        source: ProfileSource::Generic,
    }
}

/// 通用保守默认档案
pub fn generic_profile(identity: Identity) -> DeviceProfile {
    let mut p =
        DeviceProfile::new_skeleton(format!("thermo {}", env!("CARGO_PKG_VERSION")), now_unix());
    p.identity = identity;
    p.verified = false;
    p.probe_meta.method = "generic_default".to_string();
    p.probe_meta
        .notes
        .push("未匹配到出厂档案，使用通用保守默认值".to_string());
    p.probe_meta
        .notes
        .push("建议执行 `device probe` 生成本机档案".to_string());
    p
}

// ============================================================
//  写入
// ============================================================

/// 原子写入档案（先写临时文件再 rename）
///
/// ⚠️ 原子写是硬要求：模块可能在任何时刻被 kill，
/// 半写的 JSON 会导致下次加载直接失败。
pub fn save(path: &Path, profile: &DeviceProfile) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建目录失败: {}", parent.display()))?;
    }

    let text = serde_json::to_string_pretty(profile).context("序列化档案失败")?;

    // 同目录临时文件（保证 rename 不跨设备）
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text.as_bytes())
        .with_context(|| format!("写入临时文件失败: {}", tmp.display()))?;

    // fsync 后再 rename，避免掉电导致空文件
    if let Ok(f) = fs::File::open(&tmp) {
        f.sync_all().ok();
    }

    fs::rename(&tmp, path).with_context(|| format!("替换档案失败: {}", path.display()))?;
    Ok(())
}

/// 保存为本机档案
pub fn save_local(profile: &DeviceProfile) -> Result<()> {
    save(&paths::device_json(), profile)
}

/// 备份现有本机档案（导入前调用）
///
/// 返回备份路径；无现有档案时返回 `None`。
pub fn backup_local() -> Result<Option<std::path::PathBuf>> {
    let src = paths::device_json();
    if !src.exists() {
        return Ok(None);
    }
    let bak = src.with_extension(format!("json.bak.{}", now_unix()));
    fs::copy(&src, &bak).with_context(|| format!("备份失败: {}", bak.display()))?;
    Ok(Some(bak))
}

/// 删除本机档案（回退出厂档案）
pub fn remove_local() -> Result<bool> {
    let path = paths::device_json();
    if !path.exists() {
        return Ok(false);
    }
    fs::remove_file(&path).with_context(|| format!("删除失败: {}", path.display()))?;
    Ok(true)
}

// ============================================================
//  辅助
// ============================================================

/// 当前 Unix 时间戳（秒）
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 当前日期字符串（`YYYYMMDD`，用于导出文件名）
pub fn today_compact() -> String {
    // 不引入 chrono：从 /proc/uptime 与系统时间算不了日历，改用 `date` 命令
    std::process::Command::new("date")
        .arg("+%Y%m%d")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("{}", now_unix()))
}

/// 把 `thermo-ipc` 的能力边界转换为 `thermo-core` 的边界
///
/// 两个 crate 各有独立定义（IPC 需要 serde，core 需保持纯净），
/// 此处做显式转换 —— 字段不一致时编译器会报错，这是有意的守护。
pub fn to_core_caps(caps: &Capabilities) -> thermo_core::Capabilities {
    thermo_core::Capabilities {
        gear: thermo_core::profile::GearCapability {
            min: caps.gear.min,
            max: caps.gear.max,
            open: caps.gear.open,
            safe: caps.gear.safe,
            table: caps
                .gear
                .table
                .iter()
                .map(|r| thermo_core::profile::GearRow {
                    gear: r.gear,
                    current_limit_ua: r.current_limit_ua,
                })
                .collect(),
        },
        current: thermo_core::profile::CurrentCapability {
            min_ua: caps.current.min_ua,
            max_ua: caps.current.max_ua,
            safe_ua: caps.current.safe_ua,
        },
        power: thermo_core::profile::PowerCapability {
            max_w: caps.power.max_w,
            max_power_gear: caps.power.max_power_gear,
            protocol: caps.power.protocol.clone(),
        },
        thermal: thermo_core::profile::ThermalCapability {
            hardware_max_c: caps.thermal.hardware_max_c,
            abs_max_c: caps.thermal.abs_max_c,
        },
        soc_limit: caps
            .soc_limit
            .map(|s| thermo_core::profile::SocLimitCapability {
                supported: s.supported,
                min: s.min,
                max: s.max,
            }),
        verified: caps.verified,
        // ⚠️ 必须传递 —— 这是"要不要控制"的总闸门
        //
        // 漏掉它会让**已跑过检测的本机**也被判为"未检测"而拒绝全部控制。
        detected: caps.detected,
    }
}

/// 把 `thermo-core` 的边界转换回 `thermo-ipc`
///
/// # ⚠️ 来源标记（`sources`）会丢失
///
/// `thermo-core` 是**纯决策内核**，只关心数值边界，不携带"这个值从哪来"的
/// 元信息（`ValueSource` 是档案层概念）。因此本转换无法还原来源，
/// 一律填 [`Sources::default()`]（全部 `Unknown`）。
///
/// 调用方若需要保留来源，应在转换后自行覆盖对应字段。
pub fn from_core_caps(caps: &thermo_core::Capabilities) -> Capabilities {
    Capabilities {
        sources: thermo_ipc::device::Sources::default(),
        // ⚠️ 不能写死 `false` —— 那会让反向转换后的档案变成"未检测"，
        //    进而拒绝一切控制。这里如实传递。
        detected: caps.detected,
        gear: thermo_ipc::device::GearCapability {
            min: caps.gear.min,
            max: caps.gear.max,
            open: caps.gear.open,
            safe: caps.gear.safe,
            table: caps
                .gear
                .table
                .iter()
                .map(|r| GearRow {
                    gear: r.gear,
                    current_limit_ua: r.current_limit_ua,
                })
                .collect(),
        },
        current: thermo_ipc::device::CurrentCapability {
            min_ua: caps.current.min_ua,
            max_ua: caps.current.max_ua,
            safe_ua: caps.current.safe_ua,
        },
        power: thermo_ipc::device::PowerCapability {
            max_w: caps.power.max_w,
            max_power_gear: caps.power.max_power_gear,
            protocol: caps.power.protocol.clone(),
        },
        thermal: thermo_ipc::device::ThermalCapability {
            hardware_max_c: caps.thermal.hardware_max_c,
            abs_max_c: caps.thermal.abs_max_c,
        },
        soc_limit: caps
            .soc_limit
            .map(|s| thermo_ipc::device::SocLimitCapability {
                supported: s.supported,
                min: s.min,
                max: s.max,
            }),
        verified: caps.verified,
    }
}

/// 由**探测得到的**节点路径构造 `NodePaths`
///
/// ⚠️ 未探测到的节点写空串（表示"本机不支持"），而不是猜测一个路径。
pub fn node_paths(nodes: &crate::sysfs::Nodes) -> NodePaths {
    let s = |n: &Option<crate::sysfs::Node>| {
        n.as_ref()
            .map(crate::sysfs::Node::path_str)
            .unwrap_or_default()
    };
    NodePaths {
        ctrl: s(&nodes.ctrl),
        curr: s(&nodes.curr),
        curr2: nodes.curr2.as_ref().map(crate::sysfs::Node::path_str),
        remove: s(&nodes.remove),
        soc_ctrl: s(&nodes.soc_ctrl),
        soc_read: s(&nodes.soc_read),
        battery_temp: s(&nodes.battery_temp),
        battery_current: s(&nodes.battery_current),
        battery_voltage: s(&nodes.battery_voltage),
        battery_status: s(&nodes.battery_status),
        battery_capacity: s(&nodes.battery_capacity),
    }
}
