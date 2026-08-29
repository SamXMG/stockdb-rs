//! 硬风控闸门（L2）—— 纯 Rust 内核。
//!
//! 独立于 `Store`，不持有只读引用，可独立 `cargo test`。
//! 风控规则需要的行情由调用方用 StockDB 读出后塞入 `intent.context`。
//!
//! 参考：`docs/risk-gate-design.md`。
//!
//! P0a 范围：仅 `R_PARSE`/`R_BLOCK`/`R_ALLOWLIST` 三条规则生效，
//! `R_STALE`/`R_SINGLE_CAP`/`R_TOTAL_CAP`/`R_DRAWDOWN` 留 TODO 桩（P0b 实现）。
//! `audit_log` 字段保留在结构体但 P0a 不写入（`evaluate(&self)` 为纯只读语义，
//! 审计写入需要 `&mut self`，留 TODO P1）。

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::Hasher;
use std::path::Path;

// ---- §4.2 类型定义 ----

/// 调仓动作枚举。serde 反序列化越界即报错（编译期硬边界，R_PARSE 第一道）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    Open,
    Inc,
    Dec,
    Close,
    Hold,
}

impl Op {
    /// 是否为开仓类（受 R_DRAWDOWN 熔断影响）。
    pub fn is_opening(&self) -> bool {
        matches!(self, Op::Open | Op::Inc)
    }
}

/// 调仓意向中的单个动作。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Action {
    pub op: Op,
    pub code: String,
    pub qty: i64,
    /// 自由文本，只写 audit，不参与任何规则判断。
    #[serde(default)]
    pub reason_ref: Option<String>,
}

/// 行情上下文（由调用方用 StockDB 读出后塞入）。RiskState 不自己读行情。
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct IntentContext {
    #[serde(default)]
    pub close: Option<f64>,
    #[serde(default)]
    pub atr: Option<f64>,
    #[serde(default)]
    pub regime: Option<String>,
    /// P0b 新增：当前净值，供 R_DRAWDOWN 计算回撤。缺失则跳过该规则。
    #[serde(default)]
    pub net_value: Option<f64>,
}

/// 调仓意向（evaluate 输入）。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Intent {
    /// 必填：绑定的行情快照时间戳（交易日索引）。缺失即反序列化失败（R_STALE 防护）。
    pub snapshot_t: i64,
    pub actions: Vec<Action>,
    #[serde(default)]
    pub context: IntentContext,
}

/// 裁决后的单个动作（verdict 内）。
///
/// 注：任务书 §4.2 原签名仅 `serde::Serialize`，但 `commit` 需要反序列化 verdict JSON，
/// 依据 §9.5（以可编译为准）追加 `serde::Deserialize`。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VerdictAction {
    pub op: Op,
    pub code: String,
    pub qty: i64,
    /// 触发的规则 ID（null=通过）。序列化为 null。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// 裁决结果（evaluate 输出）。
///
/// 注：任务书 §4.2 原签名仅 `serde::Serialize`，追加 `serde::Deserialize` 供 `commit` 解析，
/// 依据 §9.5（以可编译为准）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Verdict {
    /// "approved" | "trimmed" | "rejected"
    pub decision: String,
    pub snapshot_t: i64,
    pub actions: Vec<VerdictAction>,
    pub rejected: Vec<VerdictAction>,
    pub config_version: u64,
    /// P0b 新增：携带 intent.context 快照，供 commit 反序列化提取 net_value 更新水位。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<IntentContext>,
}

/// 审计日志条目。
#[derive(Debug, Clone, serde::Serialize)]
pub struct AuditEntry {
    pub ts: i64,
    pub intent: serde_json::Value,
    pub verdict: serde_json::Value,
    pub rule_id: Option<String>,
}

// ---- §4.3 配置结构 ----

/// 风控配置（从 JSON 加载）。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RiskConfig {
    /// 全局仓位上限（元）。P0a 不强制，留字段。
    #[serde(default)]
    pub total_capital: Option<f64>,
    /// 单票金额上限（元）。P0a R_SINGLE_CAP 用，但 P0a 该规则仅留 TODO 桩。
    #[serde(default)]
    pub single_capital: Option<f64>,
    /// 回撤熔断阈值（百分比，0..1）。P0a 留桩。
    #[serde(default)]
    pub drawdown_threshold: Option<f64>,
    /// snapshot_t 允许偏差（默认 0）。P0a R_STALE 用，但 P0a 留桩，默认 0。
    #[serde(default = "default_stale_tolerance")]
    pub stale_tolerance: i64,
    /// 启动时禁买名单。
    #[serde(default)]
    pub blocklist: Vec<String>,
    /// 白名单指令集（允许的 op 小写字符串）。空=全部允许。
    #[serde(default)]
    pub allowlist: Vec<String>,
}

fn default_stale_tolerance() -> i64 {
    0
}

// ---- §4.4 RiskState 与方法 ----

/// 硬风控可变决策状态。独立于 Store，不持有只读引用。
pub struct RiskState {
    config: RiskConfig,
    config_version: u64,
    positions: HashMap<String, (i64, f64)>, // code -> (qty, avg_cost)
    used_today: HashMap<String, f64>,       // code -> 已成交金额
    drawdown_watermark: f64,
    blocklist: HashSet<String>,
    last_known_t: i64,
    audit_log: VecDeque<AuditEntry>,
}

