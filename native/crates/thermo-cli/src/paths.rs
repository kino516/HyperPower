//! 路径常量与运行期布局
//!
//! 与 `docs/01-项目方案.md` §4.2 一致。
//!
//! # ⚠️ 模块目录名 ≠ 数据目录名（**不要试图统一它们**）
//!
//! | 目录 | 名字 | 由谁决定 |
//! |---|---|---|
//! | 模块目录 | `a_thermo_charge` | `module.prop` 的 `id` |
//! | 数据目录 | `thermo_charge` | 本文件的常量（**与模块 id 无关**） |
//!
//! 两者刻意不同名，原因有两条：
//!
//! | # | 理由 |
//! |---|---|
//! | 1 | **改模块 id 不该丢数据** —— id 影响 KernelSU 的模块列表排序，是个纯展示层面的选择；数据目录是用户的配置/档案/历史，不该被它牵连 |
//! | 2 | `a_` 前缀是**为了排序**（KernelSU 按 id 字母序升序排列），属于"展示技巧"，不该污染持久化路径 |
//!
//! 因此改 `id` 时，**只需同步本文件的 `MODULE_DIR`**，
//! `DATA_DIR` 必须保持不变。
//!
//! 🔴 迁移陷阱：`uninstall.sh` 会删除整个数据目录（用户明确要求"卸载后不留数据"）。
//! 若从旧 id 迁移，**不能**走卸载脚本，否则配置与档案全丢 ——
//! `tools/full-deploy.sh` 的 ①B 步专门处理这件事（直接删旧模块目录、
//! 保留数据目录）。

use std::path::PathBuf;

/// 数据目录
///
/// ⚠️ **与模块 id 无关，改名时不得改动**（见模块文档）。
pub const DATA_DIR: &str = "/data/adb/thermo_charge";

/// 模块目录
///
/// ⚠️ 必须与 `module/src/module.prop` 的 `id` 一致 ——
/// KernelSU 用 `id` 作为模块目录名。
pub const MODULE_DIR: &str = "/data/adb/modules/a_thermo_charge";

/// 导出目录（用户可访问）
pub const EXPORT_DIR: &str = "/sdcard/Download";

/// 数据目录（`PathBuf` 形式）
pub fn data_dir() -> PathBuf {
    PathBuf::from(DATA_DIR)
}

/// 控制 socket 路径
pub fn control_sock() -> PathBuf {
    data_dir().join("ipc").join("control.sock")
}

/// 配置文件
pub fn config_json() -> PathBuf {
    data_dir().join("config.json")
}

/// 预设文件
pub fn presets_json() -> PathBuf {
    data_dir().join("presets.json")
}

/// 运行快照（**节流落盘**）
pub fn runtime_json() -> PathBuf {
    data_dir().join("runtime.json")
}

/// 事件流（JSON Lines，追加写 + 按大小轮转）
pub fn events_jsonl() -> PathBuf {
    data_dir().join("events.jsonl")
}

/// 会话采样
pub fn session_csv() -> PathBuf {
    data_dir().join("session.csv")
}

/// 充电历史（已完成会话的归档）
///
/// ⚠️ 与 `device.json` **完全无关** —— 档案是能力边界的权威来源，
/// 历史只是观测归档。绝不可把历史数据接进决策（见 `history` 模块文档）。
pub fn history_json() -> PathBuf {
    data_dir().join("history.json")
}

/// 充电历史草稿（进行中会话）
///
/// # 为什么是 `.jsonl`（追加写）而不是 `.json`（整份重写）
///
/// 真机踩到过一个**数据丢失**缺陷：原先草稿每 10 分钟整份重写一次，
/// 于是 **worker 每次重启都会丢掉最多 10 分钟的采样点**
/// （实测：一次 35 点应得的曲线只剩 24 点，中间两处 100~200 秒断档）。
///
/// 改为**追加写**（每行一个采样点）后：
///
/// | | 整份重写（旧） | 追加写（现在） |
/// |---|---|---|
/// | 每点写入量 | 约 12 KB | **约 50 字节** |
/// | 丢点风险 | 最多 10 分钟 | **零**（每点立即落盘） |
///
/// 写入量降低约 200 倍，反而比原来更省闪存。
pub fn history_draft_jsonl() -> PathBuf {
    data_dir().join("history.current.jsonl")
}

/// **已废弃**的草稿路径（旧格式，整份 JSON）
///
/// 保留这个常量只为在恢复时顺手清理它 —— 老版本留下的文件会一直躺在
/// 数据目录里，不清理会让 `data stats` 虚报占用。
pub fn history_draft_json_legacy() -> PathBuf {
    data_dir().join("history.current.json")
}

/// pidfile
pub fn pidfile() -> PathBuf {
    data_dir().join("worker.pid")
}

/// 单实例锁文件（`flock` 的目标）
pub fn lockfile() -> PathBuf {
    data_dir().join("worker.lock")
}

/// supervisor 的 pidfile
pub fn supervisor_pidfile() -> PathBuf {
    data_dir().join("supervisor.pid")
}

/// 日志目录
pub fn log_dir() -> PathBuf {
    data_dir().join("logs")
}

/// 主日志
pub fn main_log() -> PathBuf {
    log_dir().join("main.log")
}

/// 机型档案目录（**出厂档案 / 回流落点**）
///
/// ⚠️ 位于**模块目录**下（模块 = 只读发布物）。
/// 当前默认为空 —— 一切能力数据以本机检测为准。
pub fn profiles_dir() -> PathBuf {
    PathBuf::from(MODULE_DIR).join("profiles")
}

/// 本机设备档案（检测结果）
pub fn device_json() -> PathBuf {
    data_dir().join("device.json")
}

/// 确保数据目录结构与权限就绪
pub fn ensure_dirs() -> std::io::Result<()> {
    for d in [data_dir(), log_dir(), data_dir().join("ipc")] {
        std::fs::create_dir_all(&d)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // 数据目录 0700：只有 root 能进（socket 鉴权的第一道边界）
        std::fs::set_permissions(data_dir(), std::fs::Permissions::from_mode(0o700)).ok();
    }
    Ok(())
}
