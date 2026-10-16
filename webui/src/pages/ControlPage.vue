<!--
  控制页 —— 所有"影响充电策略"的设置都收在这一页

  # 为什么合并成一页

  旧版把「温控阈值」放在温控页、「充到多少停」放在上限页。用户想配一套
  充电策略（比如"夏天别充太快、充到 80% 就停"）得在两个页签之间来回切，
  而且改完一边完全看不到另一边 —— 这些设置**互相影响**（降速电流与温度
  阈值共同决定限速结果），分开摆没有意义。

  # 四张卡片的顺序：由整体到局部

  | # | 卡片 | 回答的问题 | 粒度 |
  |---|---|---|---|
  | 1 | 模式 | 我整体想要哪种风格？ | 最粗：一次换掉一整套参数 |
  | 2 | 温度与降速 | 热了以后怎么降、降到多少？ | 中：温控的核心三件事 |
  | 3 | 功率上限 | 最多允许多快？ | 中：功率的天花板 |
  | 4 | 充电上限 | 充到多少就停？ | 最细：到点即停 |

  用户从上往下读，就是"先说要什么风格，再说细节"。

  # 三个统一的交互约定

  | # | 约定 | 为什么 |
  |---|---|---|
  | 1 | 拖动只改本地显示，松手 / 点「应用」才提交 | 拖动时每动一档就发一条 `ctl` 命令，既慢又吵 |
  | 2 | 温度与降速电流**三个值一起提交**（`set --batch`） | 内核要跨键校验"恢复全速 < 开始降速"，单键改会被当场拒绝 |
  | 3 | 写完之后「等一拍 → 回读 → 再回 `refresh()`」 | 见下：后端所有写都是**入队**，命令返回时还没落盘 |

  # ⚠️ 写操作是"入队"，不是"立即落盘"

  `set` / `mode switch` / `limit on|off` 全都是把改动塞进队列，由 worker
  在**本拍结束时**统一应用（后端 `engine::apply_pending`）。也就是说
  `ctl` 返回 `ok` 时配置还没写进 store —— 此时立刻回读会读到**旧值**，
  界面看起来就是"点了没反应"。

  因此每次写完之后都先等 [`SETTLE_MS`]（有客户端在等时 worker 的步进
  下限是 150 ms，这里留 4 倍余量），再回读，**最后**才清掉本地的
  "待提交值" —— 这样数字不会先跳回旧的再跳到新的。

  # ⚠️「官方」模式必须讲清楚（全页最需要解释清楚的一项）

  它不是"另一种充电策略"，而是**把控制权交回系统**：切过来时模块会先撤销
  自己此前加的限制（把档位/电流放回系统管理），之后不再干预。
  不写明白，用户会以为"选了官方还在被限速"（真机上确实会这么觉得）。

  # 温度基准固定为电池温度

  按用户明确要求：温控**以电池电芯温度为准**，界面上只展示、不可改
  （机身温度受握持与外壳影响，做基准会忽冷忽热）。
-->

<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import {
  MiuixButton,
  MiuixCard,
  MiuixDivider,
  MiuixSlider,
  MiuixSmallTitle,
  MiuixSwitch,
  MiuixText,
  showSnackbar,
} from "miuix-vue";

import InfoRow from "../components/InfoRow.vue";
import * as api from "../api";
import { f0, f1, thermalPairValid } from "../format";
import { perfMark } from "../perf";
import { refresh, sysState } from "../stores/sys";
import type { AddonStatus, ModeInfo, ThermalSection } from "../types";

// ============================================================
//  常量
// ============================================================

/**
 * 温度滑块下限（℃）
 *
 * 与后端 `MIN_TEMP_C` 一致：低于 20℃ 谈降温没有意义。
 */
const TEMP_FLOOR = 20;

/**
 * 提交后等待后端落盘的时长（毫秒）
 *
 * ⚠️ `set --batch` 是**入队**：命令返回 `ok` 时配置还没写进 store，
 * 由 worker 在"本拍"结束时统一落盘（后端 `engine::apply_pending`）。
 * 此时若立刻回读，读到的是**旧值** —— 界面会看起来"没生效"。
 * 有客户端在等时 worker 的步进下限是 150 ms，这里留 4 倍余量：
 * 既能读到新值，用户又几乎无感。
 */
const SETTLE_MS = 600;

/** 电量上限的快捷档位（会被本机支持范围过滤） */
const SOC_KEY_POINTS = [70, 80, 90, 100];

/**
 * 模式补充说明
 *
 * 后端每项只给一句短描述（卡片里展示）。这里补上"选了以后会发生什么"，
 * 其中**官方模式写得最细** —— 它是唯一会改变"谁在控制充电"的选项。
 */
const MODE_NOTES: Record<string, string> = {
  conservative:
    "降速更早、电流更低，优先照顾电池寿命。适合夏天，或者打算插着充一整夜。",
  official:
    "模块完全不插手：切过来时它会先撤销自己此前加的限制（把之前压下去的电流放回系统管理），之后不再干预充电。速度、温度、充到多少，全部交回手机自己决定 —— 相当于这台手机从没装过本模块。",
  turbo:
    "降速更晚、电流更高，充得最快；但发热和电池损耗也更明显。适合临时应急，不建议长期用。",
  custom:
    "完全按你在下面几张卡片里调的数值工作：模式本身不带策略，下面怎么设就怎么充。",
};

// ============================================================
//  界面状态
// ============================================================

const loading = ref(true);
/** 有提交动作正在进行（互斥：避免同时发多条写命令） */
const busy = ref(false);

// ── 卡片 1：模式 ──
const modes = ref<ModeInfo[]>([]);
const currentMode = ref("");
/** 刚点下、还没被后端确认的模式：用来让高亮**立刻**跟手 */
const pendingMode = ref<string | null>(null);

// ── 卡片 2：温度与降速 ──
const thermal = ref<ThermalSection | null>(null);
/** 本机硬件保护温度（℃），检测得来 —— 收紧滑块的上限由它决定 */
const hwMax = ref(45);

/** 本地待提交值；`null` = 与后端一致（沿用 ThermalPage 的写法） */
const pendingTighten = ref<number | null>(null);
const pendingRelax = ref<number | null>(null);
/**
 * 待提交的「过渡区间」（℃）
 *
 * ⚠️ 是**温度跨度**不是温度点：「37℃ 开始降速 + 过渡 1℃」= 38℃ 到达终点。
 */
const pendingRamp = ref<number | null>(null);
/**
 * 待提交的「降到多少」（W）
 *
 * ⚠️ 是**功率**不是电流 —— 用户反馈电流不直白（见模板里的说明）。
 */
