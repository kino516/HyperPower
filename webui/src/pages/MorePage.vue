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
  MiuixSmallTitle,
  MiuixSwitch,
  MiuixText,
  showSnackbar,
} from "miuix-vue";

import * as api from "../api";
import { batteryCapacityMah, f0, f1 } from "../format";
import { perfMark } from "../perf";
import { failedLabel } from "../types";
import type { DeviceProfileView } from "../types";

/** 当前生效档案 */
const device = ref<DeviceProfileView | null>(null);
const loading = ref(true);

/** 正在检测 */
const probing = ref(false);

/** 完整检测的确认弹窗 */
/**
 * 「同时验证档位可写上限」的勾选状态
 *
 * 勾上 = 完整检测（含写探，会短暂改变充电功率）；
 * 不勾 = 快速检测（全程只读，零副作用）。
 */
const verifyGear = ref(false);

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
const battery = computed(() => device.value?.battery);
const notes = computed(() => device.value?.probe_meta.notes ?? []);

/**
 * 「检测记录」的一行
 *
 * 由 [`parseNote`] 从后端给的**一整句**里拆出来。
 */
interface NoteRow {
  /** 左列名称；空串表示这条没有「名称：结论」结构，整句走右列 */
  name: string;
  /** 右列结论 */
  brief: string;
  /** 语气：决定徽标与颜色 */
  tone: "ok" | "skip" | "warn";
}

/**
 * 把后端的一句 `notes` 拆成「名称 + 结论 + 语气」
 *
 * # ⚠️ 这一步是**前端猜的**
 *
 * 后端目前给的是**纯字符串数组**（`probe_meta.notes`），没有结构信息。
 * 理想做法是后端直接给 `{name, brief, tone}` —— 那时这里的全部逻辑
 * 都可以删掉，界面也不会因为后端改一句措辞就退化。
 *
 * 之所以先这么办：改 `probe_meta` 的结构会影响 `device.json` 的**双向
 * 兼容**（新档案在旧版本模块上读不出 ⇒ 档案失效 ⇒ 停止控制充电），
 * 代价比"前端多猜一层"大得多。
 *
 * # 拆法
 *
 * | 输入 | 结果 |
 * |---|---|
 * | `档位表：15 档（…）` | name=`档位表`，brief=其余 |
 * | `硬件保护温度未探测到（…）` | name=空，brief=整句（右列占满） |
 *
 * 只认**第一个** `：`：像 `充电功率上限：100W（来源：电池型号…）` 这种
 * 内层还有冒号的，右侧保留完整内容。
 *
 * # 语气判据
 *
 * 用关键词而不是"看有没有 failed_items" —— 后者只覆盖"检测失败"，
 * 而"充电中跳过了电流上限"这类**主动跳过**并不在 failed_items 的语义里。
 * 关键词会随文案变化失效，但**失效的后果只是徽标颜色不对**，
 * 不会影响任何功能。
 */
function parseNote(raw: string): NoteRow {
  const text = raw.trim();
  let tone: NoteRow["tone"] = "ok";
  if (text.includes("跳过") || text.includes("已跳过")) {
    tone = "skip";
  } else if (
    text.includes("未探测到") ||
    text.includes("未能读出") ||
    text.includes("不可用") ||
    text.includes("失败")
  ) {
    tone = "warn";
  }

  const i = text.indexOf("：");
  // 名称过长就不是名称了（多半是整句里恰好有个冒号）
  if (i > 0 && i <= 12) {
    return { name: text.slice(0, i), brief: text.slice(i + 1), tone };
  }
  return { name: "", brief: text, tone };
}

