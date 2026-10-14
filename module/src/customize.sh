#!/system/bin/sh
# ============================================================
#  customize.sh —— 安装时执行
#
#  KernelSU/APatch 内置安装器会 source 本文件。
#  可用变量: MODPATH TMPDIR ZIPFILE ARCH IS64BIT API KSU APATCH
#  可用函数: ui_print abort set_perm set_perm_recursive
# ============================================================
# shellcheck disable=SC2034

# 自行解包（需要按架构挑选文件，不能依赖默认解包）
SKIPUNZIP=1

MODNAME="温控充 ThermoCharge"

ui_print " "
ui_print "  ╔══════════════════════════════════════╗"
ui_print "  ║   $MODNAME"
ui_print "  ║   按电池温度闭环控制充电速度"
ui_print "  ╚══════════════════════════════════════╝"
ui_print " "

# ---------- 1. 环境门禁 ----------
# 支持基线：Android 12（API 31）及以上
# 与 module.yaml 的 compatibility.androidMinApi 保持一致
if [ -z "$API" ] || [ "$API" -lt 31 ]; then
  abort "! 需要 Android 12 (API 31) 及以上，当前 API: ${API:-未知}"
fi

# 仅支持 arm64
case "$ARCH" in
  arm64) ;;
  *) abort "! 不支持的架构: $ARCH（本模块仅支持 arm64-v8a）" ;;
esac

if [ -z "$KSU" ] && [ -z "$APATCH" ]; then
  ui_print "- 未检测到 KernelSU/APatch 环境，将以兼容模式继续"
fi

# ---------- 2. 解包 ----------
ui_print "- 解包模块文件"

_ver=$(grep '^version=' "$TMPDIR/module.prop" 2>/dev/null | cut -d= -f2)
ui_print "- 模块版本：v${_ver:-unknown}"

# 解出全部内容
#
# 🔴 曾经写成 `unzip ... -x 'META-INF/*' 'checksum.sha256'` —— 把校验清单
#    也排除掉了。而下面第 3 步又要检查 `$MODPATH/checksum.sha256` 是否存在，
#    于是**校验永远被跳过**（真机安装日志里从来没有"校验文件完整性"那一行）。
#
# 现在让清单一起解出来，校验完再删。校验清单本身不在清单里
# （不能自我校验），因此排除它不影响完整性验证。
unzip -o "$ZIPFILE" -x 'META-INF/*' -d "$MODPATH" >&2
[ $? -ne 0 ] && abort "! 解包失败"

# ---------- 3. 完整性校验 ----------
# 只校验分发的文件（`checksum.sha256` 自己不在清单内）
if [ -f "$MODPATH/checksum.sha256" ] && [ -f "$MODPATH/verify.sh" ]; then
  ui_print "- 校验文件完整性"
  # shellcheck disable=SC1091
  . "$MODPATH/verify.sh"
  if ! verify_module "$MODPATH"; then
    abort "! 文件校验失败 —— 安装包可能已损坏，请重新下载"
  fi
  # 校验脚本不保留在模块目录
  rm -f "$MODPATH/checksum.sha256" "$MODPATH/verify.sh"
else
  # ⚠️ 静默跳过是危险的：用户会以为"安装成功"= 文件完好
  ui_print "! 未找到校验清单，跳过完整性校验（安装包可能不完整）"
fi

# ---------- 4. 权限 ----------
# ⚠️ webroot/ 的权限与 SELinux context 由管理器自动设置，此处绝不触碰
ui_print "- 设置文件权限"

set_perm "$MODPATH/bin/thermo" 0 0 0755
set_perm "$MODPATH/bin/ctl"    0 0 0755
set_perm "$MODPATH/bin/daemon" 0 0 0755
set_perm "$MODPATH/module.prop" 0 0 0644

[ -f "$MODPATH/module.yaml" ] && set_perm "$MODPATH/module.yaml" 0 0 0644
[ -f "$MODPATH/service.sh" ]     && set_perm "$MODPATH/service.sh"     0 0 0755
[ -f "$MODPATH/uninstall.sh" ]   && set_perm "$MODPATH/uninstall.sh"   0 0 0755
[ -f "$MODPATH/post-fs-data.sh" ] && set_perm "$MODPATH/post-fs-data.sh" 0 0 0755
# ⚠️ 本模块**不使用** `action.sh`
#
# 管理器里的 Action 快捷方式（模块卡片左侧那个运行图标）由 `action.sh` 驱动；
# 本模块不提供该脚本 ⇒ **管理器不会显示左侧按钮**，只保留右侧的 WebUI 入口。
#
# 之所以移除：状态查看已由 WebUI 承担，多一个只读摘要按钮价值不大。
# 若日后需要，重新加入 `action.sh` 即可自动恢复该按钮（无需改此处）。

if [ -d "$MODPATH/profiles" ]; then
  set_perm_recursive "$MODPATH/profiles" 0 0 0644 0644
fi
if [ -d "$MODPATH/defaults" ]; then
  set_perm_recursive "$MODPATH/defaults" 0 0 0644 0644
fi

# ---------- 5. 数据目录 ----------
# 配置与模块目录分离：升级/重装模块不丢配置
DATADIR=/data/adb/thermo_charge
ui_print "- 初始化数据目录"

mkdir -p "$DATADIR/logs" "$DATADIR/ipc" 2>/dev/null
chmod 700 "$DATADIR" 2>/dev/null
chown 0:0 "$DATADIR" 2>/dev/null

if [ -f "$DATADIR/config.json" ]; then
  ui_print "  检测到已有配置，保留（升级不覆盖）"
else
  ui_print "  首次安装，写入默认配置"
  cp -f "$MODPATH/defaults/config.json"  "$DATADIR/config.json"  2>/dev/null
  cp -f "$MODPATH/defaults/presets.json" "$DATADIR/presets.json" 2>/dev/null
fi

# 清理上次运行残留
rm -f "$DATADIR/worker.pid" 2>/dev/null
rm -f "$DATADIR/ipc/control.sock" 2>/dev/null

# ---------- 6. 自带 APP 说明 ----------
# 模块不内置 APP（APP 为独立组件，单独分发）
ui_print "- 配套 APP 为独立组件（本模块不含 APK）"
ui_print "  如需 APP：请从项目发布页单独下载安装"

# ---------- 7. 收尾 ----------
ui_print " "
ui_print "  ✓ 安装完成"
ui_print "  重启后生效，或在管理器打开模块 WebUI"
ui_print " "
