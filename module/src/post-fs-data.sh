#!/system/bin/sh
# ============================================================
#  post-fs-data.sh —— 早期启动阶段
#
#  ⚠️ 约束（见 docs/01-项目方案.md §4.3）：
#     · 本阶段【阻塞启动】，且有时限 —— 只做必要清理
#     · 【禁止 setprop】（会阻塞/死锁启动），如需设置属性用 resetprop -n
#     · 【不写任何充电节点】—— 该阶段环境未就绪，写入行为不可预期
# ============================================================

MODDIR=${0%/*}
DATADIR=/data/adb/hyperpower

# 仅确保数据目录存在且权限正确
mkdir -p "$DATADIR/logs" "$DATADIR/ipc" 2>/dev/null
chmod 700 "$DATADIR" 2>/dev/null

# 清理上次异常退出可能残留的 socket（避免 worker 启动时误判）
rm -f "$DATADIR/ipc/control.sock" 2>/dev/null

exit 0
