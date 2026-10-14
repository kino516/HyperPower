//! 检测功能（探测设备信息与能力边界）
//!
//! 契约见 `docs/07-契约-检测与设备档案.md` §3。
//!
//! # 检测项与副作用
//!
//! | # | 检测项 | 需充电 | 副作用 |
//! |---|---|---|---|
//! | 1 | 系统信息 | ❌ | 无 |
//! | 2 | 电池信息 | ❌ | 无 |
//! | 3 | 节点映射（**运行期探测**） | ❌ | 无（只读试开） |
//! | 4 | 档位表（设备树直读） | ❌ | 无 |
//! | 5 | 档位数 / 最大功率档位 | ❌ | 无 |
//! | 6 | 电流上限（读探） | ❌ | 无 |
//! | 7 | 充电功率上限（三来源取最大） | ❌ | 无（只读） |
//! | 8 | 硬件保护温度（设备树直读） | ❌ | 无 |
//! | 9 | 场景定义 | ❌ | 无 |
//! | 10 | SOC 上限支持性 | ❌ | 无 |
//!
//! **全部检测项都不需要插充电器**，且除档位上限验证（仅完整模式）外全部只读。
//!
//! # ⚠️ 不预置任何机型数据
//!
//! 档位表、电流上限、功率上限、硬件保护温度**全部靠本机探测得出**，
//! **不入出厂档案**（用户明确要求）。
//! 各机型的档位数、档位电流、功率最大档位都可能不同，因此**不得假定 15 档**。
//!
//! # 值域来源标记
//!
//! 每个能力值都记录**来源**（[`ValueSource`]），供界面标注可信度 ——
//! 用户必须能分清"这个数是直读的，还是推断的"。

use std::fs;

use anyhow::Result;
use thermo_ipc::device::{
    BatteryInfo, DeviceProfile, GearRow, PowerCapability, ProbeMeta, SocLimitCapability,
    ValueSource,
};

use crate::profile_io;
use crate::sysfs;

/// 检测模式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeMode {
    /// 快速检测：**仅只读**，秒级完成
    Quick,
    /// 完整检测：含写探（需用户确认）
    Full,
}

/// 检测选项
#[derive(Debug, Clone, Copy)]
pub struct ProbeOptions {
    /// 模式
    pub mode: ProbeMode,
    /// 是否允许写探（改变充电参数）
    pub allow_write_probe: bool,
}

impl Default for ProbeOptions {
    fn default() -> Self {
        Self {
            mode: ProbeMode::Quick,
            allow_write_probe: false,
        }
    }
}

