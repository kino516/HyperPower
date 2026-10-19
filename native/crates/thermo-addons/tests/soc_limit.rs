//! 充电上限功能的离线测试（**用假的节点树，不碰真机**）
//!
//! 覆盖三层：
//!
//! | 层 | 内容 |
//! |---|---|
//! | 纯逻辑 | 控制字编解码（对照真机实测出的对照表） |
//! | 行为 | 零写入 / 写回 / 回读校验 / 探测 |
//! | 容错 | 配置非法、节点不可用、熔断 |
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;

use thermo_addons::addon::{Addon, ProbeOutcome};
use thermo_addons::io::AddonIo;
use thermo_addons::runner::{AddonRunner, AddonSlot, TRIP_AFTER_FAILURES};
use thermo_addons::soc_limit::{logic, SocLimit, SocLimitConfig};

// ============================================================
//  假的节点树
// ============================================================

const CTRL: &str = "/sys/class/xm_power/charger/smart_charge/smart_chg";
const READ: &str = "/sys/class/xm_power/charger/smart_charge/soc_limit";

/// 标准 Linux `power_supply` 框架的控制节点（2026-10-19 真机那一类）
///
/// ⚠️ 与小米的两个关键差异，正是本轮改动的全部理由：
/// | 差异 | 小米 | 标准 |
/// |---|---|---|
/// | 写入格式 | 控制字 `0x500011` | **纯百分比** `80` |
/// | 生效值在哪 | 同目录的 `soc_limit` | **控制节点自己** |
const STD_CTRL: &str = "/sys/class/power_supply/battery/charge_control_end_threshold";

/// **使能节点**（高通标准框架）
///
/// ⚠️ 这个节点是 2026-10-19 真机（K70 Pro）查出来的关键：
///    它不为 `1` 时，写 [`STD_CTRL`] **表面成功、实际被吞**。
const STD_EN: &str = "/sys/class/qcom-battery/charge_control_en";

#[derive(Debug, Default)]
struct FakeIo {
    files: HashMap<String, String>,
    writable: Vec<String>,
    /// 模拟"写了但不生效"（例如被系统的默认值立刻覆盖）
    write_ignored: bool,
    /// 记录所有写入，用于断言"有没有写"
    write_log: Vec<(String, String)>,
    /// 是否插着充电器（`is_plugged()` 的返回值）
    ///
    /// 默认 `true` —— 绝大多数测试关心的是"有电时怎么写"，
    /// 只有专门测省电逻辑的用例才需要改成 `false`。
    plugged: bool,
}

impl FakeIo {
    /// 一个正常的 HyperOS 节点树（smart_chg 可写，当前生效值 80）
    fn hyperos() -> Self {
        let mut files = HashMap::new();
        files.insert(CTRL.to_string(), "80".to_string());
        files.insert(READ.to_string(), "80".to_string());
        Self {
            files,
            writable: vec![CTRL.to_string()],
            write_ignored: false,
            write_log: Vec::new(),
            plugged: true,
        }
    }

    /// 标准 `power_supply` 框架的节点树（**没有** `soc_limit`，当前不限制）
    ///
    /// `charge_control_end_threshold` 的 `100` 表示"不限制"，
    /// 与小米驱动里 `soc_limit = 100` 的语义一致。
    fn standard_psys() -> Self {
        let mut files = HashMap::new();
        files.insert(STD_CTRL.to_string(), "100".to_string());
        Self {
            files,
            writable: vec![STD_CTRL.to_string()],
            write_ignored: false,
            write_log: Vec::new(),
            plugged: true,
        }
    }

    /// **K70 Pro 的真实节点树**（2026-10-19 真机实测）
    ///
    /// 与 [`standard_psys`](Self::standard_psys) 的区别：多一个
    /// `charge_control_en` 使能节点，且**未开使能时写入会被驱动吞掉**。
    ///
    /// | `en` | 写 threshold=95 | 读回 |
    /// |---|---|---|
    /// | `0` | `write()` 返回**成功**（谎报） | **0** |
    /// | `1` | 成功 | **95** |
    fn k70pro() -> Self {
        let mut files = HashMap::new();
        files.insert(STD_CTRL.to_string(), "0".to_string());
        files.insert(STD_EN.to_string(), "0".to_string());
        Self {
            files,
            writable: vec![STD_CTRL.to_string(), STD_EN.to_string()],
            write_ignored: false,
            write_log: Vec::new(),
            plugged: true,
        }
    }

    /// 拔掉充电器
    fn unplugged(mut self) -> Self {
        self.plugged = false;
        self
    }

    fn writes(&self) -> usize {
        self.write_log.len()
    }
}

impl AddonIo for FakeIo {
    fn exists(&mut self, path: &str) -> bool {
        self.files.contains_key(path)
    }

    fn writable(&mut self, path: &str) -> bool {
        self.writable.iter().any(|p| p == path)
    }

    fn is_plugged(&mut self) -> bool {
        self.plugged
    }

    fn read(&mut self, path: &str) -> Result<String, String> {
        self.files
            .get(path)
            .cloned()
            .ok_or_else(|| format!("节点不存在: {path}"))
    }

