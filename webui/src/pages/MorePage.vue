<!--
  更多页 —— 设备档案 + 重新检测 + 关于

  # 为什么叫「更多」而不是「设备」

  底栏四个页签是**按用户意图**划分的（见 App.vue 的说明）：
  状态 / 控制 / 日志 / 更多。前三者都是用户天天用的，
  而这一页是**低频但重要**的东西：

  | 内容 | 什么时候用 |
  |---|---|
  | 重新检测 | 换机型、升级系统、怀疑检测数据不对时 |
  | 设备档案 | 想知道模块"看到了什么" |
  | 实测功率 | 验证充电器/线材是否够好 |
  | 关于 | 报问题时要提供版本号 |

  # 页面结构

  ```
  ┌─────────────────────────┐
  │ 机型名 + 来源徽标          │
  ├─────────────────────────┤
  │ ⚡ 检测卡片（主操作）       │  ← 一键重新检测
  ├─────────────────────────┤
  │ 系统 / 电池               │
  │ 充电能力                  │
  │ 检测记录                  │
  └─────────────────────────┘
  ```

  # ⚠️ 检测不需要插充电器 —— 但有一项**要求未充电**

  初版把"最大功率"做成**充电时实测**，因此必须插原装充电器、且电量低于 60%
  才有意义 —— 对用户是很别扭的要求。

  现在改为**读出厂标称值**（来源：电池型号 `P11U_7560mah_100w` 里的 `100w`），
  它不随电量/充电器变化。于是检测项全部变成**只读**：

  | 检测项 | 来源 | 状态要求 |
  |---|---|---|
  | 机型 / 电池 | `getprop` / `power_supply` | 任意 |
  | 档位表 | 设备树直读 | 任意 |
  | **电流上限** | 读节点 | 🔴 **必须未充电** |
  | 标称功率 | 电池型号 | 任意 |
  | 硬件保护温度 | 设备树直读 | 任意 |
  | SOC 上限支持性 | 节点探测 | 任意 |

  ⚠️ 电流上限是唯一有状态要求的一项：充电中该节点反映的是
  **模块自己写入的值**，采信会形成自我循环、逐次压低上限
  （真机踩过：一次充电中检测把上限降了 18%）。
  因此充电时会**跳过**该项，并记入 `failed_items` 提示用户拔电重跑。

  # 两种检测模式

  | 模式 | 副作用 | 说明 |
  |---|---|---|
  | 快速 | **零** | 只读，秒级 |
  | 完整 | 写探 | 额外验证档位可写上界（临时改充电参数并立即恢复） |

  默认点"重新检测"走**快速**模式；完整模式放在二级确认里，避免误触。

  # 首次运行会自动检测

  正常情况下不必来这一页 —— `supervisor` 首次启动时会自动跑一次
  快速检测（全只读），让模块"装完即用"（见 `supervisor::ensure_detected`）。

  本页的按钮用于**刷新**：系统升级、排障、或怀疑数据不对时。
-->

<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import {
  MiuixButton,
  MiuixCard,
  MiuixDialog,
  MiuixDivider,
  MiuixSmallTitle,
  MiuixText,
  showSnackbar,
} from "miuix-vue";

import * as api from "../api";
import InfoRow from "../components/InfoRow.vue";
import { amps1, f0, f1 } from "../format";
import { perfMark } from "../perf";
import { failedLabel } from "../types";
import type { DeviceProfileView } from "../types";

/** 当前生效档案 */
const device = ref<DeviceProfileView | null>(null);
const loading = ref(true);

/** 正在检测 */
const probing = ref(false);

/** 完整检测的确认弹窗 */
const confirmFull = ref(false);