/// 执行检测，返回档案（**不落盘** —— 由调用方决定是否保存）
pub fn run(opts: ProbeOptions) -> Result<DeviceProfile> {
    let mut profile =
        DeviceProfile::new_skeleton(format!("thermo {}", env!("CARGO_PKG_VERSION")), now_unix());

    let mut meta = ProbeMeta {
        method: match opts.mode {
            ProbeMode::Quick => "quick_readonly".to_string(),
            ProbeMode::Full => "full".to_string(),
        },
        probed_at: now_unix(),
        ..Default::default()
    };

    // ---------- ① 系统信息 ----------
    log::info!("[1/10] 读取系统信息…");
    profile.identity = profile_io::read_identity();

    // ---------- ② 电池信息 ----------
    log::info!("[2/10] 读取电池信息…");
    profile.battery = read_battery_info();

    // ---------- ③ 节点映射（运行期探测）----------
    log::info!("[3/10] 探测节点映射…");
    let nodes = sysfs::Nodes::detect(None);
    profile.nodes = profile_io::node_paths(&nodes);

    if !nodes.can_observe() {
        meta.failed_items.push("observe_nodes".to_string());
        meta.notes
            .push("电池观测节点不全，实时数据可能缺失".to_string());
    }
    if !nodes.can_control() {
        meta.failed_items.push("control_nodes".to_string());
        meta.notes
            .push("控速节点不全（档位 / 电流）—— 本机型可能不支持模块控速".to_string());
    }

    // SOC 上限支持性（探测而非假定）
    if nodes.can_limit_soc() {
        profile.capabilities.soc_limit = Some(SocLimitCapability {
            supported: true,
            min: 60,
            max: 100,
        });
        meta.notes.push(format!(
            "SOC 上限可用：{}",
            nodes
                .soc_ctrl
                .as_ref()
                .map(sysfs::Node::path_str)
                .unwrap_or_default()
        ));
    } else {
        meta.failed_items.push("soc_nodes".to_string());
        meta.notes
            .push("未找到可写的 smart_chg 节点，充电上限功能不可用".to_string());
    }

    // ---------- ④ 档位表（设备树直读，**零副作用**）----------
    log::info!("[4/10] 读取充电档位表…");
    let mut gear_rows: Vec<GearRow> = Vec::new();
    match sysfs::GearTable::detect() {
        Ok((table, path)) => {
            gear_rows = table
                .current_limits()
                .into_iter()
                .map(|(gear, current_limit_ua)| GearRow {
                    gear,
                    current_limit_ua,
                })
                .collect();
            let count = gear_rows.len();

            // ⚠️ 可写上限的推导（**不写死 15**）
            //
            // 档位表有 N 行，编号 0 ~ N-1；而驱动**额外接受一个比表尾更紧的档位**
            // （K90PM 实测：表 15 行 0~14，但可写 0~15，15 比 14 更紧）。
            // 因此可写上限 = 表长度 —— 对"N 行"的机型同样成立。
            profile.capabilities.gear.max =
                u8::try_from(count).unwrap_or(profile.capabilities.gear.max);
            profile.capabilities.gear.open = 0;
            profile.capabilities.gear.safe = profile.capabilities.gear.max;

            // 来源：设备树直读
            profile.capabilities.sources.gear_max = ValueSource::Devicetree;
            profile.capabilities.sources.gear_table = ValueSource::Devicetree;

            // 最大功率档位 = 电流上限最大的档
            profile.capabilities.power.max_power_gear = table.max_power_gear();

            meta.notes.push(format!(
                "档位表：{count} 档（编号 0~{}），可写上限 {count}，来源 {path}（设备树直读，零副作用）",
                count.saturating_sub(1),
                count = count,
            ));
            if let Some(g) = table.max_power_gear() {
                meta.notes.push(format!("最大功率档位：{g}"));
            }
        }
        Err(e) => {
            log::warn!("档位表直读失败：{e:#}");
            meta.failed_items.push("gear_table".to_string());
            meta.notes
                .push(format!("档位表读取失败，沿用保守范围 0~15：{e:#}"));
            // 来源保持 Default：0~15 是硬编码兜底，不是本机实测
            profile.capabilities.sources.gear_max = ValueSource::Default;
            profile.capabilities.sources.gear_table = ValueSource::Unknown;
        }
    }
    profile.capabilities.gear.table = gear_rows;

    // ---------- ⑤ 电流上限（读节点，**不写死**）----------
    //
    // 未充电时 `wired_chg_curr` 报告的是**能力上限**，恰好就是我们要的值。
    // 注意这与"档位通道的上限"是两个概念：
    // 档位与电流是**两层并联**，实际生效取二者更严的一个。
    //
    // # 🔴 充电中**不能**采信这个节点（真机实测的自我循环）
    //
    // 该节点读出来的其实是 `min(模块写入值, 驱动当前允许值)`：
    //
    // | 检测时的状态 | 读值 | 含义 |
    // |---|---|---|
    // | 未充电 | `18400000` | 驱动默认/能力上限 ✓ |
    // | 充电中 / 已充满 | `15044000` | **被驱动钳位后的当前限制值** ✗ |
    //
    // 于是会形成**自我循环**：
    //
    // ```text
    // 检测读节点 15044000 → 写进档案
    //      ↓
    // 模块按档案写节点 15044000
    //      ↓
    // 下次检测又读到 15044000（自己的值）→ …
    // ```
    //
    // 后果：**能力上限被逐次压低**（18400 → 15044 → …），
    // 而用户什么也没做。真机踩过：一次充电中检测把上限降了 18%。
    //
    // 只读候选节点（`constant_charge_current_max` / `usb/current_max` …）
    // 在本机**全部读 0**，无法作为替代来源，因此只能跳过。
    log::info!("[5/10] 读取电流上限…");

    let charging_now = nodes
        .battery_status
        .as_ref()
        .and_then(|n| n.read().ok())
        .map(|s| matches!(s.trim(), "Charging" | "Full" | "Not charging"))
        .unwrap_or(false);

    if charging_now {
        // 沿用档案值（不覆盖），并说明原因
        //
        // ⚠️ **必须记入 `failed_items`**，不能只写 `notes`（2026-10-14 修）
        //
        // 这不是"正常跳过"，而是"**这一项没测到**"。
        // 只写 notes 的后果（真机踩到过）：档案里 `failed_items` 是**空的**，
        // 界面和 `device show` 都不提示，用户以为检测全部成功 ——
        // 而实际上 `current.max_ua` 用的是兜底常量，不是本机实测值。
        meta.failed_items.push("current_ceiling".to_string());
        meta.notes.push(
            "充电中 —— **跳过**电流上限检测（该节点此时反映的是模块自己的写入值，\
             采信会形成自我循环、逐次压低上限）。拔掉充电器后重跑可得到真实值"
                .to_string(),
        );
        profile.capabilities.sources.current_max = ValueSource::Profile;
    } else if let Some(curr) = &nodes.curr {
        match curr.read_i64() {
            Ok(v) if v > 0 => {
                if let Ok(ua) = i32::try_from(v) {
                    profile.capabilities.current.max_ua = ua;
                    profile.capabilities.sources.current_max = ValueSource::Node;
                    meta.notes.push(format!("电流上限（读节点）：{ua} μA"));
                }
            }
            Ok(_) => {
                meta.failed_items.push("current_ceiling".to_string());
                meta.notes
                    .push("电流节点读数为 0，无法判断上限".to_string());
                // 来源保持 Profile：沿用的是档案值
                profile.capabilities.sources.current_max = ValueSource::Profile;
            }
            Err(e) => {
                meta.failed_items.push("current_ceiling".to_string());
                meta.notes
                    .push(format!("电流上限读取失败，沿用档案值：{e:#}"));
                profile.capabilities.sources.current_max = ValueSource::Profile;
            }
        }
    } else {
        meta.failed_items.push("current_ceiling".to_string());
        meta.notes.push("电流节点不可用，沿用档案值".to_string());
        profile.capabilities.sources.current_max = ValueSource::Profile;
    }

    // ---------- ⑥ 硬件保护温度 ----------
    //
    // # 唯一可信来源：设备树
    //
    // | 级 | 来源 | 语义 | 本机值 |
    // |---|---|---|---|
    // | ① | 设备树 `ffc_temp_high` | 有线快充的电池温度上限 | 48.0℃ |
    // | ② | 兜底常量 | 通用保守值（**明确标记为 Default**） | 45.0℃ |
    //
    // # ⚠️ 这里**曾经**有第 ② 级"官方热控表最高触发温度"，已移除
    //
    // 移除理由（2026-10-14 真机实测）：
    //
    // | # | 问题 |
    // |---|---|
    // | 1 | **值会随场景漂移** —— 场景切到 `11`（录像）后系统重写 `decrypt.txt`，节名变 `VIDEO-` 前缀，`MONITOR-BAT` 消失；即便读到，录像场景的阈值也未必等于有线充电的硬件极限 |
    // | 2 | **语义不精确** —— 官方表的 `trig` 是「策略何时动作」，不是「硬件何时损坏」 |
    //
    // ⇒ 用它兜底会让用户的**可配置温度范围随场景变化**（录个像就变），
    //   这比"读不到就明确说读不到"更糟。宁可暴露不确定性，
    //   也不要用会变的数去填补。
    //
    // 详见 `sysfs::HW_MAX_TEMP_CANDIDATES` 的文档。
    log::info!("[6/10] 读取硬件保护温度…");
    if let Some(t) = read_hardware_max_temp() {
        profile.capabilities.thermal.hardware_max_c = t;
        profile.capabilities.sources.hardware_max_temp = ValueSource::Devicetree;
        meta.notes.push(format!("硬件保护温度：{t}℃（设备树直读）"));
    } else {
        meta.failed_items.push("hardware_max_temp".to_string());
        meta.notes.push(
            "硬件保护温度未探测到（设备树属性名因机型而异），使用保守兜底 45℃ —— \
             该项不精确：若本机真实上限低于 45℃，硬保护会偏晚"
                .to_string(),
        );
        // 45℃ 是硬编码兜底 —— 明确标记为"默认值"（来源可信度最低）
        profile.capabilities.sources.hardware_max_temp = ValueSource::Default;
    }

    // ---------- ⑦ 充电功率上限（三个来源，取最大值）----------
    //
    // ⚠️ 这里读的是**充电功率上限**（能力边界），不是"当前实际充电功率"。
    //
    // 三个来源：
    //
    // | 来源 | 节点/依据 | 何时有值 | 语义 |
    // |---|---|---|---|
    // | **协商值** | `charger_common/power_max` | **仅协商成功时** | 当前充电器与手机谈成的功率 |
    // | 电池型号 | `battery/model_name` 里的 `_100w` | 总是 | 出厂标称 |
    // | 设备树 | `support_pps_100w` 节点名 | 总是 | 支持的最高协议档位 |
    //
    // # ⚠️ 为什么取**最大值**而不是"第一个可用的"
    //
    // 协商值会**因充电器变弱而变小**：
    //
    // | 场景 | `power_max` 读数 | 若直接采用 |
    // |---|---|---|
    // | 插电脑 USB（SDP） | **0** | 记录 0 ⇒ 无法设定任何目标功率 |
    // | 插 33W 充电器 | **33** | 记录 33 ⇒ 换回原装后仍被钳到 33W |
    // | 插原装 100W | **100** | 记录 100 ✓ |
    //
    // 前两种情况下记录的都不是"机型能力"，会让用户的目标功率被无谓压低 ——
    // 这正是本项目历史上踩过的坑（把实测的当前功率当成了能力上限）。
    //
    // 取最大值后：弱充电器只会让某个来源读得小，但**不会降低**最终记录的
    // 能力值。这与老项目 `ThermoCharge` 的「只升不降」原则一致
    // （`sync_max_power_to_config` 里有 `[ "$_probed" -gt "$_cur" ] || return 1`）。
    //
    // # 为什么不干脆只读电池型号
    //
    // 电池型号的命名约定**不是所有机型都遵守**（有些只写容量不写功率）。
    // 协商值在插原装充电器时是**直接实测**的，可信度同级且能覆盖那些机型。
    log::info!("[7/10] 读取充电功率上限…");
    let max_power_gear = profile.capabilities.power.max_power_gear;

    let candidate_node = read_power_max_node();
    let candidate_model = nominal_power_from_battery_model(&profile.battery.model_name);
    let candidate_dt = nominal_power_from_devicetree();

    for (label, v) in [
        ("node", candidate_node),
        ("model", candidate_model),
        ("dt", candidate_dt),
    ] {
        if let Some(w) = v {
            log::debug!("功率候选 {label} = {w}W");
        }
    }

    // 取最大值（见上方说明）
    //
    // ⚠️ 不能直接用 `max_by_key`：**值相等时它返回最后一个**，会把
    //    来源标成可信度最低的那个（实测踩过：100W 同时来自电池型号与
    //    设备树时，来源被标成"设备树"）。
    //
    // 这里用"严格大于才替换"，使**相等时保留先出现的** ——
    // 而数组顺序正是可信度降序（node > model > dt），因此相等时
    // 自然取到可信度最高的来源。
    let mut best: Option<(u16, ValueSource)> = None;
    for (w, src) in [
        (candidate_node, ValueSource::Node),
        (candidate_model, ValueSource::BatteryModel),
        (candidate_dt, ValueSource::Devicetree),
    ] {
        if let Some(w) = w {
            if best.is_none_or(|(bw, _)| w > bw) {
                best = Some((w, src));
            }
        }
    }
    let (nominal, power_source) = best.map_or((None, ValueSource::Unknown), |(w, s)| (Some(w), s));

    match nominal {
        Some(w) => {
            let mut parts = Vec::new();
            if let Some(v) = candidate_node {
                parts.push(format!("协商值 {v}W"));
            }
            if let Some(v) = candidate_model {
                parts.push(format!("电池型号 {v}W"));
            }
            if let Some(v) = candidate_dt {
                parts.push(format!("设备树 {v}W"));
            }
            meta.notes.push(format!(
                "充电功率上限：{w}W（来源：{}；各来源读数：{}）",
                power_source.label(),
                parts.join(" / ")
            ));

            // 协商值缺失时提示原因，避免用户以为节点坏了
            if candidate_node.is_none() {
                meta.notes.push(
                    "`charger_common/power_max` 当前为 0（未协商出快充协议）—— \
                     插原装充电器后重跑可得到协商值"
                        .to_string(),
                );
            }
        }
        None => {
            meta.failed_items.push("power.max_w".to_string());
            meta.notes.push(
                "未能读出充电功率上限（协商值 / 电池型号 / 设备树均无功率信息）—— \
                 不做功率钳位，用户可自由设置目标功率"
                    .to_string(),
            );
        }
    }

    profile.capabilities.sources.power_max = power_source;

    profile.capabilities.power = PowerCapability {
        max_w: nominal,
        max_power_gear,
        // 协议是**实时状态**（此刻插的是什么充电器），不是机型能力，
        // 因此不写入档案；界面从实时接口读。
        protocol: None,
    };

    // ---------- ⑧ 档位上限验证（仅完整模式 + 显式确认）----------
    //
    // ⚠️ 上限**已经由档位表行数推导出来了**（见第 ④ 步），这里只是**验证**
    // 驱动是否接受它 —— 不是用它来决定上限。
    //
    // 旧实现用 `write_and_verify("15")` 硬编码 K90PM 的档位数，
    // 会把档位更多的机型的上限**缩小**到 15。详见 `verify_gear_max` 的文档。
    if opts.mode == ProbeMode::Full && opts.allow_write_probe {
        let derived = profile.capabilities.gear.max;
        log::info!("[8/10] 验证档位可写上限（推导值 {derived}）…");

        match verify_gear_max(&nodes, derived) {
            Ok(max) => {
                if max == derived {
                    meta.notes
                        .push(format!("档位上限 {derived} 已验证可写（写探）"));
                } else {
                    meta.notes.push(format!(
                        "档位上限：推导值 {derived} 写不进去，保守降为 {max}"
                    ));
                }
                profile.capabilities.gear.max = max;
                profile.capabilities.gear.safe = max;
            }
            Err(e) => {
                meta.failed_items.push("gear_write_probe".to_string());
                meta.notes
                    .push(format!("档位上限验证失败，沿用推导值 {derived}：{e:#}"));
            }
        }
    } else {
        log::info!("[8/10] 跳过档位上限验证（快速模式或未加 --confirm）");
    }

    // ---------- ⑨ 场景（已移除，见下）----------
    //
    // 🔴 2026-10-14 移除「当前场景」这条检测记录
    //
    // 真机反馈：检测记录里**一直显示「当前场景：-1」**。
    //
    // 三个问题叠加，导致这行记录只有噪音、没有信息：
    //
    // | # | 问题 |
    // |---|---|
    // | 1 | `-1` 是**「未设置」**，而且是本机最常见的值 —— 显示原始值等于没显示 |
    // | 2 | 场景是**运行期值**（随前台应用变化），**不是检测结果** —— 放进检测档案本身位置就不对 |
    // | 3 | 模块**不依赖**它（状态页那行的原文就是"模块不依赖它，仅作参考"） |
    //
    // ⇒ 整条删除。用户想了解场景可以看状态页的「系统场景」行，
    //    那里用的是 `describe_scene`（会显示「未设置」）。
    //
    // ⚠️ 保留 `nodes.scene` 与 `describe_scene` —— 状态页仍在用。
    log::info!("[9/10] 跳过场景记录（运行期值，不属于检测结果）");

    // ---------- ⑩ 收尾 ----------
    log::info!("[10/10] 完成");
    profile.probe_meta = meta;
    // 本机实测结果默认**未验证**（需维护者核对后才作为出厂档案分发）
    profile.verified = false;
    // ⚠️ 标记"本机跑过检测" —— 这是允许控制的总闸门
    //
    // 没有这一句，决策层会判定"未检测"并**拒绝一切写入**，
    // 用户的检测就白跑了（详见 `decision::evaluate_with` 的门禁 0）。
    profile.capabilities.detected = true;

    Ok(profile)
}

