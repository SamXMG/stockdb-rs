//! 稀疏历史资金流存储。
//!
//! `FundFlow` 是按全局交易日历展开的定长表，适合日线同步数据；D5/D6
//! 历史缓存通常只有最近若干年，若把它们展开到完整日历会产生大量空槽。
//! 本模块为每只股票提供一个按 `t` 升序排列的稀疏文件：不存日期字符串，
//! 通过根目录 `calendar.json` 将 `t` 还原为日期。

use std::io::{self, Read};
use std::path::Path;

use crate::lock::{atomic_write, with_exclusive_lock};

pub const MAGIC: &[u8; 8] = b"LHFLW001";
pub const VERSION: u32 = 1;
pub const HEADER_LEN: usize = 32;
pub const RECORD_LEN: usize = 56;
pub const PCT_SCALE: f64 = 1_000_000.0;
// 换手率出现过约 9335% 的极端值，使用 1e5 可保留五位小数且不溢出 i32。
pub const EXTRA_SCALE: f64 = 100_000.0;
pub const SCALED_NULL: i32 = i32::MIN;

/// 稀疏资金流一行。缺失数值使用 `f64::NAN`，与 stockdb 的数值空值约定一致。
#[derive(Debug, Clone, PartialEq)]
pub struct FlowRow {
    pub t: i64,
    pub main_net: f64,
    pub main_pct: f64,
    pub xl_net: f64,
    pub xl_pct: f64,
    pub r0_net: f64,
    pub r0_pct: f64,
    pub turnover: f64,
    pub vol_ratio: f64,
    pub source: u8,
}

impl FlowRow {
    pub fn empty(t: i64, source: u8) -> Self {
        Self {
            t,
            main_net: f64::NAN,
            main_pct: f64::NAN,
            xl_net: f64::NAN,
            xl_pct: f64::NAN,
            r0_net: f64::NAN,
            r0_pct: f64::NAN,
            turnover: f64::NAN,
            vol_ratio: f64::NAN,
            source,
        }
    }
}

pub fn source_id(source: &str) -> u8 {
    match source {
        "sina_moneyflow" => 1,
        "eastmoney_fflow" => 2,
        "eastmoney" => 3,
        "fuyao" => 4,
        _ => 0,
    }
}

pub fn source_name(id: u8) -> &'static str {
    match id {
        1 => "sina_moneyflow",
        2 => "eastmoney_fflow",
        3 => "eastmoney",
        4 => "fuyao",
        _ => "legacy_unknown",
    }
}

fn put_f64(dst: &mut Vec<u8>, v: f64) {
    dst.extend_from_slice(&v.to_le_bytes());
}

fn put_scaled(dst: &mut Vec<u8>, v: f64, scale: f64) {
    let raw = if v.is_nan() {
        SCALED_NULL
    } else {
        (v * scale).round() as i32
    };
    dst.extend_from_slice(&raw.to_le_bytes());
}

fn get_f64(src: &[u8], off: &mut usize) -> f64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&src[*off..*off + 8]);
    *off += 8;
    f64::from_le_bytes(b)
}

fn get_scaled(src: &[u8], off: &mut usize, scale: f64) -> f64 {
    let mut b = [0u8; 4];
    b.copy_from_slice(&src[*off..*off + 4]);
    *off += 4;
    let raw = i32::from_le_bytes(b);
    if raw == SCALED_NULL {
        f64::NAN
    } else {
        raw as f64 / scale
    }
}

fn encode_row(row: &FlowRow, dst: &mut Vec<u8>) {
    dst.extend_from_slice(&(row.t as u32).to_le_bytes());
    put_f64(dst, row.main_net);
    put_scaled(dst, row.main_pct, PCT_SCALE);
    put_f64(dst, row.xl_net);
    put_scaled(dst, row.xl_pct, PCT_SCALE);
    put_f64(dst, row.r0_net);
    put_scaled(dst, row.r0_pct, PCT_SCALE);
    put_scaled(dst, row.turnover, EXTRA_SCALE);
    put_scaled(dst, row.vol_ratio, EXTRA_SCALE);
    dst.push(row.source);
    dst.extend_from_slice(&[0u8; 7]);
}

