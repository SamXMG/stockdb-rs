//! 点时行业强度与资金流上下文特征。
//!
//! 该模块只读取信号日及以前的 RawDailyBar、MoneyFlowHistory、IndustryDaily
//! 和版本化行业归属。输出为 CompactFactor，Python 只负责编排和读取。

use rayon::prelude::*;
use serde_json::Value as Json;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use crate::{compact, flow, Record, Store, Value};

/// 模块级全市场聚合缓存：按 (root, industry_history) 缓存最近构建的 MarketAggCache，
/// 跨多次 `aggregate_group_daily` 复用，避免对同一数据重复全扫 RawDailyBar。
/// RawDailyBar/行业归属变更后，进程重启即失效（新 key 或 OnceLock 重建）。
static MARKET_AGG_CACHE: OnceLock<Mutex<HashMap<String, Arc<MarketAggCache>>>> = OnceLock::new();

fn get_or_build_cache(store: &Store, industry_history: &Path) -> Result<Arc<MarketAggCache>, String> {
    let lock = MARKET_AGG_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let root = store.root_dir().display().to_string();
    // 缓存 key 必须包含 industry_history 路径，否则错误的 history 路径会污染缓存
    let key = format!("{root}\0{}", industry_history.display());
    let mut map = lock.lock().map_err(|_| "market agg cache poisoned".to_string())?;
    if let Some(c) = map.get(&key) {
        return Ok(c.clone());
    }
    let cache = Arc::new(build_market_cache(store, industry_history)?);
    map.insert(key, cache.clone());
    Ok(cache)
}

const COLUMNS: &[&str] = &[
    "flow_main_pct_1d",
    "flow_main_net_5d_ratio",
    "flow_positive_rate_5d",
    "flow_source_quality",
    "industry_ret_1d",
    "industry_ret_5d",
    "industry_relative_20d",
    "industry_advance_rate",
    "industry_above_ma20_rate",
    "board_relative_20d",
    "board_advance_rate",
    "context_industry_available",
    "context_board_available",
    "context_flow_available",
];

#[derive(Clone, Default)]
struct GroupMeta {
    id: String,
}

#[derive(Clone, Default)]
struct IndustryVersion {
    effective_from: String,
    industry: String,
}

fn number(record: &Record, table: &str, field: &str) -> f64 {
    match record.get(table, field) {
        Some(Value::F64(value)) => *value,
        Some(Value::I64(value)) => *value as f64,
        _ => f64::NAN,
    }
}

fn board_for_code(code: &str) -> &'static str {
    let full = code.trim();
    if full.starts_with("300") || full.starts_with("301") {
        "创业板"
    } else if full.starts_with("688") || full.starts_with("689") {
        "科创板"
    } else if full.starts_with('4') || full.starts_with('8') || full.starts_with('9') {
        "北交所"
    } else if full.starts_with('6') {
        "沪市主板"
    } else if full.starts_with(['0', '2', '3']) {
        "深市主板"
    } else {
        "其他"
    }
}

fn load_versions(path: &Path) -> Result<HashMap<String, Vec<IndustryVersion>>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("industry history {}: {e}", path.display()))?;
    let raw: Json =
        serde_json::from_str(&text).map_err(|e| format!("industry history json: {e}"))?;
    let records = raw.get("records").unwrap_or(&raw);
    let mut out = HashMap::new();
    let object = records
        .as_object()
        .ok_or_else(|| "industry history records must be object".to_string())?;
    for (code, rows) in object {
        let mut versions = Vec::new();
        if let Some(items) = rows.as_array() {
            for row in items {
                let effective_from = row
                    .get("effective_from")
                    .and_then(Json::as_str)
                    .unwrap_or("")
                    .to_string();
                let industry = row
                    .get("industry")
                    .and_then(Json::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if !effective_from.is_empty() && !industry.is_empty() {
                    versions.push(IndustryVersion {
                        effective_from,
                        industry,
                    });
                }
            }
        }
        versions.sort_by(|a, b| a.effective_from.cmp(&b.effective_from));
        out.insert(code.trim().to_string(), versions);
    }
    Ok(out)
}

