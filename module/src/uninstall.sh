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
#  🔴 为什么本脚本【自包含】，不调用 hyperpower 二进制
#
#     卸载时模块目录可能已被移除、二进制可能已损坏 ——
#     本脚本是"最后一道保障"，因此：
#       · 不复用任何 lib/ 脚本（模块目录可能已不可读）
#       · 不调用 hyperpower 二进制（同样可能不可用）
#       · 不依赖 device.json 一定存在（用户可能从未跑过检测）
#
#     代价：候选路径表需在本脚本内维护一份，
#     必须与 native/crates/hyperpower-cli/src/sysfs.rs 的候选表保持一致。
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
DATADIR=/data/adb/hyperpower

# 卸载日志的落点：
#   数据目录里的日志可能随之被清理，故同时在 /data/local/tmp 留一份，
#   便于卸载后排查（该目录用户与 adb 都能访问、可自行删除）。
SLOG="$DATADIR/logs/service.log"
USLOG=/data/local/tmp/hyperpower_uninstall.log

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
#
# ⚠️ 必须同时认旧名 `bin/thermo`（2026-11-01 改名前的二进制）。
#    从旧版本直接卸载时，跑着的是 thermo supervisor/worker，
#    只认新名会漏杀 ⇒ 卸载后进程继续写 sysfs，上限残留。
for _bin in "$MODDIR/bin/hyperpower" "$MODDIR/bin/thermo"; do
  _pids=$(pgrep -f "$_bin supervisor" 2>/dev/null)
  for _p in $_pids; do
    kill "$_p" 2>/dev/null
    slog "已停止 supervisor（pid=$_p）"
  done
  _pids=$(pgrep -f "$_bin worker" 2>/dev/null)
  for _p in $_pids; do
    kill "$_p" 2>/dev/null
    slog "已停止 worker（pid=$_p）"
  done
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
/sys/class/power_supply/wired/charger_thermal
/sys/class/power_supply/battery
"

SOC_CTRL_CANDIDATES="
/sys/class/xm_power/charger/smart_charge/smart_chg
/sys/class/charger/smart_charge/smart_chg
/sys/class/power_supply/smart_charge/smart_chg
/sys/class/power_supply/battery/charge_control_end_threshold
"

# 控制节点的**文件名**候选（与 sysfs.rs 的 CTRL_NAMES / CURR_NAMES 同源）
#
# ⚠️ 小米封装层用 `wired_*`，标准 power_supply 框架用上游名字。
#    只认前者的话，标准框架机型**卸载后不恢复**（2026-11-01 审计 O-02/O-04）。
GEAR_NAME_CANDIDATES="wired_ctrl_limit charge_control_limit"
CURR_NAME_CANDIDATES="wired_chg_curr constant_charge_current"
CURR2_NAME_CANDIDATES="wired_chg_curr2"
REMOVE_NAME_CANDIDATES="wired_thermal_remove thermal_remove"

# 在 $1 目录下按名字候选找**第一个存在**的节点，回显其绝对路径
pick_node() {
  _dir="$1"
  shift
  for _nm in "$@"; do
    if [ -e "$_dir/$_nm" ]; then
      echo "$_dir/$_nm"
      return 0
    fi
  done
  return 1
}

CTRL_DIR=""
for _d in $CTRL_DIR_CANDIDATES; do
  if [ -w "$_d/$(echo $GEAR_NAME_CANDIDATES | cut -d' ' -f1)" ] \
     || [ -w "$_d/$(echo $GEAR_NAME_CANDIDATES | cut -d' ' -f2)" ]; then
    CTRL_DIR="$_d"
    break
  fi
done

