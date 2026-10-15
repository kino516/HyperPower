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
} from "../format";
import InfoRow from "../components/InfoRow.vue";
import { refresh, sysState } from "../stores/sys";
import type { DeviceProfileView } from "../types";

const st = computed(() => sysState.status);

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
const chargerPowerLabel = computed(() => {
  const w = st.value?.charger.power_max_w;
  if (typeof w === "number" && w > 0) {
    return `充电器 · 最高 ${f0(w)} W`;
  }
  return "充电器 · 未协商快充";
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
        常见原因：模块刚启动还没就绪（等几秒重试）、或模块未启用（重启后生效）。
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
          <MiuixText type="footnote1">{{ f1(st.battery.power_w) }} W</MiuixText>
        </div>
      </MiuixCard>

      <!-- ── 三格指标 ── -->
      <MiuixCard>
        <div class="grid">
          <div class="grid__cell">
            <MiuixText type="title3">{{ f1(st.battery.power_w) }}</MiuixText>
            <MiuixText type="footnote2" color="var(--m-color-on-surface-secondary)">
              功率 W
            </MiuixText>
          </div>
          <div class="grid__cell">
            <MiuixText type="title3">{{ f0(st.battery.soc) }}</MiuixText>
            <MiuixText type="footnote2" color="var(--m-color-on-surface-secondary)">
              电量 %
            </MiuixText>
          </div>
          <!--
            第三格原本是「已写电流」，但那对用户价值低 ——
            它是**模块的动作**，不是**充电的结果**，且与「实际电流」经常对不上。

            换成「充电器」：一眼解释"为什么只有 0.7W"
            （插电脑 USB 口 = SDP 协议 = 约 0.5A）。
            已写电流移到下方「详细读数」，排障时仍能看到。
          -->
          <div class="grid__cell">
            <MiuixText type="title3" class="grid__charger">
              {{ st.charger.label ?? "—" }}
            </MiuixText>
            <!--
              充电器格的小字：从固定的「充电器」改成**协商功率**

              🔴 用户要求（2026-10-16）：「充电功率上限会检测 3 个数据，
                 有一个为协商值……将此数值显示在主页」

              三个候选来源（见 `detect.rs` 第 7 步）：

              | 来源 | 节点 | 语义 |
              |---|---|---|
              | **协商值** | `charger_common/power_max` | **当前这个充电头**与手机谈成的功率 |
              | 电池型号 | `battery/model_name` 的 `_100w` | 出厂标称 |
              | 设备树 | `support_pps_100w` 节点名 | 支持的最高协议档位 |

              「更多」页的「充电功率上限」显示的是**三者取最大**（= 机型能力上限），
              那个值**不随换充电头变化**。而用户想知道的往往是另一种信息：
              **"我这个充电头能跑多快"** —— 那就是协商值。

              例：插 33W 头时，机型能力仍是 100W，但协商值是 33W。
              两者的差值正好解释"为什么插着 33W 头、把上限设到 100W 也没用"。
            -->
            <MiuixText
              type="footnote2"
              color="var(--m-color-on-surface-secondary)"
            >
              {{ chargerPowerLabel }}
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
          实际电流 vs 已写电流：**两个都要有**

          实测两者经常对不上（系统响应滞后）：
            | 模块写入 | 实际 |
            |---|---|
            | 8 A | 9.06 A |
            | 8 A | 6.51 A |

          只显示写入值 → 用户以为"设了 8A 就是 8A"；
          只显示实际值 → 排障时看不出模块想让它跑多少。
        -->
        <InfoRow
          title="实际电流"
          :summary="
            st.battery.current_a === null ? '本机未上报电流节点' : '真正在充的电流'
          "
        >
          <template #end>
            <MiuixText type="body2">
              {{
                st.battery.current_a === null
                  ? "—"
                  : `${f1(st.battery.current_a)} A`
              }}
            </MiuixText>
          </template>
        </InfoRow>

        <InfoRow title="已写电流" summary="模块写入节点的值（与实际的差额来自系统响应滞后）">
          <template #end>
            <MiuixText type="body2">{{ amps1(st.control.curr_now) }} A</MiuixText>
          </template>
        </InfoRow>

        <InfoRow title="档位" :summary="'当前写入的限速档'">
          <template #end>
            <MiuixText type="body2">{{ f0(st.control.gear_now) }}</MiuixText>
          </template>
        </InfoRow>

        <InfoRow title="目标功率上限">
          <template #end>
            <MiuixText type="body2">{{ f0(st.control.power_limit_w) }} W</MiuixText>
          </template>
        </InfoRow>

        <!--
          ⚠️ 硬件保护温度**只在已检测时显示**

          它是从本机设备树读出来的（`ffc_temp_high`）。未检测时档案里是
          兜底值 `45.0` —— 那是**模板默认值，不是本机数据**。
          照实显示会让用户以为"我的手机保护温度就是 45℃"，
          而真机可能是 48℃（甚至其它机型是别的值）。

          ⇒ 宁可不显示，也不显示一个"看起来像真的"的假值。
        -->
        <InfoRow
          v-if="st.detected"
          title="硬件保护温度"
          :summary="'本机硬件的上限，「收紧温度」不得超过它'"
        >
          <template #end>
            <MiuixText type="body2">{{ f1(st.thermal.abs_max) }} ℃</MiuixText>
          </template>
        </InfoRow>
        <InfoRow
          v-else
          title="硬件保护温度"
          summary="尚未检测本机 —— 该值需从设备树读取"
        >
          <template #end>
            <MiuixText type="body2">—</MiuixText>
          </template>
        </InfoRow>

        <InfoRow
          title="本次收紧峰值"
          :summary="st.thermal.peak_c === null ? '未处于收紧期' : undefined"
        >
          <template #end>
            <MiuixText type="body2">
              {{ st.thermal.peak_c === null ? "—" : `${f1(st.thermal.peak_c)} ℃` }}
            </MiuixText>
          </template>
        </InfoRow>

        <InfoRow
          title="电荷泵温度"
          :summary="
            st.thermal.cp_die_temp_c === null
              ? '本机未上报此节点'
              : '快充时的真正热点，通常高于电池温度'
          "
        >
          <template #end>
            <MiuixText type="body2">
              {{
                st.thermal.cp_die_temp_c === null
                  ? "—"
                  : `${f1(st.thermal.cp_die_temp_c)} ℃`
              }}
            </MiuixText>
          </template>
        </InfoRow>

        <InfoRow
          title="充电器协议"
          :summary="
            st.charger.power_max_w === null
              ? '未协商出快充协议'
              : `协商功率上限 ${st.charger.power_max_w} W`
          "
        >
          <template #end>
            <MiuixText type="body2">{{ st.charger.label ?? "—" }}</MiuixText>
          </template>
        </InfoRow>

        <!--
          系统场景：**只读参考**

          ⚠️ 必须写明"模块不依赖它" —— 否则用户会以为改场景能影响温控。
          模块只通过「档位 + 电流」两个节点控制功率（用户明确要求过
          「不要调整场景模式」）。
        -->
        <InfoRow
          v-if="st.scene !== null"
          title="系统场景"
          :summary="
            st.scene === '未设置'
              ? '系统未设置场景（常见状态）'
              : '来自系统热控框架；模块不依赖它，仅作参考'
          "
        >
          <template #end>
            <MiuixText type="body2">{{ st.scene }}</MiuixText>
          </template>
        </InfoRow>

        <InfoRow
          title="已运行"
          :summary="`累计 ${f0(st.ticks)} 拍`"
        >
          <template #end>
            <MiuixText type="body2">{{ duration(st.uptime_s) }}</MiuixText>
          </template>
        </InfoRow>
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