    fn write(&mut self, path: &str, value: &str) -> Result<(), String> {
        if !self.writable.iter().any(|p| p == path) {
            return Err(format!("节点不可写: {path}"));
        }
        self.write_log.push((path.to_string(), value.to_string()));

        if self.write_ignored {
            return Ok(());
        }

        // ── 使能节点：纯 0/1，直接保存 ──
        //
        // ⚠️ 不处理它的话会掉进下面的 `parse_word`（它没有 `0x` 前缀
        //    ⇒ 返回 None ⇒ 静默 Ok 且不更新文件）—— 于是"写使能"看起来
        //    成功、实际没变，测试会测出假的"回读不符"。
        if path.ends_with("charge_control_en") {
            self.files
                .insert(path.to_string(), value.trim().to_string());
            return Ok(());
        }

        // ── 标准框架：写入的是**纯百分比**，节点自己就是生效值 ──
        //
        // ⚠️ 必须与小米分支分开：若照小米那套 `parse_word` 解析 `"80"`，
        //    会因为缺少 `0x` 前缀而直接返回 Ok 且**不更新任何文件** ——
        //    于是回读永远是旧值、每拍都报"回读不符"，而假驱动其实
        //    什么都没做。那样测出来的"失败"是假的。
        if path.ends_with("charge_control_end_threshold") {
            // 🔴 **复现 K70 Pro 的真机行为**：使能位不为 1 时写入被吞
            //
            // `write()` 仍返回 Ok（驱动谎言），但值不保存、读回不变。
            // 这正是模块曾经误判"回读不符 → 有程序在抢 → 熔断"的根因。
            let en_on = self
                .files
                .get(STD_EN)
                .map(|v| v.trim() != "0")
                .unwrap_or(true);
            if en_on {
                if let Ok(v) = value.trim().parse::<u8>() {
                    self.files.insert(path.to_string(), v.to_string());
                }
            }
            return Ok(());
        }

        // 模拟驱动语义：低 16 位必须含 0x11 使能位才应用
        let Some(word) = parse_word(value) else {
            return Ok(());
        };
        let dir = path.rsplit_once('/').map_or("", |(d, _)| d);
        let read_path = format!("{dir}/soc_limit");
        if word & logic::ENABLE_FLAGS == logic::ENABLE_FLAGS {
            let (soc, _flags) = logic::decode(word);
            self.files.insert(read_path, soc.to_string());
        } else {
            // 使能位缺失 → 不应用（驱动回落"无限制"）
            self.files.insert(read_path, "100".to_string());
        }
        Ok(())
    }
}

fn parse_word(s: &str) -> Option<u32> {
    let t = s.trim();
    let hex = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X"))?;
    u32::from_str_radix(hex, 16).ok()
}

fn enabled_config(soc: u8) -> SocLimitConfig {
    SocLimitConfig {
        enabled: true,
        soc,
        ..SocLimitConfig::default()
    }
}

/// 构造"已探测"的功能实例
fn probed(config: SocLimitConfig, io: &mut FakeIo) -> SocLimit {
    let mut a = SocLimit::new(config);
    let outcome = a.probe(io);
    assert!(outcome.is_supported(), "该假节点树应当被判定为支持");
    a
}

// ============================================================
//  ① 纯逻辑：控制字（对照真机实测表）
// ============================================================

#[test]
fn 控制字编码与hyperc实测表一致() {
    assert_eq!(logic::format_word(logic::encode(100)), "0x640011");
    assert_eq!(logic::format_word(logic::encode(90)), "0x5A0011");
    assert_eq!(logic::format_word(logic::encode(85)), "0x550011");
    assert_eq!(logic::format_word(logic::encode(80)), "0x500011");
    assert_eq!(logic::format_word(logic::encode(70)), "0x460011");
    assert_eq!(logic::format_word(logic::encode(60)), "0x3C0011");
}

#[test]
fn 使能位必须是0x11() {
    assert_eq!(logic::encode(80) & 0xFFFF, 0x11, "低 16 位必须含使能位");
    let (soc, flags) = logic::decode(logic::encode(87));
    assert_eq!(soc, 87);
    assert_eq!(flags, 0x11);
}

#[test]
fn 上限被钳到本机区间() {
    // 用本机检测出的区间（K90PM 实测 60~100）
    assert_eq!(logic::clamp_soc(10, 60, 100), 60);
    assert_eq!(logic::clamp_soc(60, 60, 100), 60);
    assert_eq!(logic::clamp_soc(100, 60, 100), 100);
    assert_eq!(logic::clamp_soc(200, 60, 100), 100);
}

#[test]
fn 上限按不同机型的区间钳位() {
    // ⚠️ 这是"不硬编码 60~100"的核心验证：
    //    某机型检测出 50~90，就必须按 50~90 钳，而不是 60~100。
    assert_eq!(logic::clamp_soc(55, 50, 90), 55, "50~90 机型允许 55%");
    assert_eq!(logic::clamp_soc(40, 50, 90), 50, "低于下限被抬到 50");
    assert_eq!(logic::clamp_soc(95, 50, 90), 90, "高于上限被压到 90");
}

#[test]
fn 荒谬区间退回兜底() {
    // 反向区间（min > max）不应导致"钳到不可能的值"
    let v = logic::clamp_soc(80, 90, 50);
    assert!(
        (60..=100).contains(&v),
        "区间反了应退回兜底 60~100，实得 {v}"
    );
    // 超出物理范围的区间接物理护栏
    assert_eq!(
        logic::clamp_soc(80, 0, 200),
        80,
        "80 在任何合法区间内都应保持"
    );
}

#[test]
fn 兜底钳位与物理常量一致() {
    // 兜底下限 = 55（2026-10-19 真机 dmesg 实测：
    //   `Charge control end_threshold should be within [55 100]`）
    assert_eq!(logic::clamp_soc_default(10), 55);
    assert_eq!(logic::clamp_soc_default(80), 80);
    assert_eq!(logic::clamp_soc_default(200), 100);
}

