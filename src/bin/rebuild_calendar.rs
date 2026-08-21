//! 从已有 RawDailyBar 文件重建全局 calendar.json，并按日期重排全部日线。
//!
//! 用法：`cargo run --release --bin rebuild_calendar -- <stockdb-root>`

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;

use stockdb_rs::lock::atomic_write;

const RAW_RECORD_LEN: usize = 71;
const T_OFFSET: usize = 17;
const DATE_OFFSET: usize = 25;
const DATE_LEN: usize = 10;

fn row_date(row: &[u8]) -> &str {
    let raw = &row[DATE_OFFSET..DATE_OFFSET + DATE_LEN];
    let end = raw.iter().position(|&b| b == 0).unwrap_or(DATE_LEN);
    std::str::from_utf8(&raw[..end]).unwrap_or("").trim()
}

fn main() -> io::Result<()> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("stockdb/root"));
    let dir = root.join("RawDailyBar");
    if !dir.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("RawDailyBar not found: {}", dir.display()),
        ));
    }

    let paths: Vec<PathBuf> = std::fs::read_dir(&dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("dat"))
        .collect();
    let mut dates = Vec::<String>::new();
    let mut physical_rows = 0usize;
    for path in &paths {
        let data = std::fs::read(path)?;
        if data.len() % RAW_RECORD_LEN != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} length is not a multiple of {}",
                    path.display(),
                    RAW_RECORD_LEN
                ),
            ));
        }
        for row in data.chunks_exact(RAW_RECORD_LEN) {
            physical_rows += 1;
            if row[0] == 1 {
                let date = row_date(row);
                if date.len() == DATE_LEN {
                    dates.push(date.to_string());
                }
            }
        }
    }
    dates.sort();
    dates.dedup();
    let date_to_t: HashMap<&str, usize> = dates
        .iter()
        .enumerate()
        .map(|(i, d)| (d.as_str(), i))
        .collect();

    // 既有部分文件以股票上市日作为物理 t=0。逐文件按日期重排后，
    // 所有股票才真正共享同一个全局交易日索引。
    for (i, path) in paths.iter().enumerate() {
        let data = std::fs::read(path)?;
        let mut out = vec![0u8; dates.len() * RAW_RECORD_LEN];
        let mut present = 0usize;
        for row in data.chunks_exact(RAW_RECORD_LEN) {
            if row[0] != 1 {
                continue;
            }
            let date = row_date(row);
            let Some(&t) = date_to_t.get(date) else {
                continue;
            };
            let base = t * RAW_RECORD_LEN;
            if out[base] == 1 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("duplicate date {date} in {}", path.display()),
                ));
            }
            out[base..base + RAW_RECORD_LEN].copy_from_slice(row);
            out[base + T_OFFSET..base + T_OFFSET + 8]
                .copy_from_slice(&(t as i64).to_le_bytes());
            present += 1;
        }
        let check = out
            .chunks_exact(RAW_RECORD_LEN)
            .filter(|row| row[0] == 1)
            .count();
        if check != present {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("realign validation failed: {}", path.display()),
            ));
        }
        atomic_write(path, &out)?;
        if (i + 1) % 500 == 0 {
            eprintln!("realigned files={}/{}", i + 1, paths.len());
        }
    }

    let json = serde_json::to_vec(&dates).map_err(io::Error::other)?;
    let target = root.join("calendar.json");
    atomic_write(&target, &json)?;
    println!(
        "calendar rebuilt and RawDailyBar realigned: files={} physical_rows={} dates={} first={} last={} path={}",
        paths.len(),
        physical_rows,
        dates.len(),
        dates.first().map(String::as_str).unwrap_or(""),
        dates.last().map(String::as_str).unwrap_or(""),
        target.display()
    );
    Ok(())
}
