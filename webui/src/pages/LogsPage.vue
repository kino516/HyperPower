<!--
  日志页 —— 事件流 + 每次充电的历史曲线

  # 这一页回答什么问题

  状态页看的是「此刻」，这一页看的是「过程」。从上到下：

  1. 汇总卡片：一共记录了几次、累计充进去多少电、历史最高温度
  2. 充电历史列表：每次充电一行；点开某一行，在它下面展开那一次的曲线
  3. 曲线图：温度与功率两条线叠在同一个时间轴上
  4. 最近事件：模块的状态变化与配置改动，都用人话写
  5. 危险操作：清空历史（必须二次确认）

  # 曲线图为什么用双 Y 轴，而不是上下两张小图

  这一页最想说明的因果关系是「温度一升上去，充电功率就被压下来了」。
  两条线必须叠在**同一个时间轴**上，因果才一眼可见。

  手机屏幕宽度有限（绘图区只有 230 像素上下），拆成两张独立小图后
  每张只剩几十像素的操作高度，曲线会被压成一条几乎水平的直线，
  读数也就没意义了。双轴能让两条线各自撑满整个高度。

  代价是两条线量纲不同（℃ 与 W），因此左右各一条刻度轴：
  左边温度（红），右边功率（主色）。曲线颜色与图例、读数条一一对应，
  看错轴的风险靠配色一致来消除。

  # 一个曾经踩过的坑（写在这里提醒后来者）

  本文件的 HTML 注释里**不能出现连续两个短横线**，也不能写 CSS 变量名 ——
  HTML 解析器遇到那种写法会把注释之后的标签一起吞掉，整个脚本块会消失，
  界面永远停在骨架屏，而且构建、类型检查全都不会报错。
  因此下面的说明一律用文字描述，代码里的变量名只出现在 script 段里。

  # 性能

  后端会把曲线抽稀到 1000 点以内，但一次会话仍可能有近千个点。
  这里只做**一遍线性遍历**生成两条折线的坐标串（见 buildChart），
  没有任何逐点两两比较；触摸定位用二分查找，与点数成对数关系。
-->

<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import {
  MiuixButton,
  MiuixCard,
  MiuixDialog,
  MiuixDivider,
  MiuixSmallTitle,
  MiuixSwitch,
  MiuixText,
  showSnackbar,
} from "miuix-vue";

import * as api from "../api";
import { duration, f0, f1 } from "../format";
import { perfMark } from "../perf";
import type {
  HistoryCurrent,
  HistoryListItem,
  HistoryListResult,
  HistorySession,
  HistoryStats,
  RuntimeEvent,
} from "../types";

// ============================================================
//  状态
// ============================================================

/** 历史列表（含汇总与「正在记录」那一条） */
const list = ref<HistoryListResult | null>(null);
const loading = ref(true);
/** 读取失败的提示（成功时为 null） */
const errMsg = ref<string | null>(null);

/** 最近事件（最新在前） */
const events = ref<RuntimeEvent[]>([]);

/** 诊断日志导出状态与结果路径 */
const exporting = ref(false);
const exportedPath = ref<string | null>(null);

/**
 * 导出结果的文件名与落点
 *
 * 分成两点信息是为了**如实**提示用户：
 *
 * | 情况 | 提示 |
 * |---|---|
 * | 复制到「下载」成功 | 「已保存到「下载」目录：<文件名>」 |
 * | 复制失败（回退到数据目录） | 给出完整路径并说明需要文件管理器/电脑查看 |
 *
 * ⚠️ 回退时必须说实话 —— 假装"已保存到下载"会让用户白翻一遍文件管理器。
 */
const exportedFile = computed(() => {
  const p = exportedPath.value;
  if (!p) {
    return null;
  }
  return {
    name: p.split("/").pop() ?? p,
    inDownload: p.startsWith("/sdcard/Download/"),
  };
});

/**
 * 「现在」的秒级时间戳
 *
 * 「5 分钟前」这类相对时间需要一个基准点。刷新时更新一次，
 * 不做秒级定时器 —— 事件条目的时间晚几秒刷新没人会察觉，
 * 而一个常驻定时器会在后台白耗电。
 */
const nowSec = ref(Math.floor(Date.now() / 1000));

/** 当前展开的会话 id；null = 全部收起 */
const openId = ref<string | null>(null);
/** 展开会话的曲线数据 */
const session = ref<HistorySession | null>(null);
const sessionLoading = ref(false);
const sessionError = ref<string | null>(null);

/** 清空**全部记录**的确认弹窗 */
const confirmClear = ref(false);
const clearing = ref(false);

/**
 * 弹窗里展示的「会清什么」/「不碰什么」
 *
 * ⚠️ 由后端现查给出（见 [`openClearConfirm`]），不在前端硬编码 ——
 *    前端写一份的话迟早与真正删除的那批漂移，
 *    而"弹窗说清了 A、实际只清了 B"会让用户**以为清干净了**。
 */
const clearScope = ref<string[]>([]);
const clearKept = ref<string[]>([]);

const stats = computed<HistoryStats | null>(() => list.value?.stats ?? null);
const sessions = computed<HistoryListItem[]>(() => list.value?.sessions ?? []);
const current = computed<HistoryCurrent | null>(() => list.value?.current ?? null);

/**
 * 充电历史默认显示的条数
 *
 * 🔴 用户要求（2026-10-17）：
 * 「每次充电的记录只显示最近 3 次，如果需要看更多需要点击更多查看」
 *
 * # 为什么是 3 而不是更多
 *
 * 每条记录**展开后是一张曲线图**（几十个点 + 两条折线 + 读数条），
 * 比一条文字事件重得多。一次铺开十几条会让这一页长得没法看，
 * 而用户真正常看的永远是最新那几次。
 *
 * 与「最近事件」的 [`EVENTS_PREVIEW`]（10 条）取不同值是有意的：
 * 事件是一行文字，历史是一条图 —— **同一个数字套两种粒度并不合适**。
 */
const HISTORY_PREVIEW = 3;

/** 是否展开了全部充电记录 */
const showAllSessions = ref(false);

/**
 * 实际渲染的记录列表（未展开时只给前 N 条）
 *
 * ⚠️ 展开中的那一条如果在折叠区里，`openId` 仍然有效 ——
 *    收起时不会清掉展开状态，再展开回来曲线还在。
 *    （不做"收起时自动折叠"是因为那会让用户的手指在图上时突然消失。）
 */
const visibleSessions = computed(() =>
  showAllSessions.value
    ? sessions.value
    : sessions.value.slice(0, HISTORY_PREVIEW),
);

/** 被折叠掉的条数（>0 才显示「更多」按钮） */
const hiddenSessionCount = computed(() =>
  Math.max(0, sessions.value.length - HISTORY_PREVIEW),
);

// ⚠️ 这里原有 `nothingToClear`（"没有可清空的历史"）—— 已删除
//
// 它只服务于旧的「清空充电历史」按钮的禁用态。改成「清空全部记录」后
// **没有"无事可做"的状态了**：即使一次充电记录都没有，事件与日志文件
// 仍然存在，清空依然有意义。硬留一个永远为 false 的判断只会误导读者。

// ============================================================
//  数据加载
// ============================================================

/**
 * 拉取列表与事件
 *
 * 两条命令并发发出，但**只有列表是关键路径**：事件拉不到不该让整页报错，
 * 这与 `stores/sys.ts` 里的取舍保持一致。
 */
async function load(): Promise<void> {
  loading.value = true;
  nowSec.value = Math.floor(Date.now() / 1000);

  const listP = api.fetchHistoryList().then(
    (v) => {
      list.value = v;
      errMsg.value = null;
    },
    (e: unknown) => {
      errMsg.value = e instanceof Error ? e.message : String(e);
    },
  );

  const eventsP = api.fetchEvents(30).then(
    (v) => {
      // 后端按时间正序返回，界面上最新在前更好读
      events.value = [...v].reverse();
    },
    () => {
      /* 忽略：事件不是关键路径 */
    },
  );

  await Promise.all([listP, eventsP]);
  // 调试状态单独读：它失败不该让整页报错（与事件同一条纪律）
  await loadDebug();
  loading.value = false;
  perfMark("loaded:logs");
}

onMounted(() => {
  void load();
});

/**
 * 展开 / 收起某一次充电
 *
 * 点击已展开的那一行即收起。切换行时清掉上一次的曲线，
 * 避免新的还没到、旧的在下面闪一下。
 */