#[test]
fn 生效值解析() {
    assert_eq!(logic::parse_active("80\n"), Some(80));
    assert_eq!(logic::parse_active(" 90 "), Some(90));
    assert_eq!(
        logic::parse_active("0"),
        Some(0),
        "0 是驱动特殊态，仍可解析"
    );
    assert_eq!(logic::parse_active("abc"), None);
    assert_eq!(logic::parse_active(""), None);
    assert_eq!(logic::parse_active("-1"), None);
}

#[test]
fn 是否需要写回() {
    assert!(!logic::needs_rewrite(80, Some(80)), "已一致 → 不写");
    assert!(logic::needs_rewrite(80, Some(90)), "被改过 → 写回");
    assert!(logic::needs_rewrite(80, Some(0)), "特殊态 → 写回");
    assert!(logic::needs_rewrite(80, None), "读不到 → 尝试写回");
}

// ============================================================
//  ② 行为：未启用必须"零写入"
// ============================================================

#[test]
fn 未启用时一个字都不写() {
    let mut io = FakeIo::hyperos();
    // 把当前生效值改成 90，制造"与目标不一致"的情形
    io.files.insert(READ.to_string(), "90".to_string());

    let mut addon = probed(SocLimitConfig::default(), &mut io); // 默认 enabled=false
    let outcome = addon.tick(&mut io, 0);

    assert!(!outcome.wrote, "未启用时不得写节点");
    assert!(outcome.is_ok());
    assert_eq!(io.writes(), 0, "写入日志必须为空");
    assert_eq!(
        io.files.get(READ).map(String::as_str),
        Some("90"),
        "现有值不得被改动"
    );
}

#[test]
fn release是空操作() {
    let mut io = FakeIo::hyperos();
    let mut addon = probed(enabled_config(80), &mut io);
    let outcome = addon.release(&mut io);
    assert!(!outcome.wrote);
    assert_eq!(io.writes(), 0, "决策 B：停止干预 = 零写入");
}

// ============================================================
//  ②.5 省电：未充电时跳过巡检
// ============================================================

#[test]
fn 未充电时不巡检也不写() {
    // 制造"与目标不一致"的情形 —— 若它巡检就一定会写
    let mut io = FakeIo::hyperos().unplugged();
    io.files.insert(READ.to_string(), "90".to_string());

    let mut addon = probed(enabled_config(80), &mut io);
    let outcome = addon.tick(&mut io, 0);

    assert!(
        !outcome.wrote,
        "未充电时不该写节点 —— 没插电时限制值是多少都无所谓"
    );
    assert!(outcome.is_ok(), "这不算失败，只是跳过");
    assert_eq!(io.writes(), 0, "一次写入都不该发生");
    assert_eq!(
        io.files.get(READ).map(String::as_str),
        Some("90"),
        "现有值不得被改动"
    );
}

#[test]
fn 插电后又开始巡检并修正() {
    let mut io = FakeIo::hyperos().unplugged();
    io.files.insert(READ.to_string(), "90".to_string());

    let mut addon = probed(enabled_config(80), &mut io);

    // 未充电：跳过
    addon.tick(&mut io, 0);
    assert_eq!(io.writes(), 0, "未充电时应跳过");

    // 插上充电器：应当立即发现不一致并修回
    io.plugged = true;
    let outcome = addon.tick(&mut io, 0);

    assert!(outcome.wrote, "插电后应写回");
    assert_eq!(io.writes(), 1);
    assert_eq!(
        io.files.get(READ).map(String::as_str),
        Some("80"),
        "应把生效值修回目标 80"
    );
}

#[test]
fn 已充满仍算插着继续巡检() {
    // `Full` 是"插着但已充满"，必须继续巡检 ——
    // 否则用户在满电时改上限不会被同步
    let mut io = FakeIo::hyperos();
    io.files.insert(READ.to_string(), "90".to_string());
    assert!(io.plugged, "FakeIo 默认 plugged=true，模拟 Full/Charging");

    let mut addon = probed(enabled_config(80), &mut io);
    let outcome = addon.tick(&mut io, 0);
    assert!(outcome.wrote, "已充满（仍插着）时应照常巡检");
}

// ============================================================
//  ③ 行为：启用后的写回与校验
// ============================================================

#[test]
fn 已一致时不写() {
    let mut io = FakeIo::hyperos(); // 生效值已是 80
    let mut addon = probed(enabled_config(80), &mut io);

    let outcome = addon.tick(&mut io, 0);
    assert!(!outcome.wrote, "已一致 → 不该写");
    assert_eq!(io.writes(), 0);
}

#[test]
fn 不一致时写回并回读通过() {
    let mut io = FakeIo::hyperos(); // 生效值 80
    let mut addon = probed(enabled_config(85), &mut io);

    let outcome = addon.tick(&mut io, 0);

    assert!(outcome.wrote, "不一致 → 应当写回");
    assert!(outcome.is_ok(), "回读应当通过：{:?}", outcome.error);
    assert_eq!(io.writes(), 1);
    assert_eq!(io.write_log[0].1, "0x550011", "写的是控制字，不是十进制值");
    assert_eq!(addon.last_seen(), Some(85));
    assert_eq!(io.files.get(READ).map(String::as_str), Some("85"));
}

#[test]
fn 写的是smart_chg而非soc_limit() {
    let mut io = FakeIo::hyperos();
    let mut addon = probed(enabled_config(70), &mut io);
    addon.tick(&mut io, 0);

    assert_eq!(io.write_log[0].0, CTRL, "必须写控制节点");
    assert!(
        !io.write_log.iter().any(|(p, _)| p == READ),
        "绝不能直接写 soc_limit（会被状态机覆盖）"
    );
}

