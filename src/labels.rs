//! 严格前瞻标签计算：Rust 直接读取 StockDB，并写入 CompactLabel。

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use rayon::prelude::*;

use crate::{compact, Record, Store, Value};

#[derive(Debug, Clone, Copy)]
pub struct ForwardLabelConfig {
    pub horizon: usize,
    pub buy_cost: f64,
    pub sell_cost: f64,
    pub gain_threshold: f64,
    pub loss_threshold: f64,
    pub max_entry_gap: f64,
}

impl Default for ForwardLabelConfig {
    fn default() -> Self {
        Self {
            horizon: 5,
            buy_cost: 0.00125,
            sell_cost: 0.00175,
            gain_threshold: 0.03,
            loss_threshold: 0.02,
            max_entry_gap: 0.07,
        }
    }
}

fn number(record: &Record, table: &str, field: &str) -> f64 {
    match record.get(table, field) {
        Some(Value::F64(value)) => *value,
        Some(Value::I64(value)) => *value as f64,
        _ => f64::NAN,
    }
}

pub fn columns(config: ForwardLabelConfig) -> Vec<String> {
    let h = config.horizon;
    vec![
        format!("return_{h}d_net"),
        format!("drawdown_{h}d"),
        format!("days_to_high_{h}d"),
        format!("days_to_gain_{h}d"),
        format!("max_favorable_excursion_{h}d"),
        format!("hit_gain_before_loss_{h}d"),
        format!("entry_t_{h}d"),
        format!("entry_tradable_{h}d"),
        format!("entry_limit_up_{h}d"),
        format!("entry_gap_too_high_{h}d"),
        format!("exit_tradable_{h}d"),
        format!("exit_limit_down_{h}d"),
        format!("suspended_days_{h}d"),
        format!("window_complete_{h}d"),
        "entry_gap_1d".to_string(),
    ]
}

fn limit_pct(code: &str) -> f64 {
    let c = code.trim();
    if c.starts_with("300") || c.starts_with("301") || c.starts_with("688") || c.starts_with("689")
    {
        0.20
    } else if c.starts_with('4') || c.starts_with('8') || c.starts_with('9') {
        0.30
    } else {
        0.10
    }
}

fn is_limit_up(prev_close: f64, open: f64, code: &str) -> bool {
    prev_close.is_finite()
        && prev_close > 0.0
        && open.is_finite()
        && open > 0.0
        && open / prev_close - 1.0 >= limit_pct(code) - 0.002
}

fn is_locked_limit_down(prev_close: f64, row: Option<&Record>, table: &str, code: &str) -> bool {
    let row = match row {
        Some(value) => value,
        None => return false,
    };
    let high = number(row, table, "high");
    let low = number(row, table, "low");
    let close = number(row, table, "close");
    prev_close.is_finite()
        && prev_close > 0.0
        && high.is_finite()
        && high > 0.0
        && low.is_finite()
        && close.is_finite()
        && close / prev_close - 1.0 <= -limit_pct(code) + 0.002
        && (high - low).abs() / high <= 0.001
}

