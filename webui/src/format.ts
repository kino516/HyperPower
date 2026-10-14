/**
 * 格式化与映射工具
 *
 * 全部是**纯函数** —— 不碰 DOM、不碰网络，便于单独验证。
 */

// ============================================================
//  数字格式化
// ============================================================

/** 一位小数 */
export function f1(v: number | null | undefined): string {
  return Number.isFinite(v as number) ? (v as number).toFixed(1) : "--";
}

/** 整数（四舍五入） */
export function f0(v: number | null | undefined): string {
  return Number.isFinite(v as number) ? String(Math.round(v as number)) : "--";
}

/**
 * 微安 → 安
 *
 * # 为什么统一一位小数
 *
 * 这里曾有 `amps`（两位小数）与 `amps1`（一位小数）两个函数。实测结果是
 * **同一张卡片里两种精度并存**：状态页「详细读数」中「实际电流 10.5 A」
 * 紧挨着「已写电流 12.00 A」—— 看起来像两个不同的单位。
 *
 * 两位小数对**电流**没有意义：
 *
 * | 场景 | 说明 |
 * |---|---|
 * | 实测电流 | 本身在抖动（10.52 ↔ 10.48），第二位是噪声 |
 * | 写入设定值 | 粒度是 1 mA，但显示 `18.40 A` 与 `18.4 A` 信息量相同 |
 *
 * 而全项目其他电流（`ControlPage` 的「降到多少 0.7 A」、
 * `MorePage` 的「电流上限 18.4 A」）都是一位小数。
 *
 * ⇒ 只保留一位小数的版本。**要改精度就改这里一处**，
 *    不要再引入第二个同类函数 —— 那正是当初不一致的起因。
 */
export function amps1(ua: number | null | undefined): string {
  return Number.isFinite(ua as number)
    ? ((ua as number) / 1_000_000).toFixed(1)
    : "--";
}

// ============================================================
//  时间
// ============================================================

/** Unix 秒 → `HH:MM:SS` */
export function hhmmss(ts: number | null | undefined): string {
  if (!Number.isFinite(ts as number) || (ts as number) <= 0) {
    return "--:--:--";
  }
  const d = new Date((ts as number) * 1000);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

/** 秒数 → 人话时长 */
export function duration(sec: number | null | undefined): string {
  if (!Number.isFinite(sec as number) || (sec as number) < 0) {
    return "--";
  }
  const s = Math.floor(sec as number);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  if (h > 0) {
    return `${h} 小时 ${m} 分`;
  }
  if (m > 0) {
    return `${m} 分 ${s % 60} 秒`;
  }
  return `${s} 秒`;
}

// ============================================================
//  状态映射
// ============================================================

/** 机器状态 → 中文标签 */
export function stateLabel(state: string | null | undefined): string {
  switch (state) {
    case "normal":
      return "正常放行";
    case "tightening":
      return "正在限速";
    case "hardprotect":
      return "硬保护";
    case "idle":
      return "待机";
    default:
      return state ? state : "未知";
  }
}

/** 机器状态 → miuix 色调（决定徽标配色） */
export function stateTone(
  state: string | null | undefined,
): "normal" | "warn" | "error" | "muted" {
  switch (state) {
    case "normal":
      return "normal";
    case "tightening":
      return "warn";
    case "hardprotect":
      return "error";
    case "idle":
      return "muted";
    default:
      return "muted";
  }
}

/** 充电状态 → 中文 */
export function chargingLabel(status: string | null | undefined): string {
  switch (status) {
    case "Charging":
      return "充电中";
    case "Full":
      return "已充满";
    case "Discharging":
      return "未充电";
    case "Not charging":
      return "已插电未充";
    default:
      return status ? status : "未知";
  }
}

/** 是否正在充电（含已充满） */
export function isCharging(status: string | null | undefined): boolean {
  return status === "Charging" || status === "Full";
}

// ============================================================
//  曲线
// ============================================================

/**
 * 找出当前温度落在曲线的哪一段
 *
 * 与后端 `curve_target()` **同一规则**：取第一个满足 `temp ≤ t_max` 的分段。
 * 保持一致很重要 —— 否则界面高亮的段与实际生效的段不符。
 */
export function currentCurveIndex(
  curve: { t_max: number }[],
  tempC: number | null,
): number {
  if (tempC === null || curve.length === 0) {
    return -1;
  }
  return curve.findIndex((p) => tempC <= p.t_max);
}

// ============================================================
//  校验
// ============================================================

/**
 * 温控阈值组合是否合法
 *
 * 内核要求 `relax_temp ≤ tighten_temp - 1`，违反会让**每条决策都被拒绝**
 * （真机实测过：引擎"活着但完全不工作"）。因此提交前先在这里拦一道，
 * 省得往返报错。
 */
export function thermalPairValid(
  tighten: number,
  relax: number,
): { ok: true } | { ok: false; reason: string } {
  if (!Number.isFinite(tighten) || !Number.isFinite(relax)) {
    return { ok: false, reason: "数值无效" };
  }
  if (relax > tighten - 1) {
    return {
      ok: false,
      reason: `恢复放行温度必须比收紧温度低 1℃ 以上（当前 ${relax} ≥ ${tighten - 1}）`,
    };
  }
  return { ok: true };
}
