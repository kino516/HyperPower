# AGENTS.md · 澎湃充控 HyperPower

面向 **AI 代理与人类贡献者** 的操作手册。目标不是复述架构，而是让你在**不破坏安全边界**的
前提下，快速找到该改的文件、跑对命令、验证到位。

| 项 | 值 |
|---|---|
| 适用版本 | v1.0.0（`52c510b`，2026-10-21） |
| 项目形态 | KernelSU / APatch 模块：root 常驻 Rust 守护进程 + Vue WebUI |
| 读者 | 接手开发者、代码评审者、自动化代理 |
| 配套文档 | [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)（怎么运作）· [docs/DECISIONS.md](docs/DECISIONS.md)（为什么）· [docs/HANDOFF.md](docs/HANDOFF.md)（现在做到哪、还差什么） |

> **冲突处理顺序**：系统/用户指令 > 本文件 > `docs/ARCHITECTURE` / `docs/DECISIONS` >
> 其他 `docs/*`（多数是历史快照，**数字与路径经常过时**）。
> 代码行为与旧文档冲突时，**以代码为准**，并把差异记进 `docs/HANDOFF.md` 或 `docs/DECISIONS.md`。

---

## 0. 项目事实卡

| 项 | 值 |
|---|---|
| 模块 id | `a_hyperpower`（`a_` 前缀让 KernelSU 列表排序靠前） |
| 模块目录 | `/data/adb/modules/a_hyperpower` |
| 数据目录 | `/data/adb/hyperpower`（**与模块 id 无关**，改 id 不丢数据） |
| 控制 socket | `/data/adb/hyperpower/ipc/control.sock`（0600 root，`SO_PEERCRED` UID=0） |
| 二进制 | `bin/hyperpower`（唯一）；`bin/ctl` / `bin/daemon` 是 shim |
| 支持基线 | Android 12（API 31）+，仅 arm64-v8a |
| 配置 | `config.json`，**55 个键 / 11 个分区**（`config_store.rs` 的 `KEYS` 是唯一真源） |
| 测试基线 | 本机 Windows `cargo test --workspace` = **435 passed**（2026-11-01）；proptest 仅非 Windows |
| 命令面 | 24 个顶层命令名（子命令合计 50+；旧文档的 45/52 是含二级动作的口径） |
| 许可 | GPL-3.0；模块 zip **不含 APK** |

**一句话控制链**：`service.sh → supervisor → worker → hyperpower-core 决策 → sysfs 写入`；
所有客户端（CLI/WebUI/APP）只经 UDS 提交意图，**worker 是唯一常规写者**。

---

## 1. 快速开始

### 1.1 环境（本机 Windows，一次性）

```powershell
# 1) rustup 默认 host 必须是 GNU；1.99.0 是钉死版本
rustup set default-host x86_64-pc-windows-gnu
rustup toolchain install 1.99.0 --profile minimal --component rustfmt --component clippy
rustup target add aarch64-linux-android --toolchain 1.99.0

# 2) NDK（本机路径）
#    D:\Android\android-ndk-r29
```

### 1.2 每个新 shell 的 PATH

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
$env:Path = "D:\Android\android-ndk-r29\toolchains\llvm\prebuilt\windows-x86_64\bin;$env:Path"
$env:Path = "$env:USERPROFILE\.rustup\toolchains\1.99.0-x86_64-pc-windows-gnu\lib\rustlib\x86_64-pc-windows-gnu\bin\self-contained;$env:Path"
```

> ⚠️ `cargo` 不在默认 PATH；bash 里直接 `cargo` 会 command not found。
> ⚠️ 仓库根**没有 `Cargo.toml`**：`cargo xtask` 靠根 `.cargo/config.toml` 的 alias；
> `cargo test` / `cargo fmt` 必须 `cd native`。

### 1.3 三条命令

```powershell
# 质量门槛（Rust）
cd native
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# WebUI 门槛（构建 + 3 个自检 + 7 道源/产物门禁）
cd ..
cargo xtask webui

