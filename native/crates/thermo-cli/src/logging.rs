//! 日志初始化
//!
//! | 平台 | 后端 | 去向 |
//! |---|---|---|
//! | Android | `android_logger` | **logcat**（现场排障必需） |
//! | 其他 Unix（含 CI） | `env_logger` | stderr |
//! | Windows | 内置简易实现 | stderr |
//!
//! # 为什么 Windows 不用 `env_logger`
//!
//! `env_logger` 会拉入 `anstream` → `windows-sys`，而本机 GNU 工具链的
//! `dlltool` 无法为 `windows-sys` 生成导入库（交接摘要 §6.3 坑 2）。
//! Windows 只用于开发与跑测试，因此用一个十几行的实现替代 ——
//! 换来的是「`thermo-cli` 能在本机编译并跑全部核心测试」。

/// 初始化日志后端
pub fn init(verbose: bool) {
    let level = if verbose {
        log::LevelFilter::Debug
    } else {
        log::LevelFilter::Info
    };

    // 🔴 **尽早**打开调试写入器（用户要求 2026-10-17）
    //
    // # 为什么必须在这里，而不是等 worker 第一拍
    //
    // 首版把 `debug_log::enable` 放在 `engine::step` 里（每拍按配置同步），
    // 于是**启动阶段的所有日志都记不到** —— 真机实测：logcat 里有一堆
    // worker 启动日志（档位表读取、节点探测、检测结果），而 `debug.jsonl`
    // 里**一行 log 都没有**。
    //
    // 而启动阶段恰恰是排障最需要的一段：
    //   · 节点探测命中了哪些、哪些没命中
    //   · 设备档案是怎么读出来的
    //   · 配置有没有被迁移、被钳位
    //
    // ⇒ 在装日志器**之前**就把写入器打开，第一条日志起就能落盘。
    //
    // ⚠️ 读配置用**裸 JSON 解析**而不是 `ConfigStore::load`：
    //    后者会补默认值、跑迁移并**写盘**，不适合在日志初始化阶段做
    //    （那时还没进主流程，不该产生副作用）。
    init_debug_writer_from_config();

    #[cfg(target_os = "android")]
    {
        install_android_tee(level);
    }

    #[cfg(all(not(target_os = "android"), not(windows)))]
    {
        let _ = env_logger::Builder::new().filter_level(level).try_init();
    }

    #[cfg(windows)]
    {
        let _ = StderrLogger::install(level);
    }
}

/// 若配置里开着调试模式，就把写入器打开（**纯读，无副作用**）
///
/// 读不到文件 / 解析失败 / 没开 ⇒ 什么都不做。
fn init_debug_writer_from_config() {
    let Ok(text) = std::fs::read_to_string(crate::paths::config_json()) else {
        return;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return;
    };
    if v.pointer("/debug/enabled")
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        return;
    }
    let max_mb = v
        .pointer("/debug/max_mb")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| u16::try_from(n).ok())
        .unwrap_or(crate::debug_log::DEFAULT_MAX_MB);
    crate::debug_log::enable(crate::paths::debug_jsonl(), max_mb);
}

// ============================================================
//  Android：logcat + 调试文件 的双写
// ============================================================

