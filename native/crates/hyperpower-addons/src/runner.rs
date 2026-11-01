//! 附加功能调度器：按各自节奏挨个敲，出错熔断，互不拖累
//!
//! 设计见 `docs/09-设计-附加功能架构.md` §5。
//!
//! # 为什么不是线程 / 异步运行时
//!
//! 本模块的负载是"每 15 秒读一个文件"，用线程纯属浪费；且发布版
//! `panic = "abort"` —— 线程 panic 一样会带走整个进程，换不来隔离收益。
//! 真正的隔离由「**禁止 panic**（编译期）+ **熔断**（运行期）」保证。

use crate::addon::{Addon, AddonError, AddonStatus, ProbeOutcome, TickOutcome};
use crate::io::AddonIo;

/// 连续失败多少次后熔断（自动停用该功能）
pub const TRIP_AFTER_FAILURES: u32 = 3;

/// 本机不支持时的重探间隔（秒）
const UNSUPPORTED_RECHECK_S: u32 = 300;

/// 全部插槽都不可用时的兜底唤醒间隔（秒）
const IDLE_FALLBACK_S: u32 = 300;

/// 一个插槽：附加功能 + 它的调度与健康状态
#[derive(Debug)]
pub struct AddonSlot {
    addon: Box<dyn Addon>,
    /// 下一次该跑的时刻（单调秒）
    next_due_s: i64,
    /// 连续失败次数
    consecutive_failures: u32,
    /// 是否已熔断
    tripped: bool,
    /// 最近一次错误
    last_error: Option<AddonError>,
    /// 累计拍数 / 写入次数 / 出错次数
    ticks: u64,
    writes: u64,
    errors: u64,
}

impl AddonSlot {
    /// 包装一个附加功能
    pub fn new(addon: Box<dyn Addon>) -> Self {
        Self {
            addon,
            next_due_s: 0,
            consecutive_failures: 0,
            tripped: false,
            last_error: None,
            ticks: 0,
            writes: 0,
            errors: 0,
        }
    }

    /// 功能标识
    pub fn id(&self) -> &'static str {
        self.addon.spec().id
    }

    /// 只读访问
    pub fn addon(&self) -> &dyn Addon {
        self.addon.as_ref()
    }

    /// 可变访问（供命令面改配置）
    pub fn addon_mut(&mut self) -> &mut dyn Addon {
        self.addon.as_mut()
    }

    /// 是否已熔断
    pub fn is_tripped(&self) -> bool {
        self.tripped
    }

    /// 连续失败次数
    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    /// 最近一次错误
    pub fn last_error(&self) -> Option<&AddonError> {
        self.last_error.as_ref()
    }

    /// 累计拍数
    pub fn ticks(&self) -> u64 {
        self.ticks
    }

    /// 累计写入次数
    pub fn writes(&self) -> u64 {
        self.writes
    }

    /// 累计出错次数
    pub fn errors(&self) -> u64 {
        self.errors
    }

    /// 解除熔断；返回此前是否处于熔断状态
    pub fn recover(&mut self) -> bool {
        let was = self.tripped;
        self.tripped = false;
        self.consecutive_failures = 0;
        self.last_error = None;
        // 解除后立即安排下一拍，别让用户再等一个周期
        self.next_due_s = 0;
        was
    }

    /// 立即安排下一拍（把到期时间设为"现在"）
    ///
    /// # 何时调用
    ///
    /// **配置变更之后**。否则用户点了「启用充电上限」却要等满一个旧周期
    /// （未启用时是 60 秒）才生效 —— 体验上就是"按了没反应"。
    ///
    /// 已熔断的功能不受影响（仍由 [`Self::recover`] 显式解除）。
    pub fn reschedule_now(&mut self) {
        self.next_due_s = 0;
    }

    /// 探测本机支持性（**只读**）
    pub fn probe(&mut self, io: &mut dyn AddonIo) -> ProbeOutcome {
        self.addon.probe(io)
    }

    /// 状态快照（含熔断信息，覆盖功能自己的 `tripped`）
    pub fn status(&self) -> AddonStatus {
        let mut s = self.addon.status();
        s.tripped = self.tripped;
        s
    }

    /// 跑一拍并记账；返回本次结果
    fn run_once(&mut self, io: &mut dyn AddonIo, now_s: i64) -> TickOutcome {
        let outcome = self.addon.tick(io, now_s);
        self.record(&outcome);
        self.next_due_s = now_s + i64::from(outcome.next_interval_s.max(1));
        outcome
    }

    /// 记账 + 熔断判断
    fn record(&mut self, outcome: &TickOutcome) {
        self.ticks = self.ticks.saturating_add(1);
        if outcome.wrote {
            self.writes = self.writes.saturating_add(1);
        }

        match &outcome.error {
            Some(e) => {
                self.errors = self.errors.saturating_add(1);
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                self.last_error = Some(e.clone());

                if !self.tripped && self.consecutive_failures >= TRIP_AFTER_FAILURES {
                    self.tripped = true;
                    let label = self.addon.spec().label;
                    log::error!(
                        "附加功能「{label}」连续失败 {} 次，已自动停用（熔断）：{e}",
                        self.consecutive_failures
                    );
                }
            }
            None => {
                self.consecutive_failures = 0;
                self.last_error = None;
            }
        }
    }
}

