/**
 * 性能打点
 *
 * # 为什么需要这个
 *
 * 真机上**没有开发者控制台**，无法用 `console.time` / Performance 面板
 * 观察启动耗时。而用户能感知的"白屏很久"必须被量化才能定位 ——
 * 是靠猜是找不出瓶颈的。
 *
 * 做法：把关键时刻记录到 `window.__perf`，配合一个探针脚本
 * 把 Performance API 的数据 + 这些打点一起写回文件，再用 adb 读回分析。
 * 详见 docs/11-WebUI实现与真机踩坑.md。
 *
 * # 数值含义
 *
 * `performance.now()` 是**相对页面导航开始**的毫秒数，因此打点值
 * 直接就是"从打开页面到此刻经过了多少毫秒"，无需换算。
 *
 * # 生产环境保留
 *
 * 每次打点只是往数组里 push 一项，开销可忽略（微秒级）。
 * 保留它让**用户反馈"慢"时有数据可查**，比事后加打点再复现划算。
 */

/** 打点记录（供探针读取） */
interface PerfWindow {
  __perf?: [string, number][];
}

/**
 * 打点上限
 *
 * # 为什么需要上限
 *
 * `dumpPerfLog` 把全部打点拼成**一条 shell 命令**。若打点无限增长：
 *
 * | 风险 | 说明 |
 * |---|---|
 * | 命令行超长 | Linux 单参数上限 128 KB（`MAX_ARG_STRLEN`），超了直接报错，**导不出任何数据** |
 * | 无谓内存 | 每个打点约 60 字节，虽小但没有保留价值 |
 *
 * 600 条 ≈ 36 KB，离上限有足够余量。
 */
const MAX_MARKS = 600;

/**
 * 永久保留的**前**多少条
 *
 * 首屏打点（`js-exec-start` / `mounted` / `fetch-done` / `preload:*`）
 * 永远在最前面，而它们正是诊断"打开很慢"最需要的。
 *
 * 因此超限时丢弃的是**中段**（首屏之后、最近若干条之前的那批），
 * 而不是简单地丢最老的 —— 否则用户切页几百次后，首屏数据就被挤没了。
 */
const KEEP_HEAD = 40;

/**
 * 记录一个时刻
 *
 * @param name 打点名称（用 `英文:页签` 形式，便于机械解析）
 */
export function perfMark(name: string): void {
  const w = window as PerfWindow;
  const list = (w.__perf ??= []);
  list.push([name, Math.round(performance.now())]);

  if (list.length > MAX_MARKS) {
    // 删除索引 KEEP_HEAD 处那一条 —— 即"首屏之后最老的一条"。
    // 效果：前 KEEP_HEAD 条固定不动，之后的滚动更新。
    list.splice(KEEP_HEAD, 1);
  }
}
