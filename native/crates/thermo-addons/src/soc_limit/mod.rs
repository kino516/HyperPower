//! 附加功能 ①：充电上限（`soc_limit`）
//!
//! 设定"充到指定电量即停止"。
//!
//! # 机制是怎么来的
//!
//! 全部由**真机实测确证**（见 [`logic`] 的模块文档与那几张对照表）：
//! 写入节点是 `smart_chg`、读取节点是 `soc_limit`、控制字的使能位是 `0x11`。
//! 这些都是在这台设备上一条一条试出来的，不是从别处抄的。
//!
//! # 它碰什么、不碰什么
//!
//! | 维度 | 值 |
//! |---|---|
//! | 写节点 | `smart_chg`（控制字） |
//! | 读节点 | 同目录 `soc_limit`（生效值） |
//! | **不碰** | 档位 / 电流 / 温度阈值 / 任何温控节点 |
//! | 周期 | 启用时 15 秒；未启用时 60 秒（只为响应配置变更） |
//! | 失败后果 | 最多"没限制住"，不会伤害设备 |

pub mod config;
pub mod logic;

use serde_json::json;

use crate::addon::{Addon, AddonError, AddonSpec, AddonStatus, ProbeOutcome, TickOutcome};
use crate::io::AddonIo;
pub use config::SocLimitConfig;
use config::IDLE_INTERVAL_S;

/// `smart_chg` 的候选路径（**顺序即优先级**）
///
/// ⚠️ 路径**因机型而异**，必须运行期探测 —— 硬编码单一路径会让部分机型的
/// 充电上限功能直接失效。这与温控侧的候选表**刻意分开维护**（互不依赖）。
pub const SMART_CHG_CANDIDATES: &[&str] = &[
    // 小米 HyperOS 主路径（K90 Pro Max / Android 17 命中）
    "/sys/class/xm_power/charger/smart_charge/smart_chg",
    // 面向其他机型的兼容回退
    "/sys/class/charger/smart_charge/smart_chg",
    // ⚠️ **刻意不收录** `/sys/class/qcom-battery/smart_chg`
    //
    // 实测 2026-10-19：它「可写」（写入后节点值变成 16），但
    // **写进去不生效** —— `battery/status` 仍是 Charging，而且它旁边
    // 没有 `soc_limit` 可回读，`logic.rs` 的注释也写着"读值在 16/17/80
    // 漂移，不能用它判断生效"。
    //
    // ⇒ 一个"能写、改不了行为、也无法验证"的节点，收录它只会让
    //   `probe` 谎报"支持"，然后每拍失败、最终熔断。不如不收录：
    //   本机真诚地报「不支持」，用户就知道该去找别的方案。
    // 标准 Linux `power_supply` 接口：**纯百分比**（`0` 表示不限制）
    //
    // ⚠️ 语义与小米的 `smart_chg` 不同：那个是**控制字编码**
    //    （位域打包，见 `logic::encode`），这个是直白的 `0~100`。
    //    写值格式由 [`Dialect`] 决定，不要混用。
    "/sys/class/power_supply/battery/charge_control_end_threshold",
];

/// **使能节点**候选（标准百分比方言专用）
///
/// # 为什么必须有它（2026-10-19 真机查明，K70 Pro）
///
/// `charge_control_end_threshold` 在高通这套驱动上**不是自足的**：
///
/// | `charge_control_en` | 写 `threshold=95` | `write()` 返回值 |
/// |---|---|---|
/// | `0` | 读回 **0**（被吞掉） | **成功**（谎报） |
/// | `1` | 读回 **95** ✓ | 成功 |
///
/// 关键在最后一行：**写入失败时系统调用仍返回成功**。于是模块的回读
/// 校验看到"期望 95、实得 0"，连续三次后判定成"有另一个程序在抢这个
/// 节点"并熔断 —— 而真相只是少开了一个使能位。
///
/// ⚠️ 小米的 `smart_chg` 控制字自带 `0x11` 使能位，**不需要**这个节点
///    （所以候选命中后是否使用，由 [`Dialect`] 决定）。
pub const SOC_ENABLE_CANDIDATES: &[&str] = &[
    "/sys/class/qcom-battery/charge_control_en",
    "/sys/class/power_supply/battery/charge_control_en",
];

