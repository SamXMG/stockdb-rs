//! 与 Python `stockdb` 的字节级/字段级对齐测试。
//!
//! 基准数据: 仓库内 `testdata/` (由 Python build_db 落盘 3 只票 + repack)。
//! 测试调用 python3 用 `stockdb.engine.ColumnStore.read` 导出每表每票的
//! 记录为 JSON, 再用 Rust 读同一份 .dat 逐字段对比。
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
    // 仓库根/testdata (crate 在 stockdb-rs/, 测试在 stockdb-rs/tests)
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("testdata");
    p
}

/// 调用 python3 用 stockdb 导出 (table, code) 的全部记录为 Vec<HashMap<String, serde_json::Value>>。
fn python_read(table: &str, code: &str) -> Vec<HashMap<String, serde_json::Value>> {
    // 脚本固定, 动态参数走 sys.argv, 避免 Rust format! 与 Python 花括号冲突。
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
            d[k] = None if v != v else v  # nan -> None
        else:
            d[k] = v
    out.append(d)
print(json.dumps(out, ensure_ascii=False))
"#;
    let out = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg("/home/honor/Git/LIANGHUA/Screener")
        .arg(testdata_dir().to_str().unwrap())
        .arg(table)
        .arg(code)
        .output()
        .expect("python3 available");
    assert!(
        out.status.success(),
        "python read failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(s.trim()).expect("json parse")
}

fn approx(a: f64, b: f64) -> bool {
    if a.is_nan() && b.is_nan() {
        return true;
    }
    (a - b).abs() < 1e-9
}

#[test]
fn align_raw_daily_bar() {
    let root = testdata_dir();
    let store = Store::open(&root).expect("open store");
    for code in ["600000", "000001", "300750"] {
        let rust = store.read("RawDailyBar", code).expect("rust read");
        let py = python_read("RawDailyBar", code);
        assert_eq!(rust.len(), py.len(), "len mismatch {code}");
        for (r, p) in rust.iter().zip(py.iter()) {
            assert_eq!(r.t, p["t"].as_i64().unwrap(), "t mismatch {code}");
            for (k, v) in &r.fields {
                let pv = &p[k];
                match v {
                    Value::Str(s) => {
                        assert_eq!(s, pv.as_str().unwrap(), "str {k} {code}");
                    }
                    Value::I64(i) => {
                        assert_eq!(*i, pv.as_i64().unwrap(), "i64 {k} {code}");
                    }
                    Value::F64(f) => {
                        assert!(
                            approx(*f, pv.as_f64().unwrap()),
                            "f64 {k} {code}: {f} vs {}",
                            pv
                        );
                    }
                    Value::Bool(b) => {
                        assert_eq!(*b, pv.as_bool().unwrap(), "bool {k} {code}");
                    }
                    Value::Null => {
                        assert!(pv.is_null(), "null {k} {code}");
                    }
                }
            }
        }
    }
}

#[test]
fn align_company_profile() {
    let root = testdata_dir();
    let store = Store::open(&root).expect("open store");
    for code in ["600000", "000001", "300750"] {
        let rust = store.read("CompanyProfile", code).expect("rust read");
        let py = python_read("CompanyProfile", code);
        assert_eq!(rust.len(), py.len(), "len mismatch {code}");
        // profile 截面表通常只有 1 条
        for (r, p) in rust.iter().zip(py.iter()) {
            for (k, v) in &r.fields {
                let pv = &p[k];
                match v {
                    Value::Str(s) => {
                        assert_eq!(s, pv.as_str().unwrap_or(""), "str {k} {code}");
                    }
                    Value::I64(i) => {
                        assert_eq!(*i, pv.as_i64().unwrap_or(0), "i64 {k} {code}");
                    }
                    Value::F64(f) => {
                        let pf = pv.as_f64().unwrap_or(f64::NAN);
                        assert!(approx(*f, pf), "f64 {k} {code}: {f} vs {pv}");
                    }
                    Value::Bool(b) => {
                        assert_eq!(*b, pv.as_bool().unwrap_or(false), "bool {k} {code}");
                    }
                    Value::Null => {
                        // python 可能给 None 或缺失
                        assert!(pv.is_null() || pv.is_null(), "null {k} {code}");
                    }
                }
            }
        }
    }
}

#[test]
fn align_adjust_event() {
    let root = testdata_dir();
    let store = Store::open(&root).expect("open store");
    for code in ["600000", "000001", "300750"] {
        let rust = store.read("AdjustEvent", code).expect("rust read");
        let py = python_read("AdjustEvent", code);
        assert_eq!(rust.len(), py.len(), "len mismatch {code}");
        for (r, p) in rust.iter().zip(py.iter()) {
            for (k, v) in &r.fields {
                let pv = &p[k];
                match v {
                    Value::Str(s) => {
                        assert_eq!(s, pv.as_str().unwrap_or(""), "str {k} {code}");
                    }
                    Value::I64(i) => {
                        assert_eq!(*i, pv.as_i64().unwrap_or(0), "i64 {k} {code}");
                    }
                    Value::F64(f) => {
                        let pf = pv.as_f64().unwrap_or(f64::NAN);
                        assert!(approx(*f, pf), "f64 {k} {code}: {f} vs {pv}");
                    }
                    Value::Bool(b) => {
                        assert_eq!(*b, pv.as_bool().unwrap_or(false), "bool {k} {code}");
                    }
                    Value::Null => {
                        assert!(pv.is_null(), "null {k} {code}");
                    }
                }
            }
        }
    }
}
