/**
 * 运行状态（组合式 store）
 *
 * # 为什么用模块级 `reactive` 而不是 Pinia
 *
 * 本界面只有一份状态、一个消费者（App），引入 Pinia 等于多一个依赖
 * 换零收益。模块级单例 + `reactive` 已足够，且**更好读**。
 *
 * # 轮询策略
 *
 * | 何时 | 行为 |
 * |---|---|
 * | 页面可见 | 每 3 秒拉一次 `status` + `events` |
 * | 页面隐藏 | **停止轮询**（省电；原生 App 也这么做） |
 * | 切页/手动刷新 | 立即拉一次，不等轮询周期 |
 *
 * 温度变化是慢过程，3 秒足够跟手又不费电。
 */

import { computed, reactive, readonly } from "vue";

import * as api from "../api";
import { perfMark } from "../perf";
import type { StatusSnapshot } from "../types";

/**
 * 轮询间隔（毫秒）
 *
 * 温度变化是慢过程，3 秒足够跟手又不费电。
 *
 * ⚠️ 这个间隔同时也是**命令频率的上限**：每拍一条 `ctl status`。
 *    因此往轮询里加任何新命令都要谨慎 —— 在手机上每条命令都要起一个
 *    root shell，不是零成本。
 */
const POLL_MS = 3000;

interface State {
  /** 实时状态；未加载时为 null */
  status: StatusSnapshot | null;
  /** 后端是否可达 */
  daemonOk: boolean;
  /** 首帧是否已渲染（用于消除启动闪烁） */
  ready: boolean;
  /** **用户主动发起的**刷新是否在进行（给刷新按钮转圈） */
  loading: boolean;
  /** 最近一次错误（供界面提示） */
  lastError: string | null;
}

const state = reactive<State>({
  status: null,
  daemonOk: false,
  ready: false,
  loading: false,
  lastError: null,
});

/** 定时器句柄（模块级单例，只允许存在一个） */
let timer: number | undefined;

/** 只读状态（组件不应直接改） */
export const sysState = readonly(state);

/** 便捷派生：当前电池温度 */
export const currentTemp = computed<number | null>(
  () => state.status?.battery.temp_c ?? null,
);

/** 便捷派生：是否正在充电 */
export const charging = computed<boolean>(() => {
  const s = state.status?.battery.status;
  return s === "Charging" || s === "Full";
});

// ⚠️ 这里曾有 `refreshEvents()` + `state.events`，**已删除**（2026-10-14）
//
// 它每 15 秒发一条 `ctl logs events`，把结果存进 `state.events` ——
// 而**全项目没有任何组件读那个字段**（`LogsPage` 用自己页面内的
// `events` ref，在 `load()` 里独立拉取）。
//
// 也就是说：一条纯耗电的死命令，外加每次都给 `state.events` 赋新数组
// 触发的无谓响应式更新。删掉后每 15 秒少一个 root shell。

/**
 * 拉取一次状态
 *
 * # `silent`：轮询**不要**动 `loading`
 *
 * 🔴 这是一个真实的性能/观感缺陷（2026-10-14 修）
 *
 * `loading` 被 `App.vue` 的刷新按钮用来转圈。初版**每次拉取**都设置它，
 * 而轮询每 3 秒跑一次 —— 于是：
 *
 * | 现象 | 说明 |
 * |---|---|
 * | **刷新按钮每 3 秒转一圈** | 用户以为界面在"一直忙" |
 * | **App 每 3 秒重渲染 2 次** | `false→true→false`，两次都触发 |
 * | 连带 patch 所有已挂载页面 | App 重渲染会把 4 个页签全部 patch 一遍 |
 *
 * `loading` 的正确语义是「**用户主动发起的刷新正在进行**」，
 * 与后台轮询无关。因此轮询一律传 `silent: true`。
 *
 * @param silent 不触碰 `loading`（后台轮询用）
 */
export async function refresh(silent = false): Promise<void> {
  if (!silent) {
    state.loading = true;
  }
  try {
    // 状态是**首屏关键路径**：它决定界面能不能显示内容
    await api.fetchStatus().then(
      (v) => {
        state.status = v;
        state.daemonOk = true;
        state.lastError = null;
      },
      (e: unknown) => {
        state.daemonOk = false;
        state.lastError = e instanceof Error ? e.message : "无法读取运行状态";
      },
    );
  } finally {
    if (!silent) {
      state.loading = false;
    }
    state.ready = true;
  }
}

/** 轮询计数器 */
let pollCount = 0;

/** 开始轮询 */
export function startPolling(): void {
  stopPolling();
  timer = window.setInterval(() => {
    pollCount += 1;
    // ⚠️ **必须静默**：否则每 3 秒都会让刷新按钮转圈、并触发 App 重渲染
    //    （见 `refresh()` 的文档）。
    //
    // 轮询失败不弹提示（避免每 3 秒刷屏），状态徽标会自行提示。
    void refresh(true);
  }, POLL_MS);
}

/** 停止轮询 */
export function stopPolling(): void {
  if (timer !== undefined) {
    window.clearInterval(timer);
    timer = undefined;
  }
}

/**
 * 初始化：拉首帧 + 启动轮询 + 监听可见性
 *
 * # 首屏命令数
 *
 * **只发 1 条**（`status`）。
 *
 * 对比初版：四个页面全部挂载，启动瞬间并发 **6 条** `ctl`
 * （status + events + limit + thermal + curve + device），真机表现为
 * 「白屏时间非常长」。现在延迟挂载，首屏降到 1 条。
 *
 * ⚠️ 页面隐藏时必须停轮询：WebView 在后台仍会跑定时器，
 * 3 秒一次的命令执行会白白耗电。
 */
export async function initSys(): Promise<void> {
  perfMark("fetch-start");
  await refresh(true);
  perfMark("fetch-done");

  startPolling();

  document.addEventListener("visibilitychange", () => {
    if (document.hidden) {
      stopPolling();
    } else {
      void refresh(true);
      startPolling();
    }
  });
}