# 打包（必须先 webui，再 pack）
cargo xtask pack
```

产物：

```text
native/target/aarch64-linux-android/release/hyperpower   ~1.10 MB
dist/HyperPower-v<ver>.zip                        ~0.74 MB
```

### 1.4 没有真机时能验证什么

| 能 | 不能 |
|---|---|
| 决策内核、钳位、迟滞、硬保护 | 真实驱动行为（`write()` 谎报成功、读值漂移、`D` 死锁） |
| addon 双方言、使能位、回读（假节点树） | 真实 SELinux 类型 |
| 完整「采样→决策→写→回读」（`HYPERPOWER_SYSFS_ROOT`） | 系统「电池保护」开关的交互 |
| WebUI 构建、类型、样式/组件/注释门禁 | 真机 WebView 渲染（白屏类事故） |
| 交叉编译、16 KB 对齐、打包 | 长时间稳定性、卸载恢复、高温极限 |

**结论**：本机全绿只能证明「代码与构建正确」，不能证明「真机可用」。涉及驱动的改动必须上机。

---

## 2. 仓库地图

```text
HyperPower/
├── AGENTS.md                 ← 本文件
├── README.md                 用户向说明（构建顺序有遗漏，见 §8）
├── native/
│   ├── Cargo.toml            workspace（4 个 crate；版本号与 module.prop 同步）
│   ├── .cargo/config.toml    NDK linker 绝对路径 + 16 KB/RELRO 链接参数
│   ├── deny.toml             依赖审计（禁 yanked/wildcards/未知源；ban tokio 等）
│   └── crates/
│       ├── hyperpower-core/      纯决策内核（无 I/O/时钟/sysfs）
│       ├── hyperpower-ipc/       协议、错误码、DTO、设备档案
│       ├── hyperpower-addons/    附加功能（soc_limit / pps；**不依赖 core**）
│       └── hyperpower-cli/       单二进制：supervisor/worker/ctl/watch/probe/verify/device
├── webui/                    Vue 3 + miuix-vue + Vite；src/ 4 页签
├── module/src/               ★ 模块内容唯一来源（打包只复制这一层）
│   ├── module.prop           版本权威来源
│   ├── module.yaml           结构化元数据（androidMinApi 必须与脚本门禁一致）
│   ├── customize.sh / post-fs-data.sh / service.sh / uninstall.sh / verify.sh
│   ├── sepolicy.rule         SELinux 放行（改完必须重启）
│   ├── bin/{hyperpower,ctl,daemon}
│   ├── defaults/{config.json,presets.json}
│   ├── profiles/             出厂机型档案（当前为空）
│   └── webroot/              WebUI 产物（gitignored，由 xtask webui 生成）
├── xtask/                    build/check/pack/webui/push/install/help
├── tools/                    full-deploy.sh / jsoncheck.sh / measure-power.sh / docx
├── tests/                    contract / device / fixtures（当前基本为空）
├── docs/                     内部设计文档（**gitignored**，不随仓库发布）
└── .github/workflows/ci.yml  native / webui / module 三个 job
```

### 2.1 改哪里（按任务）

| 任务 | 主文件 | 必看测试/约束 |
|---|---|---|
| 控制律、迟滞、硬保护 | `hyperpower-core/src/decision.rs`、`config.rs` | `hyperpower-core/tests/{decision,bounds}.rs` |
| 采样、写入、回读 | `hyperpower-cli/src/engine.rs`、`sysfs.rs` | engine 内嵌假节点树测试 |
| 调度、休眠、信号 | `worker.rs`、`sleep_events.rs`、`supervisor.rs` | `docs/15`；`sleep_events` 纯函数 |
| 充电上限 | `hyperpower-addons/src/soc_limit/` | `tests/soc_limit.rs`（双方言 + 使能位） |
| PPS | `hyperpower-addons/src/pps/` | 门控四条件 + 三条硬约束 |
| 配置键 | `config_store.rs` 的 `KEYS` + `default_config()` | 测试 `键总数为五十五` |
| 检测/档案 | `detect.rs`、`profile_io.rs`、`hyperpower-ipc/src/device.rs` | `pending_items` / `failed_items` |
| 命令/输出 | `commands.rs`、`cli.rs`、`hyperpower-ipc/` | 单行 JSON；`tools/jsoncheck.sh` |
| WebUI | `webui/src/{api.ts,pages/,theme.css}` | `npm run build` 的全部门禁 |
| 构建/打包 | `xtask/src/{main,pack}.rs` | 16 KB、无 APK、checksum 排序 |
| 安装/卸载 | `module/src/*.sh` | API≥31、arm64、重启、恢复双轴 |

---

## 3. 不可违反的规则

> 每条都对应一次真机事故或安全边界。违反其中任何一条，代码评审直接拒绝。

### R1 唯一写者

- **worker 是唯一常规 sysfs 写者**；supervisor 只在崩溃/卡死路径写固定安全值；
- ctl / WebUI / APP **从不直接写节点**，只经 UDS 提交意图；
- 检测的档位写探是**受控例外**，必须用户勾选 + 充电中 + 完整模式；
- 新增"写节点"的功能，先问：它能不能变成一个 addon / 命令？

### R2 未检测 ⇒ 零控制

`Bounds.detected == false` ⇒ `should_write: false`、`release: false`，一个节点都不写。
该门禁**排在 `enabled` 之前**。不要用"保守兜底值"放行——两次真机事故都是这么来的。

### R3 不写死机型数据

档位表、电流上限、功率上限、硬件温度**全部来自本机检测**。
禁止把某台机型的实测值（如 16 档、18.4 A、120 W）写成**控制默认值或机型常量**；
文档举例时必须标明来源机型。常量只允许是**物理护栏**（如 30 A 绝对上限），且必须注释说明。

### R4 附加功能与温控编译期隔离

- `hyperpower-addons` **不得依赖** `hyperpower-core`；
- addon 接口里不得出现档位/温度/`Decision`；
- addon 自己**不碰文件系统**，只经 `AddonIo`；
- addon 禁止 panic（lint deny），连续失败 3 次熔断；
- addon 的 sysfs I/O **必须走看门狗**（专用线程 + 3 s 超时），否则一次 `D` 死锁会拖死温控。

### R5 先钳位 → 写入 → 回读

- 决策层按 `Bounds` 钳一次，写入前 `write_if_different` 再对齐；
- 写入失败按逆序回滚（档位 ← 电流 ← curr2）；
- **回读只校验档位**：未充电时电流读的是能力上限，不能当判据；
- 写入/回读连续失败达 5 次 ⇒ 交回官方；单次采样失败只跳过本拍，不释放。

### R6 电流释放只放宽不收紧

`release_control` 写电流时：节点现值 ≥ 目标值就不写。
档案是保守兜底，直接写回会在检测偏差时**反而限速**（真机 18.4 A → 5 A）。

### R7 配置：`KEYS` 单一真源，未知键保留

- 新增键：`KEYS` + `default_config()` + `module/src/defaults/config.json` + 测试计数 + WebUI + 文档；
- 加载为通用 JSON，**未知键原样保留**（升降级不丢字段）；
- 标量只读键（`thermal.abs_max`）加载时强制回正；`curve` 是数组、加载时不回正（`set` 会拒绝，手改文件可绕过——已知缺口）；
- 写入原子：tmp → `fsync` → `rename`；批量写任一失败整批拒绝；
- 运行期钳位**绝不落盘**（检测可能不准，钳位会污染用户配置）。

> 当前是 **55 键 / 11 分区**（53 个 `spec(...)` + 2 个 `ro(...)`）。
> AGENTS.md 旧版的 51、README / `docs/04` 的 43 都过时。

### R8 契约只增不改

- 命令名、JSON 字段、`--plain` 键名、错误码、退出码**冻结**；
- 新增能力只能追加字段；语义变更提升 `protocol_version`；
- **成功时 stdout 只能有一行 JSON**；提示/日志走 stderr；
- 失败时 stdout 为空、错误走 stderr、退出码符合 `ErrorCode`。

### R9 `mode` ≠ `preset`

- `mode` 是**四套独立配置**：只切换 `thermal`/`control`，切走再切回值还在；
- `preset` 是**模板**：套用一次覆盖当前值，切走即丢；
- 不要把 `modes` 当配置键（它不在 `KEYS` 里）。

### R10 两个 `0` 语义相反

| 键 | `0` 的含义 |
|---|---|
| `thermal.tighten_power_w` | **停止充电**（用户把「降到多少」拖到最左） |
| `control.target_power_w` | **不限制功率**（默认） |

写文档、写界面、写测试时必须并列说明，不能只写"0 = 关闭"。

### R11 档位不是温度旋钮

温度收紧**只调电流**；档位只在硬保护时取最紧档。
在验证机型上，档位 14/15 会切断快充（16 V → 8.9 V，44 W → 1.9 W）；用档位凑温度会静默毁掉用户体验。

### R12 关闭 ≠ 卸载

| 操作 | 语义 | 行为 |
|---|---|---|
| `limit off` | 停止干预 | 小米侧对上限节点零写入（仅还原 `fake_soc` 显示）；标准侧再把模块开过的 `en` 写回 0 |
| 卸载 | 从未介入 | 必须把上限写回 100%（`0x640011`），否则用户被卡在 80% |
| `mode official` | 交回官方 | 写**一次**释放（不是零写入） |
| `observe_only` | 只观测 | 真正不写 |
| 未检测 | 门禁 0 | 不写、也不释放 |

### R13 `sepolicy.rule` 改完必须重启

KernelSU 只在开机 `post-fs-data` 应用它；模块更新不会热加载。
不重启的症状：节点探测/界面/检测都正常，但**写入失败**（Permission denied），
表现为"设置应用了却没生效"。

### R14 构建顺序：webui → pack

`pack` 只复制 `module/src/**`，**不构建、不校验 webroot**。
`module/src/webroot/` 被 gitignore。新克隆直接 `pack` 会打出**没有 WebUI 的 zip 且全绿**。

### R15 WebUI 的构建门禁不能删

| 门禁 | 事故 |
|---|---|
| 剥 `crossorigin` | KernelSU WebView 不返回 CORS 头 ⇒ 内容区空白 |
| `index.html` 注释禁止 `--` | 解析器吞掉 Vite 注入的 `<script>` ⇒ 永远转圈 |
| 禁止运行期 `template:` | runtime-only Vue ⇒ 整树塌成 `<!---->` |
| `check-style-dup` | scoped 样式静默覆盖共享层 |
| `check-component-import` | 漏 import 的大驼峰组件渲染成未识别元素 ⇒ 控件消失 |

删任何一条 = 重新引入一次真机白屏事故。

### R16 不引入：APK、网络、异步运行时、新进程

| 禁止 | 原因 |
|---|---|
| 模块内 APK | B1；打包反向断言 |
| 运行时下载/联网 | 必须离线可用 |
| `tokio` / `async-std` | 1~3 Hz 不需要；panic 语义与体积更差 |
| 再拆进程 | 管理进程被杀不会撤掉内核限制（`docs/15`） |
| Hook / 内核模块 | 不在范围；OTA 与签名风险 |

### R17 不猜、不静默

- 读不到就报错，不猜数值（尤其电流/温度/上限）；
- 失败必须有事件/日志，不能静默 `unwrap_or_default`；
- 检测项状态不满足时归 `pending_items`（待补测），**不是** `failed_items`（真失败）；
- 面向用户的错误必须说清"该做什么"。

### R18 版本号与发布

- 两处版本必须同步：`native/Cargo.toml` 的 `[workspace.package] version` 与
  `module/src/module.prop` 的 `version` / `versionCode`（有测试守护）；
- 发布前必须 bump；同版本号不能对应两份不同二进制；
- **发布/推送需用户明确要求**；发完从 Release 下载回来核对 SHA-256。

---

## 4. 改动工作流

### 4.1 改控制律（core）

```powershell
# 1) 先读 core，不碰 I/O
#    native/crates/hyperpower-core/src/{lib,decision,config,profile,state}.rs

# 2) 改 + 补测试（纯函数，秒级）
cd native
cargo test -p hyperpower-core

# 3) 全量门槛
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# 4) 若动了边界/默认值，同步 config_store 与 defaults/config.json
# 5) 交叉编译 + 打包（见 §4.6）
```

必须补的测试类型：

| 改动 | 至少补 |
|---|---|
| 新门禁/优先级 | 一条"优先级更高时不会被绕过"的测试 |
| 新钳位 | 一条"任意输入不越界"的边界测试（最好加 proptest） |
| 新默认值 | 一条"出厂默认与 `defaults/config.json` 一致"的测试 |
| 语义变更 | 一条 golden/快照，锁定输出 |

### 4.2 新增/修改配置键

1. `config_store.rs`：`KEYS` 加 `spec(...)`（类型/范围/只读）；
2. `default_config()` + `module/src/defaults/config.json` 同步；
3. **接线**：在 `to_core()` / 消费点读取（否则就是"能改但不生效"）；
4. 测试 `键总数为五十五` 会失败 ⇒ 改成新数量；
5. WebUI 表单/类型（`webui/src/types.ts`、对应页面）；
6. 更新 `docs/04`（注意它当前已滞后）；
7. `cargo xtask check` + `cargo xtask webui`。

> 反面教材：当前有 15 个键**完全无消费点**（`device.*`、`daemon.*`、`ui.module_desc`、
> `log.level`、`log.csv`、`control.feedforward`、`control.stabilize_s`、`thermal.abs_max`）。
> 新增键时不要重复这个错误。

### 4.3 改 WebUI

```powershell
cd webui
npm install          # 首次 / 依赖变更
npm run build        # vue-tsc + 3 个门禁 + vite
```

必须遵守：

| 规则 | 原因 |
|---|---|
| `base: "./"` 不能改 | 模块目录加载，绝对路径会 404 |
| 不写 HTML 注释（尤其含 `--`） | 会吞掉 `<script>` |
| 不写运行期 `template:` | runtime-only Vue |
| 新组件必须 import（大驼峰） | 否则渲染成未识别元素 |
| 页面样式不要重复定义 `theme.css` 的类 | scoped 会静默覆盖共享层 |
| 所有 `ksu.exec` 必须有超时 | 老项目 R2 规则；不杀后端进程 |
| 不拼接任意命令字符串 | `ksu.exec` 是 root；类型化函数是安全边界 |

**改完必须实际加载一次产物**（浏览器或真机 WebView）。`vue-tsc`/build/门禁全绿也可能白屏。

### 4.4 新增一个附加功能

1. `hyperpower-addons/src/<id>/{mod,config,logic}.rs`；
2. 实现 `Addon` 的 `spec/probe/tick/release/status`；
3. `registry.rs::build_all` 加一段；
4. `defaults/config.json` 加自己的分区；
5. 端到端测试用假 `AddonIo`（不要碰真实文件系统）；
6. 温控核心**一行都不用改**。

检查清单：

- [ ] probe 零写入（零副作用）；
- [ ] 未启用时零写入；
- [ ] 连续失败 3 次熔断；
- [ ] I/O 经 `AddonIo`（由宿主包看门狗）；
- [ ] 配置坏掉只停用自己，不影响温控/其他 addon。

### 4.5 改节点/新增方言

1. `sysfs.rs` 加候选表（路径、节点名，按优先级）；
2. `soc_limit`/`pps` 的 `Dialect` 加分派；
3. 明确三件事：**生效值在哪、是否需要使能位、关闭语义**；
4. 假节点树补端到端；
5. 真机验证写读一致性（`write()` 成功 ≠ 生效）；
6. 若加 SELinux 类型，`sepolicy.rule` 逐字核对 `ls -lZ`，并**重启**。

### 4.6 构建与部署

```powershell
# 构建（含交叉编译）
cargo xtask build --variant release

# WebUI（必须先）
cargo xtask webui

# 打包
cargo xtask pack

# 推送到设备
cargo xtask push

# 安装
cargo xtask install --via ksud     # 或 apatch / magisk

# 🔴 重启（sepolicy / 开机自启）
adb reboot
```

设备端验证：

```sh
BIN=/data/adb/modules/a_hyperpower/bin
$BIN/hyperpower verify
$BIN/hyperpower probe
$BIN/hyperpower measure --duration-ms 5000     # 需插充电器
$BIN/ctl status
```

### 4.7 修 Bug 的标准流程

```text
① 复现：记录命令、配置、插电状态、dmesg/events
② 定位：先判断是「决策层」「写入层」「驱动层」还是「界面层」
③ 用假节点树写一个失败测试（能离线复现就别上机）
④ 修实现，让测试转绿
⑤ 想清楚副作用：会不会影响未检测/observe_only/官方模式/释放路径？
⑥ 真机验证：至少覆盖「正常 + 关闭 + 卸载/重启」三条路径
⑦ 记一笔：事件/文档/注释写清「为什么」
```

---

## 5. 质量门禁与完成定义

### 5.1 两条链，缺一不可

| 链 | 命令 | 覆盖 |
|---|---|---|
| Rust | `cd native && cargo xtask check`（等价 fmt + clippy + test） | 决策、I/O、配置、检测、协议 |
| WebUI | `cargo xtask webui` 或 `cd webui && npm run build` | 类型 + 3 个自检 + 7 道源/产物门禁 |

> ⚠️ Windows 上 `cargo xtask check` 的 `test_args()` 只跑 `core + ipc`，
> 但实测 `cargo test --workspace` 能跑 **435 项**（含 cli/addons）。想跑全量就直接用 `cargo test --workspace`。

### 5.2 完成定义（DoD）

一个改动只有同时满足以下条件才算完成：

- [ ] 代码编译（`cargo build` / `cargo check`）
- [ ] `cargo fmt --all --check` 通过
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` 通过
- [ ] `cargo test --workspace` 通过（新逻辑有测试）
- [ ] WebUI 改动：`npm run build` 通过 + **实际加载一次**
- [ ] 契约改动：单行 JSON 验证 + `jsoncheck.sh`（若涉及命令）
- [ ] 配置改动：`KEYS`/默认值/测试计数/WebUI/文档同步
- [ ] 版本改动：`Cargo.toml` 与 `module.prop` 同步
- [ ] 真机相关：按 §4.6 部署 + **重启** + `verify` 通过
- [ ] 文档：ARCHITECTURE / DECISIONS / HANDOFF 中至少一处更新（如果改了行为/决策）
- [ ] 没有把 `docs/`、`dist/`、`node_modules/`、`target/` 加进提交

---

## 6. 真机操作规范

### 6.1 安全顺序

```text
确认设备 → 确认模块版本 → 备份配置/档案（可选）→ 部署 → 重启 → verify → 功能验证
```

- 部署前先确认旧模块目录（`a_thermo_charge`）是否还在；**两个 supervisor 会双写**；
- 新二进制**不能直接 `cp`**（`Text file busy`），先停进程或 `mv` 原子替换；
- 卸载会**删除整个数据目录**（用户要求"卸载即无残留"）；需要数据先 `ctl data export`；
- 重启前想清楚：重启会清掉 `fake_soc`、SOC 上限等驱动运行时状态。

### 6.2 常用诊断

```sh
# 端到端
$BIN/hyperpower verify

# 节点候选表（9→实际 12 张 pub 表）
$BIN/hyperpower probe

# 官方温控表 + 节名
$BIN/hyperpower probe --official

# 当前功率（只读）
$BIN/hyperpower measure --duration-ms 5000

# 事件/日志
tail -n 50 /data/adb/hyperpower/events.jsonl
tail -n 100 /data/adb/hyperpower/logs/service.log   # 仅开机脚本记录
logcat -d -s hyperpower | tail -n 300               # worker 运行日志

# addon 状态
$BIN/ctl limit show
$BIN/ctl addon show pps
```

### 6.3 绝对不要做

| 不要 | 原因 |
|---|---|
| `echo 80 > /sys/.../smart_chg` | sysfs 不支持 truncate；且 `smart_chg` 是控制字 |
| 写 `soc_limit` | 派生显示值，会被状态机覆盖 |
| 用 `smart_chg` 的读值判断生效 | 读值在 16/17/80 漂移 |
| 未插电跑档位写探 | 驱动不接受，会把上限永久记低一档 |
| 充电中反复写 `pd_verifed` | 干扰协议握手 ⇒ 插上又断开 |
| 改完 `sepolicy.rule` 不重启 | 规则不生效 ⇒ Permission denied |
| 用 `pkill -f hyperpower` | 无单实例保证、易误杀；用 pidfile / flock |

---

## 7. 常见错误速查

| 症状 | 最可能原因 | 处理 |
|---|---|---|
| 设置不生效 | 装/更新后没重启 | 重启 |
| 写入 Permission denied | SELinux 类型没放行 | 看 avc，核对 `sepolicy.rule` 类型名，重启 |
| 充电上限「不可用」 | 标准框架缺 `en=1` / 已熔断 | `ctl limit show`；`ctl addon reset soc_limit`；重启 |
| 电池图标虚高/不准 | `capacity` 与 `fg1_rsoc` 不一致 | 模块自动写 `fake_soc`；关闭功能会还原 |
| 卸载后充不满 | 上限残留 | 重启；或卸载前先关闭功能 |
| WebUI 空白 | `crossorigin` / 注释吞 script / 运行期 template | 看产物 `index.html`；跑 `cargo xtask webui` 的门禁 |
| 控件消失 | 大驼峰组件漏 import | `node check-component-import.mjs` |
| 前端白屏无报错 | TDZ / 模板问题 | 浏览器实开；看 `window.__errs` |
| `hyperpower` 命令没反应 | worker 卡死 / socket 残留 | `verify`；看 events；必要时重启 |
| 测试 `键总数为五十五` 失败 | 加了/删了配置键没改断言 | 同步测试与文档 |
| `cargo xtask` 找不到 | 不在仓库根 / 没有 alias | `cd` 到根；或 `cargo run --manifest-path xtask/Cargo.toml --` |
| `cargo test` 找不到 Cargo.toml | 在根目录执行 | `cd native` |

---

## 8. 文档地图

| 文档 | 用途 | 可信度 |
|---|---|---|
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | 当前架构、数据流、安全模型、扩展点 | 🟢 当前 |
| [docs/DECISIONS.md](docs/DECISIONS.md) | 决策、备选、被推翻的结论 | 🟢 当前 |
| [docs/HANDOFF.md](docs/HANDOFF.md) | 现状、验证基线、待办、runbook | 🟢 当前 |
| `README.md` | 用户向功能说明 | 🟡 构建顺序漏了 `webui`；部分数字旧 |
| `docs/01`…`docs/15` | 历史设计/契约/验收 | 🟡 多数是快照；**数字/路径/结论可能过时** |
| `docs/08-交接摘要.md` | 多轮交接混排 | 🔴 不能当时间线；只信最新的 §0.x |

> 2026-10-21：`06-M0完成报告.md` 与 `交接-2026-10-19.md` 已删除；后者「K70 Pro 无法设 SOC 上限」
> 的错误结论已纠正并记录在 DECISIONS 附录 A。

**不要盲信文档里的数字**：配置键数（43→51）、候选表（9→12）、命令数（45/52→24 个顶层命令名）、
WebUI 包管理器（pnpm→npm）、SOC 下限（60/55 冲突）都以代码为准。

---

## 9. 发布

> 仅当用户明确要求时执行。

### 🔴 发布必须带当前版本号（用户要求，2026-10-22）

版本号的**唯一来源**是 `module/src/module.prop` 的 `version`。以下**四处必须一致地带上它**：

| 位置 | 格式 |
|---|---|
| tag | `v<ver>` |
| Release 标题 | `v<ver> · 澎湃充控 HyperPower` |
| release notes 首行 | `v<ver> · 澎湃充控 HyperPower` |
| commit message 首行 | `release: v<ver>（摘要）` |

发布前先取出并核对（必须与 `native/Cargo.toml` 的 `version` 相同，有测试守护）：

```bash
VER=$(grep '^version=' module/src/module.prop | cut -d= -f2)
echo "即将发布 v$VER"
```

```powershell
# 1) bump 版本（两处）
#    native/Cargo.toml 的 [workspace.package] version
#    module/src/module.prop 的 version / versionCode

# 2) 全绿
cd native; cargo test --workspace; cargo clippy --workspace --all-targets -- -D warnings; cd ..

# 3) 构建 WebUI + 打包
cargo xtask webui
cargo xtask pack

# 4) 检查产物
#    unzip -l dist/HyperPower-v<ver>.zip
#    必须含：module.prop / bin/hyperpower / defaults/ / profiles/ / webroot/index.html / checksum.sha256
#    必须不含：*.apk

# 5) 真机安装 + 重启 + verify

# 6) 写 release notes（dist/release-notes-<ver>.md，首行 `v<ver> · ...`）、提交、打 tag、发布
git tag "v$VER"
git push origin main "v$VER"
gh release create "v$VER" dist/HyperPower-v$VER.zip `
  --title "v$VER · 澎湃充控 HyperPower" `
  --notes-file "dist/release-notes-$VER.md"

# 7) 从 Release 下载回来核对 SHA-256
```

> ⚠️ 注意区分：tag / 标题带 `v` 前缀；`release-notes-<ver>.md` 文件名与版本字段用**裸版本号**。
> 不要发布没有版本号的 Release 标题或 notes。

已知发布链缺口（截至 2026-10-22）：CI 没有 release job（手工发布）；`module` job 不依赖 `webui`。
详见 HANDOFF §5。

---

## 10. 给 AI 代理的额外规则

### 10.1 工作方式

1. **先读再改**：改文件前先 `read`；改配置/契约前先读 `KEYS` 与调用点。
2. **小步验证**：每改一处，先跑最小测试（`-p hyperpower-core` / 单测名），再跑全量。
3. **不要盲改格式**：不要对未触碰的文件跑 formatter；不要重排无关代码。
4. **保留用户改动**：工作区可能有未提交内容；不要 `git checkout -- .` / `git reset --hard`。
5. **不要擅自发布**：`git push`、打 tag、发 Release、`adb reboot`、卸载模块、`rm -rf` 数据目录
   都属于"需明确确认"的操作。
6. **不碰 `docs/` 之外的隐私**：`docs/` 含本机路径/设备序列号，不要把它加进版本库或贴到外部。
7. **记录发现**：发现文档与代码不一致，不要静默"顺手改文档"；写进 HANDOFF 的待办或 DECISIONS
   的"待裁决"，并给出代码证据（`文件:行`）。
8. **中文**：注释、文档、提交信息用简体中文；技术术语保留英文。

### 10.2 何时必须停下来问

| 情况 | 为什么 |
|---|---|
| 要改安全边界（R1/R2/R5/R6/R12） | 可能直接变成充电事故 |
| 要动 SELinux / 卸载 / 清数据 | 设备可能无法恢复，用户数据会丢 |
| 要改契约（命令/字段/错误码） | WebUI 与未来 APP 都依赖 |
| 要 bump 版本 / 发布 | 发布是用户的决定 |
| 发现 P0 冲突（如 SOC 55/60） | 需要产品裁决，不是"顺手统一" |
| 真机行为与代码推断不一致 | 先收集证据（dmesg/events/节点读值），不要猜 |

### 10.3 证据标准

- 结论必须能落到 `文件:行`、命令输出或真机日志；
- 不确定就写"未确认"，不要用"应该/大概"；
- 修 bug 时先写**能失败的测试**，再修；
- 改行为时同步更新 ARCHITECTURE（行为）、DECISIONS（为什么）、HANDOFF（状态/待办）中至少一处。

---

## 11. 术语

| 术语 | 含义 |
|---|---|
| 温控轴 | 控制"充多快"：档位 + 电流 |
| 上限轴 | 控制"充到多少停"：`soc_limit` |
| Dialect | 充电框架方言（小米控制字 / 标准百分比） |
| 门禁 0 | 未检测 ⇒ 零写入 |
| 硬保护 | 达到硬件温度上限 ⇒ 最紧档 + 最小电流 |
| 虚拟硬保护 | 机身温度保护，默认 48℃ |
| 交回官方 | 释放自己加的限制 |
| 安全值 | supervisor 崩溃路径写的固定恢复值 |
| 熔断 | addon 连续失败 3 次后停用 |
| 粘性 stuck | addon I/O 超时后永久停用，不重试 |
| 轻拍 | 有客户端但未充电时只处理请求 |
| `Pending` | 命令面收集的待写配置，由 worker 落盘 |

---

**最后一句**：这个项目的第一原则是「**不猜硬件、不静默失败、不绕过安全边界**」。
当你不确定某个改动是否安全时，选"不动硬件"的那条路，并把问题记下来。
