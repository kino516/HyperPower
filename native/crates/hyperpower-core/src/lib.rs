//! 纯决策内核
//!
//! # 设计约束（不可违反）
//!
//! - **不依赖**文件系统、进程、时钟、`sysfs`、日志、环境变量
//! - 纯函数 IPO：`Sample` + `Config` + `State` → `Decision`
//! - 安全边界（上限、钳位、迟滞）**全部在内核内闭合**
//!
//! I/O 层只负责搬运数据与落盘，不得参与决策。

#![forbid(unsafe_code)]

pub mod config;
pub mod decision;
pub mod error;
pub mod profile;
pub mod sample;
pub mod state;

pub use config::{Actuator, Config, ControlLaw, CurvePoint, Mode, SensorSource};
pub use decision::{Bounds, Decision, Reason};
pub use error::CoreError;
pub use profile::{BoundaryViolation, Capabilities};
pub use sample::{ChargingStatus, Sample};
pub use state::State;

/// 决策内核的入口（使用默认保守边界）。
///
/// 纯函数：相同输入必得相同输出，无副作用、无全局状态。
///
/// # 安全性质（由属性测试保证，见 `tests/`）
///
/// 1. 任意输入下输出电流不超过安全上限
/// 2. 任意输入下输出档位落在合法范围
/// 3. 达到硬上限温度必然进入硬保护
/// 4. 迟滞区不会产生每拍振荡
/// 5. 配置损坏时不会产生越界写值
pub fn decide(sample: &Sample, config: &Config, state: &State) -> Result<Decision, CoreError> {
    decision::evaluate(sample, config, state)
}

/// 决策内核的入口（指定设备能力边界）。
///
/// **这是生产路径**：所有钳位按检测得到的档案执行，
/// 而不是按写死的常量 —— 落实「控制全部基于测出的数据」。
pub fn decide_with(
    sample: &Sample,
    config: &Config,
    state: &State,
    caps: &Capabilities,
) -> Result<Decision, CoreError> {
    decision::evaluate_with(sample, config, state, &Bounds::from_caps(caps))
}
