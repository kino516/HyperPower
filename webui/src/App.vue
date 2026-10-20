<!--
  应用外壳

  ┌─────────────────────────┐
  │ TopAppBar（标题 + 刷新）   │  ← 吸顶
  ├─────────────────────────┤
  │                         │
  │  页面容器（每页独立滚动）   │
  │                         │
  ├─────────────────────────┤
  │ NavigationBar（四页签）    │  ← 吸底
  └─────────────────────────┘

  # 页面切换方案的三次演进

  ## ① `Transition` + `KeepAlive`（照抄参考项目）→ 静默失败

  ```vue
  <Transition mode="out-in">
    <KeepAlive><component :is="currentPage" /></KeepAlive>
  </Transition>
  ```

  真机实测**内容区一片空白**，且**没有任何报错**。问题：

  | # | 问题 |
  |---|---|
  | 1 | `Transition` 要求直接子元素是**单个**元素，包 `KeepAlive` 时行为依赖版本实现 |
  | 2 | `mode="out-in"` 与 `KeepAlive` 的缓存命中路径有已知冲突 |
  | 3 | 出错时静默失败 —— 最难排查的一类 |

  ## ② 四页全部 `v-show` → 能用，但**首屏很慢**

  四个页面全部挂载。真机实测用户反馈「白屏时间非常长」。代价：

  | # | 代价 |
  |---|---|
  | 1 | 首屏要渲染 4 个页面的**全部** DOM 与 miuix 组件 |
  | 2 | 每页 `onMounted` 各发自己的命令 ⇒ 启动瞬间并发 **6 条** `ctl` |
  | 3 | 每条 `ctl` 要起一个 root shell **加**一个 `thermo` 进程 |

  ## ③ 当前：`v-show`（显示）+ `v-if` + `visited`（首次访问才挂载）

  ```vue
  <div v-for="tab in TABS" v-show="currentKey === tab.key" class="page-scroll">
    <component :is="PAGES[tab.key]" v-if="visited[tab.key]" />
  </div>
  ```

  | 好处 | 说明 |
  |---|---|
  | 首屏极轻 | 只渲染状态页、只发 1~2 条命令 |
  | 组件在可见时初始化 | 滑块等需要测量宽度的组件，首次渲染时父容器已是 `display: block` |
  | 状态保留 | 访问过的页面靠 `v-show` 留在 DOM 里，滚动位置与表单输入都在 |
  | 结构简单 | 仍然没有 `Transition` / `KeepAlive`，避开 ① 的坑 |

  唯一代价：首次切到某页时需要挂载（通常 <100ms，且该页自带 loading 态）。
-->

<script setup lang="ts">
import {
  computed,
  defineAsyncComponent,
  defineComponent,
  h,
  onBeforeUnmount,
  onMounted,
  reactive,
  ref,
  watch,
  type Component,
  type PropType,
} from "vue";
import {
  MiuixIcon,
  MiuixIconButton,
  MiuixNavigationBar,
  MiuixSnackbarHost,
  MiuixTopAppBar,
} from "miuix-vue";
import { Refresh } from "miuix-vue/icons";

import StatusPage from "./pages/StatusPage.vue";
import TabIcon from "./components/TabIcon.vue";

import { perfMark } from "./perf";
import { refresh, sysState } from "./stores/sys";

// ============================================================
//  异步页面（代码分割）用的占位组件
// ============================================================
//
// 🔴 **这两个必须定义在 `asyncPage` 调用之前**（真机白屏事故，2026-10-14）
//
// `const` 有**暂时性死区（TDZ）**：在定义语句执行前读取会抛
// `ReferenceError: Cannot access 'X' before initialization`。
//
// 而 `asyncPage()` 在**被调用时**就会读取 `loadingComponent` /
// `errorComponent` —— 所以如果把这两个常量放在调用点**之后**：
//
// ```ts
// const ControlPage = asyncPage(…);   // ← 这里立即读 AsyncLoading
// const AsyncLoading = () => h(…);    // ← 太晚了，此时还在 TDZ
// ```
//
// 后果是**模块顶层同步抛错** ⇒ App 组件树整个塌掉 ⇒ 界面全白。
//
// 最坑的是没有任何自动检查能抓住它：
//
// | 检查 | 结果 |
// |---|---|
// | `vue-tsc` | ✅ 通过（TDZ 不是类型问题） |
// | `vite build` | ✅ 成功 |
// | 产物语法检查 `node --check` | ✅ 合法 |
//
// 只有**真的把页面加载一次**才会暴露。因此本项目的前端改动，
// 必须在浏览器里实际打开验证（见 docs/11 §5.4.8）。