fn load_groups(root: &Path) -> Result<HashMap<(String, String), GroupMeta>, String> {
    let path = root.join("IndustryDaily").join("manifest.json");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("industry manifest {}: {e}", path.display()))?;
    let raw: Json =
        serde_json::from_str(&text).map_err(|e| format!("industry manifest json: {e}"))?;
    let groups = raw
        .get("groups")
        .and_then(Json::as_object)
        .ok_or_else(|| "industry manifest groups missing".to_string())?;
    let mut out = HashMap::new();
    for (id, item) in groups {
        let group_type = item.get("group_type").and_then(Json::as_str).unwrap_or("");
        let name = item.get("name").and_then(Json::as_str).unwrap_or("");
        if !group_type.is_empty() && !name.is_empty() {
            out.insert(
                (group_type.to_string(), name.to_string()),
                GroupMeta { id: id.clone() },
            );
        }
    }
    Ok(out)
}

fn version_at<'a>(versions: &'a [IndustryVersion], date: &str) -> Option<&'a str> {
    versions
        .iter()
        .rev()
        .find(|row| row.effective_from.as_str() <= date)
        .map(|row| row.industry.as_str())
}

fn group_row(store: &Store, meta: Option<&GroupMeta>, t: usize) -> Option<Record> {
    let meta = meta?;
    store.read_at("IndustryDaily", &meta.id, t).ok().flatten()
}

fn source_quality(source: u8) -> f64 {
    match flow::source_name(source) {
        "eastmoney_fflow" | "eastmoney" | "fuyao" => 1.0,
        "sina_moneyflow" => 0.5,
        _ => 0.0,
    }
}

fn context_row(
    store: &Store,
    code: &str,
    versions: &HashMap<String, Vec<IndustryVersion>>,
    groups: &HashMap<(String, String), GroupMeta>,
    flow_rows: &HashMap<i64, flow::FlowRow>,
    record_by_t: &HashMap<i64, &Record>,
    t: usize,
) -> Vec<f32> {
    let mut out = vec![f32::NAN; COLUMNS.len()];
    let date = match store.calendar().t_to_date(t) {
        Some(value) => value.to_string(),
        None => return out,
    };

    let mut flow_count = 0usize;
    let mut flow_positive = 0usize;
    let mut flow_quality: f64 = 0.0;
    let mut flow_net = 0.0;
    let mut amount = 0.0;
    for day in t.saturating_sub(4)..=t {
        if let Some(row) = flow_rows.get(&(day as i64)) {
            if row.main_net.is_finite() {
                flow_net += row.main_net;
                flow_count += 1;
                if row.main_net > 0.0 {
                    flow_positive += 1;
                }
            }
            flow_quality = flow_quality.max(source_quality(row.source));
        }
        if let Some(record) = record_by_t.get(&(day as i64)) {
            let value = number(record, "RawDailyBar", "amount");
            if value.is_finite() && value > 0.0 {
                amount += value;
            }
        }
    }
    if let Some(row) = flow_rows.get(&(t as i64)) {
        if row.main_pct.is_finite() {
            out[0] = row.main_pct as f32;
        }
    }
    if flow_count > 0 {
        out[1] = if amount > 0.0 {
            (flow_net / amount) as f32
        } else {
            f32::NAN
        };
        out[2] = (flow_positive as f64 / flow_count as f64) as f32;
        out[3] = flow_quality as f32;
        out[13] = 1.0;
    }

    let industry_name = versions
        .get(code)
        .and_then(|items| version_at(items, &date))
        .unwrap_or("");
    let industry_meta = groups.get(&("industry".to_string(), industry_name.to_string()));
    let board_name = board_for_code(code);
    let board_meta = groups.get(&("board".to_string(), board_name.to_string()));
    if let Some(row) = group_row(store, industry_meta, t) {
        for (idx, field) in [
            (4, "ret_1d"),
            (5, "ret_5d"),
            (6, "relative_20d"),
            (7, "advance_rate"),
            (8, "above_ma20_rate"),
        ] {
            let value = number(&row, "IndustryDaily", field);
            if value.is_finite() {
                out[idx] = value as f32;
            }
        }
        out[11] = 1.0;
    }
    if let Some(row) = group_row(store, board_meta, t) {
        for (idx, field) in [(9, "relative_20d"), (10, "advance_rate")] {
            let value = number(&row, "IndustryDaily", field);
            if value.is_finite() {
                out[idx] = value as f32;
            }
        }
        out[12] = 1.0;
    }
    out
}

