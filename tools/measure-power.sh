#!/system/bin/sh
# ============================================================
#  measure-power.sh —— 测量 worker 的真实开销
#
#  # 为什么需要它
#
#  讨论"要不要拆成多个进程省电"时，不能靠感觉 —— 必须知道
#  单进程方案在每个场景下**实际**占多少 CPU、多少内存、醒多少次。
#
#  # 测什么
#
#  | 指标 | 来源 | 含义 |
#  |---|---|---|
#  | CPU 时间增长 | `/proc/<pid>/stat` 的 utime+stime | 直接耗电代理 |
#  | 内存 | `/proc/<pid>/status` 的 VmRSS | 常驻进程的真实代价 |
#  | 上下文切换 | `voluntary/nonvoluntary_ctxt_switches` | **唤醒次数**（关键） |
#  | 进程状态 | `/proc/<pid>/stat` 第 3 字段 | S=阻塞（好）R=跑（耗电）D=卡死 |
#
#  # 用法
#
#  ```bash
#  adb push tools/measure-power.sh /data/local/tmp/
#  adb shell su -c 'sh /data/local/tmp/measure-power.sh 30 8'
#  #                                              间隔秒 次数
#  ```
# ============================================================

INTERVAL=${1:-30}
COUNT=${2:-8}

PIDFILE=/data/adb/hyperpower/worker.pid
PID=$(cat "$PIDFILE" 2>/dev/null)

if [ -z "$PID" ] || [ ! -d "/proc/$PID" ]; then
  echo "✗ 找不到 worker 进程（pidfile=$PIDFILE）"
  exit 1
fi

echo "════════ 测量对象 ════════"
echo "  worker pid = $PID"
echo "  采样间隔   = ${INTERVAL}s × ${COUNT} 次"
echo ""

# ── 读一次快照 ──
#
# ⚠️ `/proc/<pid>/stat` 的第 2 个字段 `comm` **可能含空格与括号**，
#    必须从**最后一个 `)` 之后**开始解析（用 `##*) ` 取最长匹配）。
#
# ⚠️ 解析**不要用 awk 的 split + `set --`**：两者在"前导空格
#    算不算一个字段"上行为不一致，实测踩到字段错位
#    （状态位读成了 PPID）。用纯 shell 展开 + `cut` 最稳。
read_snapshot() {
  S_STAT=$(cat "/proc/$PID/stat" 2>/dev/null) || return 1
  # 去掉到最后一个 ") " 为止的全部内容 ⇒ 剩下从"状态"字段开始
  S_REST=${S_STAT##*) }
  SNAP_STATE=$(printf '%s' "$S_REST" | cut -d' ' -f1)
  SNAP_UTIME=$(printf '%s' "$S_REST" | cut -d' ' -f12)  # 原第 14 字段
  SNAP_STIME=$(printf '%s' "$S_REST" | cut -d' ' -f13)  # 原第 15 字段

  SNAP_RSS=$(grep '^VmRSS:' "/proc/$PID/status" 2>/dev/null | awk '{print $2}')
  # ⚠️ 必须用 `^` 锚定：`grep voluntary_ctxt_switches` 会**同时**
  #    匹配到 `nonvoluntary_ctxt_switches`（子串）⇒ 拿到两行 ⇒ 算术炸掉
  SNAP_VCS=$(grep '^voluntary_ctxt_switches:' "/proc/$PID/status" 2>/dev/null | awk '{print $2}')
  SNAP_NVCS=$(grep '^nonvoluntary_ctxt_switches:' "/proc/$PID/status" 2>/dev/null | awk '{print $2}')

  # 数值兜底：拿不到就置 0，避免下面算术炸掉
  [ -n "$SNAP_UTIME" ] || SNAP_UTIME=0
  [ -n "$SNAP_STIME" ] || SNAP_STIME=0
  [ -n "$SNAP_VCS" ] || SNAP_VCS=0
  [ -n "$SNAP_NVCS" ] || SNAP_NVCS=0
  return 0
}

battery_status() {
  cat /sys/class/power_supply/battery/status 2>/dev/null
}

echo "════════ 开始采样 ════════"
printf '  %5s  %-5s %-12s %-8s %-9s %s\n' "时刻" "状态" "电池" "RSS(KB)" "CPU占用" "ctx/分"
printf '  %5s  %-5s %-12s %-8s %-9s %s\n' "-----" "----" "------------" "-------" "--------" "------"

read_snapshot || { echo "✗ 无法读取 /proc/$PID/stat"; exit 1; }
P_U=$SNAP_UTIME
P_S=$SNAP_STIME
P_V=$SNAP_VCS
P_N=$SNAP_NVCS

i=0
while [ "$i" -lt "$COUNT" ]; do
  sleep "$INTERVAL"
  i=$((i + 1))
  read_snapshot || break

  # jiffies → 秒：USER_HZ 固定 100 ⇒ (delta)/100/间隔 = 平均核数占用
  CPU=$(awk "BEGIN{printf \"%.4f\", (($SNAP_UTIME+$SNAP_STIME)-($P_U+$P_S))/100/$INTERVAL}")
  CTX=$(awk "BEGIN{printf \"%.0f\", ((($SNAP_VCS+$SNAP_NVCS)-($P_V+$P_N))/$INTERVAL)*60}")

  printf '  %4ds  %-5s %-12s %-8s %-9s %s\n' \
    "$((i * INTERVAL))" "$SNAP_STATE" "$(battery_status)" "$SNAP_RSS" "$CPU" "$CTX"

  P_U=$SNAP_UTIME
  P_S=$SNAP_STIME
  P_V=$SNAP_VCS
  P_N=$SNAP_NVCS
done

echo ""
echo "════════ 读数说明 ════════"
echo "  CPU占用  = 该时段平均 CPU 占用（0.0000 = 完全没跑）"
echo "  ctx/分   = 每分钟上下文切换次数（反映"醒了几次"）"
echo "  状态     = S 阻塞（理想）/ R 运行（在耗电）/ D 卡死"
echo ""
echo "  参考：单核占用 0.1% ≈ 每小时 3.6 秒 CPU 时间"