/// 装一个 **tee 日志器**：同时写 logcat 与调试文件
///
/// # 为什么需要它（用户要求 2026-10-17）
///
/// 「把运行日志落盘」—— 现在日志只进 **logcat**，而 logcat 是**环形缓冲**：
/// 第二天再去导出，出问题那段的日志早被冲掉了。用户报障时最需要的恰恰是
/// 「出问题那一刻的日志」，所以必须能在**开启调试模式后**落盘。
///
/// # 为什么不用 `android_logger::init_once`
///
/// 它内部 `log::set_logger(...)`，而 `log` 只允许装**一个** logger ——
/// 装了它就装不下我们的 tee。
///
/// 好在 `android_logger` 把两样东西都公开了：
///
/// | 接口 | 用途 |
/// |---|---|
/// | `AndroidLogger::new(config)` | 自己造一个**带 tag 与级别**的实例 |
/// | `impl log::Log for AndroidLogger` | 直接调它的 `log()` 写 logcat |
///
/// ⇒ 包一层 [`TeeLogger`]，先写 logcat（现场排障照旧），再写调试文件。
///
/// ⚠️ 用 `static` + `log::set_logger(&'static dyn Log)` 而不是
///    `set_boxed_logger` —— 后者需要 `log/std` 特性，而本项目没开
///    （见文件顶部注释里 Windows 那段的同款理由）。
#[cfg(target_os = "android")]
fn install_android_tee(level: log::LevelFilter) {
    use std::sync::OnceLock;

    use log::{Log, Metadata, Record};

    /// 双写日志器
    struct TeeLogger {
        /// logcat 那一半（带 tag 与级别的实例）
        android: android_logger::AndroidLogger,
        level: log::LevelFilter,
    }

    impl Log for TeeLogger {
        fn enabled(&self, metadata: &Metadata) -> bool {
            metadata.level() <= self.level
        }

        fn log(&self, record: &Record) {
            if !self.enabled(record.metadata()) {
                return;
            }
            // ① logcat —— **永远写**，现场排障少不了它
            self.android.log(record);
            // ② 调试文件 —— 只有开了调试模式才落盘（内部自会判断）
            crate::debug_log::write_log(
                record.level().as_str(),
                record.target(),
                &record.args().to_string(),
            );
        }

        fn flush(&self) {}
    }

    // `AndroidLogger::new` 把 config 存进 `OnceLock`，所以实例必须是静态的
    static LOGGER: OnceLock<TeeLogger> = OnceLock::new();
    let logger = LOGGER.get_or_init(|| TeeLogger {
        android: android_logger::AndroidLogger::new(
            android_logger::Config::default()
                .with_tag("hyperpower")
                .with_max_level(level),
        ),
        level,
    });

    if log::set_logger(logger).is_ok() {
        log::set_max_level(level);
    }
}

// ============================================================
//  Windows 内置日志器
// ============================================================

#[cfg(windows)]
mod builtin {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use log::{Level, LevelFilter, Metadata, Record};

    /// 日志级别（`log` 的 `LevelFilter` 不是 `const` 可构造的，故存其数值）
    static LEVEL: AtomicUsize = AtomicUsize::new(LevelFilter::Info as usize);

    /// 极简 stderr 日志器（Windows 专用）
    ///
    /// 用**零大小的静态实例**：不依赖 `log/std` 特性下的
    /// `set_boxed_logger`（那需要堆分配），因此在任何特性组合下都能装。
    pub struct StderrLogger;

    /// 全局唯一实例
    static LOGGER: StderrLogger = StderrLogger;

    impl StderrLogger {
        /// 安装为全局日志器；已被安装则返回 `Err`
        pub fn install(level: LevelFilter) -> Result<(), log::SetLoggerError> {
            LEVEL.store(level as usize, Ordering::Relaxed);
            log::set_logger(&LOGGER)?;
            log::set_max_level(level);
            Ok(())
        }

        fn level(&self) -> LevelFilter {
            // 数值来自上面 store 的 LevelFilter
            match LEVEL.load(Ordering::Relaxed) {
                0 => LevelFilter::Off,
                1 => LevelFilter::Error,
                2 => LevelFilter::Warn,
                3 => LevelFilter::Info,
                4 => LevelFilter::Debug,
                _ => LevelFilter::Trace,
            }
        }
    }

    impl log::Log for StderrLogger {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            metadata.level() <= self.level()
        }

        fn log(&self, record: &Record<'_>) {
            if !self.enabled(record.metadata()) {
                return;
            }
            let tag = match record.level() {
                Level::Error => "ERROR",
                Level::Warn => "WARN ",
                Level::Info => "INFO ",
                Level::Debug => "DEBUG",
                Level::Trace => "TRACE",
            };
            // 单行输出：形态与 Android 侧 logcat 保持一致，便于对照日志
            eprintln!("[{tag}] {}", record.args());
        }

        fn flush(&self) {}
    }
}

#[cfg(windows)]
use builtin::StderrLogger;