SOC_CTRL=""
for _n in $SOC_CTRL_CANDIDATES; do
  if [ -e "$_n" ]; then
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
  GEAR_NODE=$(pick_node "$CTRL_DIR" $GEAR_NAME_CANDIDATES)
  CURR_NODE=$(pick_node "$CTRL_DIR" $CURR_NAME_CANDIDATES)
  CURR2_NODE=$(pick_node "$CTRL_DIR" $CURR2_NAME_CANDIDATES)
  REMOVE_NODE=$(pick_node "$CTRL_DIR" $REMOVE_NAME_CANDIDATES)

  # 4.1 档位放开（0 = 最松）与移除标记归零 —— 通用语义，可安全带写死
  write_node "$GEAR_NODE" "0" "档位"
  write_node "$REMOVE_NODE" "0" "移除标记"

  # 4.2 电流放开 —— 放开值因机型而异，从本机档案读实测上限
  #
  #     读取 capabilities.current.max_ua（检测得出的本机电流上限）。
  _max_ua=$(sed -n \
    's/.*"max_ua"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p' \
    "$DATADIR/device.json" 2>/dev/null | head -1)

  if [ -n "$_max_ua" ] && [ "$_max_ua" -gt 0 ] 2>/dev/null; then
    slog "本机档案记录的电流上限：$_max_ua μA"
    write_node "$CURR_NODE" "$_max_ua" "电流"
    write_node "$CURR2_NODE" "$_max_ua" "第二路电流"
  else
    # 🔴 不猜数值。档位与电流是两层并联，只放开档位可能仍被电流层限流；
    #    但写一个凭猜测的电流值风险更大，故选择"不写 + 明确告知"。
    slog "⚠️ 未能读到本机电流上限（device.json 不存在或无法解析）"
    slog "   → 电流节点未改动。若卸载后发现充电异常，重启手机即可恢复"
    slog "     （这些节点是驱动运行时状态，重启即丢）"
  fi

  verify_node "$GEAR_NODE" "0" "档位"
  verify_node "$REMOVE_NODE" "0" "移除标记"
fi

# ============================================================
#  5. 充电上限轴：解除限制
#
#  ⚠️ 两种方言语义**完全不同**，必须分流（2026-11-01 审计 O-03）：
#
#  | 方言 | 节点 | "不限制"的写法 |
#  |---|---|---|
#  | 小米控制字 | `smart_charge/smart_chg` | `0x640011` = (100<<16)\|0x11 |
#  | 标准百分比 | `battery/charge_control_end_threshold` | 纯十进制 `100` |
#
#  往百分比节点写 `0x640011` 是越界值（驱动不接受，或行为未定义）。
# ============================================================

if [ -n "$SOC_CTRL" ]; then
  case "$SOC_CTRL" in
    */charge_control_end_threshold)
      # ── 标准框架：纯百分比 ──
      #
      # 🔴 顺序至关重要（2026-11-01 K70 Pro 真机实测）
      #
      # `charge_control_en = 0` 时写 `charge_control_end_threshold`
      # **返回成功但值被驱动吞掉**，读回恒为 0：
      #
      # | 操作 | 结果 |
      # |---|---|
      # | en=0，写 100 | 读回 **0** ❌ |
      # | en=0，写 95 | 读回 **0** ❌ |
      # | en=1 后写 100 | 读回 **100** ✅ |
      #
      # 原先的顺序是「先写阈值、后关使能位」⇒ 阈值写入被吞，
      # 用户卸载后上限仍停在旧值（充不满）。必须改成：
      #
      #   ① 先 en=1 → ② 写 100 → ③ 再 en=0
      #
      # ⚠️ 注意第 ③ 步：这里**要**把 en 关回 0（模块从未介入的状态），
      #    但必须先让 100 真正写进去。关 en 不会回滚已生效的阈值。
      EN_NODE_PRE=$(pick_node /sys/class/qcom-battery charge_control_en)
      [ -z "$EN_NODE_PRE" ] && EN_NODE_PRE=$(pick_node /sys/class/power_supply/battery charge_control_en)

      if [ -n "$EN_NODE_PRE" ]; then
        write_node "$EN_NODE_PRE" "1" "charge_control_en（先开启以便写入阈值）"
        sleep 1
      fi

      write_node "$SOC_CTRL" "100" "充电上限（百分比）"
      sleep 1
      verify_node "$SOC_CTRL" "100" "充电上限生效值"

      if [ -n "$EN_NODE_PRE" ]; then
        write_node "$EN_NODE_PRE" "0" "charge_control_en（还原为未启用）"
      fi
      ;;
    *)
      # ── 小米控制字 ──
      if write_node "$SOC_CTRL" "0x640011" "充电上限 smart_chg"; then
        sleep 1
        _read_node="$(dirname "$SOC_CTRL")/soc_limit"
        # 判断生效【永远读 soc_limit】，不能读 smart_chg（其读值会漂移）
        verify_node "$_read_node" "100" "充电上限生效值"
      fi
      ;;
  esac
fi