async function toggle(id: string): Promise<void> {
  if (openId.value === id) {
    openId.value = null;
    session.value = null;
    probe.value = null;
    return;
  }

  openId.value = id;
  session.value = null;
  sessionError.value = null;
  probe.value = null;
  sessionLoading.value = true;

  try {
    const s = await api.fetchHistorySession(id);
    // 命令是异步的：回来时用户可能已经点了别的行，过期结果直接丢掉
    if (openId.value === id) {
      session.value = s;
    }
  } catch (e) {
    if (openId.value === id) {
      sessionError.value = e instanceof Error ? e.message : String(e);
    }
  } finally {
    if (openId.value === id) {
      sessionLoading.value = false;
    }
  }
}

/**
 * 清空**全部记录**（已确认后调用）
 *
 * 🔴 用户要求（2026-10-17）：「清空充电历史记录好像只能清空历史记录，
 * 加入更多功能，需要清空所有的临时文件，包括最近事件等等」。
 *
 * 覆盖：最近事件 · 充电历史 · 诊断报告 · 界面打点日志 · 开机记录。
 * **不碰**：配置、设备档案、预设、进程锁、控制通道。
 *
 * ⚠️ 清单**由后端给出**（`recordsScope()`），前端只负责显示 ——
 *    前端自己写一份的话，迟早与真正删除的那批不一致，
 *    而后果是"弹窗说清了 A，实际只清了 B"，用户却以为清干净了。
 */
async function doClear(): Promise<void> {
  if (clearing.value) {
    return;
  }
  clearing.value = true;
  try {
    await api.clearRecords();
    confirmClear.value = false;
    openId.value = null;
    session.value = null;
    // 重新拉一次：历史、事件、统计全都变了
    await load();
    void showSnackbar({ message: "已清空全部记录", duration: "short" });
  } catch (e) {
    void showSnackbar({
      message: `清空失败：${e instanceof Error ? e.message : String(e)}`,
      duration: "long",
    });
  } finally {
    clearing.value = false;
  }
}

/**
 * 打开确认弹窗时**现查**一遍清单
 *
 * ⚠️ 不用前端硬编码的文案，也不缓存 —— 后端的清单是唯一真源。
 *    查不到时弹窗里就不显示清单（而不是显示一份可能过时的）。
 */
async function openClearConfirm(): Promise<void> {
  confirmClear.value = true;
  try {
    const r = await api.recordsScope();
    clearScope.value = Array.isArray(r.scope) ? (r.scope as string[]) : [];
    clearKept.value = Array.isArray(r.kept) ? (r.kept as string[]) : [];
  } catch {
    // 查不到就不显示清单 —— 宁可少说，也不说一份可能不对的
    clearScope.value = [];
    clearKept.value = [];
  }
}

/**
 * 写操作之后等待后端落盘的时长（毫秒）
 *
 * # 为什么必须有这个等待（与 ControlPage 同一条纪律）
 *
 * 后端所有写都是**入队**的：`set` / `debug on` 等命令返回 `ok` 时，
 * 改动还只是躺在队列里，由 worker 在**本拍结束时**统一应用
 * （后端 `engine::apply_pending`）。此时立刻回读会读到**旧值** ——
 * 界面看起来就是"点了没反应"。
 *
 * 有客户端在等时 worker 的步进下限是 150 ms，这里留 4 倍余量。
 */
const SETTLE_MS = 600;

