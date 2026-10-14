# 温控充 ThermoCharge

按电池温度闭环控制充电速度，并可设定充电上限的 **KernelSU / APatch 模块**。

| 项 | 值 |
|---|---|
| 模块 id | `a_thermo_charge`（`a_` 前缀为在 KernelSU 模块列表**排序靠前**） |
| 形态 | root 常驻原生守护进程 + WebUI（APP 由独立仓库维护） |
| 引擎 | Rust（stable）单二进制 |
| 支持基线 | **Android 12（API 31）及以上**，**仅 arm64-v8a** |
| 当前版本 | **0.3.6**（已在 REDMI K90 Pro Max / Android 17 / KernelSU 上实测运行） |
| 实现状态 | 后端 / WebUI / 检测 / 多模式 / 充电历史 **全部完成**并真机验收 |

---

## 设计基线（已锁定）

| ID | 决策 | 含义 |
|---|---|---|
| B1 | **模块 / APP 分离** | APP 由独立仓库维护；模块 zip **不含任何 APK**；打包阶段以反向断言强制 |
| B2 | **支持基线 Android 12+** | 安装脚本门禁 `API ≥ 31`；`module.yaml` 的 `androidMinApi` 与之同步 |
| B3 | **APP 与模块共用同一后端** | 同一 `thermo` 二进制、同一协议、同一 JSON 契约；APP 自行申请 root，经 root 通道接入 |
| B4 | **不设工期约束** | 路线以里程碑与判据推进，不为工期牺牲设计 |
| B5 | **仅 arm64-v8a** | 不保留 32 位扩展位 |
| B6 | **不做包签名** | 以逐文件 SHA-256 覆盖完整性校验（签名不影响性能/耗电） |
| B7 | **全新项目** | 不承担历史配置与用户的迁移负担 |

---

## 关于内部设计文档

本仓库是**发布版**，只含可构建的源码。开发过程中另有一整套内部设计文档
（架构契约、节点契约表、配置说明书、接口规范、真机验收报告、踩坑记录等
约 15 篇），**不随本仓库发布**。

因此源码注释里会出现形如「见 `docs/03-契约-节点契约表.md`」的引用 ——
那是指向内部文档的**文字索引**，不是死链；在没有它们的仓库里也
不影响理解代码（关键结论都已写进对应位置的注释）。

### 契约三件套（核心约束，已内化为代码）

实现阶段不允许偏离的三份契约，其**要点已固化在代码与测试里**：

| 契约 | 内容 | 落地位置 |
|---|---|---|
| ① 节点契约表 | 全部可读写节点的路径、方向、取值、语义、写入纪律 | `native/crates/thermo-cli/src/sysfs.rs` + 检测流程 |
| ② 配置说明书 | `config.json` 的 **43 个配置键**：类型、范围、默认值、校验规则 | `config_store.rs` 的 `KEYS` 表（单一真源） |
| ③ 接口输出规范 | 命令的 3 种输出形态、错误码与退出码、已依赖细节 | `cli.rs` / `commands.rs` + `thermo-iipc` |

> 📌 **想改代码**：先读 `native/crates/thermo-core/src/`（纯决策内核，
> 无 I/O、无时钟、无 sysfs）—— 那是理解本项目最快的地方，
> 所有控制逻辑与边界校验都在那里，且带完整单元测试。

---

## 四模式配置

内建四套**互相独立**的温控配置，切换时各自保存、互不影响：

| 模式 | 含义 |
|---|---|
| **保守** | 低温慢充，最大化电池寿命 |
| **官方** | **模块完全不插手** —— 先解除自己此前加的限制，然后交给系统 |
| **极速** | 尽可能快，温度阈值放宽 |
| **自定义** | 完全由你掌控 |

```bash
ctl mode list              # 看四种模式 + 哪个在生效
ctl mode switch turbo      # 切换（会保存旧的、装载新的）
```

**四套参数全部可自定义**（包含收紧温度、放宽温度、收紧后降到多少、功率上限）。

> 与旧的 `preset`（模板：套用一次就覆盖当前配置）不同，
> `mode` 是**独立配置**：切走再切回，之前改的值还在。

---

## 充电历史

每次充电自动记录**过程曲线**（温度 / 功率 / 限流值 / 电量），充完归档：

```bash
ctl history list           # 会话列表 + 汇总
ctl history show           # 最近一次的完整曲线
```

界面在「日志」页看曲线图。

> **省闪存**：不是每秒落盘 —— 数据先在内存攒着，每 10 分钟存一次草稿，
> **充完才写一次**归档。一次 2 小时充电约 12KB，默认保留最近 30 次。
>
> ⚠️ 它是**独立观测归档**（`history.json`），不碰 `device.json`、
> 不参与任何决策 —— 因此**不违反**「实时数据不落盘」原则
> （那条原则保护的是设备档案不被实测值污染）。

---

## 环境与工具链

