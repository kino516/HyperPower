<!--
  SafeSlider —— 给 miuix 滑块补上「手势方向判定」

  # 为什么要包一层

  `miuix-vue@0.1.1` 的滑块（`MiuixSlider`）有两个行为，在**垂直滚动的页面**里
  会互相叠加成"误触"：

  | # | 库的行为（证据） | 后果 |
  |---|---|---|
  | 1 | `.m-slider { touch-action: none }`（`dist/miuix-vue.css`） | 手指落在滑轨上时，**浏览器不接管纵向滚动** —— 页面滚不动，这次触摸被滑块吃掉 |
  | 2 | `pointerdown` 里直接 `fe(e)` 按触点位置改值（`dist/miuix-vue.js` 的 `me()`） | 没有方向判定、没有位移阈值、没有"先观察再响应"，手指搭上去值就跳了 |

  真机症状就是：想上下滚页面，结果页面不动、滑块数值乱跳、还弹出一排
  「应用 / 取消」按钮把内容顶下去。

  Android 原生滑块（Compose `Slider`）不是这么做的：它放在滚动容器里时，
  **先判断方向**，只有横向拖动才消费事件，纵向交给父级滚动。这个组件就是
  把这套「手势协商」补在应用层。

  # 为什么不在 node_modules 里改

  `npm install` 会覆盖；且这属于第三方库代码，改它无法在构建门禁和评审里
  留下证据。包一层 `<div>` 只依赖公开的 props / 事件，升级库时只需重跑测试。

  # 判定规则（阈值见下方 `SLOP_PX`）

  | 手势 | 判定 | 处理 |
  |---|---|---|
  | 先纵向移动超过 8px，且纵向位移 > 横向 | **滚动** | 本次所有值变化全部丢弃，页面正常滚 |
  | 先横向移动超过 8px，且横向 ≥ 纵向 | **拖动** | 从这一刻起直通，值跟随手指 |
  | 位移都没超过 8px 就松手 | **点击** | 默认**不生效**（`tapToJump = false`） |

  # 🔴 关键实现：判定完成前不更新 `inner`

  最容易写错的版本是"先让库改值，发现是滚动再回滚" —— 那样用户会看到
  数值跳一下又弹回、按钮闪一下又消失，比误触本身更难受。

  这里的做法是：判定完成之前，**不把库 emit 的值写回 `inner`**（`inner` 是传给
  `MiuixSlider` 的 `modelValue`）。库的值确实变了，但它的显示值由 props 驱动
  （`dist/miuix-vue.js` 里 `G` 只在 props 变化时更新），所以**屏幕上一动不动**。
  判定为拖动时再把缓存值一次性提交。

  # 惯性滚动抑制（`SCROLL_GUARD_MS`）

  另一类误触是"页面还在惯性滚动，点一下想让它停" —— 手指落点恰好是滑块。
  这种情况位移几乎为 0，会被当成点击。因此在 `window` 上捕获 `scroll`
  （scroll 不冒泡，必须用 capture），若按压发生在最近 200ms 内滚动过，
  本次手势一律视为"点停滚动"：滑块不响应，也不给按下反馈。

  ⚠️ 200ms 是**滚动结束后**的窗口，不是"滚动中"的窗口。用户停下 200ms 后
  再点滑块，照常可用。

  # 与库事件的关系

  - `update:modelValue` / `change`：由本组件**代发**（判定通过后才发），
    所以父层拿到的永远是"用户真的想改"的值；
  - `change-end`：原样转发（库在 pointerup / pointercancel 时自己会发）。
-->
<script setup lang="ts">
import { onBeforeUnmount, ref, watch } from "vue";
import { MiuixSlider } from "miuix-vue";

const props = withDefaults(
  defineProps<{
    modelValue?: number;
    min?: number;
    max?: number;
    step?: number;
    disabled?: boolean;
    orientation?: "horizontal" | "vertical";
    reverseDirection?: boolean;
    showKeyPoints?: boolean;
    keyPoints?: number[];
    magnetThreshold?: number;
    /**
     * 是否允许「点滑轨跳值」
     *
     * 默认 **false**：只能按住拇指拖动。
     *
     * 2026-11-06 用户确认：控制页上下滚动时误触的主要来源就是"点滑轨立即
     * 跳值"，因此默认关掉。若将来某个页面需要点哪跳哪，显式传 `true` 即可。
     */
    tapToJump?: boolean;
  }>(),
  {
    modelValue: 0,
    min: 0,
    max: 100,
    step: 0,
    disabled: false,
    orientation: "horizontal",
    reverseDirection: false,
    showKeyPoints: false,
    keyPoints: () => [] as number[],
    magnetThreshold: 0.02,
    tapToJump: false,
  },
);

