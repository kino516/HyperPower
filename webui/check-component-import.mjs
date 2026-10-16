/**
 * 检查 `.vue` 模板里用到的**自定义组件是否都导入了**
 *
 * # 这一起事故（2026-10-17 真机发现）
 *
 * 日志页新加的「调试模式」开关写成了 `<MiuixSwitch>`，但 import 列表里
 * **没有它** —— 于是构建产物里渲染成一个**未识别的自定义元素**：
 *
 * ```html
 * <miuixswitch model-value="false" disabled="false"></miuixswitch>
 * ```
 *
 * 界面上就是"这个开关根本不存在"，而且**什么都没坏**：
 *
 * | 检查手段 | 能不能发现 |
 * |---|---|
 * | `vue-tsc` | ❌ 类名/标签不是类型信息 |
 * | `vite build` | ❌ 构建完全成功 |
 * | 单元测试 | ❌ 测的是后端 |
 * | **在浏览器里跑一遍** | ✅ 唯一能发现的办法 |
 *
 * 同类问题这不是第一次：控制页的 `.row` 类被日志页复用却没有样式
 * （见 `theme.css` 里 `.row` 的注释）。两个的共同点是
 * **「引用了不存在的东西，而所有静态检查都看不见」**。
 *
 * ⇒ 用这个脚本把"组件必须导入"变成一条**构建期**就能拦住的规则。
 *
 * # 检查范围
 *
 * 只认**大驼峰开头**的标签（`<MiuixSwitch>`、`<InfoRow>`）——
 * 项目里全部这么写。kebab-case（`<miuix-switch>`）不做映射推断，
 * 以免把浏览器原生的自定义元素误报。
 *
 * # 用法
 *
 * ```bash
 * node check-component-import.mjs
 * ```
 *
 * 已接进 `npm run build`。
 */

import { readFileSync } from "node:fs";
import { globSync } from "node:fs";
import { resolve } from "node:path";

/** Vue 内置组件（无需导入） */
const BUILTIN = new Set([
  "Transition",
  "TransitionGroup",
  "KeepAlive",
  "Teleport",
  "Suspense",
  "component",
  "slot",
  "template",
]);

/** 递归收集 src 下的全部 .vue 文件 */
function vueFiles() {
  return globSync("src/**/*.vue", { cwd: process.cwd() }).map((p) =>
    resolve(process.cwd(), p),
  );
}

/** 取出 `<template>…</template>` 的内容（顶层那一对） */
function templateOf(text) {
  const start = text.indexOf("<template>");
  if (start < 0) {
    return "";
  }
  // 从后往前找最后的 `</template>` —— 嵌套的 slot template 也在其中
  const end = text.lastIndexOf("</template>");
  return end > start ? text.slice(start, end) : "";
}

/** 去掉 HTML 注释（注释里的示例标签不算使用） */
function stripComments(html) {
  return html.replace(/<!--[\s\S]*?-->/g, "");
}

/**
 * 模板里出现的**大驼峰标签名**（去重）
 *
 * `\b` 保证不会把 `<MyComp>` 里的 `Comp` 也单独匹配出来。
 */
function usedComponents(tpl) {
  const out = new Set();
  for (const m of stripComments(tpl).matchAll(/<([A-Z][A-Za-z0-9]*)[\s/>]/g)) {
    if (!BUILTIN.has(m[1])) {
      out.add(m[1]);
    }
  }
  return out;
}

/**
 * 该文件是否"拿得到"这个名字
 *
 * 三种合法来源：
 *
 * | 来源 | 例子 |
 * |---|---|
 * | 具名 import | `import { MiuixSwitch } from "miuix-vue";` |
 * | 默认 / 改名 import | `import InfoRow from "./InfoRow.vue";` |
 * | 本地定义 | `defineComponent` 或 `const X = …`（少见，但合法） |
 */
function hasBinding(text, name) {
  const script = text.slice(text.indexOf("<script"));
  // 具名 import：`… { A, B, C } … from`
  const named = script.match(/import\s*\{([\s\S]*?)\}\s*from/g) ?? [];
  for (const block of named) {
    const inner = block.slice(block.indexOf("{") + 1, block.lastIndexOf("}"));
    for (const part of inner.split(",")) {
      // 处理 `A as B` —— 绑定的名字是 B
      const alias = part.split(/\s+as\s+/);
      const bound = (alias[1] ?? alias[0]).trim();
      if (bound === name) {
        return true;
      }
    }
  }
  // 默认 import：`import X from` / `import X, { … }`
  if (new RegExp(`import\\s+${name}\\s*(,|\\s+from)`).test(script)) {
    return true;
  }
  // 本地定义
  if (
    new RegExp(`(const|let|function)\\s+${name}\\b`).test(script) ||
    new RegExp(`\\b${name}\\s*=`).test(script)
  ) {
    return true;
  }
  return false;
}

const problems = [];
for (const file of vueFiles()) {
  const text = readFileSync(file, "utf8");
  const tpl = templateOf(text);
  if (!tpl) {
    continue;
  }
  for (const name of usedComponents(tpl)) {
    if (!hasBinding(text, name)) {
      problems.push({ file, name });
    }
  }
}

if (problems.length > 0) {
  console.error("✗ 模板里用了未导入的组件（会渲染成未识别的自定义元素）：\n");
  for (const p of problems) {
    const rel = p.file.replace(`${process.cwd()}\\`, "").replaceAll("\\", "/");
    console.error(`  ${rel}`);
    console.error(`    <${p.name}> —— 没有 import，也没有本地定义`);
  }
  console.error(
    "\n这类问题 vue-tsc 与 vite build 都**发现不了**，" +
      "只会在真机上表现为“这个控件不见了”。",
  );
  process.exit(1);
}

console.log("✓ 组件导入检查通过");
