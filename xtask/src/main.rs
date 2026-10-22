//! xtask —— 构建编排入口
//!
//! 用法：
//! ```text
//! cargo xtask build [--variant release|debug]
//! cargo xtask check
//! cargo xtask pack  [--variant release|debug]
//! cargo xtask push  [--dest /data/local/tmp]
//! cargo xtask install [--via ksud|apatch|magisk]
//! cargo xtask help
//! ```
//!
//! ⚠️ 参数解析为手写实现（不使用 clap）：见 Cargo.toml 的依赖策略说明。

mod fs_walk;
mod pack;

use std::fs;
use std::process::Command;

use anyhow::{Context, Result};

/// Android 目标三元组
const TARGET: &str = "aarch64-linux-android";

/// 产物二进制名
const BIN_NAME: &str = "thermo";

const USAGE: &str = "\
HyperPower 构建编排

用法: cargo xtask <命令> [选项]

命令:
  build     交叉编译 Rust 引擎
  check     格式检查 + clippy + 测试
  pack      组装模块 zip（含 SHA-256 清单与 16 KB 对齐检查）
  webui     构建 WebUI 并复制到 module/src/webroot/
  push      adb push 模块 zip 到设备
  install   经管理器安装到设备
  help      显示本帮助

选项:
  --variant <release|debug>   构建变体（默认 release）
  --dest <路径>               push 目标目录（默认 /data/local/tmp）
  --via <ksud|apatch|magisk>  安装方式（默认 ksud）

⚠️ 打包（pack）是生成 checksum.sha256 的**唯一实现**：
   CI 也调用本命令，不再自己用 shell 重复一遍规则
   （见 docs/08-交接摘要.md §3.2）。
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Variant {
    Debug,
    Release,
}

impl Variant {
    fn is_release(self) -> bool {
        matches!(self, Self::Release)
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Release => "release",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Installer {
    Ksud,
    Apatch,
    Magisk,
}

#[derive(Debug)]
enum Command_ {
    Build(Variant),
    Check,
    Pack(Variant),
    /// 构建 WebUI 并放入 `module/src/webroot/`
    Webui,
    Push(String),
    Install(Installer),
    Help,
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let cmd = match parse(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("参数错误: {e}");
            eprintln!();
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };

    match cmd {
        Command_::Build(v) => build(v),
        Command_::Check => check(),
        Command_::Pack(v) => {
            // CI 场景：二进制已由前一个 job 构建并作为 artifact 下载下来，
            // 此时设置 THERMO_SKIP_BUILD=1 跳过重复编译。
            if std::env::var("THERMO_SKIP_BUILD").is_err() {
                build(v)?;
            } else {
                println!("==> 跳过编译（THERMO_SKIP_BUILD 已设置，使用已有二进制）");
            }
            pack::pack(v.is_release())
        }
        Command_::Webui => webui(),
        Command_::Push(dest) => push(&dest),
        Command_::Install(via) => install(via),
        Command_::Help => {
            println!("{USAGE}");
            Ok(())
        }
    }
}

/// 手写参数解析
fn parse(args: &[String]) -> Result<Command_> {
    if args.is_empty() {
        anyhow::bail!("缺少命令（可用: build / check / pack / push / install / help）");
    }

    let cmd = args[0].as_str();
    let rest = &args[1..];

    // 提取选项
    let mut variant = Variant::Release;
    let mut dest = "/data/local/tmp".to_string();
    let mut via = Installer::Ksud;

    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--variant" | "-v" => {
                let v = rest.get(i + 1).context("--variant 缺少取值")?;
                variant = match v.as_str() {
                    "release" => Variant::Release,
                    "debug" => Variant::Debug,
                    other => anyhow::bail!("未知变体: {other}（可用 release / debug）"),
                };
                i += 2;
            }
            "--dest" | "-d" => {
                dest = rest.get(i + 1).context("--dest 缺少取值")?.clone();
                i += 2;
            }
            "--via" => {
                let v = rest.get(i + 1).context("--via 缺少取值")?;
                via = match v.as_str() {
                    "ksud" | "kernelsu" => Installer::Ksud,
                    "apatch" | "apd" => Installer::Apatch,
                    "magisk" => Installer::Magisk,
                    other => anyhow::bail!("未知安装方式: {other}"),
                };
                i += 2;
            }
            "-h" | "--help" => return Ok(Command_::Help),
            other => anyhow::bail!("未知选项: {other}"),
        }
    }

    Ok(match cmd {
        "build" => Command_::Build(variant),
        "check" => Command_::Check,
        "pack" => Command_::Pack(variant),
        "webui" => Command_::Webui,
        "push" => Command_::Push(dest),
        "install" => Command_::Install(via),
        "help" => Command_::Help,
        other => anyhow::bail!("未知命令: {other}"),
    })
}

