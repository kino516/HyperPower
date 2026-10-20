/**
 * 与后端通信
 *
 * # 唯一的通信方式
 *
 * 全部操作都经 `bin/ctl` —— 它是**契约冻结的稳定入口**
 * （见 docs/05-契约-接口输出规范.md）。界面不直接读写 sysfs：
 *
 * | 为什么 | 说明 |
 * |---|---|
 * | 单写者纪律 | worker 是唯一的常规 sysfs 写者；界面只能"请求" |
 * | 契约稳定 | 命令名与输出格式冻结，后端重写时前端零改动 |
 * | 权限 | socket 0600 + SO_PEERCRED UID 校验 |
 *
 * # 为什么用类型化函数而不是通用 `call(字符串)`
 *
 * `ksu.exec` 以 **root** 执行字符串，通用接口等于把"任意命令执行"
 * 暴露给界面代码 —— 任何拼接失误都会变成提权通道。
 * 类型化后：参数在函数内校验（如 SOC 必须先过 60~100），返回值有具体类型。
 *
 * # 两种运行环境
 *
 * | 环境 | `window.ksu` | 行为 |
 * |---|---|---|
 * | KernelSU / APatch 管理器 WebView | 存在 | 真实调用后端 |
 * | 浏览器（开发调试） | 不存在 | 返回内置示例数据 |
 */

import { exec } from "kernelsu";

import type {
  AddonStatus,
  ControlSection,
  CurvePoint,
  DeviceProfileView,
  HistoryListResult,
  HistorySession,
  ModeList,
  PowerMeasurement,
  ProbeResult,
  RuntimeEvent,
  StatusSnapshot,
  ThermalSection,
} from "./types";

/**
 * 模块目录（与 Rust 侧 `paths.rs` 的 `MODULE_DIR` 一致）
 *
 * 🔴 **必须与 `module/src/module.prop` 的 `id` 一致** ——
 * KernelSU 用 `id` 作为模块目录名。改 id 时这里最容易漏改，
 * 而漏改的后果是**整个 WebUI 失效**（调不到 `ctl`）。
 *
 * 前缀 `a_` 是为了在 KernelSU 模块列表里**排序靠前**（列表按 id 字母序升序）。
 * 注意**数据目录仍是 `thermo_charge`**，与模块 id 无关 ——
 * 因此改 id 不影响用户的配置 / 档案 / 历史。
 *
 * 防漏改：`native/crates/thermo-cli/src/commands.rs` 有一条测试
 * （`webui路径与module_prop一致`）会读本文件并核对，改漏了会测试失败。
 */
const MODULE_DIR = "/data/adb/modules/a_hyperpower";

/** 稳定入口（shim → `thermo ctl`） */
const CTL = `${MODULE_DIR}/bin/ctl`;

/** 主二进制（本地命令如 `device probe` 直接调它） */
const BINARY = `${MODULE_DIR}/bin/thermo`;

/**
 * SOC 兜底区间（%）
 *
 * ⚠️ **只在读不到检测档案时使用**。实际区间应以本机检测值为准
 * （见 [`fetchSocRange`]）—— 不同机型的驱动支持范围不同。
 */
export const SOC_MIN = 60;
export const SOC_MAX = 100;

// ============================================================
//  设备档案缓存
// ============================================================

/**
 * 缓存中的设备档案**请求**（不是结果）
 *
 * # 🔴 为什么缓存 Promise 而不是值（修的是一个真实的缓存击穿）
 *
 * 初版缓存的是**结果**，于是并发调用会**各发一条命令**：
 *
 * ```ts
 * let deviceCache: DeviceProfileView | null = null;
 *
 * async function fetchDeviceCached() {
 *   if (deviceCache === null) {          // ← 三个调用者同时看到 null
 *     deviceCache = await run(["device", "show"]);   // ← 于是发三条！
 *   }
 *   return deviceCache;
 * }
 * ```
 *
 * `await` 之前缓存字段一直是 `null`，因此**在途请求不算缓存命中**。
 *
 * 真机后果：`ControlPage.load()` 用 `Promise.all` 并发取 6 项，其中
 * `fetchHardwareMaxTemp()` 与 `fetchSocRange()` 都会走到这里 ——
 * **同一份档案被读 3 次**。而每次 `run()` 都要起一个 root shell
 * 加一个 `thermo` 进程，在手机上不是零成本，页面切换就"卡一下"。
 *
 * 缓存 Promise 后，三个调用者共享**同一个在途请求**，只发一条命令。
 *
 * ⚠️ 失败时要清空缓存 —— 否则失败会被永久缓存，重试也拿不到数据。
 */
let deviceCache: Promise<DeviceProfileView> | null = null;

/** 让设备档案缓存失效（检测后调用） */
export function invalidateDeviceCache(): void {
  deviceCache = null;
}

/** 读取设备档案（带缓存，**并发调用共享同一请求**） */
export function fetchDeviceCached(): Promise<DeviceProfileView> {
  deviceCache ??= run(["device", "show"]).catch((e: unknown) => {
    // 失败不能留在缓存里，否则重试永远拿不到
    deviceCache = null;
    throw e;
  }) as Promise<DeviceProfileView>;
  return deviceCache;
}

/**
 * 读本机支持的电量上限区间（%）
 *
 * 读不到检测值时退回兜底区间 —— 那种情况下功能本身也用不上
 * （机型不支持该节点）。
 */
export async function fetchSocRange(): Promise<[number, number]> {
  try {
    const d = await fetchDeviceCached();
    const lo = d.capabilities.soc_limit_min;
    const hi = d.capabilities.soc_limit_max;
    if (typeof lo === "number" && typeof hi === "number" && lo < hi) {
      return [lo, hi];
    }
  } catch {
    // 读不到 → 兜底（不抛错，界面照常可用）
  }
  return [SOC_MIN, SOC_MAX];
}

/**
 * 读本机的硬件保护温度（℃）
 *
 * 温度滑块的上限用它 —— 初版写死 45℃，而本机实测是 48℃，
 * 导致**用户无法把收紧温度设到 46/47℃**（白白浪费余量）。
 *
 * 读不到时退回 45（保守）。
 */
export async function fetchHardwareMaxTemp(): Promise<number> {
  try {
    const d = await fetchDeviceCached();
    const t = d.capabilities.hardware_max_c;
    if (Number.isFinite(t) && t > 20 && t <= 60) {
      return t;
    }
  } catch {
    // 读不到 → 兜底
  }
  return 45;
}

/**
 * 把启动打点与界面异常写入 `logs/perf.log`（**诊断用**）
 *
 * # ⚠️ 这个函数现在由 [`exportDiagnosticLog`] 自动调用
 *
 * 原先它背后有一个「记录性能数据」按钮（「更多」页）。用户要求删除那个按钮
 * （2026-10-14）后，调用点就只剩导出了 —— **不能连这个函数一起删**，
 * 因为它的第二项职责无可替代：
 *
 * | 写入内容 | 为什么必须保留出口 |
 * |---|---|
 * | `window.__perf` 打点 | 排查"打开慢、切页卡" |
 * | **`window.__errs` 界面异常详情** | `main.ts` 对用户**只显示一句中文**（原始英文错误没有意义），技术细节只存在**内存数组**里 —— 不落盘就永远丢了 |
 *
 * ⇒ 导出报告里本来就有 `perf.log` 段，所以「导出诊断日志」顺带调用一次
 *    就同时补上了这两项，用户不必再知道"卡了要点哪个按钮"。
 *
 * # 为什么走裸 shell 而不是 `ctl`
 *
 * 打点在 **WebView 内存里**（`window.__perf`），worker 看不到它们。
 * 为此新增一条 `ctl` 命令得不偿失；直接用一个 shell 重定向写文件最省。
 *
 * # 数据格式
 *
 * 一行一次导出：
 *
 * ```text
 * === 2026-10-14T02:50:27 === js-exec-start=67 mounted=112 paint:control=845 …
 * ```
 *
 * 数值是 `performance.now()`，即**从页面导航开始的毫秒数** ——
 * 因此第一个数就约等于"打开界面用了多久"。
 *
 * | 打点 | 含义 |
 * |---|---|
 * | `js-exec-start` | 脚本开始执行 |
 * | `mounted` | Vue 挂载完成（**白屏结束**） |
 * | `fetch-done` | 首条数据到位（**有内容可看**） |
 * | `preload:X` | 空闲预挂载 X 页 |
 * | `switch:X` | 用户点了 X 页签（**时刻**，不是耗时） |
 * | `paint:X` | X 页首帧绘制（`paint − switch` = 切页延迟） |
 * | `loaded:X` | X 页的数据拉取完成 |
 *
 * # ⚠️ 白名单**必须包含冒号**
 *
 * 打点名的格式是 `kind:tab`。初版白名单漏了冒号，导出时被静默删掉：
 *
 * ```text
 * 实际写进日志：  preloadcontrol=122   ← 读不出是哪个页签
 * 应该写进日志：  preload:control=122
 * ```
 *
 * 名字一旦丢失分隔符就没法机械解析，白白浪费一次真机复现机会。
 */
