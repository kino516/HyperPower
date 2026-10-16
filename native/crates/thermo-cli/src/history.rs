//! 充电历史：逐次充电的**过程曲线**记录
//!
//! # 这个模块解决什么问题
//!
//! 用户想知道「这次充电到底发生了什么」：
//!
//! | 疑问 | 现有数据能否回答 |
//! |---|---|
//! | 温度是怎么涨上去的？ | ❌ `events.jsonl` 只在相位**迁移**时记一条 |
//! | 模块什么时候开始限速、限到多少？ | ❌ `runtime.json` 只有**当前一瞬** |
//! | 这次充进了多少电、平均多大功率？ | ❌ 没有累计 |
//!
//! 因此这里按**充电会话**（一次充/放循环）记录**过程曲线**，
//! 会话结束后归档，供 WebUI 画图与事后复盘。
//!
//! # ⚠️ 与「实时数据不落盘」原则的关系（必读）
//!
//! 项目有一条铁律：**实时数据不落盘**。它的**原意**是 ——
//!
//! > 把「此刻实测到的功率」写进 `device.json`，会被当成**机型能力上限**，
//! > 于是用户设的 43W 被永久钳到 9W（真机踩过，见 `docs/13` §2）。
//!
//! 也就是说，那条原则保护的是**设备档案**（能力边界的权威来源）不被实测值污染。
//!
//! 本模块：
//!
//! | 纪律 | 落实 |
//! |---|---|
//! | **不写 `device.json`** | 独立文件，键空间完全不相交 |
//! | **不参与任何决策** | 决策内核 `thermo-core` 里没有历史的概念 |
//! | **不被决策读取** | 只有命令面（展示）读它 |
//!
//! ⇒ 它是**独立的观测归档**，与档案无关，因此**不违反**该原则。
//!
//! 🚨 **绝对不要**把这里的数据接进 `Bounds` / `Capabilities` / 任何决策输入。
//!
//! # 省闪存（本模块最重要的工程约束）
//!
//! 这是 root 常驻进程，而闪存有写入寿命。项目已有纪律：
//!
//! > 「1~3 秒级巡检若直写 flash，写放大会显著缩短闪存寿命」（`observability.rs`）
//!
//! 本模块的落盘策略：
//!
//! | 时机 | 频率 | 说明 |
//! |---|---|---|
//! | **采集** | 每 30 秒一个点 | 进内存，**同时追一行到草稿**（约 50 字节） |
//! | **归档** | 会话结束**一次** | 把草稿并入历史 |
//!
//! ⚠️ **草稿是"追加写"的（JSONL），不是整份重写** —— 这一点很关键：
//!
//! ```text
//! 初版：每 10 分钟整份重写一次草稿
//!   ⇒ worker 在两次重写之间重启（崩溃 / 手动重启 / 重新部署）
//!   ⇒ 那 10 分钟内的采样点全部丢失
//!   ⇒ 真机实测：应得 35 点只剩 24 点，曲线上出现 109/207 秒两处断档
//!
//! 现在：每采一点就追加一行
//!   ⇒ 零丢失，且写入量比整份重写小约 200 倍
//! ```
//!
//! 一次 2 小时充电的实测量级：
//!
//! ```text
//! 采样点 240 个 × 约 50 字节  ≈ 12 KB（追加写，总共就写这么多）
//! 归档 1 次                    ≈ 12 KB
//! ```
//!
//! 保留次数上限（默认 30）由 [`HistoryStore`] 在归档时修剪。
//!
//! # 抽稀：让存储**有界**
//!
//! 若手机充满后一直插着（`Full` 也算充电中），会话可能持续数天 ——
//! 30 秒一点会累积到上万个点。因此单会话点数上限 [`MAX_POINTS`]，
//! 超出即**隔点抽稀**并把采样间隔翻倍：
//!
//! ```text
//! 30 秒/点 ──抽稀──▶ 60 秒/点 ──抽稀──▶ 120 秒/点 ...
//! ```
//!
//! 这样无论充多久，单次会话的存储都是**常数级**，且曲线形状不失真
//! （抽稀保留偶数索引 + 末点，即均匀降采样）。

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use thermo_core::sample::Sample;

/// 历史文件的 schema 版本
///
/// 用于将来格式演进时判定兼容性（读到时版本不符 ⇒ 当空历史处理，不报错）。
pub const HISTORY_SCHEMA: u32 = 1;

/// 单次会话最多保留的采样点数（超出则抽稀）
///
/// 1000 点 × 30 秒 = 8.3 小时。超过后间隔翻倍，因此：
///
/// | 充电时长 | 采样间隔 | 点数 |
/// |---|---|---|
/// | ≤ 8.3 小时 | 30 秒 | ≤ 1000 |
/// | 16.7 小时 | 60 秒 | 1000 |
/// | 33 小时 | 120 秒 | 1000 |
pub const MAX_POINTS: usize = 1000;

/// 最大可信的采样间隔（秒）
///
/// 相邻两点间隔超过它时，**不计入能量积分** —— 因为那通常意味着
/// 中间发生了休眠/重启（间隔被拉长），把整段时长都按末点功率积分会严重高估。
///
/// 取 10 分钟：正常采样间隔最大 30 秒（抽稀后可能到 120 秒），
/// 10 分钟足以容纳抽稀后的间隔，又能挡住真正的"断档"。
pub const MAX_GAP_S: i64 = 600;

// ============================================================
//  数据模型
// ============================================================

/// 曲线上的一个采样点
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SamplePoint {
    /// Unix 秒
    pub t: i64,
    /// 电池温度（℃）
    pub temp_c: f32,
    /// 实测功率（W）
    pub power_w: f32,
    /// 电量（%）
    pub soc: u8,
    /// 模块当前档位
    pub gear: u8,
    /// 模块当前限流上限（μA）
    ///
    /// ⚠️ 这是**模块写下去的目标值**，不是电池侧实测电流。
    /// 两者一起看才有意义：写入受限但实测功率不掉，说明限制没生效。
    pub limit_ua: i64,
    /// 模块当前状态（`normal` / `tightening` / `hard_protect` / `idle`）
    pub phase: String,
}

/// 一次充电会话
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// 会话 id（= 开始时间戳的字符串形式，天然有序且唯一）
    pub id: String,
    /// 开始时间（Unix 秒）
    pub started_at: i64,
    /// 结束时间（Unix 秒）；`0` = **进行中**
    #[serde(default)]
    pub ended_at: i64,
    /// 开始时的电量（%）
    pub soc_start: u8,
    /// 结束时的电量（%）
    #[serde(default)]
    pub soc_end: u8,
    /// 全程峰值温度（℃）
    #[serde(default)]
    pub peak_temp_c: f32,
    /// 累计充入电量（Wh，梯形法积分）
    #[serde(default)]
    pub energy_wh: f32,
    /// 平均功率（W）= 充入电量 ÷ 时长
    #[serde(default)]
    pub avg_power_w: f32,
    /// 本次会话的**当前**采样间隔（秒）—— 抽稀后会变大
    #[serde(default)]
    pub sample_s: i64,
    /// 曲线采样点
    #[serde(default)]
    pub points: Vec<SamplePoint>,
}

impl Session {
    /// 会话时长（秒）；进行中则为到 `now` 的时长
    pub fn duration_s(&self, now: i64) -> i64 {
        let end = if self.ended_at > 0 {
            self.ended_at
        } else {
            now
        };
        (end - self.started_at).max(0)
    }

    /// 是否仍在进行中（尚未归档）
    pub fn is_running(&self) -> bool {
        self.ended_at == 0
    }

    /// 转成**数字干净**的 JSON（供命令面输出）
    ///
    /// ⚠️ 不要直接 `json!(session)` —— 字段是 `f32`，序列化时会被精确
    /// 展开成 `1.0959999561309814` 这种尾巴。本方法统一在 `f64` 上取整。
    ///
    /// 曲线点也一并处理（否则界面 X 轴/标注上会出现长小数）。
    ///
    /// # `now` 的用途：进行中会话的时长与平均功率
    ///
    /// 这两个值在归档前是**动态**的（`ended_at = 0`）。若直接用字段值，
    /// 界面会看到「时长 0 秒、平均功率 0.0 W」—— 真机踩到过，
    /// 看着像模块坏了。
    pub fn to_json_clean(&self, now: i64) -> Value {
        let points: Vec<Value> = self
            .points
            .iter()
            .map(|p| {
                json!({
                    "t": p.t,
                    "temp_c": clean(p.temp_c, 1),
                    "power_w": clean(p.power_w, 2),
                    "soc": p.soc,
                    "gear": p.gear,
                    "limit_ua": p.limit_ua,
                    "phase": p.phase,
                })
            })
            .collect();

        let dur = self.duration_s(now);
        let avg = if self.is_running() && dur > 0 {
            // 进行中：现算（字段里的 avg_power_w 只有归档时才算）
            f64::from(self.energy_wh) * 3600.0 / dur as f64
        } else {
            f64::from(self.avg_power_w)
        };

        json!({
            "id": self.id,
            "started_at": self.started_at,
            "ended_at": self.ended_at,
            "duration_s": dur,
            "soc_start": self.soc_start,
            "soc_end": self.soc_end,
            "peak_temp_c": clean(self.peak_temp_c, 1),
            "energy_wh": clean(self.energy_wh, 3),
            "avg_power_w": clean_f64(avg, 2),
            "sample_s": self.sample_s,
            "points": points,
        })
    }