/** 等一会儿（写操作之后给后端落盘的时间） */
function wait(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

/**
 * 调试模式（用户要求 2026-10-17）
 *
 * # 为什么状态要**从后端读**而不是本地记
 *
 * 开关真正的作用是改配置键 `debug.enabled`，而写入器由 **worker** 打开。
 * 如果前端只记本地状态，一旦在别处（CLI、另一个页面）改了配置，
 * 这里显示的就会是错的。所以每次 `load()` 都回读一次。
 */
const debugStatus = ref<api.DebugStatus | null>(null);
const debugBusy = ref(false);

/** 开关的显示值（后端说了算） */
const debugOn = computed(() => debugStatus.value?.enabled ?? false);

/** 读一次调试状态（失败就保持上一次的值，不打断整页） */
async function loadDebug(): Promise<void> {
  try {
    debugStatus.value = await api.fetchDebugStatus();
  } catch {
    // 读不到就不显示 —— 不给用户一个可能过时的开关状态
  }
}

/**
 * 切换调试模式
 *
 * ⚠️ `on` 之后要**等一拍**才真的开始记（写入器由 worker 在下一拍打开）——
 *    这里 sleep 一下再回读，否则用户会看到"开了但显示 0 字节"而以为没生效。
 */
async function onToggleDebug(next: boolean): Promise<void> {
  if (debugBusy.value) {
    return;
  }
  debugBusy.value = true;
  try {
    await api.setDebugEnabled(next);
    await wait(SETTLE_MS);
    await loadDebug();
    void showSnackbar({
      message: next
        ? "调试模式已开启，现在复现问题即可"
        : "调试模式已关闭，调试数据保留在数据目录里",
      duration: "long",
    });
  } catch (e) {
    void showSnackbar({
      message: `切换失败：${e instanceof Error ? e.message : String(e)}`,
      duration: "long",
    });
    // 失败就回读真实状态，不要停在"看起来切成功了"的假象上
    await loadDebug();
  } finally {
    debugBusy.value = false;
  }
}

/** 字节数转人话（界面显示"已记录多少"用） */
function fmtBytes(n: number): string {
  if (n < 1024) {
    return `${n} B`;
  }
  if (n < 1024 * 1024) {
    return `${f1(n / 1024)} KB`;
  }
  return `${f1(n / 1024 / 1024)} MB`;
}

/**
 * 导出诊断日志（反馈问题时用）
 *
 * 报告落在**「下载」目录**（用户能在文件管理器里找到并直接分享），
 * 文件名带时间戳，多次导出不会互相覆盖。
 *
 * 若调试模式开着，会**另存**一份 `HyperPower-debug-*.jsonl` ——
 * 调试数据有几十 MB，塞进报告本体既发不出去也打不开。
 */
async function doExport(): Promise<void> {
  if (exporting.value) {
    return;
  }
  exporting.value = true;
  try {
    exportedPath.value = await api.exportDiagnosticLog();
    void showSnackbar({
      message: "诊断日志已导出到「下载」目录，把它发给维护者即可",
      duration: "long",
    });
  } catch (e) {
    void showSnackbar({
      message: `导出失败：${e instanceof Error ? e.message : String(e)}`,
      duration: "long",
    });
  } finally {
    exporting.value = false;
  }
}

// ============================================================
//  时间与数值的人话化
//
//  format.ts 里已有 hhmmss / duration / f0 / f1 —— 能复用的都复用了。
//  这里补两个它没有的：绝对日期，以及「刚刚 / 5 分钟前」这种相对时间。
// ============================================================

function pad2(n: number): string {
  return String(n).padStart(2, "0");
}

/**
 * 取某时刻所在那一天的**本地零点**（毫秒）
 *
 * 「隔了几天」必须按日历日算，不能用 `(t1 - t2) / 86400000` ——
 * 那样算出来的是「间隔不到 24 小时」，不是「同一天」。
 */
function dayStart(ms: number): number {
  const d = new Date(ms);
  return new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
}

/**
 * 绝对时间 → 「10月13日 20:15」
 *
 * 跨年时补上年份，否则「1月3日」会让人分不清是哪一年。
 */
function absTime(ts: number): string {
  if (!Number.isFinite(ts) || ts <= 0) {
    return "未知时间";
  }
  const d = new Date(ts * 1000);
  const hm = `${pad2(d.getHours())}:${pad2(d.getMinutes())}`;
  const md = `${d.getMonth() + 1}月${d.getDate()}日`;
  const nowY = new Date(nowSec.value * 1000).getFullYear();
  return d.getFullYear() === nowY ? `${md} ${hm}` : `${d.getFullYear()}年${md} ${hm}`;
}

/**
 * 事件时间 → 易读相对时间
 *
 * 越新的事件越关心「多久之前」，久远的则只需要知道是哪天：
 * 一小时以内精确到分钟，昨天单独说，再往前给日期。
 */
function relTime(ts: number): string {
  if (!Number.isFinite(ts) || ts <= 0) {
    return "未知时间";
  }
  const diff = nowSec.value - ts;
  if (diff < 45) {
    return "刚刚";
  }
  if (diff < 3600) {
    return `${Math.floor(diff / 60)} 分钟前`;
  }
  if (diff < 86400) {
    return `${Math.floor(diff / 3600)} 小时前`;
  }
  const d = new Date(ts * 1000);
  const days = Math.round(
    (dayStart(nowSec.value * 1000) - dayStart(ts * 1000)) / 86400000,
  );
  if (days === 1) {
    return `昨天 ${pad2(d.getHours())}:${pad2(d.getMinutes())}`;
  }
  return absTime(ts);
}

/** 分钟数 → 横轴短标签（超过 10 分钟不显示小数） */
function minLabel(min: number): string {
  if (!Number.isFinite(min) || min < 0) {
    return "--";
  }
  return min >= 10 ? f0(min) : f1(min);
}

/** 采样点里的阶段 → 中文（注意历史点用的是带下划线的写法） */
function phaseLabel(phase: string): string {
  switch (phase) {
    case "normal":
      return "正常放行";
    case "tightening":
      return "正在限速";
    case "hard_protect":
    case "hardprotect":
      return "硬保护";
    case "idle":
      return "待机";
    default:
      return phase || "未知";
  }
}

/**
 * 事件类型 → 中文徽标文字；没见过的类型直接显示原文，不吞信息
 *
 * ⚠️ 必须与后端 `observability.rs` 里的 13 个 `kind` 常量**一一对应**。
 *    初版只翻译了 7 个，于是徽标上冒出 `lifecycle`、`detect` 这样的英文 ——
 *    用户要求的正是「英文显示全部转换成中文」。
 *
 *    漏翻译时不会报错（回退成原文），所以**加新 kind 时要回来同步这里**。
 */
const KIND_LABEL: Record<string, string> = {
  // 状态与相位
  state: "状态",
  hard_protect: "硬保护",
  // 配置
  config: "配置",
  config_clamped: "配置钳位",
  // 读写异常
  write_fail: "写入失败",
  read_fail: "读取失败",
  crash: "崩溃",
  // 进程与检测
  lifecycle: "运行",
  detect: "检测",
  deny: "拒绝",
  // 交回官方
  release: "交回官方",
  // 附加功能
  addon: "附加功能",
  addon_trip: "功能熔断",
};

function kindLabel(kind: string): string {
  return KIND_LABEL[kind] ?? kind;
}

/**
 * 事件消息 → 用户能看懂的文字
 *
 * # 为什么需要兜底翻译（2026-10-16 用户反馈）
 *
 * 用户原话：「最近事件内的英文显示全部转换成中文」。
 *
 * 根因在后端：状态迁移与模式切换的消息原先用了 Rust 的 `{:?}`（Debug 格式），
 * 于是界面上显示的是 `Normal → Tightening`、`模式切换：custom → conservative`。
 * **后端已修**（`Phase::label()` / `mode_label()`），但有两个遗留问题：
 *
 * | 问题 | 说明 |
 * |---|---|
 * | **历史事件** | `events.jsonl` 里已经存了英文消息，改后端不会追溯修改它们 |
 * | 升级过渡期 | 用户装上新版前产生的事件仍是英文 |
 *
 * ⇒ 这里做一层**兜底翻译**：遇到英文形态就翻成中文，已经是中文的原样返回。
 *
 * ⚠️ 翻译是**幂等**的 —— 中文消息不含这些英文词，正则不会命中，
 *    因此新老事件混在一起也不会翻错。
 */
/**
 * 旧版事件消息里的**英文术语 → 中文**
 *
 * # 为什么需要这张表
 *
 * 0.3.8 及更早的版本，事件消息里有几个英文术语：
 *
 * | 原文 | 用户看不懂的地方 |
 * |---|---|
 * | `worker 启动` / `worker 退出` | `worker` 是内部进程名 |
 * | `supervisor 启动` | 同上 |
 * | `uevent 订阅成功` | `uevent` 是内核机制名 |
 * | `soc_limit 探测：命中 …` | `soc_limit` 是内部功能 id（界面叫「充电上限」） |
 *
 * 后端已改为直接发中文，但这张表仍然必要 ——
 * **`events.jsonl` 里已经存了英文消息**，改后端不会追溯修改历史数据。
 * 用户升级后仍会看到旧事件里的英文。
 *
 * ⚠️ 这是**过渡期兜底**，等旧事件被日志轮转淘汰后即可删除。
 *    判定依据：所有位于 `data.ts` 之前的事件都会被 `logs clear` 或轮转清掉。
 *
 * ⚠️ 用**术语替换**而不是整句匹配：一句里可能同时出现多个术语
 *    （如「交回官方：worker 退出」），整句匹配会漏。
 *    `\b` 词边界避免误伤包含这些字母的其它词。
 *
 * ⚠️ **不动节点路径**（`/sys/class/...`）—— 那是文件系统路径，
 *    翻译了反而让排障的人对不上。
 */
const LEGACY_TERMS: [RegExp, string][] = [
  [/\bworker\b/g, "充电控制进程"],
  [/\bsupervisor\b/g, "监督进程"],
  [/uevent 订阅/g, "充电事件订阅"],
  [/\bsoc_limit\b/g, "充电上限"],
];

function eventMsg(msg: string): string {
  // ① 状态迁移：`Idle → Normal` / `HardProtect → Normal`（可含空格）
  const m = msg.match(/^([A-Za-z_]+)\s*→\s*([A-Za-z_]+)$/);
  if (m) {
    // ⚠️ 捕获组在 TS 里推成 `string | undefined`（正则可能不匹配）——
    //    这里已经过了 `if (m)`，但类型系统不知道捕获组一定存在，
    //    因此用 `?? ""` 兜底；空串交给 phaseLabel 走它的 default 分支。
    return `${phaseLabel((m[1] ?? "").toLowerCase())} → ${phaseLabel(
      (m[2] ?? "").toLowerCase(),
    )}`;
  }

  // ② 模式切换：`模式切换：custom → conservative`
  const mm = msg.match(/^(模式切换：)([a-z]+)\s*→\s*([a-z]+)$/);
  if (mm) {
    return `${mm[1] ?? ""}${modeLabel(mm[2] ?? "")} → ${modeLabel(mm[3] ?? "")}`;
  }

  // ③ 旧版消息里的英文术语（历史数据兜底）
  let out = msg;
  for (const [re, zh] of LEGACY_TERMS) {
    out = out.replace(re, zh);
  }
  // ④ 收拾替换后留下的多余空格
  //
  // `worker 启动` → 替换 `worker` 后成了「充电控制进程 启动」——
  // 中文之间不该有空格。只收拾**两侧都是汉字**的那些空格，
  // 因此「温度 38℃」这类（汉字+数字）不受影响。
  //
  // ⚠️ 用到了 lookbehind（ES2018）。项目支持基线是 Android 12+ 的系统
  //    WebView，实测版本 154，支持没有问题。
  return out.replace(/(?<=[\u4e00-\u9fa5]) (?=[\u4e00-\u9fa5])/g, "");
}

/** 模式 id → 中文名（与后端 `config_store::mode_label` 保持一致） */
function modeLabel(id: string): string {
  switch (id) {
    case "conservative":
      return "保守";
    case "official":
      return "官方";
    case "turbo":
      return "极速";
    case "custom":
      return "自定义";
    default:
      return id;
  }
}

/**
 * 最近事件默认显示条数
 *
 * 🔴 用户要求（2026-10-16）：
 * 「最近事件一次性显示太多，默认显示最近 10 条，如果需要看更多需要点击更多查看」
 *
 * 「最近事件」的定位是**扫一眼有没有异常**，不是逐条阅读 ——
 * 一次铺开几十条会把「充电历史」「每次充电」这些更需要关注的内容挤出视野。
 */
const EVENTS_PREVIEW = 10;

/** 是否展开了全部事件 */
const showAllEvents = ref(false);

/** 实际渲染的事件列表（未展开时只给前 N 条） */
const visibleEvents = computed(() =>
  showAllEvents.value
    ? events.value
    : events.value.slice(0, EVENTS_PREVIEW),
);

/** 被折叠掉的条数（>0 才显示「更多」按钮） */
const hiddenEventCount = computed(() =>
  Math.max(0, events.value.length - EVENTS_PREVIEW),
);

/** 事件类型 → 徽标配色（出错类用错误色，其余按类别区分） */
function kindClass(kind: string): string {
  switch (kind) {
    // 出错类 → 错误色（最需要被一眼看到）
    case "crash":
    case "write_fail":
    case "read_fail":
    case "addon_trip":
    case "deny":
      return "evt__badge--error";
    // 硬保护也是"发生了危险情况"，用错误色
    case "hard_protect":
      return "evt__badge--error";
    case "state":
      return "evt__badge--state";
    // 配置钳位意味着"用户设的值被改了"，用配置色但值得注意
    case "config":
    case "config_clamped":
      return "evt__badge--config";
    case "release":
      return "evt__badge--release";
    case "addon":
      return "evt__badge--addon";
    default:
      return "evt__badge--muted";
  }
}

// ============================================================
//  曲线图
//
//  没有任何图表库：这里是手写 SVG 折线。整个 WebUI 只有三百多 KB，
//  引一个图表库的体积代价远大于这几十行代码。
//
//  坐标系：viewBox 固定 1000 × 100，配合 preserveAspectRatio="none"
//  让图随容器宽度伸缩。**非等比缩放会把圆压成椭圆、把文字拉变形**，
//  所以：
//    · 线条一律加 vector-effect（描边宽度不随缩放变化）
//    · 圆点与文字都用 HTML 覆盖层画，不放进 SVG
// ============================================================

/** 绘图区宽度（viewBox 单位）—— 横向是任意单位，靠拉伸适配容器 */
const CW = 1000;
/** 绘图区高度（viewBox 单位）—— 取 100，纵坐标的值即百分比 */
const CH = 100;

/** 纵向参考线位置（viewBox 单位）：1/4、1/2、3/4 */
const GRID_X = [250, 500, 750];

/** 一条刻度：值对应的纵向位置 + 显示文字 */
interface AxisTick {
  y: number;
  label: string;
}

interface ChartModel {
  /** 点数 */
  count: number;
  /** 每个点的时间戳（升序，供二分查找） */
  times: number[];
  /** 每个点的横向位置（viewBox 单位） */
  xs: number[];
  /** 每个点的纵向位置（viewBox 单位） */
  tempYs: number[];
  powerYs: number[];
  /** 折线的 points 属性 */
  tempPoints: string;
  powerPoints: string;
  /** 左右两条刻度轴 */
  tempGrid: AxisTick[];
  powerGrid: AxisTick[];
  /** 会话时间跨度：起点与总秒数（触摸定位用） */
  t0: number;
  span: number;
  /** 总时长（分钟，横轴用） */
  totalMin: number;
}

/** 数值 → 纵向位置（viewBox 单位） */
function yOf(v: number, top: number, bottom: number): number {
  const y = ((top - v) / (top - bottom)) * CH;
  return Math.min(CH, Math.max(0, y));
}

/** viewBox 横向坐标 → 百分比（HTML 覆盖层用） */
function xPct(x: number): string {
  return `${((x / CW) * 100).toFixed(2)}%`;
}

/** viewBox 纵向坐标 → 百分比（HTML 覆盖层用） */
function yPct(y: number): string {
  return `${((y / CH) * 100).toFixed(2)}%`;
}

/**
 * 生成绘图模型
 *
 * 全程一遍遍历，O(n)。纵轴量程的取法：
 *
 * | 轴 | 取法 | 为什么 |
 * |---|---|---|
 * | 温度 | 以实际范围为中心，上下各留 1℃ | 电池温度永远在 30~50℃ 之间，从 0 起画就是一条直线 |
 * | 功率 | 从 0 起，上界留 8% 余量并取整到 5W 的倍数 | 0W 对功率是有意义的基准 |
 */
function buildChart(s: HistorySession): ChartModel | null {
  const pts = s.points;
  const n = pts.length;
  const first = pts[0];
  const last = pts[n - 1];
  if (n === 0 || !first || !last) {
    return null;
  }

  // ── 时间轴 ──
  const t0 = first.t;
  const span = Math.max(1, last.t - t0);

  // ── 两条曲线各自的量程 ──
  let tLow = first.temp_c;
  let tHigh = first.temp_c;
  let pHigh = first.power_w;
  for (const p of pts) {
    if (p.temp_c < tLow) tLow = p.temp_c;
    if (p.temp_c > tHigh) tHigh = p.temp_c;
    if (p.power_w > pHigh) pHigh = p.power_w;
  }

  const tSpan = Math.max(2, Math.ceil(tHigh - tLow) + 2);
  const tBot = Math.floor((tLow + tHigh) / 2 - tSpan / 2);
  const tTop = tBot + tSpan;

  const pBot = 0;
  const pTop = Math.max(5, Math.ceil((pHigh * 1.08) / 5) * 5);

  // ── 逐点换算（一遍遍历） ──
  const times: number[] = [];
  const xs: number[] = [];
  const tempYs: number[] = [];
  const powerYs: number[] = [];
  const tempCoord: string[] = [];
  const powerCoord: string[] = [];

  for (const p of pts) {
    const x = ((p.t - t0) / span) * CW;
    const ty = yOf(p.temp_c, tTop, tBot);
    const py = yOf(p.power_w, pTop, pBot);
    times.push(p.t);
    xs.push(x);
    tempYs.push(ty);
    powerYs.push(py);
    tempCoord.push(`${x.toFixed(1)},${ty.toFixed(1)}`);
    powerCoord.push(`${x.toFixed(1)},${py.toFixed(1)}`);
  }

  // 中间刻度的值取整后再算位置 —— 保证「刻度线压在哪」与「标签写什么」一致
  const tMid = Math.round((tTop + tBot) / 2);
  const pMid = Math.round(pTop / 2);

  return {
    count: n,
    times,
    xs,
    tempYs,
    powerYs,
    tempPoints: tempCoord.join(" "),
    powerPoints: powerCoord.join(" "),
    tempGrid: [
      { y: yOf(tTop, tTop, tBot), label: `${f0(tTop)}℃` },
      { y: yOf(tMid, tTop, tBot), label: f0(tMid) },
      { y: yOf(tBot, tTop, tBot), label: f0(tBot) },
    ],
    powerGrid: [
      { y: yOf(pTop, pTop, pBot), label: `${f0(pTop)}W` },
      { y: yOf(pMid, pTop, pBot), label: f0(pMid) },
      { y: yOf(pBot, pTop, pBot), label: f0(pBot) },
    ],
    t0,
    span,
    totalMin: span / 60,
  };
}

/** 当前展开会话的绘图模型；没有数据时为 null */
const chart = computed<ChartModel | null>(() => {
  const s = session.value;
  return s ? buildChart(s) : null;
});

/** 采样间隔（秒），显示在图表右上角 */
const sampleS = computed(() => session.value?.sample_s ?? 0);

/** 横轴中间与末尾的标签 */
const xMid = computed(() => `${minLabel((chart.value?.totalMin ?? 0) / 2)} 分`);
const xEnd = computed(() => `${minLabel(chart.value?.totalMin ?? 0)} 分`);

// ── 触摸 / 鼠标查看某一点 ──

/** 正在查看的点下标；null = 没有选点 */
const probe = ref<number | null>(null);

/** 是否正在按住（按住时手指滑动，读数要跟着走） */
let pressing = false;

/**
 * 按时间找最接近的点（二分）
 *
 * 不能用「横向比例 × 下标」直接换算：后端抽稀后采样间隔会变，
 * 点位在时间轴上并不均匀。
 */
function nearestIndex(times: number[], t: number): number {
  let lo = 0;
  let hi = times.length - 1;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    const mv = times[mid];
    if (mv === undefined || mv < t) {
      lo = mid + 1;
    } else {
      hi = mid;
    }
  }
  const cur = times[lo];
  const prev = lo > 0 ? times[lo - 1] : undefined;
  if (cur === undefined) {
    return lo;
  }
  if (prev !== undefined && Math.abs(t - prev) <= Math.abs(cur - t)) {
    return lo - 1;
  }
  return lo;
}

