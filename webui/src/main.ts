/**
 * 入口
 *
 * # 执行顺序很关键
 *
 * | # | 步骤 | 为什么 |
 * |---|---|---|
 * | 1 | `miuix-vue/style.css` | 组件基础样式，**必须最先** —— 没有它所有 miuix 组件都是裸元素 |
 * | 2 | 本项目的 `theme.css` | 覆盖配色令牌，放在后面才能生效 |
 * | 3 | `initTheme()` | 挂主题类名 + 跟随系统深色 |
 * | 4 | 挂载 Vue 应用 | 此时样式已就位，首帧不会闪 |
 *
 * # 全局兜底
 *
 * 界面跑在管理器的 WebView 里，**开发者看不到控制台**。
 * 因此任何未捕获的异常都要变成用户可见的提示 —— 否则一个笔误
 * 就表现为"点了没反应"，无从排查。
 */

// ⚠️ 顺序不可调换：miuix 的基础样式必须在本项目的覆盖之前
import "miuix-vue/style.css";
import "./theme.css";

import { createApp } from "vue";
import { showSnackbar } from "miuix-vue";

import App from "./App.vue";
import { perfMark } from "./perf";
import { initTheme } from "./theme";
import { initSys } from "./stores/sys";

// 这一刻 = main.ts 开始执行 = JS 已下载并开始解析
perfMark("js-exec-start");

// ── 错误收集 ──
//
// ⚠️ 必须定义在**任何使用它的代码之前**（尤其是下面的全局监听器）。
//    `errLog` 是 `const`，有暂时性死区 —— 放在后面会让监听器第一次
//    触发时抛 `Cannot access before initialization`。
//    （本项目已经因为同类问题白屏过一次，见 docs/11 的 D-W8。）

/**
 * 记录一条界面异常（供「记录性能数据」导出）
 *
 * # 为什么要收集而不是直接显示给用户
 *
 * 真机上没有控制台，异常**只能**通过 Snackbar 露出来。但直接把
 * 原始错误摆给用户看是有问题的 —— 实测出现过这条：
 *
 * ```text
 * 界面异常（runtime-5）：Failed to execute 'setPointerCapture' on 'Element'
 * ```
 *
 * 对用户来说这既看不懂、也不知道该做什么，只是吓人。
 *
 * 而模块里已经有一个**诊断数据导出**通道（「更多」页的
 * 「记录性能数据」按钮 → `logs/perf.log`）。把技术细节记在那里，
 * 需要时用 adb 读出来，就能兼顾两端：
 *
 * | 对象 | 看到什么 |
 * |---|---|
 * | 用户 | 一句中文说明 + "可去「更多」页记录诊断数据" |
 * | 排查者 | 完整错误文本（在 perf.log 里） |
 */
const errLog: string[] = [];
(window as { __errs?: string[] }).__errs = errLog;

function recordError(text: string): void {
  // 只保留最近 20 条：导出时会拼成一条 shell 命令，太长会超出参数上限
  errLog.push(text);
  if (errLog.length > 20) {
    errLog.shift();
  }
}

/** 同一秒内不重复弹同一条提示（渲染循环出错时避免刷屏） */
let lastToast = "";
let lastToastAt = 0;

function toastError(short: string): void {
  const now = Date.now();
  if (short === lastToast && now - lastToastAt < 3000) {
    return;
  }
  lastToast = short;
  lastToastAt = now;
  void showSnackbar({ message: short, duration: "long" });
}

// ── 全局错误兜底 ──

window.addEventListener("error", (ev) => {
  recordError(`window.error: ${ev.message}`);
  toastError("界面刚出了点小问题。若显示不正常，可到「更多」页记录诊断数据");
});

window.addEventListener("unhandledrejection", (ev) => {
  const reason = ev.reason as { message?: string } | undefined;
  const msg = reason?.message ?? String(ev.reason);
  recordError(`unhandledrejection: ${msg}`);
  toastError("有一步操作没能完成。可下拉刷新，或在「更多」页记录诊断数据");
});

// ── 启动 ──

initTheme();
perfMark("theme-init");

/**
 * 🔴 **先发起数据请求，再挂载**（真机实测优化）
 *
 * # 为什么要抢这一步
 *
 * `ksu.exec` 起一个 root shell 是本界面**最慢的一步**：首条命令要等
 * 管理器的 root 通道就绪，冷启动时尤其明显。
 *
 * 而它**不需要等 Vue 挂载** —— store 是模块级单例，状态一到，
 * 已经渲染出来的页面会自动响应更新。
 *
 * 因此把两者**并行**：
 *
 * ```text
 * 旧顺序：mount（解析/创建 DOM，CPU 忙）→ 才发命令（再等 I/O）
 *         └──────────── 串行，两段耗时相加 ────────────┘
 *
 * 新顺序：发命令（I/O 等待）┐
 *        mount（CPU 干活） ┘ 并行，总耗时 = max(两者)
 * ```
 *
 * ⚠️ 必须放在 `app.mount()` **之前**，且不能用 `await` —— 一旦 `await`
 *    就变回串行，反而更慢。
 */
const firstFetch = initSys();

const app = createApp(App);

/**
 * 安装失败处理器
 *
 * `createApp().mount()` 内部的错误**不会**冒泡到 window 的 error 事件，
 * 因此必须显式捕获 —— 否则挂载失败只会留下一个空白的 `#app`。
 */
app.config.errorHandler = (err, _instance, info) => {
  const msg = err instanceof Error ? err.message : String(err);
  recordError(`vue(${info}): ${msg}`);
  console.error("[HyperPower] Vue 错误:", err, info);
  toastError("界面刚出了点小问题。若显示不正常，可到「更多」页记录诊断数据");
};

try {
  app.mount("#app");
  // 标记挂载成功：供外部诊断脚本读取（真机上没有控制台，只能靠这个）
  (window as { __vue_mounted?: boolean }).__vue_mounted = true;
  perfMark("mounted");
} catch (e) {
  // 挂载失败是最严重的情况（整页空白）—— 把原因直接写进页面
  const msg = e instanceof Error ? `${e.message}\n${e.stack ?? ""}` : String(e);
  console.error("[HyperPower] 挂载失败:", e);
  const host = document.getElementById("app");
  if (host) {
    host.innerHTML = `
      <div style="padding:20px;font:13px/1.7 monospace;color:#b3261e;background:#fef7ff">
        <b>界面启动失败</b><br><br>
        <div style="white-space:pre-wrap;word-break:break-all">${msg.replace(/</g, "&lt;")}</div>
      </div>`;
  }
}

// 首屏数据请求已在挂载前发起（见上方 `firstFetch`）。
// 这里只挂一个"处理失败"的兜底 —— 未捕获的 rejection 会触发全局提示，
// 但那发生在 `initSys` 内部已被 store 吞掉，所以显式接一下避免 lint 报未使用。
void firstFetch;
