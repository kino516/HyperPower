#!/system/bin/sh
# ============================================================
#  uninstall.sh —— 卸载时执行
#
#  职责：
#    1. 停止 supervisor / worker
#    2. 把**两条控制轴**都交回官方
#    3. 清理运行残留
#
#  ────────────────────────────────────────────────────────────
#  🔴 为什么本脚本【自包含】，不调用 thermo 二进制
#
#     卸载时模块目录可能已被移除、二进制可能已损坏 ——
#     本脚本是"最后一道保障"，因此：
#       · 不复用任何 lib/ 脚本（模块目录可能已不可读）
#       · 不调用 thermo 二进制（同样可能不可用）
#       · 不依赖 device.json 一定存在（用户可能从未跑过检测）
#
#     代价：候选路径表需在本脚本内维护一份，
#     必须与 native/crates/thermo-cli/src/sysfs.rs 的候选表保持一致。
#
#  ────────────────────────────────────────────────────────────
#  🔴 两条控制轴（互不相干，都要恢复）
#
#     ┌ 温控轴  wired_ctrl_limit + wired_chg_curr   管"充多快"
#     └ 上限轴  smart_chg                           管"充到多少停"
#
#     温控轴是【两层并联、取最严】——
#     只把档位放开而电流仍留着旧值，依然会被限流，故必须同时放开。
#
#  ────────────────────────────────────────────────────────────
#  🔴 放开值【不写死机型数据】（各机型不同，不得硬编码）
#
#     · 档位 0 / remove 0  → "最松 / 不干预"的通用语义，与机型无关 ✅ 可写死
#     · 电流上限           → 各机型不同 ⇒ 从本机档案读取实测值；
#                            读不到则【不写】并明确提示（不猜数值）⚠️ 不可写死
#     · 充电上限 100%      → 驱动定义的"不限制"常量，与机型无关 ✅ 可写死
#
#  ────────────────────────────────────────────────────────────
#  🔴 为什么充电上限必须在这里解除
#
#     不解除的话，用户卸载模块后会被"卡"在某个电量（典型 80%），
#     而系统界面仍显示 80%，用户完全不知道为什么充不满。
#     写 100% = 驱动语义"不限制"，等价于"模块从未介入"。
# ============================================================