#[test]
fn 回读不符时报告错误() {
    let mut io = FakeIo::hyperos();
    io.write_ignored = true; // 写了不生效
    let mut addon = probed(enabled_config(85), &mut io);

    let outcome = addon.tick(&mut io, 0);

    assert!(outcome.wrote, "确实尝试写了");
    assert!(outcome.error.is_some(), "回读不符必须报错");
    assert_eq!(addon.write_count(), 1);
}

// ============================================================
//  ⑩ 写入方冲突识别（2026-10-16）
//
//  `smart_chg` 是全局节点 —— 任何写它的程序都在改同一个开关。
//  两个程序同时启用时会互相覆盖，表现为**连续多次**写下去就被改回。
// ============================================================

/// 单次回读不符**不能**断言冲突（否则会误报成"有别的程序在抢"）
///
/// 驱动可能延迟一拍才把控制字应用到 `soc_limit` 上 ——
/// 凭一次不符就让用户去卸载另一个模块，是错误的指引。
#[test]
fn 单次回读不符不提示冲突() {
    let mut io = FakeIo::hyperos();
    io.write_ignored = true;
    let mut addon = probed(enabled_config(85), &mut io);

    let outcome = addon.tick(&mut io, 0);
    let msg = outcome
        .error
        .as_ref()
        .map(|e| e.msg.clone())
        .unwrap_or_default();

    assert!(msg.contains("回读不符"), "应当报回读不符，实得：{msg}");
    assert!(
        !msg.contains("另一个程序"),
        "只失败一次不该断言有冲突，实得：{msg}"
    );
}

/// 连续两次写下去就被改回 ⇒ 提示疑似写入方冲突
///
/// 这正是"两个程序抢同一个节点"的表现：谁在后面谁生效，
/// 于是我们每拍都写、每拍都被改回去。
#[test]
fn 连续回读不符提示疑似冲突() {
    let mut io = FakeIo::hyperos();
    io.write_ignored = true;
    let mut addon = probed(enabled_config(85), &mut io);

    let _ = addon.tick(&mut io, 0); // 第 1 次：只说回读不符
    let second = addon.tick(&mut io, 15); // 第 2 次：应带上冲突提示

    let msg = second
        .error
        .as_ref()
        .map(|e| e.msg.clone())
        .unwrap_or_default();
    assert!(
        msg.contains("另一个程序"),
        "连续两次不符应当提示疑似冲突，实得：{msg}"
    );
    // ⚠️ 这条断言是**防回归守卫**：锁定"用户可见的文案里不得出现
    //    第三方模块名"。下面两个是历史品牌名 —— 它们只出现在断言里，
    //    产品代码与任何输出字段中都不得再有。
    assert!(
        !msg.contains("Hyper") && !msg.contains("hypersc"),
        "提示不应指名具体模块，实得：{msg}"
    );
}

/// 关掉提示开关后不再追加冲突提示（但错误本身照报）
#[test]
fn 关掉开关后不提示冲突() {
    let mut io = FakeIo::hyperos();
    io.write_ignored = true;
    let cfg = SocLimitConfig {
        warn_on_conflict: false,
        ..enabled_config(85)
    };
    let mut addon = probed(cfg, &mut io);

    let _ = addon.tick(&mut io, 0);
    let second = addon.tick(&mut io, 15);
    let msg = second
        .error
        .as_ref()
        .map(|e| e.msg.clone())
        .unwrap_or_default();

    assert!(msg.contains("回读不符"), "错误本身仍要报，实得：{msg}");
    assert!(!msg.contains("另一个程序"), "关掉后不该提示，实得：{msg}");
}

/// 一旦写入生效，冲突计数必须清零
///
/// 否则"很久以前连续失败过两次"会让后续每一次偶发不符都顶着冲突提示，
/// 用户会以为冲突一直存在。
#[test]
fn 写入成功后冲突计数清零() {
    let mut io = FakeIo::hyperos();
    io.write_ignored = true;
    let mut addon = probed(enabled_config(85), &mut io);

    let _ = addon.tick(&mut io, 0);
    let _ = addon.tick(&mut io, 15); // 计数到 2

    // 恢复正常：让写入真的生效
    io.write_ignored = false;
    let ok = addon.tick(&mut io, 30);
    assert!(ok.is_ok(), "这次应成功");

    // 再制造一次失败 —— 计数应已清零，因此**不**带冲突提示
    io.write_ignored = true;
    let after = addon.tick(&mut io, 45);
    let msg = after
        .error
        .as_ref()
        .map(|e| e.msg.clone())
        .unwrap_or_default();
    assert!(!msg.contains("另一个程序"), "计数应已清零，实得：{msg}");
}

#[test]
fn 驱动特殊态零值时会被写回() {
    let mut io = FakeIo::hyperos();
    io.files.insert(READ.to_string(), "0".to_string()); // 驱动特殊态
    let mut addon = probed(enabled_config(80), &mut io);

    let outcome = addon.tick(&mut io, 0);
    assert!(outcome.wrote, "读到 0 应当视为需要写回");
    assert_eq!(io.files.get(READ).map(String::as_str), Some("80"));
}

#[test]
fn 值域外的配置被拒绝且不写节点() {
    let mut io = FakeIo::hyperos();
    let mut addon = probed(enabled_config(80), &mut io);
    addon.set_config(SocLimitConfig {
        enabled: true,
        soc: 30, // 越界
        ..SocLimitConfig::default()
    });

    let outcome = addon.tick(&mut io, 0);
    assert!(outcome.error.is_some(), "越界配置必须被拒绝（fail-closed）");
    assert_eq!(io.writes(), 0, "坏配置绝不拿去写节点");
}

