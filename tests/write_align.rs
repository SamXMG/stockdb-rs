//! 写入 / repack / .meta 对齐测试。
//!
//! 用 testdata/ 作为基准 (Python 落盘的 .dat): Rust 读出 -> Rust write 到
//! 临时目录 -> Python engine 读回, 逐字段对比; 同时验证 repack 与 .meta 一致。
//!
//! 运行: TESTDATA=/abs/path cargo test --test write_align

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

fn approx(a: f64, b: f64) -> bool {
    if a.is_nan() && b.is_nan() {
        return true;
    }
    (a - b).abs() < 1e-9
}

/// Python engine 读出 (table, code) 全部记录 -> Vec<HashMap<String, Json>>。
fn python_read(table: &str, code: &str, root: &PathBuf) -> Vec<HashMap<String, serde_json::Value>> {
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
        .arg("/home/honor/Git/LIANGHUA/Screener")
        .arg(root.to_str().unwrap())
        .arg(table)
        .arg(code)
        .output()
        .expect("python3");
    assert!(out.status.success(), "py read failed: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(s.trim()).expect("json")
}

fn cmp_records(rust: &[stockdb_rs::Record], py: &[HashMap<String, serde_json::Value>], ctx: &str) {
    assert_eq!(rust.len(), py.len(), "len mismatch {ctx}");
    for (r, p) in rust.iter().zip(py.iter()) {
        assert_eq!(r.t, p["t"].as_i64().unwrap(), "t mismatch {ctx}");
        for (k, v) in &r.fields {
            let pv = &p[k];
            match v {
                Value::Str(s) => assert_eq!(s, pv.as_str().unwrap_or(""), "str {k} {ctx}"),
                Value::I64(i) => assert_eq!(*i, pv.as_i64().unwrap_or(0), "i64 {k} {ctx}"),
                Value::F64(f) => {
                    let pf = pv.as_f64().unwrap_or(f64::NAN);
                    assert!(approx(*f, pf), "f64 {k} {ctx}: {f} vs {pv}");
                }
                Value::Bool(b) => assert_eq!(*b, pv.as_bool().unwrap_or(false), "bool {k} {ctx}"),
                Value::Null => assert!(pv.is_null(), "null {k} {ctx}"),
            }
        }
    }
}

#[test]
fn write_then_read_by_python() {
    let src = testdata_dir();
    let tmp = std::env::temp_dir().join("stockdb_rs_write_test");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    // 复制 calendar.json 到临时根
    std::fs::copy(src.join("calendar.json"), tmp.join("calendar.json")).unwrap();

    let src_store = Store::open(&src).unwrap();
    let out_store = Store::open(&tmp).unwrap();

    for table in ["RawDailyBar", "CompanyProfile", "AdjustEvent"] {
        for code in ["600000", "000001", "300750"] {
            if !src_store.exists(table, code) {
                continue;
            }
            let recs = src_store.read(table, code).unwrap();
            // 写 (target_n 取原文件行数, 保持等长)
            let rlen = stockdb_rs::record_len(table).unwrap();
            let n = std::fs::read(src.join(table).join(format!("{code}.dat"))).unwrap().len() / rlen;
            out_store.write(table, code, &recs, Some(n)).unwrap();
            out_store.write_meta(table, code).unwrap();

            // Python 读回
            let py = python_read(table, code, &tmp);
            cmp_records(&recs, &py, &format!("write {table}/{code}"));
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn repack_equal_length_no_change() {
    let src = testdata_dir();
    let tmp = std::env::temp_dir().join("stockdb_rs_repack_test");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::copy(src.join("calendar.json"), tmp.join("calendar.json")).unwrap();

    let src_store = Store::open(&src).unwrap();
    let out_store = Store::open(&tmp).unwrap();

    let table = "RawDailyBar";
    let code = "600000";
    let recs = src_store.read(table, code).unwrap();
    let rlen = stockdb_rs::record_len(table).unwrap();
    let n = std::fs::read(src.join(table).join(format!("{code}.dat"))).unwrap().len() / rlen;
    // repack 到相同长度, 数据应不变
    out_store.write(table, code, &recs, Some(n)).unwrap();
    out_store.repack(table, code, n).unwrap();
    let after = out_store.read(table, code).unwrap();
    assert_eq!(after.len(), recs.len(), "repack same len changes count");
    cmp_records(&after, &python_read(table, code, &tmp), "repack equal");
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn meta_matches_python() {
    let src = testdata_dir();
    let tmp = std::env::temp_dir().join("stockdb_rs_meta_test");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::copy(src.join("calendar.json"), tmp.join("calendar.json")).unwrap();

    let store = Store::open(&tmp).unwrap();
    let table = "RawDailyBar";
    let code = "600000";
    // 用 src 的 recs 写到 tmp (需先有 src store)
    let src_store = Store::open(&src).unwrap();
    let recs = src_store.read(table, code).unwrap();
    let rlen = stockdb_rs::record_len(table).unwrap();
    let n = std::fs::read(src.join(table).join(format!("{code}.dat"))).unwrap().len() / rlen;
    store.write(table, code, &recs, Some(n)).unwrap();
    store.write_meta(table, code).unwrap();

    // 用 Python 对比 meta
    let script = r#"
import sys, os, json, hashlib
root, table, code = sys.argv[1], sys.argv[2], sys.argv[3]
sys.path.insert(0, "/home/honor/Git/LIANGHUA/Screener")
from stockdb.calendar import TradingCalendar
cal = TradingCalendar.load(os.path.join(root, "calendar.json"))
expected = {"cal_len": len(cal._dates), "cal_hash": cal.hash(), "table": table}
print(json.dumps(expected))
"#;
    let out = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(tmp.to_str().unwrap())
        .arg(table)
        .arg(code)
        .output()
        .expect("python3");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let expected: HashMap<String, serde_json::Value> =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();

    let meta_txt = std::fs::read_to_string(tmp.join(table).join(format!("{code}.meta"))).unwrap();
    let meta: HashMap<String, serde_json::Value> = serde_json::from_str(&meta_txt).unwrap();
    assert_eq!(meta["cal_len"], expected["cal_len"], "cal_len");
    assert_eq!(meta["cal_hash"], expected["cal_hash"], "cal_hash");
    assert_eq!(meta["table"], expected["table"], "table");
    let _ = std::fs::remove_dir_all(&tmp);
}
