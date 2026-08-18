//! 落库桥: 接收 Python `stockdb` 采集+适配产出的模型 JSON, 用 Rust `stockdb_rs`
//! 引擎写入列式 `.dat` (与 Python `engine.ingest_stock` 字节兼容).
//!
//! 用法 (由 Python `stockdb/ingest_rust.py` 调用):
//!   python3 -m stockdb.collect_one 600000 | stockdb_rs ingest_bridge <root>
//!
//! stdin 逐行 JSON, 每行描述一只票的全部表:
//! {
//!   "code": "600000",
//!   "tables": {
//!     "RawDailyBar": [ {"date":"2023-01-03","open":..., ...}, ... ],
//!     "FundFlow":    [ ... ],
//!     ...
//!   },
//!   "minute": [ {"date":"2023-01-03","minutes":[...], ...}, ... ]   // 可选
//! }
//!
//! 每只票的每个表一次性 write (覆盖写, 与 Python 侧一致).

use std::io::{self, BufRead, Write};
use std::path::Path;

use serde::Deserialize;
use serde_json::Value as J;

use stockdb_rs::{layout, minute::MinuteStore, Record, Store, Value};

#[derive(Debug, Deserialize)]
struct RowModel {
    #[serde(flatten)]
    fields: std::collections::HashMap<String, J>,
}

#[derive(Debug, Deserialize)]
struct StockModel {
    code: String,
    #[serde(default)]
    tables: std::collections::HashMap<String, Vec<RowModel>>,
    #[serde(default)]
    minute: Vec<J>,
}

fn json_to_value(kind: layout::FieldKind, j: &J) -> Value {
    match (kind, j) {
        (layout::FieldKind::Bool, J::Bool(b)) => Value::Bool(*b),
        (layout::FieldKind::Bool, _) => Value::Bool(false),
        (layout::FieldKind::Str(_), J::String(s)) => Value::Str(s.clone()),
        (layout::FieldKind::Str(_), J::Null) => Value::Str(String::new()),
        (layout::FieldKind::Str(_), other) => Value::Str(other.to_string()),
        // 整数: 接受 i64 / f64(取整) / 字符串解析
        (layout::FieldKind::T, J::Number(n)) => {
            if let Some(i) = n.as_i64() {
                Value::I64(i)
            } else if let Some(f) = n.as_f64() {
                Value::I64(f as i64)
            } else {
                Value::Null
            }
        }
        (layout::FieldKind::T, J::String(s)) => {
            s.parse::<i64>().map(Value::I64).unwrap_or(Value::Null)
        }
        (layout::FieldKind::T, J::Null) => Value::Null,
        (layout::FieldKind::F64, J::Number(n)) => {
            if let Some(f) = n.as_f64() {
                Value::F64(f)
            } else {
                Value::Null
            }
        }
        (layout::FieldKind::F64, J::Null) => Value::Null,
        (layout::FieldKind::F64, _) => Value::Null,
        (layout::FieldKind::Present, _) => Value::Null,
        (_, J::Null) => Value::Null,
        (_, _) => Value::Null,
    }
}

fn build_record(table: &str, row: &RowModel) -> Option<Record> {
    let kinds = layout::field_kinds(table)?;
    let date = row
        .fields
        .get("date")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_default();
    if date.is_empty() {
        return None; // 无日期的行无效
    }
    let mut fields: Vec<Value> = Vec::with_capacity(kinds.len());
    let mut layout: Vec<(String, char)> = Vec::with_capacity(kinds.len());
    for (name, kind) in &kinds {
        let j = row.fields.get(name).cloned().unwrap_or(J::Null);
        fields.push(json_to_value(*kind, &j));
        layout.push((name.clone(), format_char(kind)));
    }
    Some(Record {
        t: 0, // 由 Store::write 内部 ensure(date) 定稿
        date,
        fields,
        layout,
    })
}

fn format_char(kind: &layout::FieldKind) -> char {
    match kind {
        layout::FieldKind::Bool => '?',
        layout::FieldKind::Str(_) => 's',
        layout::FieldKind::T => 'q',
        layout::FieldKind::F64 => 'd',
        layout::FieldKind::Present => 'x',
    }
}

fn ingest_one(store: &Store, min_store: &MinuteStore, model: &StockModel) -> io::Result<usize> {
    let mut written = 0usize;
    for (table, rows) in &model.tables {
        if layout::field_kinds(table).is_none() {
            eprintln!("[warn] 未知表 {table}, 跳过");
            continue;
        }
        let recs: Vec<Record> = rows
            .iter()
            .filter_map(|r| build_record(table, r))
            .collect();
        if recs.is_empty() {
            continue;
        }
        let n = store.write(table, &model.code, &recs, None)?;
        written += n;
    }
    // 分时块 (独立 JSON-block 存储)
    for m in &model.minute {
        let bar: stockdb_rs::minute::MinuteBar = match serde_json::from_value(m.clone()) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[warn] 分时解析失败 {e}");
                continue;
            }
        };
        min_store.write(&bar)?;
        written += 1;
    }
    Ok(written)
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("用法: stockdb_rs ingest_bridge <root>  (从 stdin 读 JSON 行)");
        std::process::exit(2);
    }
    let root = &args[1];
    let store = Store::open(Path::new(root))?;
    let min_store = MinuteStore::new(root);

    let stdin = io::stdin();
    let mut out = io::stdout().lock();
    let mut count = 0usize;
    for line in stdin.lock().lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let model: StockModel = match serde_json::from_str(line) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("[error] JSON 解析失败: {e}");
                continue;
            }
        };
        match ingest_one(&store, &min_store, &model) {
            Ok(n) => {
                count += 1;
                writeln!(out, "OK {} rows={}", model.code, n)?;
                out.flush()?;
            }
            Err(e) => {
                eprintln!("[error] {} 写入失败: {e}", model.code);
            }
        }
    }
    eprintln!("桥落库完成: {count} 只");
    Ok(())
}
