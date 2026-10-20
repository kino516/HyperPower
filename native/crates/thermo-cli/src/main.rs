//! 温控充 ThermoCharge · 单二进制
//!
//! 子命令：
//! - `supervisor` —— 进程监督与崩溃恢复（仅写固定安全值）
//! - `worker`     —— 采样 / 决策 / **唯一常规 sysfs 写者**
//! - `ctl`        —— 经 UDS 与 worker 通信（45 条命令）
//! - `watch`      —— 实时事件流
//! - `probe`      —— 诊断（只读）
//! - `device`     —— 设备检测与档案
//! - `version`    —— 版本信息
//!
//! 编译目标：`aarch64-linux-android`（CI 上另跑 Linux/x86_64）。
//!
//! # 关于平台范围
//!
//! | 模块 | 平台 | 说明 |
//! |---|---|---|
//! | `sysfs` `engine` `commands` `config_store` `detect` `profile_io` `observability` | 任意 | 纯逻辑 + 文件 I/O，**可在 CI（任意平台）上测试** |
//! | `worker` `supervisor` `server` | **仅 Unix** | 依赖 `timerfd` / `poll` / `flock` / netlink / UDS（Android 与 Linux 均满足） |
//!
//! 这样划分的意义：**核心控制逻辑可以在本机（Windows）直接跑测试**，
//! 不需要真机、不需要 root、也不需要等 CI。
//!
//! # 关于 `unsafe`
//!
//! 本 crate 禁止在**业务代码**中使用 `unsafe`。唯一的例外是
//! [`worker`]（`timerfd` / `poll` / netlink socket）与 [`server`]
//! （`getsockopt(SO_PEERCRED)` 取对端 UID）—— 这几处系统调用在标准库中
//! 没有安全封装。每个 `unsafe` 块都紧邻 `SAFETY` 注释说明为何安全。

#![deny(unsafe_code)]
// Windows 只用于**开发与跑测试**（生产目标是 Android / Linux，均已满足 Unix）。
// 此时 `worker` / `supervisor` / `server` 被 cfg 掉，它们用到的路径常量、
// 事件类型等自然成为"未使用"。在非 Unix 上放行死代码告警，
// 而在 Unix（含 CI 与设备）上仍保持**严格零告警**。
#![cfg_attr(not(unix), allow(dead_code))]

mod addon_host;
mod cli;
mod commands;
mod config_store;
mod debug_log;
mod detect;
mod engine;
mod history;
mod io_watchdog;
mod ipc;
mod logging;
mod observability;
mod paths;
mod profile_io;
#[cfg(unix)]
mod server;
/// ⚠️ **不要**给这个模块加 `#[cfg(unix)]`
///
/// 它是纯逻辑，且带测试。worker 本身是 `#[cfg(unix)]` 的，
/// 若把它塞回 worker，**测试在 Windows 上永远不跑**（2026-10-17 踩到）。
/// 详见模块文档。
mod sleep_events;
#[cfg(unix)]
mod supervisor;
mod sysfs;
#[cfg(unix)]
mod worker;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, Command};

fn main() -> Result<()> {
    let cli = Cli::parse();
    logging::init(cli.verbose);

    let result = match cli.command {
        Command::Supervisor => cli::run_supervisor(),
        Command::Worker => cli::run_worker(),
        Command::Ctl { .. } => cli::run_ctl(cli.command),
        Command::Watch => cli::run_watch(),
        Command::Probe { official, json } => cli::run_probe(official, json),
        Command::Verify { json } => cli::run_verify(json),
        Command::Measure {
            duration_ms,
            interval_ms,
            json,
        } => cli::run_measure(duration_ms, interval_ms, json),
        Command::Device { action } => cli::run_device(action),
        Command::Pps { action } => cli::run_pps(action),
        Command::Version => {
            println!("{}", cli::version_json());
            Ok(())
        }
    };

    if let Err(e) = &result {
        log::error!("{e:#}");
    }
    result
}