export async function dumpPerfLog(): Promise<void> {
  if (!hasBridge()) {
    return;
  }

  const marks = (window as { __perf?: [string, number][] }).__perf ?? [];
  const body = marks.map(([n, t]) => `${n}=${t}`).join(" ");

  // 白名单：字母、数字、下划线、点、等号、**冒号**、短横线、空格。
  //
  // 这样拼出的字符串不可能含引号、分号、反引号或 `$`，无法逃逸出
  // 双引号包裹的 `sh -c` 参数。冒号在双引号内无特殊含义，可安全放行。
  const safe = body.replace(/[^A-Za-z0-9_.:=\- ]/g, "");

  const ts = new Date().toISOString().slice(0, 19);
  const dir = "/data/adb/thermo_charge/logs";

  /**
   * 同时导出**界面异常**
   *
   * `main.ts` 把捕获到的异常攒在 `window.__errs` 里，但**只给用户看
   * 一句中文提示**（原始英文错误对用户毫无意义）。技术细节就靠这里
   * 带出来 —— 否则那些错误等于被丢掉了，用户报"界面出错"时无从下手。
   *
   * 逐条写成 `ERR| 文本`（文本已过白名单，与上面同样安全）。
   */
  const errs = ((window as { __errs?: string[] }).__errs ?? []).map((e) =>
    e.replace(/[^A-Za-z0-9_.:=\- ()[\]/]/g, "").slice(0, 200),
  );

  const lines = [`=== ${ts} === ${safe}`];
  for (const e of errs) {
    lines.push(`ERR| ${e}`);
  }

  // 一次写入（用循环而不是多次 append，减少一次 shell 启动）
  const cmd =
    `mkdir -p ${dir} && ` +
    lines.map((l) => `printf '%s\\n' "${l}" >> ${dir}/perf.log`).join(" && ");

  await execGuarded(cmd, TIMEOUT_WRITE_MS, "记录性能数据");
}

/**
 * 导出一份**可分享的诊断日志**
 *
 * # 为什么需要它
 *
 * 用户要求（2026-10-14）：「日志页面设计导出按钮。方便其他用户反馈时
 * 可以提供日志」。
 *
 * 此前的问题是：诊断信息**散在四个地方**，让用户逐个去找是不现实的：
 *
 * | 信息 | 原本在哪 |
 * |---|---|
 * | 模块/系统版本、机型 | 「更多」页 |
 * | 当前生效配置 | `config.json` |
 * | 事件流（充电状态变化、模式切换） | 「日志」页 |
 * | worker/supervisor 运行日志 | `logs/main.log`（**界面看不到**） |
 *
 * ⇒ 反馈问题时用户不知道该给什么，维护者收到的信息也常常不完整。
 *
 * # 设计：一个**自包含**的纯文本文件
 *
 * 写成一个文件（而不是打包成 tar/zip），理由：
 *
 * | 好处 | 说明 |
 * |---|---|
 * | 用户容易操作 | 聊天软件能直接发 `.txt`，压缩包往往被拦或要另存 |
 * | 内容可读 | 用户自己也能看懂里面有什么（隐私透明，不是黑盒） |
 * | 大小可控 | 用 `tail` 截断，不会因为日志长到几 MB 而发不出去 |
 *
 * # ⚠️ 隐私
 *
 * 有意**不含**：账号、Wi-Fi、应用列表、IMEI、手机号。
 * 含机型/系统版本是必要的（诊断离不开它）。
 * 界面上的说明要写清楚这一点，让用户在点之前就知道。
 *
 * @returns 报告文件的**设备端完整路径**（「下载」目录下，带时间戳）。
 *          复制失败时返回数据目录里的那份（见下方说明）。
 */