/**
 * 把指针位置换算成最近的点
 *
 * ⚠️ 元素从 `ev.currentTarget` 取，**不要用模板 ref**。
 *
 * 曲线块在 `v-for` 内部，Vue 对「循环里的模板引用」走的是另一套收集逻辑：
 * `setup` 里那个 ref 不会被赋值（实测它连 `setupState` 都没进），
 * 于是这里永远拿到 null、按住图毫无反应 —— 而且**不报任何错**。
 * 监听器本来就绑在这个元素上，同步读取 `currentTarget` 绝对可靠。
 */
function pickAt(ev: PointerEvent): void {
  const el = ev.currentTarget as HTMLElement | null;
  const m = chart.value;
  if (!el || !m || m.count === 0) {
    return;
  }
  const rect = el.getBoundingClientRect();
  if (rect.width <= 0) {
    return;
  }
  const frac = Math.min(1, Math.max(0, (ev.clientX - rect.left) / rect.width));
  probe.value = nearestIndex(m.times, m.t0 + frac * m.span);
}

function onPointerDown(ev: PointerEvent): void {
  pressing = true;
  pickAt(ev);
}

function onPointerMove(ev: PointerEvent): void {
  // 只跟随「按着」的指针：手指滑动、鼠标按住拖动都会命中
  if (pressing) {
    pickAt(ev);
  }
}

function onPointerUp(ev: PointerEvent): void {
  if (!pressing) {
    return;
  }
  pressing = false;
  pickAt(ev);
}

function onPointerCancel(): void {
  pressing = false;
}

/**
 * 指针离开绘图区
 *
 * · 鼠标移开：复位读数（不按着的时候不该一直留着）
 * · 手指抬起后也会收到一次离开事件，那一次要**保留**读数 ——
 *   否则点一下什么都看不见
 * · 顺便清掉按压标记：按住后滑出图外再松手是收不到抬起事件的，
 *   不清理的话之后鼠标随便一晃都会改读数
 */