const pendingPower = ref<number | null>(null);

// ── 卡片 3：功率上限 ──
/**
 * 本机最大充电功率（W），检测得来
 *
 * 两处都用它：
 * · 卡片 3「功率上限」的滑块上限
 * · 卡片 2「降到多少」的滑块上限（= 该滑块上「不减速」那一档）
 */
const hwMaxW = ref(100);
const pendingW = ref<number | null>(null);

// ── 卡片 4：充电上限 ──
const limitInfo = ref<AddonStatus | null>(null);
const socRange = ref<[number, number]>([api.SOC_MIN, api.SOC_MAX]);
const pendingSoc = ref<number | null>(null);
/** 刚拨动、还没被后端确认的开关状态：让开关**立刻**跟手 */
const pendingLimitOn = ref<boolean | null>(null);

// ============================================================
//  工具
// ============================================================

/** 统一错误提示（长停留，让用户看清后端说了什么） */
function fail(e: unknown): void {
  void showSnackbar({
    message: e instanceof Error ? e.message : String(e),
    duration: "long",
  });
}

/** 等一小会儿 */
function wait(ms: number): Promise<void> {
  return new Promise((resolve) => {
    window.setTimeout(resolve, ms);
  });
}

// ============================================================
//  读取数据
// ============================================================

/** 读模式列表 */
async function loadModes(): Promise<void> {
  const m = await api.fetchModes();
  modes.value = m.modes;
  currentMode.value = m.current;
}

/** 读温控配置 */
async function loadThermal(): Promise<void> {
  thermal.value = await api.fetchThermal();
}

/** 读充电上限状态 */
async function loadLimit(): Promise<void> {
  limitInfo.value = await api.fetchLimit();
}

/**
 * 首次加载
 *
 * 五条命令并行（模式 / 温控 / 硬件温度 / 充电上限 / 电量范围）。
 * 设备档案单独挂：它是**非关键路径**，只影响两个滑块的量程，
 * 读不到不该让整页报错。
 */
async function load(): Promise<void> {
  loading.value = true;
  const deviceP = api.fetchDeviceCached().catch(() => null);
  try {
    const [modeList, thermalData, hw, addon, range, dev] = await Promise.all([
      api.fetchModes(),
      api.fetchThermal(),
      api.fetchHardwareMaxTemp(),
      api.fetchLimit(),
      api.fetchSocRange(),
      deviceP,
    ]);

    modes.value = modeList.modes;
    currentMode.value = modeList.current;
    thermal.value = thermalData;
    hwMax.value = hw;
    limitInfo.value = addon;
    socRange.value = range;

    if (dev !== null) {
      const pw = dev.capabilities.power_max_w;
      if (typeof pw === "number" && pw > 0) {
        hwMaxW.value = pw;
      }
    }
    // 档案读不到时的兜底：至少让滑块能覆盖当前配置值
    //
    // ⚠️ 现在只需兜底**功率** —— 旧版还要兜底电流（`currMaxUa`），
    //    改成瓦数后那一项随 `tighten_power_w` 一起消失了。
    if (hwMaxW.value <= 0) {
      hwMaxW.value = Math.max(thermalData.tighten_power_w, POWER_STEP_W);
    }
  } catch (e) {
    fail(e);
  } finally {
    loading.value = false;
    perfMark("loaded:control");
  }
}

onMounted(() => {
  void load();
});

// ============================================================
//  卡片 1：模式（切换即时生效，没有「保存」这一步）
// ============================================================

/**
 * 界面上"当前模式"
 *
 * 有刚点的那个就用它（高亮立刻跟手），否则用后端确认过的值。
 */
const activeMode = computed(() => pendingMode.value ?? currentMode.value);

/** 当前模式的中文名 */
const currentLabel = computed(
  () => modes.value.find((m) => m.id === activeMode.value)?.label ?? "",
);

/** 当前模式的补充说明（没见过的 id 就不显示，宁可不写也别猜） */
const currentNote = computed(() => MODE_NOTES[activeMode.value] ?? "");

/**
 * 切换模式
 *
 * 后端语义：先**保存当前模式的值**，再**装载目标模式的值** ——
 * 所以切完必须把模式、温控、功率三处都重新读一遍，否则界面显示的还是
 * 上一个模式的数字。
 */
async function pickMode(m: ModeInfo): Promise<void> {
  if (busy.value || m.id === activeMode.value) {
    return;
  }
  busy.value = true;
  // 先高亮，别让用户盯着没反应的界面
  pendingMode.value = m.id;
  try {
    await api.switchMode(m.id);
    await wait(SETTLE_MS);
    // 本地未提交的改动作废：新模式的存档值才是"当前状态"
    pendingTighten.value = null;
    pendingRelax.value = null;
    pendingRamp.value = null;
    pendingPower.value = null;
    await Promise.all([loadModes(), loadThermal(), refresh()]);
    void showSnackbar({ message: `已切到「${m.label}」`, duration: "short" });
  } catch (e) {
    fail(e);
  } finally {
    // 成功时后端值已就位；失败时这里会让高亮退回真实模式
    pendingMode.value = null;
    busy.value = false;
  }
}

// ============================================================
//  卡片 2：温度与降速
// ============================================================

/** 开始降速的温度（℃） */
const tighten = computed(
  () => pendingTighten.value ?? thermal.value?.tighten_temp ?? 43,
);

/** 收紧滑块上限（℃）= 本机硬件保护温度，不写死 */
const tightMax = computed(() =>
  Math.max(TEMP_FLOOR + 1, Math.floor(hwMax.value)),
);

/** 恢复全速的温度上限（℃）：必须比收紧温度低 1℃ 以上 */
const relaxMax = computed(() => Math.max(TEMP_FLOOR, Math.floor(tighten.value) - 1));

/** 恢复全速的温度（℃）；显示值始终钳在合法范围内 */
const relax = computed(() => {
  const v = pendingRelax.value ?? thermal.value?.relax_temp ?? 42;
  return Math.min(v, relaxMax.value);
});

/**
 * 拖动「恢复全速」时先钳位
 *
 * 不钳的话，用户把「开始降速」调到 30℃ 后，这个滑块还能停在 42℃ ——
 * 越界值只有到提交时才被后端拒绝，那时用户已经看不出是哪一步弄错的。
 */
function onRelaxMove(v: number): void {
  pendingRelax.value = Math.min(v, relaxMax.value);
}

