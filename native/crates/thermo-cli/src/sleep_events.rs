//! 深度休眠**事件的节流与配对**（纯逻辑，可跨平台测试）
//!
//! # ⚠️ 为什么它不在 `worker.rs` 里
//!
//! 这段逻辑语义上属于 worker（它决定 worker 主循环要不要记事件），
//! 但 `main.rs` 里 worker 是 **`#[cfg(unix)]`** 的：
//!
//! ```ignore
//! #[cfg(unix)]
//! mod worker;          // ← Windows 上根本不编译
//! ```
//!
//! 于是**在 worker.rs 里写的测试，在 Windows 原生测试时一个都不会跑** ——
//! `cargo test` 显示"195 项通过"，看起来正常，实际那几条测试从未被执行
//! （2026-10-17 真机踩到：写完测试发现总数没变，才查出这个门）。
//!
//! 交叉 clippy（`--target aarch64-linux-android`）能编译它们，所以**编译错误
//! 能发现**，但**行为错误发现不了** —— 这正是最危险的地方。
//!
//! ⇒ 纯逻辑放这里：既跨平台可测，又保留 worker 引用它的清晰关系。
//!
//! Android 目标**无法运行测试**（没有执行环境），所以"只能在 unix 下测"
//! 等于"永远不测"。

/// 休眠相关**事件**的最小间隔（秒）—— 防止客户端轮询把事件流刷爆
///
/// 日志（`main.log` / logcat）不受此限，每次迁移都记；
/// 只有写进 `events.jsonl` 的那条要节流。
///
/// # 为什么必须节流（真机实测踩到）
///
/// WebUI 每 3 秒轮询一次，而每次 `ctl` 调用都是**新连接**：
///
/// ```text
/// 轮询 → 连上（退出休眠）→ 应答 → 断开（进入休眠）→ 3 秒后轮询 …
/// ```
///
/// 不节流的话，界面开着期间会产出约 **2400 条/小时**的"进入/退出休眠"
/// 噪音，把真正有价值的事件（写入失败、状态迁移）挤出 `events.jsonl`，
/// 日志 1~2 小时就轮转一次。
///
/// 300 秒与 worker 的兜底唤醒间隔对齐 —— 一个真正空闲的模块最多每 5 分钟
/// 记一条，既不丢信息也不刷屏。
pub const SLEEP_EVENT_MIN_GAP_S: u64 = 300;

