//! 调试模式的数据写入器
//!
//! 用户要求（2026-10-17）：「在反馈问题中增加一个调试模式按钮，
//! 如果开启调试模式会记录更详细的数据，方便你抓包分析。」
//!
//! # 与现有记录的关系
//!
//! | 数据 | 平时 | 调试模式 |
//! |---|---|---|
//! | 事件 `events.jsonl` | **只在状态变化时**记 | 不变（仍然稀疏） |
//! | 运行日志 | 只在 **logcat 环形缓冲**（会被冲掉） | **同时落盘到本文件** |
//! | 历史曲线 | 30 秒一个点 | 5 秒一个点（见 `history` 模块） |
//! | **每拍决策链** | ❌ 不记 | ✅ 每拍一行 |
//! | **节点原始读写** | ❌ 不记 | ✅ 每次读写一行 |
//!
//! # 为什么**不**混进 `events.jsonl`
//!
//! 两者的定位与规模完全不同，混在一起会同时毁掉两边：
//!
//! | | 事件流 | 调试流 |
//! |---|---|---|
//! | 规模 | 几百条 | **每小时上千条** |
//! | 保留期 | 想长期回看 | 只为当下排障 |
//! | 用户是否看 | 会看 | 不看（给我们看） |
//!
//! 混在一起的话，事件会在几分钟内被调试数据挤出窗口 —— 而事件恰恰是
//! 用户最需要看的东西。
//!
//! # 格式：单个 JSONL
//!
//! 统一写成一行一个 JSON，用 `kind` 区分内容：
//!
//! ```text
//! {"t":1792090164.123,"kind":"trace","temp":38.6,"target_ua":7320000,...}
//! {"t":1792090164.456,"kind":"log","level":"INFO","target":"thermo::worker","msg":"..."}
//! {"t":1792090164.789,"kind":"node","op":"write","path":"...","value":"3456789","ok":true}
//! ```
//!
//! **为什么不用"结构化 + 可读文本"两个文件**：分析时两者要按时间对齐，
//! 两个文件的时间戳对不齐会得出错误结论。一个文件、一个时间轴最省事。

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::Value;

/// 调试数据的文件名（位于数据目录下的 `debug/`）
pub const FILE_NAME: &str = "debug.jsonl";

/// 调试数据所在子目录
pub const DIR_NAME: &str = "debug";

/// 写入上限的下限（MB）—— 太小会刚开就轮转
pub const MIN_MAX_MB: u16 = 1;

/// 写入上限的上限（MB）—— 防手改配置写爆磁盘
pub const MAX_MAX_MB: u16 = 500;

/// 默认上限（MB）
///
/// 取值理由：一次过夜充电（约 8 小时）产生的量约 **6 MB**
/// （1200 拍/小时 × 3 类记录 × ~150 字节）。20 MB 能盖住
/// 一整个过夜周期还有富余，同时不会让"顺手导出来发给我"变成负担。
pub const DEFAULT_MAX_MB: u16 = 20;

/// 一个**可独立实例化**的写入器（路径 + 上限 + 计数）
///
/// # ⚠️ 为什么把逻辑放在这里，而不是直接写全局
///
/// 下面那组全局函数共享一个 `static`。如果测试直接打全局，**并行跑测试时
/// 会互相踩**（一个测试 `disable()` 会打断另一个的写入）——
/// 首次实现正是这样，三个测试立刻失败。
///
/// ⇒ 状态与逻辑放进这个结构，测试用**自己的实例**，全局那层只做转发。
///    这样单测之间零共享，且不必引入 `serial_test` 之类的串行化依赖。
struct Writer {
    path: PathBuf,
    max_bytes: u64,
    written: u64,
    lines: u64,
}

impl Writer {
    /// 打开（或接续）一个调试文件
    ///
    /// 已有文件时会**恢复**大小与行数计数 —— 否则轮转会反复触发。
    fn open(path: PathBuf, max_mb: u16) -> Self {
        let max_mb = max_mb.clamp(MIN_MAX_MB, MAX_MAX_MB);
        let written = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let lines = count_lines(&path);
        Self {
            path,
            max_bytes: u64::from(max_mb) * 1024 * 1024,
            written,
            lines,
        }
    }

    /// 改上限（路径不变时）
    fn set_max_mb(&mut self, max_mb: u16) {
        self.max_bytes = u64::from(max_mb.clamp(MIN_MAX_MB, MAX_MAX_MB)) * 1024 * 1024;
    }