/**
 * 过渡区间的步进（℃）
 *
 * 0.5℃ 一格：范围只有 0.5~5.0，1℃ 一格太粗（用户想要"从 3℃ 缩到 1℃"
 * 这种调整就做不到）。
 */
const RAMP_STEP_C = 0.5;

/** 过渡区间下限（℃）—— 与后端 `thermal.ramp_span` 的 min 一致 */
const RAMP_MIN_C = 0.5;

/** 过渡区间上限（℃）—— 与后端 `thermal.ramp_span` 的 max 一致 */
const RAMP_MAX_C = 5.0;

/**
 * 过渡区间（℃）—— 从「开始降速的温度」到「降到多少」之间走多长
 *
 * # 为什么要把它暴露出来（2026-10-14 用户报告后新增）
 *
 * 这个参数**一直都在起作用**，但界面上从来没有它 —— 于是用户设了
 * 「37℃ 开始降速、降到 10W」，在 38℃ 看到功率还是很高，就以为失效了。
 *
 * 实际上那 3℃（保守模式的出厂值）的过渡区间意味着：
 *
 * ```text
 * 37℃ → 开始降速
 * 38℃ → 才走了 1/3，功率仍在半路
 * 40℃ → 才到达用户设的 10W
 * ```
 *
 * ⇒ 「到 37℃ 就该变成 10W」是用户的合理预期，但 `ramp` 律的设计是**渐变**。
 *    看不见这个参数，用户就无法理解为什么"设了没效果"。
 *
 * 用户要求（2026-10-14）：「过渡区间调整为 1 度」—— 想让降速直接一点。
 */
const rampSpan = computed(
  () => pendingRamp.value ?? thermal.value?.ramp_span ?? 2,
);

/**
 * 过渡区间的终点温度（℃）= 开始降速温度 + 过渡区间
 *
 * ⚠️ 会被硬件保护温度夹住（与后端 `ramp_end()` 一致）—— 否则界面会显示
 * 一个后端永远不会到达的温度。
 */
const rampEndTemp = computed(() =>
  Math.min(tighten.value + rampSpan.value, hwMax.value),
);

/**
 * 降速目标功率的步进（W）
 *
 * 1 W 足够细（本机上限 100 W ⇒ 100 个档位），而且瓦数本身就是整数语义，
 * 不像旧版的电流那样需要 `0.1 A` 的步进才够用。
 */
const POWER_STEP_W = 1;


/** 温度超过收紧点后，把充电功率压到的值（W）；0 = 停止充电 */
const power = computed(
  () => pendingPower.value ?? thermal.value?.tighten_power_w ?? 0,
);

/**
 * 功率滑块上限（W）
 *
 * 取**本机档案的功率上限**（`hardware_max_power_w`）—— 那正是
 * "不减速"的含义：压到本机满功率等于没有额外限制。
 *
 * 兜底 100 W 用于档案尚未检测出来的情况（此时滑块仍可用）。
 */
const powerSliderMax = computed(() =>
  Math.max(POWER_STEP_W, hwMaxW.value > 0 ? Math.round(hwMaxW.value) : 100),
);

/**
 * 降速目标功率的快捷档位
 *
 * 「不减速」= 本机满功率（档案上限）—— 压到上限等于实际没有降速，
 * 适合"我只想要温控阈值，不想让模块压低功率"的用户。
 *
 * ⚠️ 换成功率后档位数值必须**重算**（旧版是 0.7A / 2.5A / 7A）：
 *    按 4.0 V 的名义电压折算，让"感觉上"的强度与旧版接近。
 */
const powerPresets = computed(() => {
  const list = [
    { label: "停止充电", w: 0 },
    { label: "3 W", w: 3 },
    { label: "10 W", w: 10 },
    { label: "28 W", w: 28 },
  ];
  // 超出本机量程的档位不显示（例如 30W 机型就不给 28W 以上的选项）
  return list
    .filter((p) => p.w <= powerSliderMax.value)
    .concat(
      powerSliderMax.value > 0
        ? [{ label: "不减速", w: powerSliderMax.value }]
        : [],
    );
});

/** 滑块上的刻度点：与快捷档位对齐，让用户知道那几档在什么位置 */
const powerKeyPoints = computed(() => powerPresets.value.map((p) => p.w));

/** 当前降速功率的中文显示 */
const powerLabel = computed(() => {
  const v = power.value;
  if (v <= 0) {
    return "停止充电";
  }
  if (powerSliderMax.value > 0 && v >= powerSliderMax.value) {
    return "不减速";
  }
  return `${v} W`;
});

/** 四项改动标记 */
const dirtyTighten = computed(
  () =>
    pendingTighten.value !== null &&
    pendingTighten.value !== thermal.value?.tighten_temp,
);
const dirtyRelax = computed(
  () =>
    pendingRelax.value !== null && pendingRelax.value !== thermal.value?.relax_temp,
);
const dirtyRamp = computed(
  () =>
    pendingRamp.value !== null && pendingRamp.value !== thermal.value?.ramp_span,
);
const dirtyPower = computed(
  () =>
    pendingPower.value !== null &&
    pendingPower.value !== thermal.value?.tighten_power_w,
);
const anyDirty = computed(
  () =>
    dirtyTighten.value ||
    dirtyRelax.value ||
    dirtyRamp.value ||
    dirtyPower.value,
);

/** 取消本地改动 */
function cancelThermal(): void {
  pendingTighten.value = null;
  pendingRelax.value = null;
  pendingRamp.value = null;
  pendingPower.value = null;
}

/**
 * 提交温度与降速设置
 *
 * ⚠️ 只提交**真正改过**的键；但温度两项只要有一项变了，就一起写 ——
 * 内核要求 `relax_temp ≤ tighten_temp - 1`，单独改一个会被跨键校验拒绝。
 */
async function applyThermal(): Promise<void> {
  if (busy.value) {
    return;
  }
  const t = tighten.value;
  const r = relax.value;
  const rs = rampSpan.value;
  const p = power.value;

  const check = thermalPairValid(t, r);
  if (!check.ok) {
    void showSnackbar({ message: check.reason, duration: "long" });
    return;
  }

  const pairs: [string, string][] = [];
  if (t !== thermal.value?.tighten_temp || r !== thermal.value?.relax_temp) {
    pairs.push(["thermal.tighten_temp", String(t)]);
    pairs.push(["thermal.relax_temp", String(r)]);
  }
  if (rs !== thermal.value?.ramp_span) {
    pairs.push(["thermal.ramp_span", String(rs)]);
  }
  if (p !== thermal.value?.tighten_power_w) {
    pairs.push(["thermal.tighten_power_w", String(Math.round(p))]);
  }
  if (pairs.length === 0) {
    cancelThermal();
    return;
  }

  busy.value = true;
  try {
    await api.setConfig(pairs);
    // 等后端落盘再回读；回读**完成之后**才清掉本地值，
    // 这样界面上数字是从用户设定值无缝切到后端值，不会跳回旧的
    await wait(SETTLE_MS);
    await Promise.all([loadThermal(), refresh()]);
    cancelThermal();
    void showSnackbar({ message: "温度与降速设置已更新", duration: "short" });
  } catch (e) {
    // 失败就回到后端的真实值，不要停在"看起来已生效"的假象上
    cancelThermal();
    fail(e);
  } finally {
    busy.value = false;
  }
}