pub fn compute_rows(
    store: &Store,
    table: &str,
    code: &str,
    config: ForwardLabelConfig,
) -> Result<Vec<(u32, Vec<f32>)>, String> {
    if config.horizon == 0 {
        return Err("horizon must be positive".to_string());
    }
    if config.buy_cost < 0.0
        || config.sell_cost < 0.0
        || config.gain_threshold <= 0.0
        || config.loss_threshold <= 0.0
        || config.max_entry_gap < 0.0
    {
        return Err("costs must be non-negative and thresholds must be positive".to_string());
    }
    let mut records = store
        .read_mmap(table, code)
        .map_err(|e| format!("{code}: {e}"))?;
    records.sort_by_key(|record| record.t);
    let calendar_len = store.calendar().len();
    if calendar_len <= config.horizon {
        return Ok(Vec::new());
    }
    let by_t: HashMap<i64, &Record> = records.iter().map(|r| (r.t, r)).collect();
    let mut out = Vec::with_capacity(records.len());
    for signal in &records {
        let signal_t = signal.t as usize;
        if signal_t + config.horizon >= calendar_len {
            continue;
        }
        let future: Vec<Option<&Record>> = (1..=config.horizon)
            .map(|offset| by_t.get(&((signal_t + offset) as i64)).copied())
            .collect();
        let entry_row = future[0];
        let exit_row = future[config.horizon - 1];
        let entry = entry_row
            .map(|row| number(row, table, "open"))
            .unwrap_or(f64::NAN);
        let exit = exit_row
            .map(|row| number(row, table, "close"))
            .unwrap_or(f64::NAN);
        let prev_close = number(signal, table, "close");
        let entry_gap = if prev_close.is_finite() && prev_close > 0.0 && entry.is_finite() {
            entry / prev_close - 1.0
        } else {
            f64::NAN
        };
        let entry_limit_up = is_limit_up(prev_close, entry, code);
        let entry_gap_too_high = entry_gap.is_finite() && entry_gap > config.max_entry_gap;
        let entry_volume = entry_row
            .map(|row| number(row, table, "volume"))
            .unwrap_or(f64::NAN);
        let entry_tradable = entry.is_finite()
            && entry > 0.0
            && entry_volume.is_finite()
            && entry_volume > 0.0
            && !entry_limit_up
            && !entry_gap_too_high;
        let mut suspended_days = 0usize;
        let mut window_complete = true;
        let mut min_low = entry;
        let mut max_high = entry;
        let mut days_to_high = 1usize;
        let mut days_to_gain = config.horizon + 1;
        let mut hit = f32::NAN;
        for (offset, record) in future.iter().enumerate() {
            let record = match record {
                Some(value) => value,
                None => {
                    suspended_days += 1;
                    window_complete = false;
                    continue;
                }
            };
            let low = number(record, table, "low");
            let high = number(record, table, "high");
            if !low.is_finite() || !high.is_finite() {
                window_complete = false;
            }
            if low.is_finite() {
                min_low = min_low.min(low);
            }
            if high.is_finite() && high > max_high {
                max_high = high;
                days_to_high = offset + 1;
            }
            if days_to_gain > config.horizon
                && high.is_finite()
                && high >= entry * (1.0 + config.gain_threshold)
            {
                days_to_gain = offset + 1;
            }
            if hit.is_nan() {
                // 同一天同时触发止损与止盈时保守按先止损处理。
                if low.is_finite() && low <= entry * (1.0 - config.loss_threshold) {
                    hit = 0.0;
                } else if high.is_finite() && high >= entry * (1.0 + config.gain_threshold) {
                    hit = 1.0;
                }
            }
        }
        let net_return = if entry_tradable && exit.is_finite() && exit > 0.0 {
            exit * (1.0 - config.sell_cost) / (entry * (1.0 + config.buy_cost)) - 1.0
        } else {
            f64::NAN
        };
        let drawdown = if entry_tradable && min_low.is_finite() {
            min_low / entry - 1.0
        } else {
            f64::NAN
        };
        let max_favorable_excursion = if entry_tradable && max_high.is_finite() {
            max_high / entry - 1.0
        } else {
            f64::NAN
        };
        let exit_limit_down = is_locked_limit_down(
            exit_row
                .and_then(|row| by_t.get(&(row.t - 1)).copied())
                .map(|row| number(row, table, "close"))
                .unwrap_or(f64::NAN),
            exit_row,
            table,
            code,
        );
        let exit_volume = exit_row
            .map(|row| number(row, table, "volume"))
            .unwrap_or(f64::NAN);
        let exit_tradable = exit.is_finite()
            && exit > 0.0
            && exit_volume.is_finite()
            && exit_volume > 0.0
            && !exit_limit_down;
        out.push((
            signal.t as u32,
            vec![
                net_return as f32,
                drawdown as f32,
                days_to_high as f32,
                days_to_gain as f32,
                max_favorable_excursion as f32,
                hit,
                future[0].map(|row| row.t as f32).unwrap_or(f32::NAN),
                if entry_tradable { 1.0 } else { 0.0 },
                if entry_limit_up { 1.0 } else { 0.0 },
                if entry_gap_too_high { 1.0 } else { 0.0 },
                if exit_tradable { 1.0 } else { 0.0 },
                if exit_limit_down { 1.0 } else { 0.0 },
                suspended_days as f32,
                if window_complete { 1.0 } else { 0.0 },
                entry_gap as f32,
            ],
        ));
    }
    Ok(out)
}