// ============================================================
//  ④ 探测
// ============================================================

#[test]
fn 节点不存在时判定为不支持() {
    let mut io = FakeIo::default(); // 空节点树
    let mut addon = SocLimit::new(SocLimitConfig::default());
    let outcome = addon.probe(&mut io);

    assert!(!outcome.is_supported());
    assert!(matches!(outcome, ProbeOutcome::Unsupported { .. }));
    assert!(addon.node_ctrl().is_none());
}

// ============================================================
//  ⑨ 使用前提（用户要求 2026-10-16 新增）
//
//  「加上提示：此模块需打开电池健康保护功能」
// ============================================================

/// 功能必须**自报使用前提**，且界面与 CLI 拿的是同一份文案
///
/// # 为什么这是个必须锁死的契约
///
/// `smart_charge/*` 这组节点属于系统「电池保护」功能的实现 ——
/// 系统设置里的「智能断充」（默认 80%）改的就是同一个节点。
/// 开关关掉时驱动是否仍执行 SOC 限制**不可依赖**。
///
/// 这个前提如果只写在 README 里，用户看不到（多数人是直接刷模块）；
/// 只写在前端里，`limit show` 又看不到。因此必须在 `spec()` 里，
/// 由服务端展开给两边共用。
#[test]
fn 功能自报使用前提() {
    let addon = SocLimit::new(SocLimitConfig::default());
    let requires = addon.spec().requires.expect("充电上限必须有使用前提");

    // 必须点名那个系统开关 —— 只说"需打开某功能"等于没说
    assert!(
        requires.contains("电池健康保护") || requires.contains("电池保护"),
        "前提文案要点名系统开关，实得：{requires}"
    );
    // 必须给出**路径**，否则用户不知道去哪找
    assert!(
        requires.contains("省电与电池"),
        "前提文案要给出设置路径，实得：{requires}"
    );
}

/// `status().requires` 必须与 `spec().requires` **逐字一致**
///
/// 服务端展开一份、前端再硬编码一份的话，迟早有一天会对不上 ——
/// 而对不上时，用户看到的提示与 CLI 输出的提示互相矛盾。
#[test]
fn 状态里的前提与自我介绍一致() {
    let mut io = FakeIo::hyperos();
    let addon = probed(SocLimitConfig::default(), &mut io);

    let spec = addon.spec();
    let status = addon.status();

    assert_eq!(
        status.requires.as_deref(),
        spec.requires,
        "status().requires 必须与 spec().requires 同源"
    );
}

/// 系统界面不会同步 —— 必须主动说明
///
/// 系统「电池保护」设置页有自己的界面文案；本模块是纯 root + sysfs，
/// **没有 hook 层**，改不了那个界面。
///
/// 不说清楚的话：用户设了 90%、进系统设置一看仍写着 80%，
/// 会判断成"没生效" —— 而实际上限制**已经生效**。
/// 这是"看起来失败其实成功了"的典型，必须在界面上说透。
#[test]
fn 说明系统界面不会同步() {
    let mut io = FakeIo::hyperos();
    let addon = probed(SocLimitConfig::default(), &mut io);
    let note = addon.status().detail["note_ui_not_synced"]
        .as_str()
        .expect("必须提供「系统界面不会同步」的说明")
        .to_string();

    assert!(
        note.contains("不会") || note.contains("以本页"),
        "说明要点出「系统显示不会同步」，实得：{note}"
    );
}

/// 探测失败时**先让用户查系统开关**，而不是一句"机型不支持"
///
/// 两种原因（开关没开 / 机型不支持）表象完全相同，但处理成本差一个
/// 数量级：前者花 10 秒，后者无解。若只写"本机型不支持"，
/// 绝大多数用户会直接放弃 —— 而他们很可能只是少开了一个开关。
#[test]
fn 探测失败时优先提示检查系统开关() {
    let mut io = FakeIo::default(); // 空节点树
    let mut addon = SocLimit::new(SocLimitConfig::default());
    let outcome = addon.probe(&mut io);

    let msg = outcome.message();
    assert!(
        msg.contains("电池保护"),
        "失败原因里要先提示检查系统开关，实得：{msg}"
    );
    // 但也不能只提开关 —— 真不支持的机型也要能得出结论
    assert!(
        msg.contains("机型") || msg.contains("不支持"),
        "也要保留「机型可能不支持」的判断依据，实得：{msg}"
    );
}

/// `soc_in_effect` 如实反映"目标与驱动的生效值是否一致"
///
/// ⚠️ 判据必须是读 `soc_limit` 得到的值 —— `smart_chg` 的读值在
///    16/17/80 之间漂移，与写入值无稳定对应关系（真机实测确证）。
#[test]
fn 生效判据取自soc_limit读数() {
    let mut io = FakeIo::hyperos(); // 生效值当前是 80
    let addon = probed(enabled_config(80), &mut io);

    // 未经 tick，last_seen 还是 None ⇒ 不能谎报"已生效"
    let s = addon.status();
    assert_eq!(s.detail["soc_target"], 80);
    assert_eq!(s.detail["soc_in_effect"], false, "还没读到就不该说已生效");
}

#[test]
fn 命中候选路径并推出读取节点() {
    let mut io = FakeIo::hyperos();
    let mut addon = SocLimit::new(SocLimitConfig::default());
    addon.probe(&mut io);

    assert_eq!(addon.node_ctrl(), Some(CTRL));
    assert_eq!(
        addon.node_read(),
        Some(READ),
        "读取节点取同目录的 soc_limit"
    );
}

