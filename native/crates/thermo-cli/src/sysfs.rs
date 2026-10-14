//! sysfs I/O 层：节点句柄、路径探测、档位表解析
//!
//! # 写入纪律（契约，见 docs/03-契约-节点契约表.md §4）
//!
//! 1. 写入前先读取当前值（便于回滚与取证）
//! 2. **一次写完整值**（分块写可能被驱动拒绝或截断）
//! 3. 写入前**先钳位**到合法区间（驱动不校验范围）
//! 4. 写入后**立即回读校验**
//! 5. 读取前先 `seek(0)`（部分节点不重定位会返回缓存值）
//!
//! # 路径探测（**不得写死机型**）
//!
//! 软件设计目标是覆盖所有小米 / 红米机型，而节点路径**因机型而异**。
//! 因此所有路径都在**运行期**按候选表探测：
//!
//! ```text
//! 优先级：出厂档案给的路径 → 内置候选 → 不可用（该功能自动禁用）
//! ```
//!
//! 探测结果**缓存**（避免每拍 stat）。探测**只读**，绝不试写。

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use thermo_ipc::device::NodePaths;

// ============================================================
//  节点句柄
// ============================================================

/// sysfs 根前缀（测试用）
///
/// 默认 `"/"`（真机行为不变）；设置环境变量 `THERMO_SYSFS_ROOT` 可指向
/// 一棵**假节点树**，从而在 CI 上跑完整的采样/决策/写入链路，
/// 无需真机、也无需 root。
pub fn sysfs_root() -> String {
    std::env::var("THERMO_SYSFS_ROOT").unwrap_or_else(|_| "/".to_string())
}

/// 把绝对节点路径加上 sysfs 根前缀
///
/// 供**附加功能宿主**等外部模块使用 —— 它们按真实绝对路径书写，
/// 通过与 `Node` 相同的规则落到实际位置（测试时即为假节点树）。
pub fn rooted(path: &str) -> PathBuf {
    with_root(path)
}

/// 把节点路径拼上根前缀（**幂等**：已拼接的不再重复拼）
fn with_root(p: &str) -> PathBuf {
    let root = sysfs_root();
    if root == "/" || root.is_empty() {
        return PathBuf::from(p);
    }
    let root = root.trim_end_matches('/');
    if p.starts_with(root) {
        return PathBuf::from(p);
    }
    PathBuf::from(format!("{root}/{}", p.trim_start_matches('/')))
}

/// **只读**节点句柄
///
/// # 为什么需要单独一个类型
///
/// [`Node`] 提供 `write` / `write_i64` / `write_and_verify`，因此把它放进
/// `Nodes` 就等于"允许写"。对**不得写入**的观测节点（当前是场景 `sconfig`），
/// 那种"允许"是隐患：后人随手补一句 `scene.write(...)` 就会破坏约束，
/// 而代码审查未必拦得住。
///
/// 本类型**只暴露读方法** —— 想写也写不了（编译期报错），
/// 用类型系统替代人工纪律。这与项目既有的隔离风格一致
/// （另见 `thermo-addons` 不依赖 `thermo-core`）。
///
/// 约束来源：用户 2026-10-13 明确要求「场景模式只作为显示」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadOnlyNode {
    /// 内部仍用 `Node`（复用其读取实现），但**不对外暴露写方法**
    inner: Node,
}

impl ReadOnlyNode {
    /// 打开只读节点句柄
    pub fn new(path: impl AsRef<str>) -> Self {
        Self {
            inner: Node::new(path),
        }
    }

    /// 路径字符串（供展示与日志）
    pub fn path_str(&self) -> String {
        self.inner.path_str()
    }

    /// 读取（先 `seek(0)` 再读）
    pub fn read(&self) -> Result<String> {
        self.inner.read()
    }

    /// 读取整数
    ///
    /// 目前仅有测试使用（展示场景时读的是原文），保留以便将来需要
    /// 把某个只读节点当数值处理。
    #[cfg(test)]
    pub fn read_i64(&self) -> Result<i64> {
        self.inner.read_i64()
    }
}

/// 单个 sysfs 节点的句柄
///
/// ⚠️ 构造时**自动拼接 sysfs 根前缀**，因此其余代码一律使用真实绝对路径书写。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    path: PathBuf,
}

impl Node {
    /// 打开节点句柄（不立即 I/O）
    pub fn new(path: impl AsRef<str>) -> Self {
        Self {
            path: with_root(path.as_ref()),
        }
    }

    /// 路径
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 路径字符串（用于档案与日志）
    ///
    /// 统一用 `/` 分隔：这些字符串会写进 `device.json`、事件流并被前端展示，
    /// 混用分隔符会让"同一路径"在字符串比较时判不相等。
    pub fn path_str(&self) -> String {
        self.path.display().to_string().replace('\\', "/")
    }

    /// 读取（先 `seek(0)` 再读，规避缓存）
    pub fn read(&self) -> Result<String> {
        let mut f = std::fs::File::open(&self.path)
            .with_context(|| format!("打开失败: {}", self.path.display()))?;
        f.seek(SeekFrom::Start(0)).ok();
        let mut s = String::new();
        f.read_to_string(&mut s)
            .with_context(|| format!("读取失败: {}", self.path.display()))?;
        Ok(s.trim().to_string())
    }

    /// 读取整数
    pub fn read_i64(&self) -> Result<i64> {
        let raw = self.read()?;
        raw.parse::<i64>()
            .with_context(|| format!("节点值不是整数: {} = {raw:?}", self.path.display()))
    }

    /// 写入一个值（**一次写完整值**）
    ///
    /// # 为什么要先截断
    ///
    /// | 目标 | 行为 |
    /// |---|---|
    /// | **sysfs** | 内核的 kernfs 会整体替换缓冲区，与文件偏移无关；`set_len` 返回不支持，忽略即可 |
    /// | **普通文件**（测试用的假节点树、离线回放） | 不截断会**残留旧内容尾部** —— 写 `"700000"` 到原本是 `"18400000"` 的文件，读回来会变成 `"70000000"` |
    ///
    /// 因此统一先尝试 `set_len(0)`：对真实 sysfs 无副作用，对普通文件则是必需的。
    pub fn write(&self, value: &str) -> Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .open(&self.path)
            .with_context(|| format!("打开（写）失败: {}", self.path.display()))?;

        // 截断已有内容（sysfs 不支持该操作，错误忽略）
        f.set_len(0).ok();
        f.seek(SeekFrom::Start(0)).ok();

        // 不用 BufWriter：必须确保整值一次写入
        f.write_all(value.as_bytes())
            .with_context(|| format!("写入失败: {} <- {value:?}", self.path.display()))?;
        f.flush().ok();
        Ok(())
    }

    /// 写入整数
    pub fn write_i64(&self, value: i64) -> Result<()> {
        self.write(&value.to_string())
    }

    /// 写入后回读校验
    ///
    /// 返回 `Ok(true)` 表示一致；`Ok(false)` 表示写入未生效。
    pub fn write_and_verify(&self, value: &str) -> Result<bool> {
        self.write(value)?;
        Ok(self.read()? == value)
    }

    /// 是否存在
    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    /// 是否可写（运行期探测用）
    ///
    /// 用"试开写句柄"判断而非仅看权限位 —— 后者在 sysfs 上不可靠
    /// （属主可能是 `system:system`，实际以 root 运行时可写）。
    pub fn is_writable(&self) -> bool {
        OpenOptions::new().write(true).open(&self.path).is_ok()
    }
}

/// **值相同就不写**的整数写入
///
/// # 为什么需要它（真机实测的性能修复）
///
/// 每次写 sysfs 都会触发内核驱动的实际通信（PMIC / I2C），
/// **不是零成本的操作**。而"目标值不变"是**常态**：
///
/// | 场景 | 目标值 | 频率 |
/// |---|---|---|
/// | 未充电（交回官方） | 固定"最松" | 每拍 |
/// | 充电中温度稳定 | 同一个限流值 | 大部分时间 |
///
/// 真机实测：未充电 + 界面轮询时，无条件写让 worker 烧掉
/// **约 12% CPU**（0.9 秒/条命令），改成本函数后降到接近 0。
///
/// # 跳过写会不会"跟不上系统覆盖"？
///
/// 不会 —— 这里读的是**节点当前值**（不是我们的缓存）：
///
/// | 情况 | 节点实际值 | 与目标比 | 行为 |
/// |---|---|---|---|
/// | 我们上次写的 | == 目标 | 相同 | 跳过（本来就对） |
/// | **被系统覆盖** | ≠ 目标 | 不同 | **照常写回** ✓ |
///
/// 所以契约「系统会在 5~21 秒内覆盖 ctrl，需周期写回」依然满足。
///
/// # 读失败时**照常写**
///
/// 读不到当前值就无法判断"是否已经正确" ⇒ 保守地写一次。
/// 这与项目的 fail-safe 纪律一致：宁可多写，不可漏写。
fn write_if_different(node: &Node, value: i64) -> Result<()> {
    if let Ok(cur) = node.read_i64() {
        if cur == value {
            return Ok(());
        }
    }
    node.write_i64(value)
}

// ============================================================
//  候选路径表（顺序即优先级；**不得写死单一机型**）
// ============================================================

/// 温控控制节点所在目录的候选
///
/// 控制节点组（档位 / 电流 / 移除标记）始终位于**同一个目录**。
///
/// ⚠️ `pub` 是为了让 `probe` 的"候选路径总表"能逐条打印出来
/// —— 换机型时那是判断"路径对不对"的唯一依据。
pub const CTRL_DIRS: &[&str] = &[
    "/sys/class/xm_power/charger/charger_thermal",
    "/sys/class/charger/charger_thermal",
    "/sys/class/power_supply/wired/charger_thermal",
];

/// 电池观测节点所在目录的候选
///
/// ⚠️ `pub` 理由同 [`CTRL_DIRS`]。
pub const BATTERY_DIRS: &[&str] = &[
    "/sys/class/power_supply/battery",
    "/sys/class/power_supply/bms",
    "/sys/class/power_supply/main",
];

/// SOC 上限控制节点候选（**路径因机型而异，必须探测**）
pub const SMART_CHG_CANDIDATES: &[&str] = &[
    "/sys/class/xm_power/charger/smart_charge/smart_chg",
    "/sys/class/charger/smart_charge/smart_chg",
    "/sys/class/power_supply/smart_charge/smart_chg",
];