/** 渲染用的行（后端每次检测只产出一次，这里缓存住避免模板里反复解析） */
const noteRows = computed<NoteRow[]>(() => notes.value.map(parseNote));

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
async function probe(): Promise<void> {
  if (probing.value) {
    return;
  }
  probing.value = true;
  // 勾选 = 用户已经确认过写探会改功率 ⇒ 不再弹二次对话框
  const full = verifyGear.value;

  try {
    const r = await api.probeDevice({
      quick: !full,
      save: true,
      confirm: full,
    });
    // ⚠️ 跑完**自动取消勾选**
    //
    // 它跑一次就够（见模板里的说明）。留着的话，用户下次点「重新检测」
    // 会**再次**扰动一次充电功率 —— 而他多半已经忘了自己勾过。
    verifyGear.value = false;
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
          @click="probe()"
        >
          {{ probing ? "检测中…" : "重新检测" }}
        </MiuixButton>
      </div>

      <!--
        ┌─ 档位上限验证（可选，默认关）──────────────────────────────┐

        🔴 用户 2026-10-17 要求：「重新检测与完整检测有什么不同？
           可以去掉只保留一个按钮吗」

        两者的差别**只有一项**：档位可写上限的**写探** ——
        其余（机型/电池/节点探测/档位表/电流上限/硬件温度/功率上限）
        **逐项相同**。

        因此把"完整检测"从**第二个按钮**降级成这个复选框：
        主操作只有一个，而能力一个没少。

        ⚠️ 为什么默认**关闭**：写探会试写档位节点 ⇒
           **充电功率会短暂变化**。默认勾上等于每次重跑都扰动一次充电。

        ⚠️ 为什么它**跑一次就够**：验证的是「档位表的推导上限是否真的
           可写」，而推导依据是**设备树里的档位表** —— 设备树不变 ⇒
           推导值不变 ⇒ 驱动接不接受也不变。
           之后每次重跑都是白跑，还会白白扰动一次功率。

        ⚠️ 勾选本身就是**确认**，因此不再弹二次确认对话框
           （原来那个 `confirmFull` 对话框已删）。
      -->
      <div class="row probe__opt">
        <div class="row__text">
          <MiuixText type="body2">同时验证档位可写上限</MiuixText>
          <MiuixText type="footnote2" color="var(--m-color-on-surface-secondary)">
            会短暂改变充电功率；跑一次就够，之后不必再勾
          </MiuixText>
        </div>
        <MiuixSwitch v-model="verifyGear" :disabled="probing" />
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
    <MiuixCard>
      <!--
        🔴 用户要求（2026-10-17）：
          「电池模块的信息优化显示效果」→「放到一列内」
          →「**不要显示三行，放到一行内**」

        最终形态：**横向一行三格**（与状态页的三格一致）。

        | 改动 | 原因 |
        |---|---|
        | 三行 `InfoRow` → **一行三格**（`.grid`） | 用户明确要求；也与状态页的三格视觉统一 |
        | 「型号」→「**容量**」 | 型号 `P11U_7560mah_100w` 是内部编码，用户读不懂；而它里面正好含容量 |
        | 删「材质」 | `Li-poly` 没有可操作价值 |

        ⚠️ 用全局的 `.grid` / `.grid--center`（定义在 `theme.css`）。
        本页**不要**再 scoped 重定义 —— 那正是 2026-10-17
        「状态页三格变两行」的事故原因。
      -->
      <!--
        ⚠️ 这三项是**检测（归档）**数据，不是实时读取
        —— 这是用户 2026-10-17 明确确认的设计，不是缺陷。

        | | |
        |---|---|
        | 读取时机 | 检测的第 2 步（`[2/10] 读取电池信息`），整个检测只读一次 |
        | 存在哪 | `device.json` 的 `battery` 段 |
        | 界面读哪 | `device.value?.battery`（档案，不是现场采样） |
        | 怎么更新 | 用户**手动跑一次检测** |

        # 为什么不做成实时

        三项的**变化速度差两个数量级**：

        | 项 | 来源 | 变化 |
        |---|---|---|
        | 容量 | `charge_full_design` | **永不变**（出厂值） |
        | 健康度 | `charge_full ÷ design` | 月级 |
        | 循环次数 | `cycle_count` | **每次充放电 +1** |

        循环次数确实"每天都在变"，所以曾考虑单独开一条实时读取的路
        （新增轻量命令，打开本页时读一次）。**用户决定不做** ——
        理由是"更新电池信息我手动运行一次检测就行"。

        ⇒ 因此下面的「上次检测：xx:xx」那行小字**同时**承担了
          "这三项有多新"的提示职责。**不要删它**。

        # 将来若要改

        先注意 `charge_full`（满电容量）**在充电中读可能不准** ——
        与「电流上限」同一个道理（那时节点反映的是当前充电状态，
        而非标称值）。要改的话须先用真机在各充电状态下验证读数。
      -->
      <div class="grid grid--center">
        <div class="grid__cell">
          <MiuixText type="title3">
            {{ batteryCapacityMah(battery?.model_name) }}
          </MiuixText>
          <MiuixText type="footnote2" color="var(--m-color-on-surface-secondary)">
            容量
          </MiuixText>
        </div>
        <div class="grid__cell">
          <MiuixText type="title3">{{ f1(battery?.health_percent) }}%</MiuixText>
          <MiuixText type="footnote2" color="var(--m-color-on-surface-secondary)">
            健康度
          </MiuixText>
        </div>
        <div class="grid__cell">
          <MiuixText type="title3">{{ f0(battery?.cycle_count) }}</MiuixText>
          <MiuixText type="footnote2" color="var(--m-color-on-surface-secondary)">
            循环次数
          </MiuixText>
        </div>
      </div>
    </MiuixCard>

    <!--
      ── 充电能力（已移除）──

      🔴 用户要求（2026-10-17）：「充电能力与检测记录信息重复，删除掉充电能力」

      逐项核对确实**六项全重复**：

      | 项 | 原充电能力卡片 | 「检测记录」 |
      |---|---|---|
      | 档位范围 | `0 ~ 15` | `档位表：15 档（0~14）` |
      | 电流上限 | `18.4 A` | `电流上限：…` |
      | 充电功率上限 | `100 W` | `充电功率上限：100W · 来源…` |
      | 功率最大档位 | `0` | `最大功率档位：0` |
      | 硬件保护温度 | `48 ℃` | `硬件保护温度：48℃ · 设备树直读` |
      | 充电上限支持 | `支持` | `SOC 上限：节点可用` |

      而且「检测记录」还多给一样东西：**来源**与**未成功项**
      （原卡片只在标题旁挂一个小小的来源徽标）。

      ⚠️ 删掉后**没有信息丢失**：
        · 数值仍在 `device.json` 的 `capabilities` 段
        · `ctl device show` / 导出报告里都能看到
        · 检测记录里以人话复述了关键几项
    -->


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
    <template v-if="noteRows.length > 0">
      <MiuixSmallTitle text="检测记录" />
      <MiuixCard>
        <!--
          ┌─ 状态摘要（用户 2026-10-17 从 3 个方案里选定）──────────────┐

          改前是「圆点 + 一律灰色 13px 的长句子」：

              ·  （演示模式：以下为示例数据）
              ·  标称充电功率：100W（来源：电池型号 P11U_7560mah_100w）
              ·  硬件保护温度：48℃

          问题：没有层次（全是同样的灰）、长句难扫读、而且
          **看不出哪项成功、哪项被跳过** —— 而那恰恰是用户最想知道的。

          现在每条：一个 ✓/⚠ 徽标 + 左列名称 + 右列一句话结论。
        -->
        <div class="selectable notes">
          <div v-for="(r, i) in noteRows" :key="i" class="notes__row">
            <span class="notes__mark" :class="`notes__mark--${r.tone}`">
              {{ r.tone === "ok" ? "✓" : r.tone === "skip" ? "!" : "✕" }}
            </span>
            <span v-if="r.name" class="notes__name">{{ r.name }}</span>
            <span class="notes__brief" :class="{ 'notes__brief--wide': !r.name }">
              {{ r.brief }}
            </span>
          </div>
        </div>
      </MiuixCard>
    </template>

    <!--
      ⚠️ 这里原有「执行完整检测？」的二次确认对话框，已删除（2026-10-17）

      它服务的是原来那个独立的「完整检测」按钮。现在完整模式由
      「同时验证档位可写上限」复选框触发 —— **勾选本身即确认**，
      再弹一次对话框就成了"确认两遍"。
    -->
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

/* 档位上限验证的开关行：与上面的按钮拉开一点距离 */
.probe__opt {
  margin-top: 14px;
  padding-top: 4px;
}

.probe__hint {
  display: block;
  margin-top: 8px;
  line-height: 1.5;
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

/* ── 检测记录：状态摘要行 ── */

.notes {
  display: flex;
  flex-direction: column;
}

.notes__row {
  display: flex;
  align-items: flex-start;
  gap: 9px;
  padding: 10px 0;
  font-size: 13.5px;
  line-height: 1.5;
}

/* 条目之间的分隔线（最后一条不要） */
.notes__row + .notes__row {
  border-top: 1px solid var(--m-color-outline-variant);
}

/**
 * 状态徽标
 *
 * ⚠️ 三种语气各配一个**色底圆点 + 白字**，而不是只给文字上色：
 *    纯色文字在"全是灰"的卡片里不够醒目，而这一列的**全部价值**
 *    就是让用户一眼扫出"哪项不对"。
 *
 * 用固定色值而不是主题色：主题色板里没有"成功绿 / 警告橙"这两个语义色
 * （只有 primary / error），而这里缺了它们整个设计就不成立。
 */
.notes__mark {
  flex: none;
  width: 18px;
  height: 18px;
  line-height: 18px;
  text-align: center;
  border-radius: 50%;
  font-size: 11px;
  font-weight: 700;
  color: #fff;
  margin-top: 1px;
}

/* 成功：绿 */
.notes__mark--ok {
  background: #2e7d32;
}

/* 主动跳过：橙（不是错误，但需要用户知道"这项没测到"） */
.notes__mark--skip {
  background: #ed6c02;
}

/* 真失败：红 */
.notes__mark--warn {
  background: var(--m-color-error);
}

.notes__name {
  flex: none;
  /* 固定宽度让左右两列对齐；名称过长由 `parseNote` 的 12 字上限挡住 */
  width: 96px;
  color: var(--m-color-on-surface);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.notes__brief {
  flex: 1;
  /* ⚠️ 不可省：长路径（如 /sys/firmware/devicetree/…）会把行撑破 */
  min-width: 0;
  text-align: right;
  color: var(--m-color-on-surface-secondary);
  word-break: break-word;
}

/* 没有名称的行（整句式，如"硬件保护温度未探测到…"）：整句占满，左对齐 */
.notes__brief--wide {
  text-align: left;
}
</style>
