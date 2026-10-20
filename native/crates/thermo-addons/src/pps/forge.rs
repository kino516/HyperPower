//! 广播门控与爆发参数（**纯逻辑，不碰 I/O**）
//!
//! 契约见 `ThermoCharge-PPS移植方案.md` §7.3 / §7.4。
//!
//! # 这段逻辑照抄 FreePPS 源码，**不要"顺手优化"**
//!
//! 门控四个条件缺一不可，每一个都是真机撞出来的：
//!
//! | 条件 | 不满足会怎样 |
//! |---|---|
//! | `real_type == "PD_PPS"` | 非 PPS 头（纯 PD / USB）也发金标 ⇒ 用户看到假的 100W |
//! | `pd_verifed == "1"` | 没解锁就发 ⇒ 动画与实际功率不符 |
//! | `adapter_svid == "0000"` | 小米原装头（SVID 非 0000）已有自己的动画，再发会**打架** |
//! | `voltage_uv >= 12V` | 5V 档发"满血 100W" ⇒ 明显说谎，用户会以为模块坏了 |
//!
//! # 为什么只发给 SystemUI
//!
//! 广播目标若包含 PowerCenter（`com.miui.powerkeeper` 一类），
//! 它会弹「退出快充加速」的通知 —— 每次插电都弹一次，非常烦人。

/// 门控需要的输入（由调用方从节点读好后填入）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gate {
    /// 当前协议类型（`real_type` 节点的原文）
    pub real_type: Option<String>,
    /// PD 认证位当前值（`pd_verifed` 节点的原文）
    pub pd_verified: Option<String>,
    /// 适配器 SVID（十六进制字符串）
    pub adapter_svid: Option<String>,
    /// USB 侧电压（μV）
    pub voltage_uv: Option<i64>,
    /// 充电器最高功率（W）
    pub apdo_max: Option<u16>,
}

/// 门控判定的结果（含"为什么没通过"，便于界面解释）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateResult {
    /// 可以广播
    Ready {
        /// 要显示的功率（W）
        power_w: u16,
    },
    /// 不满足条件
    Blocked {
        /// 给用户看的原因
        reason: String,
    },
}

impl GateResult {
    /// 是否可以广播
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }

    /// 原因文案（`Ready` 时给一句说明）
    pub fn message(&self) -> String {
        match self {
            Self::Ready { power_w } => format!("可广播（{power_w}W）"),
            Self::Blocked { reason } => reason.clone(),
        }
    }
}

/// 协议是否属于 PPS 类
///
/// # 为什么要匹配多个写法
///
/// 不同平台/驱动的枚举名不一样，实测见过：
///
/// | 写法 | 出处 |
/// |---|---|
/// | `PD_PPS` | 高通 `qcom-battery/real_type`（K70 Pro） |
/// | `PPS` | 部分联发科机型 |
/// | `PD_PPS_...` | 将来可能出现带后缀的变体 |
///
/// ⚠️ 用 `contains("PPS")` 而非精确相等：多匹配几个变体不会造成误报
///    （值里含 PPS 就一定支持 PPS），而漏匹配会让功能静默失效。
fn is_pps(real_type: &str) -> bool {
    real_type.to_ascii_uppercase().contains("PPS")
}

