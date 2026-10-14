// 分析 Vite/Rollup 产物中各源模块的体积占比
//
// 原理：sourcemap 的 `mappings` 字段用 VLQ 编码记录了「生成代码的每个片段
// 对应哪个源文件」。逐段统计即可得到每个源文件贡献了多少**生成后的字符数**
// —— 这正是我们要的「谁占了多少体积」。
//
// 用法：node analyze.mjs <dist/assets/xxx.js.map>

import { readFileSync } from "node:fs";

const BASE64 =
  "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/** 解码一段 VLQ，返回数字数组 */
function decodeVLQ(str) {
  const out = [];
  let shift = 0;
  let value = 0;
  for (const ch of str) {
    const digit = BASE64.indexOf(ch);
    if (digit < 0) continue;
    const cont = digit & 32;
    value += (digit & 31) << shift;
    if (cont) {
      shift += 5;
    } else {
      const negative = value & 1;
      value >>= 1;
      out.push(negative ? -value : value);
      value = 0;
      shift = 0;
    }
  }
  return out;
}

const mapPath = process.argv[2];
if (!mapPath) {
  console.error("用法: node analyze.mjs <map 文件>");
  process.exit(1);
}

const map = JSON.parse(readFileSync(mapPath, "utf8"));
const sources = map.sources ?? [];
const sizes = new Map();

/**
 * 遍历 mappings，统计每个源文件贡献的生成字符数。
 *
 * mappings 结构：以 `;` 分行、`,` 分段的 5 元组
 *   [生成列, 源文件索引, 源行, 源列, 名称索引]
 * 只有源文件索引是相对上一个的增量。
 */
let srcIdx = 0;
for (const line of (map.mappings ?? "").split(";")) {
  if (!line) continue;
  for (const seg of line.split(",")) {
    if (!seg) continue;
    const fields = decodeVLQ(seg);
    if (fields.length >= 4) {
      srcIdx += fields[1];
      const name = sources[srcIdx] ?? "(未知)";
      // 每段至少代表 1 个字符的输出
      sizes.set(name, (sizes.get(name) ?? 0) + 1);
    }
  }
}

const total = [...sizes.values()].reduce((a, b) => a + b, 0);

/** 归类：把 node_modules 的包聚合成一个条目 */
function bucket(path) {
  if (!path) return "(未知)";
  if (path.includes("node_modules/@vue/")) return "@vue/*";
  if (path.includes("node_modules/vue/")) return "vue";
  if (path.includes("node_modules/miuix-vue/")) return "miuix-vue";
  if (path.includes("node_modules/motion-v/") || path.includes("framer"))
    return "motion-v (framer)";
  if (path.includes("node_modules/")) {
    const m = path.match(/node_modules\/([^/]+)/);
    return m ? `其他: ${m[1]}` : "其他依赖";
  }
  if (path.includes("/src/")) return "本项目源码";
  return path;
}

const buckets = new Map();
for (const [path, n] of sizes) {
  const b = bucket(path);
  buckets.set(b, (buckets.get(b) ?? 0) + n);
}

console.log(`\n总映射片段: ${total.toLocaleString()}`);
console.log("=".repeat(56));

const sorted = [...buckets].sort((a, b) => b[1] - a[1]);
for (const [name, n] of sorted) {
  const pct = ((n / total) * 100).toFixed(1);
  const bar = "█".repeat(Math.max(1, Math.round((n / total) * 34)));
  console.log(`${name.padEnd(24)} ${pct.padStart(5)}%  ${bar}`);
}

console.log("\n" + "=".repeat(56));
console.log("最大的 15 个单文件：");
for (const [path, n] of [...sizes].sort((a, b) => b[1] - a[1]).slice(0, 15)) {
  const pct = ((n / total) * 100).toFixed(1);
  const short = path.replace(/^.*?node_modules\//, "").slice(0, 62);
  console.log(`  ${pct.padStart(5)}%  ${short}`);
}