export async function exportDiagnosticLog(): Promise<string> {
  if (!hasBridge()) {
    throw new ApiError("no-bridge", "只能在模块的 WebUI 里导出日志");
  }

  /**
   * ⚠️ **两个目录别搞混**（真机实测踩到）
   *
   * | 目录 | 内容 | 可否写 |
   * |---|---|---|
   * | `MOD` = `/data/adb/modules/a_hyperpower` | 二进制（`bin/`）、`module.prop`、`webroot/` | **模块目录（发布物）** |
   * | `DATA` = `/data/adb/thermo_charge` | `config.json`、`logs/`、`events.jsonl` | ✅ 数据目录 |
   *
   * 首次实现把两者混为一谈（都用 `DATA`），结果报告里：
   *
   * ```text
   * (读不到 module.prop)                        ← 它在 MOD 下
   * /data/adb/thermo_charge/bin/ctl: inaccessible or not found   ← bin 也在 MOD 下
   * ```
   *
   * 报告生成"成功"了，但最关键的版本与状态两段是空的 ——
   * 正是"看起来成功其实没用"的那类缺陷。**已实测修正**。
   */
  const MOD = "/data/adb/modules/a_hyperpower";
  const DATA = "/data/adb/thermo_charge";

  /**
   * ⚠️ 先把 **WebView 内存里**的诊断数据落盘，再生成报告
   *
   * `window.__perf`（启动/切页耗时）与 `window.__errs`（界面异常详情）
   * 都只活在**内存**里，worker 与 shell 都看不到 —— 必须先由
   * [`dumpPerfLog`] 写进 `logs/perf.log`，下面报告的 `perf.log` 段才**有内容**。
   *
   * # 为什么放在这里（而不是留一个单独按钮）
   *
   * 原先「更多」页有个「记录性能数据」按钮专门做这件事。用户要求删掉它
   * （2026-10-14）—— 删得对：用户看到卡顿时不会专门翻到「更多」页去点按钮
   * （那一刻早过去了），而且既然有了"导出日志"，就不该让用户
   * 再判断"我这次该点哪个"。
   *
   * ⇒ 合并到这里：一次导出同时拿到**运行日志 + 打点 + 异常详情**，
   *    一项都没少，用户也少一个要理解的概念。
   *
   * ⚠️ 失败**不阻断**导出：打点只是附加信息，为它让整次导出失败不值得。
   *    （`dumpPerfLog` 在无桥时本来就静默返回。）
   */
  try {
    await dumpPerfLog();
  } catch {
    // 忽略：报告其余部分仍然有价值
  }

  /**
   * 报告先生成到**数据目录**，再复制一份到**下载目录**
   *
   * # 为什么不是直接写到下载目录
   *
   * | 好处 | 说明 |
   * |---|---|
   * | 稳定 | 数据目录是本模块自己的地盘，权限与 SELinux 都是确定的 |
   * | 可回退 | 万一复制失败（存储未挂载、权限异常），数据目录那份还在，至少能用 `adb` 取 |
   * | 诊断一致 | 维护者按固定路径 `thermo-report.txt` 也能找到 |
   *
   * # 为什么要复制到下载目录（用户要求，2026-10-14）
   *
   * 用户原话：「导出位置不对，导出到下载目录」。
   *
   * 首次实现只写 `/data/adb/thermo_charge/` —— 那是**只有 root 能看的地方**：
   * 用户打开文件管理器**根本找不到**，也就**没法把文件发给别人**，
   * 而"发给别人"正是这个功能的**唯一目的**。
   *
   * ⇒ 用户能直接分享的位置（「下载」）才是正确的落点。
   *
   * # 文件名带时间戳
   *
   * `thermo-report-20261014-1500.txt` —— 用户可能隔一段时间再导一次
   * （对比问题发生的不同时刻），带时间戳才不会互相覆盖。
   * 时间戳也让维护者一眼知道**报告对应哪个时刻**。
   */
  const report = `${DATA}/thermo-report.txt`;

  // 用 `{}` 包住一段 shell 脚本，一次执行写完整个文件。
  //
  // ⚠️ 每一段都带 `2>&1` 与 `|| true`：
  //    只要有一个子命令不存在（换机型、换版本），整段不该因此中断 ——
  //    导出的是**尽力而为**的报告，有哪块缺了就在文件里留一行说明。
  //
  // ⚠️ 重定向用 `>` 只在**第一段**（最外层），里面都是普通输出 —— 否则会互相覆盖。
  const script = [
    `set +e`,
    `TS=$(date +%Y%m%d-%H%M%S)`,
    // ⚠️ `/sdcard` 在所有 Android 上都是指向当前用户内部存储的符号链接
    `DL=/sdcard/Download`,
    `DLFILE=$DL/thermo-report-$TS.txt`,
    // 调试数据另存一个文件，与报告**同名不同后缀** —— 便于配对
    // （用户手上可能同时有好几次导出）
    `DBGFILE=$DL/thermo-debug-$TS.jsonl`,
    `{`,
    `  echo "===== 澎湃充控 诊断报告 ====="`,
    `  echo "生成时间: $(date '+%Y-%m-%d %H:%M:%S %z')"`,
    `  echo`,
    `  echo "----- 版本 -----"`,
    `  grep -E '^(id|version|versionCode)=' ${MOD}/module.prop 2>/dev/null || echo "(读不到 module.prop)"`,
    `  echo`,
    `  echo "----- 机型与系统 -----"`,
    `  echo "机型    : $(getprop ro.product.marketname 2>/dev/null)"`,
    `  echo "代号    : $(getprop ro.product.device 2>/dev/null)"`,
    `  echo "Android : $(getprop ro.build.version.release 2>/dev/null) (API $(getprop ro.build.version.sdk 2>/dev/null))"`,
    `  echo "内核    : $(uname -r 2>/dev/null)"`,
    `  echo "页大小  : $(getconf PAGE_SIZE 2>/dev/null) 字节"`,
    `  echo`,
    `  echo "----- 进程 -----"`,
    `  ps -A -o PID,ETIME,ARGS 2>/dev/null | grep '[t]hermo' || echo "(未发现 thermo 进程)"`,
    `  echo`,
    `  echo "----- 当前状态 -----"`,
    `  ${MOD}/bin/ctl status 2>&1 || echo "(ctl status 失败)"`,
    `  echo`,
    `  echo "----- 生效配置 -----"`,
    `  cat ${DATA}/config.json 2>&1 || echo "(读不到 config.json)"`,
    `  echo`,
    `  echo "----- 运行时事件（最近 100 条）-----"`,
    `  ${MOD}/bin/ctl logs events 100 2>&1 || echo "(读事件失败)"`,
    `  echo`,
    /**
     * 运行日志走 **logcat**，不是 `main.log`
     *
     * 🔴 真机实测（2026-10-14）发现的第三个缺陷：
     *
     * | 文件 | 真机上 | 说明 |
     * |---|---|---|
     * | `logs/main.log` | **不存在** | `paths::main_log()` **只有定义与读取、没有写入者** —— 是预留路径 |
     * | `logs/service.log` | 存在 | 只有 `service.sh` 的启动/结束记录，无 worker 细节 |
     * | `logs/daemon.out` | 存在但 0 字节 | |
     * | **logcat（tag `thermo_charge`）** | ✅ **真正有内容** | `logging.rs`：Android 上日志走 `android_logger` |
     *
     * 首次实现 `tail main.log`，结果是 `(无 main.log)` ——
     * 而那段恰恰是排查问题**最需要**的信息（决策过程、钳位、节点探测）。
     * 报告"生成成功"却缺了核心内容，属于"看起来成功其实没用"。
     *
     * ⚠️ logcat 是**环形缓冲**：日志会被冲掉。因此这一段只在
     *    **刚出问题不久**时有效。文案里要说明，否则用户第二天才导出会疑惑。
     */
    `  echo "----- 运行日志（logcat，tag=thermo_charge，最近 300 行）-----"`,
    `  echo "（提示：logcat 是环形缓冲，较早的日志可能已被系统冲掉）"`,
    `  logcat -d -s thermo_charge 2>/dev/null | tail -n 300 || echo "(logcat 读取失败)"`,
    `  echo`,
    `  echo "----- main.log（若存在；Android 上通常为空，日志走 logcat）-----"`,
    `  tail -n 100 ${DATA}/logs/main.log 2>/dev/null || echo "(无 main.log)"`,
    `  echo`,
    `  echo "----- service.log（最近 50 行）-----"`,
    `  tail -n 50 ${DATA}/logs/service.log 2>/dev/null || echo "(无 service.log)"`,
    `  echo`,
    `  echo "----- 界面打点与异常（最近 20 次导出）-----"`,
    `  echo "（每行 = 一次导出；ERR| 开头的是界面报错的技术细节）"`,
    `  tail -n 20 ${DATA}/logs/perf.log 2>/dev/null || echo "(未记录过性能数据)"`,
    `  echo`,
    // ── 调试模式数据（用户要求 2026-10-17）──
    //
    // ⚠️ **不把 debug.jsonl 塞进报告本体**：它有 20 MB，塞进去会让报告
    //    既发不出去也打不开。这里只报**它存在与多大**，真正的数据
    //    由下面那步**另存为一个文件**。
    //
    // 同时在报告里报一份**尾部预览**（最近 30 行）——
    // 这样即使调试数据那个大文件没被发出去，报告本身也带了线索。
    `  echo "----- 调试模式数据 -----"`,
    `  if [ -s ${DATA}/debug/debug.jsonl ]; then`,
    `    echo "调试数据存在：$(wc -c < ${DATA}/debug/debug.jsonl) 字节 / $(wc -l < ${DATA}/debug/debug.jsonl) 行"`,
    `    echo "（完整数据会另存为一个 thermo-debug-*.jsonl 文件，一并发送即可）"`,
    `    echo`,
    `    echo "--- 尾部队道预览（最近 30 行）---"`,
    `    tail -n 30 ${DATA}/debug/debug.jsonl 2>/dev/null`,
    `  else`,
    `    echo "(未开启调试模式，或尚未产生数据)"`,
    `    echo "提示：在「反馈问题」里打开「调试模式」后复现一次问题，再来导出。"`,
    `  fi`,
    `} > ${report} 2>&1`,
    // 复制到下载目录。成功报告下载路径，失败退回数据目录路径 ——
    // 两种情况前端都能给用户一个**真实存在**的路径。
    `if cp ${report} $DLFILE 2>/dev/null; then`,
    `  echo $DLFILE`,
    `else`,
    `  echo ${report}`,
    `fi`,
    // 调试数据另存一个文件（与报告同名，后缀不同，便于配对）
    //
    // ⚠️ 失败**不算错误**：没开调试模式时它本来就不存在。
    //    所以这里不 echo 路径，前端用 `$DLFILE` 推导出来即可。
    `if [ -s ${DATA}/debug/debug.jsonl ]; then`,
    `  cp ${DATA}/debug/debug.jsonl $DBGFILE 2>/dev/null || true`,
    `fi`,
    // 最后一行是字节数（前端据此判断"是否真的写成功了"）
    `wc -c < ${report}`,
  ].join("\n");

  // ⚠️ `execGuarded` 返回的是 `{errno, stdout, stderr}` 对象，不是字符串。
  //
  // 脚本输出**两行**：
  //
  // ```text
  // /sdcard/Download/thermo-report-20261014-1500.txt   ← 最终路径
  // 20075                                              ← 字节数
  // ```
  const res = await execGuarded(script, TIMEOUT_WRITE_MS, "导出日志");
  const lines = String(res.stdout)
    .split("\n")
    .map((l) => l.trim())
    .filter((l) => l.length > 0);

  // ⚠️ 用索引而不是 `Array.prototype.at()` —— 那是 **ES2022**，
  //    而本项目 `target: ES2020`（`.at()` 在旧 WebView 上也没有）。
  const n = lines.length;
  const size = Number.parseInt(lines[n - 1] ?? "", 10);
  const path = lines[n - 2] ?? "";

  if (!Number.isFinite(size) || size <= 0 || !path) {
    // 文件没写成（或成了空文件）—— 报错比"看起来成功了"好
    throw new ApiError(
      "E_EXPORT",
      `导出失败（${res.stderr || "无输出"}）。可重试一次；若持续失败请反馈。`,
    );
  }

  return path;
}