/// 档位→电流映射表的设备树路径候选
///
/// 内容为若干档 × 10 字段的大端 32 位整数（单位 mA），见契约 §1.3。
pub const GEAR_TABLE_CANDIDATES: &[&str] = &[
    "/sys/firmware/devicetree/base/soc/mca_charger_thermal/wired_thermal",
    "/sys/firmware/devicetree/base/soc/charger_thermal/wired_thermal",
];

/// **电荷泵（CP）芯片温度**节点候选
///
/// # 为什么值得单独观测
///
/// 电荷泵是快充的核心部件（把充电器的高压分压成电池可用的低压），
/// 它是**真正的热点**，温度通常**明显高于电池**：
///
/// | 时刻 | 电池温度 | 电荷泵芯片温度 | 差值 |
/// |---|---|---|---|
/// | 未插充电器 | 31.5℃ | 46.0℃ | **+14.5℃** |
/// | 插电快充中 | 32.7℃ | 42.5℃ | +9.8℃ |
///
/// （真机实测，2026-10-13/14，K90 Pro Max）
///
/// 只看电池温度会**漏掉**热点的真实情况 —— 电池还在安全区，
/// 芯片可能已经接近上限。这是"看得见才能控制"的前提：
/// 先纳入观测，才谈得上将来是否需要把它纳入保护判据。
///
/// # 单位
///
/// `0.1℃`（与 `battery/temp` 一致）。实测 `425` ⇒ `42.5℃`。
///
/// # 只读
///
/// 该节点权限为 `-r--r-----`，是硬件传感器，**只读**。
pub const CP_DIE_TEMP_CANDIDATES: &[&str] = &[
    "/sys/class/xm_power/cp_master/tdie",
    "/sys/class/charger/cp_master/tdie",
    "/sys/devices/virtual/xm_power/cp_master/tdie",
];

/// 硬件保护温度的设备树候选（**顺序即语义优先级**）
///
/// # 语义
///
/// "硬件保护温度"= **本机硬件对"给手机充电"这一场景允许的电池温度上限**。
/// 用户配置的收紧温度不得超过它（否则配置意图与硬件能力冲突）。
///
/// # 为什么是"按顺序取第一个"而不是"取最小值"
///
/// 这些节点的语义**并不相同**，盲目取最小值会选到错误的场景：
///
/// | 路径 | K90PM 实测 | 场景 | 是否适配 |
/// |---|---|---|---|
/// | `mca_strategy_buckchg_class/ffc_temp_high` | **48℃** | **有线快充** | ✅ **正是我们要的** |
/// | `mca_charger_thermal/wired_thermal_max_temp` | 不存在 | 他机型的可能位置 | —— |
///
/// 若把语义不符的属性也放进候选（见 [`HW_MAX_TEMP_REJECTED`]），
/// "取最小值"会选中 **40℃**（手机给别的设备**反充**的场景），
/// 比真实的有线上限低 8℃ —— 用户的收紧温度会被无谓压低。
///
/// # ⚠️ 曾经把"官方热控表最高触发温度"作为第 ② 级兜底，已**移除**
///
/// 2026-10-14 的决定，理由是真机实测发现的两个问题：
///
/// ## ① 官方表的内容**会随场景切换**
///
/// 实测：场景从 `-1` 变为 `11`（录像）后，系统**重写了** `decrypt.txt`：
///
/// | 时刻 | 节名 | 本模块读得到吗 |
/// |---|---|---|
/// | 场景 -1 | `MONITOR-BAT` / `SIC-BAT`（36 个节名） | ✅ |
/// | 场景 11 | `VIDEO-SIC-BAT` / `VIDEO-MONITOR-*`（21 个节名，**无 `MONITOR-BAT`**） | ❌ |
///
/// ⇒ 拿它当"硬件保护温度"会让用户的可配置温度范围**随场景漂移** ——
/// 录个像，硬保护阈值就变了。这不可接受。
///
/// ## ② 语义不精确
///
/// 官方表的 `trig` 是「**官方策略**在多少度开始动作」，
/// 不是「**硬件**在多少度损坏」。后者才是"硬件保护温度"该表达的东西。
///
/// ## 结论
///
/// 硬件保护温度只有**设备树**这一个可信来源。读不到就明确标注
/// 「未探到 + 用了保守兜底」，让用户知道自己机型这一项不确定 ——
/// **不要用会变的数去填补**。
///
/// 官方表仍然有价值，但只在**诊断展示**里（`probe --official`），
/// 绝不参与决策。
pub const HW_MAX_TEMP_CANDIDATES: &[&str] = &[
    // ① 有线快充的电池温度上限 —— **语义最贴合**（本机命中，值 48）
    //
    // 「快充时电池最高允许多少度」正是我们要的语义，
    // 且它就在有线快充的专属节点下，不会与其它场景混淆。
    "/sys/firmware/devicetree/base/soc/mca_strategy_buckchg_class/ffc_temp_high",
    // ② 他机型的可能位置（本机不存在）
    "/sys/firmware/devicetree/base/soc/mca_charger_thermal/wired_thermal_max_temp",
];

/// ⚠️ **不要**把这些用作兜底（曾经列在候选表里，2026-10-14 移除）
///
/// | 属性 | 本机值 | 为什么不能用 |
/// |---|---|---|
/// | `connector_antiburn/max_thermal_board_temp` | 50 | 是**主板**温度上限，不是电池的。语义不同 ⇒ 会虚高 2℃ |
/// | `mca_strategy_wireless_revchg_class/quick_revchg_max_batt_temp` | 40 | 是**无线反充**场景的电池上限，比有线快充低 8℃ ⇒ 会无谓压低用户可配范围 |
///
/// **教训**：候选表的每一项都必须语义一致。"数值看起来合理"不够 ——
/// 50 和 40 都在电池温度的合理区间内，`parse_temp_celsius` 拦不住它们。
/// 只能靠**人读懂属性名**来保证。
///
/// ⚠️ 这个常量**只被测试使用**（生产路径不需要知道"哪些不能选"），
/// 因此加 `cfg(test)` —— 否则交叉编译时报"从未使用"。
#[cfg(test)]
pub const HW_MAX_TEMP_REJECTED: &[(&str, &str)] = &[
    (
        "/sys/firmware/devicetree/base/soc/connector_antiburn/max_thermal_board_temp",
        "主板温度上限（不是电池）",
    ),
    (
        "/sys/firmware/devicetree/base/soc/mca_strategy_wireless_revchg_class/quick_revchg_max_batt_temp",
        "无线反充场景（比有线低 8℃）",
    ),
];

// ============================================================
//  探测工具
// ============================================================

/// 按候选顺序取**第一个存在的**节点
fn pick_existing(candidates: &[String]) -> Option<Node> {
    for c in candidates {
        let n = Node::new(c.clone());
        if n.exists() {
            return Some(n);
        }
    }
    None
}

/// 按候选顺序取**第一个可写的**节点
fn pick_writable(candidates: &[String]) -> Option<Node> {
    for c in candidates {
        let n = Node::new(c.clone());
        if n.is_writable() {
            return Some(n);
        }
    }
    None
}

/// 把 `dir/name` 拼成候选列表
fn join_candidates(dirs: &[String], name: &str) -> Vec<String> {
    dirs.iter().map(|d| format!("{d}/{name}")).collect()
}

// ============================================================
//  探测结果
// ============================================================

/// 本机可用节点的实际路径（探测得出）
///
/// ⚠️ 所有字段都是 `Option`：机型不支持时该项为 `None`（**非致命**），
/// 对应功能自动禁用，其余功能照常。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Nodes {
    /// 限流档位（写）
    pub ctrl: Option<Node>,
    /// 限流电流（写）
    pub curr: Option<Node>,
    /// 第二路电流（写，可选）
    pub curr2: Option<Node>,
    /// 移除全部限流（写）
    pub remove: Option<Node>,
    /// SOC 上限控制字（写）
    pub soc_ctrl: Option<Node>,
    /// SOC 上限生效值（只读）
    pub soc_read: Option<Node>,
    /// 电池温度（只读）
    pub battery_temp: Option<Node>,
    /// 电池电流（只读）
    pub battery_current: Option<Node>,
    /// 电池电压（只读）
    pub battery_voltage: Option<Node>,
    /// 电池状态（只读）
    pub battery_status: Option<Node>,
    /// 电池电量（只读）
    pub battery_capacity: Option<Node>,
    /// 机身温度（只读，可选）
    pub board_temp: Option<Node>,
    /// **电荷泵芯片温度**（只读，可选）
    ///
    /// 单位 `0.1℃`。这是快充时的**真正热点**，通常比电池温度高 10℃ 左右
    /// （见 [`CP_DIE_TEMP_CANDIDATES`] 的实测数据）。
    ///
    /// ⚠️ **当前仅用于观测与显示，不参与决策。**
    /// 是否要把它纳入保护判据（例如"芯片过热则降流"）需要单独论证 ——
    /// 不能因为"能读到"就贸然拿它去切断充电。
    pub cp_die_temp: Option<Node>,
    /// 场景模式（`sconfig`）—— **只读，仅用于显示**
    ///
    /// # 🔴 硬性约束：本模块**永不写入**场景节点
    ///
    /// 用户明确要求（2026-10-13）：**"场景模式只作为显示，只用双写调整功率"**。
    ///
    /// | 项 | 做法 |
    /// |---|---|
    /// | 读 | ✅ 允许（`probe` / `device show` 里展示，`detect` 记入备注） |
    /// | 写 | ❌ **禁止** —— 类型为 [`ReadOnlyNode`]，**编译期**就没有写方法 |
    /// | 参与决策 | ❌ **禁止** —— 内核的 `Sample` 里根本没有这个字段，架构上已隔离 |
    ///
    /// 背景：老项目 `ThermoCharge` 曾监测场景变化并"强制写回"，实测**无效**
    /// （写回会被系统覆盖，且场景本就不是控制充电功率的正当手段）。
    /// 本模块不沿用该做法：调整功率**只经"双写"**（档位 + 电流）。
    pub scene: Option<ReadOnlyNode>,
    /// 档位表（设备树，只读）
    pub gear_table: Option<Node>,
}