| 项 | 值 |
|---|---|
| Rust | **1.99.0**（`rust-toolchain.toml` 钉住） |
| 本机 host | **`x86_64-pc-windows-gnu`** —— **无需** Visual Studio Build Tools（可省约 3 GB） |
| Android target | `aarch64-linux-android` |
| NDK | **r29**（`29.0.14206865`） |
| WebUI | Vite + TypeScript + pnpm |

### 本机一次性配置

```powershell
# 1) 让 rustup 默认 host 指向 GNU（否则 1.99.0 会被解析成 msvc 版，缺 link.exe）
rustup set default-host x86_64-pc-windows-gnu

# 2) 安装钉住版本 + Android 目标
rustup toolchain install 1.99.0 --profile minimal --component rustfmt --component clippy
rustup target add aarch64-linux-android --toolchain 1.99.0
```

### 构建时环境变量

```powershell
$sc = "$env:USERPROFILE\.rustup\toolchains\1.99.0-x86_64-pc-windows-gnu\lib\rustlib\x86_64-pc-windows-gnu\bin\self-contained"
$ndk = "D:\Android\android-ndk-r29\toolchains\llvm\prebuilt\windows-x86_64\bin"
$env:Path = "$ndk;$sc;$env:USERPROFILE\.cargo\bin;$env:Path"
```

> ⚠️ **两个已确认的本机构建约束**
>
> 1. **必须让 `default-host` 指向 GNU**：`rust-toolchain.toml` 中的 `1.99.0` 会以 default host 补全；若为 msvc 则报 `linker 'link.exe' not found`。
> 2. **避开会在本机触发 `dlltool` 的依赖**：`windows-sys 0.61`（被 `clap` / `walkdir` 等引入）在**本机构建**时需用 `dlltool` 生成导入库，而 rustup 精简版在此调用方式下执行失败。因此 `xtask` 刻意不用 `clap` / `walkdir`（手写参数解析 + 标准库遍历）。
>    注：`thermo-cli` 使用 `clap` 不受影响 —— 它只做交叉编译，不构建 Windows 版。

### 常用命令

```powershell
# 在项目根目录下执行

# 交叉编译
cargo xtask build

# 质量检查（格式 + clippy + 测试）
cargo xtask check

# 组装模块 zip（含 SHA-256 与 16 KB 对齐检查）
cargo xtask pack

# 推到设备并安装
cargo xtask push
cargo xtask install --via ksud
```

### 设备端诊断命令（排查问题时按这个顺序）

```sh
BIN=/data/adb/modules/a_thermo_charge/bin

# ① 出了任何问题先跑它 —— 端到端逐项自检，有失败项则非零退出
$BIN/thermo verify

# ② 换机型 / 节点没探到时看它 —— 9 张候选表逐条探测 + 未探到功能汇总
$BIN/thermo probe

# ③ 想知道"官方在多少度开始降"—— 官方温控表 + 全部节名清单
$BIN/thermo probe --official

# ④ 此刻实际充多快（只读，不写档案）
$BIN/thermo measure --duration-ms 5000
```

| 命令 | 回答什么问题 | 需要插充电器？ |
|---|---|---|
| `verify` | 整套是否正常工作？哪一环断了？ | 不需要 |
| `probe` | 节点在哪？档位表什么样？哪项没探到？ | 不需要 |
| `probe --official` | 官方策略是什么？它何时先动手？ | 不需要 |
| `measure` | 现在实际功率多少？ | **需要**（否则无意义） |

> ⚠️ `verify` 与 `probe` 都是**只读、零副作用**的 —— 排查问题不会制造问题。

---

## 一句话架构

```text
service.sh
  └─ thermo supervisor            仅进程监督 + 崩溃恢复（写固定安全值）
       └─ thermo worker           采样 / 决策 / 唯一常规 sysfs 写者
            ├─ thermo-core        纯决策内核（无 I/O、无时钟、无 sysfs）
            ├─ sysfs 充电节点      先钳位 → 写入 → 回读校验
            └─ UDS                control.sock（0600 root；CLI / WebUI / APP 共用）
```

---

## 设备验证基线

| 项 | 值 |
|---|---|
| 机型 | REDMI K90 Pro Max（`25102RKBEC` / `myron` / `canoe`） |
| 系统 | Android 17（API 37），内核 6.12.69 |
| Root | KernelSU v3.3.0 |
| 节点 | 主控节点路径已实测有效 |

> ⚠️ **但本项目不预置任何机型数据**：档位表、电流上限、硬件保护温度、
> 充电功率上限**全部靠上设备检测**得到。上表只是记录"在哪种机器上验证过"，
> 换机型照常工作（检测不到时**拒绝控制**，而不是用猜的值去动硬件）。

---

## 许可

[GNU General Public License v3.0](LICENSE)（GPL-3.0）。

> 选用 GPL-3.0 是因为本项目会**写入充电控制节点** —— 这类直接操作硬件的
> 代码，使用者应当有权查看、审计并改进它。