    /// 写一行（**已含换行**）
    fn write_line(&mut self, line: &str) {
        self.written = self.written.saturating_add(line.len() as u64);
        self.lines = self.lines.saturating_add(1);

        // 超过上限 ⇒ 先轮转，再写
        //
        // ⚠️ 顺序：轮转会重写文件，必须在下一次 append 之前完成
        if self.written > self.max_bytes {
            rotate(&self.path, self.max_bytes);
            self.written = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        }

        // ⚠️ 每次重新 open（append）而不持有 fd：
        //    这样外部 `rm` 掉文件（比如"清空全部记录"）不会让后续写入
        //    落到一个已删除的 inode 上 —— 与 `EventLog` 同一套纪律。
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

// ⚠️ 用 `static Mutex<Option<Writer>>` 而不是 `OnceLock`：
//    调试模式要能**反复开关**，而且开/关是运行期动作。
static STATE: Mutex<Option<Writer>> = Mutex::new(None);

/// 开启调试模式（幂等）
///
/// `max_mb` 会被钳到 [`MIN_MAX_MB`]~[`MAX_MAX_MB`]。
pub fn enable(path: PathBuf, max_mb: u16) {
    // 确保父目录存在 —— 调用方可能在很早的阶段（比如 `logging::init`）就开它，
    // 而那时 `paths::ensure_dirs()` 还没跑。
    //
    // 不做的话：文件创建会静默失败（`OpenOptions` 报"目录不存在"而我们
    // 忽略错误），表现为"开了调试模式但一个字节都没有"。
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }

    let Ok(mut g) = STATE.lock() else {
        return;
    };
    // 已经开着且路径相同 ⇒ 只更新上限（用户可能在界面上改了）
    if let Some(w) = g.as_mut() {
        if w.path == path {
            w.set_max_mb(max_mb);
            return;
        }
    }
    *g = Some(Writer::open(path, max_mb));
}

/// 关闭调试模式（幂等）
pub fn disable() {
    if let Ok(mut g) = STATE.lock() {
        *g = None;
    }
}

/// 当前是否开启
pub fn is_enabled() -> bool {
    STATE.lock().map(|g| g.is_some()).unwrap_or(false)
}

/// 写一行调试数据
///
/// `kind` 是内容分类（`trace` / `log` / `node` …）；
/// 时间戳由本函数统一补上（**毫秒精度** —— 排障时常要看"两件事差多久"）。
///
/// 未开启时直接返回，**零开销**。
///
/// # 为什么没有 `stats()` 这样的查询接口
///
/// 写入器的状态是**进程内**的静态量，而只有 worker 进程会开它。
/// CLI 进程（`ctl debug status`）调 `stats()` 永远返回 `None` ——
/// 没有意义。
///
/// ⇒ 状态查询走**文件本身**（`metadata().len()`），那才是唯一真相。
///    见 `commands::debug_cmd`。
pub fn write(kind: &str, fields: Value) {
    if !is_enabled() {
        return; // 先快速判断，避免为了"没开"也去拼 JSON
    }
    let line = format_line(kind, fields);
    if let Ok(mut g) = STATE.lock() {
        if let Some(w) = g.as_mut() {
            w.write_line(&line);
        }
    }
}

/// 把 `kind` + 字段拼成一行 JSON（**含换行**）
fn format_line(kind: &str, fields: Value) -> String {
    let mut obj = match fields {
        Value::Object(m) => m,
        // 非对象一律包一层 —— 保证每行都是对象，分析脚本不用做类型分叉
        other => {
            let mut m = serde_json::Map::new();
            m.insert("value".to_string(), other);
            m
        }
    };
    obj.insert("t".to_string(), Value::from(now_seconds()));
    obj.insert("kind".to_string(), Value::from(kind));

    let mut line = Value::Object(obj).to_string();
    line.push('\n');
    line
}

/// 记录一条 `log` crate 的日志（供 tee logger 调用）
///
/// 与 [`write`] 分开是为了让日志那侧的字段名固定（`level`/`target`/`msg`），
/// 分析时可以直接 grep。
pub fn write_log(level: &str, target: &str, msg: &str) {
    if !is_enabled() {
        return;
    }
    write(
        "log",
        serde_json::json!({ "level": level, "target": target, "msg": msg }),
    );
}

/// 秒级 Unix 时间戳（带毫秒小数）
///
/// ⚠️ 用 `f64` 秒而不是整数毫秒：与项目里其它时间戳（`ts`）单位一致，
///    都是"Unix 秒"，避免分析时来回换算踩错 1000 倍。
///    小数部分提供毫秒精度 —— 排障时常要看"两件事差多久"。
fn now_seconds() -> f64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs_f64(),
        Err(_) => 0.0,
    }
}