// ============================================================
//  ⑦ 标准 `power_supply` 框架（2026-10-19 新增支持）
//
//  这类机型**没有** `smart_chg`、**没有** `soc_limit`，只有上游内核的
//  `charge_control_end_threshold`（纯百分比）。三条差异都必须成立：
//
//  | # | 差异 | 弄错的后果 |
//  |---|---|---|
//  | 1 | 探测要能命中它 | 界面直接报"本机不支持" |
//  | 2 | 写入要用纯百分比 | 写 `0x500011` 进去，驱动当成非法值 |
//  | 3 | 生效值读控制节点自己 | 读不存在的 `soc_limit` ⇒ 每拍失败 |
// ============================================================

/// 标准框架的节点必须被探测到，且生效值就是控制节点自己
#[test]
fn 标准框架命中百分比节点() {
    let mut io = FakeIo::standard_psys();
    let mut addon = SocLimit::new(SocLimitConfig::default());
    let outcome = addon.probe(&mut io);

    assert!(
        outcome.is_supported(),
        "标准框架的 charge_control_end_threshold 应被认作支持：{}",
        outcome.message()
    );
    assert_eq!(addon.node_ctrl(), Some(STD_CTRL));
    assert_eq!(
        addon.node_read(),
        Some(STD_CTRL),
        "标准框架下控制节点自己就是生效值（没有同目录的 soc_limit 可读）"
    );
}

/// 写入必须是**纯百分比**，不能是小米的控制字
#[test]
fn 标准框架写入纯百分比() {
    let mut io = FakeIo::standard_psys();
    let mut addon = probed(enabled_config(80), &mut io);

    let outcome = addon.tick(&mut io, 0);

    assert!(outcome.wrote, "当前 100%，目标 80% ⇒ 应当写");
    assert!(
        outcome.is_ok(),
        "回读应当通过（读的是控制节点自己）：{:?}",
        outcome.error
    );
    assert_eq!(io.write_log.len(), 1);
    assert_eq!(
        io.write_log[0].1, "80",
        "标准框架要写纯百分比，不能写小米的控制字"
    );
    assert_eq!(
        io.files.get(STD_CTRL).map(String::as_str),
        Some("80"),
        "生效值应已变为 80"
    );
    assert_eq!(addon.last_seen(), Some(80));
}

/// 已一致 → 零写入（标准框架同样适用）
#[test]
fn 标准框架已一致时不写() {
    let mut io = FakeIo::standard_psys();
    io.files.insert(STD_CTRL.to_string(), "80".to_string());
    let mut addon = probed(enabled_config(80), &mut io);

    let outcome = addon.tick(&mut io, 0);

    assert!(!outcome.wrote, "已一致不该写");
    assert_eq!(io.writes(), 0);
}

/// 标准框架的前提文案**不得**让人去找「电池健康保护」开关
///
/// # 为什么这条要单独锁
///
/// `charge_control_end_threshold` 是上游内核的通用接口，**不受**系统
/// 「电池健康保护」开关控制。照抄小米那句会带来两种坏结果：
///
/// | 用户的机型 | 会发生什么 |
/// |---|---|
/// | 设置里**有**那个开关 | 白折腾一趟（本来就没关） |
/// | 设置里**没有**那个开关 | 以为缺了前提，白白放弃功能 |
#[test]
fn 标准框架的前提文案不点名电池健康保护() {
    let mut io = FakeIo::standard_psys();
    let mut addon = SocLimit::new(SocLimitConfig::default());
    addon.probe(&mut io);

    let requires = addon
        .spec()
        .requires
        .expect("充电上限必须有使用前提（哪怕是「无需设置」）");

    assert!(
        requires.contains("标准"),
        "要点出本机走的是标准接口，实得：{requires}"
    );
    assert!(
        !requires.contains("电池健康保护"),
        "标准框架不归「电池健康保护」开关管，不能叫用户去打开它，实得：{requires}"
    );
    // 小米方言仍要保留原来的提示（防回归：别把两边都改掉）
    let mut hyper_io = FakeIo::hyperos();
    let mut hyper = SocLimit::new(SocLimitConfig::default());
    hyper.probe(&mut hyper_io);
    let hyper_requires = hyper.spec().requires.expect("小米方言必须有使用前提");
    assert!(
        hyper_requires.contains("电池健康保护"),
        "小米方言仍要说清那个开关，实得：{hyper_requires}"
    );
}

// ============================================================
//  ⑧ 使能节点（2026-10-19 K70 Pro 真机踩坑，最贵的一个）
//
//  「充电上限完全不可用，界面报不支持」的**真正原因**：
//  `charge_control_en = 0` 时写 `charge_control_end_threshold` 返回
//  成功但值被吞 ⇒ 回读永远 0 ⇒ 连续三次判"有程序在抢"⇒ 熔断。
//
//  ⇒ 必须：写目标值**之前**先把使能位置 1。
// ============================================================

/// 探测时必须认出使能节点（否则 tick 里无从开它）
#[test]
fn 探测能识别使能节点() {
    let mut io = FakeIo::k70pro();
    let mut addon = SocLimit::new(SocLimitConfig::default());
    let outcome = addon.probe(&mut io);

    assert!(outcome.is_supported());
    assert!(
        outcome.message().contains("使能"),
        "探测结果应交代使能节点，实得：{}",
        outcome.message()
    );
}

