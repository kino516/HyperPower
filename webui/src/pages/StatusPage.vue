<!--
  状态页 —— **纯实时读数**（只读，不含任何设置项）

  信息层级（从大到小，符合"一眼看到最重要的"）：

  1. 大号温度数字（当前最关心的量）
  2. 两个状态徽标（充电状态 / 机器状态）
  3. 电量进度条
  4. 三格指标（功率 / 电量 / 已写电流）
  5. 详细读数列表

  # 为什么这里没有设置项（2026-10-13 重构）

  原先"功率上限"的滑块放在这一页，理由是"边看边调"。
  但底栏改成「状态 / 控制 / 日志 / 更多」之后，这个理由不再成立：

  | 问题 | 说明 |
  |---|---|
  | 职责混乱 | 一页里既有读数又有设置，用户不知道"这页能不能改" |
  | 重复 | 充电上限、温度阈值都在别的页 —— 同类的设置应该在一起 |
  | 误触风险 | 想滑动看数据时容易碰到设置控件 |

  现在：**状态页只读**，所有设置集中在「控制」页。
  功率上限的滑块搬到了控制页（那里有更多的操作空间）。
-->

<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import {
  MiuixButton,
  MiuixCard,
  MiuixSmallTitle,
  MiuixText,
} from "miuix-vue";

import * as api from "../api";
import {
  amps1,
  chargingLabel,
  duration,
  f0,
  f1,
  isCharging,
  stateLabel,
  stateTone,
  volts2,
} from "../format";
import { refresh, sysState } from "../stores/sys";
import type { DeviceProfileView } from "../types";

const st = computed(() => sysState.status);

/**
 * 「详细读数」的 12 个格子
 *
 * # 为什么用数组 + `v-for`，而不是手写 12 个 `<div>`
 *
 * 手写时"顺序、标签、单位"三处都要人工对齐，而这份列表**还会继续加项**
 * （2026-10-17 一次就加了 3 个：电压、机身温度、温度依据）。
 * 数组把三者绑在同一个对象里，加一项就是加一行。
 *
 * # 顺序依据
 *
 * 按"用户看这块时的关注顺序"排，而不是按后端字段顺序：
 * 电流 → 电压 → 档位/功率 → 温度 → 协议 → 运行时长。
 * 两个电流排最前是因为它们的**差额**最能说明问题
 * （差得多 = 系统没跟上模块的意图）。
 *
 * # ⚠️ 空值一律显示 `—`
 *
 * 不显示 `0`。`0` 是**一个有效的读数**（比如没在充电时电流确实接近 0），
 * 而 `—` 表达"本机没有这个节点 / 读不到"。两者混用会让用户以为
 * "我的手机电流真的是 0"。
 */
const readouts = computed<{ label: string; value: string }[]>(() => {
  const s = st.value;
  if (!s) {
    return [];
  }
  const dash = "—";
  return [
    {
      label: "实际电流",
      value:
        s.battery.current_a === null
          ? dash
          : `${f1(s.battery.current_a)} A`,
    },
    { label: "已写电流", value: `${amps1(s.control.curr_now)} A` },
    {
      label: "电压",
      value:
        s.battery.voltage_v === null ? dash : `${volts2(s.battery.voltage_v)} V`,
    },
    { label: "目标功率上限", value: `${f0(s.control.power_limit_w)} W` },
    // ⚠️ 这里原有「电荷泵温度」，已删除（2026-10-17 用户要求）
    //
    // 它与**主卡小字**里的「电荷泵 48.9℃」是**同一个值**。两者都留时
    // 同一个数字在一屏出现两次 ⇒ 只留主卡那个（它在"电池温度 · 收紧阈值
    // · 电荷泵"三个数并排的位置上，一起看才能判断"是电池热还是芯片热"）。
    {
      label: "机身温度",
      value:
        s.thermal.board_temp_c === null
          ? dash
          : `${f1(s.thermal.board_temp_c)} ℃`,
    },
    // ⚠️ 这里原有「本次收紧峰值」（`thermal.peak_c`），已删除
    //    （2026-10-17 用户要求）。字段本身仍在 `ctl status` 里。
    { label: "已运行", value: duration(s.uptime_s) },
  ];
});

