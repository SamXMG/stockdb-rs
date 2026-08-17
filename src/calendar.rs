//! 交易日历 —— 与 Python `stockdb/calendar.py` 的 JSON 数组格式兼容。
//! `calendar.json` 是紧凑 JSON 字符串数组: ["2023-07-14","2023-07-17",...]

use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct TradingCalendar {
    dates: Vec<String>,
    index: HashMap<String, usize>,
}

impl TradingCalendar {
    /// 从 `calendar.json` 加载（纯字符串数组）。
    /// 使用强类型 `Vec<String>` 解析，避免 `serde_json::Value` 动态开销。
    pub fn load(path: &Path) -> std::io::Result<Self> {
        let txt = std::fs::read_to_string(path)?;
        let arr: Vec<String> = serde_json::from_str(&txt)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let mut dates = Vec::with_capacity(arr.len());
        let mut index = HashMap::with_capacity(arr.len());
        for (i, d) in arr.into_iter().enumerate() {
            index.insert(d.clone(), i);
            dates.push(d);
        }
        Ok(Self { dates, index })
    }

    pub fn len(&self) -> usize {
        self.dates.len()
    }

    pub fn is_empty(&self) -> bool {
        self.dates.is_empty()
    }

    /// date -> t (全局交易日索引)。找不到返回 None。
    pub fn date_to_t(&self, d: &str) -> Option<usize> {
        self.index.get(d).copied()
    }

    /// t -> date。越界返回 None。
    pub fn t_to_date(&self, t: usize) -> Option<&str> {
        self.dates.get(t).map(|s| s.as_str())
    }

    /// 安全版: 找不到返回 default。
    pub fn get_t(&self, d: &str, default: i64) -> i64 {
        self.index.get(d).map(|t| *t as i64).unwrap_or(default)
    }

    /// 日历指纹 (与 Python `Calendar.hash` 一致)。
    /// `md5(f"{first}|{last}|{len}")[:12]`，截断为 12 位十六进制串。
    pub fn hash(&self) -> String {
        use md5::{Digest, Md5};
        let first = self.dates.first().map(|s| s.as_str()).unwrap_or("");
        let last = self.dates.last().map(|s| s.as_str()).unwrap_or("");
        let s = format!("{}|{}|{}", first, last, self.dates.len());
        let mut h = Md5::new();
        h.update(s.as_bytes());
        let digest = h.finalize();
        let hex = format!("{:x}", digest);
        hex.chars().take(12).collect()
    }
}
