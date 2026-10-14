#!/system/bin/sh
# ============================================================
#  full-deploy.sh —— 把打包产物**完整**部署到设备模块目录
#
#  # 为什么需要它
#
#  开发时常常只推二进制（`bin/thermo`），于是 webroot 与 shell 脚本
#  还是旧版本 —— 用户看到的现象就是"**模块没更新**"。
#  本脚本按 zip 内容**全覆盖**，保证设备与产物逐字节一致。
#
#  # 为什么先停进程
#
#  Linux 不允许写入正在执行的可执行文件。不停进程就覆盖 `bin/thermo`
#  会报 `Text file busy`，而且**部分文件已被覆盖、部分是旧的** ——
#  这种半更新状态比不更新更难排查。
# ============================================================

set -e

# ⚠️ 模块目录名 = `module.prop` 的 `id`（KernelSU 用 id 作目录名）
#    当前 id 为 `a_thermo_charge`（`a_` 前缀是为了在模块列表里排序靠前）
#
# ⚠️ 数据目录（`/data/adb/thermo_charge`）**与模块 id 无关**，本脚本不碰它，
#    因此改 id 或重装模块都不会丢配置/档案/历史。
MOD=/data/adb/modules/a_thermo_charge
ZIP=/data/local/tmp/thermo_charge.zip
STAGE=/data/local/tmp/thermo_stage

# 曾经用过的模块 id（改 id 后需清理旧目录，见 ①B）
LEGACY_MODS="/data/adb/modules/thermo_charge"

# ============================================================
#  ① 停进程
# ============================================================
echo "==> ① 停掉进程"
pkill -f 'thermo worker' 2>/dev/null || true
pkill -f 'thermo supervisor' 2>/dev/null || true
sleep 2

LEFT=$(ps -A -o PID,ARGS 2>/dev/null | grep thermo | grep -v grep | wc -l | tr -d ' ')
if [ "$LEFT" = "0" ]; then
  echo "    ✓ 已全部停止"
else
  echo "    ⚠️ 仍有 $LEFT 个进程，尝试强杀"
  pkill -9 -f 'thermo' 2>/dev/null || true
  sleep 2
fi

# ============================================================
#  ①B 清理旧 id 的模块目录（**改名迁移的必需步骤**）
# ============================================================
#
# # 为什么必须删掉旧目录
#
# KernelSU 用 `module.prop` 的 `id` 作为模块目录名。改 id 后若旧目录还在，
# 就会出现**两个模块目录并存**：
#
# | 目录 | 状态 |
# |---|---|
# | `/data/adb/modules/thermo_charge` | 旧的（含旧 id 的 module.prop） |
# | `/data/adb/modules/a_thermo_charge` | 新的 |
#
# 后果**不是**"显示两个模块"这么简单 —— 两个目录各有一份
# `service.sh`，开机时会**各拉一个 supervisor**，于是两个 worker
# 同时往同一批充电节点写入、互相覆盖，控制彻底错乱。
#
# # 为什么直接 rm -rf 而不走 uninstall.sh
#
# `uninstall.sh` 会**删除整个数据目录**（用户明确要求"卸载后不留数据"）。
# 而这里只是**改名迁移**，配置 / 设备档案 / 充电历史都该保留 ——
# 所以只能绕过卸载脚本，直接删模块目录。
echo ""
echo "==> ①B 清理旧 id 的模块目录"
for old in $LEGACY_MODS; do
  if [ -d "$old" ]; then
    # 顺带保住用户数据：确认数据目录存在，避免"删完才发现数据也没了"
    if [ -d /data/adb/thermo_charge ]; then
      echo "    ⚠️ 发现旧模块目录：$old"
      echo "       数据目录 /data/adb/thermo_charge 保留（配置与档案不丢）"
    else
      echo "    ⚠️ 发现旧模块目录：$old（数据目录不存在，无可保留）"
    fi
    rm -rf "$old"
    echo "    ✓ 已删除旧目录（未执行其 uninstall.sh，故数据目录未受影响）"
  fi
done
echo "    （检查清单：$LEGACY_MODS）"

# ============================================================
#  ② 解包
# ============================================================
echo ""
echo "==> ② 解包到暂存区"
rm -rf "$STAGE"
mkdir -p "$STAGE"
cd "$STAGE"
unzip -o -q "$ZIP" -x 'META-INF/*'
CNT=$(find "$STAGE" -type f | wc -l | tr -d ' ')
echo "    解出 $CNT 个文件"