/**
 * 设备档案（只为拿**设备名**与来源徽标）
 *
 * # 为什么不放在 store 里
 *
 * 它只被这一页用来显示身份，而 `fetchDeviceCached()` 本身就是
 * 模块级缓存（见 api.ts）—— 放进 store 只会多一层转发。
 *
 * # ⚠️ 异步加载，不阻塞首屏
 *
 * `status` 才是首屏关键路径（它决定温度能不能显示）。设备名晚一点到
 * 没有关系：`deviceName` 为空时那一行**整块不渲染**，
 * 而不是留一个"读取中"的占位把布局撑开。
 */
const dev = ref<DeviceProfileView | null>(null);
const deviceName = computed(
  () => dev.value?.identity.marketname || dev.value?.identity.device || "",
);
const deviceSource = computed(() => dev.value?.source ?? null);
const deviceVerified = computed(() => dev.value?.verified ?? false);

onMounted(() => {
  // 失败就静默：设备名不是关键信息，不该因此弹错误提示
  void api
    .fetchDeviceCached()
    .then((d) => {
      dev.value = d;
    })
    .catch(() => undefined);
});

/** 后端不可达时的手动重试 */
function onRetry(): void {
  void refresh();
}

const socPct = computed(() => {
  const v = st.value?.battery.soc ?? 0;
  return Math.max(0, Math.min(100, v));
});

/** 色调 → 对应 miuix 配色变量 */
const toneColor = computed(() => {
  switch (stateTone(st.value?.state)) {
    case "normal":
      return "var(--m-color-primary)";
    case "warn":
      return "var(--m-color-primary)";
    case "error":
      return "var(--m-color-error)";
    default:
      return "var(--m-color-on-surface-secondary)";
  }
});

/**
 * 充电器格的说明文字 —— **协商功率**
 *
 * 🔴 用户要求（2026-10-16）：把检测到的「协商值」显示在主页。
 *
 * 协商值 = 手机与**当前这个充电头**谈成的功率上限，读自
 * `/sys/class/xm_power/charger/charger_common/power_max`。
 *
 * 与「更多」页那个「充电功率上限」的区别：
 *
 * | | 本格（协商值） | 更多页（机型能力） |
 * |---|---|---|
 * | 含义 | **这个充电头**能跑多快 | **这台手机**最高能吃多少 |
 * | 换充电头 | **跟着变** | 不变 |
 * | 取法 | 直接读节点 | 三来源（协商/电池型号/设备树）**取最大** |
 *
 * ⚠️ 插电脑 USB 口（SDP）或普通 5V 头时**协商不出快充**，此时节点读作 0
 * （后端已 filter 成 `null`）。这种情况必须写明"未协商快充" ——
 * 留空或显示 "—" 会让用户以为界面坏了。
 */

/**
 * 三格第二格「充电头」的值
 *
 * 🔴 用户要求（2026-10-17）：
 *   · 未充电时显示「未充电」，**不要显示 UNKNOWN**
 *   · 充电时显示当前充电头的**协商功率**
 *
 * # 为什么要专门处理"未充电"
 *
 * `power_max_w` 只在**插着充电器且协商成功**后才有值，拔掉就是 `null`。
 * 照实走原来的分支会得到 `—` 或 `UNKNOWN`：
 *
 * | 显示 | 问题 |
 * |---|---|
 * | `—` | 像界面坏了 |
 * | `UNKNOWN` | 干脆是英文内部值（用户明确点名不要） |
 * | **未充电** | ✅ 说清了"为什么没有值" |
 *
 * ⚠️ 判据用 [`isCharging`]（`Charging` 或 `Full`）而不是"有没有协商值" ——
 *    插着充电器但没协商出快充（如电脑 USB 口）时**仍算充电中**，
 *    那时该显示"未协商"而不是"未充电"。
 */
const chargerCellLabel = computed(() => {
  const s = st.value;
  if (!s) {
    return "—";
  }
  if (!isCharging(s.battery.status)) {
    return "未充电";
  }
  const w = s.charger.power_max_w;
  return typeof w === "number" && w > 0 ? `${f0(w)} W` : "未协商";
});

