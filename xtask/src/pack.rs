//! 模块打包：组装 → 反向断言 → 16 KB 检查 → SHA-256 → zip

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::fs_walk;

const TARGET: &str = "aarch64-linux-android";
const BIN_NAME: &str = "thermo";

/// 生成 SHA-256 清单时排除的文件
///
/// - `checksum.sha256`：清单自身
/// - `customize.sh` / `verify.sh`：安装期脚本，KernelSU 执行后即删除，
///   记入清单会让设备端校验**永久误报**
const CHECKSUM_EXCLUDE: &[&str] = &["checksum.sha256", "customize.sh", "verify.sh"];

/// 打包
pub fn pack(release: bool) -> Result<()> {
    let root = repo_root()?;
    let src = root.join("module").join("src");
    let dist = root.join("dist");
    let variant = if release { "release" } else { "debug" };

    let bin = root
        .join("native")
        .join("target")
        .join(TARGET)
        .join(variant)
        .join(BIN_NAME);
    if !bin.exists() {
        anyhow::bail!("未找到二进制: {}（请先 cargo xtask build）", bin.display());
    }

    println!("==> 组装模块目录");
    let stage = dist.join("stage");
    if stage.exists() {
        fs::remove_dir_all(&stage)?;
    }
    copy_tree(&src, &stage)?;

    // 放入二进制
    let bin_dir = stage.join("bin");
    fs::create_dir_all(&bin_dir)?;
    fs::copy(&bin, bin_dir.join(BIN_NAME))?;
    println!(
        "    bin/{BIN_NAME} ({:.1} KB)",
        fs::metadata(&bin)?.len() as f64 / 1024.0
    );

    // ---------- 反向断言：不得含 APK ----------
    println!("==> 反向断言");
    assert_no_apk(&stage)?;

    // ---------- 16 KB 对齐检查（发布硬门槛）----------
    if release {
        println!("==> 16 KB 对齐检查");
        check_alignment(&bin_dir.join(BIN_NAME))?;
    }

    // ---------- SHA-256 清单 ----------
    println!("==> 生成 checksum.sha256");
    let count = write_checksums(&stage)?;
    println!("    {count} 项");

    // ---------- 打 zip ----------
    //
    // 🔴 产物名必须带版本号（用户要求 2026-10-22）：
    //    Release「资源」里同时存在多个版本时，只叫 `HyperPower.zip` 无法区分。
    //    版本号取自 `module/src/module.prop`（唯一真源），不在这里另存一份。
    println!("==> 打包 zip");
    let ver = module_version(&src)?;
    let zip_path = dist.join(format!("HyperPower-v{ver}.zip"));
    if zip_path.exists() {
        fs::remove_file(&zip_path)?;
    }
    make_zip(&stage, &zip_path)?;

    let size = fs::metadata(&zip_path)?.len();
    println!();
    println!("✓ 打包完成");
    println!("  产物: {}", zip_path.display());
    println!("  大小: {:.1} KB", size as f64 / 1024.0);

    Ok(())
}

/// 从 `module/src/module.prop` 读取模块版本号
///
/// 打包产物名（`HyperPower-v<ver>.zip`）与安装脚本都依赖它。
/// 读不到或为空时**直接失败**，不用猜的版本号（R17 不猜）。
pub(crate) fn module_version(src: &Path) -> Result<String> {
    let prop = fs::read_to_string(src.join("module.prop")).context("读不到 module.prop")?;
    let ver = prop
        .lines()
        .find_map(|line| line.trim().strip_prefix("version="))
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .context("module.prop 缺少 version= 或值为空")?;
    Ok(ver.to_string())
}

fn repo_root() -> Result<PathBuf> {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").context("缺少 CARGO_MANIFEST_DIR")?;
    Ok(Path::new(&manifest)
        .parent()
        .context("无法定位仓库根目录")?
        .to_path_buf())
}

fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    for (rel, is_dir) in fs_walk::list_entries(src)? {
        // `.gitkeep` 仅用于让空目录（如 profiles/）进入 git，
        // 不应进入模块包 —— 设备侧只应看到真实数据文件
        if rel.file_name().and_then(|n| n.to_str()) == Some(".gitkeep") {
            continue;
        }
        let target = dst.join(&rel);
        if is_dir {
            fs::create_dir_all(&target)?;
        } else {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(src.join(&rel), &target)?;
        }
    }
    Ok(())
}

