//! 分时存储（`MinuteBar`）。
//!
//! 语言中立 JSON 块格式：每个 (code, date) 一块，存为 `root/minute/{code}/{date}.min`，
//! 字段名与序列化顺序见 `MinuteBar`。不走列式定长体系。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::lock::atomic_write;

const MINUTE_MAGIC: &[u8; 8] = b"LHMIN001";
const MINUTE_VERSION: u32 = 1;
const MINUTE_ROW_LEN: u32 = 8 * 8;
const MINUTE_HEADER: usize = 8 + 4 + 4 + 8 + 16 + 10 + 32 + 16;

/// 单只票单日分时序列（字段顺序即 JSON 序列化顺序）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MinuteBar {
    pub code: String,
    pub date: String,
    /// 数据来源，例如 eastmoney_trends2 / tencent_minute。
    #[serde(default)]
    pub source: String,
    /// OHLC 质量：real_ohlc 或 price_only。
    #[serde(default)]
    pub ohlc_quality: String,
    /// 每分钟序号 (自开盘起)。
    #[serde(default)]
    pub minutes: Vec<f64>,
    #[serde(default)]
    pub opens: Vec<f64>,
    #[serde(default)]
    pub highs: Vec<f64>,
    #[serde(default)]
    pub lows: Vec<f64>,
    #[serde(default)]
    pub closes: Vec<f64>,
    #[serde(default)]
    pub volumes: Vec<f64>,
    #[serde(default)]
    pub amounts: Vec<f64>,
    /// 分时均价序列（经典分时图第二条线；trends2 parts[2]）。
    /// 既有 `.min` 文件无此字段时 serde 默认空序列，向后兼容。
    #[serde(default)]
    pub avgs: Vec<f64>,
}

/// 分时块存储 (按 code + date 定位文件)。
pub struct MinuteStore {
    root: PathBuf,
}

impl MinuteStore {
    pub fn new<P: AsRef<Path>>(root: P) -> Self {
        Self {
            root: root.as_ref().join("minute"),
        }
    }

    fn path(&self, code: &str, date: &str) -> PathBuf {
        let dir = self.root.join(code.replace(std::path::is_separator, "_"));
        dir.join(format!("{date}.min"))
    }

    /// 写入单日分时块（覆盖写，与 `read` 对称）。
    pub fn write(&self, bar: &MinuteBar) -> std::io::Result<()> {
        let p = self.path(&bar.code, &bar.date);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let n = bar.minutes.len();
        for column in [&bar.opens, &bar.highs, &bar.lows, &bar.closes,
                       &bar.volumes, &bar.amounts, &bar.avgs] {
            if column.len() != n {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "minute columns must have equal lengths",
                ));
            }
        }
        let mut buf = Vec::with_capacity(MINUTE_HEADER + n * MINUTE_ROW_LEN as usize);
        buf.extend_from_slice(MINUTE_MAGIC);
        buf.extend_from_slice(&MINUTE_VERSION.to_le_bytes());
        buf.extend_from_slice(&MINUTE_ROW_LEN.to_le_bytes());
        buf.extend_from_slice(&(n as u64).to_le_bytes());
        push_text(&mut buf, &bar.code, 16);
        push_text(&mut buf, &bar.date, 10);
        push_text(&mut buf, &bar.source, 32);
        push_text(&mut buf, &bar.ohlc_quality, 16);
        for i in 0..n {
            for value in [bar.minutes[i], bar.opens[i], bar.highs[i], bar.lows[i],
                           bar.closes[i], bar.volumes[i], bar.amounts[i], bar.avgs[i]] {
                buf.extend_from_slice(&value.to_le_bytes());
            }
        }
        atomic_write(&p, &buf)
    }

    /// 读取单日分时块（与 `write` 对称）；缺块返回 None。
    pub fn read(&self, code: &str, date: &str) -> std::io::Result<Option<MinuteBar>> {
        let p = self.path(code, date);
        if !p.exists() {
            return Ok(None);
        }
        let data = std::fs::read(&p)?;
        if data.starts_with(MINUTE_MAGIC) {
            return decode_binary(&data).map(Some);
        }
        let txt = String::from_utf8(data).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, e)
        })?;
        let bar: MinuteBar = serde_json::from_str(&txt).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, e)
        })?;
        Ok(Some(bar))
    }

    /// 某只票全部已有分时日期列表。
    pub fn dates_of(&self, code: &str) -> std::io::Result<Vec<String>> {
        let dir = self.root.join(code.replace(std::path::is_separator, "_"));
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(d) = name.strip_suffix(".min") {
                out.push(d.to_string());
            }
        }
        out.sort();
        Ok(out)
    }
}

