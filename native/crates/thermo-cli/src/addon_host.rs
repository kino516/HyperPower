//! 附加功能的宿主接线
//!
//! 把 `thermo-addons`（纯框架，不认识具体机型与文件系统）接到真实的
//! sysfs 与配置文件上。
//!
//! # 依赖方向（关键）
//!
//! ```text
//! thermo-cli  ──依赖──▶  thermo-addons  ──依赖──▶  thermo-ipc
//!     │                                                  ▲
//!     └──────────────依赖───────────────────────────────┘
//!     │
//!     └──依赖──▶  thermo-core（温控内核）
//!
//! thermo-addons **不依赖** thermo-core ⇒ 附加功能不认识温控概念（编译期隔离）
//! ```

use thermo_addons::io::AddonIo;
use thermo_addons::registry;
use thermo_addons::runner::AddonRunner;

use crate::config_store::ConfigStore;

/// 基于真实文件系统的 [`AddonIo`] 实现
///
/// ⚠️ 这是附加功能**唯一**的 I/O 出口 —— 它们自己不碰文件系统，
/// 因此全部逻辑都能用假节点树离线测试。
///
/// 路径统一经 [`crate::sysfs::rooted`] 处理：真机上根前缀是 `/`（无影响），
/// 测试时指向假节点树 —— 附加功能与温控因此走**同一套**路径解析规则。
#[derive(Debug, Clone, Copy, Default)]
pub struct SysfsIo;

impl AddonIo for SysfsIo {
    fn exists(&mut self, path: &str) -> bool {
        crate::sysfs::rooted(path).exists()
    }

    fn writable(&mut self, path: &str) -> bool {
        // 用"试开写句柄"判断：sysfs 的权限位不可靠
        // （属主可能是 system:system，实际以 root 运行时可写）
        std::fs::OpenOptions::new()
            .write(true)
            .open(crate::sysfs::rooted(path))
            .is_ok()
    }

    fn read(&mut self, path: &str) -> Result<String, String> {
        std::fs::read_to_string(crate::sysfs::rooted(path))
            .map(|s| s.trim().to_string())
            .map_err(|e| format!("{e}"))
    }

    /// 是否插着充电器
    ///
    /// 附加功能据此在未充电时跳过巡检（省电的关键，详见 [`AddonIo::is_plugged`]）。
    ///
    /// # 判据
    ///
    /// 读 `battery/status`，接受 `Charging` 与 `Full` 两个值：
    ///
    /// | 值 | 含义 | 是否算"插着" |
    /// |---|---|---|
    /// | `Charging` | 正在充 | ✅ |
    /// | `Full` | 已充满但**仍插着** | ✅ |
    /// | `Discharging` | 在用电池 | ❌ |
    /// | `Not charging` | 插着但因故没充（如已达 SOC 上限） | ✅ **算插着** |
    ///
    /// ⚠️ `Not charging` 必须算"插着"：那正是**充电上限生效后的正常状态**
    /// （充到 95% 就停），此时恰恰最需要继续巡检 —— 若判为未插电，
    /// 就会停止校验，用户改上限后无法生效。
    ///
    /// # 读不到时
    ///
    /// 返回 `true`（保守）：宁可多巡检，也不要因为节点读不到而
    /// 让功能静默失效。
    fn is_plugged(&mut self) -> bool {
        const STATUS: &str = "/sys/class/power_supply/battery/status";
        match std::fs::read_to_string(crate::sysfs::rooted(STATUS)) {
            Ok(s) => {
                let s = s.trim();
                matches!(s, "Charging" | "Full" | "Not charging")
            }
            Err(_) => true,
        }
    }

    fn write(&mut self, path: &str, value: &str) -> Result<(), String> {
        use std::io::{Seek, SeekFrom, Write};
        let real = crate::sysfs::rooted(path);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(&real)
            .map_err(|e| format!("打开失败：{e}"))?;
        // 与温控侧同规则：先截断（sysfs 不支持则忽略），再整值一次写入
        f.set_len(0).ok();
        f.seek(SeekFrom::Start(0)).ok();
        f.write_all(value.as_bytes())
            .map_err(|e| format!("写入失败：{e}"))?;
        f.flush().ok();
        Ok(())
    }
}