/**
 * ⚠️ 这里原有「界面性能（诊断）」卡的三个成员：
 *    `perfDumping` / `perfDumped` / `dumpPerf()`
 *
 * 用户要求删除（2026-10-14）：「更多内还有个界面性能诊断现在也没用了，
 * 可以删除掉」。**判断合理** —— 见下方说明。
 *
 * # 但它背后还有东西不能一起删
 *
 * `api.dumpPerfLog()` 做两件事，第二件很关键：
 *
 * | 内容 | 用途 |
 * |---|---|
 * | 启动/切页**耗时打点**（`window.__perf`） | 排查"打开慢、切页卡" |
 * | **界面异常的技术细节**（`window.__errs`） | 排查"界面报错" |
 *
 * 而 `main.ts` 对用户**只显示一句中文提示**（原始英文错误对用户毫无意义，
 * 见 D-W7），技术细节只攒在 `window.__errs` 这个**内存数组**里 ——
 * 若没有出口，那些细节**永远到不了磁盘**。
 *
 * ⇒ 所以删除的方式是**合并**而不是直接砍掉：
 *    「导出诊断日志」现在会**先**调用 `dumpPerfLog()` 落盘，再生成报告
 *    （报告里本来就有 `perf.log` 段）。一个按钮做完两件事。
 *
 * 这样用户的负担更小（不用知道"卡了要点哪个按钮"），
 * 而诊断信息**一个都没少**。详见 `api.exportDiagnosticLog` 的文档。
 */

/**
 * ⚠️ 这里原有 `id` / `measureHint` / `measure()` / `measured` / `measuring`
 *
 * 它们服务于已删除的「系统」与「实际功率」两张卡。
 * 删除后 `vue-tsc` 立刻报出四个"已声明未使用" —— 说明清理是干净的。
 *
 * 「设备名」搬去了状态页（用 `fetchDeviceCached()`，那边自己取）。
 */
const caps = computed(() => device.value?.capabilities);
const battery = computed(() => device.value?.battery);
const notes = computed(() => device.value?.probe_meta.notes ?? []);

/** 检测时间（Unix 秒 → 本地时间） */
const probedAt = computed(() => {
  const t = device.value?.probe_meta.probed_at;
  if (!t) {
    return "未知";
  }
  const d = new Date(t * 1000);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
});

/**
 * 档案是否由**旧版**检测生成
 *
 * 旧版把标称功率做成"需充电实测"，会往 `needs_charge_for` 里记一项。
 * 新版已全只读，该数组必为空 —— 非空即说明档案过时。
 */
const isStale = computed(
  () => (device.value?.probe_meta.needs_charge_for.length ?? 0) > 0,
);

async function load(): Promise<void> {
  loading.value = true;
  try {
    device.value = await api.fetchDevice();
  } catch (e) {
    void showSnackbar({
      message: e instanceof Error ? e.message : String(e),
      duration: "long",
    });
  } finally {
    loading.value = false;
    perfMark("loaded:more");
  }
}

onMounted(() => {
  void load();
});

/**
 * 执行检测
 *
 * @param quick 快速模式（零副作用）
 * @param confirm 完整模式需要显式确认（含写探）
 */
async function probe(quick: boolean, confirm = false): Promise<void> {
  if (probing.value) {
    return;
  }
  probing.value = true;
  confirmFull.value = false;

  try {
    const r = await api.probeDevice({ quick, save: true, confirm });
    // 用检测结果直接刷新界面（省一次往返）
    device.value = r.profile;

    const failed = r.profile.probe_meta.failed_items;
    void showSnackbar({
      message:
        failed.length === 0
          ? `检测完成（${r.elapsed_ms} ms），已保存`
          : // 转成中文再显示 —— 直接把 `current_ceiling` 摆给用户等于没说
            `检测完成，但有 ${failed.length} 项未测到：${failed
              .map(failedLabel)
              .join("、")}`,
      duration: failed.length === 0 ? "short" : "long",
    });
  } catch (e) {
    void showSnackbar({
      message: `检测失败：${e instanceof Error ? e.message : String(e)}`,
      duration: "long",
    });
  } finally {
    probing.value = false;
  }
}