// ============================================================
//  卡片 3：功率上限
// ============================================================

/**
 * 当前生效的功率上限（W）
 *
 * 取自全局状态（`status.control.power_limit_w`），不再单独拉一条命令；
 * 提交后 `refresh()` 会把它更新。`null` = 首次状态还没读到。
 */
const statusW = computed(() => sysState.status?.control.power_limit_w ?? null);
const powerReady = computed(() => statusW.value !== null);
const powerLimitW = computed(() => statusW.value ?? 0);

/** 滑块显示值（0 = 不限制） */
const shownW = computed(() => pendingW.value ?? powerLimitW.value);

/** 是否处于"不限制"状态 */
const unlimitedW = computed(() => shownW.value <= 0);

/**
 * 卡片副标题
 *
 * ⚠️ 有未提交的改动时**必须提示出来** —— 因为「拖动只是预览，
 * 点应用才生效」，若副标题直接显示 pending 值，用户会以为已经改好了。
 */
const powerSummary = computed(() => {
  if (!powerReady.value) {
    return "正在读取当前设置…";
  }
  if (powerDirty.value) {
    return "已拖动，点下方「应用」才会生效";
  }
  return unlimitedW.value
    ? "当前不限制充电功率"
    : `充电功率不超过 ${f0(shownW.value)} W`;
});

/**
 * 有未提交的改动
 *
 * # 交互约定（用户 2026-10-13 明确要求）
 *
 * > 「拖动滑块后必须点应用才能更改」
 *
 * 因此滑块**只是预览**：拖动只改本地 `pendingW`，不碰后端。
 * 这样用户可以从容调到想要的值再一次性提交，中途不会因为
 * 手抖滑过某个值就把手机限速了。
 *
 * 本页所有设置项都遵守这一条（温度、降速、功率、充电上限），
 * 保持交互一致。
 */
const powerDirty = computed(
  () => pendingW.value !== null && Math.round(pendingW.value) !== powerLimitW.value,
);

/** 取消本地改动（把滑块退回生效值） */
function cancelPower(): void {
  pendingW.value = null;
}

/**
 * 提交功率上限
 *
 * ⚠️ 只由「应用」按钮调用 —— 不再接 `change-end`（松手即提交已被移除）。
 */
async function applyPower(): Promise<void> {
  if (busy.value || pendingW.value === null) {
    return;
  }
  const w = Math.round(pendingW.value);
  if (w === powerLimitW.value) {
    pendingW.value = null;
    return;
  }

  busy.value = true;
  try {
    await api.setPowerLimit(w);
    // 等落盘再刷新全局状态；刷新完成后才清本地值（滑块不会跳回旧值）
    await wait(SETTLE_MS);
    await refresh();
    pendingW.value = null;
    void showSnackbar({
      message: w <= 0 ? "已取消功率限制" : `已限制充电功率不超过 ${w} W`,
      duration: "short",
    });
  } catch (e) {
    pendingW.value = null;
    fail(e);
  } finally {
    busy.value = false;
  }
}

// ============================================================
//  卡片 4：充电上限（整块搬自旧「上限页」，文案保留）
// ============================================================

const limitSupported = computed(() => limitInfo.value?.supported ?? false);
const limitEnabled = computed(() => limitInfo.value?.enabled ?? false);
const socTarget = computed(() => limitInfo.value?.detail.soc_target ?? 80);
const socActive = computed(() => limitInfo.value?.detail.soc_active ?? null);

/**
 * **使用前提**（后端 `spec.requires`）—— 卡片顶部那块提示
 *
 * 🔴 用户要求（2026-10-16）：「加上提示：此模块需打开电池健康保护功能」。
 *
 * ⚠️ 文案**来自后端**（`soc_limit::REQUIRES_BATTERY_PROTECT`），不在这里
 *    硬编码 —— `limit show` 输出的是同一份，两处各写一份必然会有一天对不上。
 *
 * `null`（后端未提供）时整块提示不渲染，因此对不支持前提的老版本也安全。
 */
const limitRequires = computed(() => limitInfo.value?.requires ?? null);

/**
 * **系统界面不会同步**的说明（后端 `detail.note_ui_not_synced`）
 *
 * 设了 90% 但系统设置页仍显示 80% —— 用户会误判成"没生效"。
 * 必须主动说明，否则这是"看起来失败其实成功了"的典型陷阱。
 */
const limitUiNote = computed(
  () => limitInfo.value?.detail.note_ui_not_synced ?? null,
);

/** 本机支持的电量区间（不写死 60~100，机型不同范围不同） */
const socMin = computed(() => socRange.value[0]);
const socMax = computed(() => socRange.value[1]);

/** 快捷档位：只保留落在本机范围内的 */
const socKeyPoints = computed(() =>
  SOC_KEY_POINTS.filter((v) => v >= socMin.value && v <= socMax.value),
);

/** 滑块显示值 */
const socShown = computed(() => pendingSoc.value ?? socTarget.value);

/** 是否有未提交的改动 */
const socDirty = computed(
  () => pendingSoc.value !== null && pendingSoc.value !== socTarget.value,
);

/** 开关显示状态：本地乐观值优先，让开关立刻跟手 */
const limitEnabledShown = computed(
  () => pendingLimitOn.value ?? limitEnabled.value,
);

/** 开关：开 = 开始约束，关 = 停止干预（零写入） */
async function onToggleLimit(v: boolean): Promise<void> {
  if (busy.value) {
    return;
  }
  busy.value = true;
  // 先让开关动起来，别等后端确认（确认前用户会以为点漏了）
  pendingLimitOn.value = v;
  try {
    if (v) {
      await api.limitOn();
      void showSnackbar({ message: "已启用充电上限", duration: "short" });
    } else {
      await api.limitOff();
      void showSnackbar({
        message: "已停止干预（不会改写驱动里的限制值）",
        duration: "long",
      });
    }
    await wait(SETTLE_MS);
    await Promise.all([loadLimit(), refresh()]);
  } catch (e) {
    fail(e);
  } finally {
    // 成功时后端值已就位；失败时这里会让开关退回真实状态
    pendingLimitOn.value = null;
    busy.value = false;
  }
}