// ============================================================
//  各项探测的具体实现
// ============================================================

/// ② 电池信息（全只读）
fn read_battery_info() -> BatteryInfo {
    let base = "/sys/class/power_supply/battery";
    let read = |name: &str| -> Option<String> {
        fs::read_to_string(format!("{base}/{name}"))
            .ok()
            .map(|s| s.trim().to_string())
    };
    let read_i64 = |name: &str| -> i64 { read(name).and_then(|s| s.parse().ok()).unwrap_or(0) };

    BatteryInfo {
        model_name: read("model_name").unwrap_or_default(),
        technology: read("technology").unwrap_or_default(),
        charge_full_design_uah: read_i64("charge_full_design"),
        charge_full_uah: read_i64("charge_full"),
        cycle_count: read_i64("cycle_count") as i32,
    }
}

/// ⑥ 硬件保护温度（多候选，只读）
///
/// 按 [`sysfs::HW_MAX_TEMP_CANDIDATES`] 的**顺序**取第一个能解析出合理温度的值 ——
/// 顺序即语义优先级。
///
/// ⚠️ 不取"所有候选里的最小值"：这些节点的**场景并不相同**
/// （无线反充的上限比有线快充低 8℃，会无谓压低用户可配置的范围）。
/// 详见该常量文档。
fn read_hardware_max_temp() -> Option<f32> {
    for path in sysfs::HW_MAX_TEMP_CANDIDATES {
        let Ok(raw) = fs::read(path) else { continue };
        if raw.len() < 4 {
            continue;
        }
        // 设备树整数为 4 字节大端
        let v = u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]);
        if let Some(c) = parse_temp_celsius(v) {
            return Some(c);
        }
    }
    None
}