</script>

<template>
  <div class="page">
    <!--
      🔴 后端不可达 —— 必须**说清楚**，不能一直装作"正在读取"

      真机踩到的问题：`daemonOk` / `lastError` 由 store 维护，但界面
      **从来没有用过它们**。于是 worker 没起来（比如刚重启、或模块被禁用）时，
      用户看到的是**永久**的「正在读取状态…」——
      既不知道出了错，也不知道该做什么。

      这正是"打开模块白屏很久"最可能的表现：不是渲染慢，而是
      **在等一个永远不会来的响应**。
    -->
    <MiuixCard v-if="!st && !sysState.daemonOk" class="alert alert--stack">
      <MiuixText type="title4" color="var(--m-color-on-error-container)">
        读不到模块状态
      </MiuixText>
      <MiuixText type="footnote1" color="var(--m-color-on-error-container)">
        {{ sysState.lastError ?? "模块后台没有响应。" }}
      </MiuixText>
      <MiuixText type="footnote1" color="var(--m-color-on-error-container)">
        后台进程可能已被系统回收。点「重试」会自动拉起后台进程；若仍失败，请重启设备。
      </MiuixText>
      <div class="retry">
        <MiuixButton :disabled="sysState.loading" @click="onRetry">
          {{ sysState.loading ? "重试中…" : "重试" }}
        </MiuixButton>
      </div>
    </MiuixCard>

    <!-- 首次加载中（后端可达，只是还没回）—— 不显示假数字 -->
    <MiuixCard v-else-if="!st" class="hero">
      <MiuixText type="title4">正在读取状态…</MiuixText>
      <MiuixText type="footnote1" color="var(--m-color-on-surface-secondary)">
        若长时间无响应，请检查模块是否已启动
      </MiuixText>
    </MiuixCard>

    <template v-else>
      <!--
        🔴 未检测警告 —— 必须**显眼**，不能只靠状态标签

        背景：未检测时决策层零写入（安全闸门「不猜硬件」），
        而 `state` 是 `idle` ⇒ 状态标签显示「待机」——
        一个看起来完全正常的词，用户会以为模块在工作。

        正常情况下 supervisor 首次启动会自动检测
        （见 supervisor::ensure_detected），所以看到这张卡片
        意味着**自动检测也失败了**，更需要明确指出。
      -->
      <MiuixCard v-if="!st.detected" class="alert alert--stack">
        <MiuixText type="body1" color="var(--m-color-on-error-container)">
          尚未检测本机 —— 模块当前不控制充电
        </MiuixText>
        <MiuixText type="footnote1" color="var(--m-color-on-error-container)">
          检测前模块不会写入任何充电参数（避免用猜测的硬件边界动真实硬件）。
          请到「更多」页点「重新检测」。
        </MiuixText>
      </MiuixCard>

      <!-- ── 主指标 ── -->
      <MiuixCard class="hero">
        <!--
          设备名（从「更多」页搬来）

          用户要求（2026-10-14）：「更多中最顶上显示的是设备名称，删除并放到
          状态首页」。理由：状态页是打开模块的第一眼，用户想知道
          "模块现在认为我这是什么机器"——那正是检测结果的**身份**部分。

          放在主卡片**顶部而不是单独一张卡**：
          状态页的第一眼价值是那个大温度，不该被一张身份卡推到屏幕外。

          ⚠️ 不新增命令：`fetchDeviceCached()` 有 Promise 缓存
          （见 api.ts），更多页/控制页已经发过同一条 `device show`。
        -->
        <div v-if="deviceName" class="idbar">
          <MiuixText type="body1">{{ deviceName }}</MiuixText>
          <span class="chip" :class="deviceVerified ? 'chip--on' : ''">
            {{ deviceSource ?? "读取中" }}
          </span>
        </div>

        <div class="hero__tags">
          <span class="tag" :class="{ 'tag--live': isCharging(st.battery.status) }">
            <i class="tag__dot" />
            {{ chargingLabel(st.battery.status) }}
          </span>
          <span class="tag" :style="{ color: toneColor }">
            {{ stateLabel(st.state) }}
          </span>
        </div>

        <div class="hero__value">
          <span class="hero__num">{{ f1(st.battery.temp_c) }}</span>
          <span class="hero__unit">℃</span>
        </div>
        <MiuixText type="footnote1" color="var(--m-color-on-surface-secondary)">
          电池温度 · 收紧阈值 {{ f0(st.thermal.tighten_temp) }}℃<template
            v-if="st.thermal.cp_die_temp_c !== null"
          >
            · 电荷泵 {{ f1(st.thermal.cp_die_temp_c) }}℃</template
          >
        </MiuixText>

        <div class="bar">
          <div class="bar__fill" :style="{ width: `${socPct}%` }" />
        </div>
        <div class="bar__label">
          <MiuixText type="footnote1">电量 {{ f0(socPct) }}%</MiuixText>
          <!--
            ⚠️ 这里原有 `{{ f1(st.battery.power_w) }} W`（进度条右侧的功率），
               已删除（2026-10-17）：它与下方三格里的「功率」**是同一个值**，
               同一个数字在一屏上出现两次没有意义。

               保留三格那个 —— 它是大字、有标签，比挤在进度条旁边更醒目。
          -->
        </div>
      </MiuixCard>

      <!-- ── 三格指标 ── -->
      <!--
        🔴 三格的标签与内容（用户 2026-10-17 定）：**功率 / 充电头 / 充电模式**

        | 格 | 值 | 说明 |
        |---|---|---|
        | 功率 | `42.7 W` | **实测**充进去多少 |
        | 充电头 | 未充电时「未充电」，充电时 `33 W` | 当前这个头**谈成**的功率 |
        | 充电模式 | 保守 / 官方 / 极速 / 自定义 | 模块当前生效的模式 |

        ⚠️ 「充电头」这个名字是刻意选的（原叫「充电器」）：
        那一格的答案已经**不是"插了什么"而是"能跑多快"**，
        而「充电器」这个名字会让人以为显示的是设备名。
        「充电头」是用户口语里的说法，且"未充电 / 33 W"两种内容都读得通。

        ⚠️ 容器类名必须用全局的 `.grid` / `.grid__cell`（定义在 `theme.css`）

        2026-10-17 踩到：详细读数改双列网格时，在本文件的 `<style scoped>`
        **又定义了一份 `.grid`（2 列）**，把全局的三列覆盖掉 ——
        三格于是变成两行。而 `check-style-dup.mjs` 只查**同文件内**重复，
        跨文件（scoped 覆盖全局）抓不到。

        ⇒ 详细读数现在用自己的 `.readouts*`；这里保持 `.grid*` 不动。
      -->
      <MiuixCard>
        <div class="grid grid--center">
          <div class="grid__cell">
            <MiuixText type="title3">{{ f1(st.battery.power_w) }} W</MiuixText>
            <MiuixText type="footnote2" color="var(--m-color-on-surface-secondary)">
              功率
            </MiuixText>
          </div>
          <!--
            第二格：**充电头**（协商功率）

            🔴 用户要求（2026-10-17）：
              · 未充电时显示「未充电」，**不要显示 UNKNOWN**
              · 充电时显示当前充电头的**协商功率**

            为什么要专门处理"未充电"：`power_max_w` 只在**插着充电器协商成功**
            后才有值，拔掉就是 `null`。照实显示会变成 "—" 或 "UNKNOWN" ——
            前者像界面坏了，后者干脆是英文内部值（用户明确点名不要）。
          -->
          <div class="grid__cell">
            <MiuixText type="title3" class="grid__charger">
              {{ chargerCellLabel }}
            </MiuixText>
            <MiuixText type="footnote2" color="var(--m-color-on-surface-secondary)">
              充电头
            </MiuixText>
          </div>
          <!--
            第三格：**充电模式**（用户 2026-10-17 定）

            显示「控制」页里当前生效的那个模式。文案来自后端的
            `mode_label`（与 `ctl modes` 同一份映射）——
            前端不再维护一份 id→中文的表，否则两处迟早对不上，
            而对的正是用户在控制页看到的那几个词。
          -->
          <div class="grid__cell">
            <MiuixText type="title3" class="grid__charger">
              {{ st.mode_label }}
            </MiuixText>
            <MiuixText type="footnote2" color="var(--m-color-on-surface-secondary)">
              充电模式
            </MiuixText>
          </div>
        </div>
      </MiuixCard>

      <!-- 弱电源提示：解释"为什么这么慢" -->
      <MiuixCard v-if="st.charger.weak && isCharging(st.battery.status)" class="alert alert--stack">
        <MiuixText type="body2" color="var(--m-color-on-error-container)">
          当前是弱电源（{{ st.charger.label ?? "未知" }}）
        </MiuixText>
        <MiuixText type="footnote1" color="var(--m-color-on-error-container)">
          这个功率反映的是<b>充电器 / 线材的能力</b>，不是手机的。
          想验证手机能跑多快，请用原装充电器。
        </MiuixText>
      </MiuixCard>

      <!-- ── 详细读数 ── -->
      <MiuixSmallTitle text="详细读数" />
      <MiuixCard class="rows">
        <!--
          ┌─ 双列网格（用户 2026-10-17 从 6 个方案里选定）──────────────┐

          改前是 10 条 `InfoRow`（每条两行：标题 + 说明），共 **560px**，
          一屏放不下，而且说明文字大多比标题还长（如"模块写入节点的值
          （与实际的差额来自系统响应滞后）"），把值挤到了很远的右边。

          现在每条**一格**：小字标签 + 大数字。一屏看完，数字最醒目，
          也与上方主卡的「38.6℃」大数字风格呼应。

          ⚠️ **说明文字去哪了**：删掉了。它们要么与标题重复
          （"实际电流 / 真正在充的电流"），要么在两个电流之间做解释 ——
          而那一句已经写在主卡下方与 README 里。
          真需要逐项说明时看 `thermo verify` 的输出（那里的描述更完整）。
        -->
        <div class="readouts">
          <div v-for="c in readouts" :key="c.label" class="readouts__cell">
            <div class="readouts__label">{{ c.label }}</div>
            <div class="readouts__value">{{ c.value }}</div>
          </div>
        </div>
      </MiuixCard>

      <!-- ── 已交回官方时的醒目提示 ── -->
      <MiuixCard v-if="st.released" class="alert">
        <MiuixText type="body2" color="var(--m-color-on-error-container)">
          已交回官方控制 —— 通常因为连续写入失败。
          请检查充电节点是否可写。
        </MiuixText>
      </MiuixCard>
    </template>
  </div>
