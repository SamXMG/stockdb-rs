//! 并发写安全回归测试：验证 sidecar 锁文件生成、原子写 round-trip、
//! 以及 `write` 后文件完整（无损坏、长度为 rlen 整数倍）。
//!
//! 真正的跨进程竞态由咨询锁在操作系统层面串行化，这里用确定性单进程用例
//! 锁住「不应出现半截文件 / 锁文件应存在 / 写入后数据可被读回」这几条不变量。

use std::fs;

use stockdb_rs::layout::Value;
use stockdb_rs::{record_len, Store, Value as SValue};

/// 构造一条 AdjustEvent 记录（事件表，不按日历长度膨胀，n = max_t+1）。
fn mk_adj(code: &str, ex_date: &str, date: &str, bonus: f64, cash: f64) -> stockdb_rs::Record {
    stockdb_rs::Record {
        t: 0,
        date: date.to_string(),
        fields: vec![
            Value::Str(code.to_string()),
            Value::Str(ex_date.to_string()),
            Value::I64(0),
            Value::F64(bonus),
            Value::F64(cash),
            Value::F64(0.0),
        ],
        layout: std::sync::Arc::from(vec![
            ("code".to_string(), 's'),
            ("ex_date".to_string(), 's'),
            ("t".to_string(), 'q'),
            ("bonus_per_share".to_string(), 'd'),
            ("cash_per_share".to_string(), 'd'),
            ("fwd_ratio".to_string(), 'd'),
        ]),
    }
}

fn tmp_root(name: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("stockdb_rs_conc_{name}"));
    let _ = fs::remove_dir_all(&p);
    fs::create_dir_all(&p).unwrap();
    // Store::open 需要 calendar.json
    fs::write(
        p.join("calendar.json"),
        "[\"2023-01-03\",\"2023-01-04\"]",
    )
    .unwrap();
    p
}

#[test]
fn write_is_atomic_and_produces_valid_file() {
    let root = tmp_root("atomic");
    let store = Store::open(&root).unwrap();
    let code = "600000";

    let rec = mk_adj(code, "2023-01-03", "2023-01-03", 0.5, 0.1);
    let n = store.write("AdjustEvent", code, &[rec], None).unwrap();
    assert_eq!(n, 1, "事件表 n 应为 max_t+1 = 1");

    // 1) 数据可被读回，字段正确（不变量：写后可读）
    let bars = store.read("AdjustEvent", code).unwrap();
    assert_eq!(bars.len(), 1);
    assert_eq!(
        bars[0].get("AdjustEvent", "code"),
        Some(&SValue::Str(code.to_string()))
    );
    assert_eq!(
        bars[0].get("AdjustEvent", "ex_date"),
        Some(&SValue::Str("2023-01-03".to_string()))
    );
    assert_eq!(
        bars[0].get("AdjustEvent", "bonus_per_share"),
        Some(&SValue::F64(0.5))
    );

    // 2) 文件完整：长度须为 rlen 整数倍（不变量：无半截/损坏）
    let rlen = record_len("AdjustEvent").unwrap();
    let raw = fs::read(root.join("AdjustEvent").join(format!("{code}.dat"))).unwrap();
    assert_eq!(raw.len() % rlen, 0, "dat 长度须为 rlen 整数倍");

    // 3) 校验接口无报错（防静默错读）
    store.validate("AdjustEvent", code).unwrap();

    // 4) sidecar 锁文件应已生成（不变量：并发安全机制就位）
    assert!(
        root.join("AdjustEvent").join(format!("{code}.dat.lock")).exists(),
        "dat 锁文件应存在"
    );
    assert!(
        root.join("calendar.json.lock").exists(),
        "日历锁文件应存在"
    );
    // 不应残留临时文件
    assert!(
        !root.join("AdjustEvent").join(format!("{code}.dat.tmp")).exists(),
        "临时文件应被清理"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn overwrite_then_repack_stays_valid() {
    let root = tmp_root("overwrite");
    let store = Store::open(&root).unwrap();
    let code = "600000";

    // 先写一条，再覆盖写同一条（同 code、同 date => 同 t 槽位，真正覆盖）
    let r1 = mk_adj(code, "2023-01-03", "2023-01-03", 0.5, 0.1);
    store.write("AdjustEvent", code, &[r1], None).unwrap();
    let r2 = mk_adj(code, "2023-01-04", "2023-01-03", 0.8, 0.2);
    store.write("AdjustEvent", code, &[r2], None).unwrap();

    let bars = store.read("AdjustEvent", code).unwrap();
    assert_eq!(bars.len(), 1, "覆盖写后仅剩最新一条");
    assert_eq!(
        bars[0].get("AdjustEvent", "ex_date"),
        Some(&SValue::Str("2023-01-04".to_string()))
    );
    store.validate("AdjustEvent", code).unwrap();

    // repack 到更长长度仍有效
    store.repack("AdjustEvent", code, 4).unwrap();
    let raw = fs::read(root.join("AdjustEvent").join(format!("{code}.dat"))).unwrap();
    let rlen = record_len("AdjustEvent").unwrap();
    assert_eq!(raw.len(), 4 * rlen);
    store.validate("AdjustEvent", code).unwrap();

    let _ = fs::remove_dir_all(&root);
}