# ============================================================
#  ③ 覆盖
# ============================================================
#
# 🔴 这里必须区分「目录」与「文件」—— 踩过的坑：
#
#   `cp -a src/defaults dst/` 在 `dst/defaults` **已存在**时，
#   会把源目录复制成 `dst/defaults/defaults`（**嵌套**），
#   而不是覆盖 `dst/defaults` 的内容！
#
#   真机表现（2026-10-13）：
#
#     defaults/
#     ├── config.json        ← 旧的，一直没被更新
#     └── defaults/config.json  ← 新的悄悄出现在这里
#
#   ⇒ `checksum.sha256` 校验报「2 项不一致」，而且**部署脚本
#     每一条都打了 ✓** —— 最难排查的一类：失败被成功日志掩盖。
#
#   ⇒ 修法：**目录一律「先删后拷」**。
#     这同时解决了另一个老问题：旧产物里已不存在的文件会残留
#     （原先只能靠 ④ 的废弃清单逐个列，现在目录级自动清干净）。
#
#   ⚠️ `cp -a "$item"/. "$MOD/bin/"`（带 `/.` 且结尾斜杠）是**正确**
#      的写法，但为了统一与彻底，这里不再特判 —— 所有目录同一条路径。
echo ""
echo "==> ③ 覆盖到模块目录"

for item in "$STAGE"/*; do
  name=$(basename "$item")
  if [ -d "$item" ]; then
    # 目录：整体替换（先删后拷）
    rm -rf "$MOD/$name"
    cp -a "$item" "$MOD/$name"
    n=$(find "$MOD/$name" -type f | wc -l | tr -d ' ')
    echo "    ✓ $name/（$n 个文件）"
  else
    # 文件：直接覆盖（`cp -a` 对已存在的文件是正常覆盖）
    cp -a "$item" "$MOD/$name"
    echo "    ✓ $name"
  fi
done

# ============================================================
#  ④ 清扫已废弃的文件
# ============================================================
#
# ⚠️ 覆盖式部署**不会删除**不在 zip 里的文件。
#    历史上的废弃文件会一直残留 —— 例如 `action.sh` 会继续让
#    管理器显示左侧的 Action 按钮（即使模块已不再提供该功能）。
#
# 这里显式列出"曾经存在但已废弃"的路径。
echo ""
echo "==> ④ 清扫已废弃文件"
DEPRECATED="action.sh"
for f in $DEPRECATED; do
  if [ -e "$MOD/$f" ]; then
    rm -rf "$MOD/$f"
    echo "    ✓ 已删除废弃文件：$f"
  fi
done
echo "    （清单：$DEPRECATED）"

# ============================================================
#  ⑤ 权限
# ============================================================
echo ""
echo "==> ⑤ 设置权限"
chmod 755 "$MOD/bin/thermo" "$MOD/bin/ctl" "$MOD/bin/daemon" 2>/dev/null || true
chmod 644 "$MOD/module.prop" 2>/dev/null || true
for s in service.sh uninstall.sh post-fs-data.sh; do
  [ -f "$MOD/$s" ] && chmod 755 "$MOD/$s"
done
# ⚠️ webroot 的权限与 SELinux context 由管理器负责，这里只保证可读
chmod -R 755 "$MOD/webroot" 2>/dev/null || true
echo "    ✓ 已完成"

# ============================================================
#  ⑤ checksum 校验
# ============================================================
echo ""
echo "==> ⑥ checksum 校验"
cd "$MOD"
if [ -f checksum.sha256 ]; then
  ok=0
  bad=0
  while read -r sum path; do
    [ -z "$path" ] && continue
    if [ ! -f "$path" ]; then
      bad=$((bad + 1))
      echo "    ✗ 缺失：$path"
      continue
    fi
    actual=$(sha256sum "$path" | awk '{print $1}')
    if [ "$actual" = "$sum" ]; then
      ok=$((ok + 1))
    else
      bad=$((bad + 1))
      echo "    ✗ 不匹配：$path"
    fi
  done < checksum.sha256

  if [ "$bad" = "0" ]; then
    echo "    ✓ 全部一致（$ok 项）"
  else
    echo "    ✗ 有 $bad 项不一致（$ok 项一致）"
  fi
else
  echo "    （无 checksum.sha256）"
fi

# ============================================================
#  ⑥ 启动
# ============================================================
echo ""
echo "==> ⑦ 启动进程"
nohup "$MOD/bin/thermo" supervisor >/dev/null 2>&1 &
sleep 4
ps -A -o PID,PPID,STAT,ARGS 2>/dev/null | grep thermo | grep -v grep | sed 's/^/    /'

# ============================================================
#  ⑦ 文件清单（供核对）
# ============================================================
echo ""
echo "==> ⑧ 更新后的文件时间戳"
ls -la "$MOD" | sed 's/^/    /'
echo ""
ls -la "$MOD/bin" | sed 's/^/    /'
echo ""
ls -la "$MOD/webroot" "$MOD/webroot/assets" 2>/dev/null | sed 's/^/    /'

# ============================================================
#  ⑧ 清理
# ============================================================
#
# ⚠️ **保留 zip**，只清暂存区。
#
#   旧版把 zip 一起删了，后果是「每部署一次就得重新 push」——
#   而排错时往往要连跑好几次，每次都要 adb push 656 KB，很烦。
#
#   zip 放在 `/data/local/tmp`，**重启后由系统自动清空**，
#   不必由本脚本操心。
echo ""
echo "==> ⑨ 清理暂存区（保留 zip）"
rm -rf "$STAGE"
echo "    ✓ 已清理暂存区"
echo "    · zip 保留在 $ZIP（可直接重跑本脚本）"