</template>

<style scoped>

/* ── 详细读数：双列网格 ── */

/**
 * 两列网格，每格「小字标签 + 大数字」
 *
 * 用户 2026-10-17 从 6 个方案里选定。改前是 10 条 `InfoRow`（每条两行），
 * 共 560px 放不下一屏；现在 12 格两列共 6 行，一屏看完。
 *
 * ⚠️ `minmax(0, 1fr)` 而不是 `1fr`：CSS Grid 的 `1fr` 默认
 *    `min-width: auto`，长内容（如 "PD_PPS" 或超大数字）会把列撑破。
 *    这是 Grid 与 Flex 都有的老坑。
 */
.readouts {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 8px;
}

.readouts__cell {
  /* 描边而不是填充：填充色会和卡片背景几乎一样（实测 #f3edf7 vs #fef7ff），
     分组根本看不出来 —— 首版就是这样，三个"分组卡片"糊成一片 */
  border: 1px solid var(--m-color-outline-variant);
  border-radius: 12px;
  padding: 10px 12px 9px;
  min-width: 0;
}

.readouts__label {
  font-size: 12px;
  line-height: 1.3;
  color: var(--m-color-on-surface-secondary);
  /* 标签过长时截断而不是换行 —— 换行会让同一行的两格高度不齐 */
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.readouts__value {
  font-size: 19px;
  font-weight: 600;
  line-height: 1.25;
  color: var(--m-color-on-surface);
  margin-top: 2px;
  /* 等宽数字：读数每拍都在变，比例数字会让整格的宽度跟着抖 */
  font-variant-numeric: tabular-nums;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

/* ── 功率上限卡片 ── */

.pwr {
  display: flex;
  flex-direction: column;
  gap: 12px;
}

.pwr__head {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: 12px;
}

.pwr__badge {
  flex: none;
  padding: 3px 8px;
  border-radius: 999px;
  font-size: 11px;
  line-height: 1.6;
  background: var(--m-color-primary-container);
  color: var(--m-color-on-primary-container);
}

.pwr__scale {
  display: flex;
  justify-content: space-between;
}

.pwr__actions {
  display: flex;
  gap: 10px;
}

.pwr__btn {
  flex: 1;
}
.page {
  /* 顶部 12px：内容不能紧贴顶栏（原生 App 的呼吸感） */
  padding: 12px 12px 16px;
  display: flex;
  flex-direction: column;
  gap: 12px;
}

/* ── 主指标 ── */

/* 内边距由全局 .m-card 规则统一提供（见 theme.css），此处不再重复设置 */

/*
  设备名行（主卡片顶部）

  设计取舍：用 `body1`（16px）而不是更多页那个 `title2`（24px）——
  这一页的视觉主角是下面的 52px 温度大字，设备名用 24px 会与它抢焦点。
*/
.idbar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 10px;
  margin-bottom: 10px;
}

.hero__tags {
  display: flex;
  gap: 8px;
  margin-bottom: 12px;
  flex-wrap: wrap;
}

/*
  `.tag` / `.tag__dot` / `.tag--live` / `@keyframes pulse` 定义在 theme.css
  （状态页与日志页共用，见那里的说明）。
*/

.hero__value {
  display: flex;
  align-items: baseline;
  gap: 6px;
}

.hero__num {
  font-size: 52px;
  line-height: 1.05;
  font-weight: 400;
  font-variant-numeric: tabular-nums;
  letter-spacing: -1px;
}

.hero__unit {
  font-size: 22px;
  color: var(--m-color-on-surface-secondary);
}

/* ── 进度条 ── */

.bar {
  height: 6px;
  border-radius: 999px;
  background: var(--m-color-surface-container-highest);
  overflow: hidden;
  margin: 14px 0 6px;
}

.bar__fill {
  height: 100%;
  border-radius: 999px;
  background: var(--m-color-primary);
  transition: width 0.4s cubic-bezier(0.2, 0, 0, 1);
}

.bar__label {
  display: flex;
  justify-content: space-between;
}

/* ── 三格指标 ── */

/* `.grid` / `.grid__cell` 定义在 theme.css（与日志页共用） */

/* ── 行列表：去掉 miuix 默认的外边距，避免与卡片内边距叠加 ── */


/* ── 提示卡 ── */

.alert {
  background: var(--m-color-error-container) !important;
}

/*
  充电器名可能是较长的字符串（`PD_PPS` / `USB_PD` / `SDP`）

  三格里每格宽度约 1/3 屏。用 `title3` 字号时，
  长名字会顶破格子 ⇒ 缩一号 + 允许折行。
*/
.grid__charger {
  overflow-wrap: anywhere;
}

/* 错误卡里的重试按钮：上方留白，左对齐（不铺满整行） */
.retry {
  margin-top: 12px;
}

/*
  多段文字的提示卡（标题 + 说明）

  单段时用 `.alert` 就够；两段以上必须给间距，
  否则两行会贴在一起（MiuixCard 默认不带 flex 间距）。
*/
.alert--stack {
  display: flex;
  flex-direction: column;
  gap: 6px;
}

/* ── 事件 ── */

.logs {
  display: flex;
  flex-direction: column;
}

.log {
  display: flex;
  gap: 8px;
  padding: 7px 0;
  font-size: 12px;
  line-height: 1.5;
  border-bottom: 1px solid var(--m-color-divider-line);
}

.log:last-child {
  border-bottom: none;
}

.log__time {
  flex: none;
  font-variant-numeric: tabular-nums;
  color: var(--m-color-on-surface-secondary);
  font-family: ui-monospace, monospace;
}

.log__msg {
  flex: 1;
  min-width: 0;
  word-break: break-word;
}

.log--err .log__msg {
  color: var(--m-color-error);
}

/* `.empty` 定义在 theme.css（四页共用） */
</style>