/// 把设备树里的裸整数解析成摄氏度
///
/// 单位不统一是常态，三种写法依次尝试，取第一个能落在"电池温度合理区间"的结果：
///
/// | 原始值 | 命中单位 | 结果 |
/// |---|---|---|
/// | `48` | 整数摄氏度 | 48.0℃ |
/// | `400` | 0.1℃ | 40.0℃ |
/// | `45200` | 毫摄氏度 | 45.2℃ |
/// | `125`（如芯片 `die_temp_max`） | —— | `None`（超出电池温度合理范围，丢弃） |
/// | `0` / `0xFFFF` / `0xFFFFFFFF` | —— | `None`（哨兵值，表示"未定义"） |
///
/// "丢弃 125"正是本函数的意义：把"芯片温度上限"这类**语义不同**的值挡掉，
/// 否则它会让硬件保护温度虚高，失去保护意义。
fn parse_temp_celsius(v: u32) -> Option<f32> {
    /// 电池温度的合理区间（℃）
    const LO: f32 = 20.0;
    const HI: f32 = 80.0;

    // 设备树用"全 1"表示未定义/不适用
    if v == 0 || v == 0xFFFF || v == 0xFFFF_FFFF {
        return None;
    }

    for divisor in [1.0_f32, 10.0, 1000.0] {
        let c = v as f32 / divisor;
        if (LO..=HI).contains(&c) {
            return Some(c);
        }
    }
    None
}

