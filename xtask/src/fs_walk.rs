//! 文件系统遍历（标准库实现，不依赖 walkdir）
//!
//! 为什么不用 `walkdir`：其 `winapi-util` 依赖链会触发本机 GNU 工具链的
//! `dlltool` 缺陷（rustup 自带精简版执行失败）。自行实现遍历既无依赖，
//! 也足够满足打包需求。

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

/// 遍历目录下的全部**文件**（递归），返回**相对于 `root` 的**路径
pub fn list_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let ft = entry.file_type()?;
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                // ⚠️ 必须返回相对路径：调用方可能直接把它写进
                //    校验清单（设备端需要相对模块根的路径）
                out.push(path.strip_prefix(root)?.to_path_buf());
            }
        }
    }

    out.sort();
    Ok(out)
}

/// 遍历目录下的全部**条目**（文件 + 目录，递归），返回相对路径
///
/// 用于打 zip 时保留目录结构。
pub fn list_entries(root: &Path) -> Result<Vec<(PathBuf, bool)>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let ft = entry.file_type()?;
            let rel = path.strip_prefix(root)?.to_path_buf();

            if ft.is_dir() {
                out.push((rel, true));
                stack.push(path);
            } else if ft.is_file() {
                out.push((rel, false));
            }
        }
    }

    out.sort();
    Ok(out)
}