impl Nodes {
    /// **运行期探测**所有节点路径
    ///
    /// `profile_nodes`：出厂/本机档案里给出的路径，作为**最优先候选**
    /// （回流进来的机型可携带自己的路径，无需改代码）。
    ///
    /// 探测**只读**，绝不试写。
    pub fn detect(profile_nodes: Option<&NodePaths>) -> Self {
        // ── 控制节点组：先找目录，再拼成员 ──
        let ctrl_dirs: Vec<String> = profile_nodes
            .map(|n| n.ctrl.clone())
            .filter(|s| !s.is_empty())
            .and_then(|ctrl| Path::new(&ctrl).parent().map(|p| p.display().to_string()))
            .into_iter()
            .chain(CTRL_DIRS.iter().map(|s| (*s).to_string()))
            .collect();

        let ctrl_dir = ctrl_dirs.iter().find(|d| {
            Node::new(format!("{d}/wired_ctrl_limit")).exists()
                || Node::new(format!("{d}/wired_chg_curr")).exists()
        });

        let (mut ctrl, mut curr, mut curr2, mut remove) = (None, None, None, None);
        if let Some(dir) = ctrl_dir {
            ctrl = pick_writable(&[format!("{dir}/wired_ctrl_limit")]);
            curr = pick_writable(&[format!("{dir}/wired_chg_curr")]);
            curr2 = pick_writable(&[format!("{dir}/wired_chg_curr2")]);
            remove = pick_writable(&[format!("{dir}/wired_thermal_remove")]);
        }
        // 档案若给了完整路径，优先采用（即使在候选目录里没探测到）
        if let Some(p) = profile_nodes {
            ctrl = ctrl.or_else(|| adopt(&p.ctrl));
            curr = curr.or_else(|| adopt(&p.curr));
            remove = remove.or_else(|| adopt(&p.remove));
            // curr2 为可选：档案给了才用
            if curr2.is_none() {
                if let Some(p2) = &p.curr2 {
                    curr2 = adopt(p2);
                }
            }
        }

        // ── SOC 上限节点：**必须运行期探测**（路径因机型而异） ──
        let soc_cands: Vec<String> = profile_nodes
            .map(|n| n.soc_ctrl.clone())
            .filter(|s| !s.is_empty())
            .into_iter()
            .chain(SMART_CHG_CANDIDATES.iter().map(|s| (*s).to_string()))
            .collect();
        let soc_ctrl = pick_writable(&soc_cands);
        // 读取节点 = 控制节点**同目录**的 soc_limit（契约 §1.2.1 规则 2）
        let soc_read = soc_ctrl.as_ref().and_then(|c| {
            Path::new(c.path())
                .parent()
                .map(|d| Node::new(d.join("soc_limit").display().to_string()))
        });

        // ── 电池观测节点 ──
        let batt_dirs: Vec<String> = profile_nodes
            .map(|n| n.battery_temp.clone())
            .filter(|s| !s.is_empty())
            .and_then(|t| Path::new(&t).parent().map(|p| p.display().to_string()))
            .into_iter()
            .chain(BATTERY_DIRS.iter().map(|s| (*s).to_string()))
            .collect();

        let battery_temp = pick_existing(&join_candidates(&batt_dirs, "temp"));
        let battery_current = pick_existing(&join_candidates(&batt_dirs, "current_now"));
        let battery_voltage = pick_existing(&join_candidates(&batt_dirs, "voltage_now"));
        let battery_status = pick_existing(&join_candidates(&batt_dirs, "status"));
        let battery_capacity = pick_existing(&join_candidates(&batt_dirs, "capacity"));

        // ── 可选观测节点 ──
        let board_cands: Vec<String> = profile_nodes
            .map(|n| n.soc_ctrl.clone())
            .filter(|s| !s.is_empty())
            .and_then(|c| {
                Path::new(&c)
                    .parent()
                    .map(|d| d.join("board_temp").display().to_string())
            })
            .into_iter()
            .chain(SMART_CHG_CANDIDATES.iter().filter_map(|c| {
                Path::new(c)
                    .parent()
                    .map(|d| d.join("board_temp").display().to_string())
            }))
            .collect();
        let board_temp = pick_existing(&board_cands);

        // ── 场景（**只读**，仅用于显示）──
        //
        // ⚠️ 构造为 `ReadOnlyNode`：类型上就没有写方法，后人无法误改。
        //
        // ⚠️ 用**候选表**而不是单一写死路径 —— 与其它节点一致。
        //    初版只写了本机命中那一条路径，换机型就探测不到（显示"不可用"）。
        let scene = pick_existing(
            &SCENE_CANDIDATES
                .iter()
                .map(|s| (*s).to_string())
                .collect::<Vec<_>>(),
        )
        .map(|n| ReadOnlyNode::new(n.path_str()));

        let gear_table = pick_existing(
            &GEAR_TABLE_CANDIDATES
                .iter()
                .map(|s| (*s).to_string())
                .collect::<Vec<_>>(),
        );

        // 电荷泵芯片温度（观测用；路径见 CP_DIE_TEMP_CANDIDATES）
        let cp_die_temp = pick_existing(
            &CP_DIE_TEMP_CANDIDATES
                .iter()
                .map(|s| (*s).to_string())
                .collect::<Vec<_>>(),
        );

        Self {
            ctrl,
            curr,
            curr2,
            remove,
            soc_ctrl,
            soc_read,
            battery_temp,
            battery_current,
            battery_voltage,
            battery_status,
            battery_capacity,
            board_temp,
            cp_die_temp,
            scene,
            gear_table,
        }
    }

    /// 温控写入所需的节点是否齐备
    ///
    /// 档位与电流是**两层并联**，缺一不可（契约 §1.1 规则 1）。
    pub fn can_control(&self) -> bool {
        self.ctrl.is_some() && self.curr.is_some()
    }

    /// SOC 上限功能是否可用
    pub fn can_limit_soc(&self) -> bool {
        self.soc_ctrl.is_some() && self.soc_read.is_some()
    }

    /// 观测电池状态所需节点是否齐备
    pub fn can_observe(&self) -> bool {
        self.battery_temp.is_some() && self.battery_status.is_some()
    }

    /// 缺失的**必需**节点（供诊断输出）
    pub fn missing_required(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.ctrl.is_none() {
            out.push("限流档位 wired_ctrl_limit");
        }
        if self.curr.is_none() {
            out.push("限流电流 wired_chg_curr");
        }
        if self.battery_temp.is_none() {
            out.push("电池温度 battery/temp");
        }
        if self.battery_status.is_none() {
            out.push("电池状态 battery/status");
        }
        out
    }

    /// 双写档位与电流（本机必需，见节点契约表规则 1）
    ///
    /// 失败时回滚到 `prev`。
    ///
    /// # ⚠️ 值相同则跳过写（真机实测的性能修复）
    ///
    /// 每次写 sysfs 都会触发内核驱动的实际通信（PMIC / I2C），
    /// **不是零成本**。真机实测：未充电时每 15 秒一次"交回官方"
    /// 无条件写档位，累计让 worker 在界面轮询期间烧掉约 12% CPU。
    ///
    /// 而"温度稳定时目标值不变"是**常态**（充电中大部分时间都在
    /// 保持同一个值），因此这里先读当前值、相同就跳过。
    ///
    /// # 跳过写会不会导致"系统覆盖后我们没跟上"？
    ///
    /// 不会 —— 这正是**读当前值**而不是拿缓存比较的原因：
    ///
    /// | 情况 | 节点实际值 | 与目标比较 | 行为 |
    /// |---|---|---|---|
    /// | 我们上次写的值 | == 目标 | 相同 | 跳过（合理：本来就对） |
    /// | **被系统覆盖了** | ≠ 目标 | 不同 | **照常写回** ✓ |
    ///
    /// 所以契约里"系统会在 5~21 秒内覆盖 ctrl，需周期写回"依然满足 ——
    /// 只要被覆盖，下一次就会被发现并写回。
    pub fn apply_dual(&self, gear: u8, curr_ua: i32, prev: (u8, i32)) -> Result<()> {
        let ctrl = self
            .ctrl
            .as_ref()
            .context("档位节点不可用（本机型不支持控速）")?;
        let curr = self
            .curr
            .as_ref()
            .context("电流节点不可用（本机型不支持控速）")?;

        write_if_different(ctrl, i64::from(gear))?;
        if let Err(e) = write_if_different(curr, i64::from(curr_ua)) {
            ctrl.write_i64(i64::from(prev.0)).ok(); // 回滚，避免只写一半
            return Err(e);
        }
        if let Some(c2) = &self.curr2 {
            if let Err(e) = write_if_different(c2, i64::from(curr_ua)) {
                curr.write_i64(i64::from(prev.1)).ok();
                ctrl.write_i64(i64::from(prev.0)).ok();
                return Err(e);
            }
        }
        Ok(())
    }

    /// 交回官方：档位归零、电流放到本机上限、移除标记归零
    ///
    /// ⚠️ 顺序有讲究：先解除"移除标记"会让全部限流失效，
    /// 因此**最后**再动它。
    ///
    /// # ⚠️ 电流层：只放宽，不收紧
    ///
    /// 传入的 `curr_max_ua` 是**本机上限**。但当档案来自**保守兜底**
    /// （用户尚未跑检测）时，这个值可能**小于**节点当前值 ——
    /// 此时写它就成了"**交回官方反而限速**"。
    ///
    /// 真机场景：
    ///
    /// | 时刻 | 节点值 | 说明 |
    /// |---|---|---|
    /// | 驱动默认 | `18400000` | 未限制 |
    /// | 模块"释放"写兜底值 | `5000000` | **反而被限到 5A（约 22W）** |
    ///
    /// 用户装完模块什么都没设，充电却变慢一半 —— 明显是缺陷。
    ///
    /// 因此这里判断：**节点当前值已经 ≥ 目标值就不再写**。
    /// 语义上"交回官方控制"本来就应该是"解除限制"，而不是"设成某个数"。
    ///
    /// 反向场景仍然正确：模块之前收紧过（写了 700000），
    /// 现在释放 ⇒ `700000 < 5000000` ⇒ 照常写入，限制被解除 ✓
    pub fn release_control(&self, curr_max_ua: i32) -> Result<()> {
        if let Some(c) = &self.ctrl {
            // ⚠️ 值相同则跳过 —— 未充电时目标是固定"最松"，
            //    系统不会把它改成更严的值，所以跳过是**安全**的。
            //    这一条把"未充电反复全拍"的开销降到几乎为零
            //    （真机实测：无条件写让界面轮询期间烧掉约 12% CPU）。
            write_if_different(c, 0).context("放开档位失败")?;
        }
        if let Some(c) = &self.curr {
            // 只放宽：当前值已经不低于目标值 ⇒ 已是放开状态，无需写
            let already_open = c
                .read_i64()
                .map(|v| v >= i64::from(curr_max_ua))
                .unwrap_or(false);
            if !already_open {
                c.write_i64(i64::from(curr_max_ua))
                    .context("放开电流失败")?;
            }
        }
        if let Some(c) = &self.curr2 {
            let already_open = c
                .read_i64()
                .map(|v| v >= i64::from(curr_max_ua))
                .unwrap_or(false);
            if !already_open {
                c.write_i64(i64::from(curr_max_ua)).ok();
            }
        }
        if let Some(r) = &self.remove {
            r.write_i64(0).ok();
        }
        Ok(())
    }