/** 后端错误 */
export class ApiError extends Error {
  constructor(
    readonly code: string,
    message: string,
  ) {
    super(message);
    this.name = "ApiError";
  }
}

/** 原生桥是否可用 */
function hasBridge(): boolean {
  return (
    typeof (globalThis as { ksu?: { exec?: unknown } }).ksu?.exec === "function"
  );
}

// ============================================================
//  底层调用
// ============================================================

/**
 * 执行一条 `ctl` 命令（**契约命令**通道：`ctl` → UDS → worker）
 *
 * **只包住可执行文件路径**，参数裸露。
 *
 * `ksu.exec` 把字符串交给 root shell（等价 `sh -c "<字符串>"`），
 * shell 会剥掉引号并按空白切分 argv：
 *
 * | 写法 | 收到的 argv | 结果 |
 * |---|---|---|
 * | `"/path/ctl" limit set 85` | `["limit","set","85"]` | ✅ |
 * | `"/path/ctl" "limit set 85"` | `["limit set 85"]` | ❌ `E_TYPE 未知命令` |
 *
 * 参数只允许来自本文件内部 —— 界面层调的是下面的类型化函数。
 *
 * @param parts 命令与参数（如 `["status"]`）
 * @param timeoutMs 超时；读操作用默认值，写操作传 [`TIMEOUT_WRITE_MS`]
 *
 * ⚠️ 超时**必须**有 —— 见 [`execGuarded`] 的说明（老项目 R2 规则）。
 */
async function run(
  parts: string[],
  timeoutMs = TIMEOUT_READ_MS,
): Promise<unknown> {
  if (!hasBridge()) {
    return mock(parts);
  }

  const line = `"${CTL}" ${parts.join(" ")}`;

  let result;
  try {
    result = await execGuarded(line, timeoutMs, `\`${parts.join(" ")}\``);
  } catch (e) {
    // 超时错误要**原样抛出** —— 它的文案比"无法执行 ctl"更有用
    if (e instanceof ApiError) {
      throw e;
    }
    throw new ApiError("E_INTERNAL", `无法执行 ctl：${String(e)}`);
  }

  // 契约：失败信息走 stderr，stdout 保持为空
  const raw = result.stdout.trim() || result.stderr.trim();
  if (!raw) {
    throw new ApiError(
      "E_INTERNAL",
      result.errno === 127
        ? "找不到 ctl 入口（模块是否安装完整？）"
        : `后端无输出（退出码 ${result.errno}）`,
    );
  }

  let parsed: { ok?: boolean; data?: unknown; code?: string; msg?: string };
  try {
    parsed = JSON.parse(raw);
  } catch {
    throw new ApiError("E_INTERNAL", `响应不是合法 JSON：${raw.slice(0, 120)}`);
  }

  // ⚠️ 契约提示：`Response` 是 untagged 联合，`{"ok":false,…}` 也可能被
  //    解析出来。**必须再按 ok 字段判一次**，不能只看形状。
  if (parsed.ok === true) {
    return parsed.data ?? {};
  }
  if (parsed.ok === false) {
    throw new ApiError(parsed.code ?? "E_INTERNAL", parsed.msg ?? "未知错误");
  }
  throw new ApiError("E_INTERNAL", "响应缺少 ok 字段");
}

// ============================================================
//  带超时的执行（**所有 exec 都必须有超时**）
// ============================================================

/**
 * 读类命令的超时（毫秒）
 *
 * `status` / `get` 等只读命令，正常在毫秒级返回。
 * 6 秒已是很宽的余量。
 */
const TIMEOUT_READ_MS = 6_000;

/**
 * 写类命令的超时（毫秒）
 *
 * 写路径是**入队**的（`set` 只把动作放进 `Pending`，由 worker 在拍末应用），
 * 因此正常也很快。留 15 秒是为了覆盖"worker 刚被唤醒、正在跑一拍"的情况。
 */
const TIMEOUT_WRITE_MS = 15_000;

/**
 * 本地命令的超时（毫秒）
 *
 * `device probe` 要读几十个 sysfs 节点，且完整模式还要**试写**一次充电节点
 * —— 是本项目最慢的命令，因此给到 30 秒。
 */
const TIMEOUT_LOCAL_MS = 30_000;

/** `exec` 的返回类型（`kernelsu` 未导出该 interface，从函数签名推导） */
type ExecResults = Awaited<ReturnType<typeof exec>>;

/**
 * **带超时**的 `ksu.exec`
 *
 * # 🔴 为什么必须有超时（老项目事故换来的规则）
 *
 * `kernelsu` 的 `ExecOptions` **只有 `cwd` / `env`，没有超时字段**
 * （见 `node_modules/kernelsu/index.d.ts`）—— 也就是说 `await exec(...)`
 * 在极端情况下会**永久挂起**。
 *
 * 老项目 `ThermoCharge` 为此吃过事故，并把教训固化成两条硬规则：
 *
 * | 规则 | 老项目原文 |
 * |---|---|
 * | **R1** | 事件绑定必须**同步先于**任何异步操作 —— 回调不来时按钮 `onclick` 全为 null ⇒ **"点了没反应"** |
 * | **R2** | **所有 `exec` 必须有超时**（默认 15s，读操作 6s），**绝不无限等待** |
 *
 * 本项目用 Promise 风格，R1 天然满足（绑定与 await 分离）；
 * 但 **R2 必须显式实现** —— 就是本函数。
 *
 * # 挂了会怎样
 *
 * | 场景 | 无超时 | 有超时 |
 * |---|---|---|
 * | worker 卡死（真机出过 `D` 状态） | 界面**永远转圈**，用户只能杀进程 | 6 秒后报"后端无响应" |
 * | UDS 阻塞 | 同上 | 同上 |
 * | 模块被卸载但页面没关 | 同上 | 同上 |
 *
 * # ⚠️ 超时**不会**终止后端进程
 *
 * `Promise.race` 只是让**前端**不再等待 —— root 侧的 `exec` 仍在跑。
 * 这对本项目可接受：
 *
 * - 命令都是**幂等**的（`status` 读、`set` 写同一个值）
 * - 后端自身有单写者纪律与队列，不会因重复调用而错乱
 *
 * 真要杀进程需要 `spawn` + `kill`，本项目不需要那个复杂度。
 */
async function execGuarded(
  line: string,
  timeoutMs: number,
  what: string,
): Promise<ExecResults> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      exec(line),
      new Promise<never>((_, reject) => {
        timer = setTimeout(() => {
          reject(
            new ApiError(
              "E_TIMEOUT",
              `${what}超过 ${timeoutMs / 1000} 秒未响应 —— ` +
                `后端可能已卡住。可下拉刷新重试；若持续如此，请到「更多」页查看日志`,
            ),
          );
        }, timeoutMs);
      }),
    ]);
  } finally {
    // ⚠️ 必须清理定时器
    //
    // 不清理的话，每次成功的调用都会留下一个待触发的定时器 ——
    // 界面开着轮询（每 3 秒一次）时会持续累积，既占内存，
    // 也会让已经超时过的错误**在稍后重复抛出**（Promise 已 settled 时
    // `reject` 是空操作，但定时器本身仍在跑）。
    if (timer !== undefined) {
      clearTimeout(timer);
    }
  }
}

