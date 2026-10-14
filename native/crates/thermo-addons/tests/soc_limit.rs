//! 充电上限功能的离线测试（**用假的节点树，不碰真机**）
//!
//! 覆盖三层：
//!
//! | 层 | 内容 |
//! |---|---|
//! | 纯逻辑 | 控制字编解码（对照 HyperSC 逆向出的对照表） |
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
//  ① 纯逻辑：控制字（对照 HyperSC 真机实测表）
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
    assert_eq!(logic::clamp_soc_default(10), 60);
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