/**
 * ⚠️ `measure()` 已删除 —— 它服务于已移除的「实际功率」卡
 *
 * 后端命令 `thermo measure` 仍然存在（README 的命令表里有），
 * 需要精确测量时可以从命令行调用；界面不再提供入口，因为状态页
 * 已经有 3 秒一刷的实时功率。
 */
</script>

<template>
  <div class="page">
    <!--
      ── 机型头（已移除）──

      这里原有一张卡片显示设备名（`title2`）+ 型号·平台 + 来源徽标。

      用户要求（2026-10-14）：「更多中最顶上显示的是设备名称，删除并放到
      状态首页」。

      理由（判断合理）：设备名是**身份信息**，属于"我打开模块第一眼想知道
      这台机器被识别成了什么"；而「更多」页的定位是**低频维护**
      （重新检测 / 诊断 / 关于）。身份放状态页、维护留在更多页，职责更清楚。

      现在它显示在状态页主卡片的顶部（见 StatusPage.vue 的 `.idbar`）。

      ⚠️ `device` 这个 ref 仍然需要 —— 下面的「设备检测」「系统」「电池」
        等分区都读它。
    -->

    <!--
      ── 检测（主操作）──

      ⚠️ 标题放在**卡片外面**（与其余三页一致）

      初版把「重新检测本机」写在卡片内部、用 `body1`（16px / 常规粗细），
      而分组标题实际是 **14px / 加粗**（miuix 的 `subtitle` 字号）——
      卡内标题反而比分组标题大，层级是倒的。

      现在统一为卡外分组标题，说明文字留在卡内顶部（`.lead`）。
    -->
    <MiuixSmallTitle text="设备检测" />
    <MiuixCard>
      <div class="probe">
        <div class="probe__text">
          <MiuixText
            type="footnote1"
            color="var(--m-color-on-surface-secondary)"
            class="lead"
          >
            读取档位表、电流上限、标称功率、硬件保护温度，全程不改动充电参数。
            拔掉充电器后检测最完整（插着电时电流上限那项测不准，会被跳过）。
          </MiuixText>
        </div>
      </div>

      <div class="actions">
        <MiuixButton
          class="actions__btn"
          type="primary"
          :disabled="probing"
          @click="probe(true)"
        >
          {{ probing ? "检测中…" : "重新检测" }}
        </MiuixButton>
        <MiuixButton
          class="actions__btn"
          :disabled="probing"
          @click="confirmFull = true"
        >
          完整检测
        </MiuixButton>
      </div>

      <MiuixText
        type="footnote2"
        color="var(--m-color-on-surface-secondary)"
        class="probe__hint"
      >
        上次检测：{{ probedAt }}
      </MiuixText>

      <!-- 旧版档案提醒 -->
      <MiuixText
        v-if="isStale"
        type="footnote2"
        color="var(--m-color-error)"
        class="probe__hint"
      >
        这份档案由旧版检测生成（当时标称功率需插电实测），建议重新检测。
      </MiuixText>
    </MiuixCard>

    <!--
      ── 系统（已移除）──

      用户要求（2026-10-14）：「更多中的系统模块删除」。

      判断合理：这里只有三行**静态**信息，且对本模块的使用毫无影响 ——

      | 行 | 为什么不需要 |
      |---|---|
      | Android 版本 / API | 入门门槛（API 31）由安装脚本把关，装上了就说明满足 |
      | 设备代号 | 属于"身份"，已在状态页显示设备名 |
      | 内存页大小 | 只是 16 KB 对齐的**实现细节**，用户无从也无需操作 |

      ⇒ 删掉整张卡。想查的人可以用 `thermo device show`（README 有命令表）。
    -->

    <!-- ── 电池 ── -->
    <MiuixSmallTitle text="电池" />
    <MiuixCard class="rows">
      <InfoRow title="型号" summary="标称功率即从此型号读出">
        <template #end>
          <MiuixText type="footnote2" class="mono">
            {{ battery?.model_name || "—" }}
          </MiuixText>
        </template>
      </InfoRow>
      <InfoRow title="健康度">
        <template #end>
          <MiuixText type="body2">{{ f1(battery?.health_percent) }}%</MiuixText>
        </template>
      </InfoRow>
      <InfoRow title="循环次数">
        <template #end>
          <MiuixText type="body2">{{ f0(battery?.cycle_count) }}</MiuixText>
        </template>
      </InfoRow>
      <InfoRow title="材质">
        <template #end>
          <MiuixText type="body2">{{ battery?.technology || "—" }}</MiuixText>
        </template>
      </InfoRow>
    </MiuixCard>

    <!-- ── 充电能力 ── -->
    <MiuixSmallTitle text="充电能力（本机实测）" />
    <MiuixCard class="rows">
      <InfoRow
        title="档位范围"
        :summary="`档位表 ${f0(caps?.gear_table_rows)} 行`"
        :source="caps?.sources?.gear_max"
      >
        <template #end>
          <MiuixText type="body2">
            {{ f0(caps?.gear_min) }} ~ {{ f0(caps?.gear_max) }}
          </MiuixText>
        </template>
      </InfoRow>

      <InfoRow title="电流上限" :source="caps?.sources?.current_max">
        <template #end>
          <MiuixText type="body2">{{ amps1(caps?.current_max_ua) }} A</MiuixText>
        </template>
      </InfoRow>

      <InfoRow
        title="充电功率上限"
        summary="机型能力上限，用于限制可设定的目标功率"
        :source="caps?.sources?.power_max"
      >
        <template #end>
          <MiuixText
            type="body2"
            :color="
              caps?.power_max_w === null || caps?.power_max_w === undefined
                ? 'var(--m-color-on-surface-secondary)'
                : undefined
            "
          >
            {{ caps?.power_max_w === null || caps?.power_max_w === undefined
              ? "未读出"
              : `${f0(caps.power_max_w)} W` }}
          </MiuixText>
        </template>
      </InfoRow>

      <InfoRow
        title="功率最大档位"
        summary="电流上限最高的档（本机为 0，即全速档）"
      >
        <template #end>
          <MiuixText type="body2">{{ f0(caps?.power_max_gear) }}</MiuixText>
        </template>
      </InfoRow>

      <InfoRow
        title="硬件保护温度"
        summary="超过则必须收紧"
        :source="caps?.sources?.hardware_max_temp"
      >
        <template #end>
          <MiuixText type="body2">{{ f0(caps?.hardware_max_c) }} ℃</MiuixText>
        </template>
      </InfoRow>

      <MiuixDivider />

      <InfoRow title="充电上限支持">
        <template #end>
          <MiuixText
            type="body2"
            :color="
              caps?.soc_limit_supported
                ? 'var(--m-color-primary)'
                : 'var(--m-color-on-surface-secondary)'
            "
          >
            {{ caps?.soc_limit_supported ? "支持" : "不支持" }}
          </MiuixText>
        </template>
      </InfoRow>
    </MiuixCard>

    <!--
      ── 实际功率（已移除）──

      用户要求（2026-10-14）：「实际功率也删除，首页可以看到」。

      判断合理：状态页**已经**有实时功率（主卡片右下角 + 三格里的「功率 W」），
      而且是 3 秒一刷的**实时**值；而这里的「测量实际功率」要按一下、
      等 3 秒、只给一次瞬时峰值 —— 功能重叠且更笨拙。

      同理删掉了整张卡（含测量按钮）。后端 `thermo measure` 命令保留，
      需要精确测量时仍可从命令行调用（README 有说明）。
    -->

    <!--
      ── 界面性能（诊断）（已移除）──

      用户要求（2026-10-14）：「更多内还有个界面性能诊断现在也没用了，
      可以删除掉」。

      **判断合理** —— 删除有两个理由：

      | 理由 | 说明 |
      |---|---|
      | 用户不知道何时用 | "打开很慢 / 切页卡"是**转瞬即逝**的感受；等他翻到「更多」页，那一次卡顿早过去了，打点也早被后续操作淹没 |
      | 已经有更好的入口 | 「日志」页的「导出诊断日志」现在是**唯一的**诊断出口，而它已经会附带这些打点 |

      ⚠️ 但**不能只删按钮** —— `dumpPerfLog()` 还负责把界面异常的
      技术细节（`window.__errs`）落盘，那是 `main.ts` 对用户隐藏错误详情时
      唯一的出口。因此改为：**导出诊断日志时自动调用它**。
    -->

    <!-- ── 检测记录 ── -->
    <template v-if="notes.length > 0">
      <MiuixSmallTitle text="检测记录" />
      <MiuixCard>
        <div class="selectable notes">
          <div v-for="(n, i) in notes" :key="i" class="notes__item">
            <span class="notes__dot">·</span>
            <span>{{ n }}</span>
          </div>
        </div>
      </MiuixCard>
    </template>

    <!-- ── 完整检测确认 ── -->
    <MiuixDialog
      v-model="confirmFull"
      title="执行完整检测？"
      summary="完整检测会向控速节点试写一次边界值，以验证可写上限 —— 充电功率会短暂变化，随后立即恢复原值。若只想读取本机能力，用「重新检测」即可（零副作用）。"
      @close="confirmFull = false"
    >
      <template #default="{ close }">
        <div class="actions">
          <MiuixButton class="actions__btn" @click="close">取消</MiuixButton>
          <MiuixButton
            class="actions__btn"
            type="primary"
            @click="probe(false, true)"
          >
            确认执行
          </MiuixButton>
        </div>
      </template>
    </MiuixDialog>
  </div>
