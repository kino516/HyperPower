/**
 * 检查 `index.html` 的注释里是否有 HTML 规范禁止的连续连字符
 *
 * # 这一起事故（详见 docs/12-WebUI入口文件说明.md §4）
 *
 * `index.html` 的 HTML 注释里写了 markdown 表格：
 *
 * ```html
 * <!--
 *   | # | 要点 | 后果 |
 *   |---|---|---|          ← 元凶
 * -->
 * ```
 *
 * **HTML 规范禁止注释内出现连续两个连字符**（开闭标记已占用这些序列）。
 * 解析器因此错乱，把注释**之后**的标签一并吞掉 —— 而 Vite 注入的
 * `<script type="module">` 恰好在那之后。
 *
 * 后果：产物 `index.html` 里 `<script>` 数 = 0，界面永远停在骨架转圈。
 * 而且**构建成功、零警告、类型检查与测试全过**。
 *
 * # 用法
 *
 * ```bash
 * cd webui
 * node check-html-comment.mjs
 * ```
 *
 * 退出码 0 = 干净，1 = 有问题。
 * `cargo xtask webui` 里也做了同样的检查（构建前拦截）。
 */

import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

/** 本脚本所在目录（= webui/） */
const HERE = dirname(fileURLToPath(import.meta.url));

/** 连续两个连字符（HTML 注释里禁止出现） */
const DASH = String.fromCharCode(45, 45);
/** 注释结束标记 */
const END = DASH + ">";

/** 统计字符串里连续连字符的出现次数（`---` 算两次） */
function countDashes(s) {
  let n = 0;
  let i = 0;
  for (;;) {
    const at = s.indexOf(DASH, i);
    if (at < 0) {
      break;
    }
    n += 1;
    i = at + 1;
  }
  return n;
}

/**
 * 扫描所有 HTML 注释
 *
 * @param {string} html
 * @returns {{ index: number, inner: string, closed: boolean }[]}
 */
function scanComments(html) {
  const out = [];
  let pos = 0;

  for (;;) {
    const start = html.indexOf("<!--", pos);
    if (start < 0) {
      break;
    }
    const end = html.indexOf(END, start + 4);
    if (end < 0) {
      out.push({ index: start, inner: html.slice(start + 4), closed: false });
      break;
    }
    out.push({
      index: start,
      inner: html.slice(start + 4, end),
      closed: true,
    });
    pos = end + END.length;
  }

  return out;
}

// ── 主流程 ──

const target = join(HERE, "index.html");
let html;
try {
  html = readFileSync(target, "utf8");
} catch (e) {
  console.error(`✗ 读不到 ${target}：${e.message}`);
  process.exit(1);
}

const comments = scanComments(html);
let failed = false;

console.log(`检查 ${target}`);
console.log(`发现 ${comments.length} 个 HTML 注释\n`);

for (const [i, c] of comments.entries()) {
  const label = `注释 #${i + 1}（偏移 ${c.index}）`;

  if (!c.closed) {
    console.error(`✗ ${label}：未闭合（缺少结束标记）`);
    failed = true;
    continue;
  }

  const bad = countDashes(c.inner);
  if (bad > 0) {
    console.error(`${"✗"} ${label}：含 ${bad} 处连续连字符（HTML 规范禁止）`);

    const at = c.inner.indexOf(DASH);
    const from = Math.max(0, at - 30);
    const snippet = c.inner.slice(from, from + 70).replace(/\r?\n/g, " / ");
    console.error(`    位置 ${at}：${snippet}`);
    console.error("    → 这会让解析器吞掉后续标签（如注入的 script），界面白屏");
    console.error("    → 把说明移到 docs/12-WebUI入口文件说明.md，源文件里不留注释");
    failed = true;
  } else {
    console.log(`✓ ${label}：干净`);
  }
}

if (comments.length === 0) {
  console.log("（无注释 —— 这是推荐状态）");
}

console.log();
if (failed) {
  console.error("✗ 检查未通过");
  process.exit(1);
}
console.log("✓ 检查通过");