/// **核心回归**：使能位为 0 时，写入必须先把使能位置 1，才能生效
///
/// # 不修这条会发生什么（真机实测）
///
/// ```text
/// 写 threshold=95  → write() 返回 Ok
/// 读回             → 0
/// 模块判断          → "回读不符" ×3
/// 结果             → 误报"有另一个程序在抢" + 熔断
/// 界面             → 充电上限不可用
/// ```
#[test]
fn 使能位为0时先开使能再写阈值() {
    let mut io = FakeIo::k70pro();
    let mut addon = probed(enabled_config(95), &mut io);

    let outcome = addon.tick(&mut io, 0);

    // ① 必须写成功，且回读通过（不该再报"回读不符"）
    assert!(
        outcome.is_ok(),
        "开了使能位之后写入应当生效：{:?}",
        outcome.error
    );
    assert_eq!(
        io.files.get(STD_EN).map(String::as_str),
        Some("1"),
        "必须把使能位打开"
    );
    assert_eq!(
        io.files.get(STD_CTRL).map(String::as_str),
        Some("95"),
        "阈值应真正写入（不再是 0）"
    );

    // ② 写入顺序：先使能、后阈值
    assert_eq!(
        io.write_log[0].0, STD_EN,
        "第一次写必须是使能节点（否则阈值会被吞掉）"
    );
    assert_eq!(io.write_log[1].0, STD_CTRL, "第二次写才是阈值");
    assert_eq!(addon.last_seen(), Some(95));
}

/// 已经开过使能 → 后续拍不重复写它（sysfs 写有 I2C 开销）
#[test]
fn 使能位只写一次() {
    let mut io = FakeIo::k70pro();
    let mut addon = probed(enabled_config(95), &mut io);

    addon.tick(&mut io, 0); // 开使能 + 写 95
    let after_first = io.writes();

    // 改成 90 → 再跑一拍
    addon.set_config(enabled_config(90));
    addon.tick(&mut io, 15);

    let en_writes = io.write_log.iter().filter(|(p, _)| p == STD_EN).count();
    assert_eq!(en_writes, 1, "使能节点只该写一次，实得 {en_writes} 次");
    assert_eq!(io.writes(), after_first + 1, "第二拍只写阈值");
    assert_eq!(io.files.get(STD_CTRL).map(String::as_str), Some("90"));
}

/// 使能位本来就为 1 → 不写它（省一次 I2C）
#[test]
fn 使能位已为1时不再写() {
    let mut io = FakeIo::k70pro();
    io.files.insert(STD_EN.to_string(), "1".to_string());
    let mut addon = probed(enabled_config(95), &mut io);

    let outcome = addon.tick(&mut io, 0);

    assert!(outcome.is_ok());
    assert!(
        !io.write_log.iter().any(|(p, _)| p == STD_EN),
        "已经是 1 就不该再写：{:?}",
        io.write_log
    );
    assert_eq!(io.files.get(STD_CTRL).map(String::as_str), Some("95"));
}

/// 关闭功能时必须把使能位**关回去**（否则"关了但没关"）
///
/// # 为什么这条重要
///
/// 小米那套 `release` 是零写入（不写就保持原状）。但标准框架的
/// `charge_control_en` 是**我们主动打开的** —— 不关它，用户关掉功能后
/// 驱动仍在执行我们上次写入的阈值。
#[test]
fn 关闭功能时把使能位关回去() {
    let mut io = FakeIo::k70pro();
    let mut addon = probed(enabled_config(95), &mut io);

    addon.tick(&mut io, 0); // 开使能
    assert_eq!(io.files.get(STD_EN).map(String::as_str), Some("1"));

    addon.release(&mut io);
    assert_eq!(
        io.files.get(STD_EN).map(String::as_str),
        Some("0"),
        "关闭功能必须把使能位交还系统"
    );
}

/// 从未开过使能 → `release` 仍是零写入（不能凭空写一个 0）
#[test]
fn 未开过使能时释放不写() {
    let mut io = FakeIo::k70pro();
    let mut addon = probed(enabled_config(95), &mut io);

    // 不 tick，直接 release
    let outcome = addon.release(&mut io);

    assert!(!outcome.wrote, "没开过就不该写");
    assert_eq!(io.writes(), 0);
}

/// 没有使能节点的机型（`standard_psys`）行为不变
///
/// ⚠️ 防回归：别把"必须开使能"当成所有标准框架的硬要求 ——
///    有的机型**只有** threshold 节点，没有 en。
#[test]
fn 没有使能节点的机型照常工作() {
    let mut io = FakeIo::standard_psys();
    let mut addon = probed(enabled_config(80), &mut io);

    let outcome = addon.tick(&mut io, 0);

    assert!(outcome.is_ok(), "无使能节点时也该正常：{:?}", outcome.error);
    assert_eq!(
        io.write_log.len(),
        1,
        "只写阈值，不该试图写不存在的使能节点"
    );
    assert_eq!(io.files.get(STD_CTRL).map(String::as_str), Some("80"));
}

#[test]
fn 首条候选不可写时回退到第二条() {
    let alt = "/sys/class/charger/smart_charge/smart_chg";
    let mut io = FakeIo::default();
    io.files.insert(alt.to_string(), "80".to_string());
    io.writable.push(alt.to_string());

    let mut addon = SocLimit::new(SocLimitConfig::default());
    let outcome = addon.probe(&mut io);

    assert!(outcome.is_supported());
    assert_eq!(addon.node_ctrl(), Some(alt));
}

#[test]
fn 未探测就启用时报节点缺失而不是崩溃() {
    let mut io = FakeIo::hyperos();
    let mut addon = SocLimit::new(enabled_config(80)); // 未 probe
    let outcome = addon.tick(&mut io, 0);

    assert!(outcome.error.is_some());
    assert_eq!(io.writes(), 0);
}

// ============================================================
//  ⑤ 调度与熔断
// ============================================================

