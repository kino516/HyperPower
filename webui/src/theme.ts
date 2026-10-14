/**
 * 主题：Monet 动态取色 + 深色模式
 *
 * # 机制
 *
 * KernelSU 在 WebView 里注册了一个**虚拟域名** `mui.kernelsu.org`
 * （由 `WebViewAssetLoader` 本地提供，**不走网络**），其中
 * `/internal/colors.css` 声明了一组取自系统壁纸的 CSS 变量：
 *
 * ```css
 * :root { --primary: #6750a4; --surface: #fef7ff; --onSurface: … }
 * ```
 *
 * 页面引入它之后，只要把 miuix-vue 的配色令牌指向这些变量，
 * 整个界面就会**与系统主题完全一致**（用户换壁纸/改主题，界面跟着变）。
 *
 * 本模块的 `theme.css` 做的正是这个映射；此处的 TS 只负责：
 *
 * | 职责 | 说明 |
 * |---|---|
 * | 引入颜色定义 | `import "…/colors.css"` —— 必须最先执行 |
 * | 在 `<html>` 上挂类 | miuix 的令牌只在特定类名下生效 |
 * | 跟随系统深色 | `setThemeMode('system')` |
 *
 * # 为什么不用 try/catch 兜住 import
 *
 * CSS 的 `@import` 失败不会抛异常（浏览器只是忽略该规则），
 * 因此映射里的每一行都写了**兜底色值** —— 即使虚拟域名不可用，
 * 界面也只是退回 MD3 基线配色，不会掉样式。
 */

import { setThemeMode } from "miuix-vue";

/**
 * 初始化主题
 *
 * ⚠️ `colors.css` 必须在使用任何 miuix 组件**之前**加载好，
 * 否则首帧会闪一下基线配色。因此本函数在 `main.ts` 里最先调用。
 */
export function initTheme(): void {
  // 挂上 miuix 的主题类名：miuix-vue 的配色令牌依赖它
  document.documentElement.classList.add("miuix-monet");

  // 跟随系统（'system' | 'light' | 'dark'）
  setThemeMode("system");
}