pub fn columns() -> Vec<String> {
    COLUMNS.iter().map(|x| (*x).to_string()).collect()
}

pub fn materialize(
    store: &Store,
    codes: Option<&[String]>,
    industry_history: &Path,
    out_dir: &Path,
) -> Result<String, String> {
    let started = Instant::now();
    let selected = match codes {
        Some(items) => {
            let mut values = items.to_vec();
            values.sort();
            values.dedup();
            values
        }
        None => store.codes("RawDailyBar").map_err(|e| e.to_string())?,
    };
    let versions = load_versions(industry_history)?;
    let groups = load_groups(store.root_dir())?;
    std::fs::create_dir_all(out_dir).map_err(|e| e.to_string())?;
    let names = columns();
    let results: Result<Vec<(usize, u64)>, String> = selected
        .par_iter()
        .map(|code| {
            let records = store
                .read_mmap("RawDailyBar", code)
                .map_err(|e| format!("{code}: {e}"))?;
            let flow_rows = (if store.flow_exists(code) {
                store.read_flow(code).map_err(|e| format!("{code}: {e}"))?
            } else {
                Vec::new()
            })
            .into_iter()
            .map(|row| (row.t, row))
            .collect::<HashMap<_, _>>();
            let mut rows = Vec::with_capacity(records.len());
            let record_by_t: HashMap<i64, &Record> = records.iter().map(|r| (r.t, r)).collect();
            for record in &records {
                rows.push((
                    record.t as u32,
                    context_row(
                        store,
                        code,
                        &versions,
                        &groups,
                        &flow_rows,
                        &record_by_t,
                        record.t as usize,
                    ),
                ));
            }
            let path = out_dir.join(format!("{code}.mtx"));
            compact::write_file(&path, &names, &rows)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            let bytes = std::fs::metadata(&path).map_err(|e| e.to_string())?.len();
            Ok((rows.len(), bytes))
        })
        .collect();
    let results = results?;
    serde_json::to_string(&serde_json::json!({
        "table": "RawDailyBar",
        "files": results.len(),
        "rows": results.iter().map(|x| x.0).sum::<usize>(),
        "columns": names,
        "bytes": results.iter().map(|x| x.1).sum::<u64>(),
        "elapsed_ms": started.elapsed().as_millis(),
        "output": out_dir.to_string_lossy(),
        "industry_history": industry_history,
    }))
    .map_err(|e| e.to_string())
}

