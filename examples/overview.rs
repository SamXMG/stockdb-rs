//! stockdb-rs 用法概览：读 raw 日K -> 前复权 -> 周K 聚合。
//!
//! 运行: cargo run --example overview -- /path/to/stockdb_root 600000

use stockdb_rs::layout::Value;
use stockdb_rs::view::{derive_qfq, AdjustEvent, RawBar};
use stockdb_rs::Store;

fn f64_of(v: &Value) -> f64 {
    match v {
        Value::F64(f) => *f,
        Value::Null => f64::NAN,
        _ => 0.0,
    }
}

fn main() {
    let root = std::env::args().nth(1).expect("usage: overview <root> <code>");
    let code = std::env::args().nth(2).unwrap_or_else(|| "600000".into());

    let store = Store::open(&root).expect("open store");

    // 1) 读 raw 日K
    let raw = store.read("RawDailyBar", &code).expect("read raw");
    println!("{}: {} 条日K", code, raw.len());

    let bars: Vec<RawBar> = raw
        .iter()
        .map(|r| RawBar {
            date: match r.get("RawDailyBar", "date") {
                Some(Value::Str(s)) => s.clone(),
                _ => String::new(),
            },
            open: f64_of(r.get("RawDailyBar", "open").unwrap()),
            high: f64_of(r.get("RawDailyBar", "high").unwrap()),
            low: f64_of(r.get("RawDailyBar", "low").unwrap()),
            close: f64_of(r.get("RawDailyBar", "close").unwrap()),
            volume: f64_of(r.get("RawDailyBar", "volume").unwrap()),
        })
        .collect();

    // 2) 读分红送股事件
    let adj = store.read("AdjustEvent", &code).unwrap_or_default();
    let events: Vec<AdjustEvent> = adj
        .iter()
        .map(|r| AdjustEvent {
            ex_date: match r.get("AdjustEvent", "ex_date") {
                Some(Value::Str(s)) => s.clone(),
                _ => String::new(),
            },
            bonus_per_share: f64_of(r.get("AdjustEvent", "bonus_per_share").unwrap()),
            cash_per_share: f64_of(r.get("AdjustEvent", "cash_per_share").unwrap()),
        })
        .collect();

    // 3) 前复权
    let qfq = derive_qfq(&bars, &events);
    if let (Some(f), Some(l)) = (qfq.first(), qfq.last()) {
        println!("前复权: 首 {} close={:.2} -> 末 {} close={:.2}", f.date, f.close, l.date, l.close);
    }

    // 4) 周K 聚合（先 qfq 再聚合，价格连续）
    let weekly = stockdb_rs::view::aggregate_period(&bars, "week", Some(&events));
    println!("周K 共 {} 根", weekly.len());
    if let Some(w) = weekly.last() {
        println!("最新周K {}: O={:.2} H={:.2} L={:.2} C={:.2}", w.date, w.open, w.high, w.low, w.close);
    }
}