/// 控制节点的**写入方言**
///
/// # 为什么需要区分
///
/// 同样是"限制充到百分之几"，两种框架要写的东西完全不同：
///
/// | 框架 | 节点 | 要写入的内容 |
/// |---|---|---|
/// | 小米 `smart_chg` | `xm_power/charger/smart_charge/smart_chg` | **控制字** `0x50` 这类位域打包值 |
/// | 标准 `power_supply` | `battery/charge_control_end_threshold` | **纯百分比** `80` |
///
/// 把 `0x50` 写进标准节点 ⇒ 它会当成 `80`（恰好巧合），
/// 而把 `80` 写进小米节点 ⇒ 位域全错。所以必须按命中节点区分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// 小米控制字（`logic::encode` 打包）
    ControlWord,
    /// 标准百分比（纯十进制）
    Percent,
}

impl Dialect {
    /// 由控制节点路径判定方言
    ///
    /// ⚠️ 用**节点名**判定而不是"目录里有 `xm_power` 就…"：
    ///    将来若某机型把标准节点放到别的目录，这里依然正确。
    fn of(ctrl: &str) -> Self {
        if ctrl.ends_with("/charge_control_end_threshold") {
            Self::Percent
        } else {
            Self::ControlWord
        }
    }
}

/// **使用前提**文案（界面与 `limit show` 共用同一份）
///
/// 用户明确要求（2026-10-16）：「加上提示：此模块需打开电池健康保护功能」。
///
/// ## 为什么这不是"多余的提醒"
///
/// `smart_charge/*` 这一组节点属于系统「电池保护」功能的实现 ——
/// 系统设置里的「智能断充」（默认 80%）改的就是同一个节点。
/// 开关关掉时驱动是否仍执行 SOC 限制**不可依赖**，因此这是**真实前提**，
/// 不是免责声明。
///
/// ## 措辞为什么把两个名字都写上
///
/// 同一个开关在不同 HyperOS 版本 / 机型上叫法不同：
///
/// | 叫法 | 出现位置 |
/// |---|---|
/// | **电池健康保护** | 部分新版设置项名称 |
/// | **电池保护** | 设置路径与系统组件名（`ChargeProtectFragment`） |
///
/// 只写一个名字，另一个版本的用户会在设置里**找不到**，然后以为
/// 提示写错了。写上两个 + 路径，任何版本都能对上。
pub const REQUIRES_BATTERY_PROTECT: &str =
    "需先在系统设置中打开「电池健康保护」（设置 → 省电与电池 → 电池保护）";

/// **标准 `power_supply` 框架**的使用前提文案
///
/// # 为什么不能沿用上面那句（2026-10-19）
///
/// 上面那句的前提是「我们和系统**写同一个节点**」—— 系统的「电池保护」
/// 开关关掉后，那个节点的行为就不可依赖。这个推理对小米的
/// `smart_charge/smart_chg` 成立。
///
/// 但标准框架的 `charge_control_end_threshold` 是**上游内核的通用接口**，
/// 归 `power_supply` 子系统管，**不受**「电池健康保护」开关控制。
/// 对这类机型继续喊"请先打开电池健康保护"，后果是：
///
/// | 用户照做 | 结果 |
/// |---|---|
/// | 去设置里找那个开关 | 找到也可能本来就没关 |
/// | 找不到（机型没这个开关） | 以为缺了前提，**白白放弃该功能** |
///
/// ⇒ 改成如实说明"本机走标准接口"，并保留**唯一真实存在的注意点**：
///   未插充电器时看不到效果（驱动只记录不执行，小米与标准框架都如此）。
///
/// ⚠️ 不写"无需任何前提"：那等于替用户断言他没遇到过的问题不存在。
///    只说能确证的事。
pub const REQUIRES_STANDARD_PSY: &str =
    "本机走标准充电接口，与系统「电池保护」开关无关，无需额外设置\
     （未插充电器时看不到效果是正常的）";

/// **系统界面不同步**的说明（界面用）
///
/// # 为什么需要说明这件事
///
/// 系统的「电池保护」设置页有一套**自己的界面文案**，写的是它自己记的
/// 那个百分比（默认 80%）。那个文案的显示逻辑在**系统应用里**，
/// 而本模块是**纯 root + sysfs，没有 hook 层**，改不了系统界面。
///
/// ⇒ 用户设了 90%，进系统设置一看仍写着 80%。
///
/// 不说清楚的话，用户会判断成"没生效" —— 而实际上限制**已经生效**
/// （写的是同一个节点，驱动按他设的值执行）。
/// 这是"看起来失败其实成功了"的典型，必须在界面上说透。
///
/// ⚠️ 文案里**不要提任何具体做法**（比如"改界面文案"），
///    那既不是我们能做的事，也容易让用户误以为可以做到。
pub const NOTE_UI_NOT_SYNCED: &str =
    "系统设置里显示的百分比不会跟着变（那是系统自己的文案）。以本页的「已生效」为准。";