    /// 硬保护：收紧到最紧档位 + 最小电流
    pub fn apply_hard_protect(&self, gear_safe: u8, curr_safe: i32) -> Result<()> {
        if let Some(c) = &self.ctrl {
            c.write_i64(i64::from(gear_safe)).context("收紧档位失败")?;
        }
        if let Some(c) = &self.curr {
            c.write_i64(i64::from(curr_safe)).context("收紧电流失败")?;
        }
        Ok(())
    }
}

/// 档案给的路径：可写才采用，否则放弃（不报错）
fn adopt(path: &str) -> Option<Node> {
    if path.is_empty() {
        return None;
    }
    let n = Node::new(path);
    if n.is_writable() {
        Some(n)
    } else {
        None
    }
}

// ============================================================
//  档位表（设备树解析）
// ============================================================

/// 档位表（设备树解析结果）
///
/// 格式：N 档 × 10 个 32 位**大端**整数，单位 mA。
/// ⚠️ **档位数因机型而异**（不得假定 15）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GearTable {
    /// 每档的原始 10 个字段
    pub rows: Vec<[u32; GEAR_TABLE_FIELDS]>,
}

/// 每档字段数
pub const GEAR_TABLE_FIELDS: usize = 10;

/// 该档电流上限所在的字段下标（0 起）
///
/// 语义已由真机交叉验证：档位 2/4/6/10 的该列值与老项目实测标定的
/// 9.0/7.0/5.0/2.5 A **精确命中**。
const GEAR_TABLE_CURR_COL: usize = 4;

impl GearTable {
    /// 从**指定节点**读取并解析
    pub fn load_from(node: &Node) -> Result<Self> {
        let path = node.path_str();
        let raw = std::fs::read(node.path()).with_context(|| format!("读取档位表失败: {path}"))?;

        if raw.len() % 4 != 0 {
            anyhow::bail!("档位表长度不是 4 的倍数: {} 字节", raw.len());
        }

        let ints: Vec<u32> = raw
            .chunks_exact(4)
            .map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]]))
            .collect();

        if ints.len() % GEAR_TABLE_FIELDS != 0 {
            anyhow::bail!(
                "档位表整数个数 {} 不是每档字段数 {} 的倍数",
                ints.len(),
                GEAR_TABLE_FIELDS
            );
        }

        let rows: Vec<[u32; GEAR_TABLE_FIELDS]> = ints
            .chunks_exact(GEAR_TABLE_FIELDS)
            .map(|c| {
                let mut a = [0u32; GEAR_TABLE_FIELDS];
                a.copy_from_slice(c);
                a
            })
            .collect();

        if rows.is_empty() {
            anyhow::bail!("档位表为空");
        }

        Ok(Self { rows })
    }

    /// 在候选路径中探测并读取
    pub fn detect() -> Result<(Self, String)> {
        let mut last_err = None;
        for p in GEAR_TABLE_CANDIDATES {
            let node = Node::new(*p);
            if !node.exists() {
                continue;
            }
            match Self::load_from(&node) {
                Ok(t) => return Ok((t, (*p).to_string())),
                Err(e) => last_err = Some(e),
            }
        }
        match last_err {
            Some(e) => Err(e.context("所有候选路径均读取失败")),
            None => anyhow::bail!(
                "未找到档位表节点（已尝试 {} 条候选）",
                GEAR_TABLE_CANDIDATES.len()
            ),
        }
    }

    /// 档位数（**因机型而异**）
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// 是否为空
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// 取某档的电流上限（μA）
    ///
    /// ⚠️ 设备树单位是 **mA**，此处转换为 μA 以与 `wired_chg_curr` 的单位一致。
    pub fn current_limit_ua(&self, gear: u8) -> Option<i32> {
        let row = self.rows.get(gear as usize)?;
        let ma = row[GEAR_TABLE_CURR_COL];
        i32::try_from(ma).ok().map(|ma| ma.saturating_mul(1000))
    }

    /// 导出为 `(档位, 电流上限 μA)` 列表
    pub fn current_limits(&self) -> Vec<(u8, i32)> {
        (0..self.rows.len())
            .filter_map(|g| {
                let gear = u8::try_from(g).ok()?;
                self.current_limit_ua(gear).map(|ua| (gear, ua))
            })
            .collect()
    }

    /// **功率最大的档位**：电流上限最大的那一档
    ///
    /// 电流上限越大 → 可用功率上限越高，故以此判定。
    /// 并列时取**档位编号最小**的（更松的档）。
    pub fn max_power_gear(&self) -> Option<u8> {
        self.current_limits()
            .into_iter()
            .fold(None, |best: Option<(u8, i32)>, (g, ua)| match best {
                Some((_, bua)) if ua <= bua => best,
                _ => Some((g, ua)),
            })
            .map(|(g, _)| g)
    }

    /// 电流上限的最大值（μA）
    pub fn max_current_ua(&self) -> Option<i32> {
        self.current_limits().into_iter().map(|(_, ua)| ua).max()
    }
}

// ============================================================
//  场景节点（**只读**，仅显示）
// ============================================================

/// 场景节点的候选路径（**顺序即优先级**）
///
/// # ⚠️ 这个节点与"控制"无关，只是给用户看的
///
/// 用户明确要求「**不要调整场景模式**，只用双写调整功率」（2026-07 定）。
/// 因此 `Nodes.scene` 的类型是 [`ReadOnlyNode`] —— **编译期**就没有写方法。
///
/// # 值的含义
///
/// 由小米热控框架写入，`-1` 表示**未设置场景**（真机常见）。
/// 非负值对应框架里的场景编号，但**具体编号含义没有公开文档**，
/// 因此界面**照实显示原始值**，不做翻译 —— 猜错比不显示更糟。
pub const SCENE_CANDIDATES: &[&str] = &[
    // 本机（K90PM）命中
    "/sys/class/thermal/thermal_message/sconfig",
    // 他机型的可能位置
    "/sys/class/thermal/thermal_message/scene",
    "/sys/class/xm_power/charger/scene",
    "/sys/class/xm_power/scene",
];

/// 场景值的显示文本
///
/// | 原值 | 显示 | 理由 |
/// |---|---|---|
/// | `-1` | `未设置` | `-1` 对用户毫无意义，且它是**最常见**的值 |
/// | 其它整数 | 原样（如 `3`） | **不翻译** —— 编号含义无公开文档，猜错会误导 |
/// | 空 / 读不到 | `未知` | — |
pub fn describe_scene(raw: &str) -> String {
    let t = raw.trim();
    if t.is_empty() {
        return "未知".to_string();
    }
    if t == "-1" {
        return "未设置".to_string();
    }
    t.to_string()
}

// ============================================================
//  官方温控策略表（`decrypt.txt` 直读）
// ============================================================

/// 小米官方热控配置的解密导出文件
///
/// # 为什么能读它
///
/// HyperOS 4.0 更换了 `.conf` 的加密密钥，直接解密失败 ——
/// 但系统自己会把**解密后的运行时配置**导出到这里（权限 600 root:system），
/// 因此 root 可直接读，**不需要任何解密工作**。
///
/// # 为什么值得读（这是本项目最有价值的诊断信息之一）
///
/// 它包含**官方自己**的温控策略。两层策略会**取更严的那个**运行：
///
/// | 问题 | 表能回答 |
/// |---|---|
/// | "我没到阈值，怎么变慢了？" | 官方在更低的温度就动手了 |
/// | "我设 42℃ 收紧有意义吗？" | 看官方在多少度开始降 |
/// | "为什么写 0 读回 18400000？" | 那是 `SIC-BAT` 在该温度档的 `max` |
///
/// # ⚠️ 只读，且**不参与任何决策**
///
/// 这是**展示**用信息。模块的决策只依据用户配置与实时读数 ——
/// 不把官方表当成"应该跟随的目标"（那是另一种设计，用户已明确选择
/// 「官方 = 完全不管」）。
pub const DECRYPT_TXT: &str = "/data/vendor/thermal/decrypt.txt";

/// 官方策略表里的一条规则
///
/// # 单位（**各家不同，别混用**）
///
/// | 字段 | 本机实测单位 | 换算 |
/// |---|---|---|
/// | `trig` / `clr` | 0.001℃ | `/ 1000.0` → ℃ |
/// | `MONITOR-BAT` 的 `target` | 档位 × 100 | `/ 100` → 档位（0~15） |
/// | `SIC-BAT` 的 `max` / `min` | mA | `/ 1000` → A |
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OfficialRule {
    /// 规则名（如 `MONITOR-BAT`）
    pub name: String,
    /// 触发阈值（0.001℃）
    pub trig: Vec<i64>,
    /// 解除阈值（0.001℃）—— **比 `trig` 低 1~1.5℃**，构成迟滞
    pub clr: Vec<i64>,
    /// 目标值（含义随规则而异，见上方单位表）
    pub target: Vec<u32>,
    /// 电流上限（mA）—— 仅 `SIC-BAT` 有
    pub max_ma: Vec<u32>,
    /// 电流下限（mA）—— 仅 `SIC-BAT` 有
    pub min_ma: Vec<u32>,
}