/**
 * 异步 chunk 加载中的占位
 *
 * 本地 chunk 通常几十毫秒就绪，因此刻意**不显示转圈** ——
 * 一闪而过的 spinner 比留白更让人眼花。这里只占位不显示内容。
 *
 * # ⚠️ 必须用 `h()`，不能写 `template: "<div>…</div>"`
 *
 * 本项目用的是 Vue 的 **runtime-only** 构建
 * （`vue.runtime.esm-bundler.js`，见 `node_modules/vue/package.json`
 * 的默认导出）—— 它**不含模板编译器**，因此 `template` 字符串无法编译。
 *
 * 而生产构建**剥掉了 Vue 的警告**，于是失败是**完全静默**的：
 * 渲染该组件时抛错 → 父组件渲染失败 → 整棵树塌掉 → 全白且无日志。
 *
 * 改用 `h()`（手写 render 函数）后不再依赖编译器。
 * 构建期有门禁守着（`vite.config.ts` 的 `forbidRuntimeTemplate`）。
 */
const AsyncLoading = () => h("div", { class: "page-loading" });

/**
 * 异步 chunk 加载失败的占位
 *
 * 显示原因而不是空白 —— 真机上没有控制台，静默失败无法排查。
 * `errorComponent` 会把失败原因作为 `error` prop 传进来。
 */
const AsyncFailed = defineComponent({
  props: {
    error: { type: Object as PropType<Error | null>, default: null },
  },
  setup(props) {
    return () =>
      h("div", { class: "page-failed" }, [
        h("p", "这一页没能加载出来。"),
        h(
          "p",
          { class: "page-failed__hint" },
          "模块文件可能不完整（WebUI 资源缺失）。可尝试重新安装模块。",
        ),
        h(
          "p",
          { class: "page-failed__msg" },
          props.error?.message ?? "未知原因",
        ),
      ]);
  },
});

/**
 * 非首屏页面用**异步组件**（代码分割）
 *
 * # 为什么（真机实测"白屏很久 / 切页卡"）
 *
 * 初版四个页面全是**静态导入** ⇒ 全部编译进 `index.js`，首屏必须把它
 * 整块下载 + 解析 + 执行完才能挂载。而两个重页面占了其中大部分体积：
 *
 * | 页面 | 源码行数 | 说明 |
 * |---|---|---|
 * | `LogsPage` | 1407 | 事件流 + 手写 SVG 图表 + 会话列表，四页里最重 |
 * | `ControlPage` | 1196 | 滑块 / 开关 / 模式切换 / 上限设置 |
 * | `MorePage` | 628 | 设备档案 + 检测 + 关于 |
 *
 * 改为异步后，它们各自成为一个独立 chunk，**首屏只需要** App 外壳 +
 * 状态页。用户点开某页时才取那个 chunk（本地磁盘，毫秒级）。
 *
 * # ⚠️ 必须给 `errorComponent`
 *
 * `defineAsyncComponent` 的 chunk 加载失败时，**默认什么都不渲染** ——
 * 表现为"点了页签，内容区永久空白，且没有任何提示"。这与项目
 * "不静默失败"的原则冲突，也极难排查（真机上没有控制台）。
 * 因此显式给一个报错占位。
 *
 * # 与「空闲预挂载」的配合
 *
 * 异步组件只在**真正渲染**时才开始下载。因此下面 `schedulePreload`
 * 会逐个把页签标记为已访问 —— 那既触发 chunk 下载、也完成组件挂载，
 * 把这份开销移出用户的可见路径（详见该函数）。
 */
const asyncPage = (loader: () => Promise<Component>) =>
  defineAsyncComponent({
    loader,
    loadingComponent: AsyncLoading,
    errorComponent: AsyncFailed,
    // 本地 chunk 很快；延迟太久说明真的出了问题
    delay: 120,
  });