    /// 列表项（**不含曲线点** —— 列表页不需要几百个点）
    pub fn to_list_item(&self, now: i64) -> Value {
        json!({
            "id": self.id,
            "started_at": self.started_at,
            "ended_at": self.ended_at,
            "duration_s": self.duration_s(now),
            "soc_start": self.soc_start,
            "soc_end": self.soc_end,
            "peak_temp_c": clean(self.peak_temp_c, 1),
            "energy_wh": clean(self.energy_wh, 3),
            "avg_power_w": clean(self.avg_power_w, 2),
            "points": self.points.len(),
        })
    }

    /// 从曲线点**重算**汇总字段（峰值温度 / 累计电量 / 末点电量）
    ///
    /// # 为什么需要重算
    ///
    /// 恢复草稿时，汇总值（`energy_wh` 等）并没有落盘 ——
    /// 追加写模式下每个点只写自己那一行。这不只是省空间：
    /// **让汇总永远是点的纯函数**，就不可能出现"汇总与曲线对不上"
    /// 这种数据不一致（真机上极难排查）。
    pub fn recompute_totals(&mut self) {
        if self.points.is_empty() {
            return;
        }

        let mut peak = f32::MIN;
        let mut energy = 0.0_f32;
        for (i, p) in self.points.iter().enumerate() {
            if p.temp_c > peak {
                peak = p.temp_c;
            }
            // 与实时累积用**同一套**梯形积分规则，保证"实时看到的"
            // 与"重启后恢复的"数值一致
            if i > 0 {
                let prev = &self.points[i - 1];
                let dt = p.t - prev.t;
                if dt > 0 && dt <= MAX_GAP_S {
                    energy += (prev.power_w + p.power_w) / 2.0 * (dt as f32) / 3600.0;
                }
            }
        }

        self.peak_temp_c = if peak == f32::MIN { 0.0 } else { peak };
        self.energy_wh = energy;
        self.soc_end = self.points.last().map_or(self.soc_start, |p| p.soc);

        let dur_h = (self.points.last().map_or(self.started_at, |p| p.t) - self.started_at).max(1)
            as f32
            / 3600.0;
        self.avg_power_w = self.energy_wh / dur_h;
    }

    /// 截断到指定小数位（让 `f32` 内部状态干净，便于比较与落盘）
    fn round(&mut self) {
        self.peak_temp_c = round1(self.peak_temp_c);
        self.energy_wh = round3(self.energy_wh);
        self.avg_power_w = round2(self.avg_power_w);
        for p in &mut self.points {
            p.temp_c = round1(p.temp_c);
            p.power_w = round2(p.power_w);
        }
    }
}

/// 历史文件根结构
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoreFile {
    schema: u32,
    #[serde(default)]
    sessions: Vec<Session>,
}

impl Default for StoreFile {
    fn default() -> Self {
        Self {
            schema: HISTORY_SCHEMA,
            sessions: Vec::new(),
        }
    }
}

/// **调试模式**下的采样间隔（秒）
///
/// 用户要求（2026-10-17）：调试模式要「历史曲线采样加密（30 秒 → 5 秒）」。
///
/// ⚠️ 它**突破了配置键的下限**（`history.sample_s` 的 min 是 10）——
///    那是有意为之：配置的下限是给**日常**用的保护（防止用户设太小
///    把文件写爆），而调试模式是临时的、用户明确要求更细的粒度。
///
/// 代价：曲线点密 6 倍 ⇒ 一次充电的归档从约 10 KB 涨到约 60 KB。
/// 关掉调试模式后**自动恢复**原间隔（因为是运行时覆盖，没改配置）。
pub const DEBUG_SAMPLE_S: i64 = 5;

/// 历史记录器的运行配置（由配置层注入）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryCfg {
    /// 总开关（关闭 = 不记录，已有会话立即归档）
    pub enabled: bool,
    /// 保留多少次会话（更老的自动删除）
    pub keep: usize,
    /// 采样间隔（秒，原始值；抽稀会在此基础上翻倍）
    pub sample_s: i64,
}

impl Default for HistoryCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            keep: 30,
            sample_s: 30,
        }
    }
}

/// 一拍观察的结果（供调用方记日志，不承担控制语义）
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ObserveOutcome {
    /// 无事发生（未充电且无会话 / 未到采样时刻）
    None,
    /// 会话开始（已记第一个点）
    Started,
    /// 记了一个新采样点
    Sampled,
    /// 会话结束并已归档（携带本次充入电量 Wh）
    ///
    /// ⚠️ 电量**由结束动作返回**，而不是让调用方回头去查 ——
    /// 归档后会话已移出内存，再查询会拿不到。
    Finished(f32),
}

// ============================================================
//  记录器
// ============================================================

/// 充电历史记录器
///
/// # 生命周期
///
/// ```text
/// 插上充电器 → observe() 判定开始 → 写草稿头部
///                                  └─ 每 30s 一个点
///                                       ├─ 进内存
///                                       ├─ 追一行到草稿（JSONL，零丢失）
///                                       └─ 点数超限则抽稀（此时整份重写草稿）
/// 拔掉/停止   → observe() 判定结束 → 归档进 history.json（修剪）→ 删草稿
/// ```
#[derive(Debug)]
pub struct HistoryStore {
    /// 已完成会话的归档文件
    path: PathBuf,
    /// 进行中会话的草稿文件
    draft_path: PathBuf,
    /// 运行配置
    cfg: HistoryCfg,
    /// 当前**生效**的采样间隔（抽稀后翻倍）
    eff_sample_s: i64,
    /// 进行中的会话
    current: Option<Session>,
    /// 上次记点时间
    last_point_at: i64,
}

impl HistoryStore {
    /// 新建
    ///
    /// `path` 为归档文件，`draft_path` 为进行中会话的**追加写草稿**
    /// （JSONL）。两者都**不要求已存在**（首次写入时自动创建）。
    pub fn new(path: PathBuf, draft_path: PathBuf, cfg: HistoryCfg) -> Self {
        Self {
            path,
            draft_path,
            eff_sample_s: cfg.sample_s.max(1),
            cfg,
            current: None,
            last_point_at: 0,
        }
    }

    /// 更新配置（配置热重载时调用）
    ///
    /// 采样间隔变化**不追溯**已有会话（避免曲线出现疏密突变的假象）。
    pub fn configure(&mut self, cfg: HistoryCfg) {
        let changed = cfg != self.cfg;
        self.cfg = cfg;
        if changed && self.current.is_none() {
            // 无进行中会话时才立即生效，避免打断正在记录的曲线
            self.eff_sample_s = cfg.sample_s.max(1);
        }
    }

    /// 当前是否记录中
    pub fn is_recording(&self) -> bool {
        self.current.is_some()
    }

    /// 进行中会话的快照（供 `status` / `history list` 展示）
    ///
    /// ⚠️ **必须在 f64 上取整，不能在 f32 上取整**（真机踩到）：
    ///
    /// ```text
    /// f32: round3(1.096) → 1.096f32 → 序列化成 f64 仍是 1.0959999561309814
    /// ```
    ///
    /// `f32` 根本存不下 1.096，取整只是把它变成"最接近 1.096 的 f32"，
    /// 而 JSON 序列化会把这个 f32 精确展开成 f64。⇒ 界面显示
    /// `充入 1.0959999561309814 Wh`。
    ///
    /// 修法：先转 `f64` 再取整 —— 那时精度足够，输出才是干净的 `1.096`。
    pub fn current_session(&self, now: i64) -> Option<Value> {
        let s = self.current.as_ref()?;
        let dur = s.duration_s(now);
        // 进行中会话的**实时平均功率** = 已充入 ÷ 已耗时
        //
        // ⚠️ 字段 `avg_power_w` 只在归档时才算（那时才有完整时长）。
        //    但界面在充电过程中就会展示它 —— 若直接用它，用户会看到
        //    「平均功率 0.0 W」，像是模块坏了。这里现算一个。
        let avg = if dur > 0 {
            s.energy_wh as f64 * 3600.0 / dur as f64
        } else {
            0.0
        };
        Some(json!({
            "id": s.id,
            "started_at": s.started_at,
            "duration_s": dur,
            "energy_wh": clean(s.energy_wh, 3),
            "avg_power_w": clean_f64(avg, 2),
            "peak_temp_c": clean(s.peak_temp_c, 1),
            "points": s.points.len(),
        }))
    }

