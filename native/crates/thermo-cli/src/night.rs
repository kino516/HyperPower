//! 夜间充电模式：在指定时间段自动切换到指定模式，结束后恢复
//!
//! # 用户需求（2026-10-26）
//!
//! > 「增加一个新功能，夜间充电模式。在指定时间内自动开启对应的充电模式。
//! >   1、可以指定 XX 时间到 XX 时间段。
//! >   2、可指定开启保守、官方、极速、自定义模式。」
//!
//! # 语义（**必须可预期**）
//!
//! | 时刻 | 行为 |
//! |---|---|
//! | 进入窗口（如 22:00） | 记住当前模式 → 切到 `night.mode` |
//! | 离开窗口（如 07:00） | 切回进入前的模式 |
//! | 窗口内用户**手动**切换模式 | **尊重用户**：本窗口不再自动恢复（记录 `user_override`） |
//! | 进入时本来就在 `night.mode` | 不切换；离开时也不恢复（本来没变） |
//!
//! # 跨午夜
//!
//! `22:00 → 07:00` 是**跨天**窗口：`now >= start || now < end`。
//! `start == end` 视为**不生效**（0 长度窗口没有意义，且能避免"全天都在夜间"的
//! 误配置）。
//!
//! # 为什么状态要跨重启保存（`night.json`）
//!
//! 夜间切换是**持久化**的（`switch_mode` 会写 `config.json` 的 `mode`）。
//! 如果"进入前是什么模式"只存在内存：
//!
//! | 场景 | 后果 |
//! |---|---|
//! | 夜间 worker 重启（崩溃/系统回收） | 丢失 `prev_mode` ⇒ 早上无法恢复 |
//!
//! 因此把 `NightState` 存到数据目录的 `night.json`（**运行时状态，不是配置**，
//! 不占 [`crate::config_store::KEYS`] 的键位）。它不是观测数据，因此也不进
//! `runtime.json`（那个是"某一拍的快照"，会被覆盖）。
//!
//! # 只在 worker 的**全拍**里检查
//!
//! worker 未充电且无客户端时会深度休眠（不跑拍）。此时不检查时间是**正确的**：
//! 不控制充电时切换模式没有意义；插上充电器（全拍开始）后第一拍就会补上。
//!
//! # 时间来源
//!
//! 本地时间（设备时区），按"一天中的分钟"处理 —— 不涉及日期，
//! 因此时区切换/跨天不会出现负值或溢出。
#![allow(unsafe_code)]

use serde::{Deserialize, Serialize};
use std::path::Path;

/// 一天有多少分钟
pub const MINUTES_PER_DAY: u32 = 24 * 60;

/// 解析 `HH:MM`（也接受 `H:MM`）→ 一天中的分钟
///
/// 返回 `None` 表示格式非法（越界、非数字、缺冒号）。
pub fn parse_hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    let h: u32 = h.trim().parse().ok()?;
    let m: u32 = m.trim().parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some(h * 60 + m)
}

/// 分钟 → `HH:MM`（用于把配置规范化成两位）
pub fn format_hhmm(min: u32) -> String {
    let min = min.min(MINUTES_PER_DAY - 1);
    format!("{:02}:{:02}", min / 60, min % 60)
}

/// 是否处于窗口 `[start, end)` 内（本地时间分钟）
///
/// - `start < end`：同日窗口
/// - `start > end`：跨午夜窗口
/// - `start == end`：**不生效**
pub fn in_window(now_min: u32, start_min: u32, end_min: u32) -> bool {
    if start_min == end_min || start_min >= MINUTES_PER_DAY || end_min >= MINUTES_PER_DAY {
        return false;
    }
    // 防御：now 越界时按 0 处理（调用方不应传越界值）
    let now = now_min.min(MINUTES_PER_DAY - 1);
    if start_min < end_min {
        now >= start_min && now < end_min
    } else {
        now >= start_min || now < end_min
    }
}

/// 本地时间：一天中的分钟数
///
/// Android / Linux 用 `localtime_r`（读取设备时区）；非 Unix（本机测试）
/// 退化为 UTC —— 测试里应当用 [`NightState`] 的注入时钟，不要依赖这里的时区。
#[cfg(unix)]
pub fn local_minutes(now_s: i64) -> u32 {
    // SAFETY: `localtime_r` 把结果写进调用方提供的 `tm`，失败返回空指针；
    // 这里只读本进程的时区设置，不持有任何全局状态。
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let t: libc::time_t = now_s as libc::time_t;
    // SAFETY: 指针指向上面已初始化的栈变量；`t` 是合法的 `time_t`。
    let p = unsafe { libc::localtime_r(&t, &mut tm) };
    if p.is_null() {
        return utc_minutes(now_s);
    }
    ((tm.tm_hour as u32) * 60 + tm.tm_min as u32).min(MINUTES_PER_DAY - 1)
}