/**
 * 直接调用 `thermo` 二进制（**不**经 `ctl`）
 *
 * # 为什么需要第二条通道
 *
 * 命令分两类（见 `src/cli.rs` 的文件头）：
 *
 * | 类型 | 例子 | 通道 | 为什么 |
 * |---|---|---|---|
 * | **契约命令** | `status` / `limit set` / `get` | `ctl` → UDS → worker | 需要与 worker 的单写者纪律配合 |
 * | **本地命令** | `device probe` / `device show` | 直接跑 `thermo` | 只读或瞬时操作，不碰运行态 |
 *
 * 走 `ctl` 发本地命令会得到 `E_TYPE 未知命令`。
 *
 * ⚠️ **执行耗时**：`device probe` 要读几十个 sysfs 节点，是秒级操作。
 * 调用方需要自行给用户"正在检测"的反馈。
 *
 * ⚠️ 超时用 [`TIMEOUT_LOCAL_MS`]（30 秒）—— 比契约命令宽松，
 * 因为 `device probe` 是目前最慢的命令（完整模式还要试写一次充电节点）。
 */
async function runBinary(
  parts: string[],
  timeoutMs = TIMEOUT_LOCAL_MS,
): Promise<unknown> {
  if (!hasBridge()) {
    return mock(parts);
  }

  const line = `"${BINARY}" ${parts.join(" ")}`;

  let result;
  try {
    result = await execGuarded(line, timeoutMs, `\`${parts.join(" ")}\``);
  } catch (e) {
    // 超时错误原样抛出（文案更有用）
    if (e instanceof ApiError) {
      throw e;
    }
    throw new ApiError("E_INTERNAL", `无法执行 thermo：${String(e)}`);
  }

  const out = result.stdout.trim();
  if (!out) {
    throw new ApiError(
      "E_INTERNAL",
      result.stderr.trim() ||
        (result.errno === 127
          ? "找不到 thermo 二进制（模块是否安装完整？）"
          : `命令无输出（退出码 ${result.errno}）`),
    );
  }

  // ⚠️ 本地命令的 stdout 是**裸 JSON**（没有 `ok` 包装）——
  //    与 `ctl` 的契约响应不同，这里不能按 `ok` 字段判定。
  try {
    return JSON.parse(out);
  } catch {
    throw new ApiError("E_INTERNAL", `响应不是合法 JSON：${out.slice(0, 120)}`);
  }
}

// ============================================================
//  类型化接口
// ============================================================

/** 读取实时状态 */
export function fetchStatus(): Promise<StatusSnapshot> {
  return run(["status"]) as Promise<StatusSnapshot>;
}

/** 读取充电上限功能状态 */
export function fetchLimit(): Promise<AddonStatus> {
  return run(["limit", "show"]) as Promise<AddonStatus>;
}

// ── PPS 解锁（没有 `limit` 那样的便捷别名，走通用 `addon` 命令）──

/**
 * 读取 PPS 解锁状态
 *
 * 用通用的 `addon show pps`，而不是新加一个 `pps show` 别名 ——
 * 功能侧的通用命令已经够用，多一个别名就多一处要维护的接口面。
 */
export function fetchPps(): Promise<AddonStatus> {
  return run(["addon", "show", "pps"]) as Promise<AddonStatus>;
}

/** 开启 / 关闭 PPS 解锁 */
export async function setPpsEnabled(on: boolean): Promise<void> {
  await run(["addon", on ? "on" : "off", "pps"], TIMEOUT_WRITE_MS);
}

/**
 * 设定 PPS 解锁模式
 *
 * | 值 | 含义 |
 * |---|---|
 * | `lock` | 始终锁定（不管插什么头都解锁） |
 * | `auto` | 只在已是 PPS 协议时解锁 |
 */
export async function setPpsMode(mode: "lock" | "auto"): Promise<void> {
  await run(["set", "pps.mode", mode], TIMEOUT_WRITE_MS);
}

/** 设定是否伪造金标动画 */
export async function setPpsForge(on: boolean): Promise<void> {
  await run(["set", "pps.forge_broadcast", String(on)], TIMEOUT_WRITE_MS);
}

/**
 * 读取 `pps` 配置分区（原始对象）
 *
 * ⚠️ 不做严格类型：分区里有 6 个键，而界面目前只用到
 * `forge_broadcast`。为它再定义一个完整 interface 属于过度设计 ——
 * 需要时按字段名取，缺就回退。
 */
export function fetchPpsSection(): Promise<Record<string, unknown>> {
  return run(["get", "--section", "pps"]) as Promise<Record<string, unknown>>;
}

/**
 * 设定充电上限（**同时开启**该功能，符合直觉）
 *
 * @param soc 60~100；越界在这里就拦掉，不进入命令串
 */
export async function setLimit(soc: number): Promise<void> {
  if (!Number.isFinite(soc) || soc < SOC_MIN || soc > SOC_MAX) {
    throw new ApiError(
      "E_RANGE",
      `上限 ${soc} 超出范围（${SOC_MIN} ~ ${SOC_MAX}）`,
    );
  }
  await run(["limit", "set", String(Math.round(soc))], TIMEOUT_WRITE_MS);
}

/** 开启充电上限（沿用当前设定值） */
export async function limitOn(): Promise<void> {
  await run(["limit", "on"], TIMEOUT_WRITE_MS);
}

/**
 * 关闭充电上限
 *
 * ⚠️ 语义：关闭 = **停止干预、零写入**。
 * 驱动里的限制值**保持不变** —— 若之前设过较低上限，手机会停在那个电量。
 * 界面必须把这一点讲清楚，否则用户会以为"关了就该能充满"。
 */
export async function limitOff(): Promise<void> {
  await run(["limit", "off"], TIMEOUT_WRITE_MS);
}

/** 读取温控分区 */
export function fetchThermal(): Promise<ThermalSection> {
  return run(["get", "--section", "thermal"]) as Promise<ThermalSection>;
}

/** 读取控制分区 */
export function fetchControl(): Promise<ControlSection> {
  return run(["get", "--section", "control"]) as Promise<ControlSection>;
}

/** 读取温度曲线 */
export function fetchCurve(): Promise<CurvePoint[]> {
  return run(["get", "--section", "curve"]) as Promise<CurvePoint[]>;
}

/**
 * 批量写配置
 *
 * 用 `--batch` 而不是连续单键写：内核要求 `relax_temp ≤ tighten_temp - 1`，
 * 单独改一个键会**当场被跨键校验拒绝**（真机实测踩过这个坑）。
 */
export async function setConfig(pairs: [string, string][]): Promise<void> {
  if (pairs.length === 0) {
    return;
  }
  const args = pairs.map(([k, v]) => `${k}=${v}`).join(" ");
  await run(["set", "--batch", args], TIMEOUT_WRITE_MS);
}

/** 读取设备档案 */
export function fetchDevice(): Promise<DeviceProfileView> {
  return run(["device", "show"]) as Promise<DeviceProfileView>;
}

/**
 * 执行设备检测
 *
 * # 两种模式
 *
 * | 模式 | 命令 | 内容 | 耗时 | 副作用 |
 * |---|---|---|---|---|
 * | **快速** | `--quick` | 全部只读项 | 秒级 | **无** |
 * | **完整** | （不带 quick） | 额外验证档位可写上界 | 秒级 | 写探（会临时改充电参数并恢复） |
 *
 * ⚠️ 插电状态对两项的要求**相反**（2026-10-19 真机确认）：
 *
 * | 项 | 要求 | 不满足时 |
 * |---|---|---|
 * | 电流上限 | **必须未充电** | 跳过该项（充电中读到的是我们自己的写入值） |
 * | 档位写探 | **必须充电中** | 跳过写探（驱动此时不接受写入，硬写会误降一档） |
 *
 * 其余项（标称功率、档位上限、硬件温度）与插电状态无关：
 * 标称功率从电池型号读；档位上限从「档位表（设备树）」或
 * 「节点 `charge_control_limit_max`」读；硬件温度同理，
 * 设备树或 `max_life_temp` 一类节点。
 *
 * @param opts.quick 快速模式（零副作用）
 * @param opts.save 是否保存为 `device.json`（不保存则只返回结果供预览）
 * @param opts.confirm 完整模式必须显式确认（含写探）
 */
export function probeDevice(opts: {
  quick: boolean;
  save: boolean;
  confirm: boolean;
}): Promise<ProbeResult> {
  const parts = ["device", "probe", "--json"];
  if (opts.quick) {
    parts.push("--quick");
  }
  if (opts.save) {
    parts.push("--save");
  }
  if (opts.confirm) {
    parts.push("--confirm");
  }
  return runBinary(parts) as Promise<ProbeResult>;
}

