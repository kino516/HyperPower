import { defineConfig, type Plugin } from "vite";
import vue from "@vitejs/plugin-vue";

/**
 * 去掉 `crossorigin` 属性
 *
 * # 为什么必须这么做（真机实测）
 *
 * Vite 会给注入的 `<script type="module">` / `<link>` 自动加上
 * `crossorigin`，这是为了配合 `modulepreload` 的跨域语义。
 * 但 **KernelSU 的 WebViewAssetLoader 不给响应带 CORS 头**，
 * 于是浏览器按 CORS 模式请求被拒 —— 脚本**整块不执行**，
 * 表现就是「顶部栏和底部导航都正常，中间内容一片空白」。
 *
 * 实测结论（K90 Pro Max / Android 17 / KernelSU v3.3.0）：
 *
 * | 写法 | 结果 |
 * |---|---|
 * | 带 `crossorigin` | ❌ 内容区空白（`index.js` 根本未执行） |
 * | 去掉 `crossorigin` | ✅ 界面正常渲染 |
 *
 * 直接改 `dist/index.html` 治标不治本（下次构建又回来），
 * 因此用插件在**构建阶段**统一剥掉。
 *
 * ⚠️ 只剥 HTML 上的属性，不动 JS 里的 `import` —— 那些是相对路径，
 * 同源加载不受影响。
 */
function stripCrossorigin(): Plugin {
  return {
    name: "thermo:strip-crossorigin",
    transformIndexHtml: {
      order: "post",
      handler(html: string) {
        return html.replace(/\s+crossorigin(?=[\s>])/g, "");
      },
    },
  };
}

/**
 * 去掉构建产物里 HTML 注释
 *
 * Vite **默认保留** `index.html` 里的注释。本项目的入口文件带有大段
 * 说明性注释（解释虚拟域名、`crossorigin`、骨架等），它们对用户毫无意义，
 * 却让产物从 1.05 KB 涨到 3.01 KB。
 *
 * 注意插件顺序：本插件排在 `stripCrossorigin` **之后**，先剥属性再删注释 ——
 * 这样 `stripCrossorigin` 仍然处理完整的 HTML，不受注释影响。
 */
function stripHtmlComments(): Plugin {
  return {
    name: "thermo:strip-html-comments",
    transformIndexHtml: {
      order: "post",
      handler(html: string) {
        // 保留条件注释（`<!--[if ...]>`）以防将来需要，其余全删
        return html.replace(/<!--(?!\[if)[\s\S]*?-->/g, "");
      },
    },
  };
}

/**
 * 🔴 构建期门禁：产物里不得出现**未编译的 `template` 字符串**
 *
 * # 这一起事故（2026-10-14，真机白屏）
 *
 * 本项目用 Vue 的 **runtime-only** 构建（不含模板编译器）。在 `.vue` 文件
 * 之外写 `{ template: "<div>…</div>" }` 时无法编译 —— 而生产构建
 * **剥掉了 Vue 的警告**，所以：
 *
 * ```text
 * 渲染该组件 → 编译失败 → 抛错 → 父组件渲染失败 → 整树塌成 <!---->
 * → 全白 + 控制台零线索
 * ```
 *
 * 最坑的是**类型检查与构建全都通过**：`vue-tsc` 对 `template` 字符串
 * 完全无感。只有真机打开才暴露。
 *
 * # 判据为什么可靠
 *
 * 正常的 `.vue` 单文件组件在构建期就被编译成 render 函数了，
 * 产物里**不会**出现 `template:` 这个对象属性。因此产物里一旦出现
 * `template:"` / `template:` 后面跟字符串，就一定是"运行期才想编译"。
 *
 * 若将来确实需要运行期模板（要引入带编译器的 Vue），
 * 请显式改这个门禁并说明理由 —— 不要让它在无声中失效。
 */
function forbidRuntimeTemplate(): Plugin {
  return {
    name: "thermo:forbid-runtime-template",
    apply: "build",
    generateBundle(_options, bundle) {
      for (const [file, chunk] of Object.entries(bundle)) {
        if (chunk.type !== "chunk" || !file.endsWith(".js")) {
          continue;
        }
        // `template:` 紧跟引号或反引号 ⇒ 运行期模板
        const hit = /template:\s*["'`]/.exec(chunk.code);
        if (hit) {
          const at = Math.max(0, hit.index - 80);
          this.error(
            `${file} 里发现运行期 template 字符串（Vue 是 runtime-only，无法编译）：\n` +
              `  …${chunk.code.slice(at, hit.index + 60).replace(/\n/g, " ")}…\n` +
              `改用 h() 手写 render 函数。详见 docs/11 的 D-W8。`,
          );
        }
      }
    },
  };
}

// ⚠️ 契约要求（docs/01-项目方案.md §4.4）：
//    · 入口必须是构建产物根目录的 index.html（管理器据此加载）
//    · 产物从**模块目录**加载 ⇒ 资源路径必须是**相对**的。
//      绝对路径（"/assets/..."）在 KernelSU 的虚拟域名下会 404。
//      `base: "./"` 正是为此。
export default defineConfig({
  plugins: [
    vue(),
    stripCrossorigin(),
    stripHtmlComments(),
    forbidRuntimeTemplate(),
  ],
  base: "./",
  build: {
    outDir: "dist",
    emptyOutDir: true,
    // 单文件体积目标：WebUI 随模块分发，越小越好
    assetsInlineLimit: 8192,
    // Vue 3.5 与 miuix-vue 都不需要 ES2022 以上的语法；
    // KernelSU 要求 Android 8+，其 WebView 完全支持 ES2020。
    target: "es2020",
    rollupOptions: {
      output: {
        // 把框架拆成独立 chunk：只改业务代码时它的缓存不会失效
        manualChunks: {
          vendor: ["vue", "miuix-vue", "motion-v"],
        },
      },
    },
  },
});