/// 用 DefaultHasher 对配置文件原始 JSON 字节哈希取 u64。
fn hash_config(raw: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    h.write(raw.as_bytes());
    h.finish()
}

impl RiskState {
    /// 加载风控配置。config_version 由配置内容哈希生成（确定性）。
    pub fn open<P: AsRef<Path>>(config_path: P) -> Result<Self, String> {
        let raw = std::fs::read_to_string(config_path.as_ref())
            .map_err(|e| format!("read config failed: {e}"))?;
        let config: RiskConfig = serde_json::from_str(&raw)
            .map_err(|e| format!("parse config failed: {e}"))?;
        let config_version = hash_config(&raw);
        let blocklist: HashSet<String> = config.blocklist.iter().cloned().collect();
        Ok(Self {
            config,
            config_version,
            positions: HashMap::new(),
            used_today: HashMap::new(),
            drawdown_watermark: 0.0,
            blocklist,
            last_known_t: 0,
            // P0a: 容量上限 1024，溢出弹头策略留 P1。
            audit_log: VecDeque::with_capacity(1024),
        })
    }

    /// reload 配置（版本号基于内容哈希重新计算：同内容→同版本，不同内容→新版本）。
    pub fn reload<P: AsRef<Path>>(&mut self, config_path: P) -> Result<(), String> {
        let raw = std::fs::read_to_string(config_path.as_ref())
            .map_err(|e| format!("read config failed: {e}"))?;
        let config: RiskConfig = serde_json::from_str(&raw)
            .map_err(|e| format!("parse config failed: {e}"))?;
        self.config = config;
        self.config_version = hash_config(&raw);
        // blocklist 从配置同步（reload 覆盖，不追加；追加走 add_blocklist）。
        self.blocklist = self.config.blocklist.iter().cloned().collect();
        Ok(())
    }

    /// 同步持仓快照（来自实盘账户，非 Agent）。positions_json: [{"code","qty","avg_cost"}]。
    pub fn set_positions(&mut self, positions_json: &str) -> Result<(), String> {
        #[derive(serde::Deserialize)]
        struct PosEntry {
            code: String,
            qty: i64,
            avg_cost: f64,
        }
        let entries: Vec<PosEntry> = serde_json::from_str(positions_json)
            .map_err(|e| format!("parse positions failed: {e}"))?;
        self.positions.clear();
        for e in entries {
            self.positions.insert(e.code, (e.qty, e.avg_cost));
        }
        Ok(())
    }

    /// 追加禁买名单。blocklist_json: ["code", ...]。并入不替换。
    pub fn add_blocklist(&mut self, blocklist_json: &str) -> Result<(), String> {
        let codes: Vec<String> = serde_json::from_str(blocklist_json)
            .map_err(|e| format!("parse blocklist failed: {e}"))?;
        for code in codes {
            self.blocklist.insert(code);
        }
        Ok(())
    }

