//! 可观测性：事件流（`events.jsonl`）与运行快照（`runtime.json`）
//!
//! 契约见 `docs/01-项目方案.md` §2.5 / §2.7。
//!
//! # 为什么要分开两个文件
//!
//! | 文件 | 性质 | 写入频率 |
//! |---|---|---|
//! | `events.jsonl` | **状态迁移与异常**（稀疏、有意义） | 事件驱动，追加写 |
//! | `runtime.json` | **实时快照**（供兜底读取与事后取证） | **节流**：状态变化且距上次 ≥5 秒 |
//!
//! ⚠️ 快照**必须节流**：1~3 秒级巡检若直写 flash，写放大会显著缩短闪存寿命。

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// 单文件大小上限（KB），超过则轮转
pub const DEFAULT_MAX_KB: u64 = 512;

/// 保留的历史文件份数（`.1` / `.2`）
pub const KEEP_FILES: usize = 2;

/// 快照最小写入间隔（秒）
///
/// 契约 §2.5：「状态变化时且距上次 ≥ 5 秒才落盘」。
pub const SNAPSHOT_MIN_INTERVAL_S: i64 = 5;

// ============================================================
//  事件流
// ============================================================

/// 事件类型（**固定字符串**，前端据此筛选）
pub mod kind {
    /// 状态迁移（Phase 变化）
    pub const STATE: &str = "state";
    /// 写入失败 / 回读不符
    pub const WRITE_FAIL: &str = "write_fail";
    /// 读取失败
    pub const READ_FAIL: &str = "read_fail";
    /// 交回官方
    pub const RELEASE: &str = "release";
    /// 硬保护触发
    pub const HARD_PROTECT: &str = "hard_protect";
    /// 配置变更
    pub const CONFIG: &str = "config";
    /// 配置被钳位（越界修正，**不可静默**）
    pub const CONFIG_CLAMPED: &str = "config_clamped";
    /// 崩溃恢复
    pub const CRASH: &str = "crash";
    /// 程序生命周期
    pub const LIFECYCLE: &str = "lifecycle";
    /// 检测完成
    pub const DETECT: &str = "detect";
    /// 附加功能熔断
    pub const ADDON_TRIP: &str = "addon_trip";
    /// 附加功能状态变化（写入生效等）
    pub const ADDON: &str = "addon";
    /// 拒绝（鉴权、非法请求）
    pub const DENY: &str = "deny";
}

/// 事件流写入器
///
/// # 与 `watch` 客户端的关系
///
/// 除了落盘，还保留一份**最近的**事件（内存环形缓冲），
/// 供 worker 的主循环推送给 `watch` 订阅者 —— 这样"落盘"与"推送"
/// 共用同一个产生点，不会出现"日志里有、推送里没有"的不一致。
#[derive(Debug, Clone)]
pub struct EventLog {
    path: PathBuf,
    max_bytes: u64,
    /// 最近事件（供 `watch` 推送；`drain_recent` 取走）
    recent: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
}

/// 内存中保留的最近事件条数
pub const RECENT_KEEP: usize = 64;