/** 提交目标电量 */
async function applySoc(): Promise<void> {
  if (pendingSoc.value === null || busy.value) {
    return;
  }
  const v = pendingSoc.value;
  busy.value = true;
  try {
    await api.setLimit(v);
    await wait(SETTLE_MS);
    await Promise.all([loadLimit(), refresh()]);
    pendingSoc.value = null;
    void showSnackbar({ message: `已设为 ${v}%`, duration: "short" });
  } catch (e) {
    pendingSoc.value = null;
    fail(e);
  } finally {
    busy.value = false;
  }
}
</script>

<template>
  <div class="page">
    <!--
      ── 卡片 1：模式 ──

      ⚠️ 分组标题必须放在**卡片外面**（`.m-small-title`）

      初版把标题写在卡片**内部**（`MiuixText type="title4"`），而状态 /
      日志 / 更多三页都是"卡外分组标题 + 无标题卡片"。

      两套做法并存的后果：控制页的标题字号（18px）与其他页的分组标题
      （16px）不同，位置也不同（一个在卡内、一个在卡外），
      于是四个页面切来切去时"感觉不是同一个 App"。

      现已统一为后者。说明文字留在卡内顶部 —— 它解释的是**这张卡**，
      贴着内容更自然。
    -->
    <MiuixSmallTitle text="充电模式" />
    <MiuixCard>
      <div class="head">
        <div class="head__text">
          <MiuixText
            type="footnote1"
            color="var(--m-color-on-surface-secondary)"
          >
            点一下就切换，立刻生效（不用保存）
          </MiuixText>
        </div>
        <span v-if="currentLabel" class="chip chip--on">{{ currentLabel }}</span>
      </div>

      <!--
        用 role="radio" + 键盘事件：卡片是"可点击的块"，不是按钮，
        但没有键盘通道的话辅助功能用户就用不了这一页。
      -->
      <div v-if="modes.length === 0" class="empty">
        <MiuixText
          type="footnote1"
          color="var(--m-color-on-surface-secondary)"
        >
          正在读取模式…
        </MiuixText>
      </div>
      <div v-else class="modes" role="radiogroup" aria-label="充电模式">
        <MiuixCard
          v-for="m in modes"
          :key="m.id"
          class="mode"
          :class="{ 'mode--on': m.id === activeMode }"
          role="radio"
          :aria-checked="m.id === activeMode"
          :aria-label="`${m.label}：${m.description}`"
          tabindex="0"
          show-indication
          press-feedback="sink"
          @click="pickMode(m)"
          @keydown.enter.prevent="pickMode(m)"
          @keydown.space.prevent="pickMode(m)"
        >
          <div class="mode__top">
            <MiuixText type="body1" weight="medium">{{ m.label }}</MiuixText>
            <!-- 除了底色与描边，再给一个不依赖颜色的标记 -->
            <span v-if="m.id === activeMode" class="mode__dot" aria-hidden="true" />
          </div>
          <MiuixText type="footnote1" class="mode__desc">
            {{ m.description }}
          </MiuixText>
        </MiuixCard>
      </div>

      <!-- 当前模式的后果说明；「官方」那一项写得最细 -->
      <div v-if="currentNote" class="notice">
        <MiuixText type="footnote1">{{ currentNote }}</MiuixText>
      </div>
    </MiuixCard>

    <!-- ── 卡片 2：温度与降速 ── -->
    <MiuixSmallTitle text="温度控制" />
    <MiuixCard>
      <MiuixText
        type="footnote1"
        color="var(--m-color-on-surface-secondary)"
        class="lead"
      >
        手机热起来以后怎么充、降到多慢，由这里决定
      </MiuixText>

      <!-- 开始降速的温度 -->
      <div class="field">
        <div class="field__head">
          <div class="field__text">
            <MiuixText type="body1">开始降速的温度</MiuixText>
            <MiuixText
              type="footnote1"
              color="var(--m-color-on-surface-secondary)"
            >
              达到此温度开始降速
            </MiuixText>
          </div>
          <MiuixText type="title3" color="var(--m-color-primary)">
            {{ f0(tighten) }}℃
          </MiuixText>
        </div>
        <MiuixSlider
          :model-value="tighten"
          :min="TEMP_FLOOR"
          :max="tightMax"
          :step="1"
          :disabled="loading || busy"
          @update:model-value="pendingTighten = $event"
        />
        <div class="scale">
          <MiuixText
            type="footnote2"
            color="var(--m-color-on-surface-secondary)"
          >
            {{ TEMP_FLOOR }}℃
          </MiuixText>
          <MiuixText
            type="footnote2"
            color="var(--m-color-on-surface-secondary)"
          >
            {{ f0(tightMax) }}℃（本机保护温度）
          </MiuixText>
        </div>
      </div>

      <!-- 恢复全速的温度 -->
      <div class="field">
        <div class="field__head">
          <div class="field__text">
            <MiuixText type="body1">恢复全速的温度</MiuixText>
            <MiuixText
              type="footnote1"
              color="var(--m-color-on-surface-secondary)"
            >
              降到此温度恢复全速
            </MiuixText>
          </div>
          <MiuixText type="title3" color="var(--m-color-primary)">
            {{ f0(relax) }}℃
          </MiuixText>
        </div>
        <MiuixSlider
          :model-value="relax"
          :min="TEMP_FLOOR"
          :max="relaxMax"
          :step="1"
          :disabled="loading || busy"
          @update:model-value="onRelaxMove"
        />
        <div class="scale">
          <MiuixText
            type="footnote2"
            color="var(--m-color-on-surface-secondary)"
          >
            {{ TEMP_FLOOR }}℃
          </MiuixText>
          <MiuixText
            type="footnote2"
            color="var(--m-color-on-surface-secondary)"
          >
            最高 {{ f0(relaxMax) }}℃（至少比降速点低 1℃）
          </MiuixText>
        </div>
      </div>

      <!--
        过渡区间

        🔴 2026-10-14 新增（用户报告「降速好像失效了」之后）

        这一段解释的是：从「开始降速的温度」到「降到多少」**不是瞬间完成**，
        而是用一段温度区间平滑滑过去。用户看不见它，就无法理解为什么
        "设了 37℃ 降速、38℃ 却还没降到位"。

        用 0.5℃ 步进：范围只有 0.5~5.0，1℃ 一格太粗。
      -->
      <div class="field">
        <div class="field__head">
          <div class="field__text">
            <MiuixText type="body1">过渡区间</MiuixText>
            <MiuixText
              type="footnote1"
              color="var(--m-color-on-surface-secondary)"
            >
              从开始降速到降到目标，用多长温度走完 ——
              设 {{ f1(RAMP_MIN_C) }}℃ 就几乎立刻降到位
            </MiuixText>
          </div>
          <MiuixText type="title3" color="var(--m-color-primary)">
            {{ f1(rampSpan) }}℃
          </MiuixText>
        </div>
        <MiuixSlider
          :model-value="rampSpan"
          :min="RAMP_MIN_C"
          :max="RAMP_MAX_C"
          :step="RAMP_STEP_C"
          :disabled="loading || busy"
          @update:model-value="pendingRamp = $event"
        />
        <div class="scale">
          <MiuixText
            type="footnote2"
            color="var(--m-color-on-surface-secondary)"
          >
            更快降到位
          </MiuixText>
          <MiuixText
            type="footnote2"
            color="var(--m-color-on-surface-secondary)"
          >
            更平缓
          </MiuixText>
        </div>

        <!--
          把"这段区间到哪个温度结束"直接算给用户看

          「37℃ + 1℃ = 38℃ 到达目标」比「过渡区间 1℃」好懂得多 ——
          用户真正想知道的是"我几度能看到降速完成"。
        -->
        <MiuixText
          type="footnote2"
          color="var(--m-color-on-surface-secondary)"
          class="hint"
        >
          实际过程：{{ f0(tighten) }}℃ 开始降速 →
          <b>{{ f1(rampEndTemp) }}℃</b> 降到 {{ powerLabel }}
        </MiuixText>
      </div>

      <!--
        降速后压到多少功率

        🔴 这里是**功率（W）**，不是电流（A）

        用户反馈（2026-10-14）：「降到多少是电流，控制电流不够直白，
        改成控制功率瓦数」。理由见 `thermo_core::Config::tighten_power_w`：
        瓦是用户理解的量（充电器、快充协议都标瓦），而安培需要知道电压
        才能换算成"多快"。

        ⚠️ 决策层仍只能写电流 —— 换算 `I = P / U` 在后端做
        （`decision::tighten_curr_ua`，用**实测电池电压**）。
      -->
      <div class="field">
        <div class="field__head">
          <div class="field__text">
            <MiuixText type="body1">降到多少</MiuixText>
            <MiuixText
              type="footnote1"
              color="var(--m-color-on-surface-secondary)"
            >
              温度超过收紧温度后，把充电功率压到这个值；选 0 就是停止充电
            </MiuixText>
          </div>
          <MiuixText type="title3" color="var(--m-color-primary)">
            {{ powerLabel }}
          </MiuixText>
        </div>

        <!-- 快捷档位：不用理解瓦数也能选对 -->
        <div class="chips">
          <button
            v-for="p in powerPresets"
            :key="p.w"
            type="button"
            class="chip chip--pick"
            :class="{ 'chip--on': power === p.w }"
            :disabled="loading || busy"
            @click="pendingPower = p.w"
          >
            {{ p.label }}
          </button>
        </div>

        <MiuixSlider
          :model-value="power"
          :min="0"
          :max="powerSliderMax"
          :step="POWER_STEP_W"
          :show-key-points="true"
          :key-points="powerKeyPoints"
          :disabled="loading || busy"
          @update:model-value="pendingPower = $event"
        />
        <div class="scale">
          <MiuixText
            type="footnote2"
            color="var(--m-color-on-surface-secondary)"
          >
            停止充电
          </MiuixText>
          <MiuixText
            type="footnote2"
            color="var(--m-color-on-surface-secondary)"
          >
            {{ powerSliderMax }} W（本机上限）
          </MiuixText>
        </div>
      </div>

      <div v-if="anyDirty" class="actions">
        <MiuixButton class="actions__btn" type="primary" @click="applyThermal">
          应用
        </MiuixButton>
        <MiuixButton class="actions__btn" @click="cancelThermal">
          取消
        </MiuixButton>
      </div>
      <MiuixText
        v-else
        type="footnote2"
        color="var(--m-color-on-surface-secondary)"
        class="hint"
      >
        拖动只是预览，点「应用」才写入。温度的两个值必须一起提交：
        「恢复全速」至少要比「开始降速」低 1℃
      </MiuixText>

      <MiuixDivider class="gap" />

      <!--
        温度基准：固定不可改（用户要求以电池温度为准）

        ⚠️ 这里原有三行：「温度基准」「当前电池温度」「本机保护温度」。

        用户要求（2026-10-14）：删掉**后两行**，挪到状态首页 ——
        那两个是**实时/硬件**读数，属于"看状态"，不属于"调设置"；
        放在设置页会让这一块既是仪表盘又是控制面板，职责混杂。

        保留「温度基准」：它是这一页**唯一的**说明性事实 —— 告诉用户
        "下面这两个滑块是在控制电池温度"，改不了、但必须知道。
      -->
      <InfoRow
        title="温度基准"
        summary="温控只看电池电芯温度，机身温度仅作保护，这里不可更改"
      >
        <template #end>
          <MiuixText type="body2">电池温度</MiuixText>
        </template>
      </InfoRow>
    </MiuixCard>

    <!-- ── 卡片 3：功率上限 ── -->
    <MiuixSmallTitle text="功率限制" />
    <MiuixCard>
      <div class="head">
        <div class="head__text">
          <MiuixText
            type="footnote1"
            color="var(--m-color-on-surface-secondary)"
          >
            {{ powerSummary }}
          </MiuixText>
        </div>
        <MiuixText type="title3" color="var(--m-color-primary)">
          {{ unlimitedW ? "不限" : `${f0(shownW)} W` }}
        </MiuixText>
      </div>

      <div class="slider">
        <MiuixSlider
          :model-value="shownW"
          :min="0"
          :max="hwMaxW"
          :step="1"
          :disabled="loading || busy || !powerReady"
          @update:model-value="pendingW = $event"
        />
        <div class="scale">
          <MiuixText
            type="footnote2"
            color="var(--m-color-on-surface-secondary)"
          >
            0 W（不限制）
          </MiuixText>
          <MiuixText
            type="footnote2"
            color="var(--m-color-on-surface-secondary)"
          >
            {{ f0(hwMaxW) }} W（本机上限）
          </MiuixText>
        </div>
      </div>

      <!-- 拖动只是预览；点「应用」才写入（与温度卡片同一套交互） -->
      <div v-if="powerDirty" class="actions">
        <MiuixButton class="actions__btn" type="primary" @click="applyPower">
          应用 {{ f0(shownW) }} W
        </MiuixButton>
        <MiuixButton class="actions__btn" @click="cancelPower">取消</MiuixButton>
      </div>
      <MiuixText
        v-else
        type="footnote2"
        color="var(--m-color-on-surface-secondary)"
        class="hint"
      >
        拖动只是预览，点「应用」才写入。想完全不限制充电功率，把滑块拖到最左边
      </MiuixText>
    </MiuixCard>

    <!-- ── 卡片 4：充电上限 ── -->
    <MiuixSmallTitle text="充电上限" />
    <MiuixCard v-if="!loading && !limitSupported">
      <MiuixText type="title4">本机不支持</MiuixText>
      <!--
        ⚠️ 不支持时**必须先提示检查系统开关**

        节点找不到有两种原因，表象完全相同：
          · 系统「电池健康保护」没打开 —— 用户花 10 秒就能解决
          · 机型真的不支持 —— 无解

        若只写"本机型不支持"，绝大多数用户会直接放弃，
        而他们很可能只是少开了一个开关。后端的 `probe` 文案已经把
        开关检查放在第一位，这里原样透出即可。
      -->
      <MiuixText
        type="footnote1"
        color="var(--m-color-on-surface-secondary)"
        class="hint"
      >
        {{ limitInfo?.probe || "未找到对应的控制节点" }}
      </MiuixText>
    </MiuixCard>

    <MiuixCard v-else>
      <!-- 总开关 -->
      <div class="row">
        <div class="row__text">
          <MiuixText type="body1">启用</MiuixText>
          <MiuixText
            type="footnote1"
            color="var(--m-color-on-surface-secondary)"
          >
            充到设定电量即停止
          </MiuixText>
        </div>
        <MiuixSwitch
          :model-value="limitEnabledShown"
          :disabled="loading || busy"
          @update:model-value="onToggleLimit"
        />
      </div>

      <MiuixDivider class="gap" />

      <!-- 目标电量 -->
      <div class="head" :class="{ 'is-dim': !limitEnabled }">
        <div class="head__text">
          <MiuixText type="title3">{{ socShown }}%</MiuixText>
          <MiuixText
            type="footnote1"
            color="var(--m-color-on-surface-secondary)"
          >
            目标电量
          </MiuixText>
        </div>
        <div class="chip" :class="socActive !== null ? 'chip--on' : ''">
          {{ socActive !== null ? `生效中 ${socActive}%` : "未生效" }}
        </div>
      </div>

      <div class="slider" :class="{ 'is-dim': !limitEnabled }">
        <MiuixSlider
          :model-value="socShown"
          :min="socMin"
          :max="socMax"
          :step="1"
          :disabled="!limitEnabled || busy"
          :show-key-points="true"
          :key-points="socKeyPoints"
          @update:model-value="pendingSoc = $event"
        />
        <div class="scale">
          <MiuixText
            type="footnote2"
            color="var(--m-color-on-surface-secondary)"
          >
            {{ socMin }}%
          </MiuixText>
          <MiuixText
            type="footnote2"
            color="var(--m-color-on-surface-secondary)"
          >
            {{ socMax }}%
          </MiuixText>
        </div>
      </div>

      <div v-if="socDirty" class="actions">
        <MiuixButton class="actions__btn" type="primary" @click="applySoc">
          应用 {{ socShown }}%
        </MiuixButton>
        <MiuixButton class="actions__btn" @click="pendingSoc = null">
          取消
        </MiuixButton>
      </div>
      <MiuixText
        v-else-if="!limitEnabled"
        type="footnote2"
        color="var(--m-color-on-surface-secondary)"
        class="hint"
      >
        先打开上面的开关，才能调整目标电量
      </MiuixText>

      <MiuixDivider class="gap" />

      <!-- 说明与状态 -->
      <div class="notice">
        <MiuixText type="footnote1">
          <b>关闭开关不会改写驱动里的限制值</b> —— 手机会继续停在当前上限。
          想恢复满充，把目标调成 {{ socMax }}% 即可。
        </MiuixText>
      </div>

      <!--
        ┌─ 系统界面不会同步 ─────────────────────────────────────────┐

        系统「电池保护」设置页有一套自己的界面文案，写的是它自己记的
        那个百分比（默认 80%）。那个显示逻辑在**系统应用里**，
        而本模块是**纯 root + sysfs，没有 hook 层**，改不了系统界面。

        不说清楚的话：用户设了 90%，进系统设置一看仍写着 80%，
        判断成"没生效" —— 而实际上限制**已经生效**（写的是同一个节点）。
        这是"看起来失败其实成功了"的典型，必须在界面上说透。

        文案来自后端 `detail.note_ui_not_synced`，与 `limit show` 同源。
      -->
      <div v-if="limitUiNote" class="notice">
        <MiuixText type="footnote1" color="var(--m-color-on-surface-secondary)">
          {{ limitUiNote }}
        </MiuixText>
      </div>

      <!--
        ┌─ 使用前提 ────────────────────────────────────────────────┐

        用户原话（2026-10-16）：「加上提示：此模块需打开电池健康保护功能」。
        文案来自后端 `spec.requires`（与 `limit show` 同一份），
        不在前端硬编码 —— 两处各写一份必然有一天会不一致。

        ⚠️ 位置变更（2026-10-17 用户要求「把使用前提示放到下面」）：
        原先放在**卡片顶部**，理由是"少了这个开关功能完全不生效，
        应先看到它再动手"。

        但那条理由的前提不成立 —— 实测本机**已支持**（节点探测命中），
        这时"使用前提"就成了一个**已经满足的条件说明**，
        占着卡片最显眼的位置反而把真正的操作（开关、目标电量）挤到下面。

        ⇒ 移到底部，与其它两条说明并列。
          它的告警色（`.notice--warn`）保留：万一节点真的消失
          （用户关掉了系统开关），这条仍是页面上唯一带色的提示，
          不会因为位置靠下而被忽略。

        ⚠️ 若将来发现"不支持"是常见情形，应把它挪回去 ——
           `limitSupported` 为 false 时走的是另一张卡片（顶部已有提示）。
      -->
      <div v-if="limitRequires" class="notice notice--warn">
        <MiuixText type="footnote1" color="var(--m-color-on-tertiary-container)">
          <b>使用前提：</b>{{ limitRequires }}
        </MiuixText>
      </div>

      <!-- 熔断告警 -->
      <div v-if="limitInfo?.tripped" class="notice notice--error">
        <MiuixText type="body2" color="var(--m-color-on-error-container)">
          连续失败已自动停用：{{ limitInfo.last_error ?? "未知原因" }}
        </MiuixText>
        <div class="actions">
          <MiuixButton
            class="actions__btn"
            type="primary"
            @click="onToggleLimit(true)"
          >
            重试
          </MiuixButton>
        </div>
      </div>
    </MiuixCard>
  </div>