/// 构建 WebUI 并把产物复制到 `module/src/webroot/`
///
/// 契约（§4.4）：入口必须是 `webroot/index.html`，且资源路径为**相对**路径
/// （Vite 需配 `base: "./"`）—— 绝对路径从模块目录加载会失效。
fn webui() -> Result<()> {
    let root = repo_root()?;
    let webui = root.join("webui");

    if !webui.join("package.json").exists() {
        anyhow::bail!(
            "未找到 webui/package.json —— WebUI 尚未初始化（{}）",
            webui.display()
        );
    }

    println!("==> 构建 WebUI（Vite）");

    // ⚠️ 门禁：源 `index.html` 的注释里不得含连续连字符
    //
    // HTML 规范禁止注释内出现 `--`。markdown 表格分隔线 `|---|---|`
    // 正好含它，会引发解析错误并吞掉后续标签 —— 本项目因此丢过
    // Vite 注入的 `<script>`（详见 docs/12-WebUI入口文件说明.md §4）。
    //
    // 在**构建前**检查源文件：报错时指向开发者刚写下的那行，
    // 比构建后从产物倒推容易定位得多。
    let src_index = webui.join("index.html");
    let src_html = fs::read_to_string(&src_index)
        .with_context(|| format!("读取 {} 失败", src_index.display()))?;
    let bad_comments = illegal_comment_sequences(&src_html);
    if !bad_comments.is_empty() {
        let first = bad_comments.first().map(String::as_str).unwrap_or("");
        anyhow::bail!(
            "webui/index.html 的 HTML 注释里含连续两个连字符 `--`（共 {} 处）。\n\
             HTML 规范禁止这种写法，会导致解析器吞掉后续标签（如 Vite 注入的 <script>），\n\
             界面将永远停在骨架转圈。最常见来源是 markdown 表格分隔线。\n\
             第一处：…{first}…\n\
             请把说明移到 docs/12-WebUI入口文件说明.md，源文件里不留注释。",
            bad_comments.len()
        );
    }
    println!("    ✓ 门禁通过：源 index.html 无非法注释序列");

    // ⚠️ Windows 上 npm/pnpm 是 `.cmd` 批处理，Rust 的 `Command::new` 无法直接
    //    执行（会报 program not found）。跨平台统一经一个包管理命令构造器。
    let pm = pick_package_manager();
    println!("    包管理器: {}", pm.program);

    let status = pm
        .command(&webui, &["install"])
        .status()
        .with_context(|| format!("执行 {} install 失败", pm.program))?;
    if !status.success() {
        anyhow::bail!("依赖安装失败");
    }

    let status = pm
        .command(&webui, &["run", "build"])
        .status()
        .context("执行构建失败")?;
    if !status.success() {
        anyhow::bail!("WebUI 构建失败");
    }

    // 产物 → module/src/webroot/
    let dist = webui.join("dist");
    if !dist.join("index.html").exists() {
        anyhow::bail!(
            "构建产物缺少 index.html（{}）—— 管理器要求 webroot/index.html 为入口",
            dist.display()
        );
    }

    // ⚠️ 门禁：产物里**不得出现 `crossorigin` 属性**
    //
    // 真机实测（见 docs/11 §5 D-W1）：Vite 默认给注入的
    // `<script type="module">` / `<link>` 加 `crossorigin`，而 KernelSU 的
    // WebViewAssetLoader **不给响应带 CORS 头** → 浏览器按 CORS 模式请求被拒
    // → 脚本整块不执行 → **整页空白**。
    //
    // vite.config.ts 里已有 `stripCrossorigin()` 插件处理，但那是配置；
    // 这里做**产物级**校验，防止插件被误删或后续升级 Vite 后行为变化。
    let index_html =
        fs::read_to_string(dist.join("index.html")).context("读取产物 index.html 失败")?;
    if has_crossorigin_attr(&index_html) {
        anyhow::bail!(
            "产物 index.html 含 `crossorigin` 属性 —— 会导致 KernelSU WebView 里整页空白。\n\
             请检查 vite.config.ts 的 `stripCrossorigin()` 插件是否仍在 `plugins` 数组中。\n\
             详见 docs/11-WebUI实现与真机踩坑.md §5 D-W1"
        );
    }
    println!("    ✓ 门禁通过：产物无 crossorigin 属性");

    // ⚠️ 门禁：产物必须真的引用了 JS 与 CSS
    //
    // 起因是一次真实事故：`webui/index.html` 的 HTML 注释里写了 markdown 表格
    // （分隔线 `|---|---|` 含连续两个连字符），而 **HTML 规范禁止注释内出现 `--`**。
    // 解析器因此错乱，把注释**之后**的标签一并吞掉 —— 而 Vite 注入的
    // `<script type="module">` 恰好在那之后。
    //
    // 后果：产物 `index.html` 里 `<script>` 标签数 = 0。
    // 页面能显示内联骨架（HTML/CSS 正常），但 JS 永不执行，表现为
    // **永远停在转圈**（看起来像"加载很慢"，实则是根本没加载）。
    // 而且**构建成功、无警告、类型检查与测试全过**。
    //
    // 因此这里做产物级断言：script 与 CSS link 必须存在。
    // 详见 docs/12-WebUI入口文件说明.md §4。
    let script_count = index_html.matches("<script").count();
    if script_count == 0 {
        anyhow::bail!(
            "产物 index.html 里没有任何 <script> 标签 —— JS 不会执行，界面会永远停在骨架。\n\
             最常见原因：`webui/index.html` 的 HTML 注释里含连续两个连字符（如 markdown 表格\n\
             分隔线 `|---|---|`），HTML 规范禁止这种写法，解析器会把后续标签一起吞掉。\n\
             详见 docs/12-WebUI入口文件说明.md §4"
        );
    }
    // Vite 注入的 CSS link 必须存在（否则 miuix 组件全是裸元素）
    if !index_html.contains("./assets/") && !index_html.contains("assets/") {
        anyhow::bail!(
            "产物 index.html 没有引用任何 assets/ 下的资源 —— 入口标签可能被注释吞掉了。\n\
             详见 docs/12-WebUI入口文件说明.md §4"
        );
    }
    println!("    ✓ 门禁通过：产物含 {script_count} 个 script 且引用了 assets/");

    // ⚠️ 门禁：两个虚拟域名样式表必须存在
    //
    // `mui.kernelsu.org` 是 KernelSU 的虚拟域名（WebViewAssetLoader 本地服务）。
    //   · internal/colors.css → 系统壁纸取色（Monet）
    //   · internal/insets.css → 边到边显示 + 安全区变量
    // 缺任何一个都会让界面失去"原生观感"。
    //
    // 它们写在 `index.html` 的 `<link>` 里（不是 CSS 的 `@import` ——
    // 后者是串行阻塞的，真机实测会让首屏变慢）。所以这里查 HTML。
    for need in ["internal/colors.css", "internal/insets.css"] {
        if !index_html.contains(need) {
            anyhow::bail!(
                "产物 index.html 缺少 `{}` 的 <link> —— Monet 取色 / 边到边显示会失效。\n\
                 请检查 webui/index.html 里的两个虚拟域名 <link> 是否被删除。",
                need
            );
        }
    }
    println!("    ✓ 门禁通过：Monet 与边到边样式表均在产物中");

    let target = root.join("module").join("src").join("webroot");
    if target.exists() {
        fs::remove_dir_all(&target).with_context(|| {
            // ⚠️ Windows 上最常见的原因是**有程序占用该目录里的文件**
            //    （例如为调试起过 `python -m http.server`、或资源管理器
            //    在这个目录里开着）。
            //
            // 真机踩过一次：占用导致 `remove_dir_all` 报 `os error 32`，
            // 而我用 `Select-String` 过滤输出时把 `Error:` 行吃掉了 ——
            // 看起来像"构建成功"，实际 `webroot/` 是**空的**，
            // 打出来的 zip 里一个 WebUI 文件都没有。
            format!(
                "清空 {} 失败 —— 是否有程序正占用该目录？\n\
                 （常见：调试用的 `python -m http.server`、资源管理器窗口）\n\
                 关掉占用后重试",
                target.display()
            )
        })?;
    }
    copy_dir(&dist, &target)?;

    // ⚠️ 门禁：复制结果**必须非空**
    //
    // 上面 `remove_dir_all` + `copy_dir` 的组合有个静默失效模式：
    // 删成功、拷失败（或拷到一半被中断）⇒ 产出空目录，
    // 而后续 `pack` **不检查** webroot 内容，于是打出一个
    // "安装成功但 WebUI 全白"的包。
    //
    // 这里显式核对，把问题挡在打包之前。
    let copied = count_files(&target);
    if copied == 0 || !target.join("index.html").exists() {
        anyhow::bail!(
            "复制后 {} 为空或缺少 index.html（{copied} 个文件）——\
             打出的 zip 会没有 WebUI。请重试；若反复出现，检查目录占用。",
            target.display()
        );
    }
    println!("    ✓ 已复制到 {}（{copied} 个文件）", target.display());

    Ok(())
}

