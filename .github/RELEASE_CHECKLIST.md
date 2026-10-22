# 发布检查表

> 用户要求（2026-10-22）：**以后发布新版必须带上当前版本号**。
> 版本号唯一来源：`module/src/module.prop` 的 `version`（与 `native/Cargo.toml` 的
> `[workspace.package] version` 由测试守护，必须一致）。

## 版本号硬规则

以下四处必须一致地带上 `v<ver>`：

| 位置 | 格式 |
|---|---|
| tag | `v<ver>` |
| Release 标题 | `v<ver> · 澎湃充控 HyperPower` |
| **Release 资源名** | **`HyperPower-v<ver>.zip`** |
| `dist/release-notes-<ver>.md` 首行 | `v<ver> · 澎湃充控 HyperPower` |
| commit message 首行 | `release: v<ver>（摘要）` |

## 发布前

```bash
VER=$(grep '^version=' module/src/module.prop | cut -d= -f2)
echo "即将发布 v$VER"
# 必须与 native/Cargo.toml 的 version 相同
grep -n 'version = ' native/Cargo.toml | head -3
```

```bash
cd native
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cd ..

cargo xtask webui      # 必须先于 pack（R14）
cargo xtask pack

unzip -l dist/HyperPower-v$VER.zip
# 必须含：module.prop / bin/thermo / defaults/ / profiles/ / webroot/index.html / checksum.sha256
# 必须不含：*.apk
```

## 发布

```bash
git tag "v$VER"
git push origin main "v$VER"
gh release create "v$VER" dist/HyperPower-v$VER.zip \
  --title "v$VER · 澎湃充控 HyperPower" \
  --notes-file "dist/release-notes-$VER.md"
```

## 发布后

- 从 Release 下载 `HyperPower-v$VER.zip`，核对 SHA-256 与本地 `dist/HyperPower-v$VER.zip` 一致；
- 核对包内 `module.prop` 的 `version` / `versionCode` 与 tag 一致；
- 真机安装后**重启**，跑 `thermo verify`。