</template>

<style scoped>
.page {
  padding: 12px 12px 16px;
  display: flex;
  flex-direction: column;
  gap: 12px;
}

/* ── 通用：卡片引导说明 ── */

/* `.lead` 的间距定义在 theme.css（四页共用同一套规则） */

/* ── 通用：标题行 ── */

.head {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: 12px;
}

.head__text {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 2px;
}

/* ── 通用：徽标 ── */

/* ── 卡片 1：模式 ── */

.modes {
  display: grid;
  grid-template-columns: 1fr 1fr;
  /*
    `grid-auto-rows: 1fr` —— 让**两行等高**

    需求（原代码注释就写了）："四块高度一致，网格才不会参差"。

    但只靠"描述最多两行"是达不到的 —— 那只是**上限**，不是相等。
    实测（375px 宽）：

    | 描述 | 行数 | 该行高度 |
    |---|---|---|
    | 「官方」完全交由系统管理，模块不插手 | 2 行 | **87px** |
    | 其余三条（≤12 字） | 1 行 | **68px** |

    ⇒ 上下两行差 19px，看起来像没对齐。

    `1fr` 在**自动高度**的网格里会让所有行等分到同一高度
    （取最高的那一行）。于是高度由**布局**保证，**不再依赖文案长度** ——
    将来谁改了描述文字，网格也不会忽然参差。

    ⚠️ 不要靠"把文案改短"来修 —— 那是把布局稳定性押在内容上。
  */
  grid-auto-rows: 1fr;
  gap: 10px;
  margin-top: 12px;
}