/// 统计目录下的文件数（递归）
fn count_files(dir: &std::path::Path) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| {
            if e.path().is_dir() {
                count_files(&e.path())
            } else {
                1
            }
        })
        .sum()
}

/// 找出 HTML 注释里**非法**的连续连字符序列
///
/// # 背景（真实事故）
///
/// HTML 规范禁止注释内出现连续两个连字符 `--`（`<!--` 与 `-->` 已占用
/// 这些序列）。但 markdown 表格的分隔线 `|---|---|` 正好含 `--`：
///
/// ```html
/// <!--
///   | # | 要点 | 后果 |
///   |---|---|---|          ← 非法，但肉眼完全看不出问题
/// -->
/// ```
///
/// 解析器遇到会**提前终止注释或错乱**，把注释**之后**的标签一并吞掉。
/// 本项目因此丢掉过 Vite 注入的 `<script>` —— 产物里 script 数 = 0，
/// 界面永远停在骨架转圈，而**构建成功、无警告、测试全过**。
///
/// 详见 docs/12-WebUI入口文件说明.md §4。
///
/// 返回每个非法位置所在的注释片段（便于报错时定位）。
fn illegal_comment_sequences(html: &str) -> Vec<String> {
    let mut bad = Vec::new();
    let mut rest = html;

    while let Some(start) = rest.find("<!--") {
        let after = &rest[start + 4..];
        let Some(end) = after.find("-->") else {
            // 未闭合注释本身也是问题，但那是另一类错误
            break;
        };
        let inner = &after[..end];

        if inner.contains("--") {
            // 取第一处非法的上下文，够定位就行。
            //
            // ⚠️ 必须按**字符**切，不能按字节：注释里几乎一定有中文，
            //    字节切片会落在多字节字符中间并 panic（本函数的第一版就栽在这）。
            if let Some(i) = inner.find("--") {
                // `i` 是字节位移，先转成字符数再看左边留多少
                let char_idx = inner[..i].chars().count();
                let from = char_idx.saturating_sub(20);
                let snippet: String = inner
                    .chars()
                    .skip(from)
                    .take(60)
                    .collect::<String>()
                    .replace('\n', " ");
                bad.push(snippet);
            }
        }

        rest = &after[end + 3..];
    }
    bad
}

