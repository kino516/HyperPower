#!/system/bin/sh
# ============================================================
#  verify.sh —— 安装期文件完整性校验
#
#  由 customize.sh source 后调用 verify_module()。
#  依赖同目录的 checksum.sha256（每行：<sha256>  <相对路径>）。
# ============================================================

verify_module() {
  _root="$1"
  _list="$_root/checksum.sha256"

  [ -f "$_list" ] || return 0   # 无清单则跳过（如调试包）

  _fail=0
  _ok=0
  while IFS= read -r _line; do
    [ -z "$_line" ] && continue
    _hash=$(echo "$_line" | awk '{print $1}')
    _path=$(echo "$_line" | awk '{print $2}')
    [ -z "$_hash" ] || [ -z "$_path" ] && continue

    _file="$_root/$_path"
    if [ ! -f "$_file" ]; then
      ui_print "  ! 缺少文件: $_path"
      _fail=$((_fail + 1))
      continue
    fi

    _actual=$(sha256sum "$_file" 2>/dev/null | awk '{print $1}')
    if [ "$_actual" = "$_hash" ]; then
      _ok=$((_ok + 1))
    else
      ui_print "  ! 校验失败: $_path"
      _fail=$((_fail + 1))
    fi
  done < "$_list"

  if [ "$_fail" -gt 0 ]; then
    ui_print "  校验结果: $_ok 通过 / $_fail 失败"
    return 1
  fi
  ui_print "  校验结果: $_ok 项全部通过"
  return 0
}