/// 由控制节点推出"生效值"节点（同目录的 `soc_limit`）
fn read_node_for(ctrl: &str, dialect: Dialect) -> String {
    // 标准接口：控制节点**自己**就是生效值（读它就能回读校验）
    if dialect == Dialect::Percent {
        return ctrl.to_string();
    }
    // 小米控制字：控制节点是打包值，生效值在同目录的 `soc_limit`
    match ctrl.rfind('/') {
        Some(idx) => format!("{}/soc_limit", &ctrl[..idx]),
        None => "soc_limit".to_string(),
    }
}

/// 充电上限功能
#[derive(Debug, Clone)]
pub struct SocLimit {
    /// 配置（来自 `config.json` 的 `limit` 分区）
    config: SocLimitConfig,
    /// 命中的控制节点路径（探测后填充）
    node_ctrl: Option<String>,
    /// 命中节点的写入方言（probe 时确定）
    dialect: Dialect,
    /// 对应的生效值节点路径
    node_read: Option<String>,
    /// **使能节点**路径（可选，`None` = 本机不需要）
    ///
    /// # 为什么需要它（2026-10-19 真机查明）
    ///
    /// K70 Pro 的 `charge_control_end_threshold` **不是自足的**：
    ///
    /// | `charge_control_en` | 写 `threshold=95` 后读回 |
    /// |---|---|
    /// | `0` | **0** ← 写入被驱动吞掉，且**不报错** |
    /// | `1` | **95** ✓ 正常保存 |
    ///
    /// 也就是说：不开使能位时，写入看起来成功、读回永远是 0 ——
    /// 模块据此判定"回读不符"，连续三次后误判成"有别的程序在抢"
    /// 并熔断，界面上就成了"充电上限不可用"。
    ///
    /// ⚠️ 只对**标准百分比**方言有效。小米的 `smart_chg` 控制字
    ///    自带 `0x11` 使能位，不需要额外节点。
    node_enable: Option<String>,
    /// 是否已经开过使能位（避免每拍重复写）
    enable_engaged: bool,
    /// 探测结果
    support: Option<ProbeOutcome>,
    /// 最近一次读到的生效上限
    last_seen: Option<u8>,
    /// 最近一次写入的上限
    last_written: Option<u8>,
    /// 累计写入次数
    write_count: u64,
    /// **本机支持的上限区间**（%），由宿主从检测档案注入
    ///
    /// `None` = 尚未注入（检测未跑过），此时用兜底区间
    /// `SOC_FALLBACK_MIN ~ SOC_FALLBACK_MAX`。
    ///
    /// ⚠️ 这个区间**因机型而异**：初版把它写死成 `60 ~ 100`
    /// （本机 K90PM 的值），换机型就会用错范围。
    range: Option<(u8, u8)>,
    /// **连续**"写下去又被改回"的次数 —— 用于识别**写入方冲突**
    ///
    /// # 为什么这个数能识别冲突
    ///
    /// `smart_chg` 是**全局节点**：任何写它的程序都在改同一个开关。
    /// 两个程序同时启用时，谁都可能在后 —— 于是出现
    /// 「我们写 85 → 立刻被改回 80 → 下拍再写 85 → 又被改回」的循环。
    ///
    /// 单次回读不符**不能**说明有冲突（可能只是驱动延迟一拍才生效），
    /// 所以只在**连续**多次不符时才提示 —— 见 [`Self::conflict_hint`]。
    ///
    /// 写入成功即清零。
    verify_fail_streak: u32,
}

impl SocLimit {
    /// 用给定配置构造（尚未探测）
    pub fn new(config: SocLimitConfig) -> Self {
        Self {
            config,
            node_ctrl: None,
            dialect: Dialect::ControlWord,
            node_read: None,
            node_enable: None,
            enable_engaged: false,
            support: None,
            last_seen: None,
            last_written: None,
            write_count: 0,
            range: None,
            verify_fail_streak: 0,
        }
    }