# ============================================================
#  5.5 附加功能写入的节点（**卸载必须还原**）
#
#  🔴 2026-11-01 审计 O-05：此前这三个节点**完全没有恢复**，
#     而 worker 退出路径也只做温控轴（见 H-02），⇒ 没有任何一层兜底。
#
#  | 节点 | 谁写的 | 不还原的后果 |
#  |---|---|---|
#  | `fake_soc` | soc_limit | 电池图标停在虚假电量 |
#  | `charge_control_en` | soc_limit | 标准框架上限一直有效，充不满 |
#  | `pd_verifed` | pps | 驱动跳过认证状态残留 |
#
#  ⚠️ `pd_verifed` 只在**未充电时**还原：充电中写会让驱动重新协商、
#     表现为"插上又断开"（真机结论，见 docs/03）。
# ============================================================

FAKE_SOC_NODE=""
for _n in /sys/class/qcom-battery/fake_soc /sys/class/power_supply/battery/fake_soc; do
  [ -e "$_n" ] && { FAKE_SOC_NODE="$_n"; break; }
done

if [ -n "$FAKE_SOC_NODE" ]; then
  # fake_soc 是"让系统显示值跟随真实 rsoc"的覆盖节点。
  # 卸载时把它写成真实容量即可（写成 0 会让图标掉到 0%）。
  _rsoc=""
  for _n in /sys/class/qcom-battery/capacity /sys/class/power_supply/battery/capacity; do
    [ -r "$_n" ] && { _rsoc=$(tr -d ' \t\r\n' < "$_n" 2>/dev/null); break; }
  done
  case "$_rsoc" in
    ''|*[!0-9]*)
      slog "⚠️ 读不到真实电量，跳过 fake_soc 还原（重启后驱动自行恢复）"
      ;;
    *)
      write_node "$FAKE_SOC_NODE" "$_rsoc" "fake_soc（还原为真实电量）"
      ;;
  esac
fi

# 使能位：模块自己开过才关（写 0 = 关闭使能）
EN_NODE=""
for _n in /sys/class/qcom-battery/charge_control_en /sys/class/power_supply/battery/charge_control_en; do
  [ -e "$_n" ] && { EN_NODE="$_n"; break; }
done
[ -n "$EN_NODE" ] && write_node "$EN_NODE" "0" "charge_control_en"

# PPS 认证位：仅未充电时还原
PD_NODE=""
for _n in /sys/class/qcom-battery/pd_verifed /sys/class/power_supply/battery/pd_verifed; do
  [ -e "$_n" ] && { PD_NODE="$_n"; break; }
done
if [ -n "$PD_NODE" ]; then
  _st=""
  for _n in /sys/class/power_supply/battery/status /sys/class/qcom-battery/status; do
    [ -r "$_n" ] && { _st=$(tr -d ' \t\r\n' < "$_n" 2>/dev/null); break; }
  done
  if [ "$_st" = "Charging" ]; then
    slog "⚠️ 正在充电，跳过 pd_verifed 还原（充电中写会中断协议握手）"
    slog "   → 拔电后重启手机即可恢复"
  else
    write_node "$PD_NODE" "0" "pd_verifed"
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
#      hyperpower ctl data export --dir /sdcard/Download
#
#  ⚠️ 删除前的守卫（宁可少删一次，也不在异常时删掉排查线索）：
#     ① 必须在【交回官方之后】—— 第 4、5 节已完成
#     ② 必须先确认节点已复原 —— 见下方检查
#     ③ 路径白名单 —— 防 `rm -rf ""` 类灾难
# ============================================================

_del_ok=1

if [ -n "$CTRL_DIR" ]; then
  # ⚠️ 必须同时判 `-n`（2026-11-01 审计 H-04）
  #
  # 节点不存在 / 不可读时 `tr` 输出空串。旧代码只判 `!= "0"`，
  # 空串不等于 "0" 恒成立 ⇒ `_del_ok=0` ⇒ **数据目录永远删不掉**，
  # 违反"卸载即无残留"。相邻的 `_ctl` 判断本来就有 `-n`，两者不对称。
  _rem=""
  [ -n "$REMOVE_NODE" ] && _rem=$(tr -d ' \t\r\n' < "$REMOVE_NODE" 2>/dev/null)
  _ctl=""
  [ -n "$GEAR_NODE" ] && _ctl=$(tr -d ' \t\r\n' < "$GEAR_NODE" 2>/dev/null)

  if [ -n "$_rem" ] && [ "$_rem" != "0" ]; then
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
    /data/adb/hyperpower)
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

slog "提示：如需彻底清除，可手动删除 /data/local/tmp/hyperpower_uninstall.log"
slog "===== 卸载完成 ====="
exit 0
