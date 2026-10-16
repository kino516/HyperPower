//! 充电上限的纯逻辑（无 I/O、无状态）—— **可离线测试**
//!
//! # 机制来源：真机实测
//!
//! 下面每一条都是在这台设备上一条一条试出来的，不是从别处抄的：
//!
//! | 事实 | 值 |
//! |---|---|
//! | 写入节点 | `smart_chg`（**写控制字**） |
//! | 读取节点 | 同目录 `soc_limit`（**读生效值**） |
//! | 控制字 | `(上限 << 16) \| 0x11`，十六进制字符串 |
//! | 使能位 | 低 16 位的 **bit0 + bit4**（`0x11`）；缺了不生效 |
//! | 值域 | 实测 `60 ~ 100`% |
//!
//! ⚠️ 两个必须记住的坑（真机实测确证）：
//!
//! 1. **不要写 `soc_limit`** —— 它是驱动的派生显示值，直接写会短暂生效后被状态机覆盖；
//! 2. **不要用 `smart_chg` 的读值判断生效** —— 其读值在 `16`/`17`/`80` 之间漂移，
//!    与写入值无稳定对应关系。**判断生效永远读 `soc_limit`**。

/// 控制字的使能位（低 16 位 bit0 + bit4）
pub const ENABLE_FLAGS: u32 = 0x11;

/// 编码控制字：`(上限 << 16) | 0x11`
pub fn encode(limit: u8) -> u32 {
    (u32::from(limit) << 16) | ENABLE_FLAGS
}

/// 把控制字格式化为可写入节点的字符串（十六进制）
///
/// 驱动接受十六进制串（十进制也能解析，但十六进制与实测记录对得上，
/// 便于拿着 `cat` 的结果和日志互相印证）。
pub fn format_word(word: u32) -> String {
    format!("0x{word:X}")
}

/// 解码控制字 →（上限, 标志位）
pub fn decode(word: u32) -> (u8, u16) {
    (((word >> 16) & 0xFF) as u8, (word & 0xFFFF) as u16)
}

/// 把上限钳到**本机支持区间**
///
/// `min`/`max` 来自检测档案（`capabilities.soc_limit.{min,max}`）。
/// 用 [`clamp_soc_default`] 可在未知本机范围时退回兜底区间。
pub fn clamp_soc(v: u8, min: u8, max: u8) -> u8 {
    // 物理护栏：防调用方传入荒谬范围
    let min = min.clamp(
        super::config::SOC_PHYSICAL_MIN,
        super::config::SOC_PHYSICAL_MAX,
    );
    let max = max.clamp(
        super::config::SOC_PHYSICAL_MIN,
        super::config::SOC_PHYSICAL_MAX,
    );
    if min > max {
        return clamp_soc_default(v);
    }
    v.clamp(min, max)
}

/// 用**兜底区间**钳位（检测尚未跑过时）
pub fn clamp_soc_default(v: u8) -> u8 {
    v.clamp(
        super::config::SOC_FALLBACK_MIN,
        super::config::SOC_FALLBACK_MAX,
    )
}

/// 从节点文本解析出"当前生效上限"
///
/// 返回 `None` 表示读到的不是合法数值（含读到 `0` 这种驱动特殊态），
/// 调用方应视作"需要重写"。
pub fn parse_active(raw: &str) -> Option<u8> {
    let v: u32 = raw.trim().parse().ok()?;
    u8::try_from(v).ok().filter(|x| *x <= 100)
}

/// 判断是否需要重写节点
///
/// | 期望 | 读到 | 结果 |
/// |---|---|---|
/// | 80 | `Some(80)` | `false`（已一致，零写入） |
/// | 80 | `Some(90)` | `true`（被改过，写回） |
/// | 80 | `Some(0)` | `true`（驱动特殊态，写回） |
/// | 80 | `None` | `true`（读不到，尝试写回） |
pub fn needs_rewrite(expected: u8, actual: Option<u8>) -> bool {
    actual != Some(expected)
}

/// 该功能的"启动写入"是否需要：由未生效态进入生效态
///
/// 未插充电器时驱动会记录控制字但**不执行**限充（dmesg: `adapter is plugout`），
/// 因此写入是安全的，插电后自动生效。
pub fn should_apply(enabled: bool, actual: Option<u8>, target: u8) -> bool {
    enabled && needs_rewrite(target, actual)
}
