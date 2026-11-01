//! 插槽注册表
//!
//! **新增一个附加功能，只需要在这里加一段**（外加新建一个实现目录），
//! 核心温控的代码一行都不用改 —— 见 `docs/09-设计-附加功能架构.md` §9。

use crate::pps::{config as pps_cfg, Pps, PpsConfig};
use crate::runner::AddonSlot;
use crate::soc_limit::{config as soc_cfg, SocLimit, SocLimitConfig};

/// 从 `config.json` 解析出的通用 JSON 对象构造全部附加功能
///
/// # 返回
///
/// `(插槽列表, 问题清单)`
///
/// # 容错（设计 §6.2）
///
/// 每个功能**各自取自己的分区、独立解析**：
/// 某一节写坏 ⇒ 只让该功能停用并记入问题清单，**其他功能与温控都不受影响**。
pub fn build_all(config: &serde_json::Value) -> (Vec<AddonSlot>, Vec<String>) {
    let mut slots: Vec<AddonSlot> = Vec::new();
    let mut problems: Vec<String> = Vec::new();

    // ────────────────────────────────────────────────────────────
    //  ① 充电上限（soc_limit）
    // ────────────────────────────────────────────────────────────
    {
        let section = config.get(soc_cfg::SECTION);
        let cfg = match SocLimitConfig::from_section_opt(section) {
            Ok(c) => c,
            Err(e) => {
                problems.push(format!("{e} —— 充电上限已停用（其余功能不受影响）"));
                SocLimitConfig::default()
            }
        };
        if let Err(e) = cfg.validate() {
            problems.push(format!("充电上限配置非法（{e}）—— 已回退出厂默认"));
            slots.push(AddonSlot::new(Box::new(SocLimit::new(
                SocLimitConfig::default(),
            ))));
        } else {
            slots.push(AddonSlot::new(Box::new(SocLimit::new(cfg))));
        }
    }

    // ────────────────────────────────────────────────────────────
    //  ② PPS 解锁（pps）
    //
    //  ⚠️ 出厂默认**关闭**：它会改变充电协议协商，可能让小米原装头
    //     失去 MIPPS 私有路径（见 `pps::config::WARN_MIPPS`）。
    //     必须由用户主动开启。
    // ────────────────────────────────────────────────────────────
    {
        let section = config.get(pps_cfg::SECTION);
        let cfg = match PpsConfig::from_section_opt(section) {
            Ok(c) => c,
            Err(e) => {
                problems.push(format!("{e} —— PPS 解锁已停用（其余功能不受影响）"));
                PpsConfig::default()
            }
        };
        if let Err(e) = cfg.validate() {
            problems.push(format!("PPS 解锁配置非法（{e}）—— 已回退出厂默认"));
            slots.push(AddonSlot::new(Box::new(Pps::new(PpsConfig::default()))));
        } else {
            slots.push(AddonSlot::new(Box::new(Pps::new(cfg))));
        }
    }

    // ────────────────────────────────────────────────────────────
    //  ★ 新增附加功能：在此处追加一段即可
    //    1) 新建 `src/<功能名>/` 并实现 `Addon` trait
    //    2) 在这里 push 一个 `AddonSlot::new(Box::new(...))`
    //    3) 在 `module/src/defaults/config.json` 加一个默认分区
    // ────────────────────────────────────────────────────────────

    (slots, problems)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use serde_json::json;

    /// 当前注册的功能总数（新增功能时改这里，**故意**让测试提醒你）
    const EXPECTED_SLOTS: usize = 2;

    /// 按 id 找插槽（不要按数组下标 —— 顺序会随新增功能变化）
    fn slot<'a>(slots: &'a [AddonSlot], id: &str) -> &'a AddonSlot {
        slots
            .iter()
            .find(|s| s.addon().spec().id == id)
            .unwrap_or_else(|| panic!("找不到插槽 {id}"))
    }

    #[test]
    fn 缺失分区时使用默认且不报错() {
        let (slots, problems) = build_all(&json!({}));
        assert_eq!(slots.len(), EXPECTED_SLOTS);
        assert!(problems.is_empty(), "缺失分区不算错误");
        for s in &slots {
            assert!(
                !s.addon().enabled(),
                "{} 出厂默认必须是关闭",
                s.addon().spec().id
            );
        }
    }

    #[test]
    fn 分区写坏只影响自己() {
        let (slots, problems) = build_all(&json!({ "limit": { "soc": "八十" } }));
        assert_eq!(slots.len(), EXPECTED_SLOTS, "插槽仍在，只是停用");
        assert_eq!(problems.len(), 1, "必须记录问题，不能静默");
        assert!(
            !slot(&slots, "soc_limit").addon().enabled(),
            "坏配置一律停用"
        );
        assert!(
            !slot(&slots, "pps").addon().enabled(),
            "PPS 未配置，应保持关闭"
        );
    }

    /// 🔴 PPS 的配置写坏**不得**波及充电上限
    #[test]
    fn pps分区写坏不影响充电上限() {
        let (slots, problems) = build_all(&json!({
            "pps": { "enabled": "是" },
            "limit": { "enabled": true, "soc": 85 }
        }));
        assert_eq!(problems.len(), 1, "只该报 PPS 的问题");
        assert!(
            slot(&slots, "soc_limit").addon().enabled(),
            "充电上限必须照常工作"
        );
        assert!(!slot(&slots, "pps").addon().enabled(), "PPS 自己停用");
    }

    #[test]
    fn 正常分区被正确解析() {
        let (slots, problems) = build_all(&json!({
            "limit": { "enabled": true, "soc": 85, "verify_s": 20 }
        }));
        assert!(problems.is_empty());
        let st = slot(&slots, "soc_limit").status();
        assert!(st.enabled);
        assert_eq!(st.detail["soc_target"], 85);
        assert_eq!(st.detail["verify_s"], 20);
    }

    #[test]
    fn pps分区被正确解析() {
        let (slots, problems) = build_all(&json!({
            "pps": { "enabled": true, "mode": "auto", "power_display_w": 120 }
        }));
        assert!(problems.is_empty());
        let st = slot(&slots, "pps").status();
        assert!(st.enabled);
        assert_eq!(st.detail["mode"], "自动");
    }

    #[test]
    fn 越界配置被拒绝并回退() {
        let (slots, problems) = build_all(&json!({
            "limit": { "enabled": true, "soc": 200 }
        }));
        assert_eq!(problems.len(), 1, "越界必须记问题");
        assert!(
            !slot(&slots, "soc_limit").addon().enabled(),
            "回退后必须是关闭状态"
        );
    }

    #[test]
    fn pps越界配置被拒绝并回退() {
        let (slots, problems) = build_all(&json!({
            "pps": { "enabled": true, "power_display_w": 500 }
        }));
        assert_eq!(problems.len(), 1);
        assert!(!slot(&slots, "pps").addon().enabled());
    }
}