    /// 进行中会话的**完整副本**（含曲线点）
    ///
    /// 供 `history show`（未指定 id 时）展示"正在记录的这一条" ——
    /// 它还没进归档文件，只能从内存取。
    pub fn current_session_full(&self) -> Option<Session> {
        self.current.clone()
    }

    /// 启动时恢复草稿（上次运行留下的进行中会话）
    ///
    /// # 为什么恢复而不是直接归档
    ///
    /// worker 重启**不代表充电中断**（崩溃恢复、手动重启都会重启 worker）。
    /// 此时草稿应当继续累积。真正的结束由下一拍 `observe()` 依据
    /// 真实充电状态判定 —— 若已不在充电，它会立即归档。
    ///
    /// # 格式：JSONL（每行一条记录，追加写）
    ///
    /// ```text
    /// {"h":1,"id":"1791897996","started_at":1791897996,"soc_start":56,"sample_s":30}
    /// {"t":1791897996,"temp_c":31.2,"power_w":0.82,"soc":56,"gear":0,...}
    /// {"t":1791898026,...}
    /// ```
    ///
    /// ⚠️ **容错**：最后一行可能是崩溃时写了一半的残行 ——
    /// 无法解析的行**跳过**（而不是整份丢弃）。前面已成功落盘的点
    /// 正是我们最想保住的东西。
    ///
    /// 返回是否恢复了草稿。
    pub fn restore_draft(&mut self) -> bool {
        // 顺手清掉老版本（整份 JSON）留下的文件，避免它一直占地方
        let legacy = crate::paths::history_draft_json_legacy();
        if legacy.exists() {
            std::fs::remove_file(&legacy).ok();
            log::info!("已清理旧格式的充电历史草稿（{}）", legacy.display());
        }

        let Ok(text) = std::fs::read_to_string(&self.draft_path) else {
            return false;
        };

        let mut header: Option<Value> = None;
        let mut points: Vec<SamplePoint> = Vec::new();
        let mut bad = 0usize;

        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                // 残行 / 坏行：跳过（不丢整份）
                bad += 1;
                continue;
            };
            if v.get("h").is_some() {
                header = Some(v);
            } else if let Ok(mut p) = serde_json::from_value::<SamplePoint>(v) {
                // ⚠️ 恢复时也取整一次
                //
                // 旧版本写的点带完整 f32 精度（如 `0.48896`），
                // 若原样载入内存，就会出现"内存 0.48896 / 界面 0.49"
                // 的不一致 —— 与"记录时即取整"的初衷矛盾。
                // 这是格式迁移的一次性清理，之后写入的点本就干净。
                p.temp_c = round1(p.temp_c);
                p.power_w = round2(p.power_w);
                points.push(p);
            } else {
                bad += 1;
            }
        }

        let Some(h) = header else {
            // 连头都没有 ⇒ 无用，删掉
            if bad > 0 {
                log::warn!("充电历史草稿无有效头部（{bad} 行无法解析），已丢弃");
            }
            std::fs::remove_file(&self.draft_path).ok();
            return false;
        };
        if points.is_empty() {
            // 刚开完头就崩了 ⇒ 没有价值，删掉
            std::fs::remove_file(&self.draft_path).ok();
            return false;
        }
        if bad > 0 {
            log::warn!(
                "充电历史草稿有 {bad} 行损坏（已跳过，其余 {} 点保留）",
                points.len()
            );
        }

        let started_at = h.get("started_at").and_then(Value::as_i64).unwrap_or(0);
        let sample_s = h
            .get("sample_s")
            .and_then(Value::as_i64)
            .unwrap_or(30)
            .max(1);

        // 汇总字段**从点重算** —— 不信任草稿里的中间值
        // （追加写模式下那些值本来也没落盘）
        let mut s = Session {
            id: h
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| started_at.to_string()),
            started_at,
            ended_at: 0,
            soc_start: h.get("soc_start").and_then(Value::as_u64).unwrap_or(0) as u8,
            soc_end: points.last().map_or(0, |p| p.soc),
            peak_temp_c: 0.0,
            energy_wh: 0.0,
            avg_power_w: 0.0,
            sample_s,
            points,
        };
        s.recompute_totals();

        self.eff_sample_s = sample_s;
        self.last_point_at = s.points.last().map_or(started_at, |p| p.t);
        self.current = Some(s);
        true
    }

    /// 每拍观察一次（**唯一的写入口**）
    ///
    /// # 会话边界
    ///
    /// | 上拍 | 本拍 | 动作 |
    /// |---|---|---|
    /// | 未充电 | 充电中 | **开始**新会话（立即记首点） |
    /// | 充电中 | 充电中 | 到采样间隔就记一点 |
    /// | 充电中 | 未充电 | **结束**并归档 |
    /// | 未充电 | 未充电 | 什么都不做 |
    ///
    /// ⚠️ 任何 I/O 失败都**只记日志、不中断业务** ——
    /// 与 `EventLog` 同一纪律：观测手段的故障不该拖垮控制。
    #[allow(clippy::too_many_arguments)]
    pub fn observe(
        &mut self,
        now: i64,
        sample: &Sample,
        gear: u8,
        limit_ua: i64,
        phase: &str,
    ) -> ObserveOutcome {
        // 总开关关闭：若正在记录，立即收尾归档（不留半截数据）
        if !self.cfg.enabled {
            if self.current.is_some() {
                let e = self.finish(now, sample.soc);
                return ObserveOutcome::Finished(e);
            }
            return ObserveOutcome::None;
        }

        let active = sample.status.is_active();

        match (active, self.current.is_some()) {
            (true, false) => {
                self.start(now, sample, gear, limit_ua, phase);
                ObserveOutcome::Started
            }
            (true, true) => self.maybe_point(now, sample, gear, limit_ua, phase),
            (false, true) => {
                let e = self.finish(now, sample.soc);
                ObserveOutcome::Finished(e)
            }
            (false, false) => ObserveOutcome::None,
        }
    }

    /// 开始一次新会话
    fn start(&mut self, now: i64, sample: &Sample, gear: u8, limit_ua: i64, phase: &str) {
        let s = Session {
            id: now.to_string(),
            started_at: now,
            ended_at: 0,
            soc_start: sample.soc,
            soc_end: sample.soc,
            peak_temp_c: sample.temp_c,
            energy_wh: 0.0,
            avg_power_w: 0.0,
            sample_s: self.cfg.sample_s.max(1),
            points: Vec::new(),
        };
        self.eff_sample_s = s.sample_s;
        self.current = Some(s);

        // 先落**头部**（会话身份），再记首点
        //
        // ⚠️ 顺序很重要：若先写点后写头，中间崩溃会得到一个
        //    "有点但无头"的草稿 —— 恢复时无法知道会话何时开始。
        self.write_draft_header();

        // 首点立即记 —— 否则插入瞬间的状态要等一个采样间隔才出现
        self.push_point(now, sample, gear, limit_ua, phase);
    }

    /// 到点则记一个采样点
    fn maybe_point(
        &mut self,
        now: i64,
        sample: &Sample,
        gear: u8,
        limit_ua: i64,
        phase: &str,
    ) -> ObserveOutcome {
        if now - self.last_point_at < self.eff_sample_s {
            return ObserveOutcome::None;
        }
        self.push_point(now, sample, gear, limit_ua, phase);
        self.thin_if_needed();
        ObserveOutcome::Sampled
    }

    /// 追加一个点，并更新累计量
    ///
    /// # 为什么在**记录时**就取整
    ///
    /// 采样值取整到「温度 0.1℃ / 功率 0.01W」再存。这样：
    ///
    /// | 好处 | 说明 |
    /// |---|---|
    /// | 文件可读 | 草稿与归档里不会出现 `0.045888` 这种尾巴 |
    /// | **三者一致** | 内存里的点 ≡ 草稿里的点 ≡ 界面显示的点 |
    /// | 重算自洽 | 重启后 `recompute_totals` 与实时积分得到**同一个数** |
    ///
    /// 代价是精度损失（0.01W 量级），相对误差可忽略 ——
    /// 若把原始值存进点、只在输出时取整，重启前后同一段曲线会算出
    /// 两个略有差异的能量值，那种"对不上"更难排查。
    fn push_point(&mut self, now: i64, sample: &Sample, gear: u8, limit_ua: i64, phase: &str) {
        let power = round2(sample.power_w());
        let temp = round1(sample.temp_c);

        let Some(s) = self.current.as_mut() else {
            return;
        };

        // ① 能量积分（梯形法：用相邻两点的功率均值 × 时间差）
        //
        // 为什么用梯形而不是矩形：
        //   采样间隔内功率通常单调变化（升温降流/降温回升），
        //   取两端平均比取单端更接近真实积分值，且不会系统性偏高或偏低。
        //
        // 为什么超过 MAX_GAP_S 就不计：
        //   间隔被拉长（休眠/重启）时，末点功率无法代表整段 ——
        //   强行积分会严重高估（例：10 分钟按 30W 计 = 5 Wh，实际可能只有几百 mWh）。
        //
        // ⚠️ 这里用的是**取整后**的 `power`（与 `prev.power_w` 同一精度），
        //    保证与 `recompute_totals` 的结果逐位一致。
        if let Some(prev) = s.points.last() {
            let dt = now - prev.t;
            if dt > 0 && dt <= MAX_GAP_S {
                let avg_p = (prev.power_w + power) / 2.0;
                s.energy_wh += avg_p * (dt as f32) / 3600.0;
            }
        }

        // ② 峰值温度与末点电量
        if temp > s.peak_temp_c {
            s.peak_temp_c = temp;
        }
        s.soc_end = sample.soc;

        // ③ 落点
        s.points.push(SamplePoint {
            t: now,
            temp_c: temp,
            power_w: power,
            soc: sample.soc,
            gear,
            limit_ua,
            phase: phase.to_string(),
        });

        self.last_point_at = now;

        // ⭐ 每个点**立即追加**到草稿（约 50 字节，不是整份重写）
        //
        // 这是「worker 重启不丢点」的关键：初版每 10 分钟整份重写，
        // 重启就丢掉最后一个间隔内的所有点（真机实测：35 点只剩 24 点）。
        let point = s.points.last().cloned();
        self.append_point_to_draft(point.as_ref());
    }

    /// 点数超限则抽稀（保留偶数索引 + 末点），并把采样间隔翻倍
    ///
    /// 抽稀后曲线形状不变（均匀降采样），但存储回到常数级 ——
    /// 这是「充满后一直插着数天」这种情况的安全阀。
    ///
    /// ⚠️ 抽稀后必须**整份重写草稿**：追加写模式下，旧的点已经在文件里，
    /// 无法"就地删掉一半"。这是唯一需要整份重写的场合，而它极少发生
    /// （1000 点 = 8.3 小时才触发一次，且随后间隔翻倍）。
    fn thin_if_needed(&mut self) {
        let new_interval = {
            let Some(s) = self.current.as_mut() else {
                return;
            };
            if s.points.len() <= MAX_POINTS {
                return;
            }
            let n = s.points.len();
            let mut kept = Vec::with_capacity(n / 2 + 1);
            for (i, p) in s.points.iter().enumerate() {
                // 偶数索引 + 最后一个（保证曲线两端都在）
                if i % 2 == 0 || i == n - 1 {
                    kept.push(p.clone());
                }
            }
            s.points = kept;
            s.sample_s = s.sample_s.saturating_mul(2);
            s.sample_s
        };
        self.eff_sample_s = new_interval;
        log::info!("充电历史已抽稀：采样间隔调整为 {new_interval}s");
        self.rewrite_draft();
    }

    /// 结束当前会话并归档
    ///
    /// `soc` 是**结束那一刻**的实测电量 —— 它比最后一个采样点的值更准
    /// （拔掉充电器与最后一次采样之间可能还有几十秒）。
    ///
    /// 返回本次充入电量（Wh）；无进行中会话时为 `0.0`。
    fn finish(&mut self, now: i64, soc: u8) -> f32 {
        let Some(mut s) = self.current.take() else {
            return 0.0;
        };
        s.ended_at = now;
        s.soc_end = soc;

        // 补算最后一个点到结束时刻的能量
        //
        // 否则「最后一段」永远缺失：拔掉充电器那一刻的功率不会被计入。
        // 同样受 MAX_GAP_S 约束（防止把休眠时长算进去）。
        if let Some(last) = s.points.last() {
            let dt = now - last.t;
            if dt > 0 && dt <= MAX_GAP_S {
                s.energy_wh += last.power_w * (dt as f32) / 3600.0;
            }
        }

        // 平均功率 = 充入电量 ÷ 时长
        let dur_h = (s.ended_at - s.started_at).max(1) as f32 / 3600.0;
        s.avg_power_w = s.energy_wh / dur_h;
        s.round();

        // ⚠️ 先取值再 move（归档会消费 `s`）
        let energy = s.energy_wh;

        if let Err(e) = self.archive(s) {
            log::warn!("充电历史归档失败：{e:#}");
        }
        std::fs::remove_file(&self.draft_path).ok();
        energy
    }

    /// 写入归档文件（追加 + 修剪）
    fn archive(&self, s: Session) -> Result<()> {
        let mut store = self.read_store();
        store.sessions.push(s);

        // 修剪：按开始时间排序，只留最近 `keep` 次
        //
        // 排序的意义：即使因时间跳变导致写入顺序错乱，修剪结果依然确定。
        store.sessions.sort_by_key(|x| x.started_at);
        if store.sessions.len() > self.cfg.keep {
            let drop_n = store.sessions.len() - self.cfg.keep;
            store.sessions.drain(..drop_n);
        }

        self.write_store(&store)
    }

    /// 读归档文件（不存在或损坏 ⇒ 空历史，不报错）
    ///
    /// 为什么不报错：历史是**观测数据**，损坏了不该让归档失败，
    /// 更不该让调用方（主循环）出错。
    fn read_store(&self) -> StoreFile {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return StoreFile::default();
        };
        match serde_json::from_str::<StoreFile>(&text) {
            Ok(s) if s.schema == HISTORY_SCHEMA => s,
            Ok(s) => {
                log::warn!(
                    "充电历史 schema 不符（{} ≠ {HISTORY_SCHEMA}），按空历史处理",
                    s.schema
                );
                StoreFile::default()
            }
            Err(e) => {
                log::warn!("充电历史解析失败（按空历史处理）：{e}");
                StoreFile::default()
            }
        }
    }

    /// 原子写入归档文件（临时文件 → rename）
    fn write_store(&self, store: &StoreFile) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建目录失败：{}", parent.display()))?;
        }
        let text = serde_json::to_string(store).context("序列化充电历史失败")?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, text.as_bytes())
            .with_context(|| format!("写入临时文件失败：{}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("替换历史文件失败：{}", self.path.display()))?;
        Ok(())
    }

    // ============================================================
    //  草稿（JSONL，追加写）
    // ============================================================
    //
    //  # 为什么要追加写而不是整份重写
    //
    //  初版每次落草稿都把整个会话（点数组 + 汇总）序列化后整份重写。
    //  真机暴露了一个**数据丢失**缺陷：
    //
    //  ```text
    //  草稿每 10 分钟才重写一次
    //  ⇒ worker 在两次重写之间重启（崩溃 / 手动重启 / 重新部署）
    //  ⇒ 那 10 分钟内采到的点**全部丢失**
    //  ```
    //
    //  实测：一次充电本应 35 个点，重启两次后只剩 24 点，
    //  曲线上出现 109 秒与 207 秒两处断档。
    //
    //  改为追加写后：每采一点就追加约 50 字节，**零丢失**，
    //  且写入量比整份重写（约 12 KB）小 200 倍。
    //
    //  # 格式
    //
    //  ```text
    //  第 1 行（头部）：{"h":1,"id":"…","started_at":…,"soc_start":…,"sample_s":…}
    //  第 2 行起（点）：{"t":…,"temp_c":…,"power_w":…,"soc":…,"gear":…,"limit_ua":…,"phase":"…"}
    //  ```
    //
    //  汇总字段（峰值 / 累计电量）**不入草稿** —— 它们由点重算
    //  （见 [`Self::recompute_totals`]），所以永远与曲线自洽。

    /// 写草稿头部（会话开始时一次）
    fn write_draft_header(&self) {
        let Some(s) = self.current.as_ref() else {
            return;
        };
        let result = (|| -> Result<()> {
            if let Some(parent) = self.draft_path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            // 覆盖写（不是追加）—— 新会话必须丢掉上一次的残留
            let header = json!({
                "h": 1,
                "id": s.id,
                "started_at": s.started_at,
                "soc_start": s.soc_start,
                "sample_s": s.sample_s,
            });
            let mut text = serde_json::to_string(&header).context("序列化草稿头失败")?;
            text.push('\n');
            std::fs::write(&self.draft_path, text.as_bytes())
                .with_context(|| format!("写草稿头失败：{}", self.draft_path.display()))?;
            Ok(())
        })();
        if let Err(e) = result {
            log::warn!("充电历史草稿头写入失败：{e:#}");
        }
    }

    /// 追加一个点到草稿
    fn append_point_to_draft(&self, point: Option<&SamplePoint>) {
        let Some(p) = point else {
            return;
        };
        let result = (|| -> Result<()> {
            if let Some(parent) = self.draft_path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            let mut text = serde_json::to_string(p).context("序列化采样点失败")?;
            text.push('\n');
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.draft_path)
                .with_context(|| format!("打开草稿失败：{}", self.draft_path.display()))?;
            std::io::Write::write_all(&mut f, text.as_bytes()).context("追加采样点失败")?;
            Ok(())
        })();
        if let Err(e) = result {
            // 与 EventLog 同纪律：观测失败不拖垮控制
            log::warn!("充电历史草稿追加失败：{e:#}");
        }
    }

    /// 整份重写草稿（**仅抽稀后调用**）
    ///
    /// 追加写模式下无法"就地删掉一半的点"，所以抽稀后必须重写。
    ///
    /// 用「临时文件 + rename」保证原子性：直接覆盖的话，
    /// 写到一半崩溃就会留下半份草稿 —— 虽然恢复端能容忍（逐行跳过），
    /// 但那会丢掉后半段的点。
    fn rewrite_draft(&self) {
        let Some(s) = self.current.as_ref() else {
            return;
        };
        let result = (|| -> Result<()> {
            let mut text = String::new();
            let header = json!({
                "h": 1,
                "id": s.id,
                "started_at": s.started_at,
                "soc_start": s.soc_start,
                // ⚠️ 抽稀后 sample_s 变了，必须更新头部 ——
                //    否则重启恢复后采样间隔会退回旧值，曲线从此刻起变密
                "sample_s": s.sample_s,
            });
            text.push_str(&serde_json::to_string(&header).context("序列化草稿头失败")?);
            text.push('\n');
            for p in &s.points {
                text.push_str(&serde_json::to_string(p).context("序列化采样点失败")?);
                text.push('\n');
            }

            let tmp = self.draft_path.with_extension("jsonl.tmp");
            std::fs::write(&tmp, text.as_bytes())
                .with_context(|| format!("写草稿失败：{}", tmp.display()))?;
            std::fs::rename(&tmp, &self.draft_path)
                .with_context(|| format!("替换草稿失败：{}", self.draft_path.display()))?;
            Ok(())
        })();
        if let Err(e) = result {
            log::warn!("充电历史草稿重写失败：{e:#}");
        }
    }

    // ── 读取接口（命令面用） ──

    /// 全部已完成会话，**按开始时间倒序**（最新在前）
    pub fn load_sessions(&self) -> Vec<Session> {
        let mut v = self.read_store().sessions;
        v.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        v
    }

    /// 按 id 取单次会话
    pub fn load_session(&self, id: &str) -> Option<Session> {
        self.read_store().sessions.into_iter().find(|s| s.id == id)
    }

    /// 清空全部历史（含草稿）
    ///
    /// 返回被删除的会话数。
    pub fn clear(&self) -> Result<usize> {
        let n = self.read_store().sessions.len();
        if self.path.exists() {
            std::fs::remove_file(&self.path)
                .with_context(|| format!("删除历史文件失败：{}", self.path.display()))?;
        }
        // 草稿只在**未被当前会话占用**时才删 —— 否则会打断正在记录的会话
        if self.current.is_none() {
            std::fs::remove_file(&self.draft_path).ok();
        }
        Ok(n)
    }

    /// 汇总统计（供列表页顶部展示）
    ///
    /// # 只统计**已归档**的会话
    ///
    /// 进行中的那一次单独用 `recording: true` + `current_session()` 表达。
    /// 理由：把半截数据混进"累计充入"会让数字在充电过程中不断跳动。
    pub fn stats(&self) -> Value {
        let all = self.load_sessions();
        let total: f64 = all.iter().map(|s| f64::from(s.energy_wh)).sum();
        let peak = all
            .iter()
            .map(|s| f64::from(s.peak_temp_c))
            .fold(f64::MIN, f64::max);
        json!({
            "count": all.len(),
            "energy_total_wh": clean_f64(total, 1),
            "peak_temp_c": if all.is_empty() { 0.0 } else { clean_f64(peak, 1) },
            "keep": self.cfg.keep,
            "enabled": self.cfg.enabled,
            "sample_s": self.cfg.sample_s,
            // 此刻是否正在记录一次充电（供界面显示"记录中"徽标）
            "recording": self.is_recording(),
        })
    }
}