/**
 * 读一次**实际**充电功率（诊断用）
 *
 * # 与 `fetchDevice` 的区别（重要）
 *
 * | 项 | 本函数 | `fetchDevice` |
 * |---|---|---|
 * | 测什么 | **此刻**的实际功率 | 机型**能力上限**（标称值） |
 * | 随什么变化 | 电量、充电器、温度 | 不变（出厂声明） |
 * | 写档案 | ❌ **绝不** | 由 `device probe` 写 |
 *
 * ⚠️ 结果**只用于展示**，不参与任何决策。
 *
 * 耗时由采样窗口决定（默认 3 秒）。
 */
export function measurePower(opts?: {
  durationMs?: number;
}): Promise<PowerMeasurement> {
  const parts = ["measure", "--json"];
  if (opts?.durationMs) {
    parts.push("--duration-ms", String(opts.durationMs));
  }
  return runBinary(parts) as Promise<PowerMeasurement>;
}

/**
 * 设置**最大充电功率**（W）
 *
 * # 语义
 *
 * 设 50 ⇒ 充电功率**不超过 50W**。
 *
 * @param watts `0` = **不限制**（关闭功率上限）
 *
 * ⚠️ 上限不得超过检测出的 `power_max_w`（后端会拒绝并给出 `E_RANGE`）。
 */
export async function setPowerLimit(watts: number): Promise<void> {
  if (!Number.isFinite(watts) || watts < 0) {
    throw new ApiError("E_RANGE", `功率上限 ${watts} 非法`);
  }
  await setConfig([["control.target_power_w", String(Math.round(watts))]]);
}

/** 读取最近事件 */
export async function fetchEvents(lines = 20): Promise<RuntimeEvent[]> {
  const raw = (await run([
    "logs",
    "events",
    "--lines",
    String(lines),
  ])) as { lines?: string[] };

  // 每行是一条独立 JSON；个别行坏掉不该让整个列表消失
  return (raw.lines ?? []).flatMap((t) => {
    try {
      return [JSON.parse(t) as RuntimeEvent];
    } catch {
      return [];
    }
  });
}

// ============================================================
//  多模式配置
// ============================================================

/** 读取四种模式的当前状态与各自参数 */
export function fetchModes(): Promise<ModeList> {
  return run(["mode", "list"]) as Promise<ModeList>;
}

/**
 * 切换模式
 *
 * ⚠️ 切换会**保存当前模式的值**、再**装载目标模式的值** ——
 * 这是与旧 `preset apply`（纯覆盖）的本质区别，见 `types.ts` 的 ModeInfo。
 *
 * @param id `conservative` / `official` / `turbo` / `custom`
 */
export async function switchMode(id: string): Promise<void> {
  if (!["conservative", "official", "turbo", "custom"].includes(id)) {
    throw new ApiError("E_RANGE", `未知模式：${id}`);
  }
  await run(["mode", "switch", id], TIMEOUT_WRITE_MS);
}

// ============================================================
//  充电历史
// ============================================================

/** 会话列表（**不含曲线点**，列表页专用） */
export function fetchHistoryList(): Promise<HistoryListResult> {
  return run(["history", "list"]) as Promise<HistoryListResult>;
}

/**
 * 取单次会话的完整曲线
 *
 * @param id 省略则取**最近一次**
 */
export function fetchHistorySession(id?: string): Promise<HistorySession> {
  const args = ["history", "show"];
  if (id) args.push(id);
  return run(args) as Promise<HistorySession>;
}

/**
 * 清空充电历史
 *
 * ⚠️ **正在充电的这一次会继续记录** —— 清空的只是历史归档。
 * 界面文案要把这点讲清楚，否则用户会以为"连当前的也停了"。
 *
 * ⚠️ 自 0.3.12 起，界面上的按钮已改为「清空全部记录」
 *    （走 [`clearRecords`]）—— 这个函数保留给"只想清历史"的场景。
 */
export async function clearHistory(): Promise<void> {
  // 清空要重写整个历史文件，可能比一般写操作慢 ⇒ 用写超时（15s）
  await run(["history", "clear"], TIMEOUT_WRITE_MS);
}

/**
 * **清空全部记录**（事件 + 充电历史 + 诊断报告 + 日志文件）
 *
 * 用户要求（2026-10-17）：「清空充电历史记录好像只能清空历史记录，
 * 加入更多功能，需要清空所有的临时文件，包括最近事件等等」。
 *
 * # ⚠️ 不碰配置与设备档案
 *
 * 后端已用测试把这条钉死（`清空记录绝不能碰配置与设备档案`）——
 * 特别是 `device.json`：清掉它模块会进入「未检测不控制」，
 * **完全不管充电**，必须重跑检测才恢复。
 *
 * 返回后端给出的 `scope`（会清什么）与 `kept`（不碰什么），
 * 供界面在确认弹窗里如实展示 —— 而不是前端自己写一份（那会漂移）。
 */
export async function clearRecords(): Promise<Record<string, unknown>> {
  const raw = (await run(["records", "clear"], TIMEOUT_WRITE_MS)) as Record<
    string,
    unknown
  >;
  return raw;
}

/**
 * 只查询**会清什么**，不真删
 *
 * 供界面在弹窗里展示清单 —— 文案由后端生成，前端只负责显示，
 * 因此不可能出现「弹窗说清了 A，实际只清了 B」。
 */
export async function recordsScope(): Promise<Record<string, unknown>> {
  const raw = (await run(["records", "list"])) as Record<string, unknown>;
  return raw;
}

/**
 * 调试模式状态
 *
 * 用户要求（2026-10-17）：「在反馈问题中增加一个调试模式按钮，
 * 如果开启调试模式会记录更详细的数据，方便你抓包分析」。
 */
export interface DebugStatus {
  /** 是否已开启 */
  enabled: boolean;
  /** 数据文件的完整路径 */
  path: string;
  /** 已记录字节数（从文件的 metadata 取，不数行 —— 界面每 3 秒会查一次） */
  bytes: number;
  /** 上限字节数 */
  max_bytes: number;
  /** 上限（MB，给界面直接显示） */
  max_mb: number;
  /** 一句话说明（开启/未开启两种口径） */
  note: string;
}

/** 读调试模式状态 */
export async function fetchDebugStatus(): Promise<DebugStatus> {
  const raw = (await run(["debug", "status"])) as DebugStatus;
  return raw;
}

/**
 * 开关调试模式
 *
 * ⚠️ 只改**配置**，真正的写入器由 worker 在**下一拍**打开 ——
 *    因此 `on` 之后要等一两秒才会开始有数据（界面文案里说明了）。
 */
export async function setDebugEnabled(on: boolean): Promise<void> {
  await run(["debug", on ? "on" : "off"], TIMEOUT_WRITE_MS);
}

// ============================================================
//  开发用示例数据（浏览器里打开时）
//
//  ⚠️ 仅用于开发调试：真机上面板数据必须来自后端，
//     因此这里的数值刻意做得"一眼看出是演示数据"。
// ============================================================

/** 演示模式下的可变状态（让开关/滑块在浏览器里能点动） */
const demo = {
  limitEnabled: false,
  soc: 80,
  /** 当前生效的模式（演示用） */
  mode: "custom",
  /** 调试模式开关（演示用） */
  debugOn: false,
  /** PPS 解锁开关（演示用） */
  ppsEnabled: false,
  /** PPS 解锁模式（演示用） */
  ppsMode: "lock",
};

/** 演示：四种模式 */
function mockModes(): unknown {
  const defs = [
    { id: "conservative", label: "保守", description: "低温慢充，最大化电池寿命" },
    { id: "official", label: "官方", description: "完全交由系统管理，模块不插手" },
    { id: "turbo", label: "极速", description: "尽可能快，温度阈值放宽" },
    { id: "custom", label: "自定义", description: "完全由你掌控" },
  ];
  return {
    current: demo.mode,
    modes: defs.map((d) => ({
      ...d,
      current: d.id === demo.mode,
      values: { thermal: { tighten_temp: 43, relax_temp: 42 }, control: { target_power_w: 43 } },
    })),
  };
}

