/**
 * 更新检查 —— 对比本机版本与 GitHub 最新 Release
 *
 * # 🔴 这是本项目**唯一**会联网的代码路径（R16 的最小例外）
 *
 * R16 的原则是「运行时下载/联网 → 禁止，必须离线可用」。本模块的
 * **控制功能**完全离线：不联网也能采样、决策、写节点。
 *
 * 这里的联网被刻意压到最小：
 *
 * | 约束 | 做法 |
 * |---|---|
 * | **只在用户点击时发起** | 不做页面加载即检查、不做后台轮询 |
 * | **只读** | `GET releases/latest`，不下载二进制、不上传任何信息 |
 * | **不带凭据** | `credentials: "omit"`，匿名请求（GitHub 限 60 次/小时） |
 * | **失败即降级** | 超时/断网/CORS 都只影响这张卡，不影响其他功能 |
 *
 * 若将来要"打开页面就提示"，必须先改 R16 的口径并写进 AGENTS ——
 * 否则后来者会把它当 bug 删掉（或反过来，被当成"模块偷偷联网"）。
 *
 * # 为什么用 `releases/latest` 而不是 `tags`
 *
 * `latest` 会跳过 draft / prerelease，只有**正式发布**才会提示；
 * 用 `tags` 会把调试用的 tag 也算成更新。
 */

/** GitHub 仓库（与 README / 发布脚本一致） */
export const REPO = "kino516/HyperPower";

/** 发布页（引导用户自己去看 release notes / 下载） */
export const RELEASES_URL = `https://github.com/${REPO}/releases`;

/**
 * **备用下载通道**（网盘）—— 给 GitHub 连不上的用户
 *
 * # 为什么需要
 *
 * 2026-11-21 用户反馈：部分网络环境下 GitHub 完全打不开，连"打开发布页"
 * 都看不到 Release，而模块本身是**离线**工作的 —— 装不上的原因不该是
 * "看不到下载地址"。
 *
 * # 约束（与 R16 一致）
 *
 * | 项 | 说明 |
 * |---|---|
 * | 不是联网请求 | 只把固定 URL 交给系统浏览器，前端**不发起任何 fetch** |
 * | 常量而非用户输入 | 防止把任意 URL 拼进 root 命令（见 `api.ts::openExternalPage`） |
 * | 失败降级 | 打不开只影响这张卡 |
 *
 * ⚠️ 该地址由模块作者维护；更换时**只改这一处**。
 */
export const MIRROR_URL =
  "https://1815482927.share.123pan.cn/123pan/udOiVv-3kOAh?pwd=icyA#";

/** 需要交给系统浏览器打开的固定页面（键名是界面用的枚举，值只来自本文件常量） */
export const EXTERNAL_PAGES = {
  releases: RELEASES_URL,
  mirror: MIRROR_URL,
} as const;

/** [`EXTERNAL_PAGES`] 的键 */
export type ExternalPage = keyof typeof EXTERNAL_PAGES;

const LATEST_RELEASE_API = `https://api.github.com/repos/${REPO}/releases/latest`;

/** 一次检查的结果（`null` = 仓库还没有正式 Release） */
export interface LatestRelease {
  /** 原始 tag，如 `v1.1.3` */
  tag: string;
  /** 去掉前缀的版本号，如 `1.1.3` */
  version: string;
  /** 发布页地址（后端字段缺失时退回 [`RELEASES_URL`]） */
  url: string;
  /** 发布时间（ISO8601；可能为空） */
  publishedAt: string;
}

/** 把 `v1.1.3` / `V1.1.3` / `1.1.3` 统一成 `1.1.3`；预发布后缀（`-beta.1` / `+build`）丢弃 */
export function normalizeVersion(raw: string): string {
  const [head = ""] = raw
    .trim()
    .replace(/^[vV]/, "")
    .split(/[-+]/);
  return head.trim();
}

