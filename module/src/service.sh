#!/system/bin/sh
# ============================================================
#  service.sh —— late_start 服务（非阻塞）
#
#  职责：拉起 supervisor，然后立即返回。
#
#  ⚠️ 本脚本必须极快返回，绝不能阻塞启动。
#     所有等待与重试都在 supervisor/worker 内部完成。
# ============================================================

MODDIR=${0%/*}
DATADIR=/data/adb/hyperpower
LOGDIR="$DATADIR/logs"

mkdir -p "$LOGDIR" 2>/dev/null

SLOG="$LOGDIR/service.log"

# 日志轮转（512 KB）
if [ -f "$SLOG" ]; then
  _sz=$(wc -c < "$SLOG" 2>/dev/null | tr -d ' \t')
  case "$_sz" in
    ''|*[!0-9]*) ;;
    *)
      if [ "$_sz" -gt 524288 ]; then
        tail -c 262144 "$SLOG" > "$SLOG.t.$$" 2>/dev/null && mv -f "$SLOG.t.$$" "$SLOG" 2>/dev/null
      fi
      ;;
  esac
fi

slog() {
  echo "[$(date '+%Y-%m-%d %H:%M:%S')] $*" >> "$SLOG" 2>/dev/null
}

slog "===== service.sh 启动 ====="

# 权限自愈：模块目录可能因重启丢失可执行位
chmod 755 "$MODDIR/bin/thermo" 2>/dev/null
chmod 755 "$MODDIR/bin/ctl"    2>/dev/null
chmod 755 "$MODDIR/bin/daemon" 2>/dev/null

# 清理上次运行残留
#
# ⚠️ 只清 pidfile 与 socket，**不清 worker.lock**：
#    那是 `flock` 的目标文件，由内核在进程退出时自动释放锁；
#    删掉文件反而可能让新进程锁到一个"已无人持有的新 inode"，
#    在极端时序下破坏单实例保证。
rm -f "$DATADIR/worker.pid" 2>/dev/null
rm -f "$DATADIR/supervisor.pid" 2>/dev/null
rm -f "$DATADIR/ipc/control.sock" 2>/dev/null

BIN="$MODDIR/bin/thermo"
if [ ! -x "$BIN" ]; then
  slog "错误：thermo 二进制不可执行"
  exit 0
fi

# 后台拉起 supervisor（脱离当前会话，避免 service.sh 退出时被带走）
slog "启动 supervisor"
if command -v setsid >/dev/null 2>&1; then
  setsid "$BIN" supervisor >> "$LOGDIR/daemon.out" 2>&1 &
else
  nohup "$BIN" supervisor >> "$LOGDIR/daemon.out" 2>&1 &
fi

slog "===== service.sh 结束 ====="
exit 0
