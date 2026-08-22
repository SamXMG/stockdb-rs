//! 点时行业强度与资金流上下文特征。
//!
//! 该模块只读取信号日及以前的 RawDailyBar、MoneyFlowHistory、IndustryDaily
//! 和版本化行业归属。输出为 CompactFactor，Python 只负责编排和读取。

use rayon::prelude::*;
use serde_json::Value as Json;
use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use crate::{compact, flow, Record, Store, Value};

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