/// 实时横截面聚合视图（Rust 侧，不落盘）。
///
/// 从 RawDailyBar 实时聚合某 group_type+name 在指定 date 的行业/板块日线指标，
/// 口径与 Python ingest_industry_daily.py 完全一致。返回 JSON 字符串（键见下），
/// group 无数据或未通过 min_members 门槛时返回空 dict 的 JSON "{}"。
///
/// 输入:
///   - store: 已打开的 StockDB（含 RawDailyBar + calendar）
///   - group_type: "industry" | "board"
///   - name: 行业名或板块名
///   - date: yyyy-mm-dd（须在 calendar 内）
///   - industry_history: 行业归属 JSON 路径（board 聚合可传空，仅 industry 需要）
///   - min_members: 成员数门槛（默认 3）
///
/// 内部按 root 缓存全市场聚合（跨多次查询复用，避免重复全扫 RawDailyBar）；
/// RawDailyBar 变更后由调用方负责失效（重启/close 后重建）。
///
/// 输出 JSON 键: group_id/group_type/name/date/t/member_count/
///   ret_1d/ret_5d/ret_20d/relative_20d/above_ma20_rate/advance_rate/amount_share
pub fn aggregate_group_daily(
    store: &Store,
    group_type: &str,
    name: &str,
    date: &str,
    industry_history: &Path,
    min_members: usize,
) -> Result<String, String> {
    let min_members = min_members.max(1);
    let t = store
        .calendar()
        .date_to_t(date)
        .ok_or_else(|| format!("date {date} not in calendar"))?;
    let cache = get_or_build_cache(store, industry_history)?;
    // 目标 group 的 id 与成员
    let target_id = match group_type {
        "board" => group_id("board", name),
        _ => group_id("industry", name),
    };
    let mut bucket: [f64; 7] = [0.0; 7]; // [count, ret1, ret5, ret20, above, advance, amount]
    if t < cache.groups.len() {
        if let Some(b) = cache.groups[t].get(&target_id) {
            bucket = *b;
        }
    }
    let member_count = bucket[0];
    if member_count < min_members as f64 {
        return Ok("{}".to_string());
    }
    let ret_1d = bucket[1] / member_count;
    let ret_5d = bucket[2] / member_count;
    let ret_20d = bucket[3] / member_count;
    let benchmark = cache.market_ret20_mean(t);
    let relative_20d = match benchmark {
        Some(b) => ret_20d - b,
        None => f64::NAN,
    };
    let market_amount = cache.market_amount.get(t).copied().unwrap_or(0.0);
    let amount_share = if market_amount > 0.0 {
        bucket[6] / market_amount
    } else {
        f64::NAN
    };
    Ok(serde_json::json!({
        "group_id": target_id,
        "group_type": group_type,
        "name": name,
        "date": date,
        "t": t,
        "member_count": member_count as u64,
        "ret_1d": ret_1d,
        "ret_5d": ret_5d,
        "ret_20d": ret_20d,
        "relative_20d": relative_20d,
        "above_ma20_rate": bucket[4] / member_count,
        "advance_rate": bucket[5] / member_count,
        "amount_share": amount_share,
    })
    .to_string())
}

/// 全市场聚合缓存：按 t 保存每个 group 的累加桶，以及市场级 ret20/amount。
/// 构建一次可复用于任意 group/date 查询，避免重复全扫 RawDailyBar。
#[derive(Default)]
pub struct MarketAggCache {
    /// groups[t][group_id] = [count, ret1, ret5, ret20, above, advance, amount]
    groups: Vec<HashMap<String, [f64; 7]>>,
    /// 市场级 ret20 累加与计数（按 t）
    market_ret20_sum: Vec<f64>,
    market_ret20_count: Vec<u64>,
    market_amount: Vec<f64>,
}

impl MarketAggCache {
    fn market_ret20_mean(&self, t: usize) -> Option<f64> {
        let c = self.market_ret20_count.get(t).copied().unwrap_or(0);
        if c == 0 {
            return None;
        }
        Some(self.market_ret20_sum.get(t).copied().unwrap_or(0.0) / c as f64)
    }
}

fn group_id(group_type: &str, name: &str) -> String {
    // 与 Python group_id 同规则：group_type + 名（截断/规范化见 ingest 参考实现）。
    format!("{group_type}_{}", sanitize_group_name(name))
}

fn sanitize_group_name(name: &str) -> String {
    // Python group_id 用定宽截断；这里保留完整名（与 manifest 一致即可）。
    name.trim().to_string()
}