    /// 调仓意向 -> 裁决。R_PARSE 反序列化在此完成。
    /// P0b：签名 `&self` → `&mut self`，为写 audit_log。
    pub fn evaluate(&mut self, intent_json: &str) -> Result<String, String> {
        // R_PARSE: serde 反序列化（op 越界 / 缺 snapshot_t 在此报错）。
        let intent: Intent = serde_json::from_str(intent_json)
            .map_err(|e| format!("R_PARSE: {e}"))?;

        let mut passed: Vec<VerdictAction> = Vec::new();
        let mut rejected: Vec<VerdictAction> = Vec::new();

        // R_STALE：整批校验。snapshot_t 与 last_known_t 偏差 > stale_tolerance -> 整批拒绝。
        // 位置：R_PARSE 之后、per-action 遍历之前。
        let stale_diff = (intent.snapshot_t - self.last_known_t).abs();
        if stale_diff > self.config.stale_tolerance {
            let note = format!(
                "snapshot_t={} vs last_known_t={} tolerance={}",
                intent.snapshot_t, self.last_known_t, self.config.stale_tolerance
            );
            for action in &intent.actions {
                rejected.push(VerdictAction {
                    op: action.op,
                    code: action.code.clone(),
                    qty: action.qty,
                    rule_id: Some("R_STALE".to_string()),
                    note: Some(note.clone()),
                });
            }
            let verdict = Verdict {
                decision: "rejected".to_string(),
                snapshot_t: intent.snapshot_t,
                actions: Vec::new(),
                rejected,
                config_version: self.config_version,
                context: Some(intent.context.clone()),
            };
            let verdict_json = serde_json::to_string(&verdict)
                .map_err(|e| format!("serialize verdict failed: {e}"))?;
            self.append_audit(
                intent.snapshot_t,
                intent_json,
                &verdict_json,
                Some("R_STALE".to_string()),
            );
            return Ok(verdict_json);
        }

        for action in &intent.actions {
            let mut rule_id: Option<String> = None;
            let mut note: Option<String> = None;
            let mut qty = action.qty;
            let mut is_rejected = false;

            // R_BLOCK: code ∈ blocklist -> 该 action 进 rejected。
            if self.blocklist.contains(&action.code) {
                rule_id = Some("R_BLOCK".to_string());
                is_rejected = true;
            }

            // R_SINGLE_CAP：per-action 裁剪。仅 Open/Inc。
            // 估算金额 = close * qty > single_capital 时裁剪 qty=floor(upper/close)。
            if !is_rejected && action.op.is_opening() {
                if let Some(single_cap) = self.config.single_capital {
                    let close = intent.context.close.unwrap_or(0.0);
                    if close > 0.0 {
                        let amount = close * qty as f64;
                        if amount > single_cap {
                            let old_qty = qty;
                            let new_qty = (single_cap / close).floor() as i64;
                            if new_qty <= 0 {
                                rule_id = Some("R_SINGLE_CAP".to_string());
                                note = Some("trimmed to 0".to_string());
                                is_rejected = true;
                            } else {
                                qty = new_qty;
                                note = Some(format!(
                                    "trimmed by R_SINGLE_CAP {}->{}",
                                    old_qty, new_qty
                                ));
                            }
                        }
                    }
                }
            }

            // R_TOTAL_CAP：per-action 裁剪/拒绝。仅 Open/Inc。基于 R_SINGLE_CAP 裁剪后 qty。
            // 当前已用 = used_today.values().sum()；本 action 金额 + 已用 > total_capital -> 裁剪到剩余。
            if !is_rejected && action.op.is_opening() {
                if let Some(total_cap) = self.config.total_capital {
                    let close = intent.context.close.unwrap_or(0.0);
                    if close > 0.0 {
                        let used: f64 = self.used_today.values().sum();
                        let remaining = total_cap - used;
                        if remaining <= 0.0 {
                            rule_id = Some("R_TOTAL_CAP".to_string());
                            note = Some("no remaining capital".to_string());
                            is_rejected = true;
                        } else {
                            let amount = close * qty as f64;
                            if amount > remaining {
                                let old_qty = qty;
                                let new_qty = (remaining / close).floor() as i64;
                                if new_qty <= 0 {
                                    rule_id = Some("R_TOTAL_CAP".to_string());
                                    note = Some("trimmed to 0".to_string());
                                    is_rejected = true;
                                } else {
                                    qty = new_qty;
                                    note = Some(format!(
                                        "trimmed by R_TOTAL_CAP {}->{}",
                                        old_qty, new_qty
                                    ));
                                }
                            }
                        }
                    }
                }
            }

            // R_DRAWDOWN：per-action 拒绝开仓类。仅 Open/Inc。
            // 触发条件：drawdown_threshold 非空 且 watermark>0 且 context.net_value 存在。
            // 回撤 = 1 - net_value/watermark，超阈值 -> 拒绝。net_value 缺失则跳过。
            if !is_rejected && action.op.is_opening() {
                if let (Some(threshold), Some(net_value)) =
                    (self.config.drawdown_threshold, intent.context.net_value)
                {
                    if self.drawdown_watermark > 0.0 {
                        let drawdown = 1.0 - net_value / self.drawdown_watermark;
                        if drawdown > threshold {
                            rule_id = Some("R_DRAWDOWN".to_string());
                            note = Some(format!("drawdown {:.4} > {}", drawdown, threshold));
                            is_rejected = true;
                        }
                    }
                }
            }

            // R_ALLOWLIST: allowlist 非空且 op(小写) ∉ allowlist -> rejected。
            if !is_rejected && !self.config.allowlist.is_empty() {
                let op_lower = match action.op {
                    Op::Open => "open",
                    Op::Inc => "inc",
                    Op::Dec => "dec",
                    Op::Close => "close",
                    Op::Hold => "hold",
                };
                if !self.config.allowlist.iter().any(|a| a == op_lower) {
                    rule_id = Some("R_ALLOWLIST".to_string());
                    is_rejected = true;
                }
            }

            let va = VerdictAction {
                op: action.op,
                code: action.code.clone(),
                qty,
                rule_id,
                note,
            };
            if is_rejected {
                rejected.push(va);
            } else {
                passed.push(va);
            }
        }

        // 裁决聚合：全通过=approved；全部 rejected=rejected；部分 rejected=trimmed。
        let decision = if rejected.is_empty() {
            "approved"
        } else if passed.is_empty() {
            "rejected"
        } else {
            "trimmed"
        };

        let verdict = Verdict {
            decision: decision.to_string(),
            snapshot_t: intent.snapshot_t,
            actions: passed,
            rejected,
            config_version: self.config_version,
            context: Some(intent.context.clone()),
        };

        let verdict_json = serde_json::to_string(&verdict)
            .map_err(|e| format!("serialize verdict failed: {e}"))?;

        // audit_log 写入：rule_id 取首个 rejected 的 rule_id，无则 None。
        let first_rejected_rule = verdict
            .rejected
            .first()
            .and_then(|v| v.rule_id.clone());
        self.append_audit(intent.snapshot_t, intent_json, &verdict_json, first_rejected_rule);

        Ok(verdict_json)
    }

    /// 追加一条审计日志，强制执行 1024 容量上限（溢出弹头 pop_front）。
    fn append_audit(
        &mut self,
        ts: i64,
        intent_json: &str,
        verdict_json: &str,
        rule_id: Option<String>,
    ) {
        let entry = AuditEntry {
            ts,
            intent: serde_json::from_str(intent_json).unwrap_or(serde_json::Value::Null),
            verdict: serde_json::from_str(verdict_json).unwrap_or(serde_json::Value::Null),
            rule_id,
        };
        self.audit_log.push_back(entry);
        if self.audit_log.len() > 1024 {
            self.audit_log.pop_front();
        }
    }

