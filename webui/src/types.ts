/**
 * 后端数据结构
 *
 * 全部来自 `bin/ctl` 的 `data` 字段（见 docs/05-契约-接口输出规范.md）。
 * 字段名与 Rust 侧 `serde` 输出**逐字对应**，不在这里改名 ——
 * 改名会让"契约"与"实现"两套说法分叉。
 */

/** 契约响应（`ok` 必须显式判断：Response 是 untagged 联合） */
export type ApiResponse =
  | { ok: true; data?: unknown }
  | { ok: false; code: string; msg: string; field?: string };

// ============================================================
//  状态快照（`ctl status`）
// ============================================================

/** 电池读数 */
export interface Battery {
  /**
   * **实际**充电电流（A，取绝对值）。
   *
   * ⚠️ 与 [`Control.curr_now`]（模块**写入**的值）是两回事 ——
   * 实测两者经常对不上：
   *
   * | 模块写入 | 实际 | 原因 |
   * |---|---|---|
   * | 8 A | 9.06 A | 系统响应滞后（前几拍的值还在生效） |
   * | 8 A | 6.51 A | 同上，方向相反 |
   *
   * 展示**实际值**才反映"真正在充多快"。
   * 未检测到电流节点时为 `null`。
   */
  current_a: number | null;
  /** 瞬时功率（W） */
  power_w: number;
  /** 电量（%） */
  soc: number;
  /** `Charging` / `Discharging` / `Full` / `Not charging` */
  status: string;
  /** 电池温度（℃） */
  temp_c: number;
}

/**
 * 供电来源（**纯展示，不参与任何决策**）
 *
 * # 为什么要有它
 *
 * 用户看到"功率只有 0.7W"时的第一反应是"模块坏了" ——
 * 实际往往是**插的是电脑 USB 口**（`SDP` 协议，只能给约 0.5A）。
 * 把供电类型显示出来，这个疑问当场就解开。
 */
export interface Charger {
  /** 显示用的名字：协议优先，USB 类型兜底；都读不到为 `null` */
  label: string | null;
  /** 协商出的功率上限（W）；未协商出快充时为 `null` */
  power_max_w: number | null;
  /** 充电协议（如 `PD_PPS` / `SDP`） */
  protocol: string | null;
  /** `usb/type` 的读值（如 `USB_PD`） */
  usb_type: string | null;
  /**
   * 是否明显是**弱电源**（电脑 USB 口等）
   *
   * `true` 时界面应提示：当前功率反映的是**充电器的能力**，不是手机的。
   */
  weak: boolean;
}

/** 当前控制输出 */
export interface Control {
  /** 已写入的电流（μA） */
  curr_now: number;
  /** 已写入的档位 */
  gear_now: number;
  /** 目标功率上限（W） */
  power_limit_w: number;
}

/** 温控参数摘要 */
export interface Thermal {
  /** 绝对上限（℃） */
  abs_max: number;
  /** 温度基准：`cell`（电芯）/ `virt`（机身） */
  basis: string;
  /**
   * 电荷泵芯片温度（℃）。
   *
   * ⚠️ 这是**快充时的真正热点**，实测比电池温度高约 10℃
   * （插电快充中：电池 32.7℃ / 芯片 42.5℃）。
   *
   * 机型无此节点或驱动未上报时为 `null`。
   */
  cp_die_temp_c: number | null;
  /** 本次收紧期的峰值温度（℃），未收紧时为 null */
  peak_c: number | null;
  /** 收紧阈值（℃） */
  tighten_temp: number;
}