// ============================================================
//  小工具
// ============================================================

/// 保留 1 位小数（用于内部 f32 状态；**输出到 JSON 请用 [`clean`]**）
fn round1(x: f32) -> f32 {
    let r = (x * 10.0).round() / 10.0;
    if r == 0.0 {
        0.0
    } else {
        r
    }
}

/// 保留 2 位小数（用于内部 f32 状态）
fn round2(x: f32) -> f32 {
    let r = (x * 100.0).round() / 100.0;
    if r == 0.0 {
        0.0
    } else {
        r
    }
}

/// 保留 3 位小数（用于内部 f32 状态）
fn round3(x: f32) -> f32 {
    let r = (x * 1000.0).round() / 1000.0;
    if r == 0.0 {
        0.0
    } else {
        r
    }
}

/// 输出到 JSON 的浮点：**在 f64 上取整**，得到干净数字
///
/// 实现统一在 [`crate::observability::round_json`]（**只留一份**）——
/// 项目有过"同一规则写两遍导致分叉"的教训，输出规范不应各写各的。
///
/// # 为什么不能在 f32 上取整（真机踩到）
///
/// ```text
/// f32 存不下 1.096
///   ⇒ round3(1.096f32) 只是得到"最接近 1.096 的 f32"
///   ⇒ JSON 序列化把它精确展开成 f64：1.0959999561309814
///   ⇒ 界面显示「充入 1.0959999561309814 Wh」
/// ```
fn clean(x: f32, digits: u32) -> f64 {
    crate::observability::round_json_f32(x, digits)
}