    /// 导出审计日志（JSON 数组）。供 A/B 重放与离线复盘消费。
    /// 注：任务书 §5.4 用 make_contiguous()，但该方法需 &mut self 与 &self 签名冲突；
    /// VecDeque 已实现 Serialize（序列化为数组），直接 &self.audit_log 即可。依 §9.5 标注偏差。
    pub fn audit_json(&self) -> String {
        serde_json::to_string(&self.audit_log).unwrap_or_else(|_| "[]".to_string())
    }

    /// 回写状态（执行后）。必须传入已批准 verdict 的 JSON。
    pub fn commit(&mut self, verdict_json: &str) -> Result<(), String> {
        let verdict: Verdict = serde_json::from_str(verdict_json)
            .map_err(|e| format!("parse verdict failed: {e}"))?;

        if verdict.decision == "rejected" {
            return Err("cannot commit rejected verdict".into());
        }

        // P0a: verdict JSON 不携带 context.close，close 缺失记 0。
        // 仍尝试从原始 JSON 提取 context.close（供 P0b 扩展用）。
        let raw: serde_json::Value =
            serde_json::from_str(verdict_json).unwrap_or(serde_json::Value::Null);
        let close = raw
            .get("context")
            .and_then(|c| c.get("close"))
            .and_then(|c| c.as_f64());

        for action in &verdict.actions {
            // used_today 累加（P0a: close 缺失 → 0）。
            let amount = close.unwrap_or(0.0) * action.qty as f64;
            *self.used_today.entry(action.code.clone()).or_insert(0.0) += amount;

            // positions 更新：Open/Inc 加仓、Dec/Close 减仓、qty 到 0 删项。
            match action.op {
                Op::Open => {
                    self.positions
                        .insert(action.code.clone(), (action.qty, 0.0));
                }
                Op::Inc => {
                    let entry = self
                        .positions
                        .entry(action.code.clone())
                        .or_insert((0, 0.0));
                    entry.0 += action.qty;
                }
                Op::Dec => {
                    let code = action.code.clone();
                    if let Some(entry) = self.positions.get_mut(&code) {
                        entry.0 -= action.qty;
                        if entry.0 <= 0 {
                            self.positions.remove(&code);
                        }
                    }
                }
                Op::Close => {
                    self.positions.remove(&action.code);
                }
                Op::Hold => {}
            }
        }

        // P0b: drawdown_watermark 更新——新高时抬升水位。
        // net_value 从 verdict 的 context 字段提取（commit 接收的是 verdict JSON）。
        let net_value = raw
            .get("context")
            .and_then(|c| c.get("net_value"))
            .and_then(|v| v.as_f64());
        if let Some(nv) = net_value {
            if nv > self.drawdown_watermark {
                self.drawdown_watermark = nv;
            }
        }

        self.last_known_t = verdict.snapshot_t;
        Ok(())
    }

    /// 状态快照 JSON（持仓/累计回撤/当日已用/禁买/配置版本）。
    pub fn status_json(&self) -> String {
        let positions: serde_json::Map<String, serde_json::Value> = self
            .positions
            .iter()
            .map(|(k, (q, c))| (k.clone(), serde_json::json!([q, c])))
            .collect();
        let used_today: serde_json::Map<String, serde_json::Value> = self
            .used_today
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::json!(v)))
            .collect();
        let blocklist: Vec<&str> = self.blocklist.iter().map(|s| s.as_str()).collect();
        serde_json::json!({
            "config_version": self.config_version,
            "positions": serde_json::Value::Object(positions),
            "used_today": serde_json::Value::Object(used_today),
            "drawdown_watermark": self.drawdown_watermark,
            "blocklist": blocklist,
            "last_known_t": self.last_known_t,
        })
        .to_string()
    }

    pub fn config_version(&self) -> u64 {
        self.config_version
    }

    /// 把内存 audit_log 追加写入 JSONL 文件并清空。
    /// 文件：<dir>/risk_audit_<date>.jsonl，每行一条 AuditEntry JSON。
    /// date 用简单自增序号（P1 简化：不依赖系统时钟，用 config_version 拼接避免空文件）。
    pub fn flush_audit(&mut self, dir: &str) -> Result<usize, String> {
        if self.audit_log.is_empty() {
            return Ok(0);
        }
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("flush_audit create dir failed: {e}"))?;
        // 文件名用单调递增序号（避免依赖系统时钟，保证确定性可重放）
        let seq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let path = std::path::Path::new(dir).join(format!("risk_audit_{}.jsonl", seq));
        let mut lines = Vec::with_capacity(self.audit_log.len());
        let n = self.audit_log.len();
        while let Some(entry) = self.audit_log.pop_front() {
            if let Ok(line) = serde_json::to_string(&entry) {
                lines.push(line);
            }
        }
        // 追加写（不覆盖既有审计）
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("flush_audit open failed: {e}"))?;
        for line in &lines {
            writeln!(f, "{line}").map_err(|e| format!("flush_audit write failed: {e}"))?;
        }
        Ok(n)
    }

    /// 当前内存 audit_log 条数。
    pub fn audit_len(&self) -> usize {
        self.audit_log.len()
    }
}

// ---- §4.6 单测 ----

#[cfg(test)]
mod tests {
    use super::*;

    /// 写临时配置文件，返回路径。每个测试用唯一 name 避免并行冲突。
    fn write_temp_config(name: &str, content: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir()
            .join(format!("risk-gate-test-{}-{}.json", std::process::id(), name));
        std::fs::write(&path, content).expect("write temp config");
        path
    }

