//! 视图对齐测试: Rust view 模块 vs Python engine.derive_*/aggregate_period。
//!
//! 用同一份 testdata .dat 作为输入源: Rust 侧用 stockdb_rs::Store 读原始值,
//! Python 侧用 stockdb.engine 读同一文件并调 derive_*/aggregate_period,
//! 两边输出逐字段对比。

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use stockdb_rs::layout::Value;
use stockdb_rs::view::{AdjustEvent, Bar, RawBar};
use stockdb_rs::Store;

fn testdata_dir() -> PathBuf {
    if let Ok(p) = std::env::var("TESTDATA") {
        return PathBuf::from(p);
    }
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("testdata");
    p
}

/// 相对 crate 的同级 Screener 目录 (由 CARGO_MANIFEST_DIR 推导, 跨平台, 不依赖 cwd)。
const SCREENER: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../Screener");

fn approx(a: f64, b: f64) -> bool {
    if a.is_nan() && b.is_nan() {
        return true;
    }
    (a - b).abs() < 1e-9
}

/// 用 python 计算某视图, 返回 JSON 列表。
/// mode: "qfq" | "hfq" | "weekly" | "monthly"
fn python_view(mode: &str, code: &str) -> Vec<HashMap<String, serde_json::Value>> {
    let script = r#"
import sys, os, json
screener, root, mode, code = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
sys.path.insert(0, screener)
from stockdb import engine
from stockdb.calendar import TradingCalendar
cal = TradingCalendar.load(os.path.join(root, "calendar.json"))
store = engine.ColumnStore(root, cal)
bars_raw = store.read("RawDailyBar", code)
ev_raw = store.read("AdjustEvent", code)
from types import SimpleNamespace
# store.read 返回 dict; derive_* 需要带属性的对象 -> 用 SimpleNamespace 适配
bars = [SimpleNamespace(date=b["date"], open=b["open"], high=b["high"],
                        low=b["low"], close=b["close"], volume=b["volume"]) for b in bars_raw]
events = [SimpleNamespace(ex_date=e["ex_date"], bonus_per_share=e["bonus_per_share"],
                          cash_per_share=e["cash_per_share"]) for e in ev_raw]
if mode == "qfq":
    out = engine.derive_qfq(bars, events)
elif mode == "hfq":
    out = engine.derive_hfq(bars, events)
elif mode == "weekly":
    out = engine.aggregate_period(bars, "week", events)
elif mode == "monthly":
    out = engine.aggregate_period(bars, "month", events)
else:
    raise SystemExit("bad mode")
res = []
for b in out:
    res.append({"date": b.date, "open": b.open, "high": b.high,
                "low": b.low, "close": b.close, "volume": b.volume})
print(json.dumps(res, ensure_ascii=False))
"#;
    let out = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(SCREENER)
        .arg(testdata_dir().to_str().unwrap())
        .arg(mode)
        .arg(code)
        .output()
        .expect("python3");
    assert!(out.status.success(), "py view failed: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(s.trim()).expect("json")
}

fn f64_of(v: &Value) -> f64 {
    match v {
        Value::F64(f) => *f,
        Value::Null => f64::NAN,
        _ => panic!("expected f64"),
    }
}

fn code_to_bars_and_events(store: &Store, code: &str) -> (Vec<RawBar>, Vec<AdjustEvent>) {
    let raw = store.read("RawDailyBar", code).expect("raw");
    let mut bars = Vec::new();
    for r in &raw {
        bars.push(RawBar {
            date: match r.get("RawDailyBar", "date") {
                Some(Value::Str(s)) => s.clone(),
                _ => panic!("date str"),
            },
            open: f64_of(r.get("RawDailyBar", "open").unwrap()),
            high: f64_of(r.get("RawDailyBar", "high").unwrap()),
            low: f64_of(r.get("RawDailyBar", "low").unwrap()),
            close: f64_of(r.get("RawDailyBar", "close").unwrap()),
            volume: f64_of(r.get("RawDailyBar", "volume").unwrap()),
        });
    }
    let adj = store.read("AdjustEvent", code).expect("adj");
    let mut events = Vec::new();
    for r in &adj {
        events.push(AdjustEvent {
            ex_date: match r.get("AdjustEvent", "ex_date") {
                Some(Value::Str(s)) => s.clone(),
                _ => String::new(),
            },
            bonus_per_share: f64_of(r.get("AdjustEvent", "bonus_per_share").unwrap()),
            cash_per_share: f64_of(r.get("AdjustEvent", "cash_per_share").unwrap()),
        });
    }
    (bars, events)
}

fn cmp_bars(rust: &[Bar], py: &[HashMap<String, serde_json::Value>], ctx: &str) {
    assert_eq!(rust.len(), py.len(), "len mismatch {ctx}");
    for (r, p) in rust.iter().zip(py.iter()) {
        assert_eq!(r.date, p["date"].as_str().unwrap(), "date {ctx}");
        for (k, rv) in [("open", r.open), ("high", r.high), ("low", r.low), ("close", r.close), ("volume", r.volume)] {
            let pv = p[k].as_f64().unwrap();
            assert!(approx(rv, pv), "{k} {ctx} {k}: {rv} vs {pv}");
        }
    }
}

#[test]
fn view_qfq_align() {
    let store = Store::open(&testdata_dir()).unwrap();
    for code in ["600000", "000001", "300750"] {
        let (bars, events) = code_to_bars_and_events(&store, code);
        let rust = stockdb_rs::view::derive_qfq(&bars, &events);
        let py = python_view("qfq", code);
        cmp_bars(&rust, &py, &format!("qfq {code}"));
    }
}

#[test]
fn view_hfq_align() {
    let store = Store::open(&testdata_dir()).unwrap();
    for code in ["600000", "000001", "300750"] {
        let (bars, events) = code_to_bars_and_events(&store, code);
        let rust = stockdb_rs::view::derive_hfq(&bars, &events);
        let py = python_view("hfq", code);
        cmp_bars(&rust, &py, &format!("hfq {code}"));
    }
}

#[test]
fn view_weekly_align() {
    let store = Store::open(&testdata_dir()).unwrap();
    for code in ["600000", "000001", "300750"] {
        let (bars, events) = code_to_bars_and_events(&store, code);
        let rust = stockdb_rs::view::aggregate_period(&bars, "week", Some(&events));
        let py = python_view("weekly", code);
        cmp_bars(&rust, &py, &format!("weekly {code}"));
    }
}

#[test]
fn view_monthly_align() {
    let store = Store::open(&testdata_dir()).unwrap();
    for code in ["600000", "000001", "300750"] {
        let (bars, events) = code_to_bars_and_events(&store, code);
        let rust = stockdb_rs::view::aggregate_period(&bars, "month", Some(&events));
        let py = python_view("monthly", code);
        cmp_bars(&rust, &py, &format!("monthly {code}"));
    }
}