/// [`clean`] 的 f64 版本
fn clean_f64(x: f64, digits: u32) -> f64 {
    crate::observability::round_json(x, digits)
}

/// 让 `Phase` 变成曲线里的短字符串
pub fn phase_str(p: thermo_core::state::Phase) -> &'static str {
    match p {
        thermo_core::state::Phase::Idle => "idle",
        thermo_core::state::Phase::Normal => "normal",
        thermo_core::state::Phase::Tightening => "tightening",
        thermo_core::state::Phase::HardProtect => "hard_protect",
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use thermo_core::sample::ChargingStatus;

    /// 造一个采样点
    fn sample(temp_c: f32, power_w: f32, soc: u8, status: ChargingStatus) -> Sample {
        // power = I × U ⇒ 给定功率(3.9V)反推电流
        let voltage_uv = 3_900_000_i32;
        let current_ua = ((power_w as f64) / 3.9 * 1e6) as i32;
        Sample {
            temp_c,
            virt_c: None,
            current_ua,
            voltage_uv,
            soc,
            status,
        }
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "thermo_hist_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn store(tag: &str, cfg: HistoryCfg) -> HistoryStore {
        let d = tmp_dir(tag);
        HistoryStore::new(d.join("history.json"), d.join("history.current.json"), cfg)
    }

    // ── 会话边界 ──

    #[test]
    fn 未充电时零动作() {
        let mut h = store("idle", HistoryCfg::default());
        let s = sample(30.0, 0.0, 50, ChargingStatus::Discharging);
        assert_eq!(
            h.observe(1000, &s, 0, 0, "idle"),
            ObserveOutcome::None,
            "未充电不得开始会话"
        );
        assert!(!h.is_recording());
    }

    #[test]
    fn 插入充电器即开始并记首点() {
        let mut h = store("start", HistoryCfg::default());
        let s = sample(35.0, 30.0, 50, ChargingStatus::Charging);
        assert_eq!(
            h.observe(1000, &s, 0, 9_000_000, "normal"),
            ObserveOutcome::Started
        );
        assert!(h.is_recording());

        let cur = h.current.as_ref().unwrap();
        assert_eq!(cur.started_at, 1000);
        assert_eq!(cur.points.len(), 1, "首点必须立即记下");
        assert_eq!(cur.soc_start, 50);
        assert_eq!(cur.points[0].temp_c, 35.0);
    }

    #[test]
    fn 充满也算充电中() {
        let mut h = store("full", HistoryCfg::default());
        let s = sample(35.0, 10.0, 100, ChargingStatus::Full);
        assert_eq!(
            h.observe(1000, &s, 0, 500_000, "normal"),
            ObserveOutcome::Started
        );
    }

    #[test]
    fn 拔掉充电器即归档() {
        let mut h = store("finish", HistoryCfg::default());
        let on = sample(35.0, 30.0, 50, ChargingStatus::Charging);
        let off = sample(35.0, 0.0, 60, ChargingStatus::Discharging);

        h.observe(1000, &on, 0, 9_000_000, "normal");
        h.observe(1030, &on, 0, 9_000_000, "normal");
        assert!(matches!(
            h.observe(3600, &off, 0, 0, "idle"),
            ObserveOutcome::Finished(_)
        ));
        assert!(!h.is_recording(), "归档后不应再有进行中会话");

        let all = h.load_sessions();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].started_at, 1000);
        assert_eq!(all[0].ended_at, 3600);
        assert_eq!(all[0].soc_end, 60);
    }

    // ── 采样节流 ──

    #[test]
    fn 未到采样间隔不记点() {
        let mut h = store("throttle", HistoryCfg::default());
        let s = sample(35.0, 30.0, 50, ChargingStatus::Charging);
        h.observe(1000, &s, 0, 9_000_000, "normal"); // 首点
        assert_eq!(
            h.observe(1010, &s, 0, 9_000_000, "normal"),
            ObserveOutcome::None
        );
        assert_eq!(
            h.observe(1029, &s, 0, 9_000_000, "normal"),
            ObserveOutcome::None
        );
        assert_eq!(
            h.observe(1030, &s, 0, 9_000_000, "normal"),
            ObserveOutcome::Sampled
        );
        assert_eq!(h.current.as_ref().unwrap().points.len(), 2);
    }

    #[test]
    fn 采样间隔随配置生效() {
        let mut h = store(
            "cfg_interval",
            HistoryCfg {
                enabled: true,
                keep: 30,
                sample_s: 60,
            },
        );
        let s = sample(35.0, 30.0, 50, ChargingStatus::Charging);
        h.observe(1000, &s, 0, 9_000_000, "normal");
        assert_eq!(
            h.observe(1059, &s, 0, 9_000_000, "normal"),
            ObserveOutcome::None
        );
        assert_eq!(
            h.observe(1060, &s, 0, 9_000_000, "normal"),
            ObserveOutcome::Sampled
        );
    }

    // ── 能量积分 ──

    #[test]
    fn 能量按梯形法积分() {
        let mut h = store("energy", HistoryCfg::default());
        // 30W 恒定 3600 秒 = 30 Wh
        let s = sample(35.0, 30.0, 50, ChargingStatus::Charging);
        h.observe(0, &s, 0, 9_000_000, "normal");
        for t in 1..=120 {
            h.observe(t * 30, &s, 0, 9_000_000, "normal");
        }
        let e = h.current.as_ref().unwrap().energy_wh;
        // 120 个间隔 × 30 秒 = 3600 秒
        assert!((e - 30.0).abs() < 0.5, "期望约 30Wh，实得 {e}");
    }

    #[test]
    fn 变功率时取两端均值() {
        let mut h = store("energy_trap", HistoryCfg::default());
        // 0W → 100W，30 秒：梯形法应得 (0+100)/2 × 30/3600 = 0.417Wh
        h.observe(
            0,
            &sample(35.0, 0.0, 50, ChargingStatus::Charging),
            0,
            0,
            "normal",
        );
        h.observe(
            30,
            &sample(35.0, 100.0, 50, ChargingStatus::Charging),
            0,
            0,
            "normal",
        );
        let e = h.current.as_ref().unwrap().energy_wh;
        assert!((e - 0.4167).abs() < 0.01, "梯形法应取两端均值，实得 {e}");
    }

    #[test]
    fn 断档间隔不计入能量() {
        let mut h = store("gap", HistoryCfg::default());
        let s = sample(35.0, 30.0, 50, ChargingStatus::Charging);
        h.observe(0, &s, 0, 9_000_000, "normal"); // 首点
        h.observe(30, &s, 0, 9_000_000, "normal"); // 30 秒 → 记点并积分

        let after_normal = h.current.as_ref().unwrap().energy_wh;
        assert!(
            (after_normal - 0.25).abs() < 0.01,
            "30 秒 × 30W 应为 0.25Wh，实得 {after_normal}"
        );

        // 中间断档（超过 MAX_GAP_S）⇒ 该段不得计入
        // 否则休眠/重启被拉长的间隔会按末点功率整段积分，严重高估
        let far = 30 + MAX_GAP_S + 100;
        h.observe(far, &s, 0, 9_000_000, "normal");

        let after_gap = h.current.as_ref().unwrap().energy_wh;
        assert!(
            (after_gap - after_normal).abs() < 0.001,
            "断档段不得计入能量（{after_normal} → {after_gap}）"
        );
    }

    // ── 抽稀 ──

    #[test]
    fn 点数超限触发抽稀且间隔翻倍() {
        let mut h = store(
            "thin",
            HistoryCfg {
                enabled: true,
                keep: 30,
                sample_s: 1,
            },
        );
        let s = sample(35.0, 30.0, 50, ChargingStatus::Charging);
        // 记到明显超过 MAX_POINTS
        for t in 0..(MAX_POINTS as i64 + 50) {
            h.observe(t, &s, 0, 9_000_000, "normal");
        }
        let cur = h.current.as_ref().unwrap();
        assert!(
            cur.points.len() <= MAX_POINTS,
            "抽稀后点数必须回到上限内，实为 {}",
            cur.points.len()
        );
        assert!(
            cur.sample_s > 1,
            "抽稀后采样间隔必须翻倍，实为 {}",
            cur.sample_s
        );
    }

    #[test]
    fn 抽稀保留首尾点() {
        let mut h = store(
            "thin_ends",
            HistoryCfg {
                enabled: true,
                keep: 30,
                sample_s: 1,
            },
        );
        let s = sample(35.0, 30.0, 50, ChargingStatus::Charging);
        let last_t = MAX_POINTS as i64 + 50;
        for t in 0..last_t {
            h.observe(t, &s, 0, 9_000_000, "normal");
        }
        let cur = h.current.as_ref().unwrap();
        assert_eq!(cur.points[0].t, 0, "首点必须保留");

        // ⚠️ 末点只能断言"接近末尾"，不能断言精确值：
        //    抽稀后采样间隔翻倍（1s → 2s），此后的点按新间隔记，
        //    因此最后一个点的时刻取决于"抽稀发生在第几拍"这个内部细节。
        //    真正要守住的性质是：**曲线的右端不能缺失**。
        let last = cur.points.last().unwrap().t;
        assert!(
            last >= last_t - 3,
            "末点必须落在记录末尾附近（实得 {last}，记录到 {}）",
            last_t - 1
        );
    }

    // ── 修剪 ──

    #[test]
    fn 超过保留次数则删最老的() {
        let mut h = store(
            "keep",
            HistoryCfg {
                enabled: true,
                keep: 3,
                sample_s: 30,
            },
        );
        let on = sample(35.0, 30.0, 50, ChargingStatus::Charging);
        let off = sample(35.0, 0.0, 60, ChargingStatus::Discharging);

        // 造 5 次会话
        for i in 0..5i64 {
            let base = 10_000 + i * 1000;
            h.observe(base, &on, 0, 9_000_000, "normal");
            h.observe(base + 500, &off, 0, 0, "idle");
        }

        let all = h.load_sessions();
        assert_eq!(all.len(), 3, "只应保留最近 3 次");
        // 倒序 ⇒ 最新在前
        assert_eq!(all[0].started_at, 10_000 + 4 * 1000);
        assert_eq!(all[2].started_at, 10_000 + 2 * 1000);
    }

    #[test]
    fn 最新会话排在最前() {
        let mut h = store("order", HistoryCfg::default());
        let on = sample(35.0, 30.0, 50, ChargingStatus::Charging);
        let off = sample(35.0, 0.0, 60, ChargingStatus::Discharging);
        for i in 0..3i64 {
            let base = 10_000 + i * 1000;
            h.observe(base, &on, 0, 9_000_000, "normal");
            h.observe(base + 500, &off, 0, 0, "idle");
        }
        let all = h.load_sessions();
        assert!(all[0].started_at > all[1].started_at);
        assert!(all[1].started_at > all[2].started_at);
    }

    // ── 总开关 ──

    #[test]
    fn 关闭时不记录() {
        let mut h = store(
            "disabled",
            HistoryCfg {
                enabled: false,
                keep: 30,
                sample_s: 30,
            },
        );
        let s = sample(35.0, 30.0, 50, ChargingStatus::Charging);
        for t in 0..100 {
            assert_eq!(
                h.observe(t * 30, &s, 0, 9_000_000, "normal"),
                ObserveOutcome::None
            );
        }
        assert!(!h.is_recording(), "关闭时不得开始会话");
        assert!(h.load_sessions().is_empty());
    }

    #[test]
    fn 记录中关闭会立即归档() {
        let mut h = store("disable_mid", HistoryCfg::default());
        let s = sample(35.0, 30.0, 50, ChargingStatus::Charging);
        h.observe(1000, &s, 0, 9_000_000, "normal");
        h.observe(1030, &s, 0, 9_000_000, "normal");
        assert!(h.is_recording());

        h.configure(HistoryCfg {
            enabled: false,
            keep: 30,
            sample_s: 30,
        });
        assert!(matches!(
            h.observe(3600, &s, 0, 9_000_000, "normal"),
            ObserveOutcome::Finished(_)
        ));
        assert_eq!(h.load_sessions().len(), 1, "已记录的部分必须被归档而非丢弃");
    }

    // ── 草稿 ──

    #[test]
    fn 草稿按间隔落盘并可恢复() {
        let d = tmp_dir("draft");
        let path = d.join("history.json");
        let draft = d.join("history.current.json");
        let cfg = HistoryCfg::default();

        {
            let mut h = HistoryStore::new(path.clone(), draft.clone(), cfg);
            let s = sample(35.0, 30.0, 50, ChargingStatus::Charging);
            h.observe(1000, &s, 0, 9_000_000, "normal");
            assert!(draft.exists(), "会话开始即应落一次草稿");

            // 记到超过草稿间隔
            for t in 1..=30 {
                h.observe(1000 + t * 30, &s, 0, 9_000_000, "normal");
            }
            assert!(draft.exists());
        }

        // 模拟重启：新实例恢复草稿
        let mut h2 = HistoryStore::new(path, draft.clone(), cfg);
        assert!(h2.restore_draft(), "应能恢复草稿");
        assert!(h2.is_recording());
        let cur = h2.current.as_ref().unwrap();
        assert_eq!(cur.started_at, 1000);
        assert!(cur.points.len() > 1);

        // 恢复后判定已停止充电 ⇒ 立即归档
        let off = sample(35.0, 0.0, 60, ChargingStatus::Discharging);
        assert!(matches!(
            h2.observe(9999, &off, 0, 0, "idle"),
            ObserveOutcome::Finished(_)
        ));
        assert_eq!(h2.load_sessions().len(), 1);
        assert!(!draft.exists(), "归档后草稿应被删除");
    }

    #[test]
    fn 草稿损坏不致命() {
        let d = tmp_dir("draft_bad");
        let draft = d.join("history.current.json");
        std::fs::write(&draft, b"{ this is not json").unwrap();

        let mut h = HistoryStore::new(d.join("history.json"), draft.clone(), HistoryCfg::default());
        assert!(!h.restore_draft(), "损坏的草稿应被丢弃");
        assert!(!draft.exists(), "损坏草稿应被删除，避免每拍重复解析失败");
        assert!(!h.is_recording());
    }

    // ── 清空与容错 ──

    #[test]
    fn 清空返回删除条数() {
        let mut h = store("clear", HistoryCfg::default());
        let on = sample(35.0, 30.0, 50, ChargingStatus::Charging);
        let off = sample(35.0, 0.0, 60, ChargingStatus::Discharging);
        for i in 0..3i64 {
            let base = 10_000 + i * 1000;
            h.observe(base, &on, 0, 9_000_000, "normal");
            h.observe(base + 500, &off, 0, 0, "idle");
        }
        assert_eq!(h.clear().unwrap(), 3);
        assert!(h.load_sessions().is_empty());
    }

    #[test]
    fn 历史文件损坏按空处理() {
        let d = tmp_dir("corrupt");
        let path = d.join("history.json");
        std::fs::write(&path, b"not json at all").unwrap();
        let h = HistoryStore::new(path, d.join("draft.json"), HistoryCfg::default());
        assert!(h.load_sessions().is_empty(), "损坏的历史应安全降级为空");
    }

    #[test]
    fn 按_id_取单次会话() {
        let mut h = store("byid", HistoryCfg::default());
        let on = sample(35.0, 30.0, 50, ChargingStatus::Charging);
        let off = sample(35.0, 0.0, 60, ChargingStatus::Discharging);
        h.observe(10_000, &on, 0, 9_000_000, "normal");
        h.observe(10_500, &off, 0, 0, "idle");

        let s = h.load_session("10000").expect("应按 id 找到");
        assert_eq!(s.soc_start, 50);
        assert!(h.load_session("99999").is_none());
    }

    #[test]
    fn 统计汇总正确() {
        let mut h = store("stats", HistoryCfg::default());
        let on = sample(35.0, 30.0, 50, ChargingStatus::Charging);
        let off = sample(35.0, 0.0, 60, ChargingStatus::Discharging);
        // ⚠️ 必须**按采样间隔**逐点记，不能只记首尾两点：
        //    中间断档 3600 秒会超过 MAX_GAP_S，那段能量按设计**不计入**
        //    （那是防"休眠/重启把间隔拉长导致严重高估"的保护）。
        h.observe(0, &on, 0, 9_000_000, "normal");
        for t in 1..=120 {
            h.observe(t * 30, &on, 0, 9_000_000, "normal");
        }
        h.observe(3630, &off, 0, 0, "idle");

        let st = h.stats();
        assert_eq!(st["count"], 1);
        assert_eq!(st["keep"], 30);
        assert!(
            st["energy_total_wh"].as_f64().unwrap() > 0.0,
            "正常充电序列必须累积出能量"
        );
    }

    #[test]
    fn 空历史统计不炸() {
        let h = store("stats_empty", HistoryCfg::default());
        let st = h.stats();
        assert_eq!(st["count"], 0);
        assert_eq!(st["peak_temp_c"], 0.0);
    }

    #[test]
    fn 会话时长进行中按当前时间算() {
        let mut h = store("dur", HistoryCfg::default());
        let s = sample(35.0, 30.0, 50, ChargingStatus::Charging);
        h.observe(1000, &s, 0, 9_000_000, "normal");
        let cur = h.current.as_ref().unwrap();
        assert_eq!(cur.duration_s(1000), 0);
        assert_eq!(cur.duration_s(2500), 1500);
        assert!(cur.is_running());
    }

    #[test]
    fn 阶段字符串映射完整() {
        use thermo_core::state::Phase;
        assert_eq!(phase_str(Phase::Idle), "idle");
        assert_eq!(phase_str(Phase::Normal), "normal");
        assert_eq!(phase_str(Phase::Tightening), "tightening");
        assert_eq!(phase_str(Phase::HardProtect), "hard_protect");
    }

    // ── 真机踩到的两个展示缺陷 ──

    #[test]
    fn 草稿是追加写的且不丢点() {
        // 🔴 真机暴露的**数据丢失**缺陷：
        //
        //    初版草稿每 10 分钟整份重写 ⇒ worker 在两次重写之间重启
        //    ⇒ 那 10 分钟内的点全部丢失（实测 35 点只剩 24 点，
        //      曲线上出现 109/207 秒两处断档）。
        //
        //    现在每采一点就追加一行 ⇒ 零丢失。
        let mut h = store("draft_append", HistoryCfg::default());
        let s = sample(35.0, 30.0, 50, ChargingStatus::Charging);

        h.observe(1000, &s, 0, 9_000_000, "normal");
        for t in 1..=10 {
            h.observe(1000 + t * 30, &s, 0, 9_000_000, "normal");
        }

        let mem = h.current_session_full().expect("应有进行中会话");
        let text = std::fs::read_to_string(&h.draft_path).expect("草稿应存在");
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();

        // 第 1 行是头部，其余每行一个点
        assert_eq!(
            lines.len(),
            mem.points.len() + 1,
            "草稿行数应为 1 头部 + N 点"
        );
        assert!(
            lines[0].contains("\"h\":1"),
            "第一行必须是头部，实得：{}",
            lines[0]
        );

        // 从草稿恢复 ⇒ 点数必须**一个不少**
        let mut h2 = HistoryStore::new(h.path.clone(), h.draft_path.clone(), HistoryCfg::default());
        assert!(h2.restore_draft(), "应能恢复草稿");
        let restored = h2.current_session_full().expect("恢复后应进行中");
        assert_eq!(
            restored.points.len(),
            mem.points.len(),
            "恢复后点数必须与丢失前的内存一致（这正是修掉的那个缺陷）"
        );
        // 汇总值由点重算 ⇒ 也必须一致
        assert!(
            (restored.energy_wh - mem.energy_wh).abs() < 0.01,
            "重算的累计电量应与原来一致：{} vs {}",
            restored.energy_wh,
            mem.energy_wh
        );
        assert!((restored.peak_temp_c - mem.peak_temp_c).abs() < 0.01);
    }

    #[test]
    fn 草稿末行残缺也能恢复前面的点() {
        // 崩溃时最后一行可能只写了一半 —— 必须跳过它、保住前面的点
        let mut h = store("draft_partial", HistoryCfg::default());
        let s = sample(35.0, 30.0, 50, ChargingStatus::Charging);
        h.observe(1000, &s, 0, 9_000_000, "normal");
        for t in 1..=4 {
            h.observe(1000 + t * 30, &s, 0, 9_000_000, "normal");
        }
        let before = h.current_session_full().unwrap().points.len();

        // 手工追加一行残破内容（模拟写到一半掉电）
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&h.draft_path)
                .unwrap();
            write!(f, "{{\"t\":9999,\"temp_c\":3").unwrap();
        }

        let mut h2 = HistoryStore::new(h.path.clone(), h.draft_path.clone(), HistoryCfg::default());
        assert!(h2.restore_draft(), "有残行也必须能恢复");
        assert_eq!(
            h2.current_session_full().unwrap().points.len(),
            before,
            "残行应被跳过，前面的点一个都不能丢"
        );
    }

    #[test]
    fn 草稿无头则丢弃() {
        // 只有点、没有头 ⇒ 无法知道会话何时开始 ⇒ 丢弃
        let d = tmp_dir("draft_noheader");
        let draft = d.join("history.current.jsonl");
        std::fs::write(
            &draft,
            b"{\"t\":1,\"temp_c\":30.0,\"power_w\":1.0,\"soc\":50,\"gear\":0,\"limit_ua\":0,\"phase\":\"normal\"}\n",
        )
        .unwrap();

        let mut h = HistoryStore::new(d.join("history.json"), draft.clone(), HistoryCfg::default());
        assert!(!h.restore_draft(), "无头的草稿应被丢弃");
        assert!(!draft.exists(), "无头草稿应被删除");
    }

    #[test]
    fn 抽稀后草稿被整份重写且间隔已更新() {
        let mut h = store(
            "draft_thin",
            HistoryCfg {
                enabled: true,
                keep: 30,
                sample_s: 1,
            },
        );
        let s = sample(35.0, 30.0, 50, ChargingStatus::Charging);
        for t in 0..(MAX_POINTS as i64 + 50) {
            h.observe(t, &s, 0, 9_000_000, "normal");
        }

        let mem = h.current_session_full().unwrap();
        assert!(mem.sample_s > 1, "应已触发抽稀");

        // 恢复后间隔必须跟着更新，否则曲线从此刻起又变密
        let mut h2 = HistoryStore::new(h.path.clone(), h.draft_path.clone(), HistoryCfg::default());
        assert!(h2.restore_draft());
        let restored = h2.current_session_full().unwrap();
        assert_eq!(
            restored.sample_s, mem.sample_s,
            "抽稀后的采样间隔必须落进草稿头部"
        );
        assert_eq!(restored.points.len(), mem.points.len(), "点数应与内存一致");
    }

    #[test]
    fn 记录时即取整以保证三者一致() {
        // 真机看到草稿里是 `"power_w":0.045888` —— 内存 / 草稿 / 界面
        // 三处的值必须完全一致，否则"重启前后同一段曲线算出两个能量值"。
        let mut h = store("round_on_record", HistoryCfg::default());
        // 造一个必然带浮点尾巴的功率：0.045888 W
        let s = Sample {
            temp_c: 33.4,
            virt_c: None,
            current_ua: 11766,
            voltage_uv: 3_900_000,
            soc: 44,
            status: ChargingStatus::Charging,
        };
        h.observe(1000, &s, 0, 16_000_000, "normal");

        let p = &h.current_session_full().unwrap().points[0];
        // f32 的 0.045888 取整到 2 位后应输出 "0.05"
        assert_eq!(format!("{}", p.power_w), "0.05", "功率应取整到 2 位小数");
        assert_eq!(p.temp_c, 33.4, "温度应取整到 1 位");

        // 草稿里的文本也必须是干净的
        let text = std::fs::read_to_string(&h.draft_path).unwrap();
        assert!(
            !text.contains("0.045888"),
            "草稿里不得出现原始浮点尾巴：{text}"
        );
    }

    #[test]
    fn 恢复时取整旧点() {
        // 旧版本写的点带完整 f32 精度 —— 恢复时应清理，
        // 否则"内存 0.48896 / 界面 0.49"不一致
        let d = tmp_dir("restore_round");
        let draft = d.join("history.current.jsonl");
        std::fs::write(
            &draft,
            b"{\"h\":1,\"id\":\"1\",\"started_at\":1,\"soc_start\":50,\"sample_s\":30}\n\
              {\"t\":1,\"temp_c\":31.23456,\"power_w\":0.48896,\"soc\":50,\"gear\":0,\"limit_ua\":0,\"phase\":\"normal\"}\n",
        )
        .unwrap();

        let mut h = HistoryStore::new(d.join("history.json"), draft, HistoryCfg::default());
        assert!(h.restore_draft());
        let p = &h.current_session_full().unwrap().points[0];
        assert_eq!(format!("{}", p.power_w), "0.49", "旧点应在恢复时取整");
        assert_eq!(p.temp_c, 31.2, "旧点温度也应取整");
    }

    #[test]
    fn 归档后草稿被删除() {
        let mut h = store("draft_cleanup", HistoryCfg::default());
        let on = sample(35.0, 30.0, 50, ChargingStatus::Charging);
        let off = sample(35.0, 0.0, 60, ChargingStatus::Discharging);
        h.observe(1000, &on, 0, 9_000_000, "normal");
        assert!(h.draft_path.exists(), "会话开始时草稿就该存在");

        h.observe(2000, &off, 0, 0, "idle");
        assert!(!h.draft_path.exists(), "归档后草稿必须删除");
    }

    #[test]
    fn 负零被消除() {
        // 真机看到 `energy_total_wh: -0.0` —— 界面会显示成「-0 Wh」，
        // 看起来像数据错了
        assert!(
            round1(-0.0_f32).is_sign_positive(),
            "round1 不得把 -0.0 原样传出去"
        );
        assert!(round2(-0.0_f32).is_sign_positive());
        assert!(round3(-0.0_f32).is_sign_positive());
        // 负的极小值取整后也会变成零 —— 同样要转成正零
        assert!(round1(-0.001_f32).is_sign_positive());
        // 正常值不受影响
        assert!((round1(42.34) - 42.3).abs() < 1e-6);
        // f32 的 `0.037` 内部正是 0.03700000047683716（真机上看到的尾巴）
        // ⇒ 取整到 3 位后应当干净地输出 "0.037"
        assert_eq!(format!("{}", round3(0.037_f32)), "0.037");
    }

    #[test]
    fn 进行中会话的浮点已取整() {
        let mut h = store("round_live", HistoryCfg::default());
        let s = sample(35.0, 30.0, 50, ChargingStatus::Charging);
        h.observe(0, &s, 0, 9_000_000, "normal");
        for t in 1..=5 {
            h.observe(t * 30, &s, 0, 9_000_000, "normal");
        }

        let cur = h.current_session(150).expect("应有进行中会话");
        // ⚠️ 真机暴露：f32 取整后序列化仍带尾巴（1.0959999561309814）
        //    ⇒ 必须在 f64 上取整。这里直接断言**序列化后的文本**。
        let text = serde_json::to_string(&cur).unwrap();
        assert!(
            !text.contains("0000000"),
            "序列化后不得出现浮点尾巴，实得：{text}"
        );

        // 平均功率必须是**实时算出来的**，不能是 0
        // （真机暴露：界面显示「平均功率 0.0 W」，像是模块坏了）
        let avg = cur["avg_power_w"].as_f64().unwrap();
        assert!(avg > 0.0, "进行中的会话必须给出实时平均功率，实得 {avg}");
    }

    #[test]
    fn 归档会话的浮点也干净() {
        let mut h = store("round_arch", HistoryCfg::default());
        let on = sample(35.0, 30.0, 50, ChargingStatus::Charging);
        let off = sample(35.0, 0.0, 60, ChargingStatus::Discharging);
        h.observe(0, &on, 0, 9_000_000, "normal");
        for t in 1..=120 {
            h.observe(t * 30, &on, 0, 9_000_000, "normal");
        }
        h.observe(3630, &off, 0, 0, "idle");

        let s = h.load_sessions().into_iter().next().expect("应有归档会话");
        let text = serde_json::to_string(&s.to_json_clean(3630)).unwrap();
        assert!(
            !text.contains("0000000"),
            "归档会话序列化后不得有浮点尾巴，实得：{text}"
        );

        let li = serde_json::to_string(&s.to_list_item(3630)).unwrap();
        assert!(!li.contains("0000000"), "列表项也不得有浮点尾巴：{li}");
    }
}