/// 数一个已有文件的行数（用于"接着上次继续写"时恢复计数）
fn count_lines(path: &Path) -> u64 {
    std::fs::read_to_string(path)
        .map(|t| t.lines().count() as u64)
        .unwrap_or(0)
}

/// 轮转：丢掉**最旧的一半**，保留最近的
///
/// # 为什么砍一半而不是"轮转成 .1"
///
/// 调试数据的价值集中在**最近**（用户刚复现的那个问题）。
/// 轮转成 `.1` / `.2` 会让"当前文件"和"上一次的文件"都只有一半新，
/// 分析时还得手动拼。砍一半则保证**单文件里永远是最新的一段连续记录**。
///
/// 频率：约 720 KB/小时 ⇒ 20 MB 上限下每 **28 小时**才轮转一次，
/// 所以"读 20 MB 进内存再重写"的成本完全可以接受。
fn rotate(path: &Path, max_bytes: u64) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let kept = keep_newest_half(&text, max_bytes / 2);
    // 写失败就只能等下次 —— 调试数据不值得为此报错中断控制循环
    let _ = std::fs::write(path, kept);
}

/// 保留最近的、总量不超过 `budget` 字节的**完整行**
///
/// ⚠️ 必须按**行**切：从中间截断会留下半行 JSON，
///    分析脚本读到最后一行会报错，而那一行往往正是最关键的那条。
///
/// ⚠️ 单行就超过 `budget` 时**仍然保留它**（`acc > 0` 才允许 break）——
///    否则超大的一行会被整个丢掉，而那可能正是出错的那条
///    （比如带了超长节点路径或错误栈的记录）。
fn keep_newest_half(text: &str, budget: u64) -> String {
    if text.len() as u64 <= budget {
        return text.to_string();
    }

    // `split_inclusive` 保留换行符 ⇒ 每段都是**完整的一行**
    //
    // ⚠️ 别改用 `char_indices().rev()` 手算行边界：首版就是这么写的，
    //    把"行长度"算成了"换行符本身到上一个边界"的距离（恒为 1），
    //    于是累计值永远凑不够 budget，函数返回空串。
    //    两个测试（`轮转按行切不留半行`、`写爆上限会轮转并保留最新内容`）
    //    立刻抓到 —— 它们断言的是"最后一行仍在"，空串必然不满足。
    let lines: Vec<&str> = text.split_inclusive('\n').collect();

    let mut acc = 0_u64;
    let mut start = lines.len();
    for (i, l) in lines.iter().enumerate().rev() {
        let len = l.len() as u64;
        if acc + len > budget && acc > 0 {
            break;
        }
        acc += len;
        start = i;
    }
    lines[start..].concat()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// 每个测试用**自己的目录与实例** —— 不碰全局状态
    ///
    /// ⚠️ 这是刻意的：直接测全局的话，`cargo test` 并行跑时会互相踩
    ///    （一个测试 `disable()` 会打断另一个的写入）。首次实现正是这样，
    ///    三个测试立刻失败。抽成 `Writer` 后测试之间零共享。
    fn tmp(tag: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "thermo_dbg_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join(FILE_NAME);
        (dir, f)
    }

    /// 未开启时全局写入是空操作，且**不创建文件**
    #[test]
    fn 未开启时不写文件() {
        disable();
        let (dir, f) = tmp("off");
        write("trace", serde_json::json!({ "x": 1 }));
        assert!(!f.exists(), "未开启调试模式时不该创建文件");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 写入的每行都是合法json且带时间与类型() {
        let (dir, f) = tmp("json");
        let mut w = Writer::open(f.clone(), DEFAULT_MAX_MB);

        w.write_line(&format_line("trace", serde_json::json!({ "temp": 38.6 })));
        w.write_line(&format_line("node", serde_json::json!({ "op": "write" })));

        let text = std::fs::read_to_string(&f).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        for l in lines {
            let v: Value = serde_json::from_str(l).expect("每行必须是合法 JSON");
            assert!(v["t"].is_number(), "必须带时间戳");
            assert!(v["kind"].is_string(), "必须带 kind");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 非对象的字段也要能写成合法行（分析脚本不必做类型分叉）
    #[test]
    fn 非对象字段被包成对象() {
        let line = format_line("trace", serde_json::json!(42));
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert!(v.is_object(), "每行都必须是对象");
        assert_eq!(v["value"], 42);
    }

    /// 🔴 轮转必须按**行**切，不能留半行
    ///
    /// 留半行的话，分析脚本读到最后一行会解析失败 ——
    /// 而那一行往往正是最关键的那条（最近的）。
    #[test]
    fn 轮转按行切不留半行() {
        let mut text = String::new();
        for i in 0..100 {
            text.push_str(&format!("{{\"n\":{i}}}\n"));
        }
        let kept = keep_newest_half(&text, 200);

        assert!(kept.len() <= 200 + 16, "应当裁到预算附近");
        // 每一行都能解析
        for l in kept.lines() {
            let _: Value = serde_json::from_str(l).expect("不得留下半行");
        }
        // 保留的是**最近的**（末尾的编号最大）
        let last: Value = serde_json::from_str(kept.lines().last().unwrap()).unwrap();
        assert_eq!(last["n"], 99, "必须保留最新的那条");
        // 且确实丢掉了旧的
        let first: Value = serde_json::from_str(kept.lines().next().unwrap()).unwrap();
        assert!(first["n"].as_i64().unwrap() > 0, "应当丢掉了开头的一部分");
    }

    #[test]
    fn 未超预算时原样保留() {
        let text = "{\"a\":1}\n{\"b\":2}\n";
        assert_eq!(keep_newest_half(text, 9999), text);
    }

    #[test]
    fn 上限被钳到合法区间() {
        let (dir, f) = tmp("clamp");

        let w = Writer::open(f.clone(), 0); // 太小
        assert_eq!(w.max_bytes, u64::from(MIN_MAX_MB) * 1024 * 1024);

        let w = Writer::open(f, 9999); // 太大
        assert_eq!(w.max_bytes, u64::from(MAX_MAX_MB) * 1024 * 1024);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 已有文件时"接着写"：计数要恢复，否则轮转会算错
    #[test]
    fn 打开时恢复已有文件的大小与行数() {
        let (dir, f) = tmp("resume");
        std::fs::write(&f, "{\"a\":1}\n{\"a\":2}\n{\"a\":3}\n").unwrap();

        let w = Writer::open(f, DEFAULT_MAX_MB);
        assert_eq!(w.lines, 3, "应当数出已有 3 行");
        assert!(w.written > 0, "应当量出已有大小");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 外部 `rm` 掉文件后，新写入不应复活已删除的内容
    ///
    /// （因为每次都是重新 open，不持有 fd —— 与 `EventLog` 同一套纪律。）
    /// 这条正对应用户点「清空全部记录」的场景。
    #[test]
    fn 外部删除文件后不会写到已删除的inode() {
        let (dir, f) = tmp("rm");
        let mut w = Writer::open(f.clone(), DEFAULT_MAX_MB);

        w.write_line(&format_line("trace", serde_json::json!({ "before": true })));

        // 模拟"清空全部记录"把文件删掉
        std::fs::remove_file(&f).unwrap();

        w.write_line(&format_line("trace", serde_json::json!({ "after": true })));

        let text = std::fs::read_to_string(&f).expect("应当重新创建了文件");
        assert!(
            !text.contains("before"),
            "删除后不该还能读到被删的内容，实得：{text}"
        );
        assert!(text.contains("after"), "新写的内容应当在，实得：{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 真的触发一次轮转：写爆之后文件应当明显变小，且内容仍是最新的
    #[test]
    fn 写爆上限会轮转并保留最新内容() {
        let (dir, f) = tmp("rotate");
        // 上限 1 MB（最小值），但我们只写 ~200 KB 也能触发 ——
        // 直接构造一个"上限很小"的 Writer 更干脆
        let mut w = Writer::open(f.clone(), MIN_MAX_MB);
        w.max_bytes = 4096; // 手动压到 4 KB，便于测试

        for i in 0..400 {
            w.write_line(&format_line("trace", serde_json::json!({ "i": i })));
        }

        let text = std::fs::read_to_string(&f).unwrap();
        assert!(text.len() < 4096, "轮转后应当小于上限，实得 {}", text.len());

        // 每一行仍可解析
        for l in text.lines() {
            let _: Value = serde_json::from_str(l).expect("轮转后不得留下半行");
        }
        // 最后一条是刚写的
        let last: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(last["i"], 399, "应当保留最新写入的那条");
        std::fs::remove_dir_all(&dir).ok();
    }
}