const ControlPage = asyncPage(() => import("./pages/ControlPage.vue"));
const LogsPage = asyncPage(() => import("./pages/LogsPage.vue"));
const MorePage = asyncPage(() => import("./pages/MorePage.vue"));

/**
 * 页签定义（顺序即导航栏顺序）
 *
 * # 为什么是这四个
 *
 * 按**用户的操作意图**分组，而不是按功能模块：
 *
 * | 页签 | 回答用户的什么问题 | 装什么 |
 * |---|---|---|
 * | 状态 | **现在怎么样？** | 实时读数（温度/功率/电量/档位） |
 * | 控制 | **我要它怎么工作？** | 模式切换、温度阈值、功率上限、充电上限 |
 * | 日志 | **刚才发生了什么？** | 事件流 + 每次充电的历史曲线 |
 * | 更多 | **这台机器什么情况？** | 设备档案、检测、关于 |
 *
 * 旧的四页签是「状态 / 上限 / 温控 / 设备」—— 把"控制类"的设置
 * 拆在两个页里（上限、温控），用户要调一个充电策略得来回切。
 * 现在控制类集中在一页，日志与历史也有了自己的位置。
 */
const TABS = [
  { key: "status", label: "状态", icon: "speed" },
  { key: "control", label: "控制", icon: "thermostat" },
  { key: "logs", label: "日志", icon: "history" },
  { key: "more", label: "更多", icon: "device" },
] as const;

type TabKey = (typeof TABS)[number]["key"];

/** 页签 → 页面组件 */
const PAGES: Record<TabKey, Component> = {
  status: StatusPage,
  control: ControlPage,
  logs: LogsPage,
  more: MorePage,
};

const navIndex = ref(0);

/**
 * 已访问过的页签
 *
 * # 为什么需要这个（性能）
 *
 * 初版四个页面全部挂载（`v-show` 切换），真机实测**首屏很慢**。原因：
 *
 * | # | 代价 |
 * |---|---|
 * | 1 | 首屏要渲染 4 个页面的**全部** DOM 和 miuix 组件 |
 * | 2 | 每个页面在 `onMounted` 里发自己的命令 ⇒ 启动瞬间并发 **6 条** `ctl` |
 * | 3 | 每条 `ctl` 都要起一个 root shell + 一个 `thermo` 进程 |
 *
 * 改为**首次访问才挂载**后，首屏只渲染状态页、只发 1 条命令。
 *
 * 未挂载的页面用一个空占位 `<div v-show>` 撑着 —— 位置不影响布局（都是绝对定位铺满）。
 *
 * ### 补充：首屏后**空闲预挂载**
 *
 * 只做按需挂载会带来新问题：用户第一次点某个页签时，那一刻才创建 DOM，
 * 表现为「切换卡一下」。因此首屏渲染完成、主线程空闲后，**提前挂载**
 * 最常用的两页（上限、温控）—— 把挂载开销移出用户可见路径。
 *
 * 设备页刻意**不预挂载**：它的 `device show` 要读大量节点，是四页里最重的，
 * 而用户很少看它。留作按需，代价可接受。
 */
const visited = reactive<Record<TabKey, boolean>>({
  status: true, // 首屏就是它，直接挂载
  control: false,
  logs: false,
  more: false,
});

/** 页签标题（供 MiuixNavigationBar） */
const navItems = TABS.map((t) => ({ label: t.label }));

/** 当前页签 key */
const currentKey = computed(() => TABS[navIndex.value]?.key ?? "status");

/** 顶部栏标题 */
const pageTitle = computed(() => {
  const label = TABS[navIndex.value]?.label ?? "";
  return label ? `澎湃充控 · ${label}` : "澎湃充控";
});