impl EventLog {
    /// 新建（不立即创建文件）
    pub fn new(path: impl Into<PathBuf>, max_kb: u64) -> Self {
        Self {
            path: path.into(),
            max_bytes: max_kb.max(64).saturating_mul(1024),
            recent: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// 记一条事件
    ///
    /// ⚠️ 失败**只记日志、不中断业务**：观测手段的故障不该拖垮控制。
    pub fn emit(&self, kind: &str, msg: &str, data: Value) {
        self.rotate_if_needed();

        let line = json!({
            "ts": now_unix(),
            "kind": kind,
            "msg": msg,
            "data": data,
        });

        // ① 记入内存（供 watch 推送）—— 即使落盘失败，订阅者仍能看到
        if let Ok(mut r) = self.recent.lock() {
            r.push(line.clone());
            if r.len() > RECENT_KEEP {
                let overflow = r.len() - RECENT_KEEP;
                r.drain(..overflow);
            }
        }

        // ② 落盘
        let Ok(text) = serde_json::to_string(&line) else {
            log::warn!("事件序列化失败（已丢弃）：{kind} {msg}");
            return;
        };

        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(mut f) => {
                if let Err(e) = writeln!(f, "{text}") {
                    log::warn!("事件写入失败：{e}");
                }
            }
            Err(e) => log::warn!("事件文件打开失败（{}）：{e}", self.path.display()),
        }
    }

    /// 取走待推送的事件（供 `watch` 订阅者消费）
    ///
    /// 主循环每拍调用一次；没有订阅者时也应当调用，否则缓冲会被填满
    /// 而丢掉最新的推送（不影响落盘）。
    pub fn drain_recent(&self) -> Vec<Value> {
        match self.recent.lock() {
            Ok(mut r) => std::mem::take(&mut *r),
            Err(_) => Vec::new(),
        }
    }

    /// 超过上限则轮转（`.1` → `.2`，当前 → `.1`）
    fn rotate_if_needed(&self) {
        let Ok(meta) = std::fs::metadata(&self.path) else {
            return;
        };
        if meta.len() < self.max_bytes {
            return;
        }

        // 从最老的开始往后挪，避免覆盖
        for i in (1..=KEEP_FILES).rev() {
            let from = self.suffixed(i);
            let to = self.suffixed(i + 1);
            if from.exists() && i == KEEP_FILES {
                std::fs::remove_file(&from).ok();
            }
            if from.exists() {
                std::fs::rename(&from, &to).ok();
            }
        }
        std::fs::rename(&self.path, self.suffixed(1)).ok();
    }

    /// 生成 `events.jsonl.1` 形式的路径
    fn suffixed(&self, n: usize) -> PathBuf {
        let mut s = self.path.clone().into_os_string();
        s.push(format!(".{n}"));
        PathBuf::from(s)
    }
}

// ============================================================
//  运行快照（节流落盘）
// ============================================================

/// 运行快照写入器
///
/// **节流逻辑**：值发生变化**且**距上次落盘 ≥ [`SNAPSHOT_MIN_INTERVAL_S`] 秒才写。
#[derive(Debug, Clone)]
pub struct SnapshotWriter {
    path: PathBuf,
    last_write_s: i64,
    last_hash: u64,
}

impl SnapshotWriter {
    /// 新建
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            last_write_s: 0,
            last_hash: 0,
        }
    }

    /// 尝试落盘；返回 `true` 表示本次确实写了
    ///
    /// `force` 用于关键时点（启动、交回官方、退出）—— 那些时刻必须留下记录。
    pub fn maybe_write(&mut self, snapshot: &Value, now_s: i64, force: bool) -> bool {
        let hash = fingerprint(snapshot);
        let changed = hash != self.last_hash;

        if !force {
            if !changed {
                return false;
            }
            if now_s - self.last_write_s < SNAPSHOT_MIN_INTERVAL_S {
                return false;
            }
        }

        match write_atomic(&self.path, snapshot) {
            Ok(()) => {
                self.last_write_s = now_s;
                self.last_hash = hash;
                true
            }
            Err(e) => {
                log::warn!("运行快照写入失败：{e:#}");
                false
            }
        }
    }
}

/// 内容指纹（用于判断"值是否变化"，无需序列化比较）
fn fingerprint(v: &Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let text = v.to_string();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// 原子写文件（临时文件 → `fsync` → `rename`）
pub fn write_atomic(path: &Path, value: &Value) -> anyhow::Result<()> {
    use anyhow::Context;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建目录失败: {}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(value).context("序列化失败")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text.as_bytes())
        .with_context(|| format!("写入临时文件失败: {}", tmp.display()))?;
    if let Ok(f) = std::fs::File::open(&tmp) {
        f.sync_all().ok();
    }
    std::fs::rename(&tmp, path).with_context(|| format!("替换失败: {}", path.display()))?;
    Ok(())
}

/// 读取最近 n 行（供 `logs events`）
pub fn tail_lines(path: &Path, n: usize) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let skip = lines.len().saturating_sub(n);
    lines[skip..].iter().map(|s| (*s).to_string()).collect()
}

/// 当前 Unix 秒
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 给机器看的浮点：**取整到指定小数位**，并消掉 `-0.0`
///
/// # 为什么需要它（真机踩到三次）
///
/// 原始浮点序列化后会带长尾巴，界面直接显示出来极难看：
///
/// | 现象 | 根因 |
/// |---|---|
/// | `0.6525578000000001` | 除法产生（`sum / len`） |
/// | `1.0959999561309814` | 在 **f32** 上取整 —— f32 存不下 1.096 |
/// | `energy_total_wh: -0.0` | 浮点负零 ⇒ 界面显示「-0 Wh」 |
///
/// # ⚠️ 必须在 `f64` 上取整
///
/// ```text
/// f32 存不下 1.096
///   ⇒ round(1.096f32) 只是得到"最接近 1.096 的 f32"
///   ⇒ 序列化成 f64 时精确展开：1.0959999561309814
/// ```
///
/// 所以调用方应先把 `f32` 转成 `f64` 再传进来（见 [`round_json_f32`]）。
///
/// # 为什么放在这里而不是各模块自己写
///
/// 项目有过"同一份规则写两遍导致分叉"的教训（见 `docs/08` §3.2 的
/// checksum 事件）。输出规范属于可观测性范畴，因此**只留这一份**。
pub fn round_json(x: f64, digits: u32) -> f64 {
    if !x.is_finite() {
        // 非有限值不该出现在 JSON 里；原样返回让调用方自己发现
        return x;
    }
    let p = 10_f64.powi(digits as i32);
    let r = (x * p).round() / p;
    // 消掉 -0.0：界面会显示成「-0」看着像数据错了
    if r == 0.0 {
        0.0
    } else {
        r
    }
}