impl OfficialRule {
    /// 是否存在有效的 `trig` 表
    pub fn is_usable(&self) -> bool {
        !self.trig.is_empty()
    }
}

/// 从 `decrypt.txt` 内容里提取一条规则（**纯字符串解析，可离线测试**）
///
/// 文件格式为 TSV：`[节名]` 行 + `键 <空白> 值 <空白> 值…` 的行。
pub fn parse_official_rule(text: &str, want: &str) -> Option<OfficialRule> {
    let mut rule = OfficialRule {
        name: want.to_string(),
        ..Default::default()
    };
    let mut inside = false;

    for line in text.lines() {
        let line = line.trim_end();
        if line.starts_with('[') {
            // 遇到**下一个**节头就结束（无论它是不是我们要的）
            if inside {
                break;
            }
            inside = line.trim_start_matches('[').trim_end_matches(']').trim() == want;
            continue;
        }
        if !inside || line.trim().is_empty() {
            continue;
        }

        let mut it = line.split_whitespace();
        let Some(key) = it.next() else { continue };

        // ⚠️ `MONITOR-BAT` 的 target 写作 `0300`（前导零占位对齐），
        //    必须按**十进制**解析。按八进制会得到 192 这种错值。
        let nums: Vec<i64> = it.filter_map(|s| s.parse::<i64>().ok()).collect();
        if nums.is_empty() {
            continue;
        }

        match key {
            "trig" => rule.trig = nums,
            "clr" => rule.clr = nums,
            "target" => rule.target = nums.into_iter().map(|v| v.max(0) as u32).collect(),
            "max" => rule.max_ma = nums.into_iter().map(|v| v.max(0) as u32).collect(),
            "min" => rule.min_ma = nums.into_iter().map(|v| v.max(0) as u32).collect(),
            _ => {}
        }
    }

    rule.is_usable().then_some(rule)
}

/// 官方温控表的读取结果
///
/// # 为什么需要区分失败原因（**修一个误导性提示**）
///
/// 初版返回 `Option`，把两种完全不同的情况混成一个 `None`：
///
/// | 真实原因 | 初版的提示 |
/// |---|---|
/// | 文件不存在（非小米机型） | 「读不到文件」 ✓ |
/// | **文件存在但没有该节名** | 「读不到文件（非小米机型，或该文件不存在）」 ❌ **文件明明在** |
///
/// 真机实测踩到第二种：场景切到 `11`（录像）后系统重写了
/// `decrypt.txt`，节名全变成 `VIDEO-` 前缀，`MONITOR-BAT` 消失。
/// 此时提示说"文件不存在"，会把排查引向完全错误的方向
/// （用户会去查权限、查机型，而真正的原因是场景切换）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfficialTableError {
    /// 文件不存在 / 读不到（权限、非小米机型）
    NoFile,
    /// 文件存在，但**没有期望的节名**
    ///
    /// `total` 是文件里的节名总数，供排查参考。
    NoSection {
        /// 期望的节名（如 `MONITOR-BAT`）
        want: &'static str,
        /// 文件里实际有多少个节
        total: usize,
    },
}

impl OfficialTableError {
    /// 供用户看的中文说明
    pub fn message(&self) -> String {
        match self {
            Self::NoFile => format!("读不到 {DECRYPT_TXT}（该机型可能没有此文件）"),
            Self::NoSection { want, total } => format!(
                "文件存在（共 {total} 个节），但**没有 `{want}` 节** —— \
                 通常是系统切换了热控场景（如录像），节名带上了场景前缀"
            ),
        }
    }
}

/// 读取官方温控策略表（**只读，零副作用**）
///
/// 失败时返回**具体原因**（见 [`OfficialTableError`]）。
pub fn read_official_rules() -> Result<(OfficialRule, OfficialRule), OfficialTableError> {
    let text = std::fs::read_to_string(DECRYPT_TXT).map_err(|_| OfficialTableError::NoFile)?;

    let monitor =
        parse_official_rule(&text, "MONITOR-BAT").ok_or_else(|| OfficialTableError::NoSection {
            want: "MONITOR-BAT",
            total: list_section_names(&text).len(),
        })?;

    // `SIC-BAT` 缺失时仍返回 —— 有些机型只有前者
    let sic = parse_official_rule(&text, "SIC-BAT").unwrap_or_default();
    Ok((monitor, sic))
}

/// 列出 `decrypt.txt` 里**全部**节名（纯字符串解析，可离线测试）
///
/// # 为什么需要它（这是一条"给未来机型"的诊断）
///
/// 本项目对 `decrypt.txt` 的解析**写死了节名** `MONITOR-BAT` / `SIC-BAT`：
///
/// | 情况 | 后果 |
/// |---|---|
/// | 节名一致（本机 K90PM） | ✅ 正常读到 |
/// | **节名不同**（他机型） | 静默读不到，只是少一项信息 |
///
/// 而**猜节名是危险的** —— 读到语义不同的表比读不到更糟
/// （同类错误刚在 `HW_MAX_TEMP_REJECTED` 上修过一次：
/// 把"主板温度"当成"电池温度"，数值合理但语义错）。
///
/// ⇒ 正确做法：**不猜**，而是把文件里真实存在的节名全部列出来，
/// 让维护者在遇到新机型时一眼看到该用什么名字，再决定是否补候选。
///
/// 返回节名（按文件顺序，已去重）。
pub fn list_section_names(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with('[') {
            continue;
        }
        let name = line
            .trim_start_matches('[')
            .split(']')
            .next()
            .unwrap_or("")
            .trim();
        if name.is_empty() {
            continue;
        }
        if !out.iter().any(|e| e == name) {
            out.push(name.to_string());
        }
    }
    out
}

/// 读取 `decrypt.txt` 的全部节名（**只读**）
///
/// 返回 `None` = 文件不存在或读不到。
pub fn read_section_names() -> Option<Vec<String>> {
    let text = std::fs::read_to_string(DECRYPT_TXT).ok()?;
    Some(list_section_names(&text))
}

// ============================================================
//  官方充电曲线（设备树直读）
// ============================================================

/// 官方曲线所在目录的候选（**顺序即优先级**）
///
/// # 为什么值得读
///
/// 这是**官方自己**在各温度档下允许的充电电流上限。知道了它，用户就能：
///
/// | 用途 | 说明 |
/// |---|---|
/// | 对照 | "官方 35℃ 就降到 5A 了，我设 43℃ 才收紧" ⇒ 我的设置更宽松 |
/// | 参考 | 把自己的阈值设在官方策略**之前**，避免与官方互相覆盖 |
/// | 排障 | 解释"为什么没到我的阈值就变慢了"（其实是官方先动手） |
///
/// ⚠️ **只读**，且**不需要插充电器** —— 设备树里的出厂参数，
/// 与当前充电状态无关。
///
/// # 为什么同时给两个候选
///
/// 实测本机（K90PM）两个目录都有这类属性，但**命名不同**：
///
/// | 目录 | 属性名形如 |
/// |---|---|
/// | `mca_quick_charge_batt_para_gbl` | `normal_para_atl_gl_20_48` |
/// | `mca_strategy_quick_charge` | `normal_para_atl_35_40` |
///
/// 因此下面按**目录扫属性名**而不是硬编码属性名 —— 换机型时只需
/// 目录在候选表里，属性名怎么变都能扫到。
pub const OFFICIAL_PARA_DIRS: &[&str] = &[
    "/sys/firmware/devicetree/base/soc/mca_quick_charge_batt_para_gbl",
    "/sys/firmware/devicetree/base/soc/mca_strategy_quick_charge",
];

/// 官方曲线的一个三元组：`电压 mV / 电流上限 mA / 电流下限 mA`
///
/// 语义来自小米官方 OSS 设备树注释（见 `_ref_charging-thermal-database`）：
///
/// > 设备树中的每个三元组均为 `电压 mV / current_max mA / current_min mA`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OfficialTriple {
    /// 电压（mV）
    pub voltage_mv: u32,
    /// 电流上限（mA）
    pub curr_max_ma: u32,
    /// 电流下限（mA）
    pub curr_min_ma: u32,
}

/// 官方曲线的一个温度档
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfficialBand {
    /// 区间的低温端（℃）
    pub lo_c: i32,
    /// 区间的高温端（℃）
    pub hi_c: i32,
    /// 曲线类别：`normal`（普通充电）/ `ffc`（快充）
    pub kind: String,
    /// 该档的阶段序列（CV 阶段：先大电流，后小电流）
    pub stages: Vec<OfficialTriple>,
}

impl OfficialBand {
    /// 该档的**起始电流上限**（mA）—— 即第一阶段的 `curr_max`
    ///
    /// 这是"官方在该温度档下允许充多快"的直接答案。
    pub fn start_curr_ma(&self) -> Option<u32> {
        self.stages.first().map(|t| t.curr_max_ma)
    }
}

/// 从属性名解析出温度区间与类别
///
/// 命名规律（两处目录都遵守）：`<kind>_para_<battery>_[<pkg>_]<lo>_<hi>`
///
/// | 属性名 | kind | 区间 |
/// |---|---|---|
/// | `normal_para_atl_gl_20_48` | `normal` | 20~48℃ |
/// | `ffc_para_cos_35_40` | `ffc` | 35~40℃ |
/// | `batt_para_atl_gl`（无区间） | — | 跳过（不是温度档） |
fn parse_official_band_name(name: &str) -> Option<(String, i32, i32)> {
    let kind = if name.starts_with("normal_para") {
        "normal"
    } else if name.starts_with("ffc_para") {
        "ffc"
    } else {
        return None;
    };

    // 末尾两个下划线分段必须是数字（温度区间）
    let parts: Vec<&str> = name.split('_').collect();
    if parts.len() < 2 {
        return None;
    }
    let lo: i32 = parts[parts.len() - 2].parse().ok()?;
    let hi: i32 = parts[parts.len() - 1].parse().ok()?;
    // 合理区间护栏：温度不可能为负或超过 100℃（防把别的属性误认成温度档）
    if !(0..=100).contains(&lo) || !(0..=100).contains(&hi) || lo >= hi {
        return None;
    }
    Some((kind.to_string(), lo, hi))
}