/// 剥离 HTML 注释（含未闭合时的剩余部分）
fn strip_html_comments(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;

    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + 3..],
            // 未闭合：后面全是注释，丢弃
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// 判断 HTML 里是否存在**真实的** `crossorigin` 属性
///
/// # 为什么不能直接 `html.contains("crossorigin")`
///
/// 入口文件 `webui/index.html` 的**说明性注释**里就写着这个词
/// （用来解释"为什么要剥掉它"）。虽然与之配套的 `stripHtmlComments()`
/// 插件会删掉注释，但门禁不该依赖那个前提 —— 否则一旦插件被误删，
/// 门禁会误报，而误报的排查成本比漏报更高。
///
/// # 为什么必须先剥注释
///
/// 朴素做法是"看 `crossorigin` 前面最近的 `<` 是否比最近的 `>` 更近"，
/// 但**注释里的 `<` 会骗过它**：
///
/// ```html
/// <head>
/// <!-- 讲解 crossorigin 的注释 -->   ← `<!--` 的 `<` 比 `<head>` 的 `>` 更近
/// ```
///
/// 于是注释里的词被当成属性。本函数先用 [`strip_html_comments`] 去掉注释，
/// 再做逐字符的标签扫描 —— 只有 `<` … `>` 之间的内容才算属性区。
fn has_crossorigin_attr(html: &str) -> bool {
    let code = strip_html_comments(html);

    let mut in_tag = false;
    let mut attr = String::new();

    // 用 `chars()` 迭代（不是字节索引）：HTML 里有中文，字节切分会 panic
    for ch in code.chars() {
        match ch {
            '<' => {
                in_tag = true;
                attr.clear();
            }
            '>' => {
                if in_tag && attr.contains("crossorigin") {
                    return true;
                }
                in_tag = false;
                attr.clear();
            }
            _ if in_tag => attr.push(ch),
            _ => {}
        }
    }
    false
}

