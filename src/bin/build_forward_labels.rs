//! Rust 前瞻标签生成器。
//!
//! build_forward_labels <root> <table> <dataset> <horizon> [codes]

use std::io;
use std::path::Path;

use stockdb_rs::{labels, Store};

fn err(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn main() -> io::Result<()> {
    let mut args = std::env::args().skip(1);
    let root = args.next().ok_or_else(|| err("missing root"))?;
    let table = args.next().unwrap_or_else(|| "RawDailyBar".to_string());
    let dataset = args.next().ok_or_else(|| err("missing dataset"))?;
    let horizon: usize = args
        .next()
        .ok_or_else(|| err("missing horizon"))?
        .parse()
        .map_err(|_| err("invalid horizon"))?;
    let codes: Option<Vec<String>> = args.next().map(|value| {
        value
            .split(',')
            .filter(|x| !x.is_empty())
            .map(str::to_string)
            .collect()
    });
    let store = Store::open(&root)?;
    let result = labels::materialize(
        &store,
        &table,
        codes.as_deref(),
        &Path::new(&root).join("CompactLabel").join(dataset),
        labels::ForwardLabelConfig {
            horizon,
            ..Default::default()
        },
    )
    .map_err(err)?;
    println!("{result}");
    Ok(())
}