    /// 注入**本机支持的上限区间**（%）
    ///
    /// 由宿主在加载档案后调用（见 `addon_host::reload_config`）。
    /// 不注入时用兜底区间 —— 那种情况只在检测尚未跑过时出现。
    pub fn set_range(&mut self, min: u8, max: u8) {
        self.range = Some((min, max));
    }

    /// 当前生效的区间（本机 or 兜底）
    fn effective_range(&self) -> (u8, u8) {
        self.range
            .unwrap_or((config::SOC_FALLBACK_MIN, config::SOC_FALLBACK_MAX))
    }

    /// **写入方冲突**的提示后缀（不含则返回空串）
    ///
    /// # 判据
    ///
    /// `smart_chg` 是**全局节点** —— 任何写它的程序都在改同一个开关。
    /// 两个程序同时启用时会出现「我们写 85 → 立刻被改回 80 → 下拍再写 85
    /// → 又被改回」的循环，表现为**连续多次**回读不符。
    ///
    /// ⚠️ 门槛设在 **2 次**而不是 1 次：
    ///    单次回读不符未必是冲突 —— 驱动可能延迟一拍才把控制字应用到
    ///    `soc_limit` 上。凭一次不符就断言"有别的程序在抢"会误报，
    ///    反而让用户去卸载一个根本没冲突的模块。
    ///
    /// ⚠️ 这个提示**不指名任何具体模块**：会写这个节点的程序不止一个，
    ///    写死名字将来必然过时，而且用户可能装的是我们不知道的东西。
    fn conflict_hint(&self) -> &'static str {
        if self.config.warn_on_conflict && self.verify_fail_streak >= 2 {
            "（连续多次写下去就被改回 —— 可能有另一个程序也在写这个节点，请二选一）"
        } else {
            ""
        }
    }

    /// 当前配置
    pub fn config(&self) -> &SocLimitConfig {
        &self.config
    }

    /// 覆盖配置（**不会**自动写节点 —— 下一拍 `tick` 才会生效）
    pub fn set_config(&mut self, config: SocLimitConfig) {
        self.config = config;
    }

    /// 命中的控制节点路径
    pub fn node_ctrl(&self) -> Option<&str> {
        self.node_ctrl.as_deref()
    }

    /// 命中的读取节点路径
    pub fn node_read(&self) -> Option<&str> {
        self.node_read.as_deref()
    }

    /// 最近一次读到的生效上限
    pub fn last_seen(&self) -> Option<u8> {
        self.last_seen
    }

    /// 累计写入次数
    pub fn write_count(&self) -> u64 {
        self.write_count
    }

    /// 只读地把当前生效值刷新一次（供 `limit show` 之类的查询命令用）
    ///
    /// **只读，不写节点。**
    pub fn refresh_active(&mut self, io: &mut dyn AddonIo) {
        if let Some(read) = self.node_read.clone() {
            self.last_seen = io
                .read(&read)
                .ok()
                .and_then(|raw| logic::parse_active(&raw));
        }
    }
}

impl Addon for SocLimit {
    fn spec(&self) -> AddonSpec {
        AddonSpec {
            id: "soc_limit",
            label: "充电上限",
            summary: "充到指定电量即停止，保护电池寿命",
            // ⚠️ 来源说明**不指名任何第三方模块**（用户要求，2026-10-16）
            //
            // 这里只描述**机制本身是怎么确证的** —— 那才是这条信息
            // 对使用者的价值（"这不是猜的"）。
            // 而"参考过哪个程序"既不影响使用，也会让成品看起来像是别人的附庸，
            // 因此不写进任何面向用户的字段。
            //
            // ⚠️ 按方言说（2026-10-19）：标准 `power_supply` 的机型
            //    既没有 `smart_chg` 也没有 `soc_limit`，照抄小米那套
            //    说法会让用户去核对两个他机器上根本不存在的节点。
            origin: match self.dialect {
                Dialect::ControlWord => {
                    "节点机制由真机实测确证：写 smart_chg 控制字、读 soc_limit 生效值"
                }
                Dialect::Percent => {
                    "节点机制由真机实测确证：写 charge_control_end_threshold 直读百分比"
                }
            },
            default_interval_s: 15,
            // ⚠️ **使用前提**（用户明确要求加上，2026-10-16）
            //
            // # 为什么这个前提是真的
            //
            // 本功能写的是 `smart_charge/smart_chg` —— 它属于系统
            // 「电池保护」（HyperOS 里也叫「电池健康保护」）功能的实现：
            // 系统设置页里那个 80% 默认值，改的就是这个节点，
            // 而我们改的是同一个节点上的自定义值。
            //
            // 也就是说：**我们和系统在写同一个开关**。系统那侧的功能开关
            // 关掉时，驱动是否仍执行 SOC 限制**不可依赖** —— 它本来就是
            // 该功能的一部分。
            //
            // ⇒ **系统开关关掉时，这个节点的行为不可靠**（驱动可能不执行
            //    SOC 限制）。所以必须让用户先打开它。
            //
            // ⚠️ 未插充电器时驱动**只记录不执行**（dmesg:
            //    `adapter is plugout, don't handle soc limit`），
            //    因此"没插电时看不到效果"是正常的，与开关无关。
            //
            // ⚠️ **按方言选文案**（2026-10-19）：标准 `power_supply` 的
            //    `charge_control_end_threshold` 不归「电池保护」开关管，
            //    对它喊"请先打开电池健康保护"是错的。见
            //    [`REQUIRES_STANDARD_PSY`] 的说明。
            requires: Some(match self.dialect {
                Dialect::ControlWord => REQUIRES_BATTERY_PROTECT,
                Dialect::Percent => REQUIRES_STANDARD_PSY,
            }),
        }
    }