/// 包管理器选择结果
struct PackageManager {
    /// 实际执行的程序（Windows 上为 `cmd`，参数里带上真正的包管理器）
    program: String,
    /// 前置参数（Windows 用于 `cmd /C`）
    prefix: Vec<String>,
}

impl PackageManager {
    /// 构造一个可在当前平台执行的命令
    fn command(&self, dir: &std::path::Path, args: &[&str]) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.current_dir(dir);
        cmd.args(&self.prefix);
        cmd.args(args);
        cmd
    }
}

/// 选一个可用的包管理器
///
/// 优先 `pnpm`（仓库里若有 `pnpm-lock.yaml` 则必须用它），否则退到 `npm`。
fn pick_package_manager() -> PackageManager {
    let prefer_pnpm = repo_root()
        .map(|r| r.join("webui").join("pnpm-lock.yaml").exists())
        .unwrap_or(false);
    let name = if prefer_pnpm && which("pnpm") {
        "pnpm"
    } else if which("npm") {
        "npm"
    } else {
        "pnpm"
    };

    // Windows：npm/pnpm 是 .cmd，必须经 cmd 解释器执行
    #[cfg(windows)]
    {
        PackageManager {
            program: "cmd".to_string(),
            prefix: vec!["/C".to_string(), name.to_string()],
        }
    }
    #[cfg(not(windows))]
    {
        PackageManager {
            program: name.to_string(),
            prefix: Vec::new(),
        }
    }
}