MODDIR=${0%/*}
DATADIR=/data/adb/thermo_charge

# 卸载日志的落点：
#   数据目录里的日志可能随之被清理，故同时在 /data/local/tmp 留一份，
#   便于卸载后排查（该目录用户与 adb 都能访问、可自行删除）。
SLOG="$DATADIR/logs/service.log"
USLOG=/data/local/tmp/thermo_charge_uninstall.log

# 同时输出到三处：stdout（管理器界面可见）、数据目录日志、临时日志
slog() {
  _m="[$(date '+%Y-%m-%d %H:%M:%S')] [uninstall] $*"
  echo "$_m" >> "$SLOG" 2>/dev/null
  echo "$_m" >> "$USLOG" 2>/dev/null
  echo "$_m"
}

slog "===== 卸载开始 ====="

# ============================================================
#  1. 停止进程
# ============================================================

if [ -f "$DATADIR/worker.pid" ]; then
  _pid=$(tr -d ' \r\n' < "$DATADIR/worker.pid" 2>/dev/null)
  if [ -n "$_pid" ] && [ -d "/proc/$_pid" ]; then
    kill "$_pid" 2>/dev/null
    sleep 1
    [ -d "/proc/$_pid" ] && kill -9 "$_pid" 2>/dev/null
    slog "已停止 worker（pid=$_pid）"
  fi
  rm -f "$DATADIR/worker.pid" 2>/dev/null
fi

# supervisor：按"二进制路径 + 子命令"匹配，避免误杀同名进程
_pids=$(pgrep -f "$MODDIR/bin/thermo supervisor" 2>/dev/null)
for _p in $_pids; do
  kill "$_p" 2>/dev/null
  slog "已停止 supervisor（pid=$_p）"
done

sleep 1

# ============================================================
#  2. 路径探测（候选表 —— 与 sysfs.rs 同源，顺序即优先级）
#
#  ⚠️ 路径因机型而异：硬编码单一路径会让部分机型恢复不到。
# ============================================================

CTRL_DIR_CANDIDATES="
/sys/class/xm_power/charger/charger_thermal
/sys/class/charger/charger_thermal
"

SOC_CTRL_CANDIDATES="
/sys/class/xm_power/charger/smart_charge/smart_chg
/sys/class/charger/smart_charge/smart_chg
"

CTRL_DIR=""
for _d in $CTRL_DIR_CANDIDATES; do
  if [ -w "$_d/wired_ctrl_limit" ]; then
    CTRL_DIR="$_d"
    break
  fi
done

SOC_CTRL=""
for _n in $SOC_CTRL_CANDIDATES; do
  if [ -w "$_n" ]; then
    SOC_CTRL="$_n"
    break
  fi
done

if [ -n "$CTRL_DIR" ]; then
  slog "温控节点命中：$CTRL_DIR"
else
  slog "未找到可写的温控节点（本机型可能不支持，或路径已变更）"
fi

if [ -n "$SOC_CTRL" ]; then
  slog "充电上限节点命中：$SOC_CTRL"
else
  slog "未找到可写的 smart_chg 节点（跳过充电上限恢复）"
fi

# ============================================================
#  3. 读写辅助
# ============================================================

# 写入节点。返回 0 = 成功；1 = 未写或失败（原因已记日志，不静默）
write_node() {
  _node="$1"
  _val="$2"
  _label="$3"

  [ -n "$_node" ] || return 1

  if [ ! -w "$_node" ]; then
    slog "⚠️ 跳过（不可写）：$_label → $_node"
    return 1
  fi

  if echo "$_val" > "$_node" 2>/dev/null; then
    slog "已写 $_label = $_val"
    return 0
  fi

  slog "⚠️ 写入失败：$_label → $_node"
  return 1
}

# 回读校验：不一致时【明确告警】，不静默放过
verify_node() {
  _node="$1"
  _expect="$2"
  _label="$3"

  [ -n "$_node" ] || return 0
  [ -r "$_node" ] || return 0

  _got=$(tr -d ' \t\r\n' < "$_node" 2>/dev/null)

  if [ "$_got" = "$_expect" ]; then
    slog "✓ $_label 回读一致（$_got）"
  else
    slog "⚠️ $_label 回读为 '$_got'，期望 '$_expect'"
  fi
}

# ============================================================
#  4. 温控轴：交回官方
# ============================================================

if [ -n "$CTRL_DIR" ]; then
  # 4.1 档位放开（0 = 最松）与移除标记归零 —— 通用语义，可安全带写死
  write_node "$CTRL_DIR/wired_ctrl_limit" "0" "档位 wired_ctrl_limit"
  write_node "$CTRL_DIR/wired_thermal_remove" "0" "移除标记 wired_thermal_remove"

  # 4.2 电流放开 —— 放开值因机型而异，从本机档案读实测上限
  #
  #     读取 capabilities.current.max_ua（检测得出的本机电流上限）。
  _max_ua=$(sed -n \
    's/.*"max_ua"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p' \
    "$DATADIR/device.json" 2>/dev/null | head -1)

  if [ -n "$_max_ua" ] && [ "$_max_ua" -gt 0 ] 2>/dev/null; then
    slog "本机档案记录的电流上限：$_max_ua μA"
    write_node "$CTRL_DIR/wired_chg_curr" "$_max_ua" "电流 wired_chg_curr"
    write_node "$CTRL_DIR/wired_chg_curr2" "$_max_ua" "电流 wired_chg_curr2"
  else
    # 🔴 不猜数值。档位与电流是两层并联，只放开档位可能仍被电流层限流；
    #    但写一个凭猜测的电流值风险更大，故选择"不写 + 明确告知"。
    slog "⚠️ 未能读到本机电流上限（device.json 不存在或无法解析）"
    slog "   → 电流节点未改动。若卸载后发现充电异常，重启手机即可恢复"
    slog "     （这些节点是驱动运行时状态，重启即丢）"
  fi

  verify_node "$CTRL_DIR/wired_ctrl_limit" "0" "档位"
  verify_node "$CTRL_DIR/wired_thermal_remove" "0" "移除标记"
fi

# ============================================================
#  5. 充电上限轴：解除限制
#
#  控制字 = (100 << 16) | 0x11 = 0x640011
#    · 100 = 驱动语义"不限制"
#    · 0x11 = 使能位（低 16 位 bit0+bit4），缺了不生效
# ============================================================

if [ -n "$SOC_CTRL" ]; then
  if write_node "$SOC_CTRL" "0x640011" "充电上限 smart_chg"; then
    sleep 1
    _read_node="$(dirname "$SOC_CTRL")/soc_limit"
    # 判断生效【永远读 soc_limit】，不能读 smart_chg（其读值会漂移）
    verify_node "$_read_node" "100" "充电上限生效值"
  fi
fi

# ============================================================
#  6. 清理运行残留
# ============================================================

rm -f "$DATADIR/ipc/control.sock" 2>/dev/null
rm -f "$DATADIR/worker.pid" "$DATADIR/supervisor.pid" 2>/dev/null

# ============================================================
#  7. 彻底清除数据目录（**卸载即无残留**）
#
#  用户明确要求：卸载后不要保留任何数据。
#  需要保全数据的用户，应在卸载前自行导出：
#      thermo ctl data export --dir /sdcard/Download
#
#  ⚠️ 删除前的守卫（宁可少删一次，也不在异常时删掉排查线索）：
#     ① 必须在【交回官方之后】—— 第 4、5 节已完成
#     ② 必须先确认节点已复原 —— 见下方检查
#     ③ 路径白名单 —— 防 `rm -rf ""` 类灾难
# ============================================================

_del_ok=1

if [ -n "$CTRL_DIR" ]; then
  _rem=$(tr -d ' \t\r\n' < "$CTRL_DIR/wired_thermal_remove" 2>/dev/null)
  _ctl=$(tr -d ' \t\r\n' < "$CTRL_DIR/wired_ctrl_limit" 2>/dev/null)
  if [ "$_rem" != "0" ]; then
    slog "⚠️ 移除标记未复原（当前 '$_rem'）—— 暂不删除数据目录，保留日志供排查"
    _del_ok=0
  fi
  if [ -n "$_ctl" ] && [ "$_ctl" != "0" ]; then
    slog "⚠️ 档位未复原（当前 '$_ctl'）—— 暂不删除数据目录，保留日志供排查"
    _del_ok=0
  fi
fi

if [ "$_del_ok" = "1" ]; then
  # 把"最后状态"记到**不会被删的位置**（数据目录随即消失）
  {
    echo "--- 卸载快照（$(date '+%Y-%m-%d %H:%M:%S')）---"
    echo "温控节点目录 : ${CTRL_DIR:-（未命中）}"
    echo "充电上限节点 : ${SOC_CTRL:-（未命中）}"
    echo "删除前的数据目录清单："
    ls -la "$DATADIR" 2>/dev/null
    echo "（本文件位于 /data/local/tmp，可自行删除）"
  } >> "$USLOG" 2>/dev/null

  # 路径白名单：只允许删除确定的这一个路径
  case "$DATADIR" in
    /data/adb/thermo_charge)
      if rm -rf "$DATADIR" 2>/dev/null && [ ! -e "$DATADIR" ]; then
        slog "已彻底删除数据目录 $DATADIR（无残留）"
      else
        slog "⚠️ 数据目录删除未完成，请手动检查 $DATADIR"
      fi
      ;;
    *)
      slog "⚠️ 数据目录路径异常（'$DATADIR'）—— 拒绝删除以免误伤"
      ;;
  esac
else
  slog "数据目录已保留（因节点状态异常，见上方告警）"
fi

slog "提示：如需彻底清除，可手动删除 /data/local/tmp/thermo_charge_uninstall.log"
slog "===== 卸载完成 ====="
exit 0