const emit = defineEmits<{
  (e: "update:modelValue", value: number): void;
  (e: "change", value: number): void;
  (e: "change-end"): void;
}>();

/** 方向判定阈值（px）：小于它视为"还没决定是滚动还是拖动" */
const SLOP_PX = 8;

/** 滚动刚结束的抑制窗口（ms）：此窗口内的按压只当"点停滚动" */
const SCROLL_GUARD_MS = 200;

/**
 * 传给 `MiuixSlider` 的值
 *
 * ⚠️ 手势判定完成前**不更新它** —— 这是"不闪"的关键，见文件头注释。
 */
const inner = ref(props.modelValue);

/** 手势状态（不需要驱动视图，用普通变量即可） */
let active = false;
let mode: "idle" | "undecided" | "drag" | "scroll" = "idle";
let startX = 0;
let startY = 0;
let pointerId = -1;
/** 最近一次库 emit 的值（判定通过后才提交） */
let candidate: number | null = null;
/** 手势期间父层刷新过值（例如 apply 后 refresh），结束后补同步 */
let pendingParent: number | null = null;

/**
 * 最近一次页面滚动的时间戳
 *
 * 用 `performance.now()`（单调时钟），不能用 `Date.now()`：系统时间会被
 * 校时修改，差值可能为负。
 */
let lastScrollAt = Number.NEGATIVE_INFINITY;
let scrollListenerReady = false;

function onAnyScroll() {
  lastScrollAt = performance.now();
}

/**
 * 只在第一个实例挂载时注册一次全局滚动监听
 *
 * 多个滑块共享同一个时间戳即可；组件卸载时**不摘除** —— 它没有引用任何
 * 组件状态，只是一个时间戳写入器，常驻的成本可以忽略。
 */
function ensureScrollListener() {
  if (scrollListenerReady) {
    return;
  }
  scrollListenerReady = true;
  // ⚠️ scroll 事件不冒泡，必须在捕获阶段监听 window 才能收到滚动容器的事件
  window.addEventListener("scroll", onAnyScroll, {
    capture: true,
    passive: true,
  });
}

ensureScrollListener();

onBeforeUnmount(() => {
  // 卸载时若手还在屏幕上，把 window 上的临时监听摘掉，避免泄漏
  if (active) {
    stopWindowListeners();
  }
});

/**
 * 父层值变化时同步
 *
 * 手势进行中不同步：拖动时父层会收到我们自己 emit 的值，props 随之更新，
 * 这时若直接覆盖 `inner` 会打断滑动过程；记录到 `pendingParent`，结束后处理。
 */
watch(
  () => props.modelValue,
  (value) => {
    if (active) {
      pendingParent = value;
      return;
    }
    inner.value = value;
  },
);

/** 提交一个值给父层（判定通过后调用） */
function commit(value: number) {
  candidate = null;
  inner.value = value;
  emit("update:modelValue", value);
  emit("change", value);
}

function stopWindowListeners() {
  window.removeEventListener("pointermove", onPointerMove);
  window.removeEventListener("pointerup", onPointerUp);
  window.removeEventListener("pointercancel", onPointerCancel);
}

/**
 * 按下：只记录起点，**不改任何值**
 *
 * ⚠️ 必须用捕获阶段（模板里的 `@pointerdown.capture`）：
 * `MiuixSlider` 的值更新挂在**它自己的滑轨**上（冒泡阶段）。
 * 若本组件也在冒泡阶段监听，顺序是「库先改值 → 我们再判定」，
 * 判定之前的瞬间值已经跳了。捕获阶段先于滑轨执行，才能拦住。
 */