function onPointerLeave(ev: PointerEvent): void {
  pressing = false;
  if (ev.pointerType === "mouse") {
    probe.value = null;
  }
}

/** 选中点的纵向准线位置；未选中时为 null */
const cursorX = computed<number | null>(() => {
  const m = chart.value;
  const i = probe.value;
  if (!m || i === null) {
    return null;
  }
  return m.xs[i] ?? null;
});

interface Marker {
  key: string;
  cls: string;
  style: string;
}

/**
 * 图上的圆点
 *
 * · 每条曲线的**结束点**各一个：这是「当前值」在图上对应的位置
 * · 正在查看的点再叠一对空心点（与结束点重合时不重复画）
 */
const markers = computed<Marker[]>(() => {
  const m = chart.value;
  if (!m || m.count === 0) {
    return [];
  }
  const last = m.count - 1;
  const ex = m.xs[last];
  const et = m.tempYs[last];
  const ep = m.powerYs[last];
  const out: Marker[] = [];
  if (ex !== undefined && et !== undefined && ep !== undefined) {
    out.push({
      key: "end-temp",
      cls: "ch__dot--temp",
      style: `left:${xPct(ex)};top:${yPct(et)}`,
    });
    out.push({
      key: "end-power",
      cls: "ch__dot--power",
      style: `left:${xPct(ex)};top:${yPct(ep)}`,
    });
  }

  const i = probe.value;
  if (i !== null && i !== last) {
    const x = m.xs[i];
    const ty = m.tempYs[i];
    const py = m.powerYs[i];
    if (x !== undefined && ty !== undefined && py !== undefined) {
      const pos = `left:${xPct(x)};`;
      out.push({ key: "at-temp", cls: "ch__dot--temp ch__dot--at", style: `${pos}top:${yPct(ty)}` });
      out.push({ key: "at-power", cls: "ch__dot--power ch__dot--at", style: `${pos}top:${yPct(py)}` });
    }
  }
  return out;
});

/**
 * 读数条
 *
 * 没有选点时就显示结束那一刻的值（这就是图上两条线的落点），
 * 选了点就显示那一刻的值 —— 一根手指就能看清任意时刻的状态。
 */
interface Readout {
  head: string;
  temp: string;
  power: string;
  soc: string;
}

const readout = computed<Readout | null>(() => {
  const m = chart.value;
  const s = session.value;
  if (!m || !s || m.count === 0) {
    return null;
  }
  const i = probe.value;
  const idx = i === null ? m.count - 1 : i;
  const p = s.points[idx];
  const first = s.points[0];
  if (!p) {
    return null;
  }
  const minute = first ? (p.t - first.t) / 60 : 0;
  return {
    head:
      i === null
        ? `结束 · ${phaseLabel(p.phase)}`
        : `${minLabel(minute)} 分 · ${phaseLabel(p.phase)}`,
    temp: f1(p.temp_c),
    power: f1(p.power_w),
    soc: f0(p.soc),
  };
});
</script>