/** 演示：一次充电的曲线 */
function mockHistorySession(): unknown {
  const base = Math.floor(Date.now() / 1000) - 5400;
  const points = Array.from({ length: 60 }, (_, i) => {
    // 前 30 点升温、后 30 点回落
    const temp = i < 30 ? 35 + i * 0.25 : 42.5 - (i - 30) * 0.15;
    const power = i < 20 ? 90 : i < 35 ? 45 : 20;
    return {
      t: base + i * 90,
      temp_c: Number(temp.toFixed(1)),
      power_w: power,
      soc: 20 + i,
      gear: i < 20 ? 0 : 3,
      limit_ua: i < 20 ? 18_400_000 : 7_000_000,
      phase: i < 20 ? "normal" : "tightening",
    };
  });
  return {
    id: String(base),
    started_at: base,
    ended_at: base + 5400,
    soc_start: 20,
    soc_end: 79,
    peak_temp_c: 42.5,
    energy_wh: 48.6,
    avg_power_w: 32.4,
    sample_s: 90,
    points,
  };
}

/** 演示：历史列表 */
function mockHistoryList(): unknown {
  const now = Math.floor(Date.now() / 1000);
  const lastStart = now - 5400;
  return {
    // ⚠️ 刻意给 **5 条**（多于 HISTORY_PREVIEW = 3）
    //
    // 演示模式应当能展示分页效果 —— 只给 2 条的话「更多」按钮永远不出现，
    // 界面改动在演示模式下就验证不到（真机上有 5 条真实记录，一致）。
    sessions: [
      {
        id: "1",
        started_at: lastStart,
        ended_at: now,
        duration_s: 5400,
        soc_start: 20,
        soc_end: 79,
        peak_temp_c: 42.5,
        energy_wh: 48.6,
        avg_power_w: 32.4,
        points: 60,
      },
      {
        id: "2",
        started_at: lastStart - 86_400,
        ended_at: lastStart - 79_000,
        duration_s: 7400,
        soc_start: 15,
        soc_end: 100,
        peak_temp_c: 39.8,
        energy_wh: 62.1,
        avg_power_w: 30.2,
        points: 82,
      },
      {
        id: "3",
        started_at: lastStart - 172_800,
        ended_at: lastStart - 165_000,
        duration_s: 7800,
        soc_start: 28,
        soc_end: 82,
        peak_temp_c: 40.6,
        energy_wh: 54.3,
        avg_power_w: 25.1,
        points: 88,
      },
      {
        id: "4",
        started_at: lastStart - 259_200,
        ended_at: lastStart - 251_000,
        duration_s: 8200,
        soc_start: 48,
        soc_end: 85,
        peak_temp_c: 38.7,
        energy_wh: 33.9,
        avg_power_w: 15.4,
        points: 94,
      },
      {
        id: "5",
        started_at: lastStart - 345_600,
        ended_at: lastStart - 340_000,
        duration_s: 5600,
        soc_start: 83,
        soc_end: 85,
        peak_temp_c: 40.3,
        energy_wh: 2.8,
        avg_power_w: 0.9,
        points: 62,
      },
    ],
    stats: {
      count: 5,
      energy_total_wh: 201.7,
      peak_temp_c: 42.5,
      keep: 30,
      enabled: true,
      sample_s: 30,
    },
    current: null,
  };
}

function mock(parts: string[]): unknown {
  const [head, sub] = parts;

  if (head === "mode") {
    if (sub === "switch") {
      demo.mode = parts[2] ?? "custom";
      return { changed: true, mode: demo.mode };
    }
    return mockModes();
  }

  if (head === "history") {
    if (sub === "show") return mockHistorySession();
    if (sub === "clear") return { cleared: true };
    return mockHistoryList();
  }

  // 调试模式（演示模式：只回状态，不真写文件）
  if (head === "debug") {
    if (sub === "on") {
      demo.debugOn = true;
    } else if (sub === "off") {
      demo.debugOn = false;
    }
    return {
      enabled: demo.debugOn,
      path: "/data/adb/thermo_charge/debug/debug.jsonl",
      bytes: demo.debugOn ? 2_411_724 : 0,
      max_bytes: 20 * 1024 * 1024,
      max_mb: 20,
      note: demo.debugOn
        ? "正在记录。复现完问题后记得关掉。"
        : "未开启。开启后会记录每拍决策链、节点原始读写与运行日志。",
    };
  }

  // 清空全部记录（演示模式：只回一份与后端同形的清单，不真删）
  if (head === "records") {
    return {
      cleared: sub === "clear",
      scope: [
        "最近事件（含轮转归档）",
        "充电历史（已完成会话）",
        "诊断报告",
        "界面打点日志",
        "开机记录",
        "主日志",
        "守护输出",
      ],
      kept: [
        "config.json（你的全部设置）",
        "device.json（设备档案 —— 清了模块会停止控制充电）",
        "presets.json（预设模板）",
      ],
    };
  }

  if (head === "status") {
    return {
      battery: {
        // 演示值：实际电流与「已写电流」刻意不同，展示系统的响应滞后
        current_a: 10.52,
        power_w: 42.7,
        soc: 65,
        status: "Charging",
        temp_c: 38.6,
        // 演示值取恒流末段的高电压（真机全程约 3.9 → 4.5 V），
        // 好让两位小数的意义一眼可见
        voltage_v: 4.32,
      },
      charger: {
        label: "PD_PPS",
        power_max_w: 100,
        protocol: "PD_PPS",
        usb_type: "USB_PD",
        weak: false,
      },
      // ⚠️ 这里原有 `scene: "3"`（系统场景的演示值），已随该字段删除（2026-10-17）
      control: { curr_now: 12_000_000, gear_now: 0, power_limit_w: 43 },
      // 演示值：模式取「极速」—— 与 mockModes() 里的 current 保持一致，
      // 否则状态页显示的模式和控制页高亮的那个会对不上
      mode: "turbo",
      mode_label: "极速",
      daemon: "running",
      detected: true,
      released: false,
      session: { duration_s: 3600 },
      state: "normal",
      thermal: {
        abs_max: 45,
        basis: "cell",
        // 演示值：比电池温度高约 10℃（真机实测的真实关系）
        cp_die_temp_c: 48.9,
        // 演示值：机身温度通常略高于电池温度（这也是 `basis = virt`
        // 时会"看起来更早降速"的原因）
        board_temp_c: 41.2,
        peak_c: null,
        tighten_temp: 43,
      },
      ticks: 1200,
      ts: Math.floor(Date.now() / 1000),
      uptime_s: 3600,
    };
  }

  if (head === "limit") {
    if (sub === "set") {
      demo.soc = Number(parts[2]);
      demo.limitEnabled = true;
      return { enabled: true, soc: String(demo.soc) };
    }
    if (sub === "on") {
      demo.limitEnabled = true;
      return { enabled: true };
    }
    if (sub === "off") {
      demo.limitEnabled = false;
      return { enabled: false };
    }
    return mockLimit();
  }

  // ── PPS 解锁（演示）──
  if (head === "addon") {
    // `addon show pps` / `addon on pps` / `addon off pps`
    if (parts[2] === "pps" || sub === "pps") {
      if (sub === "on") demo.ppsEnabled = true;
      if (sub === "off") demo.ppsEnabled = false;
    }
    if (sub === "show") return mockPps();
    return { count: 2, addons: [] };
  }

  // `set pps.mode auto` / `set pps.forge_broadcast false`
  if (head === "set" && sub === "pps.mode") {
    demo.ppsMode = parts[2] ?? "lock";
    return { key: "pps.mode", value: demo.ppsMode };
  }
  if (head === "set" && sub === "pps.forge_broadcast") {
    return { key: "pps.forge_broadcast", value: parts[2] };
  }

  // `get --section pps`
  if (head === "get" && sub === "--section" && parts[2] === "pps") {
    return {
      enabled: demo.ppsEnabled,
      mode: demo.ppsMode,
      forge_broadcast: true,
      power_display_w: 100,
      min_vbus_uv: 12_000_000,
      verify_s: 30,
    };
  }

  if (head === "get") {
    const section = parts[2];
    if (section === "curve") {
      return [
        { curr: 18_400_000, t_max: 38 },
        { curr: 12_000_000, t_max: 40 },
        { curr: 9_000_000, t_max: 41 },
        { curr: 7_000_000, t_max: 42 },
        { curr: 5_000_000, t_max: 43 },
        { curr: 2_500_000, t_max: 44 },
        { curr: 700_000, t_max: 45 },
      ];
    }
    if (section === "control") {
      return {
        actuator: "dual",
        closed_loop: true,
        feedforward: 0,
        hardware_protect: true,
        patrol_s: 3,
        shift_s: 6,
        stabilize_s: 15,
        target_power_w: 43,
      };
    }
    return {
      // 与出厂默认一致（`ramp`）—— 演示数据不能展示一个已经不用的默认值
      abs_max: 45,
      control_law: "ramp",
      delay_s: 5,
      ramp_span: 2,
      relax_temp: 42,
      sensor: "cell",
      // 与出厂默认一致（3 W）—— 演示数据不该展示一个已经不用了的旧值
      tighten_power_w: 3,
      tighten_temp: 43,
      virt_guard: false,
      virt_hard: 46,
    };
  }

  if (head === "device") {
    // `device probe --json`（本地命令通道）
    if (sub === "probe") {
      return mockProbe(parts.includes("--save"));
    }
    return mockDevice();
  }

  // `measure --json`（本地命令通道）
  if (head === "measure") {
    return mockMeasure();
  }

  if (head === "logs") {
    return { lines: [] };
  }

  throw new ApiError("E_TYPE", `演示模式未实现的命令：${parts.join(" ")}`);
}