/// 附加功能调度器（宿主持有一个）
#[derive(Debug)]
pub struct AddonRunner {
    slots: Vec<AddonSlot>,
}

impl AddonRunner {
    /// 用插槽列表构造
    pub fn new(slots: Vec<AddonSlot>) -> Self {
        Self { slots }
    }

    /// 全部插槽
    pub fn slots(&self) -> &[AddonSlot] {
        &self.slots
    }

    /// 按 id 查找
    pub fn slot(&self, id: &str) -> Option<&AddonSlot> {
        self.slots.iter().find(|s| s.id() == id)
    }

    /// 按 id 查找（可变）
    pub fn slot_mut(&mut self, id: &str) -> Option<&mut AddonSlot> {
        self.slots.iter_mut().find(|s| s.id() == id)
    }

    /// 是否一个插槽都没有
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// 启动时把所有功能探测一遍（**只读，零副作用**）
    ///
    /// 返回 `(标识, 是否支持, 详情)`，供宿主记事件 / 提示用户。
    pub fn probe_all(&mut self, io: &mut dyn AddonIo) -> Vec<(&'static str, bool, String)> {
        let mut out = Vec::with_capacity(self.slots.len());
        for slot in &mut self.slots {
            let outcome = slot.probe(io);
            out.push((
                slot.id(),
                outcome.is_supported(),
                outcome.message().to_string(),
            ));
        }
        out
    }

    /// 把所有"到点"的功能各跑一拍；返回本次真正跑过的功能标识
    ///
    /// ⚠️ 已熔断、本机不支持的功能会被跳过 —— **跳过不算失败**。
    pub fn tick_due(&mut self, io: &mut dyn AddonIo, now_s: i64) -> Vec<&'static str> {
        let mut ran: Vec<&'static str> = Vec::new();

        for slot in &mut self.slots {
            if slot.tripped || now_s < slot.next_due_s {
                continue;
            }

            // 本机不支持 → 放慢重探（不算失败，也不计错误）
            if !slot.addon.supported() {
                slot.next_due_s = now_s + i64::from(UNSUPPORTED_RECHECK_S);
                continue;
            }

            ran.push(slot.id());
            slot.run_once(io, now_s);
        }

        ran
    }

    /// 距离下一次"有功能要跑"还有多少秒（宿主据此决定睡多久）
    pub fn next_due_in_s(&self, now_s: i64) -> u32 {
        let mut min: Option<i64> = None;
        for slot in &self.slots {
            if slot.tripped || !slot.addon.supported() {
                continue;
            }
            let d = (slot.next_due_s - now_s).max(0);
            min = Some(min.map_or(d, |m| m.min(d)));
        }
        match min {
            Some(d) => u32::try_from(d).unwrap_or(IDLE_FALLBACK_S),
            None => IDLE_FALLBACK_S,
        }
    }

    /// 全部收尾（进程退出 / 交回官方时调用）
    pub fn release_all(&mut self, io: &mut dyn AddonIo) {
        for slot in &mut self.slots {
            let outcome = slot.addon.release(io);
            if let Some(e) = &outcome.error {
                log::warn!("附加功能「{}」收尾时报错：{e}", slot.addon.spec().label);
            }
        }
    }
}