/// 判断命令是否可用（Windows 上也要能找到 `.cmd`）
fn which(cmd: &str) -> bool {
    // Windows 走 where，其余平台走 which —— 两者都比"直接执行"可靠
    let probe = if cfg!(windows) { "where" } else { "which" };
    Command::new(probe)
        .arg(cmd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// 递归复制目录
fn copy_dir(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// 仓库根目录
fn repo_root() -> Result<std::path::PathBuf> {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").context("缺少 CARGO_MANIFEST_DIR")?;
    let root = std::path::Path::new(&manifest)
        .parent()
        .context("无法定位仓库根目录")?;
    Ok(root.to_path_buf())
}

/// 交叉编译
fn build(variant: Variant) -> Result<()> {
    let root = repo_root()?;
    let native = root.join("native");

    println!("==> 交叉编译 {TARGET}（{}）", variant.as_str());

    let mut cmd = Command::new("cargo");
    cmd.current_dir(&native)
        .args(["build", "--target", TARGET, "-p", "thermo-cli"]);
    if variant.is_release() {
        cmd.arg("--release");
    }

    let status = cmd.status().context("执行 cargo build 失败")?;
    if !status.success() {
        anyhow::bail!("cargo build 失败（退出码 {:?}）", status.code());
    }

    let out = native
        .join("target")
        .join(TARGET)
        .join(variant.as_str())
        .join(BIN_NAME);
    if !out.exists() {
        anyhow::bail!("未找到产物: {}", out.display());
    }
    let size = std::fs::metadata(&out)?.len();
    println!(
        "    产物: {} ({:.1} KB)",
        out.display(),
        size as f64 / 1024.0
    );

    Ok(())
}

/// 质量检查
fn check() -> Result<()> {
    let root = repo_root()?;
    let native = root.join("native");

    let steps: [(&str, Vec<&str>); 3] = [
        ("格式检查", vec!["fmt", "--all", "--check"]),
        (
            "Clippy",
            vec![
                "clippy",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings",
            ],
        ),
        ("测试", test_args()),
    ];

    for (desc, args) in steps {
        println!("==> {desc}");
        let status = Command::new("cargo")
            .current_dir(&native)
            .args(&args)
            .status()
            .with_context(|| format!("执行 cargo {} 失败", args.join(" ")))?;
        if !status.success() {
            anyhow::bail!("{desc} 未通过");
        }
    }

    println!("✓ 全部检查通过");
    Ok(())
}

/// 测试参数（按平台选择范围）
///
/// ⚠️ Windows 上无法 `--workspace`：`cargo test` 需要**原生**构建
///    `thermo-cli`（为二进制建测试宿主），而它依赖 `clap` → `windows-sys`；
///    后者在本机 GNU 工具链下编译失败（rustup 自带 dlltool 的 CreateProcess
///    缺陷，详见 README「环境与工具链」）。故 Windows 上只测纯逻辑 crate，
///    **CI（ubuntu-24.04）跑全量**。
fn test_args() -> Vec<&'static str> {
    if cfg!(windows) {
        println!("    注：Windows 上仅测试纯逻辑 crate（thermo-cli 的原生构建受");
        println!("        dlltool 限制）；CI 会跑全量。详见 README「环境与工具链」。");
        vec!["test", "-p", "thermo-core", "-p", "thermo-ipc"]
    } else {
        vec!["test", "--workspace"]
    }
}

/// adb push
fn push(dest: &str) -> Result<()> {
    let root = repo_root()?;
    // 产物名带版本号（与 `cargo xtask pack` 一致）
    let ver = crate::pack::module_version(&root.join("module").join("src"))?;
    let zip = root.join("dist").join(format!("HyperPower-v{ver}.zip"));
    if !zip.exists() {
        anyhow::bail!(
            "未找到模块 zip（请先运行 cargo xtask pack）: {}",
            zip.display()
        );
    }

    println!("==> adb push {} → {dest}", zip.display());
    let status = Command::new("adb")
        .args(["push", &zip.to_string_lossy(), dest])
        .status()
        .context("执行 adb push 失败（adb 是否在 PATH？）")?;
    if !status.success() {
        anyhow::bail!("adb push 失败");
    }

    println!("✓ 已推送。下一步：cargo xtask install --via ksud");
    Ok(())
}

/// 经管理器安装
fn install(via: Installer) -> Result<()> {
    let root = repo_root()?;
    // 与 push 的目标名保持一致（产物名带版本号）
    let ver = crate::pack::module_version(&root.join("module").join("src"))?;
    let modpath = format!("/data/local/tmp/HyperPower-v{ver}.zip");
    let cmd = match via {
        Installer::Ksud => format!("ksud module install {modpath}"),
        Installer::Apatch => format!("apd module install {modpath}"),
        Installer::Magisk => format!("magisk --install-module {modpath}"),
    };

    println!("==> adb shell su -c '{cmd}'");
    let status = Command::new("adb")
        .args(["shell", "su", "-c", &cmd])
        .status()
        .context("执行安装命令失败")?;
    if !status.success() {
        anyhow::bail!("安装失败（退出码 {:?}）", status.code());
    }

    println!("✓ 安装命令已执行。请重启设备以生效。");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{has_crossorigin_attr, illegal_comment_sequences};

    #[test]
    fn 检出真实的_crossorigin_属性() {
        assert!(has_crossorigin_attr(
            r#"<script type="module" crossorigin src="./a.js"></script>"#
        ));
        // 属性在 tag 末尾、紧跟 `>`（无空格）
        assert!(has_crossorigin_attr(r#"<link crossorigin>"#));
        // 前面还有别的属性
        assert!(has_crossorigin_attr(
            r#"<link rel="modulepreload" crossorigin href="./v.js">"#
        ));
    }

    #[test]
    fn 不把注释里的_crossorigin_当属性() {
        // 这正是本项目入口文件的真实形态：注释里讲解"为什么要剥掉它"
        let html = r#"<head>
<!-- ⚠️ `crossorigin` 必须被剥掉，否则整页空白 -->
<script type="module" src="./a.js"></script>
</head>"#;
        assert!(
            !has_crossorigin_attr(html),
            "注释里的词被误判为属性 —— 门禁会误报"
        );
    }

    #[test]
    fn 不把标签外的文本当属性() {
        assert!(!has_crossorigin_attr("crossorigin"));
        assert!(!has_crossorigin_attr("<p>crossorigin 是什么？</p>"));
        // 标签闭合之后的裸文本
        assert!(!has_crossorigin_attr("<div></div>crossorigin"));
    }

    #[test]
    fn 无_crossorigin_时不误报() {
        assert!(!has_crossorigin_attr(
            r#"<script type="module" src="./a.js"></script><link rel="stylesheet" href="./a.css">"#
        ));
        assert!(!has_crossorigin_attr(""));
    }

    #[test]
    fn 多个出现时任一命中即检出() {
        // 第一个在注释里（不算），第二个是真属性（算）
        let html = r#"<!-- crossorigin --><script crossorigin src="./a.js"></script>"#;
        assert!(has_crossorigin_attr(html));
    }

    // ── 注释合法性 ──
    //
    // 这组测试对应一次真实事故：入口文件的 HTML 注释里写了 markdown 表格，
    // 分隔线 `|---|---|` 含连续两个连字符，HTML 规范禁止此写法，
    // 导致解析器把后续的 <script> 标签一起吞掉 → 产物里 script 数 = 0
    // → 界面永远停在骨架转圈。
    // 详见 docs/12-WebUI入口文件说明.md §4

    #[test]
    fn 注释里的_markdown_表格分隔线被检出() {
        let bad = r#"<head>
<!--
  | # | 要点 | 后果 |
  |---|---|---|
-->
</head>"#;
        let found = illegal_comment_sequences(bad);
        assert!(
            !found.is_empty(),
            "注释里的 `---|---|---|` 应被检出（它含非法的连续连字符）"
        );
    }

    #[test]
    fn 正常注释不误报() {
        let ok = r#"<head>
<!-- 单行说明 -->
<link rel="stylesheet" href="./a.css">
<script type="module" src="./a.js"></script>
</head>"#;
        assert!(
            illegal_comment_sequences(ok).is_empty(),
            "普通注释不该被判为非法"
        );
    }

    #[test]
    fn 没有注释时不误报() {
        let ok = r#"<head><script type="module" src="./a.js"></script></head>"#;
        assert!(illegal_comment_sequences(ok).is_empty());
    }

    #[test]
    fn 检出被吞掉标签的产物() {
        // 复现事故产物：有骨架 <style>，但 script 数为 0
        let broken = r#"<!doctype html>
<html><head>
<link rel="stylesheet" href="https://mui.kernelsu.org/internal/colors.css">
<style>#app>.boot{height:100vh}</style>
</head><body><div id="app"><div class="boot"></div></div></body></html>"#;
        assert_eq!(
            broken.matches("<script").count(),
            0,
            "这份产物应被判定为 script 缺失"
        );

        let good = r#"<!doctype html>
<html><head>
<link rel="stylesheet" href="./assets/index-x.css">
</head><body><div id="app"></div>
<script type="module" src="./assets/index-y.js"></script></body></html>"#;
        assert_eq!(good.matches("<script").count(), 1);
        assert!(good.contains("assets/"));
    }
}