/// [`round_json`] 的 `f32` 便捷包装
///
/// ⚠️ 先转 `f64` 再取整 —— 直接在 `f32` 上取整会留尾巴（见上）。
pub fn round_json_f32(x: f32, digits: u32) -> f64 {
    round_json(f64::from(x), digits)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn tmp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("thermo_test_{name}_{}", std::process::id()));
        p
    }

    #[test]
    fn 节流_无变化不落盘() {
        let path = tmp_path("snap_nochange.json");
        let mut w = SnapshotWriter::new(&path);
        let v = json!({"a": 1});
        assert!(w.maybe_write(&v, 100, false), "首次应写入");
        assert!(!w.maybe_write(&v, 200, false), "内容未变则不写");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn 节流_变化但间隔不足也不写() {
        let path = tmp_path("snap_throttle.json");
        let mut w = SnapshotWriter::new(&path);
        assert!(w.maybe_write(&json!({"a": 1}), 100, false));
        assert!(
            !w.maybe_write(&json!({"a": 2}), 102, false),
            "距上次不足 5 秒，即使变了也不写"
        );
        assert!(
            w.maybe_write(&json!({"a": 3}), 200, false),
            "间隔足够且已变化 → 写"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn 强制写入忽略节流() {
        let path = tmp_path("snap_force.json");
        let mut w = SnapshotWriter::new(&path);
        assert!(w.maybe_write(&json!({"a": 1}), 100, false));
        assert!(
            w.maybe_write(&json!({"a": 1}), 101, true),
            "关键时点必须留下记录"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn 事件写入与读取() {
        let path = tmp_path("events.jsonl");
        let log = EventLog::new(&path, 512);
        log.emit(kind::STATE, "进入收紧", json!({"phase": "tightening"}));
        log.emit(kind::WRITE_FAIL, "写入失败", json!({"retry": 1}));

        let lines = tail_lines(&path, 10);
        assert_eq!(lines.len(), 2);
        let first: Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(first["kind"], "state");
        assert_eq!(first["data"]["phase"], "tightening");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn 事件轮转保留历史() {
        let path = tmp_path("rotate.jsonl");
        let _ = std::fs::remove_file(&path);
        let log = EventLog::new(&path, 64); // 上限 64 KB

        // 写满超过上限
        let big = "x".repeat(1024);
        for _ in 0..80 {
            log.emit(kind::STATE, &big, json!({}));
        }

        assert!(path.exists(), "当前文件应存在（轮转后重建）");
        let rotated = {
            let mut s = path.clone().into_os_string();
            s.push(".1");
            PathBuf::from(s)
        };
        assert!(rotated.exists(), "应产生 .1 历史文件");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&rotated);
    }

    // ────────────────────────────────────────────────────────────
    //  输出用的浮点取整（真机踩到三次，见 `round_json` 的文档）
    // ────────────────────────────────────────────────────────────

    #[test]
    fn 取整消掉除法产生的尾巴() {
        // 真机实测：measure 输出 `avg_w: 0.6525578000000001`
        assert_eq!(round_json(0.6525578000000001, 2), 0.65);
        // 序列化后必须干净
        let v = json!({ "avg_w": round_json(0.6525578000000001, 2) });
        assert!(
            !v.to_string().contains("0000000"),
            "序列化后不得有浮点尾巴：{v}"
        );
    }

    #[test]
    fn 取整消掉_f32_精度陷阱() {
        // 真机实测：`energy_wh: 1.0959999561309814`
        // 根因是在 f32 上取整 —— f32 根本存不下 1.096
        assert_eq!(round_json_f32(1.096_f32, 3), 1.096);
        let v = json!({ "energy_wh": round_json_f32(1.096_f32, 3) });
        assert!(
            !v.to_string().contains("0000000"),
            "f32 转 f64 取整后必须干净：{v}"
        );
    }

    #[test]
    fn 取整消掉负零() {
        // 真机实测：`energy_total_wh: -0.0` ⇒ 界面显示「-0 Wh」
        assert!(round_json(-0.0, 1).is_sign_positive());
        assert!(round_json(-0.001, 1).is_sign_positive());
        assert!(round_json_f32(-0.0_f32, 2).is_sign_positive());
    }

    #[test]
    fn 取整不影响正常值() {
        assert_eq!(round_json(42.34, 1), 42.3);
        assert_eq!(round_json(42.36, 1), 42.4);
        assert_eq!(round_json(0.0, 2), 0.0);
        assert_eq!(round_json(100.0, 0), 100.0);
    }

    #[test]
    fn 非有限值原样返回且不崩溃() {
        // NaN / Inf 不该出现在 JSON 里，但也不该让取整崩掉
        assert!(round_json(f64::NAN, 2).is_nan());
        assert!(round_json(f64::INFINITY, 2).is_infinite());
    }
}
