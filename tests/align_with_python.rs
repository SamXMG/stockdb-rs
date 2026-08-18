//! 与 Python `stockdb` 的字节级/字段级对齐测试 (全部 8 张列式表)。
//!
//! 基准数据: 仓库内 `testdata/` (由 Python 落盘)。
//! 测试调用 python3 用 `stockdb.engine.ColumnStore.read` 导出每表每票的记录
//! 为 JSON, 再用 Rust 读同一份 .dat 逐字段对比。
//!
//! 运行: TESTDATA=/abs/path cargo test --test align_with_python
//! (默认 TESTDATA = 仓库根/testdata)

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use stockdb_rs::layout::Value;
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

/// 调用 python3 用 stockdb 导出 (table, code) 的全部记录。
fn python_read(table: &str, code: &str) -> Vec<HashMap<String, serde_json::Value>> {
    let script = r#"
import sys, os, json
screener, root, table, code = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
sys.path.insert(0, screener)
from stockdb import engine
from stockdb.calendar import TradingCalendar
store = engine.ColumnStore(root, TradingCalendar.load(os.path.join(root, "calendar.json")))
rows = store.read(table, code)
out = []
for r in rows:
    d = {}
    for k, v in r.items():
        if isinstance(v, float):
            d[k] = None if v != v else v
        else:
            d[k] = v
    out.append(d)
print(json.dumps(out, ensure_ascii=False))
"#;
    let out = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(SCREENER)
        .arg(testdata_dir().to_str().unwrap())
        .arg(table)
        .arg(code)
        .output()
        .expect("python3 available");
    assert!(out.status.success(), "python read failed: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(s.trim()).expect("json parse")
}

fn approx(a: f64, b: f64) -> bool {
    if a.is_nan() && b.is_nan() {
        return true;
    }
    (a - b).abs() < 1e-9
}

/// Rust 记录与 Python 记录逐字段对比。
fn cmp(rust: &[stockdb_rs::Record], py: &[HashMap<String, serde_json::Value>], ctx: &str) {
    assert_eq!(rust.len(), py.len(), "len mismatch {ctx}");
    for (r, p) in rust.iter().zip(py.iter()) {
        assert_eq!(r.t, p["t"].as_i64().unwrap_or(0), "t mismatch {ctx}");
        for (i, v) in r.fields.iter().enumerate() {
            let k = &r.layout[i].0;
            let pv = &p[k];
            match v {
                &Value::Str(ref s) => assert_eq!(s, pv.as_str().unwrap_or(""), "str {k} {ctx}"),
                &Value::I64(i) => assert_eq!(i, pv.as_i64().unwrap_or(0), "i64 {k} {ctx}"),
                &Value::F64(f) => {
                    let pf = pv.as_f64().unwrap_or(f64::NAN);
                    assert!(approx(f, pf), "f64 {k} {ctx}: {f} vs {pv}");
                }
                &Value::Bool(b) => assert_eq!(b, pv.as_bool().unwrap_or(false), "bool {k} {ctx}"),
                &Value::Null => assert!(pv.is_null() || pv.is_null(), "null {k} {ctx}"),
            }
        }
    }
}

#[test]
fn align_all_tables() {
    let root = testdata_dir();
    let store = Store::open(&root).expect("open store");

    // (表, 票清单)
    let cases: &[(&str, &[&str])] = &[
        ("RawDailyBar", &["600000", "000001", "300750"]),
        ("CompanyProfile", &["600000", "000001", "300750"]),
        ("AdjustEvent", &["600000", "000001", "300750"]),
        ("FundFlow", &["600000", "000001", "300750"]),
        ("IndexDaily", &["000001", "399001"]),
        ("Announcement", &["600000", "000001", "300750"]),
        ("RenameEvent", &["600000"]),
        ("DailySnapshot", &["600000", "000001", "300750"]),
    ];

    for (table, codes) in cases {
        for code in *codes {
            if !store.exists(table, code) {
                panic!("missing testdata for {table}/{code}");
            }
            let rust = store.read(table, code).expect("rust read");
            let py = python_read(table, code);
            cmp(&rust, &py, &format!("{table}/{code}"));
        }
    }
}