// 切到新页签时标记为已访问（触发挂载）。
// 用 `flush: "pre"`（默认）保证在 DOM 更新前完成，这样组件首次渲染时
// 父容器已经是 `display: block` —— 内部的滑块等组件能测到真实宽度。
watch(currentKey, (k, prev) => {
  if (!visited[k]) {
    visited[k] = true;
    perfMark(`first-mount:${k}`);
  }
  if (prev !== undefined) {
    perfMark(`switch:${k}`);
    // 🔴 记录**切页首帧**的绘制时刻
    //
    // `switch:` 只标出"用户点了哪个页签"，不含耗时信息 ——
    // 因此光看它无法判断切页到底卡不卡（那是真机上唯一能拿到的数据，
    // 曾经只能靠猜）。
    //
    // `requestAnimationFrame` 的回调在**下一次绘制前**执行，
    // 因此 `paint - switch` 就是"点击 → 新页面画出来"的延迟：
    //
    // | 差值 | 含义 |
    // |---|---|
    // | ≈ 16~30 ms | 正常（只是等了 1~2 帧） |
    // | > 100 ms | 有长任务阻塞了主线程 —— 值得查 |
    requestAnimationFrame(() => {
      perfMark(`paint:${k}`);
    });
  }
});

/**
 * 空闲预挂载（**全部页签，逐个进行**）
 *
 * # 为什么要预挂载
 *
 * 只做按需挂载会导致「第一次点某个页签时那一刻才创建 DOM」⇒ 切页卡一下。
 * 首屏渲染完成、主线程空闲后提前挂载，把这份开销移出用户可见路径。
 *
 * # 为什么是"逐个"而不是"一次全挂"
 *
 * 每挂载一页都要下载其 chunk、创建一批 miuix 组件、并**发它自己的
 * `ctl` 命令**（控制页一次并发 6 条，见 `ControlPage.load`）。
 * 一次性全做会把主线程和 root shell 都占满，反而让用户此刻的操作
 * （比如正在拖的温度滑块）变卡。
 *
 * 因此按**使用概率**排队，每次空闲只做一件：
 *
 * | 顺序 | 页签 | 理由 |
 * |---|---|---|
 * | 1 | 控制 | 用户最常调的就是温度与模式 |
 * | 2 | 日志 | 次常用 |
 * | 3 | 更多 | 最少用，且它的 `device show` 最重 |
 *
 * `requestIdleCallback` 会在一帧的空闲时间调用回调；若一直不空闲，
 * `timeout` 保证最终仍会执行 —— 只是会晚一些，那时用户多半已不再操作。
 */
function schedulePreload(): void {
  // `requestIdleCallback` 在旧 WebView 上可能不存在，退到 setTimeout
  const idle: (cb: () => void) => void =
    typeof window.requestIdleCallback === "function"
      ? (cb) =>
          window.requestIdleCallback(cb, {
            // 即使一直不空闲，也在这个时间后强制执行，保证最终挂载
            timeout: 3000,
          })
      : (cb) => window.setTimeout(cb, 400);

  // 逐个排队：后一个在前一个完成后才申请下一次空闲
  const queue: TabKey[] = ["control", "logs", "more"];

  const next = (): void => {
    const k = queue.shift();
    if (k === undefined) {
      return;
    }
    if (visited[k]) {
      next();
      return;
    }
    idle(() => {
      visited[k] = true;
      perfMark(`preload:${k}`);
      // 让这一页的 chunk 下载与挂载先落定，再排下一页
      idle(next);
    });
  };

  next();
}

// ── snackbar 位置跟随底部栏高度 ──
//
// 底部栏高度会随安全区变化（有无手势条不同），
// 因此用 ResizeObserver 实时同步给 miuix 的 snackbar。

const bottomBarRef = ref<HTMLElement | null>(null);
let observer: ResizeObserver | null = null;

function syncSnackbarInset(): void {
  const h = bottomBarRef.value?.offsetHeight ?? 0;
  document.documentElement.style.setProperty(
    "--m-snackbar-inset-bottom",
    `${h}px`,
  );
}

onMounted(() => {
  if (bottomBarRef.value) {
    observer = new ResizeObserver(syncSnackbarInset);
    observer.observe(bottomBarRef.value);
  }
  syncSnackbarInset();

  // 首屏渲染完成后再安排预挂载 —— 此刻主线程即将空闲
  schedulePreload();
});

onBeforeUnmount(() => {
  observer?.disconnect();
  document.documentElement.style.removeProperty("--m-snackbar-inset-bottom");
});

/** 手动刷新 */
function onRefresh(): void {
  void refresh();
}
</script>

