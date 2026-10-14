<!--
  页签图标

  # 为什么自绘而不是用 miuix 图标

  miuix 的图标包有 156 个（名称如 `Tune` / `Report` / `Th1`…），
  但没有语义明确的"温度计""电池"图标，且 `Th1`~`Th31` 用途不明
  （无法从类型声明判断）。逐个猜测代价高于自绘。

  因此这四个页签图标用 Material Symbols 的路径 —— 它们是
  **实心填充风格**，与 miuix 图标的观感一致；尺寸、颜色都由父级
  （`MiuixNavigationBar`）控制，因此会自动跟随选中态配色。
-->

<script setup lang="ts">
import { computed } from "vue";

const props = defineProps<{
  /** 图标名 */
  name: string;
  /** 边长（px） */
  size?: number;
}>();

/**
 * 图标路径表
 *
 * 全部来自 Material Symbols（Apache-2.0）。
 */
const PATHS: Record<string, string> = {
  /** 仪表盘 —— 速度/状态 */
  speed:
    "M20.38 8.57l-1.23 1.85a8 8 0 0 1-.22 7.58H5.07A8 8 0 0 1 15.58 6.85l1.85-1.23A10 10 0 0 0 3.35 19a2 2 0 0 0 1.72 1h13.85a2 2 0 0 0 1.74-1 10 10 0 0 0-.27-10.44zm-9.79 6.84a2 2 0 0 0 2.83 0l5.66-8.49-8.49 5.66a2 2 0 0 0 0 2.83z",
  /** 带闪电的电池 —— 充电上限 */
  battery:
    "M15.67 4H14V2h-4v2H8.33C7.6 4 7 4.6 7 5.33v15.33C7 21.4 7.6 22 8.33 22h7.33c.74 0 1.34-.6 1.34-1.33V5.33C17 4.6 16.4 4 15.67 4zM11 20v-5.5H9L13 7v5.5h2L11 20z",
  /** 温度计 —— 温控 */
  thermostat:
    "M15 13V5c0-1.66-1.34-3-3-3S9 3.34 9 5v8c-1.21.91-2 2.37-2 4 0 2.76 2.24 5 5 5s5-2.24 5-5c0-1.63-.79-3.09-2-4zm-4-8c0-.55.45-1 1-1s1 .45 1 1h-1v1h1v2h-1v1h1v2h-2V5z",
  /** 手机 —— 设备信息 */
  device:
    "M17 1.01L7 1c-1.1 0-2 .9-2 2v18c0 1.1.9 2 2 2h10c1.1 0 2-.9 2-2V3c0-1.1-.9-1.99-2-1.99zM17 19H7V5h10v14z",
  /** 带指针的时钟 —— 日志/历史 */
  history:
    "M13 3c-4.97 0-9 4.03-9 9H1l3.89 3.89.07.14L9 12H6c0-3.87 3.13-7 7-7s7 3.13 7 7-3.13 7-7 7c-1.93 0-3.68-.79-4.94-2.06l-1.42 1.42A8.954 8.954 0 0 0 13 21c4.97 0 9-4.03 9-9s-4.03-9-9-9zm-1 5v5l4.28 2.54.72-1.21-3.5-2.08V8H12z",
};

/** 未知名字时退回第一个，避免整个图标消失 */
const path = computed(() => PATHS[props.name] ?? PATHS["speed"] ?? "");
const px = computed(() => props.size ?? 24);
</script>

<template>
  <svg
    :width="px"
    :height="px"
    viewBox="0 0 24 24"
    fill="currentColor"
    aria-hidden="true"
  >
    <path :d="path" />
  </svg>
</template>