/** `ctl status` 的 data */
export interface StatusSnapshot {
  battery: Battery;
  /** 供电来源（诊断展示，见 [`Charger`]） */
  charger: Charger;
  /**
   * 系统场景（**只读参考，模块不使用它做任何决策**）
   *
   * `null` = 本机无此节点。值为 `-1` 时后端已归一化为 `"未设置"`
   * （真机上这是最常见的值）。
   *
   * ⚠️ 显示时**照实呈现原始编号**，不翻译成中文 ——
   * 场景编号的含义没有公开文档，猜错比不显示更糟。
   */
  scene: string | null;
  control: Control;
  /** `running` / 其它 */
  daemon: string;
  /**
   * 是否已跑过检测。
   *
   * ⚠️ `false` 时模块**零写入**（安全闸门「不猜硬件」）——
   * 界面必须给出明确提示，不能只显示「待机」（那看起来是正常的）。
   *
   * 正常情况下 supervisor 首次启动会自动检测
   * （见 `supervisor::ensure_detected`），因此 `false` 表示自动检测也失败了。
   */
  detected: boolean;
  /** 是否已交回官方控制 */
  released: boolean;
  session: { duration_s: number };
  /** `idle` / `normal` / `tightening` / `hardprotect` */
  state: string;
  thermal: Thermal;
  /** 累计拍数 */
  ticks: number;
  /** 快照时间戳 */
  ts: number;
  /** 运行时长（秒） */
  uptime_s: number;
}

// ============================================================
//  附加功能（`limit show` / `addon list`）
// ============================================================

/** 充电上限功能的细节 */
export interface SocLimitDetail {
  keep_on_restore: boolean;
  last_written: string | null;
  /** 控制节点路径 */
  node_ctrl: string;
  /** 生效值节点路径 */
  node_read: string;
  /** 驱动报告的**实际生效**上限；未启用时为 null */
  soc_active: number | null;
  /** 用户设定的目标上限 */
  soc_target: number;
  verify_s: number;
  warn_on_hypersc: boolean;
  /** 累计写入次数 */
  write_count: number;
}

/** 附加功能状态 */
export interface AddonStatus {
  addon_api_version: number;
  consecutive_failures: number;
  detail: SocLimitDetail;
  enabled: boolean;
  errors: number;
  id: string;
  interval_s: number;
  label: string;
  last_error: string | null;
  origin: string;
  /** 探测结果描述 */
  probe: string;
  summary: string;
  supported: boolean;
  ticks: number;
  /** 是否已熔断（连续失败被自动停用） */
  tripped: boolean;
  writes: number;
}

// ============================================================
//  配置（`get --section …`）
// ============================================================

/** `get --section thermal` */
export interface ThermalSection {
  abs_max: number;
  /** `curve` / `switch` / `ramp` */
  control_law: string;
  delay_s: number;
  ramp_span: number;
  relax_temp: number;
  /** `cell` / `virt` */
  sensor: string;
  /**
   * 收紧时降到多少（**W**）
   *
   * ⚠️ 是功率不是电流（2026-10-14 改）。用户反馈「控制电流不够直白」——
   * 瓦是充电器/快充协议的通用标注单位，而安培要知道电压才能换算成"多快"。
   *
   * `0` = **停止充电**（注意与 `ControlSection.target_power_w` 的 `0`
   * 语义相反，那个是"不限制"）。
   */
  tighten_power_w: number;
  tighten_temp: number;
  virt_guard: boolean;
  virt_hard: number;
}

/** `get --section control` */
export interface ControlSection {
  /** `dual` / 其它 */
  actuator: string;
  closed_loop: boolean;
  feedforward: number;
  hardware_protect: boolean;
  patrol_s: number;
  shift_s: number;
  stabilize_s: number;
  target_power_w: number;
}

/** 曲线的一段 */
export interface CurvePoint {
  /** 该段允许的最大电流（μA） */
  curr: number;
  /** 该段的温度上界（℃） */
  t_max: number;
}

// ============================================================
//  设备档案（`device show`）
// ============================================================

/** 设备身份 */
export interface Identity {
  android_api: number;
  android_release: string;
  device: string;
  marketname: string;
  model: string;
  page_size: number;
  platform: string;
}

/** 电池信息 */
export interface BatteryInfo {
  cycle_count: number;
  health_percent: number;
  model_name: string;
  technology: string;
}

