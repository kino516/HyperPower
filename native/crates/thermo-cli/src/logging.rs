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

    #[cfg(target_os = "android")]
    {
        android_logger::init_once(
            android_logger::Config::default()
                .with_tag("thermo_charge")
                .with_max_level(level),
        );
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