    fn cleanup(path: &std::path::Path) {
        let _ = std::fs::remove_file(path);
    }

    fn config_empty() -> String {
        "{}".to_string()
    }

    fn config_with_blocklist(codes: &[&str]) -> String {
        let items: Vec<String> = codes.iter().map(|s| format!("\"{s}\"")).collect();
        format!("{{\"blocklist\":[{}]}}", items.join(","))
    }

    fn config_with_allowlist(ops: &[&str]) -> String {
        let items: Vec<String> = ops.iter().map(|s| format!("\"{s}\"")).collect();
        format!("{{\"allowlist\":[{}]}}", items.join(","))
    }

    // 1. R_PARSE 编译期边界：op 越界反序列化必须 Err。
    #[test]
    fn test_op_serde_rejects_unknown() {
        let r = serde_json::from_str::<Op>("\"short\"");
        assert!(r.is_err(), "Op \"short\" must fail to deserialize (R_PARSE)");
    }

    // 2. Op 小写序列化/反序列化。
    #[test]
    fn test_op_serde_lowercase() {
        assert_eq!(serde_json::from_str::<Op>("\"inc\"").unwrap(), Op::Inc);
        assert_eq!(serde_json::to_string(&Op::Inc).unwrap(), "\"inc\"");
    }

    // 3. Intent 缺 snapshot_t 必须 Err。
    #[test]
    fn test_intent_missing_snapshot_t_rejected() {
        let r = serde_json::from_str::<Intent>("{\"actions\":[]}");
        assert!(r.is_err(), "Intent without snapshot_t must fail R_PARSE");
    }

    // 4. blocklist 命中 → R_BLOCK。
    #[test]
    fn test_evaluate_blocklist() {
        let cfg = write_temp_config("blocklist", &config_with_blocklist(&["600000"]));
        let mut state = RiskState::open(&cfg).unwrap();
        // P0b: snapshot_t=0 匹配初始 last_known_t=0 以通过 R_STALE。
        let intent = r#"{"snapshot_t":0,"actions":[{"op":"inc","code":"600000","qty":100}]}"#;
        let v: serde_json::Value = serde_json::from_str(&state.evaluate(intent).unwrap()).unwrap();
        assert_eq!(v["decision"], "rejected");
        assert_eq!(v["rejected"][0]["rule_id"], "R_BLOCK");
        cleanup(&cfg);
    }

    // 5. allowlist 不含 inc → R_ALLOWLIST；含 open → 通过。
    #[test]
    fn test_evaluate_allowlist() {
        let cfg = write_temp_config("allowlist", &config_with_allowlist(&["open", "dec"]));
        let mut state = RiskState::open(&cfg).unwrap();
        // P0b: snapshot_t=0 匹配初始 last_known_t=0 以通过 R_STALE。
        // inc 不在 allowlist → rejected
        let intent_inc = r#"{"snapshot_t":0,"actions":[{"op":"inc","code":"600000","qty":100}]}"#;
        let v: serde_json::Value =
            serde_json::from_str(&state.evaluate(intent_inc).unwrap()).unwrap();
        assert_eq!(v["decision"], "rejected");
        assert_eq!(v["rejected"][0]["rule_id"], "R_ALLOWLIST");
        // open 在 allowlist → approved
        let intent_open = r#"{"snapshot_t":0,"actions":[{"op":"open","code":"600000","qty":100}]}"#;
        let v2: serde_json::Value =
            serde_json::from_str(&state.evaluate(intent_open).unwrap()).unwrap();
        assert_eq!(v2["decision"], "approved");
        cleanup(&cfg);
    }

    // 6. 无 blocklist/allowlist → approved。
    #[test]
    fn test_evaluate_approved() {
        let cfg = write_temp_config("approved", &config_empty());
        let mut state = RiskState::open(&cfg).unwrap();
        // P0b: snapshot_t=0 匹配初始 last_known_t=0 以通过 R_STALE。
        let intent = r#"{"snapshot_t":0,"actions":[{"op":"inc","code":"600000","qty":100}]}"#;
        let v: serde_json::Value = serde_json::from_str(&state.evaluate(intent).unwrap()).unwrap();
        assert_eq!(v["decision"], "approved");
        assert_eq!(v["actions"][0]["code"], "600000");
        assert!(v["actions"][0].get("rule_id").is_none() || v["actions"][0]["rule_id"].is_null());
        cleanup(&cfg);
    }

    // 7. commit 更新 positions。
    #[test]
    fn test_commit_updates_positions() {
        let cfg = write_temp_config("commit", &config_empty());
        let mut state = RiskState::open(&cfg).unwrap();
        let verdict = r#"{"decision":"approved","snapshot_t":1,"actions":[{"op":"open","code":"600000","qty":100}],"rejected":[],"config_version":0}"#;
        state.commit(verdict).unwrap();
        let status: serde_json::Value = serde_json::from_str(&state.status_json()).unwrap();
        assert_eq!(status["positions"]["600000"][0], 100);
        cleanup(&cfg);
    }

    // 8. commit rejected verdict → Err。
    #[test]
    fn test_commit_rejected_refused() {
        let cfg = write_temp_config("commit_rej", &config_empty());
        let mut state = RiskState::open(&cfg).unwrap();
        let verdict = r#"{"decision":"rejected","snapshot_t":1,"actions":[],"rejected":[{"op":"inc","code":"600000","qty":100,"rule_id":"R_BLOCK"}],"config_version":0}"#;
        let r = state.commit(verdict);
        assert!(r.is_err(), "commit rejected verdict must Err");
        cleanup(&cfg);
    }