/**
 * 能力值的来源（可信度递减）
 *
 * 界面上必须展示 —— 用户要能分清"这个数是直读的，还是推断的"。
 */
export type ValueSource =
  | "node"
  | "battery_model"
  | "devicetree"
  | "profile"
  | "default"
  | "unknown";

/** 来源 → 中文标签 */
export const SOURCE_LABEL: Record<ValueSource, string> = {
  node: "协商/节点",
  battery_model: "电池型号",
  devicetree: "设备树",
  profile: "机型档案",
  default: "默认值",
  unknown: "未探到",
};

/**
 * 该来源是否属于"直读/实测"级别（用于**取值优先级**）
 *
 * ⚠️ 不要用它决定"要不要提醒用户" —— 那是
 * [`isLocalSource`]() 的职责（设备树算本机数据）。
 */
export function isTrustedSource(s: ValueSource | undefined): boolean {
  return s === "node" || s === "battery_model";
}

/**
 * 该来源是否来自**本机**（而非他人的档案或硬编码猜测）
 *
 * 用于决定要不要提醒用户"数据可能不准"：
 *
 * | 来源 | 本机？ | 说明 |
 * |---|---|---|
 * | `node` | ✅ | 本机节点直读 |
 * | `battery_model` | ✅ | 本机电池型号解析 |
 * | `devicetree` | ✅ | 本机设备树直读（档位表/硬件温度的唯一来源） |
 * | `profile` | ❌ | **可能来自他人实测**（档案是分发的） |
 * | `default` | ❌ | 硬编码猜测 |
 * | `unknown` | ❌ | 没探到 |
 */
export function isLocalSource(s: ValueSource | undefined): boolean {
  return s === "node" || s === "battery_model" || s === "devicetree";
}

/** 各能力值的来源集合 */
export interface Sources {
  gear_max: ValueSource;
  gear_table: ValueSource;
  current_max: ValueSource;
  power_max: ValueSource;
  hardware_max_temp: ValueSource;
}

/** 能力（全部来自本机检测） */
export interface Capabilities {
  current_max_ua: number;
  gear_max: number;
  gear_min: number;
  gear_open: number;
  gear_safe: number;
  /** 档位表行数 */
  gear_table_rows: number;
  /** 硬件保护温度（℃） */
  hardware_max_c: number;
  power_max_gear: number;
  /**
   * **充电功率上限**（W）—— 机型能力上限，用于钳位用户配置
   *
   * ⚠️ 这不是"当前实际充电功率"（那个看 `status.battery.power_w`）。
   *
   * | 概念 | 例子 | 来源 |
   * |---|---|---|
   * | 功率上限 | 本机 100W | 协商值 / 电池型号 / 设备树，取最大 |
   * | 当前功率 | 此刻 0.5W | 实时采样 |
   *
   * 未读出时为 `null` ⇒ **不做功率钳位**，用户可自由设置目标功率。
   */
  power_max_w: number | null;
  /** 供电类型 / 快充协议 */
  protocol: string | null;
  soc_limit_supported: boolean;
  /**
   * 本机支持的电量上限区间（%）
   *
   * ⚠️ **因机型而异**（本机 K90PM 为 60~100）。界面滑块的范围必须用它，
   * 而不是写死 60~100 —— 否则别的机型会用错范围。
   *
   * `null` = 本机不支持该功能，或档案未包含此信息。
   */
  soc_limit_min: number | null;
  soc_limit_max: number | null;
  /** 各值的来源标记（旧档案可能缺失） */
  sources?: Sources;
}

/** 档位表的一行 */
export interface GearRow {
  gear: number;
  /** 该档的电流上限（μA） */
  current_limit_ua: number;
}