/*
  模式块：底色靠 miuix 的 `--m-card-color` 覆盖（内部 .m-card 会继承它），
  选中态再加一圈外描边 —— 描边不占布局，2×2 网格不会被撑歪。
*/
.mode {
  /*
    🔴 `display: flex` 是**必需的**，不能用 `height: 100%`（真机重叠事故）

    需求：四块即使描述行数不同，也要撑满各自网格行的高度。

    初版写的是内层 `height: 100%`，结果**四块互相重叠 14px**。原因在
    `box-sizing`：miuix 没给 `.m-card` 设 `border-box`，它是 `content-box`，
    于是 `height: 100%` 只约束**内容区**，内边距照样往外加：

    ```text
    网格行高 87px
      └─ .m-card  height: 100% ⇒ 内容 87px
                 + padding 12px × 2 = 24px
                 = 实际占 111px          ← 超出 24px
      下一行从 272px 开始，而它到 286px ⇒ **重叠 14px**
    ```

    改成 flex 后由**布局**决定高度，不依赖 `box-sizing`：

    | 机制 | 作用 |
    |---|---|
    | 网格项默认 `align-items: stretch` | 外层 `.mode` 撑满行高 |
    | flex 默认 `align-items: stretch` | 内层 `.m-card` 撑满外层高度 |
    | 内层 `flex: 1` | 宽度也撑满（否则只占内容宽） |

    ⇒ 高度由容器分配，永远不会超出。**百分比高度遇到 content-box
    就会有这个坑，能不用就不用。**
  */
  display: flex;
  --m-card-color: var(--m-color-surface-container-high);
  border-radius: var(--m-radius-md);
}