function mockLimit(): AddonStatus {
  return {
    addon_api_version: 1,
    consecutive_failures: 0,
    detail: {
      keep_on_restore: true,
      last_written: "0x550011",
      node_ctrl: "/sys/class/xm_power/charger/smart_charge/smart_chg",
      node_read: "/sys/class/xm_power/charger/smart_charge/soc_limit",
      soc_active: demo.limitEnabled ? demo.soc : null,
      soc_target: demo.soc,
      verify_s: 15,
      warn_on_conflict: true,
      conflict_suspected: false,
      write_count: 3,
      // 与后端 `NOTE_UI_NOT_SYNCED` 同款文案（演示模式要能看出这块提示的样子）
      note_ui_not_synced:
        "系统设置里显示的百分比不会跟着变（那是系统自己的文案）。以本页的「已生效」为准。",
    },
    enabled: demo.limitEnabled,
    errors: 0,
    id: "soc_limit",
    interval_s: 60,
    label: "充电上限",
    last_error: null,
    origin: "节点机制由真机实测确证：写 smart_chg 控制字、读 soc_limit 生效值",
    probe: "命中 smart_chg",
    // 与后端 `REQUIRES_BATTERY_PROTECT` 同款文案
    requires:
      "需先在系统设置中打开「电池健康保护」（设置 → 省电与电池 → 电池保护）",
    summary: "充到指定电量即停止，保护电池寿命",
    supported: true,
    ticks: 5,
    tripped: false,
    writes: 3,
  };
}

/**
 * 演示：PPS 解锁状态
 *
 * ⚠️ 刻意演示**门控通过**的场景（公版 PPS 头、20V、100W），
 *    这样浏览器里能看到"可广播"的样子；
 *    真机上拔电时会显示"当前协议不是 PPS"，两种都值得看。
 */
function mockPps(): AddonStatus {
  return {
    addon_api_version: 1,
    consecutive_failures: 0,
    detail: {
      platform: "qcom",
      node: "/sys/class/qcom-battery/pd_verifed",
      node_value: demo.ppsEnabled ? "1" : "0",
      mode: demo.ppsMode === "auto" ? "自动" : "锁定",
      session: demo.ppsEnabled ? "充电中（已解锁）" : "充电中（未解锁）",
      engaged: demo.ppsEnabled,
      real_type: "PD_PPS",
      adapter_svid: "0000",
      voltage_uv: 20_000_000,
      apdo_max: 100,
      broadcast_ready: demo.ppsEnabled,
      broadcast_reason: demo.ppsEnabled
        ? "可广播（100W）"
        : "PD 认证位是 0（未解锁），先启用本功能",
      last_burst_at: demo.ppsEnabled ? Math.floor(Date.now() / 1000) - 30 : null,
      write_count: demo.ppsEnabled ? 2 : 0,
    },
    enabled: demo.ppsEnabled,
    errors: 0,
    id: "pps",
    interval_s: 30,
    label: "PPS 解锁",
    last_error: null,
    origin:
      "节点机制由真机实测确证：写 qcom-battery/pd_verifed，读 real_type 与 adapter_svid 判断协议",
    probe: "命中 /sys/class/qcom-battery/pd_verifed（平台 qcom）",
    // 与后端 `WARN_MIPPS` 同款文案
    requires:
      "锁定 PPS 可能让小米原装头失去私有快充协议（MIPPS），导致功率反而下降。用原装头时若发现变慢，请关闭本功能或改用「自动」模式",
    summary: "绕过 PD 认证，让第三方充电头也能触发快充；可选伪造金标动画",
    supported: true,
    ticks: 5,
    tripped: false,
    writes: demo.ppsEnabled ? 2 : 0,
  };
}

function mockDevice(): DeviceProfileView {
  return {
    battery: {      cycle_count: 205,
      health_percent: 96.6,
      model_name: "P11U_7560mah_100w",
      technology: "Li-poly",
    },
    capabilities: {
      current_max_ua: 18_400_000,
      gear_max: 15,
      gear_min: 0,
      gear_open: 0,
      gear_safe: 15,
      gear_table_rows: 15,
      hardware_max_c: 48,
      power_max_gear: 0,
      // 与真机一致：功率上限来自三来源取最大
      power_max_w: 100,
      protocol: null,
      soc_limit_supported: true,
      // 与真机一致：检测出的支持区间
      soc_limit_min: 60,
      soc_limit_max: 100,
      sources: {
        gear_max: "devicetree",
        gear_table: "devicetree",
        current_max: "node",
        power_max: "battery_model",
        hardware_max_temp: "devicetree",
      },
    },
    identity: {
      android_api: 37,
      android_release: "17",
      device: "myron",
      marketname: "REDMI K90 Pro Max",
      model: "25102RKBEC",
      page_size: 4096,
      platform: "canoe",
    },
    probe_meta: {
      failed_items: [],
      method: "quick_readonly",
      // 新版检测已全只读，不再需要充电
      needs_charge_for: [],
      // 演示值：取**真机实测的那 7 条**（2026-10-17 从 K90 Pro Max 上 dump）
      //
      // ⚠️ 刻意包含「已跳过」那条 —— 它对应橙色徽标，
      //    是三种语气里唯一"不是成功也不是失败"的一类。
      //    只放成功项的话，浏览器预览里根本看不出状态徽标有没有生效。
      notes: [
        "（演示模式：以下为示例数据）",
        "SOC 上限：节点可用",
        "档位表：15 档（0~14）· 设备树直读",
        "最大功率档位：0",
        "电流上限：已跳过（充电中测得不准）—— 拔掉充电器后重跑",
        "硬件保护温度：48℃（设备树直读）",
        "充电功率上限：100W · 来源电池型号",
        "档位上限：未做写探（快速检测零副作用）",
      ],
      probed_at: Math.floor(Date.now() / 1000) - 300,
    },
    source: "演示模式",
    unverified: true,
    verified: false,
  };
}

/** 演示模式下的检测结果 */
function mockProbe(saved: boolean): ProbeResult {
  const profile = mockDevice();
  return {
    saved,
    elapsed_ms: 28,
    mode: "quick",
    profile,
  };
}

/**
 * 演示模式下的功率测量
 *
 * ⚠️ 刻意演示**弱电源**场景（电脑 USB 口）：这样在浏览器里就能看到
 * "实测 2.4W 但标称 100W" 这种对比，正好说明两者语义不同。
 */
function mockMeasure(): PowerMeasurement {
  return {
    charging: true,
    status: "Charging",
    soc: 89,
    samples: 15,
    peak_w: 4.8,
    avg_w: 3.9,
    duration_ms: 3000,
    source: {
      usb_type: "USB",
      protocol: "SDP",
      weak: true,
    },
    nominal_w: 100,
  };
}