/** 检测元信息 */
export interface ProbeMeta {
  /**
   * 未能成功测到的检测项（后端用英文 ID）
   *
   * ⚠️ 用 [`FAILED_LABEL`] 转成中文再显示 —— 直接把 ID 摆给用户
   * 等于没说（"current_ceiling 是什么？"）。
   */
  failed_items: string[];
  /** `quick_readonly` / `full` */
  method: string;
  /**
   * 需要插充电器才能测的项目
   *
   * 新版检测**已全只读**，这里应当为空。
   * 保留字段是为了兼容**旧版档案**，界面据此提示"建议重跑检测"。
   */
  needs_charge_for: string[];
  notes: string[];
  /** 检测时刻（Unix 秒） */
  probed_at?: number;
}

/**
 * 检测项 ID → 用户可读的中文
 *
 * 后端 `failed_items` 用的是英文 ID（便于日志检索），
 * 但界面上必须转成中文 —— 否则用户看到的是 `current_ceiling`
 * 这种看不懂的字符串。
 *
 * ⚠️ 新增检测项时记得同步这里；缺失的 ID 会**原样显示**（见
 * [`failedLabel`]），不至于看不见，但会显得突兀。
 */
export const FAILED_LABEL: Record<string, string> = {
  current_ceiling: "电流上限（需拔掉充电器才能测准）",
  control_nodes: "控速节点（本机型可能不支持）",
  gear_table: "充电档位表",
  gear_write_probe: "档位可写上限验证",
  hardware_max_temp: "硬件保护温度",
  observe_nodes: "电池观测节点",
  "power.max_w": "充电功率上限",
  soc_nodes: "充电上限节点",
};

/** 取检测项的中文名（未知 ID 原样返回，避免信息丢失） */
export function failedLabel(id: string): string {
  return FAILED_LABEL[id] ?? id;
}

/** `device show` 的 data */
export interface DeviceProfileView {
  battery: BatteryInfo;
  capabilities: Capabilities;
  identity: Identity;
  probe_meta: ProbeMeta;
  /** 档案来源（`本机实测` / `通用默认` / …） */
  source: string;
  /** 是否未经用户确认 */
  unverified: boolean;
  /** 是否已确认可用 */
  verified: boolean;
}

/** `device probe` 的结果（不落盘，供界面即时展示） */
export interface ProbeResult {
  /** 是否本次检测有保存 */
  saved: boolean;
  /** 检测耗时（毫秒） */
  elapsed_ms: number;
  /** 检测模式：`quick`（零副作用）/ `full`（含写探） */
  mode: "quick" | "full";
  /** 检测出的档案 */
  profile: DeviceProfileView;
}

/**
 * 实际充电功率的测量结果（`thermo measure`）
 *
 * ⚠️ 这是**此刻**的功率，随电量/充电器/温度变化 ——
 * 与档案里的「标称功率」（机型能力上限）是两回事。
 *
 * 结果**只展示**，不参与任何决策（后端也不落盘）。
 */
export interface PowerMeasurement {
  /** 是否正在充电 */
  charging: boolean;
  /** 电池状态原文（`Charging` / `Full` / …） */
  status: string;
  /** 电量（%） */
  soc: number;
  /** 实际采到的样本数 */
  samples: number;
  /** 峰值功率（W）；未充电时为 null */
  peak_w: number | null;
  /** 平均功率（W）；未充电时为 null */
  avg_w: number | null;
  /** 采样时长（毫秒） */
  duration_ms: number;
  /** 供电来源 */
  source: {
    /** `power_supply/usb/type` */
    usb_type: string | null;
    /** 充电协议（如 `PD_PPS`）；电脑 USB 口会报 `SDP` */
    protocol: string | null;
    /** 是否明显是弱电源（电脑 USB 口等） */
    weak: boolean;
  };
  /** 档案里的标称功率（W），供界面对比展示 */
  nominal_w: number | null;
}

// ============================================================
//  事件
// ============================================================

/** 一条运行事件 */
export interface RuntimeEvent {
  data?: Record<string, unknown>;
  /** 事件类型（见 Rust 侧 `observability::kind`） */
  kind: string;
  /** 人话说明 */
  msg: string;
  /** Unix 秒 */
  ts: number;
}