function onPointerDown(event: PointerEvent) {
  if (props.disabled) {
    return;
  }
  // 右键 / 中键不是调整滑块的操作
  if (event.pointerType === "mouse" && event.button !== 0) {
    return;
  }
  // 多指：只认主指针，其余一律不交给滑块（否则库里的 pointerId 会被覆盖）
  if (!event.isPrimary || active) {
    event.stopPropagation();
    return;
  }

  active = true;
  mode = "undecided";
  pointerId = event.pointerId;
  startX = event.clientX;
  startY = event.clientY;
  candidate = null;
  pendingParent = null;

  if (performance.now() - lastScrollAt < SCROLL_GUARD_MS) {
    // 刚滚完就按下来：这是"点停滚动"，不是调值。
    // 阻止事件到达滑轨，连按下反馈都不出，避免用户以为值被改了。
    mode = "scroll";
    event.stopPropagation();
  }

  // 监听挂在 window 上：库会 setPointerCapture 把后续事件锁到滑轨，
  // 但事件仍然会冒泡到 window，这里能稳定收到。
  window.addEventListener("pointermove", onPointerMove, { passive: true });
  window.addEventListener("pointerup", onPointerUp);
  window.addEventListener("pointercancel", onPointerCancel);
}

function onPointerMove(event: PointerEvent) {
  if (!active || event.pointerId !== pointerId) {
    return;
  }

  const dx = event.clientX - startX;
  const dy = event.clientY - startY;
  const absX = Math.abs(dx);
  const absY = Math.abs(dy);

  if (mode !== "undecided") {
    return;
  }

  // 纵向优先：用户想滚页面时，手指不会横向走太远
  if (absY > SLOP_PX && absY > absX) {
    mode = "scroll";
    candidate = null;
    return;
  }

  // 横向优先：判定为拖动，把已经缓存的值一次性提交，之后直通
  if (absX > SLOP_PX && absX >= absY) {
    mode = "drag";
    if (candidate !== null) {
      commit(candidate);
    }
  }
}

function onPointerUp(event: PointerEvent) {
  if (!active || event.pointerId !== pointerId) {
    return;
  }

  // 位移没超过阈值 = 点击滑轨。默认不生效（见 `tapToJump` 说明）
  if (mode === "undecided" && props.tapToJump && candidate !== null) {
    commit(candidate);
  }

  finish();
}

function onPointerCancel(event: PointerEvent) {
  if (!active || event.pointerId !== pointerId) {
    return;
  }
  // 浏览器接管滚动时会 pointercancel：丢掉这次手势，值保持原样
  finish();
}

function finish() {
  stopWindowListeners();

  const wasDrag = mode === "drag";
  active = false;
  mode = "idle";
  pointerId = -1;
  candidate = null;

  // 拖动期间父层的刷新值（如 apply 后回读）在拖动分支会被我们的值覆盖，
  // 这里不要抢回来；其余分支同步为父层的最新值。
  if (!wasDrag && pendingParent !== null) {
    inner.value = pendingParent;
  }
  pendingParent = null;
}

/**
 * 库的值变化
 *
 * - 没有手势在跑（键盘 / 程序赋值）：直接转发；
 * - 判定为拖动：直通；
 * - 其余（未判定 / 判定为滚动 / 被抑制）：只缓存，**不动显示、不通知父层**。
 */
function onInnerUpdate(value: number) {
  if (!active) {
    commit(value);
    return;
  }
  if (mode === "drag") {
    commit(value);
    return;
  }
  candidate = value;
}

function onInnerChangeEnd() {
  emit("change-end");
}
</script>

<template>
  <div class="safe-slider" @pointerdown.capture="onPointerDown">
    <MiuixSlider
      :model-value="inner"
      :min="props.min"
      :max="props.max"
      :step="props.step"
      :disabled="props.disabled"
      :orientation="props.orientation"
      :reverse-direction="props.reverseDirection"
      :show-key-points="props.showKeyPoints"
      :key-points="props.keyPoints"
      :magnet-threshold="props.magnetThreshold"
      @update:model-value="onInnerUpdate"
      @change-end="onInnerChangeEnd"
    />
  </div>
</template>

<style scoped>
/*
 * 只负责把滑轨包成独立的一行：
 * 纵向手势的放行靠 `theme.css` 里对 `.m-slider` 的 `touch-action: pan-y` 覆盖，
 * 不能写在这里 —— scoped 选择器选不到子组件内部元素（库的滑轨）。
 */
.safe-slider {
  display: block;
}
</style>