<template>
  <div class="app">
    <!-- 顶栏：吸顶，独立于滚动区 -->
    <div class="app__top">
      <MiuixTopAppBar :title="pageTitle" class="app__appbar">
        <template #actions>
          <MiuixIconButton
            aria-label="刷新"
            :class="{ 'is-spinning': sysState.loading }"
            @click="onRefresh"
          >
            <MiuixIcon :icon="Refresh" :size="24" />
          </MiuixIconButton>
        </template>
      </MiuixTopAppBar>
    </div>

    <!--
      页面容器

      两层控制：
        · 外层 `v-show`：控制显示哪个页签（保留各页的滚动位置与表单状态）
        · 内层 `v-if`  + `visited`：**首次访问才挂载**（首屏性能的关键）

      未访问的页签只有一个空的 `.page-scroll` 占位，几乎无成本。
    -->
    <div class="app__body">
      <div
        v-for="tab in TABS"
        v-show="currentKey === tab.key"
        :key="tab.key"
        class="page-scroll"
      >
        <component :is="PAGES[tab.key]" v-if="visited[tab.key]" />
      </div>
    </div>

    <!-- 底栏：吸底，含安全区 -->
    <div ref="bottomBarRef" class="app__bottom">
      <MiuixNavigationBar
        :model-value="navIndex"
        :items="navItems"
        @update:model-value="navIndex = $event"
      >
        <template #icon="{ index }">
          <TabIcon :name="TABS[index]?.icon ?? 'speed'" :size="26" />
        </template>
      </MiuixNavigationBar>
    </div>
  </div>

  <MiuixSnackbarHost />
</template>

<style scoped>
.app {
  display: flex;
  flex-direction: column;
  /* ⚠️ 必须写两行：旧 WebView 不认 dvh，不写兜底会导致高度整条失效 */
  height: 100vh;
  height: 100dvh;
  background: var(--m-color-surface);
}

/* ── 顶栏 ── */

.app__top {
  flex: none;
  /* 避开状态栏（KernelSU 注入的安全区） */
  padding-top: var(--inset-top);
  background: var(--m-color-surface);
  position: relative;
  z-index: 2;
}

.app__appbar {
  /* miuix 的 TopAppBar 自身不带底部边框，这里补一条分隔线更像原生 */
  border-bottom: 1px solid var(--m-color-divider-line);
}

/* ── 内容区 ── */

.app__body {
  flex: 1;
  min-height: 0;
  position: relative;
}

/* 每个页面各自的滚动容器：位置独立保留 */
.page-scroll {
  position: absolute;
  inset: 0;
  overflow-y: auto;
  overflow-x: hidden;
  -webkit-overflow-scrolling: touch;
  /* 隐藏滚动条但保留滚动能力（原生 App 风格） */
  scrollbar-width: none;
  /* 切页时的淡入（替代 Transition 组件，更可靠） */
  animation: page-in 0.18s ease;
}

.page-scroll::-webkit-scrollbar {
  width: 0;
  height: 0;
}

@keyframes page-in {
  from {
    opacity: 0;
  }
  to {
    opacity: 1;
  }
}

/* ── 异步页面的占位 ── */

/* 加载中：只占位，不显示任何东西（太快了，闪一下更糟） */
.page-loading {
  height: 1px;
}

.page-failed {
  padding: 24px 20px;
  color: var(--m-color-on-surface);
  font-size: 14px;
  line-height: 1.7;
}

.page-failed__hint {
  color: var(--m-color-on-surface-secondary);
  font-size: 13px;
}

.page-failed__msg {
  margin-top: 12px;
  font-family: monospace;
  font-size: 12px;
  word-break: break-all;
  color: var(--m-color-error);
}

/* 尊重"减少动效"设置 */
@media (prefers-reduced-motion: reduce) {
  .page-scroll {
    animation: none;
  }
}

/* ── 底栏 ── */

.app__bottom {
  flex: none;
  z-index: 2;
  padding-bottom: var(--inset-bottom);
  background: var(--m-color-surface);
}

/* 刷新按钮转圈 */
.is-spinning :deep(svg) {
  animation: spin 0.9s linear infinite;
}

@keyframes spin {
  to {
    transform: rotate(360deg);
  }
}
</style>