<template>
  <div class="page">
    <!-- ── 首屏占位：拿不到数据时不显示假数字 ── -->
    <MiuixCard v-if="!list" class="hero">
      <MiuixText type="title4">
        {{ loading ? "正在读取充电历史…" : "读取充电历史失败" }}
      </MiuixText>
      <MiuixText
        type="footnote1"
        color="var(--m-color-on-surface-secondary)"
        class="hero__hint"
      >
        {{
          loading
            ? "若长时间无响应，请检查模块是否已启动"
            : "可以稍后再试，或确认模块是否正常运行"
        }}
      </MiuixText>
      <MiuixText
        v-if="errMsg"
        type="footnote1"
        color="var(--m-color-error)"
        class="hero__hint"
      >
        {{ errMsg }}
      </MiuixText>
      <MiuixButton v-if="!loading" class="hero__retry" @click="load">
        重试
      </MiuixButton>
    </MiuixCard>

    <template v-else>
      <!--
        ── 汇总 ──

        ⚠️ 标题必须放在**卡片外面**，且与卡片内文字同一条竖线

        初版把「充电历史」写在卡片内部，字号是 `body1`（16px / 常规粗细）。
        而分组标题（`MiuixSmallTitle`）实际是 **14px / 加粗** ——
        于是卡内标题**比分组标题还大**，层级是倒的：

        | 位置 | 修正前 | 修正后 |
        |---|---|---|
        | 卡内标题「充电历史」 | 16px / 常规 ← 过大 | 14px / 加粗 ✓ |
        | 分组标题「每次充电」 | 14px / 加粗 | 14px / 加粗 ✓ |

        现在改成标准的「分组标题 + 右侧操作」结构：标题与「刷新」
        同处一行，标题左边缘与卡内文字对齐（都是 28px），
        「刷新」右边缘与卡内文字对齐。四页的标题体系至此完全一致。
      -->
      <div class="sec">
        <MiuixSmallTitle text="充电历史" class="sec__title" />
        <MiuixButton class="sec__action" :disabled="loading" @click="load">
          刷新
        </MiuixButton>
      </div>

      <MiuixCard>
        <MiuixText
          type="footnote1"
          color="var(--m-color-on-surface-secondary)"
          class="lead"
        >
          每次插上充电器都会自动记一次
        </MiuixText>

        <div v-if="stats" class="grid">
          <div class="grid__cell">
            <MiuixText type="title3">{{ f0(stats.count) }}</MiuixText>
            <MiuixText type="footnote2" color="var(--m-color-on-surface-secondary)">
              记录次数
            </MiuixText>
          </div>
          <div class="grid__cell">
            <MiuixText type="title3">{{ f0(stats.energy_total_wh) }}</MiuixText>
            <MiuixText type="footnote2" color="var(--m-color-on-surface-secondary)">
              累计充入 Wh
            </MiuixText>
          </div>
          <div class="grid__cell">
            <MiuixText type="title3">{{ f1(stats.peak_temp_c) }}</MiuixText>
            <MiuixText type="footnote2" color="var(--m-color-on-surface-secondary)">
              历史最高 ℃
            </MiuixText>
          </div>
        </div>

        <MiuixText
          v-if="stats"
          type="footnote2"
          color="var(--m-color-on-surface-secondary)"
          class="sum__foot"
        >
          每 {{ f0(stats.sample_s) }} 秒采一个点，最多保留 {{ f0(stats.keep) }} 次
        </MiuixText>

        <!-- 记录被关掉时必须说清楚：不是坏了，是开关关了 -->
        <div v-if="stats && !stats.enabled" class="notice notice--error">
          <MiuixText type="footnote1" color="var(--m-color-on-error-container)">
            充电历史记录已关闭 —— 现在充电不会留下任何记录，
            已有的历史仍然可以查看。
          </MiuixText>
        </div>
      </MiuixCard>

      <!-- ── 充电历史列表 ── -->
      <MiuixSmallTitle text="每次充电" />
      <MiuixCard class="hcard">
        <!-- 正在记录的这一次：还没归档，所以只给个摘要，不与下面的历史混排 -->
        <div v-if="current" class="now">
          <div class="now__head">
            <span class="tag tag--live">
              <i class="tag__dot" />
              正在记录
            </span>
            <MiuixText type="footnote2" color="var(--m-color-on-primary-container)">
              已充入 {{ f1(current.energy_wh) }} Wh · 已用 {{ duration(current.duration_s) }}
            </MiuixText>
          </div>
          <MiuixText type="footnote2" color="var(--m-color-on-primary-container)">
            峰值 {{ f1(current.peak_temp_c) }} ℃ · 平均 {{ f1(current.avg_power_w) }} W ·
            本次充完后会出现在下面的列表里
          </MiuixText>
        </div>

        <div v-if="sessions.length === 0" class="empty">
          <MiuixText type="footnote1" color="var(--m-color-on-surface-secondary)">
            还没有充电记录。插上充电器开始充电后，这里会自动记录。
          </MiuixText>
        </div>

        <div
          v-for="s in visibleSessions"
          v-else
          :key="s.id"
          class="sess"
        >
          <!-- 点这一行展开 / 收起曲线。曲线块是本行的兄弟节点，
               这样在图上的手指操作不会冒泡回来把行收起来 -->
          <div
            class="sess__row"
            role="button"
            tabindex="0"
            :aria-expanded="openId === s.id"
            @click="toggle(s.id)"
            @keydown.enter="toggle(s.id)"
          >
            <div class="sess__main">
              <div class="sess__head">
                <MiuixText type="body2" class="sess__when">
                  {{ absTime(s.started_at) }}
                </MiuixText>
                <MiuixText type="footnote2" color="var(--m-color-on-surface-secondary)">
                  {{ duration(s.duration_s) }}
                </MiuixText>
              </div>

              <div class="sess__soc">
                <span class="soc__v">{{ f0(s.soc_start) }}%</span>
                <span class="soc__arrow">→</span>
                <span class="soc__v soc__v--end">{{ f0(s.soc_end) }}%</span>
              </div>

              <MiuixText
                type="footnote2"
                color="var(--m-color-on-surface-secondary)"
                class="sess__meta"
              >
                {{ f1(s.energy_wh) }} Wh · 平均 {{ f1(s.avg_power_w) }} W · 峰值
                {{ f1(s.peak_temp_c) }} ℃ · {{ f0(s.points) }} 个采样点
              </MiuixText>
            </div>

            <span class="chev" :class="{ 'chev--open': openId === s.id }">›</span>
          </div>

          <!-- ── 曲线 ── -->
          <div v-if="openId === s.id" class="sess__chart">
            <MiuixText
              v-if="sessionLoading"
              type="footnote1"
              color="var(--m-color-on-surface-secondary)"
            >
              正在读取这一次的曲线…
            </MiuixText>

            <MiuixText
              v-else-if="sessionError"
              type="footnote1"
              color="var(--m-color-error)"
            >
              曲线读取失败：{{ sessionError }}
            </MiuixText>

            <!-- 点数太少就画不出线，直接说清楚而不是给一张空图 -->
            <MiuixText
              v-else-if="!chart || chart.count < 2"
              type="footnote1"
              color="var(--m-color-on-surface-secondary)"
            >
              这一次只采到 {{ chart ? chart.count : 0 }} 个点，画不出曲线。
            </MiuixText>

            <div v-else class="chart">
              <!-- 图例：颜色与下面两条曲线、读数条一一对应 -->
              <div class="ch__legend">
                <span class="lg">
                  <i class="lg__dot lg__dot--temp" />
                  <MiuixText type="footnote2">温度 ℃</MiuixText>
                </span>
                <span class="lg">
                  <i class="lg__dot lg__dot--power" />
                  <MiuixText type="footnote2">功率 W</MiuixText>
                </span>
                <MiuixText
                  type="footnote2"
                  color="var(--m-color-on-surface-secondary)"
                  class="ch__sample"
                >
                  {{ chart.count }} 点 · 每 {{ f0(sampleS) }} 秒
                </MiuixText>
              </div>

              <div class="ch__grid">
                <!-- 左轴：温度 -->
                <div class="ch__axis">
                  <span
                    v-for="t in chart.tempGrid"
                    :key="`t${t.label}`"
                    class="ch__tick"
                    :style="{ top: yPct(t.y) }"
                  >
                    {{ t.label }}
                  </span>
                </div>

                <!-- 绘图区：SVG 只画线与文字之外的图形，刻度与圆点在 HTML 层 -->
                <div
                  class="ch__plot"
                  @pointerdown="onPointerDown"
                  @pointermove="onPointerMove"
                  @pointerup="onPointerUp"
                  @pointercancel="onPointerCancel"
                  @pointerleave="onPointerLeave"
                >
                  <svg
                    class="ch__svg"
                    viewBox="0 0 1000 100"
                    preserveAspectRatio="none"
                    aria-hidden="true"
                  >
                    <line
                      v-for="t in chart.tempGrid"
                      :key="`h${t.label}`"
                      class="ch__gline"
                      x1="0"
                      x2="1000"
                      :y1="t.y"
                      :y2="t.y"
                    />
                    <line
                      v-for="x in GRID_X"
                      :key="`v${x}`"
                      class="ch__gline"
                      :x1="x"
                      :x2="x"
                      y1="0"
                      y2="100"
                    />
                    <line
                      v-if="cursorX !== null"
                      class="ch__cursor"
                      :x1="cursorX"
                      :x2="cursorX"
                      y1="0"
                      y2="100"
                    />
                    <polyline class="ch__line ch__line--power" :points="chart.powerPoints" />
                    <polyline class="ch__line ch__line--temp" :points="chart.tempPoints" />
                  </svg>

                  <span
                    v-for="m in markers"
                    :key="m.key"
                    class="ch__dot"
                    :class="m.cls"
                    :style="m.style"
                  />
                </div>

                <!-- 右轴：功率 -->
                <div class="ch__axis ch__axis--right">
                  <span
                    v-for="t in chart.powerGrid"
                    :key="`p${t.label}`"
                    class="ch__tick"
                    :style="{ top: yPct(t.y) }"
                  >
                    {{ t.label }}
                  </span>
                </div>

                <!-- 横轴：从会话开始算起的分钟数 -->
                <div class="ch__xrow">
                  <span class="ch__xlabel">0 分</span>
                  <span class="ch__xlabel">{{ xMid }}</span>
                  <span class="ch__xlabel">{{ xEnd }}</span>
                </div>
              </div>

              <!-- 读数条：默认是结束时的值，按住图上任意位置就变成那一刻的值 -->
              <div v-if="readout" class="ch__readout">
                <MiuixText
                  type="footnote2"
                  color="var(--m-color-on-surface-secondary)"
                  class="ch__rohead"
                >
                  {{ readout.head }}
                </MiuixText>
                <span class="ch__roval">
                  <b class="val val--temp">{{ readout.temp }}</b>
                  <MiuixText type="footnote2" color="var(--m-color-on-surface-secondary)">℃</MiuixText>
                  <b class="val val--power">{{ readout.power }}</b>
                  <MiuixText type="footnote2" color="var(--m-color-on-surface-secondary)">W</MiuixText>
                  <MiuixText type="footnote2" color="var(--m-color-on-surface-secondary)">
                    电量 {{ readout.soc }}%
                  </MiuixText>
                </span>
              </div>

              <MiuixText
                type="footnote2"
                color="var(--m-color-on-surface-secondary)"
                class="ch__hint"
              >
                按住曲线，可以看那一刻的具体读数
              </MiuixText>
            </div>
          </div>
        </div>

        <!--
          「更多」/「收起」—— 与「最近事件」同一套交互

          ⚠️ 只在真的被折叠时才出现（`hiddenSessionCount > 0`）——
            记录不足 3 条时冒出一个按钮、点下去毫无变化，比不显示更差。
        -->
        <div v-if="hiddenSessionCount > 0 || showAllSessions" class="actions">
          <MiuixButton class="actions__btn" @click="showAllSessions = !showAllSessions">
            {{ showAllSessions ? "收起" : `更多（还有 ${hiddenSessionCount} 次）` }}
          </MiuixButton>
        </div>
      </MiuixCard>

      <!-- ── 最近事件 ── -->
      <MiuixSmallTitle text="最近事件" />
      <MiuixCard>
        <div v-if="events.length === 0" class="empty">
          <MiuixText type="footnote1" color="var(--m-color-on-surface-secondary)">
            暂时没有事件。插上充电器、改一次设置，这里就会有记录。
          </MiuixText>
        </div>
        <template v-else>
          <div class="selectable evts">
            <!--
              ⚠️ 遍历 `visibleEvents`（默认只给前 10 条），不是 `events`

              用户要求（2026-10-16）：「最近事件一次性显示太多，默认显示
              最近 10 条，如果需要看更多需要点击更多查看」。
            -->
            <div
              v-for="(e, i) in visibleEvents"
              :key="`${e.ts}-${i}`"
              class="evt"
            >
              <span class="evt__badge" :class="kindClass(e.kind)">
                {{ kindLabel(e.kind) }}
              </span>
              <!--
                ⚠️ 走 `eventMsg()` 而不是直接 `e.msg`

                它是**兜底翻译**：升级前产生的旧事件消息仍是英文
                （`Normal → Tightening`），而后端修好之后新事件已经是中文。
                两者混在一起时，这个函数只翻英文的那种，中文原样返回。
              -->
              <span class="evt__msg">{{ eventMsg(e.msg) }}</span>
              <span class="evt__time">{{ relTime(e.ts) }}</span>
            </div>
          </div>

          <!--
            「更多」/「收起」

            ⚠️ 只在真的被折叠时才出现（`hiddenEventCount > 0`）——
              事件不足 10 条时冒出一个"更多"按钮，点下去毫无变化，
              是比不显示按钮更差的体验。
          -->
          <div v-if="hiddenEventCount > 0 || showAllEvents" class="actions">
            <MiuixButton class="actions__btn" @click="showAllEvents = !showAllEvents">
              {{ showAllEvents ? "收起" : `更多（还有 ${hiddenEventCount} 条）` }}
            </MiuixButton>
          </div>
        </template>
      </MiuixCard>

      <!-- ── 导出诊断日志（反馈问题时用）── -->
      <MiuixSmallTitle text="反馈问题" />
      <MiuixCard>
        <MiuixText
          type="footnote1"
          color="var(--m-color-on-surface-secondary)"
          class="lead"
        >
          遇到问题时导出一份诊断报告，把它发给维护者即可定位。
          报告含机型、系统版本、当前设置、事件记录、运行日志与界面打点 ——
          <b>不含</b>账号、WiFi、应用列表等隐私信息。
        </MiuixText>

        <MiuixDivider class="gap" />

        <!--
          ┌─ 调试模式（用户要求 2026-10-17）──────────────────────────┐

          「在反馈问题中增加一个调试模式按钮，如果开启调试模式会记录
            更详细的数据，方便你抓包分析。」

          开关放在**导出按钮之前**：正确顺序是先开它、再复现、最后导出。
          放在后面的话用户容易先导出再看见它，然后白导出一次。
        -->
        <div class="row">
          <div class="row__text">
            <MiuixText type="body1">调试模式</MiuixText>
            <MiuixText
              type="footnote1"
              color="var(--m-color-on-surface-secondary)"
            >
              记录每拍决策过程、节点读写与运行日志 ——
              复现完问题后请关掉
            </MiuixText>
          </div>
          <MiuixSwitch
            :model-value="debugOn"
            :disabled="debugBusy"
            @update:model-value="onToggleDebug"
          />
        </div>

        <!--
          开启后显示**已记录多少** —— 让用户对"要不要现在导出"有个判断，
          也顺带证明它真的在记（只显示一个开关，用户没法确认）。
        -->
        <MiuixText
          v-if="debugOn && debugStatus"
          type="footnote2"
          color="var(--m-color-on-surface-secondary)"
          class="hint"
        >
          已记录 {{ fmtBytes(debugStatus.bytes) }} /
          {{ debugStatus.max_mb }} MB（超出后自动丢掉最旧的一半）
        </MiuixText>

        <MiuixDivider class="gap" />

        <div class="actions">
          <MiuixButton
            class="actions__btn"
            :disabled="exporting"
            @click="doExport"
          >
            {{ exporting ? "导出中…" : "导出诊断日志" }}
          </MiuixButton>
        </div>

        <!--
          导出成功后给出**文件名与位置**

          只弹一句"已导出"是不够的 —— 用户接下来要做的是"找到这个文件
          并发出去"，不给位置他就得自己翻文件管理器。

          🔴 用户要求导出到**「下载」目录**（2026-10-14）：
          首次实现只写到 `/data/adb/hyperpower/`，那是**只有 root 能看的
          地方**，用户在文件管理器里根本找不到、也就发不出去 ——
          而"发给别人"正是这个功能的唯一目的。
        -->
        <MiuixText
          v-if="exportedFile"
          type="footnote1"
          color="var(--m-color-primary)"
          class="hint selectable"
        >
          <template v-if="exportedFile.inDownload">
            ✓ 已保存到「下载」目录：{{ exportedFile.name }}
          </template>
          <!--
            回退分支：复制到下载目录失败（存储未挂载等极端情况）。
            这时报告仍在数据目录里，如实告诉用户路径 ——
            不能假装"已保存到下载"，那会让用户白翻一遍文件管理器。
          -->
          <template v-else>
            ✓ 已导出到：{{ exportedPath }}
            （复制到「下载」失败，此路径需用文件管理器或电脑查看）
          </template>
        </MiuixText>
      </MiuixCard>

      <!-- ── 危险操作 ── -->
      <MiuixSmallTitle text="危险操作" />
      <MiuixCard>
        <MiuixText
          type="footnote1"
          color="var(--m-color-on-surface-secondary)"
          class="lead"
        >
          清空后无法恢复。会清掉最近事件、充电历史、诊断报告与日志文件；
          <b>不会动你的设置与设备档案</b>。
        </MiuixText>

        <!--
          单颗按钮也用 `.actions` 包一层

          与「更多」页的「测量实际功率」保持一致：`.actions` 是 flex 容器、
          `.actions__btn` 是 `flex: 1` —— 只有一个孩子时它自然铺满整行。
          这样"单颗铺满"和"两颗并排"用的是同一套类，不必再写
          `width: 100%` 这种一次性样式。
        -->
        <div class="actions">
          <MiuixButton
            class="actions__btn actions__btn--danger"
            :disabled="clearing"
            @click="openClearConfirm"
          >
            清空全部记录
          </MiuixButton>
        </div>
      </MiuixCard>

      <!-- ── 清空确认 ── -->
      <MiuixDialog
        v-model="confirmClear"
        title="清空全部记录？"
        summary="清空后无法恢复。正在充电的这一次会继续记录。"
        @close="confirmClear = false"
      >
        <template #default="{ close }">
          <!--
            ⚠️ 清单**来自后端**（`records scope`），不在前端硬编码 ——
               前端写一份迟早与真正删除的那批漂移，
               而"弹窗说清了 A、实际只清了 B"会让用户**以为清干净了**。
          -->
          <div v-if="clearScope.length > 0" class="scope">
            <MiuixText type="footnote1" class="scope__head">会被清空：</MiuixText>
            <MiuixText
              v-for="s in clearScope"
              :key="s"
              type="footnote2"
              color="var(--m-color-on-surface-secondary)"
              class="scope__item"
            >
              · {{ s }}
            </MiuixText>
          </div>

          <div v-if="clearKept.length > 0" class="scope scope--kept">
            <MiuixText type="footnote1" class="scope__head">不受影响：</MiuixText>
            <MiuixText
              v-for="s in clearKept"
              :key="s"
              type="footnote2"
              color="var(--m-color-on-surface-secondary)"
              class="scope__item"
            >
              · {{ s }}
            </MiuixText>
          </div>

          <div class="actions">
            <MiuixButton class="actions__btn" @click="close">取消</MiuixButton>
            <MiuixButton
              class="actions__btn"
              type="primary"
              :disabled="clearing"
              @click="doClear"
            >
              确认清空
            </MiuixButton>
          </div>
        </template>
      </MiuixDialog>
    </template>
  </div>