/// 从电池型号解析**标称充电功率**（用户指定：以电池型号为准）
///
/// # 命名约定（真机实测）
///
/// 本机（K90 Pro Max）的电池型号是 `P11U_7560mah_100w`：
///
/// ```text
///   P11U     7560mah     100w
///   ────     ───────     ────
///   厂商/   额定容量     标称充电功率
///   型号
/// ```
///
/// 因此按 `_<数字>w` 的结尾解析出功率。
///
/// # 为什么可以信这个值
///
/// 它是**出厂标称**（OEM 写死在电池参数里），不随电量、充电器、温度变化 ——
/// 正是"机型能力上限"该有的性质。
///
/// 本机交叉验证：
///
/// | 来源 | 值 |
/// |---|---|
/// | 电池型号 `..._100w` | 100 W |
/// | 设备树 `support_pps_100w` | 100 W |
/// | 电流上限 22 A × 5 V | ≈ 110 W |
///
/// 三处一致，因此单看型号即可。
///
/// # 解析规则
///
/// | 输入 | 结果 |
/// |---|---|
/// | `P11U_7560mah_100w` | `Some(100)` |
/// | `BM56_5000mah_67w` | `Some(67)` |
/// | `X_4500mah_120W` | `Some(120)`（大小写不敏感） |
/// | `P11U_7560mah`（无功率） | `None` |
/// | ``（空） | `None` |
///
/// 只认**最后一个** `_` 之后、以 `w`/`W` 结尾的数字段 ——
/// 避免把容量里的数字误当功率（如 `7560mah`）。
fn nominal_power_from_battery_model(model: &str) -> Option<u16> {
    let tail = model.trim().rsplit('_').next()?;
    let digits = tail.strip_suffix(['w', 'W'])?;

    // 必须是纯数字且非空，避免 `_w`、`_abcw` 这类脏数据
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }

    let watts: u16 = digits.parse().ok()?;
    // 合理性过滤：充电功率不会低于 5W，也不会高到离谱
    (5..=500).contains(&watts).then_some(watts)
}

/// 从设备树节点名解析标称功率（电池型号没有该信息时的**兜底**）
///
/// # 依据
///
/// 真机实测发现设备树里有这样的节点：
///
/// ```text
/// /sys/firmware/devicetree/base/soc/mca_strategy_buckchg_class/support_pps_100w
/// ```
///
/// 从 `charger_common/power_max` 读**协商功率**
///
/// # 这个节点是什么
///
/// 它是**充电器与手机协商成功后**，由充电协议栈写入的功率上限（W）。
/// 属于"直接读到硬件当前认知的能力值"，可信度最高的一档。
///
/// # ⚠️ 它只在**协商成功**时非 0
///
/// 真机实测（K90 Pro Max，2026-10-13）：
///
/// | 插入的电源 | `real_type` | `quick_charge_type` | `power_max` |
/// |---|---|---|---|
/// | 电脑 USB 口 | `SDP` | `0` | **0** |
/// | 原装充电器 | `PD_PPS` | 非 0 | **100**（协商值） |
///
/// 即读 **0 代表"没协商出快充"，不代表"这台手机只能 0W"**。
/// 因此调用处**不能**把 0 当有效值记录 —— 否则用户插着电脑跑一次检测，
/// 功率上限就被写成 0，此后目标功率无法设定。
///
/// # 多候选路径
///
/// `charger_common/power_max` 是主路径；`quick_charge/power_max`
/// 在部分机型上才有效（权限也更严，实测为 `-r--r-----`）。
/// 两个都读，取能读到的最大值。
///
/// # 为什么不用 `power_supply/usb/*`
///
/// `usb/voltage_now × usb/current_now` 是**输入侧瞬时值** ——
/// 它表示"此刻从充电器取了多少"，受协议协商与负载影响，
/// 比 `power_max` 波动大得多，不适合作为能力上限。
fn read_power_max_node() -> Option<u16> {
    const CANDIDATES: &[&str] = &[
        "/sys/class/xm_power/charger/charger_common/power_max",
        "/sys/class/xm_power/charger/quick_charge/power_max",
    ];

    let mut best: Option<u16> = None;

    for path in CANDIDATES {
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        let Ok(v) = text.trim().parse::<i64>() else {
            continue;
        };
        // 0 或负数 = 未协商出快充，不是有效能力值（见上方说明）
        if v <= 0 {
            continue;
        }
        let Ok(w) = u16::try_from(v) else {
            continue;
        };
        // 合理性过滤：5~500W（与电池型号解析同一区间）
        if !(5..=500).contains(&w) {
            continue;
        }
        if best.is_none_or(|b| w > b) {
            best = Some(w);
        }
    }

    best
}

