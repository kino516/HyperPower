#!/system/bin/sh
# ============================================================
#  post-fs-data.sh —— 早期启动阶段
#
#  ⚠️ 约束：本阶段阻塞启动且有时限 —— 只做必要清理
#     · 禁止 setprop（会阻塞/死锁启动）
#     · 不写任何充电节点
#
#  修复：软重启时旧 worker 可能仍在运行，此时绝不能删它的 socket。
# ============================================================

MODDIR=${0%/*}
DATADIR=/data/adb/hyperpower
BIN="$MODDIR/bin/hyperpower"

# 仅确保数据目录存在且权限正确
mkdir -p "$DATADIR/logs" "$DATADIR/ipc" 2>/dev/null
chmod 700 "$DATADIR" 2>/dev/null

# 旧 worker 还活着（软重启）—— socket 仍在使用，保留
if [ -x "$BIN" ] && pgrep -f "^$BIN worker" >/dev/null 2>&1; then
  exit 0
fi

# 真开机 / 无残留时才清理上次异常退出留下的 socket
rm -f "$DATADIR/ipc/control.sock" 2>/dev/null

exit 0
