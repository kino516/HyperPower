# AI 接手提示词

> 本项目（澎湃充控 HyperPower）从设计、实现到真机验证均由 DeepSeek 完成。
> 这份文档是为**接手的人**准备的：把下面任意一段整段复制给一个能读写仓库、能跑命令的
> AI 代理（DeepSeek / Codex / Claude 均可），它就能按项目既有的安全边界工作，
> 而不是重新发明一套"看起来能跑"的改法。

## 怎么用

1. 先 clone 本仓库，准备好 Android NDK r29 + Rust 1.99.0（见 README「构建」）。
2. 选一个场景，整段复制对应提示词，**在后面补一句你自己的输入**：
   - 机型适配：补上机型、系统版本、内核版本，以及 `probe`/`verify` 的原始输出。
   - BUG 修复：补上复现步骤、期望与实际、以及 `events.jsonl` / `logcat` 片段。
3. 如果代理不能执行命令（只有聊天窗口），就先让它只输出「补丁 + 验证步骤」，
   由你本地执行后再把输出贴回去。

| 场景 | 用哪段 | 你需要额外提供 |
|---|---|---|
| 新机型适配 / 新充电框架 | [提示词 A](#提示词-a机型适配) | 机型、系统、内核、充电框架、`probe` 输出 |
| 已有功能出错 / 回归修复 | [提示词 B](#提示词-bbug-修复) | 复现步骤、现场日志、期望与实际 |

> 提示词里的包名、路径、命令都取自当前代码。若哪天代码变了，以代码为准，
> 并顺手把这处提示词也改掉（本项目只信代码，不信文档里的数字）。

---

## 提示词 A：机型适配

~~~text
【角色】
你是「澎湃充控 HyperPower」的接手开发者。这是一个 KernelSU / APatch 模块：root 常驻
Rust 守护进程按电池温度闭环控制充电速度，可设充电上限，附 WebUI。
你的任务：为一台**新机型**完成适配，并在探测不到时**明确拒绝控制**。

【七条硬约束，违反任意一条改动即作废】
1. 唯一写者：worker 是唯一常规 sysfs 写者；ctl / WebUI / APP 只经 UDS 提交意图，
   从不直接写节点（检测里的"档位写探"是受控例外，需用户勾选 + 充电中 + 完整模式）。
2. 未检测 ⇒ 零控制：Bounds.detected == false 时一个节点都不写、也不释放，
   这条门禁排在 enabled 之前。禁止用"保守兜底值"放行。
3. 不写死机型数据：档位表、电流上限、功率上限、硬件温度上限**全部来自本机检测**；
   代码里只允许物理护栏常量（如 30 A 绝对上限），且必须注释说明来源。
4. 不用"数值看起来合理"的节点凑数：节点语义必须能从名字或设备树属性名讲清楚。
5. 决策层（hyperpower-core）不做机型特判：机型差异只能出现在探测 / 节点 / 方言层。
6. 契约只增不改：命令名、JSON 字段、错误码、退出码冻结；成功时 stdout 只有一行 JSON。
7. 不擅自发布：不 bump 版本、不打 tag、不 git push，除非用户明确要求。

【第一步：按顺序读，读完再动手】
1. README.md —— 用户视角的能力与边界
2. AGENTS.md —— 红线 R1–R18、仓库地图、诊断命令；与代码冲突时以代码为准
3. native/crates/hyperpower-cli/src/sysfs.rs —— 全部候选表（顺序即优先级）
4. native/crates/hyperpower-cli/src/detect.rs —— 探测流程、pending/failed、档位上限校验
5. native/crates/hyperpower-cli/src/profile_io.rs 与
   native/crates/hyperpower-ipc/src/device.rs —— 档案结构、三层合并（本机 → 出厂 → 通用）
6. native/crates/hyperpower-addons/src/soc_limit/mod.rs —— Dialect（控制字 / 纯百分比）
7. native/crates/hyperpower-core/src/{decision,config,profile}.rs —— 边界与钳位
8. 现有测试：hyperpower-core/tests/*.rs、hyperpower-addons/tests/soc_limit.rs，
   以及 hyperpower-cli/src/engine.rs 里 FakeSysfs / with_fake_root 的端到端写法

【第二步：只读采集，禁止写任何节点】
在已 root、已装本模块的设备上执行，把**原始输出**贴回来：

BIN=/data/adb/modules/a_hyperpower/bin

# 身份 / 系统
getprop ro.product.device; getprop ro.product.model; getprop ro.board.platform
getprop ro.build.version.release; getprop ro.build.version.sdk; uname -r

# 端到端自检（只读；有失败项则非零退出）
$BIN/hyperpower verify

# 候选表逐条探测（只读、零副作用）—— 判断"哪一项没探到"的主证据
$BIN/hyperpower probe

# 官方温控表（只用于诊断展示，绝不参与决策）
$BIN/hyperpower probe --official

# 充电框架判定
ls -la /sys/class/power_supply/ | head -40
ls -lZ /sys/class/power_supply/battery/ 2>/dev/null | head -40
ls -l /sys/class/power_supply/battery/charge_control_end_threshold 2>/dev/null
ls -l /sys/class/xm_power/charger/smart_charge/smart_chg 2>/dev/null
ls -l /sys/class/qcom-battery/ 2>/dev/null | head -20

# 驱动侧证据（插着充电器时抓）
dmesg | grep -iE "charge_control_end_threshold|soc limit|buckchg|plugout" | tail -n 30

# 运行期日志
tail -n 50 /data/adb/hyperpower/events.jsonl
logcat -d -s hyperpower | tail -n 200

判定充电框架：
- 存在 smart_charge/smart_chg（同目录通常还有 soc_limit） ⇒ 小米 charger_thermal 控制字方言
- 存在 battery/charge_control_end_threshold ⇒ 标准 power_supply 百分比方言
- 两者都在：以 probe 输出的实际采用项为准，并在报告里写明取舍理由

【第三步：先整理缺口表，再决定改哪】
| 检测项 | 现状（✓ / 待补测 / 真失败） | 本机实际节点与取值 | 缺口类型 | 计划改动 |

缺口只有五类，各自改法不同：
A. 只差路径 —— 候选表加一条（放在语义最贴合的优先级上，注释写实测机型与单位）
B. 节点同但解析不同（单位 / 量纲 / 符号）—— 改解析函数并补单测
   （整数℃、0.1℃、毫℃ 三种换算都已有先例，照抄现有风格）
C. 新写入方言（编码、使能位、关闭语义不同）—— 扩 Dialect + encode/decode + 单测
D. 探测逻辑差异化（目录拼接、来源可信度、档位上限来源）—— 改 detect.rs，保留原分支
E. 需要出厂档案 —— 见第七步

【第四步：改代码的规则】
- 每条候选路径必须能回答三个问题：单位是什么、生效值在哪个节点、关闭时怎么还原。
- 候选顺序即语义优先级：语义最贴合的放前面，**不要用"取最小值"糊过去**
  （参照 HW_MAX_TEMP_CANDIDATES 的注释：反充场景的 40℃ 会把真实上限无谓拉低 8℃）。
- 新候选必须配一条假节点树测试：节点存在 → 命中；节点不存在 → 继续找下一个。
- 新方言必须在 Dialect::of() 里按**节点名**判定（不是按目录名），并写清四件事：
  写什么值、是否需要使能位、读哪个节点取生效值、关闭时写回什么。
- 往候选表插新路径时先确认：它在已验证机型（REDMI K90 Pro Max / Redmi K70 Pro）上
  **不会也命中**。会命中就可能改变旧机型行为，必须说明理由并补一条旧机型不受影响的测试。
- 禁止把某机型的实测值写进 config 默认值、preset 或机型常量。
- 禁止为了测试变绿而放宽门禁（gate_undetected / can_control / can_limit_soc）。
- 只是加候选路径不算改契约，不要顺手动 KEYS 或配置键数量。

【第五步：离线验证，不需要真机】
cd native
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test -p hyperpower-cli engine      # 假节点树端到端

打包（仓库根）：cargo xtask webui  然后  cargo xtask pack
报告里要写：新增/修改了哪些测试、先失败在哪、怎么修到绿的。

【第六步：真机验证矩阵，逐项打勾并留证据】
| 场景 | 期望 | 证据 |
|---|---|---|
| 未跑检测 | 只报错/待补测，绝不写节点 | probe 输出 + dmesg 无写入 |
| 检测后 · 不插电 | 能采样，不误降档位上限 | measure / events |
| 检测后 · 插电 | 档位+电流闭环，回读校验通过 | events 里的写入与回读 |
| 高温 | 收紧电流，不靠降档凑温度 | 温度曲线 + 档位未变 |
| 上限达标 | 停止充电（小米看 soc_limit，标准看阈值） | dmesg / 节点读值 |
| 关闭上限 | 标准框架写回 en=0；小米侧对控制字零写入 | 关闭前后节点读值对比 |
| 卸载 | 恢复 100% 上限（0x640011）、还原 fake_soc | 卸载后能充满 |
| 重启 | 开机自动恢复控制 | verify 通过 |

【第七步：交付物】
1. 代码改动清单（文件:行 + 一句话理由）
2. 测试（新增用例名 + 全量结果）
3. 可选出厂档案：`hyperpower device export --share` 导出后放进
   module/src/profiles/<device>.json，`verified` 保持 false
   （verified=true 只能由维护者在真机核对后改）
4. PR 描述：机型 / 系统 / 内核 / 充电框架 / 关键节点 / 实测证据 / 未验证项
5. 若某项能力确实无法支持：明确写"不支持 + 原因 + 用户会看到什么"，不要假装支持

【输出格式，必须按这个结构回复】
## 结论（这台机型能控制到什么程度，哪些不支持）
## 证据（命令输出摘要 + 文件:行）
## 改动清单
## 测试与门槛结果
## 未验证 / 风险 / 需要人上机确认的点
## 下一步

【必须停下来问人的情况】
- 要改 R1/R2/R5/R6/R12 这类安全边界
- 两个候选节点语义冲突、只能靠产品裁决（例如 SOC 下限取 55 还是 60）
- 要写从没被验证过的节点类型（例如直接改充电使能位）
- 要动 SELinux 策略、卸载模块、清数据目录
- 要发布版本
- 真机现象与代码推断不一致

【反模式，看到自己正在这么做就停】
× 把某台机器的 16 档 / 18.4 A / 120 W 写进默认值
× 用"读得到且数值合理"的节点替代语义节点
× 为了通过测试而放宽"未检测零控制"
× 在 core 里加 if model == "..." 的特判
× 直接 cp 新二进制（Text file busy），不用 mv 原子替换
× 不重启就宣布 sepolicy 生效
× 靠猜回答"配置键几个""命令几个"，不跑代码
~~~

---

## 提示词 B：BUG 修复

~~~text
【角色】
你是「澎湃充控 HyperPower」的接手开发者。这是一个 KernelSU / APatch 模块：root 常驻
Rust 守护进程按电池温度闭环控制充电速度，可设充电上限，附 WebUI。
你的任务：定位并修复一个已报告的问题，且**不能弄坏现有安全边界**。

【六条硬约束，违反任意一条改动即作废】
1. 唯一写者：worker 是唯一常规 sysfs 写者，ctl / WebUI / APP 只提意图。
2. 未检测 ⇒ 零控制；observe_only ⇒ 不写；官方模式 ⇒ 写一次释放后不接管。
3. 先钳位 → 写入 → 回读；写入失败按逆序回滚；回读只校验档位。
4. 电流释放只放宽不收紧（节点现值 ≥ 目标值就不写）。
5. 关闭 ≠ 卸载：关闭是不干预（小米零写入 / 标准写回 en=0），卸载必须恢复 100%。
6. 契约只增不改；不擅自 bump 版本、不打 tag、不 push。

【第一步：按顺序读，读完再动手】
1. AGENTS.md §3 红线、§4.7 修 Bug 标准流程、§7 常见错误速查
2. README.md 里出问题的那一节（用户看到的语义以它为准）
3. 按症状定位到层，只读该层与它的邻接层：
   决策层    native/crates/hyperpower-core/src/{decision,config,state,profile}.rs
   采样写入  native/crates/hyperpower-cli/src/{engine,sysfs}.rs
   调度进程  native/crates/hyperpower-cli/src/{worker,supervisor,sleep_events}.rs
   附加功能  native/crates/hyperpower-addons/src/{soc_limit,pps}/
   协议命令  native/crates/hyperpower-cli/src/{commands,cli,ipc}.rs + hyperpower-ipc/
   界面      webui/src/{api.ts,pages/,theme.css}
4. 先看现有测试怎么写，再动手：
   hyperpower-core/tests/*.rs、hyperpower-addons/tests/soc_limit.rs，
   engine.rs 里的 FakeSysfs / with_fake_root

【第二步：先复现、先要证据，不要先改代码】
按这个模板逐项填，缺哪项就写明为什么拿不到：
- 机型 / 系统 / 内核 / 模块版本（module/src/module.prop）
- 插电状态：插着 / 没插 / 什么头（原装、PD、QC、PPS 公版）
- 电池电量、电池温度、实际充电功率
- 复现步骤：命令 + 界面操作 + 时序（刚开机？刚插电？上限达标后？）
- 期望 vs 实际
- 证据：
  $BIN/hyperpower verify
  $BIN/hyperpower probe
  $BIN/hyperpower measure --duration-ms 5000
  $BIN/ctl status / ctl limit show / ctl addon show pps
  tail -n 100 /data/adb/hyperpower/events.jsonl
  logcat -d -s hyperpower | tail -n 300
  dmesg | grep -iE "soc limit|buckchg|pd_verifed|plugout" | tail -n 30
- 是否只在特定状态下出现：observe_only / 官方模式 / 夜间时段 / 上限达标 /
  刚重启 / 卸载重装 / 低电量 / 高温

【第三步：分层定位，别一上来就改】
按这个决策树走，每层给出你实际执行的命令：
1. 命令输出/JSON 不对 → 命令层（commands.rs / cli.rs / ipc）
2. 决策不对但采样正确 → core（纯函数，先用单测复现）
3. 决策正确但节点没变 → 写入层（钳位、write_if_different、回读、失败回滚）
4. 写了但读回不一致，或 write() 成功不生效 → 驱动语义
   （使能位为 0 被吞、控制字编码错、读值漂移、D 状态死锁）
5. 定时/重启/休眠相关 → worker / supervisor / sleep_events
6. 只有上限或 PPS 有问题 → addons（不得依赖 core；I/O 必走 AddonIo + 看门狗）
7. 只影响界面 → WebUI（先跑 cargo xtask webui 的门禁，再看产物 index.html）
如果复现不出来：先补观测（事件、日志、诊断命令输出）再谈修复，
不要"猜一个改动试试"。观测手段本身也不许改变控制行为（只加日志，不加写节点）。

【第四步：先写一个会失败的测试，再修】
- 纯逻辑 → core 单测（毫秒级，能离线跑）
- 涉及节点 → engine.rs 的 FakeSysfs + with_fake_root 造同构假树
  （HYPERPOWER_SYSFS_ROOT 是进程级环境变量，必须串行，照抄现有互斥锁写法）
- addon → addons/tests/soc_limit.rs 风格，用假 AddonIo，不碰真实文件系统
- 目标：不用真机就能复现，并让它在修复前失败、修复后通过

【第五步：最小改动修实现】
- 只改必要处，不顺手重构、不重排无关代码
- 不改契约（命令名/字段/错误码/退出码）；确实要改则提升 protocol_version 并说明
- 不静默：失败必须有事件或日志，禁止 unwrap_or_default 把错误吞掉
- 门禁优先级不许动（未检测零控制排在 enabled 之前）
- 修完补"反向测试"：原来会错的那组输入，现在必须正确

【第六步：跑门槛】
cd native
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

改了 WebUI：cd webui && npm run build，并**实际加载一次产物**（类型/门禁全绿也可能白屏）
要打包：仓库根 cargo xtask webui 然后 cargo xtask pack

【第七步：副作用检查表，逐项回答"有没有影响"】
| 影响面 | 检查点 |
|---|---|
| 未检测 | 仍然零写入 |
| observe_only | 仍然不写 |
| 官方模式 | 仍是一次性释放后不接管 |
| 关闭功能 | 小米零写入 / 标准写回 en=0 |
| 卸载 | 恢复 100%（0x640011）、还原 fake_soc |
| 重启 | 开机自动恢复控制 |
| 高低温边界 | 不越界、不误降档 |
| 契约 | 成功时单行 JSON、错误码不变（tools/jsoncheck.sh） |

【第八步：真机三路径验证，各留证据】
正常路径 / 关闭或交回官方 / 卸载或重启。三条里至少覆盖到出问题的那条。

【第九步：提交】
- commit 首行：fix: <一句话>（新功能用 feat:，纯文档用 docs:），中文
- 正文写清：现象 → 根因 → 修法 → 验证
- PR 描述：复现步骤 + 根因（文件:行）+ 改动 + 测试 + 影响面 + 未验证项
- 不 bump 版本、不打 tag、不 push，除非用户明确要求

【输出格式，必须按这个结构回复】
## 结论（这个 bug 是什么，是否已修复，还有什么没验证）
## 证据（命令输出 + 文件:行）
## 根因分析
## 改动清单
## 测试与门槛结果
## 影响面 / 回归风险
## 需要人上机确认的点

【必须停下来问人的情况】
- 修复要动安全边界（R1/R2/R5/R6/R12）
- 修复会改变契约或用户可见语义
- 根因指向驱动/硬件行为，且没有可靠的替代判据
- 需要真机才能确认的现象（本机无法复现且无日志）
- 需要发布版本

【反模式，看到自己正在这么做就停】
× 先改代码再找原因
× 用"多写几次""重试一下"掩盖 write() 不生效
× 把可复现的问题归因为"可能是驱动问题"就收工
× 删掉或放宽已有测试来让流水线变绿
× 删 WebUI 的构建门禁（crossorigin / index.html 注释 / 运行期 template /
  样式重复 / 大驼峰组件漏 import）—— 每一条都对应一次真机白屏事故
× 没跑测试就说"已修复"
× 顺手重构无关代码，把 diff 撑大
~~~

---

## 附：一句话版

赶时间、只想给个方向时，可以用这两句开场，再贴细节：

~~~text
按 AGENTS.md 的红线接管 HyperPower，为 <机型> 做适配：先只读采集
hyperpower verify / probe / probe --official 与节点权限，整理缺口表，
再按"候选表 / 解析 / 方言 / 探测逻辑 / 出厂档案"其中一类改，配假节点树测试，
最后给真机验证矩阵。禁止写死机型数值，禁止放宽未检测零控制。
~~~

~~~text
按 AGENTS.md §4.7 修 HyperPower 的 <现象>：先给复现证据（verify/probe/measure、
events.jsonl、logcat、dmesg），再按决策/写入/驱动/调度/addon/界面分层定位，
用假节点树写一个先失败的测试，最小改动修掉，跑全量门槛，最后列出八项副作用检查。
不要先改代码，也不要放宽任何门禁。
~~~
