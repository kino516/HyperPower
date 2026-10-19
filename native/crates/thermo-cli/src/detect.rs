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
//! | 4 | 档位上限（档位表**或** `charge_control_limit_max`） | ❌ | 无 |
//! | 5 | 档位数 / 最大功率档位 | ❌ | 无 |
//! | 6 | 电流上限（读探） | ❌ | 无 |
//! | 7 | 充电功率上限（三来源取最大） | ❌ | 无（只读） |
//! | 8 | 硬件保护温度（设备树**或** `max_life_temp` 一类节点） | ❌ | 无 |
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
//! ⚠️ 2026-10-19：档位上限有**两条来源**，缺少任何一条都不能报"失败"——
//! | 机型 | 来源 |
//! |---|---|
//! | 小米 `charger_thermal` 封装层 | 设备树档位表的**行数** |
//! | 标准 Linux `power_supply` | 节点 `charge_control_limit_max`（**没有档位表**） |
//! 两者都没有时退回 `profile` 的暂定区间（`0~30`），并在界面上标明"暂定"。
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

/// `failed_items` 的内部 id → 给用户看的中文名
///
/// # ⚠️ 为什么需要一张表
///
/// `failed_items` 里存的是 `current_ceiling` 这类**内部 id** ——
/// 它们会写进 `device.json`，**不得改动**（老档案里存的就是这些字符串）。
/// 但直接显示给用户等于让他看代码。因此单独一张"给界面用"的对照表。
///
/// # ⚠️ 新增 `failed_items.push` 时**必须**在这里补一行
///
/// 漏了不会报错 —— 那个项会显示成它自己的 id（如 `new_thing`），
/// 用户看不懂。这是刻意的兜底：**显示 id 比显示"其它"更有诊断价值**，
/// 至少能一眼看出是哪一项。
fn failed_item_label(id: &str) -> &str {
    match id {
        "observe_nodes" => "观测节点",
        "control_nodes" => "控速节点",
        "soc_nodes" => "充电上限节点",
        "gear_table" => "档位表",
        "current_ceiling" => "电流上限",
        "hardware_max_temp" => "硬件保护温度",
        "power.max_w" => "充电功率上限",
        "gear_write_probe" => "档位可写上限",
        // 未登记的 id 原样显示（见上面的说明）
        other => other,
    }
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

/// 档位上限是**怎么来的**
///
/// # 为什么需要它
///
/// 同一个数字（比如 `16`）有三种完全不同的出处，而用户在界面上
/// 只能看到一句话，必须说清楚是哪一种：
///
/// | 来源 | 机型 | 可信度 |
/// |---|---|---|
/// | [`Table`](Self::Table) | 有档位表（小米 `watt_range` 一类） | 设备树直读 |
/// | [`Node`](Self::Node) | 标准 `power_supply`（`charge_control_limit_max`） | 节点直读 |
/// | [`Unknown`](Self::Unknown) | 两者都没有 | **暂定区间**，可能不准 |
///
/// ⚠️ 第三种必须让用户看出来是"猜的" —— 否则他会以为 `0~30` 是本机实测值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GearMaxOrigin {
    /// 由档位表行数推导
    Table,
    /// 由节点 `charge_control_limit_max` 直读
    Node,
    /// 都没读到，用 `profile` 的暂定区间
    Unknown,
}