pub fn materialize(
    store: &Store,
    table: &str,
    codes: Option<&[String]>,
    out_dir: &Path,
    config: ForwardLabelConfig,
) -> Result<String, String> {
    let started = Instant::now();
    let selected = match codes {
        Some(items) => {
            let mut values = items.to_vec();
            values.sort();
            values.dedup();
            values
        }
        None => store.codes(table).map_err(|e| e.to_string())?,
    };
    std::fs::create_dir_all(out_dir).map_err(|e| e.to_string())?;
    let names = columns(config);
    let results: Result<Vec<(usize, u64)>, String> = selected
        .par_iter()
        .map(|code| {
            let rows = compute_rows(store, table, code, config)?;
            let path = out_dir.join(format!("{code}.mtx"));
            compact::write_file(&path, &names, &rows)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            let bytes = std::fs::metadata(&path).map_err(|e| e.to_string())?.len();
            Ok((rows.len(), bytes))
        })
        .collect();
    let results = results?;
    serde_json::to_string(&serde_json::json!({
        "table": table,
        "files": results.len(),
        "rows": results.iter().map(|x| x.0).sum::<usize>(),
        "columns": names,
        "bytes": results.iter().map(|x| x.1).sum::<u64>(),
        "elapsed_ms": started.elapsed().as_millis(),
        "output": out_dir.to_string_lossy(),
        "horizon": config.horizon,
        "buy_cost": config.buy_cost,
        "sell_cost": config.sell_cost,
        "gain_threshold": config.gain_threshold,
        "loss_threshold": config.loss_threshold,
        "max_entry_gap": config.max_entry_gap,
    }))
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn forward_labels_use_next_open_and_conservative_barrier_order() {
        let root = std::env::temp_dir().join(format!("stockdb-labels-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("calendar.json"), "[]").unwrap();
        let store = Store::open(&root).unwrap();
        let layout: Arc<[(String, char)]> = crate::layout::record_layout("RawDailyBar").unwrap();
        let specs = [
            ("2025-01-01", 10.0, 10.0, 10.0, 10.0),
            ("2025-01-02", 10.0, 10.4, 9.7, 10.2),
            ("2025-01-03", 10.2, 10.5, 10.0, 10.4),
        ];
        let mut records = Vec::new();
        for (date, open, high, low, close) in specs {
            records.push(Record {
                t: 0,
                date: date.to_string(),
                fields: vec![
                    Value::Str("000001".to_string()),
                    Value::I64(0),
                    Value::Str(date.to_string()),
                    Value::F64(open),
                    Value::F64(high),
                    Value::F64(low),
                    Value::F64(close),
                    Value::F64(100.0),
                    Value::F64(1000.0),
                    Value::F64(1.0),
                ],
                layout: layout.clone(),
            });
        }
        store
            .write("RawDailyBar", "000001", &records, None)
            .unwrap();
        let rows = compute_rows(
            &store,
            "RawDailyBar",
            "000001",
            ForwardLabelConfig {
                horizon: 2,
                buy_cost: 0.0,
                sell_cost: 0.0,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert!((rows[0].1[0] - 0.04).abs() < 1e-6);
        assert!((rows[0].1[1] + 0.03).abs() < 1e-6);
        assert_eq!(rows[0].1[2], 2.0);
        assert_eq!(rows[0].1[3], 1.0);
        assert!((rows[0].1[4] - 0.05).abs() < 1e-6);
        assert_eq!(rows[0].1[5], 0.0);
        assert_eq!(rows[0].1[6], 1.0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn no_gain_uses_horizon_plus_one_instead_of_false_short_wait() {
        let root =
            std::env::temp_dir().join(format!("stockdb-labels-no-gain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("calendar.json"), "[]").unwrap();
        let store = Store::open(&root).unwrap();
        let layout: Arc<[(String, char)]> = crate::layout::record_layout("RawDailyBar").unwrap();
        let specs = [
            ("2025-01-01", 10.0, 10.0, 10.0, 10.0),
            ("2025-01-02", 10.0, 9.9, 9.5, 9.7),
            ("2025-01-03", 9.7, 9.8, 9.2, 9.4),
        ];
        let records = specs
            .into_iter()
            .map(|(date, open, high, low, close)| Record {
                t: 0,
                date: date.to_string(),
                fields: vec![
                    Value::Str("000001".to_string()),
                    Value::I64(0),
                    Value::Str(date.to_string()),
                    Value::F64(open),
                    Value::F64(high),
                    Value::F64(low),
                    Value::F64(close),
                    Value::F64(100.0),
                    Value::F64(1000.0),
                    Value::F64(1.0),
                ],
                layout: layout.clone(),
            })
            .collect::<Vec<_>>();
        store
            .write("RawDailyBar", "000001", &records, None)
            .unwrap();
        let rows = compute_rows(
            &store,
            "RawDailyBar",
            "000001",
            ForwardLabelConfig {
                horizon: 2,
                buy_cost: 0.0,
                sell_cost: 0.0,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(rows[0].1[3], 3.0);
        assert_eq!(rows[0].1[4], 0.0);
        let _ = std::fs::remove_dir_all(root);
    }
}
