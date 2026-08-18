//! 交易日历 —— 语言中立 JSON 字符串数组格式。
//! `calendar.json` 为紧凑 JSON 数组: ["2023-07-14","2023-07-17",...]

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
    /// 加载后**排序 + 去重**，保证日历严格升序（ISO 日期串字典序即时间序），
    /// 修正此前 ensure 尾追加导致的乱序膨胀。
    pub fn load(path: &Path) -> std::io::Result<Self> {
        let txt = std::fs::read_to_string(path)?;
        let arr: Vec<String> = serde_json::from_str(&txt)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let mut unique: Vec<String> = arr.into_iter().collect();
        unique.sort();
        unique.dedup();
        let mut dates = Vec::with_capacity(unique.len());
        let mut index = HashMap::with_capacity(unique.len());
        for (i, d) in unique.into_iter().enumerate() {
            index.insert(d.clone(), i);
            dates.push(d);
        }
        Ok(Self { dates, index })
    }

    /// 空日历（用于首次打开尚不存在的数据根目录）。
    pub fn empty() -> Self {
        Self {
            dates: Vec::new(),
            index: HashMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.dates.len()
    }

    pub fn is_empty(&self) -> bool {
        self.dates.is_empty()
    }

    /// append-only 扩展: 若 `d` 不在日历, 按升序插入到正确位置并返回新 t; 否则返回已有 t。
    /// ISO 日期串可直接字典序比较，保证日历始终严格升序（避免乱序膨胀）。
    /// 全局交易日索引唯一且稳定，append-only 扩展。
    pub fn ensure(&mut self, d: &str) -> usize {
        if let Some(&t) = self.index.get(d) {
            return t;
        }
        // 二分查找插入点 (dates 已升序)
        let pos = self.dates.binary_search_by(|x| x.as_str().cmp(d)).unwrap_err();
        self.dates.insert(pos, d.to_string());
        // 重建索引 (插入后后续偏移全部 +1)
        self.index.clear();
        for (i, x) in self.dates.iter().enumerate() {
            self.index.insert(x.clone(), i);
        }
        pos
    }

    /// 把另一个日历的日期并入自身（去重 + 保持升序 + 重建索引）。
    ///
    /// 用于跨进程写日历时，把磁盘上已被其他进程 `ensure` 过的日期补回内存，
    /// 避免 `save_calendar` 互相覆盖导致丢失交易日。O((n+m)·log(n+m))。
    pub fn merge(&mut self, other: &TradingCalendar) {
        if other.dates.is_empty() {
            return;
        }
        let mut all: Vec<String> = self.dates.clone();
        all.extend(other.dates.iter().cloned());
        all.sort();
        all.dedup();
        let mut index = HashMap::with_capacity(all.len());
        for (i, d) in all.iter().enumerate() {
            index.insert(d.clone(), i);
        }
        self.dates = all;
        self.index = index;
    }

    /// 序列化回 `calendar.json` 格式 (紧凑字符串数组)。
    pub fn to_json(&self) -> String {
        serde_json::to_string(&self.dates)
            .unwrap_or_else(|_| "[]".to_string())
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

    /// 日历指纹：md5(first|last|len) 截断为 12 位十六进制串。
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