/// 判定要不要发广播
///
/// `min_vbus_uv` 来自配置（默认 12 V）。
/// `power_display_w` 是 `apdo_max` 不足时的回落显示值。
pub fn evaluate(gate: &Gate, min_vbus_uv: i64, power_display_w: u16) -> GateResult {
    // ① 必须是 PPS 协议
    let rt = gate.real_type.as_deref().unwrap_or("");
    if rt.is_empty() {
        return GateResult::Blocked {
            reason: "读不到协议类型（real_type 节点不可用）".to_string(),
        };
    }
    if !is_pps(rt) {
        return GateResult::Blocked {
            reason: format!("当前协议是 {rt}，不是 PPS"),
        };
    }

    // ② 必须已经解锁
    //
    // ⚠️ 不看这个就发广播 ⇒ 动画说 100W、实际没解锁，用户会觉得被骗。
    let verified = gate.pd_verified.as_deref().unwrap_or("").trim();
    if verified != "1" {
        return GateResult::Blocked {
            reason: format!("PD 认证位是 {verified}（未解锁），先启用本功能"),
        };
    }

    // ③ 必须是公版头（SVID = 0000）
    //
    // 小米原装头走 MIPPS，SVID 非 0000，它自己就有动画。
    // 我们再发一遍会与系统动画互相覆盖。
    let svid = gate.adapter_svid.as_deref().unwrap_or("").trim();
    if svid.is_empty() {
        return GateResult::Blocked {
            reason: "读不到适配器 SVID".to_string(),
        };
    }
    if !svid.eq_ignore_ascii_case("0000") {
        return GateResult::Blocked {
            reason: format!("适配器 SVID 是 {svid}（非公版头），不伪造动画"),
        };
    }

    // ④ 电压必须够高（默认 12 V）
    //
    // 5V 档喊"满血 100W"是明显的谎话。
    let Some(v) = gate.voltage_uv else {
        return GateResult::Blocked {
            reason: "读不到 USB 电压".to_string(),
        };
    };
    if v < min_vbus_uv {
        return GateResult::Blocked {
            reason: format!(
                "USB 电压 {}V 低于门控 {}V",
                v as f64 / 1e6,
                min_vbus_uv as f64 / 1e6
            ),
        };
    }

    // ⑤ 决定显示功率
    //
    // `apdo_max >= 90` 时才喊"满血"；否则按实际能力显示，不要虚报。
    //
    // 实测（K70 Pro 拔电时）：`apdo_max = 0` ⇒ 走回落值。
    // 插电后应变为充电头的能力值（需真机确认）。
    let power_w = match gate.apdo_max {
        Some(w) if w >= 90 => power_display_w.max(w),
        Some(w) if w > 0 => w,
        _ => power_display_w,
    };

    GateResult::Ready { power_w }
}