/// 非 Unix：按 UTC 计算（只用于测试/构建可移植性）
#[cfg(not(unix))]
pub fn local_minutes(now_s: i64) -> u32 {
    utc_minutes(now_s)
}

/// Unix 秒 → UTC 的"一天中分钟"
fn utc_minutes(now_s: i64) -> u32 {
    let secs = now_s.rem_euclid(86_400) as u32;
    (secs / 60).min(MINUTES_PER_DAY - 1)
}

/// 夜间模式状态（**跨重启保存**）
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NightState {
    /// 是否已经应用了夜间模式（窗口内）
    #[serde(default)]
    pub active: bool,
    /// 进入窗口前的模式（用于恢复）
    #[serde(default)]
    pub prev_mode: Option<String>,
    /// 本次是否由我们**实际切换**过
    ///
    /// 进入时本来就在 `night.mode` 则为 `false` —— 此时离开窗口也不恢复
    /// （本来就没变，恢复反而会把用户切走）。
    #[serde(default)]
    pub applied: bool,
    /// 用户在窗口内手动切换过模式（尊重用户，本窗口不再自动恢复）
    #[serde(default)]
    pub user_override: bool,
}

/// 一次检查的决策（**纯函数**的返回值，便于测试）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NightAction {
    /// 什么都不做
    None,
    /// 需要切换到 `night.mode`（`prev_mode` 已记进状态）
    Enter,
    /// 进入窗口时已经在 `night.mode`（无需切换）
    EnterAlready,
    /// 窗口内检测到用户手动切换（本窗口不再自动恢复）
    Override,
    /// 离开窗口，需要恢复到给定模式
    Leave(String),
    /// 离开窗口但无需恢复（用户覆盖过 / 本来就没切换）
    LeaveNoop,
}

/// 夜间状态机（**唯一**修改 [`NightState`] 的地方）
///
/// 输入：是否在窗口内、夜间目标模式、当前生效模式。
/// 输出：调用方需要执行的动作（切换/恢复/仅记录）。
pub fn transition(
    state: &mut NightState,
    in_win: bool,
    target: &str,
    current: &str,
) -> NightAction {
    if in_win {
        if !state.active {
            state.active = true;
            state.user_override = false;
            state.prev_mode = Some(current.to_string());

            if current == target {
                state.applied = false;
                NightAction::EnterAlready
            } else {
                state.applied = true;
                NightAction::Enter
            }
        } else if state.applied && !state.user_override && current != target {
            // 我们在窗口内切到了 target，现在 current 变了 ⇒ 用户手动切走
            state.user_override = true;
            NightAction::Override
        } else {
            NightAction::None
        }
    } else if state.active {
        state.active = false;

        // 只有"我们切过、用户没覆盖、且现在仍在夜间模式"才恢复
        let restore = if state.applied && !state.user_override && current == target {
            state.prev_mode.clone().filter(|p| p != current)
        } else {
            None
        };

        state.applied = false;
        state.prev_mode = None;
        state.user_override = false;

        match restore {
            Some(p) => NightAction::Leave(p),
            None => NightAction::LeaveNoop,
        }
    } else {
        NightAction::None
    }
}

