//! MinuteBar 分时块 (JSON) 的跨语言对齐测试。
//!
//! 双向: Python 写 -> Rust 读; Rust 写 -> Python 读回。
//! 基准数据由 tests/gen_testdata.py 生成 (写 root/minute/{code}/{date}.min)。

use std::path::PathBuf;
use std::process::Command;

use stockdb_rs::minute::{MinuteBar, MinuteStore};

fn testdata_dir() -> PathBuf {
    if let Ok(p) = std::env::var("TESTDATA") {
        return PathBuf::from(p);
    }
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("testdata");
    p
}

const SCREENER: &str = "/home/honor/Git/LIANGHUA/Screener";

/// 用 Python MinuteStore 读一块, 反序列化为 MinuteBar。
fn py_read(code: &str, date: &str) -> MinuteBar {
    let script = r#"
import sys, os, json
screener, root, code, date = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
sys.path.insert(0, screener)
from stockdb import engine
from stockdb.schema import MinuteBar
mstore = engine.MinuteStore(root)
bar = mstore.read(code, date)
print(json.dumps(bar.__dict__ if hasattr(bar, "__dict__") else {}))
"#;
    let out = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(SCREENER)
        .arg(testdata_dir().to_str().unwrap())
        .arg(code)
        .arg(date)
        .output()
        .expect("python3");
    assert!(out.status.success(), "py read: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str::<serde_json::Value>(s.trim())
        .ok()
        .filter(|v| !v.is_null())
        .map(|v| MinuteBar {
            code: v["code"].as_str().unwrap().to_string(),
            date: v["date"].as_str().unwrap().to_string(),
            minutes: v["minutes"].as_array().map(|a| a.iter().map(|x| x.as_f64().unwrap()).collect()).unwrap_or_default(),
            opens: v["opens"].as_array().map(|a| a.iter().map(|x| x.as_f64().unwrap()).collect()).unwrap_or_default(),
            highs: v["highs"].as_array().map(|a| a.iter().map(|x| x.as_f64().unwrap()).collect()).unwrap_or_default(),
            lows: v["lows"].as_array().map(|a| a.iter().map(|x| x.as_f64().unwrap()).collect()).unwrap_or_default(),
            closes: v["closes"].as_array().map(|a| a.iter().map(|x| x.as_f64().unwrap()).collect()).unwrap_or_default(),
            volumes: v["volumes"].as_array().map(|a| a.iter().map(|x| x.as_f64().unwrap()).collect()).unwrap_or_default(),
            amounts: v["amounts"].as_array().map(|a| a.iter().map(|x| x.as_f64().unwrap()).collect()).unwrap_or_default(),
        })
        .expect("parse python MinuteBar")
}

/// 用 Python MinuteStore 写一块 (供 Rust 读回对比)。
fn py_write(bar: &MinuteBar) {
    let script = r#"
import sys, os
screener, root, js = sys.argv[1], sys.argv[2], sys.argv[3]
sys.path.insert(0, screener)
from stockdb import engine
from stockdb.schema import MinuteBar
import json
d = json.loads(js)
bar = MinuteBar(**d)
engine.MinuteStore(root).write(bar)
"#;
    let js = serde_json::to_string(bar).unwrap();
    let out = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(SCREENER)
        .arg(testdata_dir().to_str().unwrap())
        .arg(&js)
        .output()
        .expect("python3");
    assert!(out.status.success(), "py write: {}", String::from_utf8_lossy(&out.stderr));
}

fn sample(code: &str, date: &str, n: usize) -> MinuteBar {
    let minutes: Vec<f64> = (0..n).map(|i| i as f64).collect();
    MinuteBar {
        code: code.to_string(),
        date: date.to_string(),
        minutes: minutes.clone(),
        opens: minutes.iter().map(|m| 10.0 + m * 0.1).collect(),
        highs: minutes.iter().map(|m| 10.5 + m * 0.1).collect(),
        lows: minutes.iter().map(|m| 9.8 + m * 0.1).collect(),
        closes: minutes.iter().map(|m| 10.2 + m * 0.1).collect(),
        volumes: minutes.iter().map(|m| 1000.0 + m).collect(),
        amounts: minutes.iter().map(|m| 1e6 + m * 100.0).collect(),
    }
}

fn eq(a: &MinuteBar, b: &MinuteBar) {
    assert_eq!(a.code, b.code);
    assert_eq!(a.date, b.date);
    assert_eq!(a.minutes.len(), b.minutes.len(), "minutes len");
    for (x, y) in a.minutes.iter().zip(b.minutes.iter()) {
        assert!((x - y).abs() < 1e-9, "minutes {x} {y}");
    }
    for (x, y) in a.opens.iter().zip(b.opens.iter()) {
        assert!((x - y).abs() < 1e-9, "opens {x} {y}");
    }
    for (x, y) in a.highs.iter().zip(b.highs.iter()) {
        assert!((x - y).abs() < 1e-9, "highs {x} {y}");
    }
    for (x, y) in a.lows.iter().zip(b.lows.iter()) {
        assert!((x - y).abs() < 1e-9, "lows {x} {y}");
    }
    for (x, y) in a.closes.iter().zip(b.closes.iter()) {
        assert!((x - y).abs() < 1e-9, "closes {x} {y}");
    }
    for (x, y) in a.volumes.iter().zip(b.volumes.iter()) {
        assert!((x - y).abs() < 1e-9, "volumes {x} {y}");
    }
    for (x, y) in a.amounts.iter().zip(b.amounts.iter()) {
        assert!((x - y).abs() < 1e-9, "amounts {x} {y}");
    }
}

#[test]
fn rust_read_python_written() {
    let root = testdata_dir();
    let store = MinuteStore::new(&root);
    let codes = ["600000", "000001", "300750"];
    for code in codes {
        let rust = store.read(code, "2023-07-17").unwrap().expect("rust read");
        let py = py_read(code, "2023-07-17");
        eq(&rust, &py);
    }
}

#[test]
fn python_reads_rust_written() {
    let root = testdata_dir();
    let store = MinuteStore::new(&root);
    let bar = sample("688981", "2021-07-16", 60);
    store.write(&bar).unwrap();
    let py = py_read("688981", "2021-07-16");
    eq(&bar, &py);
    // 清理
    let _ = std::fs::remove_file(root.join("minute").join("688981").join("2021-07-16.min"));
}