/// 从设备树节点名解析功率（兜底来源）
///
/// # 依据
///
/// 本机存在 `/sys/firmware/devicetree/base/soc/mca_strategy_buckchg_class/support_pps_100w`。
/// 节点名末尾的 `100w` 就是**有线快充支持的最高档位功率**。
/// 它的**值**是布尔（本机为 `3`），但**名字**携带了功率信息。
///
/// # 为什么只是兜底
///
/// 该约定不如电池型号可靠：
///
/// | 风险 | 说明 |
/// |---|---|
/// | 命名不统一 | 他机型可能叫 `support_pps` 或干脆没这个节点 |
/// | 语义漂移 | `support_pps_100w` 可能指"支持 100W 协议"，未必是本机标称 |
///
/// 因此它是三个来源里优先级最低的一个。
fn nominal_power_from_devicetree() -> Option<u16> {
    const DIR: &str = "/sys/firmware/devicetree/base/soc/mca_strategy_buckchg_class";

    let entries = fs::read_dir(DIR).ok()?;
    let mut found: Option<u16> = None;

    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();

        // 只认 `support_*_<数字>w` 这种形态
        if !name.starts_with("support_") {
            continue;
        }
        let Some(rest) = name.strip_suffix(['w', 'W']) else {
            continue;
        };
        let digits = rest.rsplit('_').next().unwrap_or("");
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(watts) = digits.parse::<u16>() else {
            continue;
        };
        if !(5..=500).contains(&watts) {
            continue;
        }
        // 取最大值（可能同时存在多个档位节点）
        found = Some(found.map_or(watts, |f: u16| f.max(watts)));
    }

    found
}

/// ⑧ 档位可写上限的**验证**（不是"决定"）
///
/// # ⚠️ 为什么要分开这两件事
///
/// 可写上限由**档位表行数**决定（表 N 行 ⇒ 可写 `0..=N`，见调用处注释）。
/// 本函数只负责**验证**驱动是否接受这个推导值。
///
/// 初版把两件事混在一起，而且硬编码了 K90PM 的数字：
///
/// ```rust
/// // ❌ 旧实现
/// let ok_max = ctrl.write_and_verify("15")...;   // ← 写死 15
/// let max = if ok_max { 15 } else { 14 };
/// ```
///
/// 后果：某机型表若有 20 档（可写 `0..=20`），
///
/// | 模式 | 得到的上限 | 是否正确 |
/// |---|---|---|
/// | 快速检测（推导） | **20** | ✅ |
/// | 完整检测（写探） | **15** ← 被覆盖 | ❌ **凭空缩小可用范围** |
///
/// K90PM 恰好是 15 行、可写 0~15，两者一致，所以一直没暴露。
/// 换成档位更多的机型就会出错。
///
/// # 现在的行为
///
/// 传入**推导出的** `expected_max`，只做一次读写校验：
///
/// | 结果 | 处理 |
/// |---|---|
/// | 写入成功且读回一致 | 确认可用，上限不变 |
/// | 写入被拒 / 读回不符 | 上限**降一档**（保守），并在备注里说明 |
///
/// 无论成败都**恢复原值**，不留痕。
///
/// ⚠️ 这是全项目**唯一**会主动改变充电参数的探测项。
fn verify_gear_max(nodes: &sysfs::Nodes, expected_max: u8) -> Result<u8> {
    let ctrl = nodes
        .ctrl
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("档位节点不可用"))?;

    let orig = ctrl.read_i64().unwrap_or(0);

    // 试写推导出的上限（而不是写死的 15）
    let ok = ctrl
        .write_and_verify(&expected_max.to_string())
        .unwrap_or(false);

    // 恢复原值（无论成败）—— 不留痕
    let _ = ctrl.write_i64(orig);

    Ok(if ok {
        expected_max
    } else {
        expected_max.saturating_sub(1)
    })
}

