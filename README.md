# 澎湃充控 HyperPower

[![Release](https://img.shields.io/github/v/release/kino516/HyperPower?style=flat-square&label=release&color=2ea043)](https://github.com/kino516/HyperPower/releases)
[![License](https://img.shields.io/badge/license-GPL--3.0-blue?style=flat-square)](LICENSE)
[![Platform](https://img.shields.io/badge/Android-12%2B-3ddc84?style=flat-square&logo=android&logoColor=white)](#项目速览)
[![ABI](https://img.shields.io/badge/ABI-arm64--v8a-orange?style=flat-square)](#项目速览)
[![Module](https://img.shields.io/badge/KernelSU%20%7C%20APatch-module-8957e5?style=flat-square)](#安装)

> 按电池温度闭环控制充电速度，并可设定充电上限的 **KernelSU / APatch 模块**。
> root 常驻原生守护进程 + WebUI，引擎为 Rust 单二进制。

> [!WARNING]
> **安装 / 更新后必须重启手机。**
>
> 模块内含 `sepolicy.rule`，为充电控制节点放行 SELinux（部分机型上即使 `uid=0`
> 也会因 SELinux 类型不匹配而被拒绝写入）。KernelSU **只在开机 `post-fs-data`
> 阶段应用这个文件**，模块更新不会热加载它。
>
> 不重启的症状：节点探测、界面、检测档案都正常，但**写入失败**
> （报 `Permission denied`），表现为「设置应用了却没生效」。

---

## 目录

- [项目速览](#项目速览)
- [功能](#功能)
- [安装](#安装)
  - [前置条件](#前置条件)
  - [安装步骤](#安装步骤)
  - [升级与卸载](#升级与卸载)
- [快速上手](#快速上手)
- [四模式配置](#四模式配置)
- [夜间模式](#夜间模式)
- [充电上限](#充电上限)
  - [两类机型与两套节点](#两类机型与两套节点)
  - [定期满充](#定期满充)
  - [关闭的语义](#关闭的语义)
  - [系统界面不会同步](#系统界面不会同步)
  - [电量显示可能虚高](#电量显示可能虚高)
  - [常见坑](#常见坑)
  - [与其它模块的冲突](#与其它模块的冲突)
  - [机制是怎么确证的](#机制是怎么确证的)
- [PPS 解锁](#pps-解锁)
  - [重要警示](#重要警示)
  - [两种模式](#两种模式)
  - [伪造金标动画](#伪造金标动画)
  - [真机验证](#真机验证)
  - [实现约束](#实现约束)
- [充电历史](#充电历史)
- [诊断](#诊断)
  - [设备端诊断命令](#设备端诊断命令)
  - [检测结果怎么读](#检测结果怎么读)
- [架构](#架构)
  - [控制链](#控制链)
  - [设计基线](#设计基线)
- [构建](#构建)
  - [环境与工具链](#环境与工具链)
  - [本机一次性配置](#本机一次性配置)
  - [构建时环境变量](#构建时环境变量)
  - [常用命令](#常用命令)
  - [内部设计文档](#内部设计文档)
- [设备验证基线](#设备验证基线)
- [版本历史](#版本历史)
- [贡献](#贡献)
- [许可](#许可)

---

## 项目速览

| 项 | 值 |
|---|---|
| 显示名 | **澎湃充控 HyperPower**（2026-10-20 起；原名「温控充 ThermoCharge」） |
| 模块 id | `a_hyperpower` —— `a_` 前缀让模块在 KernelSU 列表**排序靠前** |
| 数据目录 | `/data/adb/hyperpower`（全新项目，不继承旧 `thermo_charge` 配置） |
| 形态 | root 常驻原生守护进程 + WebUI（APP 由独立仓库维护） |
| 引擎 | Rust（stable）单二进制 |
| 支持基线 | **Android 12（API 31）及以上**，**仅 arm64-v8a** |
| 当前版本 | **v1.1.0**（见[版本历史](#版本历史)） |
| 实现状态 | 后端 / WebUI / 检测 / 多模式 / 充电历史 **全部完成**并真机验收 |
| 许可 | [GPL-3.0](LICENSE) |

---

## 功能

| 功能 | 一句话 | 入口 |
|---|---|---|
| 四模式 | 保守 / 官方 / 极速 / 自定义，四套**互相独立**的配置 | `ctl mode list` |
| 夜间模式 | 指定时段自动切到某模式，时段结束后还原 | `ctl set night.enabled=true` |
| 充电上限 | 充到设定值即停，含定期满充校准与电量校正 | `ctl limit on 90` |
| PPS 解锁 | 让第三方 PPS 充电头也能触发快充，可选金标动画 | `ctl addon on pps` |
| 充电历史 | 每次充电记录温度 / 功率 / 限流值 / 电量曲线 | `ctl history list` |
| 设备自适应 | 节点、档位、功率上限全部靠上设备检测得到 | `hyperpower probe` |

---

## 安装

### 前置条件

| 条件 | 说明 |
|---|---|
| 环境 | **KernelSU** 或 **APatch**（已 root，具备模块安装器） |
| 系统 | **Android 12（API 31）及以上** |
| 架构 | **仅 arm64-v8a**，不保留 32 位扩展位 |
| 安装包 | 从 [Releases](https://github.com/kino516/HyperPower/releases) 下载 `HyperPower-vX.Y.Z.zip` |

> [!NOTE]
> 模块 zip **不含任何 APK**，打包阶段以反向断言强制；配套 APP 由独立仓库维护。
> 模块安装前后都不需要选择机型或节点 —— 一切靠上设备检测。

### 安装步骤

1. 下载最新 `HyperPower-vX.Y.Z.zip`。
2. 打开 KernelSU / APatch 管理器 → **模块** → **从本地安装** → 选择该 zip。
3. **重启手机** —— `sepolicy.rule` 只在开机 `post-fs-data` 阶段生效。
4. 重启后自检（详见[诊断](#诊断)）：

   ```sh
   /data/adb/modules/a_hyperpower/bin/hyperpower verify
   ```

5. 在模块页点「打开」进入 WebUI；未跑检测前模块**不做任何控制**，先跑一次检测。

### 升级与卸载

| 操作 | 做法 | 注意 |
|---|---|---|
| 升级 | 直接覆盖安装新 zip，然后**重启** | 数据目录 `/data/adb/hyperpower` 不受影响 |
| 从旧 id 升级 | **先卸载旧模块**（`thermo_charge`），再装本模块 | 全新项目，不继承旧配置 |
| 卸载 | 先在 WebUI / CLI 关闭充电上限功能，再删模块 | 否则 `fake_soc` 可能残留，电池图标停在最后一次写入的电量 |

> [!IMPORTANT]
> 卸载前若用过充电上限，请先执行 `ctl limit off`。残留的 `fake_soc` 是驱动内存值，
> **重启手机即可恢复**。

> [!NOTE]
> **升级后想刷新检测结果**（例如新版本补了机型适配），执行
> `hyperpower device probe --quick --save`（或 WebUI「更多 → 重新检测」）。
> 本机档案只会自动检测一次，**升级本身不会让它重跑** —— 不重检的话，
> 控制侧仍沿用旧档案里的值（`probe` 会显示新值，但 worker 用的是旧档案）。

---

## 快速上手

设备端常用命令（二进制在 `/data/adb/modules/a_hyperpower/bin`，`ctl` 为其 shim）：

```sh
ctl mode list                 # 四种模式 + 当前生效的是哪个
ctl mode switch turbo         # 切换模式（会保存旧的、装载新的）
ctl limit on 90               # 打开充电上限并设为 90%
ctl limit show                # 目标值、驱动实际生效值、写入次数
ctl set night.enabled=true    # 打开夜间模式
ctl addon on pps              # 开启 PPS 解锁
ctl history list              # 充电会话列表 + 汇总
```

---

## 四模式配置

内建四套**互相独立**的温控配置，切换时各自保存、互不影响：

| 模式 | 含义 |
|---|---|
| **保守** | 低温慢充，最大化电池寿命 |
| **官方** | **模块完全不插手** —— 先解除自己此前加的限制，然后交给系统 |
| **极速** | 尽可能快，温度阈值放宽 |
| **自定义** | 完全由你掌控 |

```sh
ctl mode list              # 看四种模式 + 哪个在生效
ctl mode switch turbo      # 切换（会保存旧的、装载新的）
```

**四套参数全部可自定义**（包含收紧温度、放宽温度、收紧后降到多少、功率上限）。

> [!NOTE]
> 与旧的 `preset`（模板：套用一次就覆盖当前配置）不同，`mode` 是**独立配置**：
> 切走再切回，之前改的值还在。

> [!NOTE]
> 选了「官方」后，控制页的温控项会变灰不可调 —— 那是刻意的：官方模式下模块不参与
> 充电控制，这些参数根本不会被读取。显示的是本机已保存的数值（仅供查看），
> 需要接管时切回其他模式。

---

## 夜间模式

在设定的**本地时间**段内自动切换到指定模式，时段结束后恢复到进入前的模式。
适合「晚上插着电睡觉，希望慢充」的场景。

```sh
ctl set night.enabled=true        # 打开
ctl set night.start=22:00         # 开始（本地时间）
ctl set night.end=07:00           # 结束（早于开始 ⇒ 跨午夜）
ctl set night.mode=conservative   # 时段内使用：conservative / official / turbo / custom
```

| 时刻 | 行为 |
|---|---|
| 到达开始时间 | 记住当前模式 → 切到 `night.mode` |
| 到达结束时间 | 切回进入前的模式 |
| 时段内你**手动**切换模式 | 尊重你的选择：本次时段不再自动恢复 |
| 进入时本来就在目标模式 | 不切换，结束也不恢复 |

> [!NOTE]
> - 开始时间与结束时间相同 ⇒ 视为关闭（不会「全天夜间」）。
> - 时段**跨午夜**受支持（如 22:00 ~ 07:00）。
> - 只在控制进程运行时检查；深度休眠（未充电且没开界面）时不会为了切换而唤醒，
>   插电或打开界面后的第一拍会补上。
> - 夜间状态存在 `night.json`，控制进程重启后仍能恢复进入前的模式。
> - 该功能只是**切换已有四套模式之一**，不新增一套独立温控参数。
> - 出厂默认**关闭**；WebUI 控制页有「夜间模式」卡片（开关 / 时段 / 目标模式）。

---

## 充电上限

设定「充到多少就停」，用于延长电池寿命（长期插电场景尤其有用）。

### 两类机型与两套节点

**自动识别，无需选择** —— 模块会自己探测本机属于哪一类，并采用对应的写入方式：

| 项 | 小米 `charger_thermal` 封装层 | 标准 Linux `power_supply` |
|---|---|---|
| 代表机型 | REDMI K90 Pro Max | Redmi K70 Pro |
| 控制节点 | `smart_charge/smart_chg` | `battery/charge_control_end_threshold` |
| 写入内容 | **控制字**（位域打包） | **纯百分比** |
| 生效值在哪 | 同目录的 `soc_limit` | 控制节点自己 |
| 需要使能位 | 不需要（控制字自带 `0x11`） | **需要** `charge_control_en=1` |

> [!WARNING]
> **使用前提因机型而异。**
>
> **小米封装层机型**：请先在系统设置中打开「电池健康保护」
> —— 设置 → 省电与电池 → 电池保护。本功能写的就是系统「电池保护 / 智能断充」功能的
> 同一个节点，系统开关关着时驱动是否仍执行 SOC 限制**不可依赖**。
>
> **标准 `power_supply` 机型**：与那个系统开关**无关**，无需额外设置。
>
> 界面上会按你的机型显示对应的一条，不必记。

> [!WARNING]
> **未插充电器时看不到效果。** 驱动在未充电时**只记录不执行**
> （dmesg: `adapter is plugout, don't handle soc limit`）。没插电时设了上限看不到变化
> 是正常的 —— 插上充电器后自会生效。

### 怎么用

```sh
ctl limit on 90        # 打开并设定为 90%
ctl limit off          # 关掉（注意：见下方「关闭的语义」）
ctl limit show         # 看状态：目标值、驱动实际生效值、写入次数
```

界面：**控制 → 充电上限**。

可设范围**因机型而异**：检测跑过之后按本机档案来，未跑检测时用通用区间
`55 ~ 100`（下限 55 来自真机驱动报错信息
`Charge control end_threshold should be within [55 100]`）。

### 定期满充

长期把上限压在 80% 左右，电量计（BMS）会逐渐积累误差，偶尔满充一次有助于校准。
本功能可以设置：满足条件时临时把上限放开到 100% 充满一次，充满后自动恢复设定值。

| 条件 | 配置键 | 默认 |
|---|---|---|
| 充入 **N 个完整循环**后满充一次 | `limit.full_after_cycles` | `10`（`0` = 关闭该条件） |
| 每 **N 天**满充一次 | `limit.full_interval_days` | `7`（`0` = 关闭该条件） |
| 总开关 | `limit.full_enabled` | `false` |

两个条件**任一满足**即触发；出厂默认关闭（行为与旧版完全一致）。

> [!IMPORTANT]
> **「完整循环」不是「插一次电源」。** 一次完整循环 = **累计充入 100% 电池电量**。

| 场景 | 计数 |
|---|---|
| 从 60% 充到 80%（一个充电会话） | **0.2 次** |
| 从 20% 充到 80% | 0.6 次 |
| 一天插拔四五次、每次只充一点 | 按实际充入量累加，**不会**每次算一次 |

所以「充 10 次」通常需要十几天到几十天，具体取决于你每天实际充进去多少电。
进度会跨重启保存（`/data/adb/hyperpower/state/soc_limit.json`），卸载时随数据目录清除。

```sh
ctl set limit.full_enabled true       # 打开定期满充
ctl set limit.full_after_cycles 10    # 10 个完整循环满充一次（0 = 关闭）
ctl set limit.full_interval_days 7    # 7 天满充一次（0 = 关闭）
ctl set limit.sync_display false      # 关闭「系统显示 ↔ SOC 电量」同步（默认开）
ctl limit show                        # detail 里有 full_cycles_done / full_pending / full_next_epoch_s
                                      # 以及 sync_display / sync_available / sync_engaged
```

### 关闭的语义

关闭开关**不会**把限制改回 100% —— 手机会继续停在当前上限
（这是刻意的：你可能是想「暂时不管它」，而不是「现在就充满」）。
想恢复满充，把目标调成区间上限即可。

> [!NOTE]
> 例外：标准 `power_supply` 机型上，模块主动打开过 `charge_control_en`，
> 关闭功能时会把它**写回 0**（这个使能位是模块开的，必须交还系统）。
> 小米控制字机型则是真正的零写入。

### 系统界面不会同步

你在系统设置里可能仍看到「80%」，而你设的是 90% —— **这是正常的，不是没生效**。

原因：系统的「智能断充」设置页有一套**自己的界面文案**（写的是它自己记的那个百分比）。
那个显示逻辑在**系统应用里**，而本模块是**纯 root + sysfs，没有 hook 层**，改不了系统界面。

**判断是否生效，以本模块显示的「已生效」为准** —— 它读的是驱动报告的生效值，
那才是真实状态。

### 电量显示可能虚高（两个「电量」不是同一个数）

某些机型上，**电池图标显示的电量和驱动内部用的电量不一致**，而且差得不少：

| 来源 | 节点 | 实测值（K70 Pro） |
|---|---|---|
| 电池图标（系统显示） | `battery/capacity` | **90 ~ 99%** |
| 驱动真实（**SOC 电量**） | `qcom-battery/fg1_rsoc` | **78 ~ 81%** |

而 `charge_control_end_threshold` 是按**驱动真实值**比较的 —— 于是设 95% 时，
驱动会一直充到 `rsoc=95` 才停，那时图标早就 100% 了。

**在哪看这两个数**：

| 位置 | 显示 |
|---|---|
| 状态页「详细读数」 | **SOC 电量**一格（与进度条的「电量」并列） |
| 控制页「充电上限」 | 「系统显示 x% · SOC 电量 y%」+ **电量显示同步**开关 |
| 命令行 | `ctl status` 的 `battery.soc`（系统显示）/ `battery.soc_driver`（SOC 电量） |

> [!NOTE]
> 「SOC 电量」来自电量计节点（`fg1_rsoc` / `rsoc`）。本机没有该节点时显示「—」，
> 不会显示 0%（那是两回事）。

**模块的处理（默认开启，可关）**：把图标校正成驱动真实值（写 `qcom-battery/fake_soc`），
两边永远同一口径：

```text
设 90%  →  写入驱动 90  →  图标显示 90 时停止
```

控制页的「电量显示同步」开关对应配置键 `limit.sync_display`（默认 `true`）。
关闭后模块会**写回 `fake_soc` 的原始值**（不是"停止写入就不管"），图标恢复系统自己的
算法（含校准与平滑）—— 此时两个数可能重新出现偏差，这是你选择的取舍。

关闭充电上限功能时会**自动恢复** `fake_soc` 的原始值。

> [!WARNING]
> 如果你**直接卸载模块**（不先关闭功能），`fake_soc` 可能残留，图标会停在最后一次
> 写入的电量。**重启手机即可恢复**（它是驱动内存值，重启重置）。

### 常见坑

| 坑 | 说明 |
|---|---|
| **别写 `soc_limit`** | 它是驱动的**派生显示值**，直接写会短暂生效后被内部状态机覆盖。**写入用 `smart_chg`，读取用 `soc_limit`。**（仅小米封装层机型有此节点） |
| **别用 `smart_chg` 的读值判断生效** | 它的读值在 `16` / `17` / `80` 之间漂移，与写入值无稳定对应关系 —— 那是驱动内部状态，不是「你设了多少」。 |
| **标准框架机型别忘使能位** | `charge_control_en=0` 时写阈值会**表面成功、实际被吞**（`write()` 返回成功但值不保存）。模块已自动处理；若你手工调试，务必先 `echo 1 > .../charge_control_en`。 |

### 与其它模块的冲突

`smart_chg` / `charge_control_end_threshold` 都是**全局节点** —— 任何写它的程序都在改
同一个开关。如果你同时装了另一个改充电上限的模块（Xposed 模块、其它 KernelSU 模块等），
两者会**互相覆盖**（后写者胜）：你会看到「设置过一会儿自己变回去了」，
却不知道是有两个程序在抢。

**请二选一。** 本模块检测到冲突会提示（配置项 `limit.warn_on_conflict`，默认开）。

### 其他边界

| 场景 | 行为 |
|---|---|
| **未插充电器** | 驱动只记录不执行（dmesg: `adapter is plugout, don't handle soc limit`）。此时「看不到效果」是正常的，**插电后自动生效** |
| **重启** | 该节点是驱动运行时状态，重启即丢 —— 模块会在开机后自动重新写入 |
| **系统覆盖** | 系统「电池保护」组件会在电池状态变化等事件时写回它的默认值，模块按 `limit.verify_s`（默认 15 秒）巡检并写回 |
| **关机充电** | KernelSU 未加载，不受控 —— 属预期 |

### 机制是怎么确证的

全部由**真机对照实验**得出：在同一台手机上、同一个充电器，只改 `smart_chg` 的设定值，
观察驱动日志里的充电使能位：

| 设定 | 电量 | `chg_en` | `ibus` |
|---|---|---|---|
| **90%** | 93% | **`[0][soc_limit]`** —— 禁止充电，原因写明是 SOC 上限 | 0.36 A |
| **100%** | 93% | **`[1]`** —— 允许充电，且立刻进入快充 | **1.59 A** |

⇒ `soc_limit` **真实控制充电通断**，不是「只改显示」。

写回 90% 后驱动立即响应，`ibus` 从 1591 mA 降到 28 mA —— 充电确实被切断，
仅保留极小的待机取电。

> [!TIP]
> 复现方法：插着充电器执行
> `dmesg | grep strategy_buckchg_update_charge_status | tail -3`，
> 其中 `chg_en` 字段直接给出「是否允许充电」及原因。

---

## PPS 解锁

让**支持 PPS 的第三方充电头**也能触发手机快充（绕过 PD 认证），并可选让状态栏显示
「金标」快充动画。

> 移植自 [FreePPS](https://github.com/Xposed-Modules-Repo/cn.buffcow.hypersc)（GPL-3.0），
> 节点写入纪律与广播门控均沿用其真机结论。

### 什么时候有用

| 场景 | 效果 |
|---|---|
| 第三方 **PPS** 充电头（公版，SVID = `0000`） | ✅ 可以解锁到满功率 |
| 小米**原装**头 | ⚠️ **可能变慢**（见下方警示） |
| 普通 PD / QC 头（不支持 PPS） | 无效（本来就不是 PPS） |

### 重要警示

> [!WARNING]
> **可能反而变慢。**
>
> 小米原装充电头走的是**私有协议（MIPPS）**。强制解锁 PPS 后，驱动可能改走标准
> PD/PPS 路径 —— 结果**拿不到私有档位，功率反而下降**。
>
> - 出厂**默认关闭**，需要你主动开启
> - 用原装头且发现变慢 → 关掉本功能，或改用「自动」模式
> - 界面上也常驻这条警示

### 两种模式

| 模式 | 行为 | 建议 |
|---|---|---|
| **锁定**（默认） | 不管插什么头都解锁 | 效果最彻底 |
| **自动** | 只在充电头**本来就是 PPS** 时才解锁 | 保守，不动原装头的私有路径 |

### 怎么用

```sh
ctl addon on pps            # 开启
ctl addon show pps          # 看平台、节点、当前协议、门控状态
ctl addon off pps           # 关闭（拔电时会还原成 0）
ctl set pps.mode auto       # 切到「自动」模式
```

界面：**控制 → PPS 解锁**。

> [!WARNING]
> **改完设置要插拔一次充电器** —— 协议握手在插电那一刻进行。

### 伪造金标动画

插电时若满足**全部四个条件**，会向 SystemUI 发一串广播让状态栏显示 100W 快充图标：

| 条件 | 为什么 |
|---|---|
| 协议是 `PD_PPS` | 非 PPS 头显示金标等于说谎 |
| 认证位已解锁 | 没解锁还喊 100W，用户会觉得被骗 |
| SVID = `0000`（公版头） | 原装头有自己的动画，再发会打架 |
| USB 电压 ≥ 12 V | 5V 档喊「满血 100W」是明显的谎话 |

`ctl addon show pps` 的 `broadcast_reason` 会告诉你**为什么能/不能广播**，不必猜。

### 真机验证

| 机型 | 平台 | 结果 |
|---|---|---|
| Redmi K70 Pro | qcom | 节点命中（`qcom-battery/pd_verifed`），拔电时正确判定「不是 PPS」 |
| REDMI K90 Pro Max | qcom | ✅ **接公版 PPS 头实测通过**：`PD_PPS` / SVID `0000` / 18.9 V / 100 W，插电瞬间广播成功 |

### 实现约束

| # | 约束 | 原因 |
|---|---|---|
| 1 | 充电中**不重复写** `pd_verifed` | 反复写会干扰驱动的协议握手，用户会看到「插上又断开」 |
| 2 | 广播走**子进程**（`spawn_detached`） | 5 条 `am broadcast` 约需 0.5 ~ 1 秒；同步执行会阻塞温控采样近一秒 |
| 3 | 关闭功能时**充电中不还原** | 此刻改 `pd_verifed` 会让驱动重新协商，导致充电中断 |

---

## 充电历史

每次充电自动记录**过程曲线**（温度 / 功率 / 限流值 / 电量），充完归档：

```sh
ctl history list           # 会话列表 + 汇总
ctl history show           # 最近一次的完整曲线
```

界面在「日志」页看曲线图。

> [!NOTE]
> **省闪存**：不是每秒落盘 —— 数据先在内存攒着，每 10 分钟存一次草稿，
> **充完才写一次**归档。一次 2 小时充电约 12KB，默认保留最近 30 次。
>
> 它是**独立观测归档**（`history.json`），不碰 `device.json`、不参与任何决策 ——
> 因此**不违反**「实时数据不落盘」原则（那条原则保护的是设备档案不被实测值污染）。

---

## 诊断

### 设备端诊断命令

排查问题时按这个顺序执行：

```sh
BIN=/data/adb/modules/a_hyperpower/bin

# ① 出了任何问题先跑它 —— 端到端逐项自检，有失败项则非零退出
$BIN/hyperpower verify

# ② 换机型 / 节点没探到时看它 —— 候选表逐条探测 + 未探到功能汇总
$BIN/hyperpower probe

# ③ 想知道「官方在多少度开始降」—— 官方温控表 + 全部节名清单
$BIN/hyperpower probe --official

# ④ 此刻实际充多快（只读，不写档案）
$BIN/hyperpower measure --duration-ms 5000
```

| 命令 | 回答什么问题 | 需要插充电器？ |
|---|---|---|
| `verify` | 整套是否正常工作？哪一环断了？ | 不需要 |
| `probe` | 节点在哪？档位表什么样？哪项没探到？ | 不需要 |
| `probe --official` | 官方策略是什么？它何时先动手？ | 不需要 |
| `measure` | 现在实际功率多少？ | **需要**（否则无意义） |

> [!TIP]
> `verify` 与 `probe` 都是**只读、零副作用**的 —— 排查问题不会制造问题。

### 检测结果怎么读

WebUI「更多 → 检测记录」里两种语气是**分开的**，含义完全不同。这里的「未成功」
与「待补测」不是一回事：

| 标记 | 含义 | 你该做什么 |
|---|---|---|
| **✓** | 测到了 | —— |
| **–**（灰） | **待补测**：当前插电状态不对，换个状态就能测 | 按提示插上/拔掉充电器再跑一次 |
| **✕**（红） | **真的失败**：节点缺失或读取失败 | 查 `hyperpower probe` 的候选表 |

> [!NOTE]
> 有两个检测项的插电要求**正好相反**，所以任何一次检测都必然有一项是「待补测」：
>
> | 项 | 要求 | 原因 |
> |---|---|---|
> | **电流上限** | **必须拔掉充电器** | 充电中该节点反映的是模块自己写入的值，采信会逐次压低上限 |
> | **档位可写上限**（勾选时才测） | **必须插着充电器** | 未插电时驱动不接受档位写入，硬写会误降一档 |
>
> 这不是故障，**不要**把它当成需要修的告警。

---

## 架构

### 控制链

```text
service.sh
  └─ hyperpower supervisor            仅进程监督 + 崩溃恢复（写固定安全值）
       └─ hyperpower worker           采样 / 决策 / 唯一常规 sysfs 写者
            ├─ hyperpower-core        纯决策内核（无 I/O、无时钟、无 sysfs）
            ├─ sysfs 充电节点          先钳位 → 写入 → 回读校验
            └─ UDS                    control.sock（0600 root；CLI / WebUI / APP 共用）
```

### 设计基线

以下决策已锁定：

| ID | 决策 | 含义 |
|---|---|---|
| B1 | **模块 / APP 分离** | APP 由独立仓库维护；模块 zip **不含任何 APK**；打包阶段以反向断言强制 |
| B2 | **支持基线 Android 12+** | 安装脚本门禁 `API ≥ 31`；`module.yaml` 的 `androidMinApi` 与之同步 |
| B3 | **APP 与模块共用同一后端** | 同一 `hyperpower` 二进制、同一协议、同一 JSON 契约；APP 自行申请 root，经 root 通道接入 |
| B4 | **不设工期约束** | 路线以里程碑与判据推进，不为工期牺牲设计 |
| B5 | **仅 arm64-v8a** | 不保留 32 位扩展位 |
| B6 | **不做包签名** | 以逐文件 SHA-256 覆盖完整性校验（签名不影响性能/耗电） |
| B7 | **全新项目** | 不承担历史配置与用户的迁移负担 |

**契约三件套** —— 实现阶段不允许偏离的三份契约，其要点已固化在代码与测试里：

| 契约 | 内容 | 落地位置 |
|---|---|---|
| ① 节点契约表 | 全部可读写节点的路径、方向、取值、语义、写入纪律 | `native/crates/hyperpower-cli/src/sysfs.rs` + 检测流程 |
| ② 配置说明书 | `config.json` 的 **59 个配置键**：类型、范围、默认值、校验规则 | `config_store.rs` 的 `KEYS` 表（单一真源） |
| ③ 接口输出规范 | 命令的 3 种输出形态、错误码与退出码、已依赖细节 | `cli.rs` / `commands.rs` + `hyperpower-ipc` |

> [!TIP]
> **想改代码**：先读 `native/crates/hyperpower-core/src/`（纯决策内核，无 I/O、
> 无时钟、无 sysfs）—— 那是理解本项目最快的地方，所有控制逻辑与边界校验都在那里，
> 且带完整单元测试。

---

## 构建

### 环境与工具链

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

> [!WARNING]
> **两个已确认的本机构建约束**
>
> 1. **必须让 `default-host` 指向 GNU**：`rust-toolchain.toml` 中的 `1.99.0`
>    会以 default host 补全；若为 msvc 则报 `linker 'link.exe' not found`。
> 2. **避开会在本机触发 `dlltool` 的依赖**：`windows-sys 0.61`（被 `clap` /
>    `walkdir` 等引入）在**本机构建**时需用 `dlltool` 生成导入库，而 rustup 精简版在此
>    调用方式下执行失败。因此 `xtask` 刻意不用 `clap` / `walkdir`（手写参数解析 +
>    标准库遍历）。
>    注：`hyperpower-cli` 使用 `clap` 不受影响 —— 它只做交叉编译，不构建 Windows 版。

### 常用命令

```powershell
# 在项目根目录下执行

# 交叉编译
cargo xtask build

# 质量检查（格式 + clippy + 测试）
cargo xtask check

# 构建 WebUI 产物（**必须在 pack 之前**：pack 只复制 module/src，不构建 WebUI）
cargo xtask webui

# 组装模块 zip（含 SHA-256 与 16 KB 对齐检查）
cargo xtask pack

# 推到设备并安装
cargo xtask push
cargo xtask install --via ksud
```

### 内部设计文档

本仓库是**发布版**，只含可构建的源码。开发过程中另有一整套内部设计文档
（架构契约、节点契约表、配置说明书、接口规范、真机验收报告、踩坑记录等约 15 篇），
**不随本仓库发布**。

因此源码注释里会出现形如「见 `docs/03-契约-节点契约表.md`」的引用 —— 那是指向内部文档的
**文字索引**，不是死链；在没有它们的仓库里也不影响理解代码
（关键结论都已写进对应位置的注释）。

---

## 设备验证基线

| 机型 | 系统 | 充电框架 | 验证内容 |
|---|---|---|---|
| REDMI K90 Pro Max（`25102RKBEC` / `myron` / `canoe`） | Android 17（API 37），内核 6.12.69 | 小米 `charger_thermal` 封装层 | 全套功能，KernelSU v3.3.0 |
| Redmi K70 Pro（`23117RK66C` / `manet`） | Android 16（API 36），内核 6.1.138 | 标准 `power_supply` | 节点探测 / 档位上限 / 充电上限（含使能位） |

> [!IMPORTANT]
> **本项目不预置任何机型数据**：档位表、电流上限、硬件保护温度、充电功率上限
> **全部靠上设备检测**得到。上表只是记录「在哪种机器上验证过」，换机型照常工作
> （检测不到时**拒绝控制**，而不是用猜的值去动硬件）。
>
> 两套充电框架的差异（节点名、写入格式、是否需要使能位）已内化进代码的 `Dialect`
> 分流逻辑，**用户无需选择或配置**。

> [!NOTE]
> **v1.1.0 起**，走标准 `power_supply` 框架的 MediaTek 机型也能自动探到两个**只读**温度节点：
> 硬件保护温度 `battery/max_life_temp`、机身温度 `usb/board_temp`
> （小米 17T / `chagall` / MT6899 的只读节点 dump）。该机型**尚未上机验证**，
> 因此不列入上表；未命中这些节点的机型行为完全不变。

---

## 版本历史

| 版本 | 要点 |
|---|---|
| **1.1.0** | 机型适配：MTK / 标准 `power_supply` 框架的温度节点候选（chagall） |
| 1.0.16 | 修复 v1.0.15 的 socket 自愈误判（WebUI 每拍被断开） |
| 1.0.15 | 软重启后模块失效修复 + CI 转绿 + 电量显示同步 |
| 1.0.14 | 充电上限：定期满充（电池校准） |
| 1.0.13 | WebUI：滑块防误触 + 删除顶栏刷新 |
| 1.0.12 | 发布前审计修复：12 高危 + 11 中危 + 19 项新测试 |
| 1.0.10 | 软重启后控制通道自愈 |
| 1.0.9 | 夜间充电模式 + supervisor 自愈 |
| 1.0.7 | 修复交回官方时电流节点只读导致整体失败 |
| 1.0.6 | 修复充电上限达标后电流节点空写振荡 |
| 1.0.5 | 温度门控定稿：电池低于硬件保护温度时全程压制，达到/超过才交给官方 |
| 1.0.4 | 温度门控避免高温协商 |
| 1.0.3 | 电流压流写回 |
| 1.0.2 | 档位压制 + 品牌清理 |
| 1.0.1 | 档位回读修复 |
| 1.0.0 | 首个正式版 |

> 完整变更见 [Releases](https://github.com/kino516/HyperPower/releases)。

---

## 贡献

欢迎提交机型适配与缺陷修复。仓库里有两份给接手者的文档：

| 文档 | 用途 |
|---|---|
| [AGENTS.md](AGENTS.md) | 面向贡献者与 AI 代理的操作手册：仓库地图、R1–R18 安全红线、构建与诊断命令 |
| [PROMPTS.md](PROMPTS.md) | 可直接复制给 AI 代理的提示词：**机型适配**与 **BUG 修复**两套完整流程 |

提交前至少跑通本机门槛：

```bash
cd native
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

机型适配请附 `hyperpower probe` 的原始输出（能用 `hyperpower device export --share`
导出档案更好）。没在真机上验证过的项请标注「未验证」，不要写成已验证 —— 这个仓库
宁可少一条结论，也不要多一条猜测。

---

## 许可

[GNU General Public License v3.0](LICENSE)（GPL-3.0）。

> 选用 GPL-3.0 是因为本项目会**写入充电控制节点** —— 这类直接操作硬件的代码，
> 使用者应当有权查看、审计并改进它。