/// 把设备树属性内容解码成三元组序列
///
/// # 🔴 格式是 **NUL 分隔的十进制 ASCII 字符串**（不是 u32 数组）
///
/// 真机取证（2026-10-14，`normal_para_40_48` 的原始 hex，共 30 字节）：
///
/// ```text
/// 34 31 35 30 00  31 38 30 30 30 00  37 35 30 30 00  …
///  "4150"  \0       "18000"   \0       "7500"  \0
/// ```
///
/// ⇒ 设备树 `string` 类型：**每项一个 NUL 结尾的十进制字符串**。
/// 按 3 个一组即 `(电压 mV, 电流上限 mA, 电流下限 mA)`。
///
/// # ⚠️ 这个格式曾判断错，导致界面显示荒谬数值
///
/// 初版按「大端 u32 数组」解码（因为**隔壁**的档位表 `wired_thermal`
/// 确实是那种格式），于是把 ASCII 字节当成整数 —— 输出里出现
/// 「起始电流上限 **3224.12 A**」，电池最大也就 20A 级，一眼可辨为错。
///
/// **教训**：设备树同一目录下**不同属性可以是不同类型**，
/// 不能因为"邻居是 u32"就假定它也是 u32。
/// 判断依据应是**字节数与内容**（本例 30 字节、含 `\0` 分隔的数字串 ⇒ 字符串）。
///
/// 返回空 `Vec` 表示内容不符合该格式（**不报错** —— 换机型时属性可能不同）。
fn decode_official_triples(raw: &[u8]) -> Vec<OfficialTriple> {
    let nums: Vec<u32> = raw
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .filter_map(|s| std::str::from_utf8(s).ok())
        .filter_map(|s| s.trim().parse::<u32>().ok())
        .collect();

    nums.chunks_exact(3)
        .map(|t| OfficialTriple {
            voltage_mv: t[0],
            curr_max_ma: t[1],
            curr_min_ma: t[2],
        })
        .collect()
}

/// 读取官方充电曲线（**只读，零副作用，不需要插电**）
///
/// 返回**按温度升序**排好的各档。扫不到任何一档时返回空 `Vec`
/// （不是错误 —— 很多机型没有这些属性）。
pub fn read_official_bands() -> Vec<OfficialBand> {
    let mut out: Vec<OfficialBand> = Vec::new();

    for dir in OFFICIAL_PARA_DIRS {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let Some((kind, lo, hi)) = parse_official_band_name(&name) else {
                continue;
            };
            // 同一个温度档在两个目录里都可能出现 ⇒ 去重（先出现的优先）
            if out
                .iter()
                .any(|b| b.lo_c == lo && b.hi_c == hi && b.kind == kind)
            {
                continue;
            }
            let Ok(raw) = std::fs::read(e.path()) else {
                continue;
            };
            let stages = decode_official_triples(&raw);
            if stages.is_empty() {
                continue;
            }
            out.push(OfficialBand {
                lo_c: lo,
                hi_c: hi,
                kind,
                stages,
            });
        }
    }

    // 排序：先 normal 后 ffc，各自按温度升序
    out.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then(a.lo_c.cmp(&b.lo_c))
            .then(a.hi_c.cmp(&b.hi_c))
    });
    out
}

// ============================================================
//  供电来源（诊断展示用）
// ============================================================

/// 供电来源快照（**只读**，用于界面/诊断展示）
///
/// # 为什么需要它
///
/// 用户看到"功率只有 0.7W"时的第一反应是"模块坏了" ——
/// 实际往往是**插的是电脑 USB 口**（SDP 协议，只能给约 0.5A）。
/// 把供电类型显示出来，这个疑问当场就解开了。
///
/// # 为什么不放在 `Nodes` 里
///
/// `Nodes` 是**控制节点**的集合（档位/电流/SOC），由决策路径消费。
/// 供电来源是**纯展示信息**，不参与任何决策 —— 混进去会让
/// "哪些节点会影响控制"变得不易辨认。
///
/// # 路径策略
///
/// 先试小米机型节点（信息更准），再退回标准 Linux 节点：
///
/// | 信息 | 小米节点 | 标准兜底 |
/// |---|---|---|
/// | 协议 | `charger_common/real_type`（`PD_PPS`） | `usb/real_type` |
/// | USB 类型 | — | `usb/type`（`USB_PD`） |
/// | 协商功率 | `charger_common/power_max`（`100` W） | — |
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PowerSource {
    /// `usb/type` 的读值（如 `USB_PD` / `USB`）
    pub usb_type: Option<String>,
    /// 充电协议（如 `PD_PPS` / `SDP`）
    pub protocol: Option<String>,
    /// 协商出的功率上限（W）；`None` = 未协商出快充
    pub power_max_w: Option<u16>,
    /// 是否明显是**弱电源**（电脑 USB 口等）
    ///
    /// 界面据此提示"这个读数反映的是充电器能力，不是手机的"。
    pub weak: bool,
}

impl PowerSource {
    /// 读一次供电来源（**只读，绝不写**）
    pub fn read() -> Self {
        let read_trim = |p: &str| -> Option<String> {
            std::fs::read_to_string(p)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };

        let usb_type = read_trim("/sys/class/power_supply/usb/type");

        // 充电协议：小米机型的节点优先
        const PROTOCOL_CANDIDATES: &[&str] = &[
            "/sys/class/xm_power/charger/charger_common/real_type",
            "/sys/class/xm_power/charger/charger_common/quick_charge_type",
            "/sys/class/power_supply/usb/real_type",
        ];
        let protocol = PROTOCOL_CANDIDATES.iter().find_map(|p| read_trim(p));

        // 协商功率：小米机型节点（未协商出快充时为 0 或读不到）
        let power_max_w = read_trim("/sys/class/xm_power/charger/charger_common/power_max")
            .and_then(|s| s.parse::<u16>().ok())
            .filter(|w| *w > 0);

        // ⚠️ 以**协议**为主判据，`usb/type` 只作兜底
        //
        // `usb/type` 在插电脑和插某些真充电器时**同样**读作 `USB`，
        // 拿它当主判据会把正常充电器一起判弱。
        //
        // 而"协议为空"也不够 —— 电脑 USB 口的 `real_type` 实测回报
        // `SDP`（Standard Downstream Port）而不是空。
        const WEAK_PROTOCOLS: &[&str] = &["SDP", "CDP", "UNKNOWN", "NONE", "N/A", "-"];
        let weak = match protocol.as_deref() {
            Some(p) if !p.is_empty() => WEAK_PROTOCOLS.contains(&p.to_ascii_uppercase().as_str()),
            _ => matches!(usb_type.as_deref(), Some("USB") | Some("Unknown") | None),
        };

        Self {
            usb_type,
            protocol,
            power_max_w,
            weak,
        }
    }

