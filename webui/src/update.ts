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
      url: typeof data.html_url === "string" ? data.html_url : RELEASES_URL,
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