/** 把版本号拆成数字段（非数字段按 0 处理，不抛异常） */
function versionParts(raw: string): number[] {
  return normalizeVersion(raw)
    .split(".")
    .map((s) => {
      const n = Number.parseInt(s, 10);
      return Number.isFinite(n) ? n : 0;
    });
}

/**
 * 比较两个版本号：`a > b` 返回 1，相等 0，`a < b` 返回 -1
 *
 * # 为什么自己写而不用 semver 库
 *
 * 模块版本是**固定三段数字**（`1.1.3`），没有 prerelease/build 语义；
 * 为这一个函数引一个依赖，体积与供应链风险都不划算。
 *
 * ⚠️ 已知取舍：预发布后缀（`1.2.0-beta.1`）会被截断成 `1.2.0`。
 *    对本项目无影响 —— `releases/latest` 本身就不会返回 prerelease。
 */
export function compareVersions(a: string, b: string): number {
  const pa = versionParts(a);
  const pb = versionParts(b);
  const n = Math.max(pa.length, pb.length);
  for (let i = 0; i < n; i += 1) {
    const x = pa[i] ?? 0;
    const y = pb[i] ?? 0;
    if (x !== y) return x > y ? 1 : -1;
  }
  return 0;
}

/**
 * 读取 GitHub 最新的正式 Release
 *
 * 返回 `null`：仓库还没有 Release（新仓库/刚建）。
 * 抛异常：断网、请求超时、限流（HTTP 403）、响应不像 JSON。
 *
 * ⚠️ 超时**必须**有：KernelSU 的 WebView 在无网络时有时不会立刻失败，
 *    而是挂到系统超时（几十秒）—— 用户会以为按钮坏了。
 */
export async function fetchLatestRelease(timeoutMs = 8_000): Promise<LatestRelease | null> {
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(), timeoutMs);
  try {
    const res = await fetch(LATEST_RELEASE_API, {
      signal: ctrl.signal,
      // 不携带 cookie / 凭据：这是公共仓库的只读查询
      credentials: "omit",
      cache: "no-store",
      headers: { Accept: "application/vnd.github+json" },
    });

    if (res.status === 404) {
      // 没有任何 Release —— 不是错误，是"暂无"
      return null;
    }
    if (!res.ok) {
      throw new Error(
        res.status === 403
          ? "被 GitHub 限流（匿名查询每小时 60 次），请稍后再试"
          : `GitHub 返回 HTTP ${res.status}`,
      );
    }

    const data = (await res.json()) as {
      tag_name?: unknown;
      html_url?: unknown;
      published_at?: unknown;
    };
    const tag = typeof data.tag_name === "string" ? data.tag_name : "";
    if (!tag) return null;

    return {
      tag,
      version: normalizeVersion(tag),
      // 🔴 **刻意不采信 API 返回的 `html_url`**（2026-11-21）
      //
      // 打开外部页面走的是 `ksu.exec("am start … -d \"<url>\"")`，即在
      // **root shell** 里拼字符串。任何来自网络响应的字符串都不该进那条命令 ——
      // 一旦 GitHub 响应被污染（或将来换了字段），就是一个提权面。
      //
      // 固定成仓库的 releases 页：用户点进去同样能看到版本列表与下载，
      // 代价只是不能"直达某个 tag"，换来的是 URL 只来自本文件常量。
      url: RELEASES_URL,
      publishedAt: typeof data.published_at === "string" ? data.published_at : "",
    };
  } catch (e) {
    if (e instanceof DOMException && e.name === "AbortError") {
      throw new Error(`连接 GitHub 超时（${timeoutMs / 1000} 秒）`);
    }
    // fetch 在断网时抛 TypeError，文案系统各异；统一成一句能看懂的话
    if (e instanceof TypeError) {
      throw new Error("连不上 GitHub —— 请检查网络（或系统限制了 WebView 联网）");
    }
    throw e instanceof Error ? e : new Error(String(e));
  } finally {
    clearTimeout(timer);
  }
}