    fn probe(&mut self, io: &mut dyn AddonIo) -> ProbeOutcome {
        for cand in SMART_CHG_CANDIDATES {
            if io.writable(cand) {
                // ⚠️ 先定方言，再据此决定"生效值"节点是哪一个
                let d = Dialect::of(cand);
                let read_path = read_node_for(cand, d);

                // ── ⚠️ 探测阶段**不做试写**（2026-10-19 撤销）──
                //
                // 曾经在这里"试写一次自己的原值"，想验证"权限看着可写、
                // 驱动是否真接受"。撤销的原因有两条，都是硬的：
                //
                // ① **写回原值不是零副作用**。
                //    `smart_chg` 是**控制字**节点，而它的**读值**在
                //    16 / 17 / 80 之间漂移，与写入值没有稳定对应关系
                //    （真机实测确证，见 `logic` 的注释）。把读到的
                //    `80` 写回去，驱动按十六进制解析成 `0x80` ——
                //    低 16 位没有 `0x11` 使能位 ⇒ **等于把用户当前的
                //    SOC 限制关掉**。这不是"内容不变"。
                // ② **它破坏了「未启用 = 零写入」**。
                //    `probe` 在模块启动时无条件执行，用户哪怕没启用
                //    充电上限，也会被写一次节点。这条纪律是用户明确
                //    要求的（决策 B），不能因为"想早点知道能不能写"
                //    而破例。
                //
                // ⇒ 可写性验证**推迟到真要写的那一刻**（`tick`）：
                //   那时写的就是目标值本身，写不进去会如实报错
                //   （含 SELinux 的 `Permission denied`）、计入连续失败、
                //   最终熔断。用户看到的是真实原因，而不是一个
                //   探测阶段的"支持/不支持"。
                self.dialect = d;
                self.node_ctrl = Some((*cand).to_string());
                self.node_read = Some(read_path);

                // ── 探测**使能节点**（仅标准百分比方言需要）──
                //
                // ⚠️ 只探测、**不写入**（保持 probe 零写入的纪律）。
                //    真正的 `1` 在 `tick` 要写目标值时才写。
                self.node_enable = if d == Dialect::Percent {
                    SOC_ENABLE_CANDIDATES
                        .iter()
                        .find(|p| io.writable(p))
                        .map(|p| (*p).to_string())
                } else {
                    None
                };
                self.enable_engaged = false;

                let outcome = ProbeOutcome::Supported {
                    detail: match &self.node_enable {
                        Some(e) => format!("命中 {cand}（使能 {e}）"),
                        None => format!("命中 {cand}"),
                    },
                };
                self.support = Some(outcome.clone());
                return outcome;
            }
        }

        self.node_ctrl = None;
        self.node_read = None;
        self.node_enable = None;
        self.enable_engaged = false;
        // ⚠️ 找不到节点时，**先怀疑系统开关、再怀疑机型**
        //
        // # 为什么把"电池保护没开"放在第一位
        //
        // 这两种原因的**表象完全相同**（都表现为节点不存在或不可写），
        // 但处理成本差了一个数量级：
        //
        // | 原因 | 用户要做什么 | 成本 |
        // |---|---|---|
        // | **电池保护开关没开** | 去设置里打开 | 10 秒 |
        // | 机型真的不支持 | 无解，放弃该功能 | —— |
        //
        // 若只写"本机型不支持"，绝大多数用户会直接放弃 ——
        // 而他们很可能只是**少开了一个开关**。
        //
        // 反过来写（先提示查开关）不会误导：真不支持的机型用户
        // 查完开关发现本来就是开的，自然得出"确实不支持"的结论。
        //
        // ⚠️ 2026-10-19：文案**不再点名 `smart_chg`**，也不再假定
        //    "本机一定有电池保护开关"。标准 `power_supply` 的机型
        //    用的是 `charge_control_end_threshold`，有些机型压根没有
        //    「电池健康保护」这个设置项 —— 点名某个节点/开关会让
        //    这类用户以为自己的机器缺了东西。
        //    措辞改成条件式（"若你的机型有…开关"），两类机型都对得上。
        let outcome = ProbeOutcome::Unsupported {
            reason: format!(
                "未找到可写的充电上限节点。若你的机型有「电池健康保护」开关，\
                 请先确认它已打开（设置 → 省电与电池 → 电池保护）；\
                 若本来就开着，则本机型可能不支持充电上限\
                 （已尝试 {} 条候选路径）",
                SMART_CHG_CANDIDATES.len()
            ),
        };
        self.support = Some(outcome.clone());
        outcome
    }

