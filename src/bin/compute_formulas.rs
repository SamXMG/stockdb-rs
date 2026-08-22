//! StockDB 内公式计算器：直接 mmap 读取、Rust 并行计算并写紧凑矩阵。
//!
//! 用法：
//! compute_formulas <root> <table> <formulas.json> <factor|label|signal> [code1,code2,...] [dataset]

use std::io;
use std::path::Path;

use stockdb_rs::{expr, Store};

fn err(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn main() -> io::Result<()> {
    let mut args = std::env::args().skip(1);
    let root = args.next().ok_or_else(|| err("missing root"))?;
    let table = args.next().ok_or_else(|| err("missing table"))?;
    let formulas_path = args.next().ok_or_else(|| err("missing formulas.json"))?;
    let kind = args.next().ok_or_else(|| err("missing kind"))?;
    let codes_arg = args.next();
    let dataset = args.next().unwrap_or_else(|| "dsl".to_string());
    if dataset.is_empty()
        || !dataset
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err(err(
            "dataset must contain only ASCII letters, digits, '-' or '_'",
        ));
    }
    let out_name = match kind.as_str() {
        "factor" => "CompactFactor",
        "label" => "CompactLabel",
        "signal" => "CompactSignal",
        _ => return Err(err("kind must be factor, label or signal")),
    };
    let raw = std::fs::read_to_string(&formulas_path)?;
    let specs = expr::parse_formula_specs(&raw).map_err(err)?;
    let codes: Option<Vec<String>> = codes_arg.map(|value| {
        value
            .split(',')
            .map(str::trim)
            .filter(|x| !x.is_empty())
            .map(str::to_string)
            .collect()
    });
    let store = Store::open(&root)?;
    let result = expr::compute_formulas_to_compact(
        &store,
        &table,
        &specs,
        codes.as_deref(),
        &Path::new(&root).join(out_name).join(dataset),
    )
    .map_err(err)?;
    println!("{result}");
    Ok(())
}
