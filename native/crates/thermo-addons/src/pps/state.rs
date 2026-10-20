//! 会话状态机（**纯逻辑，不碰 I/O**）
//!
//! 契约见 `ThermoCharge-PPS移植方案.md` §4。
//!
//! # 写入纪律（照抄 FreePPS 真机结论，**不要"顺手优化"**）
//!
//! | 时机 | 写什么 | 为什么 |
//! |---|---|---|
//! | 启动 / 插电 | `1` | 解锁 |
//! | 充电中**每拍** | **不重复写** | 反复写会干扰小米原装头的 MIPPS 握手 |
//! | `Charging → Discharging` | `1` | 为下次插电准备 |
//! | 禁用且 `Discharging` | `0` | 还原官方状态 |
//! | 禁用但 `Charging` | **不动** | 避免打断协议协商 |
//!
//! # 为什么"充电中每拍不重复写"是硬要求
//!
//! `pd_verifed` 不是一个静态开关 —— 驱动在插电握手期间会读写它。
//! 我们在握手过程中反复写 `1`，可能让驱动的状态机认为认证结果反复跳变，
//! 从而**重置协商**（用户看到的是"插上又断开"）。
//!
//! 因此：**只在需要状态跳变时写一次**，之后靠 `engaged` 标记守住。

/// 本拍该做什么
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// 写 `1`（解锁）
    Engage,
    /// 写 `0`（还原）
    Release,
    /// 什么都不写
    Nothing,
}

impl Action {
    /// 是否需要写节点
    pub fn writes(self) -> bool {
        !matches!(self, Self::Nothing)
    }

    /// 要写入的值（`Nothing` 时无意义）
    pub fn value(self) -> Option<&'static str> {
        match self {
            Self::Engage => Some("1"),
            Self::Release => Some("0"),
            Self::Nothing => None,
        }
    }
}

/// 会话状态（跨拍保留）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    /// 是否已经**写过 `1` 且未撤销**
    ///
    /// 用来防止"充电中每拍重复写"。
    engaged: bool,
    /// 上一拍是否在充电（`None` = 还没跑过第一拍）
    last_charging: Option<bool>,
    /// 最近一次触发广播的时刻（Unix 秒）
    ///
    /// 只为 `status` 展示与"同一次插电不重复广播"服务。
    pub last_burst_at: Option<i64>,
}

impl State {
    /// 本拍是否在充电
    pub fn is_charging(&self) -> bool {
        self.last_charging.unwrap_or(false)
    }

    /// 是否已经解锁
    pub fn is_engaged(&self) -> bool {
        self.engaged
    }