fn decode_row(src: &[u8]) -> FlowRow {
    let mut off = 0usize;
    let mut tb = [0u8; 4];
    tb.copy_from_slice(&src[..4]);
    let t = u32::from_le_bytes(tb) as i64;
    off += 4;
    let main_net = get_f64(src, &mut off);
    let main_pct = get_scaled(src, &mut off, PCT_SCALE);
    let xl_net = get_f64(src, &mut off);
    let xl_pct = get_scaled(src, &mut off, PCT_SCALE);
    let r0_net = get_f64(src, &mut off);
    let r0_pct = get_scaled(src, &mut off, PCT_SCALE);
    let turnover = get_scaled(src, &mut off, EXTRA_SCALE);
    let vol_ratio = get_scaled(src, &mut off, EXTRA_SCALE);
    let source = src[off];
    FlowRow { t, main_net, main_pct, xl_net, xl_pct, r0_net, r0_pct, turnover, vol_ratio, source }
}

/// 将一只股票的稀疏资金流写入文件。调用方应保证 `rows` 按 `t` 唯一；
/// 这里仍会排序并去重，后出现的同一 `t` 覆盖前一行。
pub fn write_file(path: &Path, rows: &[FlowRow]) -> io::Result<()> {
    let mut sorted = rows.to_vec();
    sorted.sort_by_key(|r| r.t);
    let mut unique: Vec<FlowRow> = Vec::with_capacity(sorted.len());
    for row in sorted {
        if let Some(last) = unique.last_mut() {
            if last.t == row.t {
                *last = row;
                continue;
            }
        }
        unique.push(row);
    }

    let mut out = Vec::with_capacity(HEADER_LEN + unique.len() * RECORD_LEN);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&(RECORD_LEN as u32).to_le_bytes());
    out.extend_from_slice(&(unique.len() as u64).to_le_bytes());
    out.extend_from_slice(&[0u8; 8]);
    for row in &unique {
        encode_row(row, &mut out);
    }

    with_exclusive_lock(path, || atomic_write(path, &out))
}

/// 读取并验证一只股票的稀疏资金流文件。
pub fn read_file(path: &Path) -> io::Result<Vec<FlowRow>> {
    let mut f = std::fs::File::open(path)?;
    let mut data = Vec::new();
    f.read_to_end(&mut data)?;
    if data.len() < HEADER_LEN || &data[..8] != MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid flow magic"));
    }
    let version = u32::from_le_bytes(data[8..12].try_into().unwrap());
    let record_len = u32::from_le_bytes(data[12..16].try_into().unwrap()) as usize;
    let n = u64::from_le_bytes(data[16..24].try_into().unwrap()) as usize;
    if version != VERSION || record_len != RECORD_LEN {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "unsupported flow format"));
    }
    let expected = HEADER_LEN.checked_add(n.checked_mul(RECORD_LEN).ok_or_else(||
        io::Error::new(io::ErrorKind::InvalidData, "flow row count overflow"))?).ok_or_else(||
        io::Error::new(io::ErrorKind::InvalidData, "flow file length overflow"))?;
    if data.len() != expected {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "flow file length mismatch"));
    }
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        out.push(decode_row(&data[HEADER_LEN + i * RECORD_LEN..HEADER_LEN + (i + 1) * RECORD_LEN]));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_sort_dedup() {
        let path = std::env::temp_dir().join("lh_flow_roundtrip.flow");
        let mut a = FlowRow::empty(3, 1);
        a.main_net = 12.5;
        let mut b = FlowRow::empty(1, 2);
        b.main_pct = -2.25;
        let mut c = FlowRow::empty(3, 4);
        c.main_net = 99.0;
        write_file(&path, &[a, b, c]).unwrap();
        let rows = read_file(&path).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].t, 1);
        assert_eq!(rows[1].t, 3);
        assert_eq!(rows[1].main_net, 99.0);
        assert_eq!(rows[0].main_pct, -2.25);
        let _ = std::fs::remove_file(path);
    }
}
