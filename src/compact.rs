//! 紧凑回测矩阵格式。
//!
//! 每只股票一个文件，文件头保存一次列名；数据区为稀疏交易日行：
//! `t(u32) + values[f32]`。缺失值使用 IEEE NaN。该格式适合因子/标签/信号
//! 的按票 mmap 读取，避免标准日历表重复保存 code/date/维度 ID。

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::Path;

use crate::lock::{atomic_write, with_exclusive_lock};

pub const MAGIC: &[u8; 8] = b"LHMTX001";
pub const VERSION: u32 = 1;
pub const FIXED_HEADER: usize = 40;

#[derive(Debug, Clone, PartialEq)]
pub struct Matrix {
    pub columns: Vec<String>,
    pub rows: Vec<(u32, Vec<f32>)>,
}

fn put_u16(out: &mut Vec<u8>, x: usize) -> io::Result<()> {
    let n = u16::try_from(x)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "matrix string too long"))?;
    out.extend_from_slice(&n.to_le_bytes());
    Ok(())
}

pub fn encode(columns: &[String], rows: &[(u32, Vec<f32>)]) -> io::Result<Vec<u8>> {
    if columns.is_empty() || columns.len() > u32::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "matrix requires 1..u32::MAX columns",
        ));
    }
    for (_, values) in rows {
        if values.len() != columns.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "matrix row width mismatch",
            ));
        }
    }
    let mut header = Vec::new();
    for name in columns {
        put_u16(&mut header, name.len())?;
        header.extend_from_slice(name.as_bytes());
    }
    let header_len = FIXED_HEADER
        .checked_add(header.len())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "matrix header overflow"))?;
    let mut out = Vec::with_capacity(header_len + rows.len() * (4 + 4 * columns.len()));
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&(header_len as u32).to_le_bytes());
    out.extend_from_slice(&(rows.len() as u64).to_le_bytes());
    out.extend_from_slice(&(columns.len() as u32).to_le_bytes());
    out.extend_from_slice(
        &(rows
            .iter()
            .map(|(_, v)| v.iter().filter(|x| x.is_nan()).count() as u64)
            .sum::<u64>())
        .to_le_bytes(),
    );
    out.extend_from_slice(&[0u8; 4]);
    out.extend_from_slice(&header);
    for (t, values) in rows {
        out.extend_from_slice(&t.to_le_bytes());
        for value in values {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
    Ok(out)
}

pub fn write_file(path: &Path, columns: &[String], rows: &[(u32, Vec<f32>)]) -> io::Result<()> {
    // Incremental rebuilds may emit the same trading day more than once.
    // The last input row wins, then rows are persisted in t order.
    let mut by_t = BTreeMap::new();
    for (t, values) in rows {
        by_t.insert(*t, values.clone());
    }
    let sorted: Vec<_> = by_t.into_iter().collect();
    let bytes = encode(columns, &sorted)?;
    with_exclusive_lock(path, || atomic_write(path, &bytes))
}

fn u32_at(data: &[u8], off: &mut usize) -> io::Result<u32> {
    if *off + 4 > data.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "matrix truncated",
        ));
    }
    let x = u32::from_le_bytes(data[*off..*off + 4].try_into().unwrap());
    *off += 4;
    Ok(x)
}

pub fn decode(data: &[u8]) -> io::Result<Matrix> {
    if data.len() < FIXED_HEADER || &data[..8] != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid matrix magic",
        ));
    }
    let version = u32::from_le_bytes(data[8..12].try_into().unwrap());
    let header_len = u32::from_le_bytes(data[12..16].try_into().unwrap()) as usize;
    let nrows = u64::from_le_bytes(data[16..24].try_into().unwrap()) as usize;
    let ncols = u32::from_le_bytes(data[24..28].try_into().unwrap()) as usize;
    if version != VERSION || header_len < FIXED_HEADER || header_len > data.len() || ncols == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported matrix header",
        ));
    }
    let mut off = FIXED_HEADER;
    let mut columns = Vec::with_capacity(ncols);
    for _ in 0..ncols {
        let len = u16_at(data, &mut off)? as usize;
        if off + len > header_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "matrix column header truncated",
            ));
        }
        let name = std::str::from_utf8(&data[off..off + len])
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "matrix column is not utf8"))?;
        columns.push(name.to_string());
        off += len;
    }
    if off != header_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "matrix header length mismatch",
        ));
    }
    let row_len = 4usize
        .checked_add(
            ncols
                .checked_mul(4)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "matrix row overflow"))?,
        )
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "matrix row overflow"))?;
    let expected =
        header_len
            .checked_add(nrows.checked_mul(row_len).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "matrix size overflow")
            })?)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "matrix size overflow"))?;
    if expected != data.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "matrix file length mismatch",
        ));
    }
    let mut rows = Vec::with_capacity(nrows);
    off = header_len;
    for _ in 0..nrows {
        let t = u32_at(data, &mut off)?;
        let mut values = Vec::with_capacity(ncols);
        for _ in 0..ncols {
            if off + 4 > data.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "matrix row truncated",
                ));
            }
            values.push(f32::from_le_bytes(data[off..off + 4].try_into().unwrap()));
            off += 4;
        }
        rows.push((t, values));
    }
    Ok(Matrix { columns, rows })
}

fn u16_at(data: &[u8], off: &mut usize) -> io::Result<u16> {
    if *off + 2 > data.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "matrix truncated",
        ));
    }
    let x = u16::from_le_bytes(data[*off..*off + 2].try_into().unwrap());
    *off += 2;
    Ok(x)
}

pub fn read_file(path: &Path) -> io::Result<Matrix> {
    let mut data = Vec::new();
    std::fs::File::open(path)?.read_to_end(&mut data)?;
    decode(&data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_roundtrip() {
        let cols = vec!["a".to_string(), "b".to_string()];
        let rows = vec![(2, vec![1.25, f32::NAN]), (5, vec![2.5, 3.0])];
        let bytes = encode(&cols, &rows).unwrap();
        let got = decode(&bytes).unwrap();
        assert_eq!(got.columns, cols);
        assert_eq!(got.rows[0].0, 2);
        assert_eq!(got.rows[0].1[0], 1.25);
        assert!(got.rows[0].1[1].is_nan());
    }

    #[test]
    fn compact_write_keeps_last_duplicate_t() {
        let dir = std::env::temp_dir().join(format!("stockdb-compact-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("duplicate.mtx");
        let cols = vec!["value".to_string()];
        let rows = vec![(3, vec![1.0]), (1, vec![2.0]), (3, vec![9.0])];
        write_file(&path, &cols, &rows).unwrap();
        let got = read_file(&path).unwrap();
        assert_eq!(got.rows, vec![(1, vec![2.0]), (3, vec![9.0])]);
        let _ = std::fs::remove_file(path.with_extension("mtx.lock"));
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir(dir);
    }
}