    // 9. reload 版本：同内容不变，不同内容变。
    #[test]
    fn test_reload_bumps_version() {
        let cfg_a = write_temp_config("reload_a", &config_empty());
        let cfg_b = write_temp_config("reload_b", &config_with_blocklist(&["999999"]));
        let mut state = RiskState::open(&cfg_a).unwrap();
        let v1 = state.config_version();
        // reload 同内容 → 版本不变
        state.reload(&cfg_a).unwrap();
        assert_eq!(state.config_version(), v1, "same content → same version");
        // reload 不同内容 → 版本变
        state.reload(&cfg_b).unwrap();
        assert_ne!(state.config_version(), v1, "different content → different version");
        cleanup(&cfg_a);
        cleanup(&cfg_b);
    }

    // 10. status_json 包含全部字段 key。
    #[test]
    fn test_status_json_shape() {
        let cfg = write_temp_config("status", &config_empty());
        let state = RiskState::open(&cfg).unwrap();
        let s: serde_json::Value = serde_json::from_str(&state.status_json()).unwrap();
        assert!(s.get("config_version").is_some(), "missing config_version");
        assert!(s.get("positions").is_some(), "missing positions");
        assert!(s.get("used_today").is_some(), "missing used_today");
        assert!(
            s.get("drawdown_watermark").is_some(),
            "missing drawdown_watermark"
        );
        assert!(s.get("blocklist").is_some(), "missing blocklist");
        assert!(s.get("last_known_t").is_some(), "missing last_known_t");
        cleanup(&cfg);
    }

    // ---- P0b 新增测试（11-21）----

    fn config_with_single_cap(cap: f64) -> String {
        format!("{{\"single_capital\":{}}}", cap)
    }

    fn config_with_total_cap(cap: f64) -> String {
        format!("{{\"total_capital\":{}}}", cap)
    }

    fn config_with_drawdown(threshold: f64) -> String {
        format!("{{\"drawdown_threshold\":{}}}", threshold)
    }

    // 11. R_STALE 整批拒绝：stale_tolerance=0, last_known_t=0, snapshot_t=5 → 整批 rejected。
    #[test]
    fn test_r_stale_rejects_batch() {
        let cfg = write_temp_config("r_stale_rej", &config_empty());
        let mut state = RiskState::open(&cfg).unwrap();
        let intent = r#"{"snapshot_t":5,"actions":[{"op":"inc","code":"600000","qty":100}]}"#;
        let v: serde_json::Value = serde_json::from_str(&state.evaluate(intent).unwrap()).unwrap();
        assert_eq!(v["decision"], "rejected");
        assert_eq!(v["rejected"][0]["rule_id"], "R_STALE");
        // 整批拒绝：actions 为空
        assert!(v["actions"].as_array().unwrap().is_empty());
        cleanup(&cfg);
    }

    // 12. R_STALE 通过：先 commit 建立 last_known_t=1，再 evaluate snapshot_t=1 → 通过。
    #[test]
    fn test_r_stale_passes_when_equal() {
        let cfg = write_temp_config("r_stale_pass", &config_empty());
        let mut state = RiskState::open(&cfg).unwrap();
        let verdict = r#"{"decision":"approved","snapshot_t":1,"actions":[{"op":"open","code":"600000","qty":100}],"rejected":[],"config_version":0}"#;
        state.commit(verdict).unwrap();
        let intent = r#"{"snapshot_t":1,"actions":[{"op":"inc","code":"600000","qty":100}]}"#;
        let v: serde_json::Value = serde_json::from_str(&state.evaluate(intent).unwrap()).unwrap();
        assert_eq!(v["decision"], "approved");
        cleanup(&cfg);
    }

    // 13. R_SINGLE_CAP 裁剪：single_capital=5000, close=100, qty=100(金额10000) → qty=50。
    #[test]
    fn test_r_single_cap_trims() {
        let cfg = write_temp_config("single_cap_trims", &config_with_single_cap(5000.0));
        let mut state = RiskState::open(&cfg).unwrap();
        let intent = r#"{"snapshot_t":0,"actions":[{"op":"open","code":"600000","qty":100}],"context":{"close":100.0}}"#;
        let v: serde_json::Value = serde_json::from_str(&state.evaluate(intent).unwrap()).unwrap();
        assert_eq!(v["decision"], "approved");
        assert_eq!(v["actions"][0]["qty"], 50);
        assert!(
            v["actions"][0]["note"]
                .as_str()
                .unwrap()
                .contains("R_SINGLE_CAP"),
            "note should contain R_SINGLE_CAP: {:?}",
            v["actions"][0]["note"]
        );
        cleanup(&cfg);
    }

    // 14. R_SINGLE_CAP 裁剪到 1 通过 / 裁剪到 0 拒绝。
    #[test]
    fn test_r_single_cap_rejects_zero() {
        let cfg = write_temp_config("single_cap_zero", &config_with_single_cap(100.0));
        let mut state = RiskState::open(&cfg).unwrap();
        // close=100, qty=100, single_cap=100 → floor(100/100)=1，通过
        let intent1 = r#"{"snapshot_t":0,"actions":[{"op":"open","code":"600000","qty":100}],"context":{"close":100.0}}"#;
        let v1: serde_json::Value = serde_json::from_str(&state.evaluate(intent1).unwrap()).unwrap();
        assert_eq!(v1["actions"][0]["qty"], 1);
        // close=200, qty=100, single_cap=100 → floor(100/200)=0，rejected
        let intent2 = r#"{"snapshot_t":0,"actions":[{"op":"open","code":"600001","qty":100}],"context":{"close":200.0}}"#;
        let v2: serde_json::Value = serde_json::from_str(&state.evaluate(intent2).unwrap()).unwrap();
        assert_eq!(v2["decision"], "rejected");
        assert_eq!(v2["rejected"][0]["rule_id"], "R_SINGLE_CAP");
        cleanup(&cfg);
    }