/// 反向断言：模块不得包含任何 APK 或 `app/` 目录（模块/APP 分离）
fn assert_no_apk(stage: &Path) -> Result<()> {
    let mut offenders: Vec<String> = Vec::new();

    for (rel, is_dir) in fs_walk::list_entries(stage)? {
        let name = rel
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();

        if !is_dir && name.ends_with(".apk") {
            offenders.push(format!("APK: {}", rel.display()));
        }
        if is_dir && name == "app" {
            offenders.push(format!("目录 app/: {}", rel.display()));
        }
    }

    if !offenders.is_empty() {
        for o in &offenders {
            eprintln!("    ✗ {o}");
        }
        anyhow::bail!("模块内出现 APK —— 违反模块/APP 分离约定，打包中止");
    }

    println!("    ✓ 未含 APK（符合模块/APP 分离约定）");
    Ok(())
}

/// 检查 ELF 的 LOAD 段是否按 16 KB 对齐
fn check_alignment(bin: &Path) -> Result<()> {
    let out = std::process::Command::new("llvm-readelf")
        .args(["-lW", &bin.to_string_lossy()])
        .output();

    let out = match out {
        Ok(o) if o.status.success() => o,
        _ => {
            eprintln!("    ! llvm-readelf 不可用，跳过 16 KB 对齐检查");
            eprintln!("      （CI 上会强制执行；本地可将 NDK 的 bin 加入 PATH）");
            return Ok(());
        }
    };

    let text = String::from_utf8_lossy(&out.stdout);
    let loads: Vec<&str> = text
        .lines()
        .filter(|l| l.trim_start().starts_with("LOAD"))
        .collect();

    if loads.is_empty() {
        anyhow::bail!("未能从二进制中读取 LOAD 段");
    }

    let mut bad = Vec::new();
    for l in &loads {
        let align = l.split_whitespace().last().unwrap_or("");
        if align != "0x4000" {
            bad.push(((*l).to_string(), align.to_string()));
        }
    }

    if !bad.is_empty() {
        for (l, a) in &bad {
            eprintln!("    ✗ 对齐 {a}: {l}");
        }
        anyhow::bail!("存在未按 16 KB 对齐的 LOAD 段 —— 禁止发布");
    }

    println!("    ✓ 全部 LOAD 段对齐 0x4000（16 KB）");
    Ok(())
}

/// 写入 SHA-256 清单，返回条目数
///
/// ⚠️ 清单中的路径必须是**相对模块根**的路径（设备端 `sha256sum -c` 在
/// 模块目录内执行），绝不能写入打包机的绝对路径。
fn write_checksums(stage: &Path) -> Result<usize> {
    let mut entries: Vec<(String, String)> = Vec::new();

    for rel in fs_walk::list_files(stage)? {
        let name = rel
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        if CHECKSUM_EXCLUDE.contains(&name.as_str()) {
            continue;
        }

        let data = fs::read(stage.join(&rel))?;
        let mut hasher = Sha256::new();
        hasher.update(&data);
        let hash = hex::encode(hasher.finalize());

        // 路径统一用 / 分隔（设备端为 Unix）
        let rel_unix = rel.to_string_lossy().replace('\\', "/");
        entries.push((hash, rel_unix));
    }

    entries.sort_by(|a, b| a.1.cmp(&b.1));

    let mut out = String::new();
    for (hash, path) in &entries {
        out.push_str(hash);
        out.push_str("  ");
        out.push_str(path);
        out.push('\n');
    }

    fs::write(stage.join("checksum.sha256"), out)?;
    Ok(entries.len())
}

/// 打包为 zip（内容位于 zip 根）
fn make_zip(stage: &Path, zip_path: &Path) -> Result<()> {
    let file = fs::File::create(zip_path)?;
    let mut zip = zip::ZipWriter::new(file);
    let options: zip::write::FileOptions<'_, ()> =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    for (rel, is_dir) in fs_walk::list_entries(stage)? {
        let name = rel.to_string_lossy().replace('\\', "/");

        if is_dir {
            zip.add_directory(format!("{name}/"), options)?;
        } else {
            zip.start_file(name, options)?;
            let data = fs::read(stage.join(&rel))?;
            zip.write_all(&data)?;
        }
    }

    zip.finish()?;
    Ok(())
}