    fn tick(&mut self, io: &mut dyn AddonIo, _now_s: i64) -> TickOutcome {
        let interval = self.config.interval_s();

        // ① 未启用 → **零写入**（决策 B：停止干预，一个字都不写）
        if !self.config.enabled {
            return TickOutcome::idle(interval);
        }

        // ② ⚠️ **插着充电器才巡检**
        //
        // # 为什么要加这个判据
        //
        // 限制值只可能在**充电时**被改写 —— 没插电时驱动根本不会去充，
        // 也就不存在"限制失效"的风险。而每次巡检都是一次 sysfs 读取，
        // 会打断 CPU 深度睡眠。
        //
        // 宿主（worker）据同一判据把"未充电 + 无客户端"整段进入深度阻塞，
        // 真正做到空闲时零周期性唤醒。详见 docs/13-检测功能与后台策略.md §5.3。
        //
        // ⚠️ 必须**每拍现查**（而不是缓存成字段）：缓存会在插拔瞬间过期，
        //    导致"插了电却仍在跳过"或反之。
        //
        // # 边界：插电瞬间
        //
        // 深度阻塞由 `power_supply` uevent 唤醒，插上充电器会**立即**跑一拍，
        // 此时 `is_plugged()` 为真，本函数正常工作。因此不存在
        // "插了电但限制值没同步"的窗口。
        //
        // # 边界：拔电后限制值被系统改写
        //
        // 拔电后不再巡检，若此时驱动自行改写了限制值，要等到**下次插电**
        // 才会被发现并修回。这是可接受的：
        //   · 没插电时限制值是多少都无所谓（不充电）
        //   · 下次插电的第一件事就是校验并修正（见上面"插电瞬间"）
        if !io.is_plugged() {
            return TickOutcome::idle(interval);
        }

        // ③ 配置自检（fail-closed：坏配置绝不拿去写节点）
        //
        // ⚠️ 用**本机检测出的区间**校验，而非写死的 60~100
        let (range_min, range_max) = self.effective_range();
        if let Err(e) = self.config.validate_within(range_min, range_max) {
            return TickOutcome::failed(AddonError::out_of_range(e), interval);
        }

        // ④ 节点必须已探测到
        let (Some(ctrl), Some(read)) = (self.node_ctrl.clone(), self.node_read.clone()) else {
            return TickOutcome::failed(
                AddonError::node_missing("smart_chg 节点不可用（本机型可能不支持充电上限）"),
                interval,
            );
        };

        // ⑤ 读"生效值"——**永远读 soc_limit，不读 smart_chg**
        // ⚠️ 读不到「生效值」**不是错误**
        //
        // `read` 由 `read_node_for` 推出：
        //   · 小米方言 → 同目录的 `soc_limit`
        //   · 标准方言 → 控制节点自己
        //
        // 而 `soc_limit` 这个节点**大多数机型根本没有**（实测 2026-10-19：
        // 走 `qcom-battery/smart_chg` 的机型上它不存在）。原实现把它当
        // `failed`，于是每拍都报错、`writes` 永远是 0 —— 限制值一次都
        // 没写进去，而界面上只看到"失败"，看不出原因。
        //
        // 读不到 ⇒ 视为**未知** ⇒ 照常走下面的 `needs_rewrite` 写入。
        // 这正是那个函数的设计意图：`actual: Option<u8>`，`None` 就写。
        let actual = match io.read(&read) {
            Ok(raw) => logic::parse_active(&raw),
            Err(_) => None,
        };

        self.last_seen = actual;

        // ⑥ 已一致 → 不写（这是常态：绝大多数拍什么都不做）
        if !logic::needs_rewrite(self.config.soc, actual) {
            return TickOutcome::idle(interval);
        }

        // ⑦ 写控制字 `(上限 << 16) | 0x11`
        // 按命中节点的方言生成写入内容
        let text = match self.dialect {
            // 小米：控制字（位域打包）
            Dialect::ControlWord => logic::format_word(logic::encode(self.config.soc)),
            // 标准：纯百分比
            Dialect::Percent => self.config.soc.to_string(),
        };

        // ⑦-0 🔴 **先开使能位**（标准百分比方言，2026-10-19 真机查明）
        //
        // # 不开会怎样
        //
        // K70 Pro 上 `charge_control_en = 0` 时，写
        // `charge_control_end_threshold = 95` **返回成功但值被吞**，
        // 读回永远是 0。模块的 ⑧ 回读校验于是判"不符"，连续三次后
        // 误报"有别的程序在抢"并熔断 —— 界面上就成了"充电上限不可用"。
        //
        // ⇒ 写目标值之前必须先把使能位置 1。
        //
        // ⚠️ 只在**需要写目标值**时才开（即走到这里），不在 probe 阶段开
        //    —— 否则又破了"未启用 = 零写入"。
        // ⚠️ `enable_engaged` 只是省掉重复写：掉电/系统重置后它会
        //    变回 0，但那时回读校验失败会让本拍重来一次，不会卡死。
        if let (Dialect::Percent, Some(en)) = (self.dialect, self.node_enable.clone()) {
            if !self.enable_engaged {
                // 先读一次当前值：已经是 1 就不重复写（写 sysfs 有 I2C 开销）
                let already = io.read(&en).ok().map(|s| s.trim() != "0").unwrap_or(false);
                if !already {
                    if let Err(e) = io.write(&en, "1") {
                        return TickOutcome::failed(
                            AddonError::bad_cond(format!(
                                "打开充电控制使能 {en} 失败：{e}\
                                 （该节点必须为 1，否则充电上限写入会被驱动丢弃）"
                            )),
                            interval,
                        );
                    }
                }
                self.enable_engaged = true;
            }
        }

        if let Err(e) = io.write(&ctrl, &text) {
            return TickOutcome::failed(
                AddonError::bad_cond(format!("写入 {ctrl} 失败：{e}")),
                interval,
            );
        }
        self.last_written = Some(self.config.soc);
        self.write_count = self.write_count.saturating_add(1);

        // ⑧ 回读校验（读 soc_limit）：写得下去 ≠ 生效
        let verify = io
            .read(&read)
            .ok()
            .and_then(|raw| logic::parse_active(&raw));
        self.last_seen = verify;
        if logic::needs_rewrite(self.config.soc, verify) {
            self.verify_fail_streak = self.verify_fail_streak.saturating_add(1);
            let seen = verify.map_or_else(|| "读不到".to_string(), |v| format!("{v}%"));
            return TickOutcome {
                wrote: true,
                changed: true,
                error: Some(AddonError::bad_cond(format!(
                    "写入后回读不符：期望 {}%，实得 {seen}{}",
                    self.config.soc,
                    self.conflict_hint()
                ))),
                next_interval_s: interval,
            };
        }
        // 生效了 ⇒ 冲突计数清零（单次不符可能只是驱动延迟一拍）
        self.verify_fail_streak = 0;

        TickOutcome::wrote(interval)
    }