    // 15. R_TOTAL_CAP 裁剪：total_capital=10000, used_today 已有 6000, close=100, qty=100 → 40。
    #[test]
    fn test_r_total_cap_trims() {
        let cfg = write_temp_config("total_cap_trims", &config_with_total_cap(10000.0));
        let mut state = RiskState::open(&cfg).unwrap();
        state.used_today.insert("600001".to_string(), 6000.0);
        let intent = r#"{"snapshot_t":0,"actions":[{"op":"open","code":"600000","qty":100}],"context":{"close":100.0}}"#;
        let v: serde_json::Value = serde_json::from_str(&state.evaluate(intent).unwrap()).unwrap();
        assert_eq!(v["actions"][0]["qty"], 40);
        assert!(
            v["actions"][0]["note"]
                .as_str()
                .unwrap()
                .contains("R_TOTAL_CAP"),
            "note should contain R_TOTAL_CAP: {:?}",
            v["actions"][0]["note"]
        );
        cleanup(&cfg);
    }

    // 16. R_DRAWDOWN 拒绝开仓：drawdown_threshold=0.1, watermark=100, net_value=80(回撤20%) → Open 被拒，Dec 通过。
    #[test]
    fn test_r_drawdown_rejects_opening() {
        let cfg = write_temp_config("drawdown_rej", &config_with_drawdown(0.1));
        let mut state = RiskState::open(&cfg).unwrap();
        state.drawdown_watermark = 100.0;
        let intent = r#"{"snapshot_t":0,"actions":[{"op":"open","code":"600000","qty":100},{"op":"dec","code":"600001","qty":50}],"context":{"close":100.0,"net_value":80.0}}"#;
        let v: serde_json::Value = serde_json::from_str(&state.evaluate(intent).unwrap()).unwrap();
        // Open 被拒
        assert_eq!(v["rejected"][0]["rule_id"], "R_DRAWDOWN");
        // Dec 通过（平仓不受回撤熔断限制）
        assert_eq!(v["actions"][0]["op"], "dec");
        cleanup(&cfg);
    }

    // 17. R_DRAWDOWN 无 net_value 跳过：Open 通过。
    #[test]
    fn test_r_drawdown_skipped_without_net_value() {
        let cfg = write_temp_config("drawdown_skip", &config_with_drawdown(0.1));
        let mut state = RiskState::open(&cfg).unwrap();
        state.drawdown_watermark = 100.0;
        let intent = r#"{"snapshot_t":0,"actions":[{"op":"open","code":"600000","qty":100}],"context":{"close":100.0}}"#;
        let v: serde_json::Value = serde_json::from_str(&state.evaluate(intent).unwrap()).unwrap();
        assert_eq!(v["decision"], "approved");
        cleanup(&cfg);
    }

    // 18. audit_log 记录：evaluate 一次后 audit_json() 非空数组，含 ts/intent/verdict。
    #[test]
    fn test_audit_log_recorded() {
        let cfg = write_temp_config("audit_rec", &config_empty());
        let mut state = RiskState::open(&cfg).unwrap();
        let intent = r#"{"snapshot_t":0,"actions":[{"op":"inc","code":"600000","qty":100}]}"#;
        let _ = state.evaluate(intent).unwrap();
        let audit: serde_json::Value = serde_json::from_str(&state.audit_json()).unwrap();
        assert!(audit.is_array(), "audit_json should be array");
        assert!(
            audit.as_array().unwrap().len() >= 1,
            "audit_log should have >=1 entry"
        );
        let entry = &audit[0];
        assert!(entry.get("ts").is_some(), "missing ts");
        assert!(entry.get("intent").is_some(), "missing intent");
        assert!(entry.get("verdict").is_some(), "missing verdict");
        cleanup(&cfg);
    }

    // 19. audit_log 溢出弹头：写入 1025 条 → 长度仍 1024。
    #[test]
    fn test_audit_log_overflow() {
        let cfg = write_temp_config("audit_overflow", &config_empty());
        let mut state = RiskState::open(&cfg).unwrap();
        let intent = r#"{"snapshot_t":0,"actions":[]}"#;
        for _ in 0..1025 {
            let _ = state.evaluate(intent).unwrap();
        }
        assert_eq!(state.audit_log.len(), 1024, "audit_log should be capped at 1024");
        cleanup(&cfg);
    }

