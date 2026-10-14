/**
 * 重复样式检测器
 *
 * 用法：node _dupcheck.mjs
 *
 * 比较 `src/theme.css`（共享层）与各页面 `<style scoped>` 里的
 * **顶层类定义**，找出：
 *
 * | 类别 | 含义 | 处理 |
 * |---|---|---|
 * | **完全相同** | 页面里的定义与 theme.css 逐字一致 | 删页面的（纯冗余） |
 * | **数值不同** | 同名类两边都有、但不是同一份声明 | 人工判断保留哪边 |
 *
 * # 为什么要工具化
 *
 * 手工比对很容易漏 —— 这次就是漏了 LogsPage 的 `.grid` / `.grid__cell` /
 * `.empty`（它们散在 1100+ 行的样式区里）。而 scoped 样式因为多一个
 * `[data-v-xxx]`，**优先级高于 theme.css** ⇒ 漏掉的那个会静默生效，
 * 表现为"改了共享层却没生效"。
 */

import { readFileSync, readdirSync } from "node:fs";

/** 归一化声明体，便于比较 */
const norm = (s) =>
  s
    .replace(/\/\*[\s\S]*?\*\//g, " ") // 去注释
    .replace(/\s+/g, " ")
    .replace(/\s*:\s*/g, ":")
    .replace(/\s*;\s*/g, ";")
    .trim()
    .replace(/;$/, "");

/** 从一个 CSS 文本里抽出顶层「选择器 → 声明体」 */
function topLevel(css) {
  const map = new Map();
  // 逐行扫描：选择器行 + 到下一个顶格 } 为止
  const lines = css.split("\n");
  for (let i = 0; i < lines.length; i++) {
    const m = /^\.([a-zA-Z][\w-]*)(\s*[,{])/.exec(lines[i]);
    if (!m) continue;
    const sel = m[1];
    // 收集选择器行（可能跨行，如 `.a ,\n.b {`），找 { 与 }
    let head = lines[i];
    let j = i;
    while (!head.includes("{") && j + 1 < lines.length) {
      head += lines[++j];
    }
    let body = head.slice(head.indexOf("{") + 1);
    let depth = 1 - (head.match(/\{/g)?.length ?? 1) + 1;
    let k = j + 1;
    while (k < lines.length && depth > 0) {
      if (lines[k].includes("{")) depth++;
      if (lines[k].includes("}")) depth--;
      if (depth > 0) body += "\n" + lines[k];
      else body += "\n" + lines[k].replace(/\}.*$/, "");
      if (depth <= 0) break;
      k++;
    }
    // 只关心单选择器（多选择器略过，避免误判）
    if (/^\.([a-zA-Z][\w-]*)$/.test(lines[i].replace(/[,{].*$/, "").trim())) {
      if (!map.has(sel)) map.set(sel, norm(body));
    }
  }
  return map;
}

const theme = topLevel(readFileSync("src/theme.css", "utf8"));

console.log(`共享层（theme.css）定义了 ${theme.size} 个类\n`);

let dupCount = 0;
let diffCount = 0;

for (const f of readdirSync("src/pages").filter((x) => x.endsWith(".vue"))) {
  const path = `src/pages/${f}`;
  const src = readFileSync(path, "utf8");
  const sm = /<style[^>]*>([\s\S]*?)<\/style>/.exec(src);
  if (!sm) continue;
  const page = topLevel(sm[1]);

  const same = [];
  const diff = [];
  for (const [sel, body] of page) {
    if (!theme.has(sel)) continue;
    (theme.get(sel) === body ? same : diff).push(sel);
  }

  if (same.length === 0 && diff.length === 0) continue;

  console.log(`== ${f}`);
  if (same.length) {
    console.log(`   🔴 与共享层**完全相同**（应删除，纯冗余）：`);
    for (const s of same) console.log(`      .${s}`);
    dupCount += same.length;
  }
  if (diff.length) {
    console.log(`   ⚠️  同名但**内容不同**（scoped 优先级更高，会覆盖共享层）：`);
    for (const s of diff) {
      console.log(`      .${s}`);
      console.log(`         theme : ${theme.get(s).slice(0, 90)}`);
      console.log(`         page  : ${page.get(s).slice(0, 90)}`);
    }
    diffCount += diff.length;
  }
  console.log("");
}

console.log(`小结：纯重复 ${dupCount} 处，冲突 ${diffCount} 处`);

/**
 * 退出码：有纯重复就失败
 *
 * 本脚本挂在 `npm run build` 里当门禁（见 `package.json`），
 * 因此必须用**非零退出码**表示失败，否则门禁形同虚设。
 *
 * 「冲突」（同名但内容不同）只警告不失败 —— 它可能是有意为之的
 * scoped 覆盖（某页确实要微调）。但**纯重复**一定是遗漏：那份声明
 * 在共享层里已经有一模一样的一份，留着它只会让优先级更难推理。
 */
if (dupCount > 0) {
  console.log(
    `\n✗ 有 ${dupCount} 处定义与共享层完全重复。\n` +
      `  请删掉页面里那一份 —— scoped 样式优先级更高，会覆盖共享层，\n` +
      `  造成"改了 theme.css 却没生效"的假象。`,
  );
  process.exit(1);
}
console.log("✓ 无重复定义");
