//! IPC 契约层
//!
//! 定义后端（`thermo-cli`）与客户端（WebUI / 独立 APP）之间的**全部协议要素**：
//! 请求、响应、错误码、退出码、版本标识、设备档案结构。
//!
//! 契约详见 `docs/05-契约-接口输出规范.md` 与 `docs/07-契约-检测与设备档案.md`。
//! **只增不改**。

#![forbid(unsafe_code)]

pub mod device;
pub mod error;
pub mod protocol;
pub mod response;

pub use device::{
    BatteryInfo, Capabilities, DeviceProfile, EffectiveProfile, GearCapability, GearRow, Identity,
    NodePaths, PowerCapability, ProbeMeta, ProfileSource, PROFILE_KIND, PROFILE_SCHEMA,
};
pub use error::{ErrorCode, ERROR_CODES};
pub use protocol::{Request, Response, MAX_LINE_BYTES, PROTOCOL_VERSION};
pub use response::{ApiVersion, StatusSnapshot};