// ============================================================
//  多模式配置（保守 / 官方 / 极速 / 自定义）
// ============================================================

/**
 * 一个模式
 *
 * # 与旧「预设」的关键区别
 *
 * 旧预设是**模板**：套用一次就覆盖当前配置，切走即丢。
 * 模式是**独立配置**：四套各自保存，切走再切回，之前改的值还在。
 */
export interface ModeInfo {
  /** 模式 id（`conservative` / `official` / `turbo` / `custom`） */
  id: string;
  /** 中文名（保守 / 官方 / 极速 / 自定义） */
  label: string;
  /** 一句话说明 */
  description: string;
  /** 是否为当前生效的模式 */
  current: boolean;
  /**
   * 该模式的存档参数（`thermal` + `control` 两节）
   *
   * `null` = 该模式从未自定义过，首次切入时用出厂默认。
   */
  values: Record<string, unknown> | null;
}

/** `mode list` 的返回 */
export interface ModeList {
  /** 当前生效的模式 id */
  current: string;
  /** 全部模式（顺序即界面展示顺序） */
  modes: ModeInfo[];
}

// ============================================================
//  充电历史
// ============================================================

/** 曲线上的一个采样点 */
export interface HistoryPoint {
  /** Unix 秒 */
  t: number;
  /** 电池温度（℃） */
  temp_c: number;
  /** 实测功率（W） */
  power_w: number;
  /** 电量（%） */
  soc: number;
  /** 模块当时写入的档位 */
  gear: number;
  /** 模块当时写入的限流上限（μA） */
  limit_ua: number;
  /** 模块当时的状态（normal / tightening / hard_protect / idle） */
  phase: string;
}

/**
 * 一次充电会话（含完整曲线）
 *
 * ⚠️ 这是**观测数据**，与 `device.json` 的能力边界无关，绝不参与决策。
 */
export interface HistorySession {
  /** 会话 id（= 开始时间戳） */
  id: string;
  started_at: number;
  /** `0` = 仍在进行中 */
  ended_at: number;
  soc_start: number;
  soc_end: number;
  /** 全程峰值温度（℃） */
  peak_temp_c: number;
  /** 累计充入电量（Wh，梯形法积分） */
  energy_wh: number;
  /** 平均功率（W） */
  avg_power_w: number;
  /** 当前采样间隔（秒）；充太久会抽稀导致变大 */
  sample_s: number;
  points: HistoryPoint[];
}

/** 列表项（**不含曲线点** —— 列表页不需要几百个点） */
export interface HistoryListItem {
  id: string;
  started_at: number;
  ended_at: number;
  duration_s: number;
  soc_start: number;
  soc_end: number;
  peak_temp_c: number;
  energy_wh: number;
  avg_power_w: number;
  /** 曲线点数 */
  points: number;
}

/** 历史汇总 */
export interface HistoryStats {
  /** 已归档的会话数 */
  count: number;
  /** 累计充入电量（Wh） */
  energy_total_wh: number;
  /** 历史最高温度（℃） */
  peak_temp_c: number;
  /** 保留上限（次） */
  keep: number;
  /** 是否开启记录 */
  enabled: boolean;
  /** 采样间隔（秒） */
  sample_s: number;
  /** 此刻是否正在记录一次充电 */
  recording: boolean;
}

/** 进行中会话的摘要 */
export interface HistoryCurrent {
  id: string;
  started_at: number;
  duration_s: number;
  energy_wh: number;
  avg_power_w: number;
  peak_temp_c: number;
  points: number;
}

/** `history list` 的返回 */
export interface HistoryListResult {
  /** 已归档会话（**最新在前**） */
  sessions: HistoryListItem[];
  stats: HistoryStats;
  /** 正在记录的这一条（未充电时为 `null`） */
  current: HistoryCurrent | null;
}