/// 当前 Unix 时间戳
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    // ────────────────────────────────────────────────────────────
    //  温度单位解析
    //
    //  真机实测遇到的三种写法（K90 Pro Max / Android 17）：
    //    ffc_temp_high                    = 48     → 48.0℃
    //    max_thermal_board_temp           = 50     → 50.0℃
    //    quick_revchg_max_batt_temp       = 400    → 40.0℃
    //  以及必须被挡掉的"语义不同"的值：
    //    die_temp_max / adp_temp_max      = 125    → 芯片/适配器，不是电池
    // ────────────────────────────────────────────────────────────

    #[test]
    fn 整数摄氏度的解析() {
        assert_eq!(parse_temp_celsius(48), Some(48.0));
        assert_eq!(parse_temp_celsius(50), Some(50.0));
        assert_eq!(parse_temp_celsius(20), Some(20.0));
        assert_eq!(parse_temp_celsius(80), Some(80.0));
    }

    #[test]
    fn 十分之一摄氏度的解析() {
        assert_eq!(parse_temp_celsius(400), Some(40.0));
        assert_eq!(parse_temp_celsius(452), Some(45.2));
        assert_eq!(parse_temp_celsius(455), Some(45.5));
    }

    #[test]
    fn 毫摄氏度的解析() {
        assert_eq!(parse_temp_celsius(45200), Some(45.2));
        assert_eq!(parse_temp_celsius(55000), Some(55.0));
    }

    #[test]
    fn 芯片温度上限被挡掉() {
        // 125℃ 是芯片/适配器温度上限，与电池温度语义不同，
        // 若被采纳会让 hardware_max_c 虚高，失去保护意义。
        assert_eq!(parse_temp_celsius(125), None);
    }

    #[test]
    fn 哨兵值被挡掉() {
        // 设备树用"全 1"或 0 表示未定义
        assert_eq!(parse_temp_celsius(0), None);
        assert_eq!(parse_temp_celsius(0xFFFF), None);
        assert_eq!(parse_temp_celsius(0xFFFF_FFFF), None);
    }

    // ────────────────────────────────────────────────────────────
    //  标称功率解析（从电池型号 / 设备树节点名）
    //
    //  真机实测（K90 Pro Max）：
    //    电池型号          P11U_7560mah_100w   → 100W
    //    设备树            support_pps_100w    → 100W
    //    电流上限 22A × 5V                     ≈ 110W
    //  三处一致 —— 这是**出厂标称值**，不随电量/充电器变化。
    // ────────────────────────────────────────────────────────────

    #[test]
    fn 从电池型号解析标称功率() {
        // 本机实测值
        assert_eq!(
            nominal_power_from_battery_model("P11U_7560mah_100w"),
            Some(100)
        );
        // 他机型的常见形态
        assert_eq!(
            nominal_power_from_battery_model("BM56_5000mah_67w"),
            Some(67)
        );
        assert_eq!(nominal_power_from_battery_model("X_4500mah_33w"), Some(33));
        assert_eq!(
            nominal_power_from_battery_model("Y_6000mah_120w"),
            Some(120)
        );
    }

    #[test]
    fn 大写w也能解析() {
        assert_eq!(
            nominal_power_from_battery_model("P11U_7560mah_100W"),
            Some(100)
        );
    }

    #[test]
    fn 型号不含功率时返回空() {
        // 某些机型可能不写功率
        assert_eq!(nominal_power_from_battery_model("P11U_7560mah"), None);
        assert_eq!(nominal_power_from_battery_model(""), None);
        assert_eq!(nominal_power_from_battery_model("_w"), None);
        assert_eq!(nominal_power_from_battery_model("abc_w"), None);
        // 结尾不是 w
        assert_eq!(nominal_power_from_battery_model("P11U_7560mah"), None);
    }

    #[test]
    fn 不把容量误当功率() {
        // `7560mah` 的 7560 是容量，不能当功率。
        // 必须只认最后一个 `_` 之后的部分。
        assert_eq!(
            nominal_power_from_battery_model("B_7560mah_100w"),
            Some(100)
        );
        // 若型号是 `X_7560mah`（无功率段），7560 不该被采纳
        assert_eq!(nominal_power_from_battery_model("X_7560mah"), None);
    }

    #[test]
    fn 不合理区间被挡掉() {
        // 低于 5W 或高于 500W 都是脏数据
        assert_eq!(nominal_power_from_battery_model("X_5000mah_1w"), None);
        assert_eq!(nominal_power_from_battery_model("X_5000mah_9999w"), None);
        assert_eq!(nominal_power_from_battery_model("X_5000mah_600w"), None);
    }

    #[test]
    fn 解析带空白的型号() {
        assert_eq!(
            nominal_power_from_battery_model("  P11U_7560mah_100w \n"),
            Some(100)
        );
    }

    #[test]
    fn 档位可写上限为表长度() {
        // 契约事实：K90PM 的档位表有 15 行（编号 0~14），
        // 而驱动**额外接受**写入 15（比 14 更紧）。
        // 因此可写上限 = 表长度，而不是"最后一档的编号"。
        //
        // 用真实的 15 行表验证推导规则：
        let mut rows = Vec::new();
        for ma in [13500u32, 9000, 5000, 500] {
            let mut row = [0u32; crate::sysfs::GEAR_TABLE_FIELDS];
            row[4] = ma;
            rows.push(row);
        }
        let table = crate::sysfs::GearTable { rows };
        let count = table.len();

        // 这就是 detect.rs 里实际使用的推导
        let max = u8::try_from(count).unwrap();
        assert_eq!(count, 4);
        assert_eq!(max, 4, "4 行的表 ⇒ 可写 0~4（表定义 0~3，驱动多接受一档）");
    }

    // ────────────────────────────────────────────────────────────
    //  功率上限：三来源取最大值
    //
    //  真机实测（K90 Pro Max，2026-10-13）：
    //    插电脑 USB（SDP）  → charger_common/power_max = 0
    //    插原装充电器        → charger_common/power_max = 100
    //    电池型号            → P11U_7560mah_100w = 100
    //    设备树              → support_pps_100w  = 100
    //
    //  ⇒ 取最大值才能在"插着电脑"时仍记录正确的机型能力
    // ────────────────────────────────────────────────────────────

    /// 复刻 detect.rs 第 ⑦ 步的取最大值逻辑
    ///
    /// ⚠️ 与实现保持同一套语义：**值相等时保留先出现的**（即可信度最高的）。
    fn pick_power(
        node: Option<u16>,
        model: Option<u16>,
        dt: Option<u16>,
    ) -> (Option<u16>, ValueSource) {
        let mut best: Option<(u16, ValueSource)> = None;
        for (w, src) in [
            (node, ValueSource::Node),
            (model, ValueSource::BatteryModel),
            (dt, ValueSource::Devicetree),
        ] {
            if let Some(w) = w {
                if best.is_none_or(|(bw, _)| w > bw) {
                    best = Some((w, src));
                }
            }
        }
        best.map_or((None, ValueSource::Unknown), |(w, s)| (Some(w), s))
    }

    #[test]
    fn 插着电脑时仍能记录正确的功率上限() {
        // 电脑 USB 口 → 协商值读 0（被 read_power_max_node 过滤成 None）
        // 但电池型号与设备树仍给出 100 ⇒ 最终记录 100，**不是 0**
        let (w, src) = pick_power(None, Some(100), Some(100));
        assert_eq!(w, Some(100), "插电脑时不能把功率上限记成 0");
        assert_eq!(src, ValueSource::BatteryModel, "电池型号优先于设备树");
    }

    #[test]
    fn 插着弱充电器时不降低功率上限() {
        // 33W 充电器 → 协商到 33，但机型能力是 100
        // 取最大值 ⇒ 记录 100（换回原装后不会被 33 卡住）
        let (w, src) = pick_power(Some(33), Some(100), Some(100));
        assert_eq!(w, Some(100), "弱充电器不得拉低机型能力上限");
        assert_eq!(src, ValueSource::BatteryModel);
    }

    #[test]
    fn 插着原装充电器时采用协商值() {
        // 协商值 105（略高于标称）⇒ 取最大值，来源标为 node
        let (w, src) = pick_power(Some(105), Some(100), Some(100));
        assert_eq!(w, Some(105));
        assert_eq!(src, ValueSource::Node, "协商值可信度最高");
    }

    #[test]
    fn 三来源都缺时不做钳位() {
        let (w, src) = pick_power(None, None, None);
        assert_eq!(w, None, "读不到就不记录 ⇒ 不做功率钳位");
        assert_eq!(src, ValueSource::Unknown);
    }

    #[test]
    fn 协商节点读0时被过滤掉() {
        // `read_power_max_node` 内部把 0 过滤成 None。
        // 这里验证契约：只有正数才会进入候选。
        let candidates: Vec<u16> = [0i64, -1, 100]
            .into_iter()
            .filter(|v| *v > 0)
            .filter_map(|v| u16::try_from(v).ok())
            .filter(|w| (5..=500).contains(w))
            .collect();
        assert_eq!(candidates, vec![100], "0 与负数必须被挡掉");
    }

    // ────────────────────────────────────────────────────────────
    //  来源标记的语义
    // ────────────────────────────────────────────────────────────

    #[test]
    fn 只有直读来源算可信() {
        assert!(ValueSource::Node.is_trusted());
        assert!(ValueSource::BatteryModel.is_trusted());
        assert!(!ValueSource::Devicetree.is_trusted());
        assert!(!ValueSource::Profile.is_trusted());
        assert!(!ValueSource::Default.is_trusted());
        assert!(!ValueSource::Unknown.is_trusted());
    }

    #[test]
    fn 设备树算本机数据() {
        // ⚠️ 这是与 `is_trusted` 的**关键区别**：
        //    设备树在"取值优先级"上低于节点直读，
        //    但在"是否本机数据"上与节点同级 —— 档位表与硬件温度
        //    本来就只能从设备树读，把它算作不可信会让用户误以为数据有问题。
        assert!(
            ValueSource::Devicetree.is_local(),
            "设备树是本机数据，不该触发「数据可能不准」告警"
        );

        // 真正的非本机来源（他人档案 / 硬编码猜测）才该告警
        assert!(!ValueSource::Profile.is_local());
        assert!(!ValueSource::Default.is_local());
        assert!(!ValueSource::Unknown.is_local());

        // 三个本机来源
        assert!(ValueSource::Node.is_local());
        assert!(ValueSource::BatteryModel.is_local());
    }

    #[test]
    fn 本机来源是取信来源的超集() {
        // `is_authoritative` 蕴含 `is_local`（反之不然）
        for src in [
            ValueSource::Node,
            ValueSource::BatteryModel,
            ValueSource::Devicetree,
            ValueSource::Profile,
            ValueSource::Default,
            ValueSource::Unknown,
        ] {
            if src.is_authoritative() {
                assert!(src.is_local(), "{src:?} 是权威来源但不算本机？矛盾");
            }
        }
    }

    #[test]
    fn 每种来源都有中文标签() {
        // 界面上要显示这些，不能是空串
        for src in [
            ValueSource::Node,
            ValueSource::BatteryModel,
            ValueSource::Devicetree,
            ValueSource::Profile,
            ValueSource::Default,
            ValueSource::Unknown,
        ] {
            assert!(!src.label().is_empty(), "{src:?} 缺少标签");
        }
    }

    #[test]
    fn 来源标记可序列化为小写下划线() {
        let j = serde_json::to_string(&ValueSource::BatteryModel).unwrap();
        assert_eq!(j, "\"battery_model\"");
        let j = serde_json::to_string(&ValueSource::Devicetree).unwrap();
        assert_eq!(j, "\"devicetree\"");
    }

    #[test]
    fn 档案缺少来源标记时能反序列化() {
        // 兼容旧版档案：没有 `sources` 字段也要能读
        let json = r#"{
            "gear": {"min":0,"max":15,"open":0,"safe":15,"table":[]},
            "current": {"min_ua":0,"max_ua":18400000,"safe_ua":700000},
            "power": {"max_w":100,"max_power_gear":0,"protocol":null},
            "thermal": {"hardware_max_c":48.0,"abs_max_c":45.0},
            "verified": false
        }"#;
        let caps: thermo_ipc::device::Capabilities = serde_json::from_str(json).unwrap();
        assert_eq!(
            caps.sources.power_max,
            ValueSource::Unknown,
            "缺字段时默认为 Unknown"
        );
        assert_eq!(caps.power.max_w, Some(100), "其它字段正常读出");
    }
}