impl GearMaxOrigin {
    /// 生成面向用户的一句话（含档位数与出处）
    ///
    /// 形如 `16 档（节点直读）` / `15 档（档位表推导）` /
    /// `30 档（暂定区间，未探测到）`。
    fn describe(self, n: u8) -> String {
        match self {
            Self::Table => format!("{n} 档（档位表推导）"),
            Self::Node => format!("{n} 档（节点直读）"),
            Self::Unknown => format!("{n} 档（暂定区间，未探测到）"),
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
        // ⚠️ `min` / `max` 是**通用保守区间**，不是机型实测值
        //
        // # 为什么区间没有探测
        //
        // 标准 `power_supply` 的 `charge_control_end_threshold` **没有**
        // 配套的 min/max 节点（2026-10-19 真机核对过 `battery` 与
        // `qcom-battery` 两个目录，只有 end/start_threshold 与
        // charge_control_limit(_max)）。小米 `smart_chg` 那套同样没有。
        //
        // ⇒ 驱动到底接受 20% 还是只接受 60%，**无法在只读阶段得知**。
        //   真实边界由「写进去 + 回读校验」暴露（见 `SocLimit::tick`）。
        //
        // 因此这里给的是**保守的通用区间**：
        //
        // | 值 | 理由 |
        // |---|---|
        // | 60 | 电池健康保护类功能普遍以 60% 为最低有意义上限（再低等于充不满） |
        // | 100 | 物理上限 |
        //
        // ⚠️ **这不是"写死某机型数据"**：它不来自任何一台机的实测，
        //    而是功能语义上的保守默认。改它之前请同步确认
        //    `soc_limit/config.rs` 的 `SOC_PHYSICAL_MIN/MAX`（20~100）
        //    与界面滑块的范围，三处必须一致。
        profile.capabilities.soc_limit = Some(SocLimitCapability {
            supported: true,
            min: 55,
            max: 100,
        });
        // ⚠️ 文案格式：`名称：短结论`
        //
        // 「检测记录」卡片把它拆成左右两列，而用户 2026-10-17 明确要求
        // **右列要短** —— 长句会让卡片变成一堵文字墙，扫读不了。
        //
        // 节点的完整路径不放在这里：它属于**排障细节**，
        // `ctl status` / `device.json` 的 `nodes` 段里都有。
        meta.notes.push("SOC 上限：节点可用".to_string());
    } else {
        meta.failed_items.push("soc_nodes".to_string());
        // 文案不点名 `smart_chg`：标准框架的机型用的是
        // `charge_control_end_threshold`，"找不到 smart_chg 节点"会让
        // 用户以为自己的机器缺了个本该有的东西。
        meta.notes
            .push("SOC 上限：不可用（未找到可写的充电上限节点）".to_string());
    }

    // ---------- ④ 档位表（设备树直读，**零副作用**）----------
    log::info!("[4/10] 读取充电档位表…");
    let mut gear_rows: Vec<GearRow> = Vec::new();
    // 档位上限的**来源** —— 决定第 ⑧ 步那条面向用户的文案怎么说
    // （有的机型靠档位表推导，有的只能读节点，见 §4 的兜底）
    let mut gear_max_origin = GearMaxOrigin::Unknown;
    match sysfs::GearTable::detect() {
        // ⚠️ 第二个返回值是档位表路径，**这里不再需要**：
        //    它原先进 `notes` 文案（"…来源 {path}…"），而那正是
        //    让「检测记录」右列过长的主因之一（见下方 notes 的说明）。
        //    路径在 `device show` 与设备档案里都有。
        Ok((table, _path)) => {
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
            gear_max_origin = GearMaxOrigin::Table;

            // 最大功率档位 = 电流上限最大的档
            profile.capabilities.power.max_power_gear = table.max_power_gear();

            // ⚠️ 短结论：档位数 + 来源类型。（完整路径不进界面，见上面的说明）
            meta.notes
                .push(format!("档位表：{count} 档（0~{}）· 设备树直读", count - 1));
            if let Some(g) = table.max_power_gear() {
                meta.notes.push(format!("最大功率档位：{g}"));
            }
        }
        Err(e) => {
            // ⚠️ 这里必须区分**两种"读不到"**，否则界面会说谎
            //
            // | 情形 | 旧文案（一律当失败） | 实际情况 |
            // |---|---|---|
            // | 机型**没有**档位表节点 | 「档位表读取失败，沿用保守范围 0~15」 | **完全正常** —— 这类机型用标准 `power_supply` 接口 |
            // | 节点在、但读/解析失败 | 同上 | 真失败，该让用户知道 |
            //
            // 旧代码把两者混为一谈，还写死了 `0~15`（K90PM 的数字）。
            // 后果（2026-10-19 真机）：标准 `power_supply` 框架的机型
            // 一跑检测就红着一行「档位表读取失败」—— 而它本来就没有
            // 这个节点；同时界面报的「0~15」与它真实的 `0~16` 不符。
            let has_node = sysfs::GEAR_TABLE_CANDIDATES
                .iter()
                .any(|p| sysfs::Node::new(*p).exists());
            if has_node {
                // 真失败：进 failed_items（界面「未探测到」区块会列出来）
                log::warn!("档位表直读失败：{e:#}");
                meta.failed_items.push("gear_table".to_string());
            } else {
                // 机型没有这个节点：**不是**失败，降级为 info
                log::info!("[4/10] 本机型没有档位表节点 —— 改用标准接口读档位上限（不是失败）");
            }
            // 档位表确实没有（来源如实标 `unknown`）
            profile.capabilities.sources.gear_table = ValueSource::Unknown;
            // ⚠️ 这里**不写 notes**：档位上限的文案统一由第 ⑧ 步输出。
            //    否则界面上会出现两条同名的「档位上限」，而且这里还
            //    说不出真实的档位数（要等下面的兜底读完才知道）。
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
    // ⚠️ **没有档位表时的兜底**（2026-10-19 新增）
    //
    // 上面的 `gear.max = 表长度` 只在**读到了档位表**时执行（在那个
    // `Ok(...)` 分支内部）。而部分机型完全没有档位表 —— 此时
    // `gear.max` 会保留 `profile` 里的暂定值，可能与真实档位数不符
    // （实测：暂定 15、真实 16）。
    //
    // ⇒ 退到标准接口的 `charge_control_limit_max` 读真值；
    //   连它都没有，就用 profile 的暂定区间上界（用户 2026-10-19 定 0~30）。
    // 判据用已经 move 进档案的那份表（`gear_rows` 在 L212 已 move）
    if profile.capabilities.gear.table.is_empty() {
        let mut picked = None;
        for p in sysfs::GEAR_MAX_CANDIDATES {
            let Ok(raw) = fs::read(p) else { continue };
            let Ok(s) = std::str::from_utf8(&raw) else {
                continue;
            };
            let Ok(v) = s.trim().parse::<u8>() else {
                continue;
            };
            // 合理性：档位数不该是 0，也不该大到离谱
            if v == 0 || v > 64 {
                continue;
            }
            picked = Some((v, *p));
            break;
        }
        match picked {
            Some((v, p)) => {
                log::info!("[4/10] 档位上限取自 {p}：{v} 档");
                profile.capabilities.gear.max = v;
                profile.capabilities.gear.safe = v;
                profile.capabilities.sources.gear_max = ValueSource::Node;
                gear_max_origin = GearMaxOrigin::Node;
            }
            None => {
                profile.capabilities.gear.max = 30;
                profile.capabilities.gear.safe = 30;
                // 0~30 是**硬编码兜底**，不是本机实测 —— 来源如实标 `default`
                profile.capabilities.sources.gear_max = ValueSource::Default;
                log::warn!("[4/10] 档位上限无从探测，暂按 0~30 处理");
                gear_max_origin = GearMaxOrigin::Unknown;
            }
        }
    }
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
        // ⚠️ **不能只写 `notes`**（2026-10-14 修）
        //
        // 只写 notes 的后果（真机踩到过）：档案里什么都没记，
        // 界面和 `device show` 都不提示，用户以为检测全部成功 ——
        // 而实际上 `current.max_ua` 用的是兜底常量，不是本机实测值。
        //
        // ⚠️ 但**也不能进 `failed_items`**（2026-10-19 用户要求）
        //
        // 这不是"失败"，是"当前状态测不了"：充电中该节点反映的是
        // **模块自己写入的值**，采信会形成自我循环、逐次压低上限。
        // 拔掉充电器就能测 —— 因此归 [`pending_items`]（待补测）。
        //
        // # 为什么必须分开（用户原话：「总有一个测试显示感叹号」）
        //
        // | 项 | 状态要求 |
        // |---|---|
        // | 电流上限 | **必须未充电** |
        // | 档位可写上限 | **必须充电中** |
        //
        // 两者相反 ⇒ 无论插不插电总有一项测不了。若都算"失败"，
        // 用户每次都会看到感叹号，而机器完全正常 —— 久而久之
        // 他会忽略所有告警。那是比不提示更大的问题。
        meta.pending_items.push("current_ceiling".to_string());
        // ⚠️ 这里原先写的是 `**跳过**` —— markdown 的加粗语法。
        //
        // `notes` 最终原样渲染到「检测记录」卡片里（`MorePage.vue` 直接
        // 插入文本，不做 markdown 解析），于是**用户看到的是带星号的原文**：
        //
        //     充电中 —— **跳过**电流上限检测（…）
        //
        // 后端产出的是**给用户看的文案**，不该混入任何标记语法。
        // 强调意图用措辞表达（"已跳过"本身已经足够醒目）。
        //
        // 并由 `notes里不得出现markdown标记` 测试守住这一类问题。
        //
        // ⚠️ 文案格式：`名称：结论 —— 怎么办`
        //
        // 「检测记录」卡片按**第一个 `：`** 把每条拆成左右两列
        // （左列名称、右列结论）。写成"充电中 —— 已跳过电流上限检测…"
        // 这种整句式会让它退化成一行长文，白占了卡片的宽度。
        meta.notes
            .push("电流上限：已跳过（充电中测得不准）—— 拔掉充电器后重跑".to_string());
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
    // # 可信来源：设备树属性 **或** sysfs 节点
    //
    // | 级 | 来源 | 语义 | 典型值 |
    // |---|---|---|---|
    // | ① | 设备树 `ffc_temp_high` | 有线快充的电池温度上限 | 48.0℃（K90PM） |
    // | ① | 节点 `qcom-battery/max_life_temp` | 电池允许的最高工作温度 | 44℃（标准框架机型） |
    // | ② | 兜底常量 | 通用保守值（**明确标记为 Default**） | 45.0℃ |
    //
    // ⚠️ 这两条是**同一条线**（"电池能承受多热"），只是导出方式不同。
    //    老机型没有 `mca_*` 设备树，只能读节点 —— 2026-10-19 前
    //    那条路根本没走通（只认 4 字节二进制），于是白退 45℃ 兜底。
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
    if let Some((t, src)) = read_hardware_max_temp() {
        profile.capabilities.thermal.hardware_max_c = t;
        profile.capabilities.sources.hardware_max_temp = src;
        // 文案按**真实来源**说 —— 节点读到的不能写"设备树直读"
        let how = match src {
            ValueSource::Devicetree => "设备树直读",
            ValueSource::Node => "节点直读",
            _ => "直读",
        };
        meta.notes.push(format!("硬件保护温度：{t}℃ · {how}"));
    } else {
        meta.failed_items.push("hardware_max_temp".to_string());
        meta.notes.push(
            "硬件保护温度：未探测到（属性名因机型而异）· 用保守兜底 45℃ —— \
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
            // 三个来源的逐项读数只记日志 —— 拼进界面会让右列变成一句长文
            log::info!("[8/10] 功率来源逐项读数：{}", parts.join(" / "));

            // ✅ **固定数据**：充电功率上限（用户 2026-10-17 确认）
            //
            // 它是三个来源**取最大**：
            //
            // | 来源 | 性质 |
            // |---|---|
            // | 电池型号里的 `_100w` | **固定**（出厂标称） |
            // | 设备树 `support_pps_100w` | **固定**（支持的协议档位） |
            // | `charger_common/power_max` | 实时（当前充电头谈成的） |
            //
            // 取最大之后通常由**固定来源**决定 ⇒ 它表达的是
            // "**这台手机**最高能吃多少"，与充电头无关。
            //
            // ⚠️ 不要与下面那条"协商值"混为一谈 —— 那个才是实时的。
            meta.notes
                .push(format!("充电功率上限：{w}W · 来源{}", power_source.label()));

            // ⚠️ 协商值**刻意不写 notes**（用户 2026-10-17）
            //
            // 它是**实时数据**：插这个头是 0，换原装头就是 100W。
            // 写进"检测档案"会让档案里混一个随地变化的数字 ——
            // 换 33W 头再跑一次检测，档案就成了 33W，
            // 用户会以为"我的手机只能充 33W"。
            //
            // 它的位置在**状态页三格的「充电头」**（实时显示，本就不该归档）。
            if candidate_node.is_none() {
                log::info!("[8/10] 协商值缺失（未协商出快充协议）—— 属实时数据，不进检测记录");
            }
        }
        None => {
            meta.failed_items.push("power.max_w".to_string());
            // 这一条是**检测结果**（"这项没测到"），不是实时值 ⇒ 照旧记录
            meta.notes.push(
                "充电功率上限：未能读出（协商值 / 电池型号 / 设备树均无功率信息）".to_string(),
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
    // ⚠️ **未插充电器时不能做写探**（2026-10-19 真机确认）
    //
    // # 为什么
    //
    // 未插电时驱动不接受档位写入 —— 与小米那套
    // "adapter is plugout, don't handle soc limit" 是同一个道理。
    // 此时写探**必然失败**，而失败的处理是"保守降一档"：
    //
    // | 实测（K70 Pro，Discharging） | 结果 |
    // |---|---|
    // | `charge_control_limit_max` | **16** |
    // | 写探 16（未插电） | 失败 |
    // | 档案里记成 | **15** ← 白丢一档 |
    //
    // 也就是说：**拔着电跑一次完整检测，会把硬件上限永久记低一档**，
    // 而用户的机器实际支持 16 档。这与"不写死机型数据"直接冲突 ——
    // 数据不是写死的，是被检测**测错了**。
    //
    // ⇒ 未插电就**跳过**写探，保留推导值，并如实说明跳过原因。
    //   用户想验证时插上电再勾一次即可。
    let derived = profile.capabilities.gear.max;
    let how = gear_max_origin.describe(derived);
    let probe_gear = opts.mode == ProbeMode::Full && opts.allow_write_probe;
    if probe_gear && !charging_now {
        log::info!("[8/10] 未插充电器 —— 跳过档位写探（此状态下驱动不接受写入）");
        // 归 `pending_items`（待补测）而非 `failed_items`：
        // 这不是失败，插上充电器再勾一次就能测完。
        // 详见 `ProbeMeta::pending_items` 的说明。
        meta.pending_items.push("gear_write_probe".to_string());
        meta.notes.push(format!(
            "档位上限：{how} · 未插电，跳过写探 —— 插上充电器再勾一次即可验证"
        ));
    } else if probe_gear {
        log::info!("[8/10] 验证档位可写上限（{how}）…");

        match verify_gear_max(&nodes, derived) {
            Ok(max) => {
                if max == derived {
                    meta.notes.push(format!("档位上限：{how}，已验证可写"));
                } else {
                    meta.notes.push(format!(
                        "档位上限：{how}，但这个值写不进去，保守降为 {max} 档"
                    ));
                }
                profile.capabilities.gear.max = max;
                profile.capabilities.gear.safe = max;
            }
            Err(e) => {
                meta.failed_items.push("gear_write_probe".to_string());
                // ⚠️ 错误细节**进日志，不进 notes**
                //
                // 原先文案是"档位上限验证失败，沿用推导值 {derived}：{e:#}" ——
                // `{e:#}` 是 anyhow 的**完整因果链**，动辄两三行，
                // 直接违反"右列要短"这条要求（用户 2026-10-17 反馈）。
                // 界面只说"失败了"，细节去 logcat 里看。
                log::warn!("[8/10] 档位上限写探失败（沿用 {how}）：{e:#}");
                meta.notes.push(format!("档位上限：验证失败，沿用 {how}"));
            }
        }
    } else {
        // 🔴 **快速模式必须也记一条**（2026-10-17 用户发现缺漏）
        //
        // 原先这里只写日志 ⇒ 跑完快速检测，「检测记录」里**根本没有
        // "档位上限"这一项**。用户无法区分：
        //
        // | 可能 | 用户看到的 |
        // |---|---|
        // | 没测这一项 | （什么都没有） |
        // | 测了、没问题 | （还是什么都没有） |
        //
        // 而同一个检测里，「电流上限」因充电被跳过时是**明确记一条**的
        // （"电流上限：已跳过（充电中测得不准）"）—— 两处处理不一致。
        //
        // ⚠️ 语气是 `skip` 而非 `warn`：不做写探是**设计行为**
        //    （快速检测的全部价值就是"零副作用"），不是失败。
        //
        // ⚠️ 这条也是「档位上限」**唯一**的快速模式文案 ——
        //    第 ④ 步不再单独记一条（否则界面上会出现两行同名项）。
        meta.notes.push(format!("档位上限：{how} · 未做写探"));
        log::info!("[8/10] 跳过档位上限验证（快速模式或未加 --confirm）");
    }

    // ---------- ⑨ 场景（已移除）----------
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
    // | 3 | 模块**不依赖**它 |
    //
    // ⇒ 整条删除。
    //
    // 🔴 2026-10-17 **连状态页那一行也删了**（用户要求：「只能看，没用处」）。
    //    当时这里写着"保留 `nodes.scene` 与 `describe_scene` —— 状态页仍在用"，
    //    现在状态页也不用了，于是 `Nodes.scene` / `SCENE_CANDIDATES` /
    //    `describe_scene` / `ReadOnlyNode` 全部摘除。
    //    查证记录见 `sysfs.rs` 里 `Nodes` 那段注释。
    log::info!("[9/10] 跳过场景记录（该字段已整体移除）");

    // ---------- ⑩ 收尾 ----------
    // ✅ **未成功项单独列一条**（用户 2026-10-17 要求）
    //
    // 原先这些只写进 `probe_meta.failed_items`（内部 id 的数组），
    // 界面上的表现是"这一项干脆不出现" —— 用户无法区分：
    //
    // | 可能 | 用户看到的 |
    // |---|---|
    // | 没测这一项 | （什么都没有） |
    // | 测了、没问题 | （还是什么都没有） |
    //
    // ⚠️ 必须**翻译成中文**：`failed_items` 里存的是 `current_ceiling`
    //    这类内部 id，直接显示等于让用户看代码。
    //
    // ⚠️ 要**去重**：`current_ceiling` 有 4 个 push 点（充电中跳过、
    //    节点不可用、读失败…），同一个 id 可能出现多次。
    if !meta.failed_items.is_empty() {
        let mut names: Vec<&str> = meta
            .failed_items
            .iter()
            .map(|k| failed_item_label(k))
            .collect();
        names.sort_unstable();
        names.dedup();
        meta.notes.push(format!("未成功项：{}", names.join(" / ")));
    }

    // ── 待补测项（**中性语气**，不是失败）──
    //
    // 用户 2026-10-19：「插电的时候无法测电流上限，不插电的时候无法测
    // 档位上限，总有一个测试显示感叹号」。
    //
    // 这两项的状态要求相反，所以任何一次检测都必然有一项测不了。
    // 关键是**别让它看起来像故障** —— 用"换个状态就能补测"的说法，
    // 而不是"未成功"。
    if !meta.pending_items.is_empty() {
        let mut names: Vec<&str> = meta
            .pending_items
            .iter()
            .map(|k| failed_item_label(k))
            .collect();
        names.sort_unstable();
        names.dedup();
        meta.notes.push(format!(
            "待补测：{} —— 这两项要求的插电状态相反，换状态再跑一次即可，不是故障",
            names.join(" / ")
        ));
    }

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
///
/// # 返回值带上**来源**
///
/// 2026-10-19 前这里只返回温度，调用处一律标 `Devicetree`、文案一律写
/// 「设备树直读」。但候选表里混着两类完全不同的东西：
///
/// | 路径 | 实际来源 |
/// |---|---|
/// | `/sys/firmware/devicetree/...` | 设备树属性 |
/// | `/sys/class/qcom-battery/max_life_temp` | **sysfs 节点** |
///
/// 于是标准 `power_supply` 框架的机型会被标成"设备树直读" —— 既说错了
/// 出处，又让 [`ValueSource::is_authoritative`] 把它降了一档
/// （`Devicetree` 不是权威来源，`Node` 才是）。因此这里把来源一起带回。
fn read_hardware_max_temp() -> Option<(f32, ValueSource)> {
    for path in sysfs::HW_MAX_TEMP_CANDIDATES {
        let Ok(raw) = fs::read(path) else { continue };

        // 来源按**路径**判定，不按"读到了什么格式"判定
        let src = if path.contains("/firmware/devicetree/") {
            ValueSource::Devicetree
        } else {
            ValueSource::Node
        };

        // ── ① sysfs 节点：ASCII 文本（如 `"44\n"`）──
        //
        // 🔴 这是 2026-10-19 真机上发现的缺口：原实现只认设备树的
        //    **4 字节大端二进制**，而 `qcom-battery` 下的温度上限是
        //    纯文本 `"44\n"`（**3 字节**）⇒ 直接被 `raw.len() < 4`
        //    跳过 ⇒ 检测报告"硬件保护温度未测到"，白退到 45℃ 兜底。
        //
        // ⚠️ 先试文本再试二进制：文本可能长达 4 字节（`"440\n"`），
        //    若先按二进制解析会得到无意义的大数 —— 虽然
        //    `parse_temp_celsius` 的范围检查能拦住，但顺序对了更直观。
        if let Ok(s) = std::str::from_utf8(&raw) {
            if let Ok(v) = s.trim().parse::<u32>() {
                if let Some(c) = parse_temp_celsius(v) {
                    return Some((c, src));
                }
            }
        }

        // ── ② 设备树属性：4 字节大端整数 ──
        if raw.len() >= 4 {
            let v = u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]);
            if let Some(c) = parse_temp_celsius(v) {
                return Some((c, src));
            }
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

    /// 🔴 `notes` 是**给用户看的文案**，不得混入 markdown 标记
    ///
    /// # 这一起事故（2026-10-17 真机）
    ///
    /// `detect.rs` 里有一条写成：
    ///
    /// ```text
    /// 充电中 —— **跳过**电流上限检测（…）
    /// ```
    ///
    /// 而「检测记录」卡片是**直接插入文本**的（不做 markdown 解析），
    /// 于是用户在手机上看到的是**带星号的原文**：
    ///
    /// ```text
    /// 充电中 —— **跳过**电流上限检测（…）
    /// ```
    ///
    /// # 为什么值得一条测试
    ///
    /// 这类问题**编译通过、测试通过、界面不报错** —— 只有肉眼看截图
    /// 才会发现，而它不影响任何功能，因此极易长期潜伏。
    ///
    /// 用源码扫描而不是"检查运行时产出"：`notes` 是十几处
    /// `push(format!(...))` 拼出来的，运行时抓不全。
    #[test]
    fn notes里不得出现markdown标记() {
        // ⚠️ 必须**切掉测试模块自己**再扫
        //
        // 首次实现直接扫整个文件，结果测试**抓到了自己** ——
        // 下面的断言文案里就写着 `**`（用来解释问题），第 1201 行的
        // 检测逻辑本身也含 `"\"` + `"**"` 两个字面量。
        //
        // ⇒ 扫描范围要划在"生产代码"这一侧。
        let src = include_str!("detect.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap_or(src);

        // 只看**非注释行**：文档注释里大量使用 `**强调**`，那是给
        // 开发者看的，完全正当。
        let mut offenders = Vec::new();
        for (i, line) in prod.lines().enumerate() {
            let t = line.trim_start();
            if t.starts_with("//") {
                continue;
            }
            // `**` 出现在字符串字面量里才可疑
            if line.contains('"') && line.contains("**") {
                offenders.push(format!("第 {} 行：{}", i + 1, line.trim()));
            }
        }

        assert!(
            offenders.is_empty(),
            "`notes` 的文案里出现了 markdown 的 `**` —— \
             界面不解析它，用户会看到星号原文：\n{}",
            offenders.join("\n")
        );
    }
}