</template>

<style scoped>
.page {
  /* 顶部 12px：内容不能紧贴顶栏（原生 App 的呼吸感） */
  padding: 12px 12px 16px;
  display: flex;
  flex-direction: column;
  gap: 12px;
}

/* ── 清空确认弹窗里的清单 ── */

/**
 * 「会被清空」/「不受影响」两段清单
 *
 * ⚠️ 刻意**不用** `.notice`（有色底框）：弹窗里已经有两个按钮块，
 *    再加两个色块会让这个弹窗变成一堵墙，用户反而不读。
 *    用纯文字 + 缩进就够 —— 重点是让用户**逐条看得到**，
 *    而不是把它们框起来（框起来会显得像警告，而"不受影响"那段不是）。
 */
.scope {
  margin: 0 0 10px;
}

.scope__head {
  display: block;
  margin-bottom: 2px;
}

.scope__item {
  display: block;
  padding-left: 8px;
  line-height: 1.7;
}

/**
 * 「不受影响」那段与上面拉开一点距离
 *
 * 两段贴在一起时，用户容易把"会被清空"的最后一条
 * 读成"不受影响"的第一条 —— 这个歧义在**删除确认**里代价很大。
 */
.scope--kept {
  margin-top: 14px;
}

/* ── 首屏占位 ── */

.hero__hint {
  display: block;
  margin-top: 6px;
  line-height: 1.5;
}

.hero__retry {
  margin-top: 12px;
}

/* `.empty` 的基础样式在 theme.css（四页共用）；这里只加本页的细化 */
.empty .m-text {
  line-height: 1.6;
}

/* ── 汇总 ── */

/*
  分组标题 + 右侧操作（「充电历史」+「刷新」）

  对齐目标：标题左边缘与卡内文字同为 28px（12 页面 + 16 卡片内边距），
  按钮右边缘与卡内文字右边缘同为 28px。因此：

  | 元素 | 处理 |
  |---|---|
  | 标题 | 保留 `padding-left: 16px`（12+16=28 ✓），去掉 `padding-right`（否则把按钮多推 16px） |
  | 按钮 | 补 `margin-right: 16px`（否则右边缘在 12，比卡内文字多出 16px） |

  两侧都对齐后，这一行看起来才与下面的卡片属于同一套版式。
*/
.sec {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
}

.sec__title {
  padding-right: 0;
}

.sec__action {
  flex: none;
  margin-right: 16px;
}

/* `.grid` / `.grid__cell` 的基础样式在 theme.css（四页共用）；
   这里只加本页的细化：本页格子里的数值不允许折行 */
.grid__cell .m-text {
  white-space: nowrap;
}