.mode--on {
  --m-card-color: var(--m-color-primary-container);
  --m-card-content-color: var(--m-color-on-primary-container);
  box-shadow: 0 0 0 2px var(--m-color-primary);
}

/* 小块不需要 16px 的内边距（全局 .m-card 给的），收紧到 12px */
.mode :deep(.m-card) {
  padding: 12px;
  flex: 1;
  min-width: 0;
}

.mode__top {
  display: flex;
  align-items: center;
  gap: 6px;
}

/* 选中标记：不依赖颜色也能看出来（色觉障碍友好） */
.mode__dot {
  flex: none;
  width: 7px;
  height: 7px;
  border-radius: 999px;
  background: var(--m-color-primary);
}

.mode__desc {
  margin-top: 2px;
  line-height: 1.45;
  /* 描述最多两行：四块高度一致，网格才不会参差 */
  display: -webkit-box;
  -webkit-line-clamp: 2;
  line-clamp: 2;
  -webkit-box-orient: vertical;
  overflow: hidden;
  /* 用透明度而不是固定灰色：选中时随内容色一起变深 */
  opacity: 0.72;
}

/* ── 通用：滑块区 ── */

.field {
  margin-top: 14px;
}

.field__head {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: 12px;
  margin-bottom: 4px;
}

.field__text {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 2px;
}

.slider {
  padding-top: 4px;
}

.scale {
  display: flex;
  justify-content: space-between;
  margin-top: 2px;
  /* 与滑块轨道端点对齐：miuix 的滑轨两端各留出滑块半径的内缩，
     不加这个内缩，两端文字会贴到卡片边缘 */
  padding: 0 12px;
}

/* ── 电流档位按钮 ── */

.chips {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
  margin: 8px 0 10px;
}

.chip--pick {
  border: 0;
  font-family: inherit;
  cursor: pointer;
  min-height: 32px;
  padding: 6px 12px;
}

.chip--pick:disabled {
  cursor: not-allowed;
  opacity: 0.5;
}

/* ── 通用：操作与提示 ── */

.gap {
  margin: 14px 0;
}

/*
 * ⚠️ `.row` / `.row__text`（"左边文字 + 右边控件"的行布局）**已移到
 *    `theme.css`** —— 日志页的「调试模式」开关也要用同一套。
 *
 * 留在本文件的 scoped 样式里时，别的页面用 `.row` 会**完全没有样式**：
 * 标题与说明文字挤在同一行，而且**不报任何错**（2026-10-17 真机截图发现）。
 */

/* 未启用时整块淡化，明确表达"当前不起作用" */
.is-dim {
  opacity: 0.55;
}

/* ── 卡片 4：熔断告警 ── */

.alert {
  margin-top: 14px;
  padding: 12px;
  border-radius: 12px;
  background: var(--m-color-error-container);
}
</style>
