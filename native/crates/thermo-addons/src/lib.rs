//! 附加功能框架（addons）
//!
//! 设计见 `docs/09-设计-附加功能架构.md`。
//!
//! # 一句话
//!
//! 把"充电上限"这类小功能做成**可插拔的零件**：核心温控（管"充多快"）与附加功能
//! （管"充到多少停"）是两个互不相干的循环 —— 各自有开关、各自有配置、各自有周期、
//! 各自出错、各自恢复。
//!
//! # 隔离契约（硬性）
//!
//! | # | 规则 | 由什么保证 |
//! |---|---|---|
//! | 1 | 附加功能**不认识**温控概念（档位 / 电流 / 温度 / 决策） | 本 crate **不依赖 `thermo-core`**（编译期切断） |
//! | 2 | 附加功能出错**不影响**温控，反之亦然 | 错误一律走返回值 + 连续失败**熔断**（[`runner`]） |
//! | 3 | 附加功能**禁止 panic** | workspace lint 已 `deny` `unwrap_used` / `expect_used` / `panic`（发布版 `panic = "abort"`，一次 panic 会带走整个进程） |
//! | 4 | 不共享运行状态 | 各自在内存里维护自己的状态；配置各解析各的分区 |
//! | 5 | 一个功能的配置损坏不影响别人 | 分区独立解析（[`registry::build_all`]） |
//!
//! # 目录
//!
//! ```text
//! addon.rs      插槽接口（Addon trait + 相关类型）
//! io.rs         I/O 抽象（宿主注入，功能自己不碰文件系统）
//! registry.rs   插槽注册表 —— 新增功能只改这里一行
//! runner.rs     调度 + 熔断
//! soc_limit/    附加功能 ①：充电上限（机制来自 HyperSC 逆向）
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod addon;
pub mod io;
pub mod registry;
pub mod runner;
pub mod soc_limit;

pub use addon::{Addon, AddonError, AddonSpec, AddonStatus, ProbeOutcome, TickOutcome};
pub use io::AddonIo;
pub use registry::build_all;
pub use runner::{AddonRunner, AddonSlot, TRIP_AFTER_FAILURES};
pub use soc_limit::{SocLimit, SocLimitConfig};

/// 框架的"接口版本"（附加功能对外输出的结构发生变化时提升）
pub const ADDON_API_VERSION: u32 = 1;