    /// 会话的中文描述（供界面显示）
    pub fn session_label(&self) -> &'static str {
        match self.last_charging {
            None => "未开始",
            Some(true) if self.engaged => "充电中（已解锁）",
            Some(true) => "充电中（未解锁）",
            Some(false) if self.engaged => "未充电（已预备下次插电）",
            Some(false) => "未充电",
        }
    }

    /// **启用状态**下决定本拍动作
    ///
    /// # 规则
    ///
    /// ```text
    /// 充电中：已解锁 → 不写；未解锁 → 写 1
    /// 未充电：已解锁 → 不写；未解锁 → 写 1（为下次插电预备）
    /// ```
    ///
    /// 也就是"**只要还没解锁，就先解锁**"—— 未充电时提前写，
    /// 是为了让用户插上充电器的那一刻**立刻**生效（不用等一拍）。
    pub fn on_tick(&mut self, charging: bool, _now_s: i64) -> Action {
        let was = self.last_charging;
        self.last_charging = Some(charging);

        // 从"充电中"变成"未充电" ⇒ 为下次插电预备（写 1）
        //
        // ⚠️ 这里**不看 engaged**：上次插电时可能因为驱动重置而变回 0，
        //    拔电时补写一次，保证下次插上就是解锁态。
        if let (Some(true), false) = (was, charging) {
            self.engaged = true;
            return Action::Engage;
        }

        // 其余情况：还没解锁就解锁，已解锁就不动
        if self.engaged {
            Action::Nothing
        } else {
            self.engaged = true;
            Action::Engage
        }
    }

    /// **禁用状态**下决定本拍动作（`release` / 关闭功能时）
    ///
    /// ```text
    /// 未充电 → 写 0（还原官方状态）
    /// 充电中 → 不动（避免打断正在进行的协议协商）
    /// ```
    ///
    /// ⚠️ "充电中不动"这条容易让人以为漏了还原 —— 但它是对的：
    ///    此刻改 `pd_verifed` 会让驱动重新协商，用户会看到**充电中断**。
    ///    真正要还原，拔掉充电器后本函数自会写入 0。
    pub fn on_release(&mut self, charging: bool) -> Action {
        self.last_charging = Some(charging);

        if charging {
            // 保持 engaged（等拔电后再还原）
            return Action::Nothing;
        }

        if self.engaged {
            self.engaged = false;
            Action::Release
        } else {
            Action::Nothing
        }
    }

    /// **只**更新"是否在充电"，**不动** `engaged`
    ///
    /// # 为什么需要这个方法（2026-10-20 真机 bug）
    ///
    /// `auto` 模式下若条件不满足，我们**不写节点**。但当时的实现顺手调用了
    /// [`on_tick`](Self::on_tick) —— 它内部会把 `engaged` 置为 `true`
    /// （返回值却被丢弃了）。
    ///
    /// 后果（真机实测）：
    ///
    /// | 观察 | 值 |
    /// |---|---|
    /// | `engaged` | **true** ← 谎称已解锁 |
    /// | `node_value` | `0` ← 节点根本没写 |
    /// | `write_count` | **0** |
    ///
    /// 更糟的是：`engaged` 一旦为 `true`，之后**切回「锁定」模式也不会写**
    /// （状态机认为"已解锁，不必重复写"）—— 用户看到的就是
    /// **「开启 PPS 解锁无效」**。
    ///
    /// ⇒ 不写节点的路径只能调这个方法。
    pub fn note_charging(&mut self, charging: bool) {
        self.last_charging = Some(charging);
    }

    /// 外部把节点改回 0（或驱动重置）时同步内部状态
    ///
    /// 用于"回读发现实际值与我们应该写的值不一致"的情况：
    /// 把 `engaged` 修正成真实值，下一拍就会重新写。
    pub fn note_actual(&mut self, actual_is_one: bool) {
        self.engaged = actual_is_one;
    }

    /// 记录一次广播
    pub fn mark_burst(&mut self, now_s: i64) {
        self.last_burst_at = Some(now_s);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn 首拍充电就写1() {
        let mut s = State::default();
        assert_eq!(s.on_tick(true, 0), Action::Engage);
        assert!(s.is_engaged());
    }

    #[test]
    fn 首拍未充电也写1为下次插电预备() {
        let mut s = State::default();
        assert_eq!(s.on_tick(false, 0), Action::Engage);
        assert!(s.is_engaged(), "拔电状态也要预备好");
    }

    /// 🔴 核心纪律：充电中**不得**重复写（会干扰 MIPPS 握手）
    #[test]
    fn 充电中不重复写() {
        let mut s = State::default();
        assert_eq!(s.on_tick(true, 0), Action::Engage);

        for t in 1..10 {
            assert_eq!(s.on_tick(true, t), Action::Nothing, "第 {t} 拍不该重复写");
        }
    }

    /// 🔴 回归守卫：`note_charging` **不得**改动 `engaged`
    ///
    /// # 这个 bug 真机踩过（2026-10-20）
    ///
    /// `auto` 模式条件不满足时，原实现调了 `on_tick()` 并把返回值丢弃 ——
    /// `engaged` 被置 `true` 但节点根本没写。之后切回「锁定」模式，
    /// 状态机认为"已解锁"而**永远不写** ⇒ 用户看到「开启 PPS 解锁无效」。
    #[test]
    fn 仅记录充电状态不污染engaged() {
        let mut s = State::default();
        s.note_charging(true);
        assert!(!s.is_engaged(), "只记录状态，不该认为已解锁");
        assert!(s.is_charging());

        s.note_charging(false);
        assert!(!s.is_engaged());
        assert!(!s.is_charging());
    }

    /// 已解锁后 `note_charging` 也不该把它清掉（它只管"是否在充电"）
    #[test]
    fn 已解锁时记录状态不影响解锁标记() {
        let mut s = State::default();
        s.on_tick(true, 0); // 解锁
        assert!(s.is_engaged());

        s.note_charging(false);
        assert!(s.is_engaged(), "不该因为拔电就清掉标记");
    }

    /// 拔电时补写一次，保证下次插上就是解锁态
    #[test]
    fn 拔出时补写1() {
        let mut s = State::default();
        s.on_tick(false, 0); // 首拍拔电 → Engage
        assert_eq!(s.on_tick(false, 1), Action::Nothing, "持续拔电不必再写");

        // 插上（已解锁）
        assert_eq!(s.on_tick(true, 2), Action::Nothing);
        // 再拔出 → 补写
        assert_eq!(s.on_tick(false, 3), Action::Engage);
    }

    #[test]
    fn 禁用时未充电写0还原() {
        let mut s = State::default();
        s.on_tick(true, 0); // 先解锁
        s.on_tick(false, 1);

        assert_eq!(s.on_release(false), Action::Release);
        assert!(!s.is_engaged(), "还原后内部状态同步");
    }

    /// 🔴 禁用但充电中 ⇒ **不动**，避免打断协议协商
    #[test]
    fn 禁用时充电中不动() {
        let mut s = State::default();
        s.on_tick(true, 0);
        assert_eq!(s.on_release(true), Action::Nothing);
    }

    #[test]
    fn 从未解锁时释放不写() {
        let mut s = State::default();
        assert_eq!(s.on_release(false), Action::Nothing, "没写过就不必还原");
    }

    /// 驱动把我们写的 1 重置成 0 ⇒ 下一拍要能重新写
    #[test]
    fn 节点被重置后能重新解锁() {
        let mut s = State::default();
        s.on_tick(true, 0);
        assert_eq!(s.on_tick(true, 1), Action::Nothing);

        // 回读发现实际是 0（驱动重置 / 别的程序改了）
        s.note_actual(false);
        assert_eq!(s.on_tick(true, 2), Action::Engage, "应重新写回 1");
    }

    #[test]
    fn 动作的写入值正确() {
        assert_eq!(Action::Engage.value(), Some("1"));
        assert_eq!(Action::Release.value(), Some("0"));
        assert_eq!(Action::Nothing.value(), None);
        assert!(Action::Engage.writes());
        assert!(!Action::Nothing.writes());
    }

    #[test]
    fn 会话描述随状态变化() {
        let mut s = State::default();
        assert_eq!(s.session_label(), "未开始");
        s.on_tick(true, 0);
        assert_eq!(s.session_label(), "充电中（已解锁）");
        s.on_tick(false, 1);
        assert_eq!(s.session_label(), "未充电（已预备下次插电）");

        let mut s2 = State::default();
        s2.on_release(false);
        assert_eq!(s2.session_label(), "未充电");
    }
}