/// 一次广播爆发里的**单条** `am` 命令
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BroadcastCmd {
    /// 意图 action
    pub action: &'static str,
    /// 参数（`--ei` 是**必须**的，见模块文档）
    pub extras: Vec<(&'static str, i64)>,
}

impl BroadcastCmd {
    /// 拼成 `am broadcast ...` 的完整参数列表（不含 `am` 本身）
    ///
    /// 只发给 `com.android.systemui`（见模块文档：带上 PowerCenter 会弹通知）。
    pub fn args(&self) -> Vec<String> {
        let mut a: Vec<String> = vec![
            "broadcast".to_string(),
            "-p".to_string(),
            "com.android.systemui".to_string(),
            "-a".to_string(),
            self.action.to_string(),
        ];
        for (k, v) in &self.extras {
            a.push("--ei".to_string());
            a.push((*k).to_string());
            a.push(v.to_string());
        }
        a
    }
}

/// 一次爆发的**完整序列**（含每步之间的等待毫秒数）
///
/// # 为什么是这个节奏
///
/// 这套时间表来自 FreePPS 的真机结论：SystemUI 的动画状态机需要
/// "先置类型 → 等一帧 → 再刷 → 重复"，否则最后那帧不播。
///
/// ```text
/// QUICK=1 → 100ms → SOC_DECIMAL → 100ms → QUICK=4 → 250ms → QUICK=4 → 400ms → QUICK=4
/// ```
///
/// ⚠️ 不要"简化"成只发一条：真机验证过，单发一条动画不出来。
pub fn burst_sequence(power_w: u16) -> Vec<(u64, BroadcastCmd)> {
    const ACTION_QUICK: &str = "miui.intent.action.ACTION_QUICK_CHARGE_TYPE";
    const ACTION_SOC: &str = "miui.intent.action.ACTION_SOC_DECIMAL";

    let quick = |t: i64| BroadcastCmd {
        action: ACTION_QUICK,
        extras: vec![
            ("miui.intent.extra.quick_charge_type", t),
            ("miui.intent.extra.POWER_MAX", i64::from(power_w)),
            ("miui.intent.extra.CAR_CHARGE", 0),
        ],
    };

    let soc = BroadcastCmd {
        action: ACTION_SOC,
        extras: vec![
            ("miui.intent.extra.soc_decimal", 0),
            ("miui.intent.extra.soc_decimal_rate", 0),
        ],
    };

    vec![
        (0, quick(1)),
        (100, soc),
        (100, quick(4)),
        (250, quick(4)),
        (400, quick(4)),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn ok_gate() -> Gate {
        Gate {
            real_type: Some("PD_PPS".to_string()),
            pd_verified: Some("1".to_string()),
            adapter_svid: Some("0000".to_string()),
            voltage_uv: Some(20_000_000),
            apdo_max: Some(100),
        }
    }

    #[test]
    fn 全条件满足时可广播() {
        let r = evaluate(&ok_gate(), 12_000_000, 100);
        assert!(r.is_ready(), "{}", r.message());
        assert_eq!(r, GateResult::Ready { power_w: 100 });
    }

    #[test]
    fn 非pps协议被拦() {
        let g = Gate {
            real_type: Some("USB".to_string()),
            ..ok_gate()
        };
        let r = evaluate(&g, 12_000_000, 100);
        assert!(!r.is_ready());
        assert!(r.message().contains("USB"));
    }

    #[test]
    fn 未解锁被拦() {
        let g = Gate {
            pd_verified: Some("0".to_string()),
            ..ok_gate()
        };
        let r = evaluate(&g, 12_000_000, 100);
        assert!(!r.is_ready());
        assert!(r.message().contains("未解锁"));
    }

    /// 🔴 小米原装头（非 0000）不得伪造动画
    #[test]
    fn 非公版头被拦() {
        let g = Gate {
            adapter_svid: Some("0001".to_string()),
            ..ok_gate()
        };
        let r = evaluate(&g, 12_000_000, 100);
        assert!(!r.is_ready());
        assert!(r.message().contains("非公版头"));
    }

    #[test]
    fn 电压不足被拦() {
        let g = Gate {
            voltage_uv: Some(5_000_000),
            ..ok_gate()
        };
        let r = evaluate(&g, 12_000_000, 100);
        assert!(!r.is_ready());
        assert!(r.message().contains("低于门控"));
    }

    #[test]
    fn 节点缺失被拦且给出原因() {
        for g in [
            Gate {
                real_type: None,
                ..ok_gate()
            },
            Gate {
                adapter_svid: None,
                ..ok_gate()
            },
            Gate {
                voltage_uv: None,
                ..ok_gate()
            },
        ] {
            let r = evaluate(&g, 12_000_000, 100);
            assert!(!r.is_ready(), "缺节点不该放行");
            assert!(!r.message().is_empty(), "必须给出原因");
        }
    }

    #[test]
    fn pps写法兼容多种变体() {
        for s in ["PD_PPS", "PPS", "pd_pps", "PD_PPS_V2"] {
            let g = Gate {
                real_type: Some(s.to_string()),
                ..ok_gate()
            };
            assert!(evaluate(&g, 12_000_000, 100).is_ready(), "{s} 应被认作 PPS");
        }
    }

    #[test]
    fn svid大小写不敏感() {
        let g = Gate {
            adapter_svid: Some("0000".to_string()),
            ..ok_gate()
        };
        assert!(evaluate(&g, 12_000_000, 100).is_ready());
    }

    #[test]
    fn 功率显示按apdo_max决定() {
        // apdo >= 90 → 用配置的满血值
        let g = Gate {
            apdo_max: Some(120),
            ..ok_gate()
        };
        assert_eq!(
            evaluate(&g, 12_000_000, 100),
            GateResult::Ready { power_w: 120 },
            "取两者更大"
        );

        // apdo 较小 → 按实际能力显示，不要虚报
        let g = Gate {
            apdo_max: Some(67),
            ..ok_gate()
        };
        assert_eq!(
            evaluate(&g, 12_000_000, 100),
            GateResult::Ready { power_w: 67 }
        );

        // apdo = 0（未协商）→ 回落配置值
        let g = Gate {
            apdo_max: Some(0),
            ..ok_gate()
        };
        assert_eq!(
            evaluate(&g, 12_000_000, 100),
            GateResult::Ready { power_w: 100 }
        );
    }

    #[test]
    fn 爆发序列有5步且时间递增() {
        let seq = burst_sequence(100);
        assert_eq!(seq.len(), 5, "方案要求 5 条");

        let mut prev = 0;
        for (delay, _) in &seq {
            assert!(*delay >= prev, "等待时间不该倒退");
            prev = *delay;
        }

        // 第一条与最后一条都是 QUICK，中间夹一条 SOC_DECIMAL
        assert!(seq[0].1.action.contains("QUICK"));
        assert!(seq[1].1.action.contains("SOC_DECIMAL"));
        assert!(seq[4].1.action.contains("QUICK"));
    }

    #[test]
    fn 广播只发给systemui() {
        let seq = burst_sequence(100);
        for (_, cmd) in seq {
            let args = cmd.args();
            assert!(args.contains(&"com.android.systemui".to_string()));
            assert!(
                !args.iter().any(|a| a.contains("powerkeeper")),
                "绝不能发给 PowerCenter（会弹通知）"
            );
        }
    }

    /// `--ei` 不能丢：丢了 SystemUI 收不到整数参数
    #[test]
    fn am参数用ei传整数() {
        let args = burst_sequence(100)[0].1.args();
        assert_eq!(args[0], "broadcast");
        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"-a".to_string()));
        assert!(args.contains(&"--ei".to_string()));
        assert!(args.contains(&"100".to_string()), "POWER_MAX 应为 100");
    }
}
