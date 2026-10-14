//! UDS 协议：JSON Lines
//!
//! - 端点：`/data/adb/thermo_charge/ipc/control.sock`（`0600 root:root`）
//! - 认证：`SO_PEERCRED` UID 必须为 0
//! - 编码：一请求一行、一响应一行

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 协议版本
///
/// ⚠️ 语义变更时提升；客户端不匹配时应提示升级而非静默失败。
pub const PROTOCOL_VERSION: u32 = 1;

/// 单行长度上限（64 KB）
pub const MAX_LINE_BYTES: usize = 64 * 1024;

/// 客户端请求
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// 协议版本
    pub protocol_version: u32,
    /// 命令名（如 `status` / `set` / `watch`）
    pub command: String,
    /// 命令参数
    #[serde(default)]
    pub args: Value,
    /// 是否请求只读流（`watch`）
    #[serde(default, skip_serializing_if = "is_false")]
    pub stream: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// 服务端响应
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Response {
    /// 成功
    Ok {
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
    },
    /// 失败
    Err {
        ok: bool,
        code: String,
        msg: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        field: Option<String>,
    },
}

impl Response {
    /// 构造成功响应
    pub fn success() -> Self {
        Self::Ok {
            ok: true,
            data: None,
        }
    }

    /// 构造带数据的成功响应
    pub fn success_with(data: Value) -> Self {
        Self::Ok {
            ok: true,
            data: Some(data),
        }
    }

    /// 构造失败响应
    pub fn failure(code: &str, msg: impl Into<String>) -> Self {
        Self::Err {
            ok: false,
            code: code.to_string(),
            msg: msg.into(),
            field: None,
        }
    }

    /// 序列化为单行 JSON（追加换行）
    pub fn to_line(&self) -> Result<String, serde_json::Error> {
        let mut s = serde_json::to_string(self)?;
        s.push('\n');
        Ok(s)
    }
}