.sum__foot {
  display: block;
  margin-top: 12px;
  line-height: 1.5;
}

/* ── 历史列表 ── */

/*
  🔴 这里曾把 padding 加错在**外层容器**上，导致内容比全站其他页多缩进 16px

  `MiuixCard` 渲染出来是**两层**元素：

  ```html
  <div class="m-card-wrapper">   ← class="hcard" 落在这里
    <div class="m-card">         ← 全局规则给它 16px 内边距
      …内容…
  ```

  初版写的是 `.hcard { padding: 0 16px }`，本意是"清掉卡片的上下内边距、
  左右保留 16px"。但 `.hcard` 是**外层**，内层 `.m-card` 的 16px **依然生效**，
  两者叠加：

  | 层 | 左边距累计 |
  |---|---|
  | 页面 `.page` | 12 |
  | `.hcard`（外层） | 12 + 16 = 28 |
  | `.m-card`（内层，全局 16px） | 28 + 16 = **44** ✗ |

  而其他三页的卡片内文字都在 28px。于是**切到日志页时整块内容会向右跳 16px**，
  与上面刚对齐好的分组标题（28px）也脱节 —— 这正是"页面之间大小不一致"的
  一个具体来源。

  改法：只清掉**外层**多余的内边距，内层的 16px 保持不变，内容回到 28px。

  ⚠️ 不要试图清掉内层的 16px 再让每行自己补 —— 实测那会让分隔线撑满整卡
  （12→446），而控制页与更多页的分隔线都是内缩的（28→430）。
  保持一致比"单页好看"更重要。
*/
.hcard {
  padding: 0;
}

.now {
  /* 负外边距抵消内层 .m-card 的 16px，让高亮块铺满整张卡片 */
  margin: 0 -16px;
  padding: 12px 16px;
  background: var(--m-color-primary-container);
}

.now__head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  margin-bottom: 4px;
  flex-wrap: wrap;
}

/* 正在记录 → 呼吸灯，暗示"还在长" */


.now .tag--live {
  background: var(--m-color-primary);
  color: var(--m-color-on-primary);
}

/* 列表行之间的分隔线（最后一行不画） */
.sess + .sess {
  border-top: 1px solid var(--m-color-divider-line);
}

.sess__row {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 12px 0;
  cursor: pointer;
}

.sess__row:active {
  background: var(--m-color-surface-container-high);
}

.sess__main {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 4px;
}

.sess__head {
  display: flex;
  align-items: baseline;
  justify-content: space-between;
  gap: 8px;
}

.sess__when {
  font-weight: 500;
}

.sess__soc {
  display: flex;
  align-items: center;
  gap: 6px;
  font-variant-numeric: tabular-nums;
}

.soc__v {
  font-size: 16px;
}

/* 充电的落点用主色标出来，一眼看到"充到了多少" */
.soc__v--end {
  color: var(--m-color-primary);
}

.soc__arrow {
  font-size: 13px;
  color: var(--m-color-on-surface-secondary);
}

.sess__meta {
  line-height: 1.5;
}

.chev {
  flex: none;
  font-size: 20px;
  line-height: 1;
  color: var(--m-color-on-surface-secondary);
  transition: transform 0.15s cubic-bezier(0.2, 0, 0, 1);
}

.chev--open {
  transform: rotate(90deg);
}

/* ── 曲线 ── */

.sess__chart {
  margin: 0 -4px 12px;
  padding: 10px 4px 4px;
  border-radius: 12px;
  background: var(--m-color-surface-container-high);
}

.sess__chart .m-text {
  line-height: 1.5;
}

.chart {
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.ch__legend {
  display: flex;
  align-items: center;
  gap: 12px;
  padding-left: 4px;
}

.lg {
  display: inline-flex;
  align-items: center;
  gap: 5px;
}

.lg__dot {
  width: 8px;
  height: 8px;
  border-radius: 999px;
  flex: none;
}

.lg__dot--temp {
  background: var(--m-color-error);
}

.lg__dot--power {
  background: var(--m-color-primary);
}

.ch__sample {
  margin-left: auto;
}

/* 左轴 + 绘图区 + 右轴，横轴标签与绘图区同列 */
.ch__grid {
  display: grid;
  grid-template-columns: 34px 1fr 34px;
  grid-template-rows: 168px auto;
  align-items: stretch;
}

.ch__axis {
  position: relative;
  grid-row: 1;
  grid-column: 1;
}

.ch__axis--right {
  grid-column: 3;
}

.ch__tick {
  position: absolute;
  left: 0;
  transform: translateY(-50%);
  font-size: 10px;
  line-height: 1;
  color: var(--m-color-on-surface-secondary);
  font-variant-numeric: tabular-nums;
  white-space: nowrap;
}

.ch__axis--right .ch__tick {
  left: auto;
  right: 0;
}

.ch__plot {
  position: relative;
  grid-row: 1;
  grid-column: 2;
  /* 纵向留给页面滚动，横向留给手指查看读数 */
  touch-action: pan-y;
}

.ch__svg {
  position: absolute;
  inset: 0;
  width: 100%;
  height: 100%;
  display: block;
}

/* 参考线：不随缩放变粗细，否则拉伸后横竖粗细不一致 */
.ch__gline {
  stroke: var(--m-color-divider-line);
  stroke-width: 1;
  vector-effect: non-scaling-stroke;
}

.ch__line {
  fill: none;
  stroke-width: 1.8;
  stroke-linejoin: round;
  stroke-linecap: round;
  vector-effect: non-scaling-stroke;
}

.ch__line--temp {
  stroke: var(--m-color-error);
}

.ch__line--power {
  stroke: var(--m-color-primary);
}

.ch__cursor {
  stroke: var(--m-color-on-surface-secondary);
  stroke-width: 1;
  stroke-dasharray: 4 4;
  vector-effect: non-scaling-stroke;
}

/* 圆点用 HTML 画：SVG 被非等比拉伸，圆会变成椭圆 */
.ch__dot {
  position: absolute;
  width: 7px;
  height: 7px;
  margin: -3.5px 0 0 -3.5px;
  border-radius: 999px;
  pointer-events: none;
}

.ch__dot--temp {
  background: var(--m-color-error);
}

.ch__dot--power {
  background: var(--m-color-primary);
}

/* 手指按住的那个点：加一圈描边，与结束点区分开 */
.ch__dot--at {
  box-shadow: 0 0 0 2px var(--m-color-surface-container-high);
}

.ch__xrow {
  grid-row: 2;
  grid-column: 2;
  display: flex;
  justify-content: space-between;
  padding-top: 4px;
}

.ch__xlabel {
  font-size: 10px;
  line-height: 1;
  color: var(--m-color-on-surface-secondary);
  font-variant-numeric: tabular-nums;
}

.ch__readout {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  padding-left: 4px;
  flex-wrap: wrap;
}

.ch__roval {
  display: inline-flex;
  align-items: baseline;
  gap: 3px;
}

.val {
  font-size: 14px;
  font-weight: 500;
  font-variant-numeric: tabular-nums;
}

.val--temp {
  color: var(--m-color-error);
}

.val--power {
  color: var(--m-color-primary);
}

.ch__hint {
  padding-left: 4px;
}

/* ── 事件 ── */

.evts {
  display: flex;
  flex-direction: column;
}

.evt {
  display: flex;
  align-items: flex-start;
  gap: 8px;
  padding: 9px 0;
  font-size: 12px;
  line-height: 1.5;
  border-bottom: 1px solid var(--m-color-divider-line);
}

.evt:last-child {
  border-bottom: none;
}

.evt__badge {
  flex: none;
  margin-top: 1px;
  padding: 1px 6px;
  border-radius: 6px;
  font-size: 11px;
  line-height: 1.6;
  white-space: nowrap;
  background: var(--m-color-surface-container-high);
  color: var(--m-color-on-surface-secondary);
}

.evt__badge--state {
  background: var(--m-color-primary-container);
  color: var(--m-color-on-primary-container);
}

.evt__badge--config {
  background: var(--m-color-secondary-container);
  color: var(--m-color-on-secondary-container);
}

.evt__badge--release {
  background: var(--m-color-tertiary-container);
  color: var(--m-color-on-tertiary-container);
}

.evt__badge--error {
  background: var(--m-color-error-container);
  color: var(--m-color-on-error-container);
}

.evt__badge--addon {
  background: var(--m-color-surface-container-highest);
  color: var(--m-color-on-surface-container);
}

.evt__msg {
  flex: 1;
  min-width: 0;
  word-break: break-word;
}

.evt__time {
  flex: none;
  font-size: 11px;
  color: var(--m-color-on-surface-secondary);
  font-variant-numeric: tabular-nums;
  white-space: nowrap;
}

/* ── 危险操作 ── */

</style>
