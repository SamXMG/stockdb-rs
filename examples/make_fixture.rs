//! 造一个最小可读 fixture（RawDailyBar/demo600000），用于验证
//! "Rust 写 → cdylib → 任意语言读（示例为 C ABI）" 的端到端链路。
//!
//! 运行: cargo run --example make_fixture
use stockdb_rs::{Record, Store, Value};
use std::path::Path;

fn main() -> std::io::Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixture");
    std::fs::create_dir_all(&root)?;
    // 全新空 store：先放一个空日历，否则 Store::open 会因 calendar.json 缺失而失败。
    std::fs::write(root.join("calendar.json"), "[]")?;

    let store = Store::open(&root)?;

    // RawDailyBar schema (顺序即落盘顺序): code,t,date,open,high,low,close,volume,amount,turnover
    let layout: Vec<(String, char)> = vec![
        ("code".into(), 's'),
        ("t".into(), 'q'),
        ("date".into(), 's'),
        ("open".into(), 'I'),
        ("high".into(), 'I'),
        ("low".into(), 'I'),
        ("close".into(), 'I'),
        ("volume".into(), 'd'),
        ("amount".into(), 'd'),
        ("turnover".into(), 'I'),
    ];
    let dates = ["2024-01-02", "2024-01-03", "2024-01-04"];
    let closes = [10.2_f64, 10.8, 11.5]; // 这些值专门用于 cdylib 侧回读校验
    let mut recs = Vec::new();
    for (i, d) in dates.iter().enumerate() {
        let fields = vec![
            Value::Str("demo600000".into()),
            Value::I64(0), // t 由 write 内部按 date 经日历 ensure 重算，落盘即可
            Value::Str(d.to_string()),
            Value::F64(10.0),
            Value::F64(10.5),
            Value::F64(9.8),
            Value::F64(closes[i]),
            Value::F64(1000.0),
            Value::F64(10200.0),
            Value::F64(1.2),
        ];
        recs.push(Record {
            t: 0,
            date: d.to_string(),
            fields,
            layout: std::sync::Arc::from(layout.clone()),
        });
    }
    let n = store.write("RawDailyBar", "demo600000", &recs, None)?;
    store.write_meta("RawDailyBar", "demo600000")?;
    println!(
        "wrote {} rows -> {}/RawDailyBar/demo600000.dat",
        n,
        root.display()
    );
    Ok(())
}