/// 休眠事件要不要记录的**纯决策**
///
/// # 参数
///
/// | 参数 | 含义 |
/// |---|---|
/// | `sleeping` | 本轮**即将**处于深度休眠吗 |
/// | `was_sleeping` | 上一轮处于深度休眠吗 |
/// | `event_open` | 本轮休眠的「进入」事件**记过了吗**（跨轮保持） |
/// | `since_last_s` | 距上次记休眠事件过了多少秒；`None` = 从没记过 |
/// | `gap_s` | 节流间隔（[`SLEEP_EVENT_MIN_GAP_S`]） |
///
/// # 返回 `(记日志「进入」, 记事件「进入」, 记事件「退出」)`
///
/// # 🔴 这个函数存在的唯一理由：**让进出的配对无法被破坏**
///
/// 用户反馈（2026-10-17）：「最近时间，有两次退出深度休眠，为何没有
/// 进入深度休眠？」
///
/// 初版让两个事件**各自**判断节流，于是（真机事件流，GAP = 300 秒）：
///
/// ```text
/// 23:28:06  进入 → 记（距上次已超 5 分钟）
/// 23:52:41  退出 → 记（距 23:28:06 已 24 分钟）        ⇒ 配成一对 ✓
/// 23:52:41  立刻又睡 → 距上次事件仅几毫秒 ⇒ **不记「进入」** ✗
/// 00:00:13  退出 → 距 23:52:41 已 7 分半 ⇒ **记「退出」** 🔴 孤零零一条
/// ```
///
/// ⇒ 用户看到两条没有配对的「退出」，完全无法理解发生了什么。
///    而实际上那两次休眠**都真实发生了**，只是「进入」被节流吞掉。
///
/// # 修法：退出**跟随**进入
///
/// 「退出」不再自己判断节流，而是看本轮休眠的「进入」有没有被记 ——
/// **要么都记、要么都不记**。
///
/// ⚠️ 同时「退出」**不刷新**节流窗口：那是"上次记事件"的时刻，只应由
///    「进入」刷新。否则「进入 → 立刻退出」会把窗口顺延，下一次真正的
///    「进入」又会被吞掉，问题原样复现。
///
/// ⚠️ 调用方应只在 `sleeping != was_sleeping` 时调用（状态迁移时）。
pub fn sleep_event_decision(
    sleeping: bool,
    was_sleeping: bool,
    event_open: bool,
    since_last_s: Option<u64>,
    gap_s: u64,
) -> (bool, bool, bool) {
    if sleeping && !was_sleeping {
        // 进入休眠：距上次事件够久才记
        let due = since_last_s.is_none_or(|s| s >= gap_s);
        // (记日志「进入」, 记事件「进入」, 记事件「退出」)
        (true, due, false)
    } else if !sleeping && was_sleeping {
        // 退出休眠：**只跟随**「进入」有没有被记
        (false, false, event_open)
    } else {
        // 状态没变 —— 调用方本不该走到这里；不记任何事件
        (false, false, false)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// 走完一次「进入 → 退出」，返回两次决策
    fn cycle(since_last_s: Option<u64>) -> ((bool, bool, bool), (bool, bool, bool)) {
        let enter = sleep_event_decision(true, false, false, since_last_s, 300);
        // 退出时 `event_open` = 进入是否记了
        let exit = sleep_event_decision(false, true, enter.1, None, 300);
        (enter, exit)
    }

    /// 距上次事件够久 ⇒ 进入与退出**都**记（正常的一次完整休眠）
    #[test]
    fn 完整休眠的进出成对记录() {
        let (enter, exit) = cycle(Some(600));
        assert_eq!(enter, (true, true, false), "够久 ⇒ 记「进入」");
        assert_eq!(exit, (false, false, true), "与「进入」配对 ⇒ 记「退出」");
    }

    /// 🔴 进入被节流时，退出**也必须不记** —— 这是本次修复的核心
    ///
    /// 复现用户报的场景：刚记过一次，立刻又睡又醒。
    /// 修复前这里会返回"记退出" ⇒ 事件流里出现孤零零的「退出」。
    #[test]
    fn 进入被节流时退出也不记() {
        let (enter, exit) = cycle(Some(3)); // 距上次仅 3 秒 < 300
        assert_eq!(enter, (true, false, false), "不够久 ⇒ 不记「进入」");
        assert_eq!(
            exit,
            (false, false, false),
            "「进入」没记时「退出」**必须**也不记，否则用户看到无配对的退出"
        );
    }

    /// 从没记过事件时，第一次「进入」要记
    #[test]
    fn 首次进入一定记() {
        let (enter, exit) = cycle(None);
        assert_eq!(enter, (true, true, false), "首次必须记，否则功能像没工作");
        assert_eq!(exit, (false, false, true));
    }

    /// 退出**不刷新**节流窗口 —— 否则"进入→立刻退出"会把窗口顺延，
    /// 下一次真正的「进入」又会被吞掉，问题原样复现
    #[test]
    fn 退出不刷新节流窗口() {
        // 场景：记过一次「进入」（窗口起点）→ 立刻退出 → 立刻再进入
        // 第二次「进入」的 `since_last_s` 仍是"距**那次进入**"的秒数，
        // 而不是"距退出"的秒数 ⇒ 因此仍被节流。
        let second_enter = sleep_event_decision(true, false, false, Some(4), 300);
        assert_eq!(
            second_enter,
            (true, false, false),
            "退出不该刷新窗口：距上次「进入」才 4 秒 ⇒ 仍节流"
        );
    }

    /// 状态没变时不记任何事件（调用方本不该传，但要有明确行为）
    #[test]
    fn 状态未变时不记事件() {
        assert_eq!(
            sleep_event_decision(true, true, true, None, 300),
            (false, false, false),
            "一直在睡 ⇒ 不该重复记"
        );
        assert_eq!(
            sleep_event_decision(false, false, false, None, 300),
            (false, false, false),
            "一直醒着 ⇒ 不该重复记"
        );
    }

    /// 边界：刚好等于间隔就记（`>=` 而不是 `>`）
    #[test]
    fn 间隔取等号时记事件() {
        let (enter, _) = cycle(Some(300));
        assert!(enter.1, "正好 300 秒应当记（边界取 >=，否则永远差一秒）");
    }

    /// 一整串真实时序：两次"刚记过就又睡又醒" + 一次真的充电唤醒
    ///
    /// 这是用户报的那个场景的**完整复现**，断言事件序列严格成对。
    #[test]
    fn 用户报的场景不再产生孤立退出() {
        // (即将休眠, 上一轮休眠, 距零点秒数)
        //
        // 秒数取自真机事件流（23:28:06 记为零点）：
        //   0     23:28:06 进入
        //   1475  23:52:41 退出 → 立刻又睡
        //   1927  00:00:13 退出 → 又睡
        //   2390  00:07:56 退出（这次真的充电了）
        let steps: [(bool, bool, u64); 6] = [
            (true, false, 0),
            (false, true, 1475),
            (true, false, 1475),
            (false, true, 1927),
            (true, false, 1927),
            (false, true, 2390),
        ];

        let mut open = false;
        let mut last: Option<u64> = None;
        let mut seq: Vec<&str> = Vec::new();

        for (sleeping, was_sleeping, now) in steps {
            let since = last.map(|t| now - t);
            let (_, rec_enter, rec_exit) =
                sleep_event_decision(sleeping, was_sleeping, open, since, SLEEP_EVENT_MIN_GAP_S);
            if rec_enter {
                seq.push("进入");
                last = Some(now);
                open = true;
            }
            if rec_exit {
                seq.push("退出");
                open = false;
            }
        }

        assert_eq!(
            seq,
            vec!["进入", "退出", "进入", "退出", "进入", "退出"],
            "事件必须严格成对；修复前会是 [进入, 退出, 退出, 退出] —— \
             后两个「退出」没有配对的「进入」，正是用户报的现象"
        );
    }
}