fn build_market_cache(store: &Store, industry_history: &Path) -> Result<MarketAggCache, String> {
    let started = Instant::now();
    let versions = load_versions(industry_history)?;
    let calendar = store.calendar();
    let cal_len = calendar.len();
    let mut groups: Vec<HashMap<String, [f64; 7]>> = (0..cal_len).map(|_| HashMap::new()).collect();
    let mut market_ret20_sum = vec![0.0; cal_len];
    let mut market_ret20_count = vec![0u64; cal_len];
    let mut market_amount = vec![0.0; cal_len];

    let codes = store.codes("RawDailyBar").map_err(|e| e.to_string())?;
    for code in &codes {
        let records = store
            .read_mmap("RawDailyBar", code)
            .map_err(|e| format!("{code}: {e}"))?;
        if records.len() < 21 {
            continue;
        }
        let board = board_for_code(code);
        let board_id = group_id("board", board);
        let industries = versions.get(code).cloned().unwrap_or_default();
        for (i, r) in records.iter().enumerate() {
            let t = r.t as usize;
            if t >= cal_len {
                continue;
            }
            // 需要 t-20 之前的数据计算 ret20/ma20；不足则跳过该日
            if i < 20 {
                continue;
            }
            let close = match r.get("RawDailyBar", "close") {
                Some(Value::F64(v)) => *v,
                Some(Value::I64(v)) => *v as f64,
                _ => f64::NAN,
            };
            let amount = match r.get("RawDailyBar", "amount") {
                Some(Value::F64(v)) => *v,
                Some(Value::I64(v)) => *v as f64,
                _ => 0.0,
            };
            if !close.is_finite() || close <= 0.0 {
                continue;
            }
            let prev = match records[i - 1].get("RawDailyBar", "close") {
                Some(Value::F64(v)) => *v,
                Some(Value::I64(v)) => *v as f64,
                _ => f64::NAN,
            };
            let c20 = match records[i - 20].get("RawDailyBar", "close") {
                Some(Value::F64(v)) => *v,
                Some(Value::I64(v)) => *v as f64,
                _ => f64::NAN,
            };
            let c5 = if i >= 5 {
                match records[i - 5].get("RawDailyBar", "close") {
                    Some(Value::F64(v)) => *v,
                    Some(Value::I64(v)) => *v as f64,
                    _ => f64::NAN,
                }
            } else {
                f64::NAN
            };
            if !prev.is_finite() || prev <= 0.0 || !c20.is_finite() || c20 <= 0.0 {
                continue;
            }
            // MA20
            let mut ma20 = 0.0;
            for j in (i.saturating_sub(19)..=i).rev() {
                if let Some(cc) = records.get(j) {
                    match cc.get("RawDailyBar", "close") {
                        Some(Value::F64(v)) => ma20 += *v,
                        Some(Value::I64(v)) => ma20 += *v as f64,
                        _ => {}
                    }
                }
            }
            ma20 /= 20.0;
            let ret1 = close / prev - 1.0;
            let ret5 = if c5.is_finite() && c5 > 0.0 { close / c5 - 1.0 } else { f64::NAN };
            let ret20 = close / c20 - 1.0;
            // 板 block（所有成员都计入，含无行业归属的）
            let b = groups[t].entry(board_id.clone()).or_insert([0.0; 7]);
            b[0] += 1.0;
            b[1] += ret1;
            if ret5.is_finite() {
                b[2] += ret5;
            }
            b[3] += ret20;
            b[4] += if close >= ma20 { 1.0 } else { 0.0 };
            b[5] += if close >= prev { 1.0 } else { 0.0 };
            b[6] += amount;
            market_ret20_sum[t] += ret20;
            market_ret20_count[t] += 1;
            market_amount[t] += amount;
            // 行业 block（需时点行业归属）
            // read_mmap 的 record 不含 date 文本, 用 t 从日历反查日期
            let date_str = calendar.t_to_date(t).unwrap_or("");
            if let Some(industry) = version_at(&industries, date_str) {
                let iid = group_id("industry", industry);
                let ib = groups[t].entry(iid).or_insert([0.0; 7]);
                ib[0] += 1.0;
                ib[1] += ret1;
                if ret5.is_finite() {
                    ib[2] += ret5;
                }
                ib[3] += ret20;
                ib[4] += if close >= ma20 { 1.0 } else { 0.0 };
                ib[5] += if close >= prev { 1.0 } else { 0.0 };
                ib[6] += amount;
            }
        }
    }
    eprintln!(
        "[context] MarketAggCache built: {} codes, cal={}, elapsed_ms={}",
        codes.len(),
        cal_len,
        started.elapsed().as_millis()
    );
    Ok(MarketAggCache {
        groups,
        market_ret20_sum,
        market_ret20_count,
        market_amount,
    })
}