/// 结果：运行器 + 加载期发现的问题
#[derive(Debug)]
pub struct AddonSetup {
    /// 调度器（含全部插槽）
    pub runner: AddonRunner,
    /// 配置解析期的问题（**必须记事件，不可静默**）
    pub problems: Vec<String>,
}

/// 从配置构建附加功能调度器
///
/// 每个功能**各自解析自己的分区**：某一节写坏只让它停用，
/// 温控与其余功能不受影响（见 `docs/09-设计-附加功能架构.md` §6.2）。
pub fn setup(cfg: &ConfigStore) -> AddonSetup {
    let (slots, problems) = registry::build_all(cfg.raw());
    AddonSetup {
        runner: AddonRunner::new(slots),
        problems,
    }
}

/// 把附加功能的配置变更推回各功能实例
///
/// 调用时机：`set limit.soc 85` 之类的配置写入之后。
///
/// # 两个动作
///
/// | 动作 | 为什么 |
/// |---|---|
/// | 覆盖功能实例的配置 | 否则拍到了也还是用旧配置 |
/// | **立即重新调度**（`reschedule_now`） | 否则用户点了「启用」要等满一个旧周期才生效 —— 体验上就是"按了没反应" |
///
/// ⚠️ 本函数**不会立即写节点** —— 下一拍才生效。这样一次命令只触发一次写入，
/// 也避免在命令处理路径上做 I/O。
pub fn reload_config(runner: &mut AddonRunner, cfg: &ConfigStore) {
    // 当前只有充电上限一个功能；新增功能时在此追加分支。
    if let Some(slot) = runner.slot_mut("soc_limit") {
        if let Some(soc) = slot
            .addon_mut()
            .as_any_mut()
            .downcast_mut::<thermo_addons::soc_limit::SocLimit>()
        {
            // ⚠️ 先注入**本机支持的范围**（来自检测档案），再校验配置
            //
            // 顺序不能反：`validate_within` 用这个范围判定合法性。
            // 范围因机型而异（初版写死 60~100），若用错范围会误拒合法值。
            if let Some(r) = detected_soc_range() {
                soc.set_range(r.0, r.1);
            }

            let section = cfg.section(thermo_addons::soc_limit::config::SECTION);
            if let Ok(new_cfg) = thermo_addons::soc_limit::SocLimitConfig::from_section_opt(section)
            {
                let (rmin, rmax) = detected_soc_range().unwrap_or((
                    thermo_addons::soc_limit::config::SOC_FALLBACK_MIN,
                    thermo_addons::soc_limit::config::SOC_FALLBACK_MAX,
                ));
                if new_cfg.validate_within(rmin, rmax).is_ok() {
                    soc.set_config(new_cfg);
                    // 配置已变 → 让它在下一拍就跑，而不是等旧周期
                    slot.reschedule_now();
                }
            }
        }
    }
}

/// 从**生效档案**读本机支持的电量上限区间（%）
///
/// 返回 `None` 表示档案里没有该信息（检测未跑过 / 机型不支持），
/// 调用方应退回兜底区间。
fn detected_soc_range() -> Option<(u8, u8)> {
    let eff = crate::profile_io::effective_profile();
    let s = eff.profile.capabilities.soc_limit?;
    if !s.supported {
        return None;
    }
    Some((s.min, s.max))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn 出厂默认下充电上限为关闭() {
        let cfg = ConfigStore::with_defaults("x.json");
        let setup = setup(&cfg);
        assert!(setup.problems.is_empty(), "出厂默认不应有问题");
        let slot = setup.runner.slot("soc_limit").unwrap();
        assert!(!slot.addon().enabled(), "出厂默认必须关闭");
    }

    #[test]
    fn 配置分区写坏只影响该功能() {
        // 直接用一份写坏的 JSON（模拟外部工具把分区写成错误类型），
        // 无需经过配置文件即可验证容错路径。
        let bad = serde_json::json!({"limit": {"soc": "不是数字"}});
        let (slots, problems) = registry::build_all(&bad);
        assert_eq!(slots.len(), 1, "插槽仍在（只是停用）");
        assert!(!problems.is_empty(), "必须记录问题");
    }

    #[test]
    fn 系统io识别不存在的节点() {
        let mut io = SysfsIo;
        assert!(!io.exists("/definitely/not/exist/xyz"));
        assert!(io.read("/definitely/not/exist/xyz").is_err());
    }
}