</template>

<style scoped>
.page {
  padding: 12px 12px 16px;
  display: flex;
  flex-direction: column;
  gap: 12px;
}

/* ── 机型头 ── */

.hero {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: 12px;
}

.hero__text {
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 2px;
}

/* ── 检测卡片 ── */

.probe {
  display: flex;
  gap: 10px;
  margin-bottom: 12px;
}

.probe__text {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 3px;
}

.probe__hint {
  display: block;
  margin-top: 8px;
  line-height: 1.5;
}

/* ── 通用 ── */

.mono {
  font-family: ui-monospace, monospace;
}

/* ── 实测功率结果 ── */

.result {
  margin-bottom: 12px;
  padding: 12px;
  border-radius: 12px;
  background: var(--m-color-surface-container-high);
}

.result__main {
  display: flex;
  align-items: baseline;
  gap: 6px;
}

.result__num {
  font-size: 34px;
  line-height: 1.1;
  font-variant-numeric: tabular-nums;
  color: var(--m-color-primary);
}

.result__unit {
  font-size: 14px;
  color: var(--m-color-on-surface-secondary);
}

.result__none {
  font-size: 16px;
  color: var(--m-color-on-surface-secondary);
}

.result__rows {
  margin-top: 10px;
  display: flex;
  flex-direction: column;
  gap: 4px;
}

.result__row {
  display: flex;
  justify-content: space-between;
  gap: 12px;
  font-size: 13px;
  line-height: 1.5;
}

.result__k {
  color: var(--m-color-on-surface-secondary);
}

.result__v {
  font-variant-numeric: tabular-nums;
  text-align: right;
}

.notes {
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.notes__item {
  display: flex;
  gap: 8px;
  font-size: 13px;
  line-height: 1.6;
  color: var(--m-color-on-surface-secondary);
  word-break: break-word;
}

.notes__dot {
  flex: none;
  color: var(--m-color-outline);
}
</style>
