#!/system/bin/sh
# ============================================================
#  service.sh —— late_start 服务（非阻塞）
#
#  职责：拉起 supervisor，然后立即返回。
#
#  ⚠️ 本脚本必须极快返回，绝不能阻塞启动。
#     所有等待与重试都在 supervisor/worker 内部完成。
#
#  ── 软重启修复 ───────────────────────────────────────────
#  旧版无条件删除 ipc/control.sock 并再起一个 supervisor。
#  但“软重启”只重启 Android 框架，不会杀掉 setsid 拉起的旧
#  supervisor/worker：
#    · 旧 worker 仍占着 worker.lock，但它的 socket 文件已被删；
#    · 旧 supervisor 仍占着 supervisor.lock，新 supervisor 直接退出。
#  ⇒ 所有 ctl 连接永久失败（“连接 worker 失败”）。
#  现在：存活且 socket 正常就跳过；有残留就先停干净再启动。
# ============================================================

MODDIR=${0%/*}
DATADIR=/data/adb/hyperpower
LOGDIR="$DATADIR/logs"
BIN="$MODDIR/bin/hyperpower"

mkdir -p "$LOGDIR" "$DATADIR/ipc" 2>/dev/null

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
chmod 755 "$BIN" "$MODDIR/bin/ctl" "$MODDIR/bin/daemon" 2>/dev/null

if [ ! -x "$BIN" ]; then
  slog "错误：hyperpower 二进制不可执行"
  exit 0
fi

# 按“命令行以二进制路径开头”匹配，避免误杀只是提到该路径的 shell
_worker_pids() { pgrep -f "^$BIN worker" 2>/dev/null; }
_sup_pids()    { pgrep -f "^$BIN supervisor" 2>/dev/null; }

_w=$(_worker_pids | head -n 1)
_s=$(_sup_pids    | head -n 1)

# 1) 完整健康：旧进程还能正常服务 ⇒ 保持原样，不重启
if [ -n "$_w" ] && [ -n "$_s" ] && [ -S "$DATADIR/ipc/control.sock" ]; then
  # socket 文件存在不等于有人监听（进程卡死时文件会残留），
  # 用一次带超时的真实 ctl 探测确认。ctl 若因连接失败尝试自愈，
  # 新进程也会被 supervisor.lock / worker.lock 挡住，不影响旧进程。
  _alive=1
  if command -v timeout >/dev/null 2>&1; then
    _out=$(timeout 1 "$BIN" ctl status 2>/dev/null)
    case "$_out" in
      *'"battery"'*|*'"daemon"'*) _alive=1 ;;
      *) _alive=0 ;;
    esac
  fi
  if [ "$_alive" = "1" ]; then
    slog "已有存活的 supervisor($_s)/worker($_w) 且 ctl 可响应，跳过启动"
    exit 0
  fi
  slog "socket 存在但 ctl 无响应，按残留进程处理"
fi

# 2) 有残留但不完整（软重启最常见）⇒ 先停干净，避免锁被旧进程占着
if [ -n "$_w" ] || [ -n "$_s" ]; then
  slog "发现残留进程（supervisor=${_s:-无} worker=${_w:-无}），先停止再重启"

  # 先 TERM worker，再 TERM supervisor：worker 收到后会先交回官方控制再退出
  for _p in $_w $_s; do kill -TERM "$_p" 2>/dev/null; done
  sleep 1

  # supervisor 可能在这 1 秒内又拉起新 worker，按路径全部 KILL
  for _p in $(_sup_pids) $(_worker_pids); do kill -KILL "$_p" 2>/dev/null; done

  # 等内核释放 flock（最多 3 秒）
  _i=0
  while [ "$_i" -lt 3 ]; do
    [ -z "$(_sup_pids)" ] && [ -z "$(_worker_pids)" ] && break
    sleep 1
    _i=$((_i + 1))
  done

  if [ -n "$(_sup_pids)" ] || [ -n "$(_worker_pids)" ]; then
    slog "错误：残留进程无法终止（可能处于 D 状态），本次不启动；请硬重启"
    exit 0
  fi
fi

# 3) 清理残留。⚠️ 不删 worker.lock / supervisor.lock：
#    那两个文件由 flock 管；进程已全部退出后锁自然释放，删文件没有好处。
rm -f "$DATADIR/worker.pid" 2>/dev/null
rm -f "$DATADIR/supervisor.pid" 2>/dev/null
rm -f "$DATADIR/ipc/control.sock" 2>/dev/null

# daemon.out 只增不减，panic 循环时会撑大 /data；在启动新进程前轮转一次
_dsz=$(wc -c < "$LOGDIR/daemon.out" 2>/dev/null | tr -d ' \t')
if [ -n "$_dsz" ] && [ "$_dsz" -gt 524288 ]; then
  tail -c 262144 "$LOGDIR/daemon.out" > "$LOGDIR/daemon.out.t.$$" 2>/dev/null \
    && mv -f "$LOGDIR/daemon.out.t.$$" "$LOGDIR/daemon.out" 2>/dev/null
fi

# 4) 后台拉起唯一的 supervisor
slog "启动 supervisor"
cd / 2>/dev/null || true
if command -v setsid >/dev/null 2>&1; then
  setsid "$BIN" supervisor >> "$LOGDIR/daemon.out" 2>&1 < /dev/null &
else
  nohup "$BIN" supervisor >> "$LOGDIR/daemon.out" 2>&1 < /dev/null &
fi

slog "===== service.sh 结束 ====="
exit 0
