//! 隔离基准：量化「逐行物化 Record」 vs 「零拷贝游标」的差值。
//!
//! 目的：回答"unsafe 核 + safe 外壳（Cursor）能不能优化咱的底层读路径"。
//! 做法：把单票 .dat 读进内存，对**同一份字节**跑两条路径：
//!   A) layout::decode_row  → 每行物化 Vec<Value> + Arc + String
//!   B) RowCursor          → 纯 &[u8] + 预计算列偏移，按列按需取 f64（100% safe）
//! 差值即"物化成本"。若 B 已接近 memcpy 上限，则 unsafe 无剩余空间可榨。
//!
//! 用法：cargo run --release --example bench_cursor -- <stockdb_root> [code]

use std::time::Instant;

use stockdb_rs::layout::{self, FieldKind, Schema};
use stockdb_rs::{ColumnData, Store};

/// 零拷贝行游标：**全程 safe Rust**，无一行 unsafe。
/// mmap/&[u8] 的借用本身就是"受控的裸指针"，定长布局下按列偏移取字节即可。
struct RowCursor<'a> {
    row: &'a [u8],
    schema: &'a Schema,
}

impl<'a> RowCursor<'a> {
    #[inline]
    fn new(row: &'a [u8], schema: &'a Schema) -> Option<Self> {
        if row.first()? == &0 {
            return None; // present=0 空槽
        }
        Some(Self { row, schema })
    }

    /// 按字段下标取 f64（F64 直读 / Scaled 反缩放）。越界或类型不符返回 None。
    #[inline]
    fn f64_at(&self, i: usize) -> Option<f64> {
        let (off, kind) = *self.schema.offsets.get(i)?;
        match kind {
            FieldKind::F64 => {
                let b: [u8; 8] = self.row.get(off..off + 8)?.try_into().ok()?;
                Some(f64::from_le_bytes(b))
            }
            FieldKind::Scaled(scale) => {
                let b: [u8; 4] = self.row.get(off..off + 4)?.try_into().ok()?;
                let raw = i32::from_le_bytes(b);
                if raw == i32::MIN {
                    None
                } else {
                    Some(raw as f64 / scale)
                }
            }
            _ => None,
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let root = args
        .next()
        .unwrap_or_else(|| r"D:\Code\Git\LiangHua\Screener\stockdb\root".to_string());
    let code = args.next().unwrap_or_else(|| "000001".to_string());
    let table = "RawDailyBar";

    let path = format!(r"{root}\{table}\{code}.dat");
    let data = std::fs::read(&path).expect("read .dat");
    let rlen = layout::record_len(table).expect("rlen");
    let n = data.len() / rlen;
    println!("file={path}\nrlen={rlen} rows={n} bytes={}\n", data.len());

    let schema = layout::schema_ref(table).expect("schema");
    // 取一个 Scaled/F64 字段做代表列（close 通常存在）
    let col = schema
        .index
        .get("close")
        .copied()
        .unwrap_or(0);
    println!("bench column: idx={col} name={:?}", schema.kinds[col].0);

    let iters = 200;
    let mut sink = 0.0f64;

    // ---- A) 物化路径：decode_row 每行建 Vec<Value> + Arc + String ----
    let t0 = Instant::now();
    for _ in 0..iters {
        for t in 0..n {
            let row = &data[t * rlen..(t + 1) * rlen];
            if let Some(rec) = layout::decode_row(table, row) {
                if let Some(layout::Value::F64(v)) = rec.fields.get(col) {
                    sink += v;
                }
            }
        }
    }
    let ta = t0.elapsed();

    // ---- B) 游标路径：零分配，按列偏移直读 ----
    let t1 = Instant::now();
    for _ in 0..iters {
        for t in 0..n {
            let row = &data[t * rlen..(t + 1) * rlen];
            if let Some(c) = RowCursor::new(row, &schema) {
                if let Some(v) = c.f64_at(col) {
                    sink += v;
                }
            }
        }
    }
    let tb = t1.elapsed();

    // ---- C) 下限参照：纯字节求和（memcpy 级，不含任何解码语义）----
    let t2 = Instant::now();
    for _ in 0..iters {
        let mut s = 0u64;
        for chunk in data.chunks_exact(8) {
            s += u64::from_le_bytes(chunk.try_into().unwrap());
        }
        sink += s as f64;
    }
    let tc = t2.elapsed();

    // ---- D) 单测 record_len(table)：decode_row 每行都调，且**无缓存** ----
    let t3 = Instant::now();
    for _ in 0..iters {
        for _ in 0..n {
            sink += layout::record_len(table).unwrap_or(0) as f64;
        }
    }
    let td = t3.elapsed();

    // ---- E) 单测 schema_ref(table)：decode_row 每行都调，走全局 Mutex + HashMap ----
    let t4 = Instant::now();
    for _ in 0..iters {
        for _ in 0..n {
            sink += layout::schema_ref(table).map(|s| s.kinds.len()).unwrap_or(0) as f64;
        }
    }
    let te = t4.elapsed();

    // ---- F) Store::read_columns 一次取回测需要的 6 列（替代 Python decode_raw_bar）----
    // 口径对齐 Python 端 decode_raw_bar：date/open/high/low/close/volume 全解。
    let store = Store::open(&root).expect("open store");
    let fields = ["date", "open", "high", "low", "close", "volume"];
    let _ = store
        .read_columns(table, &code, &fields, 0, n)
        .expect("warmup read_columns"); // 预热：含首次 mmap
    let t5 = Instant::now();
    for _ in 0..iters {
        let cols = store
            .read_columns(table, &code, &fields, 0, n)
            .expect("read_columns");
        if let ColumnData::F64(v) = &cols[4].data {
            sink += v.iter().filter_map(|x| *x).sum::<f64>();
        }
    }
    let tf = t5.elapsed();

    let rows = (n * iters) as f64;
    println!("\n{:<22} {:>12} {:>14} {:>12}", "path", "total", "ns/row", "Mrow/s");
    let line = |name: &str, d: std::time::Duration| {
        let ns = d.as_nanos() as f64 / rows;
        println!(
            "{:<22} {:>12?} {:>14.1} {:>12.2}",
            name,
            d,
            ns,
            rows / d.as_secs_f64() / 1e6
        );
    };
    line("A) decode_row 物化", ta);
    line("B) RowCursor 零拷贝", tb);
    line("C) 纯字节求和(下限)", tc);
    line("D) 仅 record_len()", td);
    line("E) 仅 schema_ref()", te);
    line("F) read_columns 6列", tf);
    println!(
        "\nA 的构成估算: record_len 占 {:.1}%, schema_ref 占 {:.1}%, 其余(分配/解码)占 {:.1}%",
        td.as_secs_f64() / ta.as_secs_f64() * 100.0,
        te.as_secs_f64() / ta.as_secs_f64() * 100.0,
        (1.0 - (td.as_secs_f64() + te.as_secs_f64()) / ta.as_secs_f64()) * 100.0,
    );
    println!(
        "\n物化开销倍数 A/B = {:.2}x   游标相对下限 B/C = {:.2}x",
        ta.as_secs_f64() / tb.as_secs_f64(),
        tb.as_secs_f64() / tc.as_secs_f64().max(1e-9),
    );
    println!(
        "read_columns(6列)/游标(1列) = {:.2}x   ← 6 列全解 vs 单列\n\
         Python decode_raw_bar 实测 990.0 ns/row：read_columns 相对它 = {:.1}x\n\
         (sink={sink:.3} 防优化)",
        tf.as_secs_f64() / tb.as_secs_f64(),
        990.0 / (tf.as_nanos() as f64 / rows),
    );
}
