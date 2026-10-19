<!--
  只读信息行

  # 为什么不用 `MiuixArrowPreference`

  那个组件自带 `>` 箭头 —— 在 MIUIX 设计语言里箭头意味着「点进去还有内容」。
  但本模块的这些行（档位、电流上限、硬件保护温度…）**纯粹是展示数据**，
  点了没有任何反应。留着箭头等于给用户一个假的交互承诺。

  本组件**接口与 `MiuixArrowPreference` 完全一致**（`title` / `summary` / `#end` 插槽），
  因此替换是机械的 —— 也便于将来某个行真的要变成可点击时改回去。

  # 视觉规格

  照 MIUIX 的设置项规范：

  | 元素 | 规格 |
  |---|---|
  | 行高 | 最小 56px（触摸目标足够大） |
  | 标题 | 15px，主文字色 |
  | 摘要 | 13px，次要文字色，两行截断 |
  | 取值 | 15px，右对齐，次要文字色 |
  | 分隔线 | 用 `MiuixDivider` 显式插入（不做自动分隔，避免与卡片内边距打架） |
-->

<script setup lang="ts">
import { computed } from "vue";
import { MiuixText } from "miuix-vue";

import { SOURCE_LABEL, isLocalSource, type ValueSource } from "../types";

const props = defineProps<{
  /** 行标题 */
  title?: string;
  /** 副标题（说明文字） */
  summary?: string;
  /**
   * 值的来源标记
   *
   * 显示为标题旁的小徽标。不可信来源用次要色，可信来源用主色 ——
   * 让用户一眼区分"这个数是直读的，还是推断的"。
   */
  source?: ValueSource;
}>();

/** 来源标签文字（`unknown` 不显示） */
const sourceLabel = computed(() => {
  const s = props.source;
  if (!s || s === "unknown") {
    return "";
  }
  return SOURCE_LABEL[s] ?? "";
});

/**
 * 是否为**本机**数据
 *
 * 本机（节点/电池型号/设备树）用主色徽标；
 * 他人档案或默认值用次要色 —— 提示"这个数不一定代表你的机器"。
 *
 * ⚠️ 设备树算本机数据：档位表（以及部分机型的硬件温度）只能从设备树读；
 *    没有设备树的机型则读节点（来源标 `node`，同样算本机）。
 */
const trusted = computed(() => isLocalSource(props.source));
</script>

<template>
  <div class="info-row">
    <div class="info-row__main">
      <div class="info-row__head">
        <MiuixText v-if="title" type="body1" class="info-row__title">
          {{ title }}
        </MiuixText>
        <!-- 来源徽标：只在有值时渲染 -->
        <span
          v-if="sourceLabel"
          class="badge"
          :class="trusted ? 'badge--trusted' : 'badge--weak'"
        >
          {{ sourceLabel }}
        </span>
      </div>
      <MiuixText
        v-if="summary"
        type="footnote1"
        color="var(--m-color-on-surface-secondary)"
        class="info-row__summary"
      >
        {{ summary }}
      </MiuixText>
    </div>
    <!-- 取值区：由调用方决定放什么（文字 / 徽标 / 颜色） -->
    <div v-if="$slots.end" class="info-row__end">
      <slot name="end" />
    </div>
  </div>
</template>

<style scoped>
.info-row {
  display: flex;
  align-items: center;
  gap: 12px;
  /* 最小 56px：符合触摸目标规范，也让密集的列表不显得拥挤 */
  min-height: 56px;
}

.info-row__main {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 2px;
}

.info-row__head {
  display: flex;
  align-items: center;
  gap: 6px;
  min-width: 0;
}

.badge {
  flex: none;
  padding: 1px 6px;
  border-radius: 6px;
  font-size: 11px;
  line-height: 1.6;
  white-space: nowrap;
}

/* 直读/实测：主色，表示可信 */
.badge--trusted {
  background: var(--m-color-primary-container);
  color: var(--m-color-on-primary-container);
}

/* 推断/兜底：次要色，提示"仅供参考" */
.badge--weak {
  background: var(--m-color-surface-container-high);
  color: var(--m-color-on-surface-secondary);
}

.info-row__title {
  /* 标题过长时截断而不是撑破布局 */
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.info-row__summary {
  /* 摘要最多两行 */
  display: -webkit-box;
  -webkit-line-clamp: 2;
  line-clamp: 2;
  -webkit-box-orient: vertical;
  overflow: hidden;
  line-height: 1.45;
}

.info-row__end {
  flex: none;
  display: flex;
  align-items: center;
  gap: 6px;
  /* 取值与标题区之间留出最小间隙 */
  margin-left: auto;
}
</style>