#[test]
fn 调度器只跑到点的功能() {
    let mut io = FakeIo::hyperos();
    let addon = probed(enabled_config(85), &mut io);
    let mut runner = AddonRunner::new(vec![AddonSlot::new(Box::new(addon))]);
    runner.probe_all(&mut io);

    let ran = runner.tick_due(&mut io, 0);
    assert_eq!(ran.len(), 1, "首次应当跑");
    assert_eq!(io.writes(), 1);

    let ran2 = runner.tick_due(&mut io, 1);
    assert!(ran2.is_empty(), "没到点不该跑");
    assert_eq!(io.writes(), 1, "不该重复写");
}

#[test]
fn 连续失败后熔断且不再被调用() {
    let mut io = FakeIo::hyperos();
    io.write_ignored = true; // 永远写不生效 → 每拍都回读不符

    let addon = probed(enabled_config(85), &mut io);
    let mut runner = AddonRunner::new(vec![AddonSlot::new(Box::new(addon))]);
    runner.probe_all(&mut io);

    let mut now = 0;
    for _ in 0..TRIP_AFTER_FAILURES {
        runner.tick_due(&mut io, now);
        now += 20;
    }

    let slot = runner.slot("soc_limit").expect("插槽应存在");
    assert!(slot.is_tripped(), "连续失败应触发熔断");
    assert_eq!(slot.consecutive_failures(), TRIP_AFTER_FAILURES);

    let writes_before = io.writes();
    runner.tick_due(&mut io, now + 1_000);
    assert_eq!(io.writes(), writes_before, "熔断后不得再写");
}

#[test]
fn 成功一拍后失败计数清零() {
    let mut io = FakeIo::hyperos();
    let addon = probed(enabled_config(85), &mut io);
    let mut runner = AddonRunner::new(vec![AddonSlot::new(Box::new(addon))]);
    runner.probe_all(&mut io);

    runner.tick_due(&mut io, 0);
    let slot = runner.slot("soc_limit").expect("插槽应存在");
    assert!(!slot.is_tripped());
    assert_eq!(slot.consecutive_failures(), 0);
    assert_eq!(slot.writes(), 1);
}

#[test]
fn 熔断可手动解除() {
    let mut io = FakeIo::hyperos();
    io.write_ignored = true;
    let addon = probed(enabled_config(85), &mut io);
    let mut runner = AddonRunner::new(vec![AddonSlot::new(Box::new(addon))]);
    runner.probe_all(&mut io);

    let mut now = 0;
    for _ in 0..TRIP_AFTER_FAILURES {
        runner.tick_due(&mut io, now);
        now += 20;
    }
    let slot = runner.slot_mut("soc_limit").expect("插槽应存在");
    assert!(slot.recover(), "此前应在熔断状态");
    assert!(!slot.is_tripped());
}

#[test]
fn 本机不支持时不算失败也不熔断() {
    let mut io = FakeIo::default(); // 空节点树 → 不支持
    let addon = SocLimit::new(enabled_config(80));
    let mut runner = AddonRunner::new(vec![AddonSlot::new(Box::new(addon))]);
    runner.probe_all(&mut io);

    let ran = runner.tick_due(&mut io, 0);
    assert!(ran.is_empty(), "不支持的功能不该跑");

    let slot = runner.slot("soc_limit").expect("插槽应存在");
    assert!(!slot.is_tripped(), "不支持 ≠ 失败");
    assert_eq!(slot.errors(), 0);
}

// ============================================================
//  ⑥ 调度及时性
//
//  用户点了「启用充电上限」后必须**尽快**生效，而不是等满一个旧周期
//  （未启用时是 60 秒）—— 否则体验上就是"按了没反应"。
//  真机实测发现过这个问题，因此固化为测试。
// ============================================================

#[test]
fn 配置变更后可立即重新调度() {
    let mut io = FakeIo::hyperos();
    io.files.insert(READ.to_string(), "80".to_string());

    let addon = probed(enabled_config(85), &mut io);
    let mut runner = AddonRunner::new(vec![AddonSlot::new(Box::new(addon))]);
    runner.probe_all(&mut io);

    // 先跑一拍，把 next_due 推到未来
    runner.tick_due(&mut io, 0);
    assert_eq!(io.writes(), 1);

    // 未到点 → 不该跑
    assert!(runner.tick_due(&mut io, 1).is_empty(), "未到点不该跑");

    // 重新调度后，**同一时刻**就应当可以再跑
    runner
        .slot_mut("soc_limit")
        .expect("插槽应存在")
        .reschedule_now();
    let ran = runner.tick_due(&mut io, 1);
    assert_eq!(ran.len(), 1, "重新调度后应立即到期");
}

#[test]
fn 解除熔断后立即安排下一拍() {
    let mut io = FakeIo::hyperos();
    io.write_ignored = true; // 永远写不生效 → 会熔断

    let addon = probed(enabled_config(85), &mut io);
    let mut runner = AddonRunner::new(vec![AddonSlot::new(Box::new(addon))]);
    runner.probe_all(&mut io);

    let mut now = 0;
    for _ in 0..TRIP_AFTER_FAILURES {
        runner.tick_due(&mut io, now);
        now += 20;
    }
    assert!(runner.slot("soc_limit").expect("插槽应存在").is_tripped());

    // 解除熔断 → 应立即到期（不必再等一个周期）
    let mut io2 = FakeIo::hyperos();
    let slot = runner.slot_mut("soc_limit").expect("插槽应存在");
    assert!(slot.recover());
    let ran = runner.tick_due(&mut io2, now);
    assert_eq!(ran.len(), 1, "解除熔断后应立即重试");
}