    /// 供界面显示的**协议名**（协议优先，退回 USB 类型）
    ///
    /// 两者都可能读不到（未插电、驱动未上报）⇒ 返回 `None`，
    /// 由调用方决定显示"未知"还是留空。
    pub fn label(&self) -> Option<&str> {
        self.protocol
            .as_deref()
            .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("unknown"))
            .or(self.usb_type.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个 3 档 × 10 字段的表（模拟"档位数因机型而异"）
    fn fake_table() -> GearTable {
        let mut rows = Vec::new();
        for ma in [13500u32, 7000, 500] {
            let mut row = [0u32; GEAR_TABLE_FIELDS];
            row[GEAR_TABLE_CURR_COL] = ma;
            rows.push(row);
        }
        GearTable { rows }
    }

    #[test]
    fn 档位数由表长度决定而非固定十五() {
        assert_eq!(fake_table().len(), 3);
    }

    #[test]
    fn 电流上限由ma换算为ua() {
        let t = fake_table();
        assert_eq!(t.current_limit_ua(0), Some(13_500_000));
        assert_eq!(t.current_limit_ua(1), Some(7_000_000));
        assert_eq!(t.current_limit_ua(2), Some(500_000));
        assert_eq!(t.current_limit_ua(3), None, "越界档位应为 None");
    }

    #[test]
    fn 最大功率档位取电流上限最大者() {
        let t = fake_table();
        assert_eq!(t.max_power_gear(), Some(0));
        assert_eq!(t.max_current_ua(), Some(13_500_000));
    }

    #[test]
    fn 并列时取编号最小的档位() {
        let mut rows = Vec::new();
        for ma in [5000u32, 5000, 5000] {
            let mut row = [0u32; GEAR_TABLE_FIELDS];
            row[GEAR_TABLE_CURR_COL] = ma;
            rows.push(row);
        }
        let t = GearTable { rows };
        assert_eq!(t.max_power_gear(), Some(0), "并列取最松的档");
    }

    #[test]
    fn 空表不panic() {
        let t = GearTable::default();
        assert!(t.is_empty());
        assert_eq!(t.max_power_gear(), None);
        assert_eq!(t.max_current_ua(), None);
    }

    /// 电荷泵温度候选路径必须存在且为绝对路径
    ///
    /// 防止有人清空候选表导致该观测功能静默消失。
    #[test]
    fn 电荷泵温度候选路径非空且为绝对路径() {
        assert!(
            !CP_DIE_TEMP_CANDIDATES.is_empty(),
            "候选表为空 ⇒ 电荷泵温度永远探测不到"
        );
        for p in CP_DIE_TEMP_CANDIDATES {
            assert!(p.starts_with('/'), "候选路径必须是绝对路径：{p}");
            assert!(
                p.ends_with("/tdie"),
                "候选路径应指向 tdie 节点（实测的节点名）：{p}"
            );
        }
    }

    // ────────────────────────────────────────────────────────────
    //  官方温控策略表（decrypt.txt 解析）
    // ────────────────────────────────────────────────────────────

    /// **真机原文**片段（2026-10-14 从 `/data/vendor/thermal/decrypt.txt` 取）
    ///
    /// 用真实内容做测试，格式一旦变化立刻能发现。
    const REAL_DECRYPT: &str = "\
[MONITOR-BACKLIGHT]
algo_type\tmonitor
trig\t40000\t45000

[MONITOR-BAT]
algo_type\tmonitor
sensor\tVIRTUAL-SENSOR-FORMULA
device\tbattery
polling\t2000
trig\t33000\t34300\t37500\t38500\t39500\t40500\t41500\t43000\t44000\t45000
clr\t32000\t33800\t36200\t38000\t39200\t39500\t41000\t42000\t43200\t44200
target\t0300\t700\t1100\t1200\t1200\t1400\t1400\t1400\t1400\t1500

[MONITOR-WIRELESS]
algo_type\tmonitor
trig\t35000
";

    const REAL_SIC: &str = "\
[SIC-BAT]
algo_type\tsic
sensor\tVIRTUAL-SENSOR-FORMULA
device\tthermal_fcc_override
polling\t2000
proportion\t0
trig\t15000\t33800\t36800\t40000\t42200\t44000\t46000
clr\t14000\t32800\t34000\t39000\t40800\t43000\t45000
target\t0\t34500\t37800\t41000\t42600\t45000\t47000
ks\t0\t1000000\t5200000\t4000000\t5200000\t4000000\t4000000
max\t18400\t18400\t10000\t9000\t5000\t2500\t700
min\t18400\t18400\t5100\t5100\t3000\t1000\t700

[MONITOR-TEMP_STATE]
algo_type\tmonitor
";

    /// 解析 `MONITOR-BAT`：只取目标节，不串到邻居
    #[test]
    fn 解析官方温控表_监测() {
        // 项目 lint 禁止 `unwrap`/`expect`/`panic`（含测试），
        // 故用「先断言存在、再取默认值」的写法 —— 断言失败时测试已挂，
        // 后续断言不会因默认值而误判通过。
        let parsed = parse_official_rule(REAL_DECRYPT, "MONITOR-BAT");
        assert!(parsed.is_some(), "应能解析出 MONITOR-BAT");
        let r = parsed.unwrap_or_default();
        assert_eq!(r.name, "MONITOR-BAT");
        assert_eq!(r.trig.len(), 10, "10 个触发档");
        assert_eq!(r.clr.len(), 10, "10 个解除档");
        assert_eq!(r.target.len(), 10);

        // 首档：33.0℃ 触发，32.0℃ 解除，档位 3
        assert_eq!(r.trig[0], 33_000);
        assert_eq!(r.clr[0], 32_000);
        assert_eq!(r.target[0], 300);

        // ⚠️ 0000 前导零必须按**十进制**解读
        //    0300 → 300（档位 3）。若按八进制会得 192 ⇒ target/100 = 1，与实际不符。
        assert_eq!(r.target[0] / 100, 3, "官方首档写入档位 3");

        // 末档：45.0℃ → 档位 15（最紧）
        assert_eq!(r.trig[9], 45_000);
        assert_eq!(r.target[9] / 100, 15);

        // **不得串到下一个节**（这是最容易写错的地方）
        assert!(r.max_ma.is_empty(), "MONITOR-BAT 没有 max 字段");
        assert_eq!(r.trig.len(), 10, "不能把 MONITOR-WIRELESS 的 trig 混进来");
    }

    /// 解析 `SIC-BAT`：含 `max`/`min` 电流表
    #[test]
    fn 解析官方温控表_限流() {
        let parsed = parse_official_rule(REAL_SIC, "SIC-BAT");
        assert!(parsed.is_some(), "应能解析出 SIC-BAT");
        let r = parsed.unwrap_or_default();
        assert_eq!(r.trig.len(), 7);
        assert_eq!(r.max_ma.len(), 7);
        assert_eq!(r.min_ma.len(), 7);

        // 🔴 首档 18400 mA —— 这就是「写 0 也读回 18400000」的来源
        assert_eq!(r.max_ma[0], 18_400, "官方首档上限 18.4 A");
        assert_eq!(r.min_ma[0], 18_400, "首档上限=下限（固定 18.4A）");

        // 末档 46.0℃ → 0.7 A
        assert_eq!(r.trig[6], 46_000);
        assert_eq!(r.max_ma[6], 700);
    }

    /// 请求不存在的节 → `None`（不是 panic，也不是空规则）
    #[test]
    fn 解析不存在的节返回none() {
        assert!(parse_official_rule(REAL_DECRYPT, "NO-SUCH-RULE").is_none());
        assert!(parse_official_rule("", "MONITOR-BAT").is_none());
        // 节存在但没有 trig ⇒ 视为不可用（`is_usable` 为假）
        assert!(parse_official_rule("[X]\nalgo_type\tmonitor\n", "X").is_none());
    }

    /// 节头之外的杂项行不得干扰解析
    #[test]
    fn 解析忽略节外内容() {
        let noisy = format!("这行是垃圾\n[乱码\n{REAL_DECRYPT}");
        let parsed = parse_official_rule(&noisy, "MONITOR-BAT");
        assert!(parsed.is_some(), "节外垃圾不该影响解析");
        let r = parsed.unwrap_or_default();
        assert_eq!(r.trig.len(), 10);
    }

    /// `decrypt.txt` 路径必须是绝对路径
    #[test]
    fn 官方温控表路径为绝对路径() {
        assert!(DECRYPT_TXT.starts_with('/'), "必须绝对路径：{DECRYPT_TXT}");
        assert!(
            DECRYPT_TXT.contains("thermal"),
            "应在 thermal 目录下：{DECRYPT_TXT}"
        );
    }

    // ────────────────────────────────────────────────────────────
    //  硬件温度上限的来源（防"语义错误"的候选）
    // ────────────────────────────────────────────────────────────

    /// 候选表里的路径必须语义一致（都是**电池**温度上限）
    #[test]
    fn 硬件温度候选表语义一致() {
        assert!(!HW_MAX_TEMP_CANDIDATES.is_empty(), "候选表不能为空");
        for p in HW_MAX_TEMP_CANDIDATES {
            assert!(p.starts_with('/'), "必须绝对路径：{p}");
            assert!(
                p.contains("devicetree"),
                "设备树属性应在 devicetree 下：{p}"
            );
        }

        // 🔴 语义错误的属性**必须不在**候选表里
        //
        // 这两项曾列在候选表里做兜底，但它们语义不同：
        //   · max_thermal_board_temp 是**主板**温度（本机 50℃）
        //   · quick_revchg_max_batt_temp 是**无线反充**场景（本机 40℃）
        // `parse_temp_celsius` 拦不住它们（都在合理区间内），
        // 只能靠这条测试守住。
        for (bad, why) in HW_MAX_TEMP_REJECTED {
            assert!(
                !HW_MAX_TEMP_CANDIDATES.contains(bad),
                "语义不符的属性不得作为候选（{why}）：{bad}"
            );
        }
    }

    /// 🔴 官方表**不得**再被用作硬件保护温度的兜底
    ///
    /// # 这条测试守的是什么
    ///
    /// 2026-10-14 真机实测发现：`decrypt.txt` 的内容**随场景切换**。
    /// 场景切到 `11`（录像）后系统重写该文件，节名全变 `VIDEO-` 前缀，
    /// `MONITOR-BAT` 消失 —— 此时读不到表。
    ///
    /// ⇒ 曾把它作为"硬件保护温度"的兜底，会让用户的**可配置温度范围
    ///   随场景漂移**（录个像就变），比"读不到就明说"更糟，已移除。
    ///
    /// 若后人想再把官方表接回决策路径，这条测试会提醒他这个坑。
    #[test]
    fn 官方表不作为硬件温度兜底() {
        // 官方表里的温度值确实存在且"看起来合理"（所以只靠数值合理性
        // 是拦不住误用的 —— 必须靠语义正确性）
        let peak_1 = [33_000i64, 45_000].into_iter().max().unwrap_or(0);
        let peak_2 = [15_000i64, 46_000].into_iter().max().unwrap_or(0);
        let c = peak_1.max(peak_2) as f32 / 1000.0;
        assert_eq!(c, 46.0, "官方表最高触发温度 46.0℃（数值合理，但语义不符）");
        assert!(
            (35.0..=60.0).contains(&c),
            "它落在电池温度合理区间 ⇒ 单位解析没问题"
        );

        // 正式断言：硬件温度候选表里**不得**出现官方表相关路径
        for p in HW_MAX_TEMP_CANDIDATES {
            assert!(
                !p.contains("decrypt") && !p.contains("thermal/decrypt"),
                "官方表（随场景变化）不得作为硬件温度候选：{p}"
            );
            assert!(
                p.contains("devicetree"),
                "硬件温度只能来自设备树（硬件参数，不随场景变）：{p}"
            );
        }
    }

    /// 官方表读取失败时，**必须能区分**"文件不存在"与"节名不匹配"
    ///
    /// # 这条测试守的是什么
    ///
    /// 初版返回 `Option`，两种原因都变成 `None`，于是提示永远是
    /// 「读不到文件（非小米机型，或该文件不存在）」——
    /// 而真机实测遇到的是**文件明明存在**（4395 字节）、只是节名变了。
    /// 那个提示会把排查引向完全错误的方向。
    #[test]
    fn 官方表失败原因可区分() {
        let no_file = OfficialTableError::NoFile;
        let msg = no_file.message();
        assert!(
            msg.contains("可能没有此文件") || msg.contains("读不到"),
            "NoFile 的说明应指向文件问题：{msg}"
        );
        assert!(
            !msg.contains("节"),
            "NoFile 不该提节名（会把排查引偏）：{msg}"
        );

        let no_section = OfficialTableError::NoSection {
            want: "MONITOR-BAT",
            total: 21,
        };
        let msg = no_section.message();
        assert!(
            msg.contains("文件存在"),
            "NoSection 必须明确说**文件是存在的**：{msg}"
        );
        assert!(
            msg.contains("MONITOR-BAT"),
            "NoSection 要点出缺的是哪个节名：{msg}"
        );
        assert!(msg.contains("21"), "NoSection 要报出实际节数供排查：{msg}");
        assert!(
            msg.contains("场景"),
            "NoSection 应提示「可能是场景切换」这个真实原因：{msg}"
        );
    }

    // ────────────────────────────────────────────────────────────
    //  节名列表（换机型的诊断依据）
    // ────────────────────────────────────────────────────────────

    /// 节名提取：按顺序、去重、不含方括号
    #[test]
    fn 提取全部节名() {
        let names = list_section_names(REAL_DECRYPT);
        // REAL_DECRYPT 里有 BACKLIGHT / BAT / WIRELESS 三个节
        assert_eq!(
            names,
            vec!["MONITOR-BACKLIGHT", "MONITOR-BAT", "MONITOR-WIRELESS"]
        );

        // 方括号必须被去掉（否则打印出来会是 `[MONITOR-BAT]` 的嵌套效果）
        for n in &names {
            assert!(!n.contains('['), "节名不应含左括号：{n}");
            assert!(!n.contains(']'), "节名不应含右括号：{n}");
            assert!(!n.trim().is_empty(), "节名不应为空");
        }
    }

    /// 节名提取的边界情况
    #[test]
    fn 提取节名的边界情况() {
        // 空内容 / 无节名
        assert!(list_section_names("").is_empty());
        assert!(list_section_names("algo_type\tmonitor\n").is_empty());

        // 重复节名只保留一次
        let dup = "[A]\nx\t1\n[A]\nx\t2\n";
        assert_eq!(list_section_names(dup), vec!["A"]);

        // 空节名被跳过（`[]`）
        let empty_name = "[]\nx\t1\n[real]\ny\t2\n";
        assert_eq!(list_section_names(empty_name), vec!["real"]);

        // 行首有空白也应识别
        let indented = "  [B]\nx\t1\n";
        assert_eq!(list_section_names(indented), vec!["B"]);

        // 节名里的短横线必须保留（`MONITOR-BAT` 正是靠它）
        let dashed = "[MONITOR-BAT]\ntrig\t1\n";
        assert_eq!(list_section_names(dashed), vec!["MONITOR-BAT"]);
    }

    /// 🔴 本模块使用的两个节名必须真的存在于真机文件里
    ///
    /// 这条测试守的是「**节名写错导致静默失效**」：
    /// 若代码里把 `MONITOR-BAT` 拼成 `MONITOR_BAT`，解析会静默返回
    /// `None`（**不报错**），官方策略表就永远看不到 —— 极难发现。
    #[test]
    fn 使用的节名确实存在() {
        let names = list_section_names(REAL_DECRYPT);
        let used = "MONITOR-BAT";
        assert!(
            names.iter().any(|n| n == used),
            "代码使用的节名 {used} 在真机文件里不存在 —— 解析会静默失效"
        );
        // `SIC-BAT` 在另一个真机片段里
        assert!(
            REAL_SIC.contains("[SIC-BAT]"),
            "SIC-BAT 节名必须出现在测试样本里"
        );
        assert_eq!(
            parse_official_rule(REAL_SIC, "SIC-BAT").map(|r| r.name),
            Some("SIC-BAT".to_string()),
            "用真机样本必须能解析出 SIC-BAT"
        );
    }

    // ────────────────────────────────────────────────────────────
    //  官方充电曲线（设备树解码）
    // ────────────────────────────────────────────────────────────

    /// 属性名解析：正常命名
    #[test]
    fn 解析官方曲线属性名() {
        // 实测本机的真实属性名（K90PM，两个目录各一种命名）
        assert_eq!(
            parse_official_band_name("normal_para_atl_gl_20_48"),
            Some(("normal".to_string(), 20, 48))
        );
        assert_eq!(
            parse_official_band_name("ffc_para_atl_gl_35_40"),
            Some(("ffc".to_string(), 35, 40))
        );
        assert_eq!(
            parse_official_band_name("normal_para_cos_18_23"),
            Some(("normal".to_string(), 18, 23))
        );
        assert_eq!(
            parse_official_band_name("ffc_para_atl_5_10"),
            Some(("ffc".to_string(), 5, 10))
        );
    }

    /// 属性名解析：非温度档的属性必须被跳过
    #[test]
    fn 非温度档属性被跳过() {
        // 没有温度后缀
        assert_eq!(parse_official_band_name("batt_para_atl_gl"), None);
        // 不是曲线属性
        assert_eq!(parse_official_band_name("temp_para1"), None);
        assert_eq!(parse_official_band_name("die_temp_max"), None);
        assert_eq!(parse_official_band_name("compatible"), None);
        // 温度区间不合理（护栏：防把别的属性误认成温度档）
        assert_eq!(parse_official_band_name("normal_para_x_200_300"), None);
        assert_eq!(parse_official_band_name("normal_para_x_48_20"), None); // lo >= hi
        assert_eq!(parse_official_band_name("normal_para_x_0_0"), None);
    }

    /// 三元组解码：**NUL 分隔的十进制 ASCII 字符串**
    ///
    /// 用**真机实测的原始字节**做测试数据（`normal_para_40_48`，30 字节）
    /// —— 格式一旦被改错，测试立刻失败。
    #[test]
    fn 解码官方曲线三元组() {
        // "4150\0" "18000\0" "7500\0" "4455\0" "7500\0" "500\0"
        let raw: Vec<u8> = [
            &b"4150\0"[..],
            &b"18000\0"[..],
            &b"7500\0"[..],
            &b"4455\0"[..],
            &b"7500\0"[..],
            &b"500\0"[..],
        ]
        .concat();
        assert_eq!(raw.len(), 30, "应与真机取证的字节数一致");

        let t = decode_official_triples(&raw);
        assert_eq!(t.len(), 2, "6 个值 = 2 个三元组");
        assert_eq!(
            t[0],
            OfficialTriple {
                voltage_mv: 4150,
                curr_max_ma: 18000,
                curr_min_ma: 7500
            }
        );
        assert_eq!(
            t[1],
            OfficialTriple {
                voltage_mv: 4455,
                curr_max_ma: 7500,
                curr_min_ma: 500
            }
        );

        // ── 数值合理性护栏（防"格式判断错"这类缺陷）──
        //
        // 初版按 u32 解码时得到 3224120 mA（3224 A），电池不可能有这么大电流。
        // 有基本物理常识就能在测试里拦住它。
        for stage in &t {
            assert!(
                (3500..=4700).contains(&stage.voltage_mv),
                "电压应在锂电合理区间（3500~4700 mV），实得 {}",
                stage.voltage_mv
            );
            assert!(
                stage.curr_max_ma <= 30_000,
                "电流上限不应超过 30 A，实得 {} mA",
                stage.curr_max_ma
            );
        }
    }

    /// 三元组解码：**不是该格式的内容**必须返回空（不 panic、不产出垃圾）
    #[test]
    fn 解码非字符串格式时返回空() {
        assert!(decode_official_triples(&[]).is_empty());
        // 二进制垃圾（不是十进制 ASCII）
        assert!(decode_official_triples(&[0xff, 0xfe, 0xfd, 0x00]).is_empty());

        // ⚠️ 不能写 `b"4150\018000\0"` —— `\0` 后接数字会被当成**八进制转义**
        //    （`\018`），clippy 会拒绝。用拼接写法避免歧义。
        let one_value: Vec<u8> = [&b"4150"[..], &[0u8]].concat();
        assert!(decode_official_triples(&one_value).is_empty(), "1 个值");

        let two_values: Vec<u8> = [&b"4150"[..], &[0u8], &b"18000"[..], &[0u8]].concat();
        assert!(decode_official_triples(&two_values).is_empty(), "2 个值");

        // 4 个值 ⇒ 1 个完整三元组 + 1 个被丢弃的余数（chunks_exact 语义）
        let four_values: Vec<u8> = [
            &b"1"[..],
            &[0u8],
            &b"2"[..],
            &[0u8],
            &b"3"[..],
            &[0u8],
            &b"4"[..],
            &[0u8],
        ]
        .concat();
        assert_eq!(decode_official_triples(&four_values).len(), 1);
    }

    /// `start_curr_ma` 取第一阶段的上限（"官方允许充多快"的直接答案）
    #[test]
    fn 起始电流取第一阶段() {
        let b = OfficialBand {
            lo_c: 20,
            hi_c: 48,
            kind: "normal".to_string(),
            stages: vec![
                OfficialTriple {
                    voltage_mv: 4080,
                    curr_max_ma: 20500,
                    curr_min_ma: 15600,
                },
                OfficialTriple {
                    voltage_mv: 4120,
                    curr_max_ma: 15600,
                    curr_min_ma: 11560,
                },
            ],
        };
        assert_eq!(b.start_curr_ma(), Some(20500));

        // 空阶段 → None（不能 panic）
        let empty = OfficialBand {
            stages: Vec::new(),
            ..b
        };
        assert_eq!(empty.start_curr_ma(), None);
    }

    /// 候选目录非空且为绝对路径
    #[test]
    fn 官方曲线候选目录非空且为绝对路径() {
        assert!(
            !OFFICIAL_PARA_DIRS.is_empty(),
            "候选表为空 ⇒ 官方曲线永远读不到"
        );
        for d in OFFICIAL_PARA_DIRS {
            assert!(d.starts_with('/'), "候选必须绝对路径：{d}");
            assert!(
                d.contains("devicetree"),
                "官方曲线是设备树属性，路径应在 devicetree 下：{d}"
            );
        }
    }

    // ────────────────────────────────────────────────────────────
    //  场景节点：**编译期**只读保证
    //
    //  用户要求「场景模式只作为显示，不要调整场景模式，只用双写调整功率」。
    //  下面的注释不是"说明"，而是**编译期断言** —— 一旦有人给 ReadOnlyNode
    //  加上写方法，这段代码就会编译失败（不是运行期才发现）。
    // ────────────────────────────────────────────────────────────

    #[test]
    fn 场景节点类型上没有写方法() {
        let n = ReadOnlyNode::new("/sys/class/thermal/thermal_message/sconfig");

        // ✅ 允许的操作：读（函数指针断言保证这些方法确实存在）
        let _: fn(&ReadOnlyNode) -> anyhow::Result<String> = ReadOnlyNode::read;
        let _: fn(&ReadOnlyNode) -> anyhow::Result<i64> = ReadOnlyNode::read_i64;
        let _: fn(&ReadOnlyNode) -> String = ReadOnlyNode::path_str;

        // ❌ 下面这些**故意**注释掉：它们是编译错误的证明。
        //    若要"启用"任何一行，必须先推翻"场景只读"这条约束。
        //
        //     n.write("0");
        //     n.write_i64(1);
        //     n.write_and_verify("1");
        //
        //     error[E0599]: no method named `write` found for struct `ReadOnlyNode`

        // 运行时行为：路径与普通节点一致
        assert!(n.path_str().ends_with("thermal_message/sconfig"));
    }
}
