#!/system/bin/sh
# ============================================================
#  jsoncheck.sh —— 验证 WebUI 依赖的每条命令，stdout **只有一行 JSON**
#
#  # 为什么需要它
#
#  WebUI 直接对 stdout 做 `JSON.parse`。只要有任何一行提示文字混进
#  stdout，界面就会报「响应不是合法 json」——而这个错误**在终端里
#  看不出来**（人眼会忽略前几行，只看到 JSON）。
#
#  真机踩过：`notify_worker_reload()` 里的 `println!("已通知 worker 重载新档案")`
#  排在 JSON 之前，导致完整检测在界面上永远失败。
#
#  # 为什么分两大类
#
#  | 类别 | 走什么 | 由谁调用 |
#  |---|---|---|
#  | **本地命令** | 直接起 `thermo` 进程 | `device probe` / `measure` / `version` |
#  | **控制通道** | 经 UDS 问 worker | 其余全部（`api.ts` 里绝大多数） |
#
#  ⚠️ 只测本地命令是不够的：WebUI 的主路径是 `ctl *`。
#     新增命令（`mode` / `history`）曾经完全没被覆盖。
#
#  # 用法
#
#  ```bash
#  adb push tools/jsoncheck.sh /data/local/tmp/
#  adb shell su -c 'sh /data/local/tmp/jsoncheck.sh'
#  ```
#
#  退出码 = 失败项数（0 为全通过）。
# ============================================================

# ⚠️ 模块目录名 = module.prop 的 id（KernelSU 用 id 作目录名）
#    改 id 时需同步此处的路径（见 tools/full-deploy.sh 的说明）
BIN=/data/adb/modules/a_hyperpower/bin
CTL="$BIN/ctl"
THERMO="$BIN/thermo"
fail=0

# 检查一条命令的 stdout 是否**恰好一行**且以 { 或 [ 开头
check() {
  local name="$1"
  shift

  # 只取 stdout；stderr 丢弃（人类可读信息应全在那边）
  out=$("$@" 2>/dev/null)
  n=$(printf '%s\n' "$out" | wc -l | tr -d ' ')
  first=$(printf '%s' "$out" | head -1 | cut -c1)

  if [ -z "$out" ]; then
    printf '  ✗ %-30s stdout 为空\n' "$name"
    fail=$((fail + 1))
  elif [ "$n" -ne 1 ]; then
    printf '  ✗ %-30s stdout %s 行（应为 1）\n' "$name" "$n"
    printf '      前两行：\n'
    printf '%s\n' "$out" | head -2 | cut -c1-70 | sed 's/^/        | /'
    fail=$((fail + 1))
  elif [ "$first" != "{" ] && [ "$first" != "[" ]; then
    printf '  ✗ %-30s 首字符是 "%s"（应为 { 或 [）\n' "$name" "$first"
    fail=$((fail + 1))
  else
    printf '  ✓ %-30s 1 行，首字符 %s\n' "$name" "$first"
  fi
}

echo "════════ 本地命令（WebUI 用 runBinary 直调）════════"
echo ""
# ⚠️ 本地命令只有这两个带 --json（WebUI 用 `runBinary()` 调它们）。
#
#    别再往这里加 `thermo device show --json` —— **该子命令不支持 --json**
#    （它输出人类可读文本，见 cli.rs）。校验档案请用下面的 `ctl device show`。
check "version"                "$THERMO" version
check "device probe --json"    "$THERMO" device probe --json --quick
check "measure --json"         "$THERMO" measure --json --duration-ms 500

echo ""
echo "════════ 控制通道（WebUI 主路径：ctl *）════════"
echo ""
# ⚠️ 只测**只读**命令 —— 写命令（set / limit set / history clear）
#    有副作用，不放进冒烟脚本。
check "info"                   "$CTL" info
check "status"                 "$CTL" status
check "get --all"              "$CTL" get --all
check "get --section thermal"  "$CTL" get --section thermal
check "get --section control"  "$CTL" get --section control
check "get --section curve"    "$CTL" get --section curve
check "get --section history"  "$CTL" get --section history
check "get --section device"   "$CTL" get --section device
check "get --section log"      "$CTL" get --section log
check "get --section daemon"   "$CTL" get --section daemon
check "get --section ui"       "$CTL" get --section ui
check "get --section limit"    "$CTL" get --section limit
check "logs events"            "$CTL" logs events --lines 5
check "logs list"              "$CTL" logs list
check "device show"            "$CTL" device show
check "device probe"           "$CTL" device probe
check "limit show"             "$CTL" limit show
check "limit probe"            "$CTL" limit probe
check "addon list"             "$CTL" addon list
check "addon show soc_limit"   "$CTL" addon show soc_limit
check "preset list"            "$CTL" preset list
# ── 本轮新增 ──
check "mode list"              "$CTL" mode list
check "mode show custom"       "$CTL" mode show custom
check "history list"           "$CTL" history list
check "history show"           "$CTL" history show
check "history stats"          "$CTL" history stats
check "probe"                  "$CTL" probe
check "help"                   "$CTL" help

echo ""
echo "════════ 结论 ════════"
if [ "$fail" -eq 0 ]; then
  echo "  ✓ 全部通过 —— stdout 均只有一行 JSON"
else
  echo "  ✗ $fail 项有问题 —— WebUI 会对这些命令报「响应不是合法 json」"
fi

exit "$fail"
