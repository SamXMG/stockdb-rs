//! 将 screener_cn/data/d5d6/*.json 迁移为稀疏 MoneyFlowHistory/*.flow。
//!
//! 用法：
//! `cargo run --release --bin migrate_d5d6 -- <stockdb-root> <d5d6-cache>`
//!
//! 原始 JSON 永远不会被删除；迁移结果可由 Python `flow_history.load_history`
//! 自动优先读取，格式错误时回退 JSON。

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;

use serde_json::Value as J;
use stockdb_rs::calendar::TradingCalendar;
use stockdb_rs::flow::{self, FlowRow};

fn num(obj: &serde_json::Map<String, J>, key: &str) -> f64 {
    obj.get(key).and_then(|v| v.as_f64()).unwrap_or(f64::NAN)
}

fn row_from_json(t: i64, obj: &serde_json::Map<String, J>) -> FlowRow {
    let source = obj.get("source").and_then(|v| v.as_str()).unwrap_or("legacy_unknown");
    FlowRow {
        t,
        main_net: num(obj, "main_net"),
        main_pct: num(obj, "main_pct"),
        xl_net: num(obj, "xl_net"),
        xl_pct: num(obj, "xl_pct"),
        r0_net: num(obj, "r0_net"),
        r0_pct: num(obj, "r0_pct"),
        turnover: num(obj, "turnover"),
        vol_ratio: num(obj, "vol_ratio"),
        source: flow::source_id(source),
    }
}

fn main() -> io::Result<()> {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(args.next().unwrap_or_else(|| "stockdb/root".to_string()));
    let cache = PathBuf::from(args.next().unwrap_or_else(|| "data/d5d6".to_string()));
    let cal_path = root.join("calendar.json");
    let cal = TradingCalendar::load(&cal_path).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("load calendar {}: {e}", cal_path.display())))?;
    let out_dir = root.join("MoneyFlowHistory");
    std::fs::create_dir_all(&out_dir)?;

    let mut manifest: BTreeMap<String, J> = BTreeMap::new();
    let mut files = 0usize;
    let mut rows = 0usize;
    let mut skipped_dates = 0usize;
    for entry in std::fs::read_dir(&cache)? {
        let path = entry?.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") || path.file_name().and_then(|x| x.to_str()) == Some("manifest.json") {
            continue;
        }
        let code = match path.file_stem().and_then(|x| x.to_str()) {
            Some(x) => x.to_string(),
            None => continue,
        };
        let txt = std::fs::read_to_string(&path)?;
        let raw: J = serde_json::from_str(&txt).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", path.display())))?;
        let obj = match raw.as_object() { Some(x) => x, None => continue };
        let mut out_rows = Vec::with_capacity(obj.len());
        for (date, row) in obj {
            let t = match cal.date_to_t(date) {
                Some(t) => t as i64,
                None => { skipped_dates += 1; continue; }
            };
            if let Some(row_obj) = row.as_object() {
                out_rows.push(row_from_json(t, row_obj));
            }
        }
        out_rows.sort_by_key(|r| r.t);
        let n = out_rows.len();
        if n > 0 {
            flow::write_file(&out_dir.join(format!("{code}.flow")), &out_rows)?;
            rows += n;
            manifest.insert(code, serde_json::json!({"rows": n, "first_t": out_rows.first().unwrap().t, "last_t": out_rows.last().unwrap().t}));
            files += 1;
        }
        if files % 250 == 0 && files > 0 {
            eprintln!("migrated files={} rows={} skipped_dates={}", files, rows, skipped_dates);
        }
    }
    let info = serde_json::json!({
        "format": "LHFLW001",
        "version": flow::VERSION,
        "header_len": flow::HEADER_LEN,
        "record_len": flow::RECORD_LEN,
        "pct_scale": flow::PCT_SCALE,
        "extra_scale": flow::EXTRA_SCALE,
        "files": files,
        "rows": rows,
        "skipped_dates": skipped_dates,
        "source_codes": {"0":"legacy_unknown", "1":"sina_moneyflow", "2":"eastmoney_fflow", "3":"eastmoney", "4":"fuyao"},
        "stocks": manifest,
    });
    std::fs::write(out_dir.join("manifest.json"), serde_json::to_vec_pretty(&info).map_err(io::Error::other)?)?;
    println!("D5/D6 migration complete: files={} rows={} skipped_dates={} out={}", files, rows, skipped_dates, out_dir.display());
    Ok(())
}