fn push_text(buf: &mut Vec<u8>, value: &str, width: usize) {
    let bytes = value.as_bytes();
    let end = bytes.len().min(width);
    buf.extend_from_slice(&bytes[..end]);
    buf.resize(buf.len() + width - end, 0);
}

fn decode_text(data: &[u8]) -> String {
    let end = data.iter().position(|b| *b == 0).unwrap_or(data.len());
    String::from_utf8_lossy(&data[..end]).trim().to_string()
}

fn decode_binary(data: &[u8]) -> std::io::Result<MinuteBar> {
    if data.len() < MINUTE_HEADER || &data[..8] != MINUTE_MAGIC {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid minute magic"));
    }
    let version = u32::from_le_bytes(data[8..12].try_into().unwrap());
    let row_len = u32::from_le_bytes(data[12..16].try_into().unwrap());
    let n = u64::from_le_bytes(data[16..24].try_into().unwrap()) as usize;
    if version != MINUTE_VERSION || row_len != MINUTE_ROW_LEN
        || data.len() != MINUTE_HEADER + n * MINUTE_ROW_LEN as usize {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid minute layout"));
    }
    let code = decode_text(&data[24..40]);
    let date = decode_text(&data[40..50]);
    let source = decode_text(&data[50..82]);
    let ohlc_quality = decode_text(&data[82..98]);
    let mut columns: [Vec<f64>; 8] = std::array::from_fn(|_| Vec::with_capacity(n));
    let mut offset = MINUTE_HEADER;
    for _ in 0..n {
        for column in &mut columns {
            let value = f64::from_le_bytes(data[offset..offset + 8].try_into().unwrap());
            column.push(value);
            offset += 8;
        }
    }
    Ok(MinuteBar {
        code, date, source, ohlc_quality,
        minutes: columns[0].clone(), opens: columns[1].clone(),
        highs: columns[2].clone(), lows: columns[3].clone(),
        closes: columns[4].clone(), volumes: columns[5].clone(),
        amounts: columns[6].clone(), avgs: columns[7].clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(n: usize) -> MinuteBar {
        let minutes: Vec<f64> = (0..n).map(|i| i as f64).collect();
        MinuteBar {
            code: "600000".into(),
            date: "2023-07-14".into(),
            source: "test".into(),
            ohlc_quality: "real_ohlc".into(),
            minutes: minutes.clone(),
            opens: minutes.iter().map(|m| 10.0 + m).collect(),
            highs: minutes.iter().map(|m| 10.0 + m * 2.0).collect(),
            lows: minutes.iter().map(|m| 10.0 - m).collect(),
            closes: minutes.iter().map(|m| 10.0 + m * 1.5).collect(),
            volumes: minutes.iter().map(|m| 1000.0 + m).collect(),
            amounts: minutes.iter().map(|m| 1e6 + m * 100.0).collect(),
            // 用精确可表示的增量（避免 f64 在 JSON 往返时踩 Rust FromStr 的 1-ULP 边界），
            // 仍验证 avgs 字段的序列化/反序列化。
            avgs: minutes.iter().map(|m| (*m) + 100.0).collect(),
        }
    }

    #[test]
    fn write_then_read_roundtrip() {
        let tmp = std::env::temp_dir().join("stockdb_rs_minute_test");
        let _ = std::fs::remove_dir_all(&tmp);
        let store = MinuteStore::new(&tmp);
        let bar = sample(48);
        store.write(&bar).unwrap();
        let back = store.read("600000", "2023-07-14").unwrap().unwrap();
        assert_eq!(bar, back);
        let dates = store.dates_of("600000").unwrap();
        assert_eq!(dates, vec!["2023-07-14".to_string()]);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn missing_block_is_none() {
        let tmp = std::env::temp_dir().join("stockdb_rs_minute_missing");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let store = MinuteStore::new(&tmp);
        assert!(store.read("600000", "1999-01-01").unwrap().is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