/// 读取 `night.json`（不存在/损坏 ⇒ 默认状态，不报错）
pub fn load(path: &Path) -> NightState {
    let Ok(text) = std::fs::read_to_string(path) else {
        return NightState::default();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// 原子写入 `night.json`（tmp → rename；失败只影响跨重启恢复，不中断控制）
pub fn save(path: &Path, state: &NightState) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(state)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text.as_bytes())?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn 解析时间并规范化() {
        assert_eq!(parse_hhmm("22:00"), Some(22 * 60));
        assert_eq!(parse_hhmm("7:05"), Some(7 * 60 + 5));
        assert_eq!(parse_hhmm(" 23:59 "), Some(23 * 60 + 59));

        assert_eq!(parse_hhmm("24:00"), None, "小时越界");
        assert_eq!(parse_hhmm("12:60"), None, "分钟越界");
        assert_eq!(parse_hhmm("12"), None, "缺冒号");
        assert_eq!(parse_hhmm("ab:cd"), None, "非数字");
        assert_eq!(parse_hhmm(""), None);

        assert_eq!(format_hhmm(7 * 60 + 5), "07:05");
        assert_eq!(format_hhmm(22 * 60), "22:00");
    }

    #[test]
    fn 同日窗口边界() {
        // 09:00 ~ 17:00
        let (s, e) = (9 * 60, 17 * 60);
        assert!(!in_window(8 * 60 + 59, s, e));
        assert!(in_window(9 * 60, s, e), "起点属于窗口");
        assert!(in_window(16 * 60 + 59, s, e));
        assert!(!in_window(17 * 60, s, e), "终点不属于窗口");
        assert!(!in_window(0, s, e));
    }

    #[test]
    fn 跨午夜窗口() {
        // 22:00 ~ 07:00
        let (s, e) = (22 * 60, 7 * 60);
        assert!(in_window(22 * 60, s, e), "22:00 属于窗口");
        assert!(in_window(23 * 60 + 59, s, e));
        assert!(in_window(0, s, e), "午夜属于窗口");
        assert!(in_window(6 * 60 + 59, s, e));
        assert!(!in_window(7 * 60, s, e), "07:00 不属于窗口");
        assert!(!in_window(12 * 60, s, e), "中午不属于窗口");
        assert!(!in_window(21 * 60 + 59, s, e));
    }

    #[test]
    fn 零长度窗口视为关闭() {
        assert!(!in_window(12 * 60, 12 * 60, 12 * 60));
        assert!(!in_window(0, 0, 0));
    }

    /// 进入 → 退出：恢复进入前的模式
    #[test]
    fn 状态机_进入切走_退出恢复() {
        let mut st = NightState::default();

        // 22:00 进入（当前极速 → 切保守）
        assert_eq!(
            transition(&mut st, true, "conservative", "turbo"),
            NightAction::Enter
        );
        assert!(st.active && st.applied);
        assert_eq!(st.prev_mode.as_deref(), Some("turbo"));
        assert!(!st.user_override);

        // 窗口内下一拍：已在目标模式，什么都不做
        assert_eq!(
            transition(&mut st, true, "conservative", "conservative"),
            NightAction::None
        );

        // 07:00 退出：恢复 turbo
        assert_eq!(
            transition(&mut st, false, "conservative", "conservative"),
            NightAction::Leave("turbo".to_string())
        );
        assert!(!st.active && !st.applied);
        assert_eq!(st.prev_mode, None);
    }

    /// 窗口内用户手动切走：本窗口不再自动恢复
    #[test]
    fn 状态机_用户手动切换后不再恢复() {
        let mut st = NightState::default();
        assert_eq!(
            transition(&mut st, true, "conservative", "turbo"),
            NightAction::Enter
        );

        // 用户手动切到 custom
        assert_eq!(
            transition(&mut st, true, "conservative", "custom"),
            NightAction::Override
        );
        assert!(st.user_override);

        // 退出窗口：不恢复（尊重用户）
        assert_eq!(
            transition(&mut st, false, "conservative", "custom"),
            NightAction::LeaveNoop
        );
        assert!(!st.active);
    }

    /// 进入时本来就在目标模式：不切换、退出也不恢复
    #[test]
    fn 状态机_本来就在目标模式则不切换() {
        let mut st = NightState::default();
        assert_eq!(
            transition(&mut st, true, "conservative", "conservative"),
            NightAction::EnterAlready
        );
        assert!(!st.applied, "没有实际切换过");
        assert_eq!(
            transition(&mut st, false, "conservative", "conservative"),
            NightAction::LeaveNoop
        );
    }

    /// 用户覆盖后，窗口内手动切回目标模式：仍不恢复（用户已表达过意图）
    #[test]
    fn 状态机_覆盖后切回目标也不恢复() {
        let mut st = NightState::default();
        transition(&mut st, true, "conservative", "turbo");
        transition(&mut st, true, "conservative", "custom"); // override
                                                             // 用户又切回夜间模式
        assert_eq!(
            transition(&mut st, true, "conservative", "conservative"),
            NightAction::None
        );
        assert_eq!(
            transition(&mut st, false, "conservative", "conservative"),
            NightAction::LeaveNoop
        );
    }

    /// 跨重启：读到 active=true 且仍在窗口内 ⇒ 保持，不重复切换
    #[test]
    fn 状态机_重启后仍在窗口内不重复切换() {
        let mut st = NightState {
            active: true,
            prev_mode: Some("turbo".to_string()),
            applied: true,
            user_override: false,
        };
        assert_eq!(
            transition(&mut st, true, "conservative", "conservative"),
            NightAction::None
        );

        // 重启后已离开窗口 ⇒ 仍能恢复
        assert_eq!(
            transition(&mut st, false, "conservative", "conservative"),
            NightAction::Leave("turbo".to_string())
        );
    }

    #[test]
    fn 状态跨重启持久化() {
        let dir = std::env::temp_dir().join(format!("thermo_night_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("night.json");

        assert_eq!(load(&p), NightState::default(), "不存在 ⇒ 默认");

        let st = NightState {
            active: true,
            prev_mode: Some("turbo".into()),
            applied: true,
            user_override: false,
        };
        save(&p, &st).unwrap();
        assert_eq!(load(&p), st, "往返一致");

        std::fs::write(&p, "{ broken").unwrap();
        assert_eq!(load(&p), NightState::default(), "损坏 ⇒ 默认，不 panic");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