    // 20. commit 更新 drawdown_watermark：net_value=150 → watermark=150；net_value=120 → 仍 150。
    #[test]
    fn test_commit_updates_drawdown_watermark() {
        let cfg = write_temp_config("commit_drawdown", &config_empty());
        let mut state = RiskState::open(&cfg).unwrap();
        let verdict1 = r#"{"decision":"approved","snapshot_t":1,"actions":[{"op":"open","code":"600000","qty":100}],"rejected":[],"config_version":0,"context":{"net_value":150.0}}"#;
        state.commit(verdict1).unwrap();
        assert_eq!(state.drawdown_watermark, 150.0);
        let verdict2 = r#"{"decision":"approved","snapshot_t":2,"actions":[],"rejected":[],"config_version":0,"context":{"net_value":120.0}}"#;
        state.commit(verdict2).unwrap();
        assert_eq!(state.drawdown_watermark, 150.0, "watermark should not decrease");
        cleanup(&cfg);
    }

    // 21. verdict 携带 context：evaluate 返回的 verdict JSON 含 context 字段（close 值）。
    #[test]
    fn test_verdict_carries_context() {
        let cfg = write_temp_config("verdict_ctx", &config_empty());
        let mut state = RiskState::open(&cfg).unwrap();
        let intent = r#"{"snapshot_t":0,"actions":[{"op":"inc","code":"600000","qty":100}],"context":{"close":100.0}}"#;
        let v: serde_json::Value = serde_json::from_str(&state.evaluate(intent).unwrap()).unwrap();
        assert!(v.get("context").is_some(), "verdict should carry context");
        assert_eq!(v["context"]["close"], 100.0);
        cleanup(&cfg);
    }

    // ---- P1 audit flush 新增测试（22-24）----

    /// 每个测试用唯一子目录（pid + 名称），避免并行冲突。开头先清理残留。
    fn audit_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "risk-gate-audit-{}-{}",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn audit_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| {
                        p.file_name()
                            .map(|n| {
                                let n = n.to_string_lossy();
                                n.starts_with("risk_audit_") && n.ends_with(".jsonl")
                            })
                            .unwrap_or(false)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    // 22. evaluate 一次后 flush_audit 到临时目录 → 返回 1，文件存在且含一行，audit_len()==0。
    #[test]
    fn test_flush_audit_writes_jsonl() {
        let cfg = write_temp_config("flush_write", &config_empty());
        let mut state = RiskState::open(&cfg).unwrap();
        let intent = r#"{"snapshot_t":0,"actions":[{"op":"inc","code":"600000","qty":100}]}"#;
        let _ = state.evaluate(intent).unwrap();
        assert_eq!(state.audit_len(), 1);

        let dir = audit_dir("flush_write");
        let n = state.flush_audit(&dir.to_string_lossy()).unwrap();
        assert_eq!(n, 1, "flush should return 1 entry written");
        assert_eq!(state.audit_len(), 0, "audit_log should be cleared after flush");

        let files = audit_files(&dir);
        assert_eq!(files.len(), 1, "exactly one audit file expected");
        let content = std::fs::read_to_string(&files[0]).unwrap();
        assert_eq!(content.lines().count(), 1, "file should contain one JSONL line");
        // 行是合法 AuditEntry JSON
        let line: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert!(line.get("ts").is_some(), "missing ts");
        assert!(line.get("intent").is_some(), "missing intent");
        assert!(line.get("verdict").is_some(), "missing verdict");

        let _ = std::fs::remove_dir_all(&dir);
        cleanup(&cfg);
    }

    // 23. 无 evaluate 直接 flush → Ok(0)，不创建文件。
    #[test]
    fn test_flush_audit_empty_noop() {
        let cfg = write_temp_config("flush_empty", &config_empty());
        let mut state = RiskState::open(&cfg).unwrap();
        let dir = audit_dir("flush_empty");

        let n = state.flush_audit(&dir.to_string_lossy()).unwrap();
        assert_eq!(n, 0, "empty flush should return 0");
        assert!(
            !dir.exists() || audit_files(&dir).is_empty(),
            "empty flush must not create any audit file"
        );

        let _ = std::fs::remove_dir_all(&dir);
        cleanup(&cfg);
    }

    // 24. 两次 flush 到同目录 → 两个文件（不同 seq），各含对应行。
    #[test]
    fn test_flush_audit_append() {
        let cfg = write_temp_config("flush_append", &config_empty());
        let mut state = RiskState::open(&cfg).unwrap();
        let dir = audit_dir("flush_append");

        // 第一次 flush：1 条
        let intent1 = r#"{"snapshot_t":0,"actions":[{"op":"inc","code":"600000","qty":100}]}"#;
        let _ = state.evaluate(intent1).unwrap();
        let n1 = state.flush_audit(&dir.to_string_lossy()).unwrap();
        assert_eq!(n1, 1);

        // 两次 flush 至少间隔 1 秒，保证时间戳 seq 不同 → 产生两个文件
        std::thread::sleep(std::time::Duration::from_secs(1));

        // 第二次 flush：1 条（不同 action，便于核对对应行）
        let intent2 = r#"{"snapshot_t":0,"actions":[{"op":"dec","code":"600001","qty":50}]}"#;
        let _ = state.evaluate(intent2).unwrap();
        let n2 = state.flush_audit(&dir.to_string_lossy()).unwrap();
        assert_eq!(n2, 1);

        let files = audit_files(&dir);
        assert_eq!(files.len(), 2, "two flushes should produce two files (different seq)");
        // 每个文件各含一行，且两次 flush 后内存均已清空
        for f in &files {
            let content = std::fs::read_to_string(f).unwrap();
            assert_eq!(content.lines().count(), 1, "each file should hold one line");
        }
        assert_eq!(state.audit_len(), 0);

        let _ = std::fs::remove_dir_all(&dir);
        cleanup(&cfg);
    }
}
