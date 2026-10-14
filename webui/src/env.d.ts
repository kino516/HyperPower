/// <reference types="vite/client" />

/**
 * 环境类型补充
 *
 * `vite/client` 提供 `*.css` / `*.vue` 等资源导入的类型声明 ——
 * 没有它，`import "./theme.css"` 会被 TypeScript 判为找不到模块。
 *
 * ⚠️ Vue 组件的类型由 `vue-tsc` 通过 `@vitejs/plugin-vue` 提供，
 * **不要**手写 `declare module "*.vue"` —— 那会让所有组件退化成 `any`，
 * 失去模板类型检查（`vue-tsc` 的核心价值）。
 *
 * KernelSU 桥接的类型由官方 `kernelsu` 包自带，不在此重复声明。
 */
