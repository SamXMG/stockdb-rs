//! 交易日历 —— 语言中立 JSON 字符串数组格式。
//! `calendar.json` 为紧凑 JSON 数组: ["2023-07-14","2023-07-17",...]

use std::collections::HashMap;
use std::path::Path;

/// 判断 "YYYY-MM-DD" 是否为周六/周日。非法格式按 false 处理(不误伤)。
pub fn is_weekend_date(d: &str) -> bool {
    if d.len() != 10 {
        return false;
    }
    let bytes = d.as_bytes();
    if bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    let (y, m, day) = match (
        d[0..4].parse::<i32>(),
        d[5..7].parse::<i32>(),
        d[8..10].parse::<i32>(),
    ) {
        (Ok(y), Ok(m), Ok(day)) => (y, m, day),
        _ => return false,
    };
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) {
        return false;
    }
    // 蔡勒公式变体: 计算该日星期, 0=周日 6=周六
    let (y2, m2) = if m < 3 { (y - 1, m + 12) } else { (y, m) };
    let c = y2 / 100;
    let yy = y2 % 100;
    let w = (yy + yy / 4 + c / 4 - 2 * c + (26 * (m2 + 1)) / 10 + day - 1).rem_euclid(7);
    w == 0 || w == 6
}

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
    /// 同时**过滤周末幽灵日**（2026-09-16 起：历史脏日历加载时自动净化）。
    pub fn load(path: &Path) -> std::io::Result<Self> {
        let txt = std::fs::read_to_string(path)?;
        let arr: Vec<String> = serde_json::from_str(&txt)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let mut unique: Vec<String> = arr
            .into_iter()
            .filter(|d| !is_weekend_date(d))
            .collect();
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
    ///
    /// **周末守卫**: A 股日历不允许出现周六/周日。2026-09-16 实测
    /// import_json(snapshot) 在周末运行, 把 08-29/30、09-05/06 四个周末日期
    /// ensure 进日历, 造成幽灵槽位与后续 t 整体偏移。这里硬性拒绝周末,
    /// 防止任何入口(import_json / write / ensure_today)再次污染。
    pub fn ensure(&mut self, d: &str) -> usize {
        if let Some(&t) = self.index.get(d) {
            return t;
        }
        if is_weekend_date(d) {
            return usize::MAX; // 哨兵: 调用方把该记录视为"无合法槽位"跳过
        }
        // 二分查找插入点 (dates 已升序)
        let pos = self
            .dates
            .binary_search_by(|x| x.as_str().cmp(d))
            .unwrap_err();
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
    /// 并入时同样过滤周末幽灵日（与 ensure 守卫一致）。
    pub fn merge(&mut self, other: &TradingCalendar) {
        if other.dates.is_empty() {
            return;
        }
        let mut all: Vec<String> = self.dates.clone();
        all.extend(other.dates.iter().filter(|d| !is_weekend_date(d)).cloned());
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
        serde_json::to_string(&self.dates).unwrap_or_else(|_| "[]".to_string())
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

#[cfg(test)]
mod weekend_tests {
    use super::*;

    #[test]
    fn weekend_detection_matches_known_dates() {
        // 2026-08-29(六) 2026-08-30(日) 2026-09-05(六) 2026-09-06(日)
        assert!(is_weekend_date("2026-08-29"));
        assert!(is_weekend_date("2026-08-30"));
        assert!(is_weekend_date("2026-09-05"));
        assert!(is_weekend_date("2026-09-06"));
        // 工作日
        assert!(!is_weekend_date("2026-09-07"));
        assert!(!is_weekend_date("2026-09-11"));
        assert!(!is_weekend_date("2026-09-16"));
        assert!(!is_weekend_date("2026-09-14"));
        // 非法格式不误伤
        assert!(!is_weekend_date(""));
        assert!(!is_weekend_date("20260907"));
        assert!(!is_weekend_date("abc"));
    }

    #[test]
    fn ensure_rejects_weekend() {
        let mut cal = TradingCalendar::empty();
        cal.ensure("2026-09-11"); // 工作日
        assert_eq!(cal.len(), 1);
        let t = cal.ensure("2026-09-12"); // 周六
        assert_eq!(t, usize::MAX);
        assert_eq!(cal.len(), 1); // 未被插入
    }
}