    fn release(&mut self, io: &mut dyn AddonIo) -> TickOutcome {
        // ⚠️ 标准百分比方言下，**开过使能位就要关回去**（2026-10-19）
        //
        // 小米 `smart_chg` 那套是"零写入"就能停止干预（控制字自带使能位，
        // 不写就保持原状）。但标准框架的 `charge_control_en` 是**我们主动
        // 打开的** —— 不关它，用户关掉模块功能后，驱动仍在执行我们上次
        // 写入的阈值，等于"关了但没关"。
        //
        // ⇒ 只有真的开过（`enable_engaged`）才写 0，避免无谓写入。
        let mut wrote = false;
        if self.enable_engaged {
            if let Some(en) = self.node_enable.clone() {
                if io.write(&en, "0").is_ok() {
                    wrote = true;
                }
            }
            self.enable_engaged = false;
        }

        // 其余为决策 B：停止干预 = **零写入**。
        // 小米那套由系统自带的智能断充（默认 80%）照常工作。
        self.last_written = None;

        if wrote {
            // 有写入就必须让下一拍尽快确认状态
            TickOutcome::wrote(IDLE_INTERVAL_S)
        } else {
            TickOutcome::idle(IDLE_INTERVAL_S)
        }
    }

    fn status(&self) -> AddonStatus {
        let spec = self.spec();
        // 「已生效」的判据：**读生效值节点**
        //
        // ⚠️ 不能用控制节点的读值（小米方言下那是 `smart_chg`）——
        //    它在 16/17/80 之间漂移，与写入值无稳定对应关系
        //    （真机实测确证）。详见 [`logic`] 的文档。
        //    标准方言下控制节点自己就是生效值，读它恰好是对的
        //    （见 [`read_node_for`]）。
        //
        // 未插充电器时驱动只记录不执行，此时生效值可能读到一个
        // 与目标不符的值 —— 这**不是失败**，插电后自会生效。
        // 因此这里只如实报告"实际读到多少"，不在后端做"是否生效"的判决。
        let active = self.last_seen;
        let matches_target = active == Some(self.config.soc);

        // ⚠️ 使能位没开时，即使读数与目标一致也**不算生效**（2026-10-19）
        //
        // # 为什么必须扣掉这一种情况
        //
        // `charge_control_end_threshold` 的读值是**持久化**的：写进去
        // 90 之后，即使把 `charge_control_en` 关回 0，节点仍然读 90。
        // 而 en=0 时驱动**不执行**这个阈值。
        //
        // 于是（真机实测）：模块刚启动、还没写过使能位，读回 90 恰好
        // 等于目标 ⇒ 报"已生效" ⇒ 用户以为限制生效了，其实系统根本没管。
        //
        // ⇒ 本机有使能节点、且我们没开它 ⇒ 一律 `false`。
        let enable_ok = match &self.node_enable {
            // 没有使能节点的机型：不受这条限制
            None => true,
            // 有使能节点：必须是我们开过了
            Some(_) => self.enable_engaged,
        };
        let in_effect = matches_target && enable_ok;

        AddonStatus {
            id: spec.id.to_string(),
            label: spec.label.to_string(),
            summary: spec.summary.to_string(),
            origin: spec.origin.to_string(),
            enabled: self.config.enabled,
            supported: self.supported(),
            tripped: false, // 熔断状态由 runner 覆盖
            interval_s: self.config.interval_s(),
            probe: self
                .support
                .as_ref()
                .map_or_else(|| "尚未探测".to_string(), |p| p.message().to_string()),
            // ⚠️ 与 `spec().requires` **同源** —— 界面与 CLI 拿同一份文案
            requires: spec.requires.map(str::to_string),
            detail: json!({
                "soc_target": self.config.soc,
                "soc_active": active,
                // 目标与生效是否一致（界面据此决定要不要提示"尚未生效"）
                "soc_in_effect": in_effect,
                "verify_s": self.config.verify_s,
                "keep_on_restore": self.config.keep_on_restore,
                "warn_on_conflict": self.config.warn_on_conflict,
                // 当前是否已构成"疑似写入方冲突"（界面可据此显示提示）
                "conflict_suspected": self.verify_fail_streak >= 2,
                "node_ctrl": self.node_ctrl,
                "node_read": self.node_read,
                "last_written": self.last_written,
                "write_count": self.write_count,
                // 系统界面不会同步 —— 必须主动说明，否则用户看到系统的
                // 80% 会以为没生效
                "note_ui_not_synced": NOTE_UI_NOT_SYNCED,
            }),
        }
    }

    fn enabled(&self) -> bool {
        self.config.enabled
    }

    fn supported(&self) -> bool {
        self.support
            .as_ref()
            .is_some_and(ProbeOutcome::is_supported)
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
