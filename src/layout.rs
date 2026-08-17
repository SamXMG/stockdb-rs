//! 二进制编码契约 —— 与 Python `stockdb/engine.py` 的 `_TABLE_FIELDS` /
//! `_BOOL_FIELDS` / `_STR_W` / `_build_layout` 严格 1:1 对应。
//!
//! 每条记录布局（小端 `<`）：
//!   首字节 `<?` 为 present 标记（1=有数据, 0=空槽）；其后按字段序列排布。
//!   - bool 字段：`?` (1 字节 u8)
//!   - 字符串字段：`{w}s` 定宽 utf-8，右截断 + `\x00` 补齐
//!   - `t` 字段：`q` (i64，全局交易日索引)
//!   - 其余数值：`d` (f64，空值用 NaN 占位)

use std::collections::HashMap;

/// 各字符串字段宽度（字节），与 Python `_STR_W` 一致。
pub const STR_W: &[(&str, usize)] = &[
    ("code", 16), ("index_code", 16), ("date", 10), ("ex_date", 10),
    ("list_date", 10), ("delist_date", 10), ("ann_date", 10),
    ("announce_date", 10), ("effective_date", 10), ("board", 16),
    ("exchange", 8), ("industry", 24), ("region", 16), ("company_type", 16),
    ("ann_type", 16), ("name", 32), ("former_names", 64), ("full_name", 64),
    ("old_name", 32), ("new_name", 32), ("title", 128), ("summary", 128),
    ("url", 64), ("reason", 64), ("note", 64), ("concepts", 192),
];

/// 各表的 bool 字段集合，与 Python `_BOOL_FIELDS` 一致。
pub const BOOL_FIELDS: &[(&str, &[&str])] = &[
    ("RawDailyBar", &[]),
    ("FundFlow", &[]),
    ("AdjustEvent", &[]),
    ("IndexDaily", &[]),
    ("CompanyProfile", &[
        "is_st", "is_hs300", "is_zz500", "is_zz1000", "is_zz2000", "is_finance",
    ]),
    ("DailySnapshot", &["is_st"]),
    ("Announcement", &[]),
    ("RenameEvent", &[]),
];

/// 各表字段序列（顺序即落盘顺序），与 Python `_TABLE_FIELDS` 一致。
pub const TABLE_FIELDS: &[(&str, &[&str])] = &[
    ("RawDailyBar", &[
        "code", "t", "date", "open", "high", "low", "close",
        "volume", "amount", "turnover",
    ]),
    ("FundFlow", &[
        "code", "t", "date", "main_net", "main_pct", "xl_net",
        "xl_pct", "l_net", "l_pct", "mid_net", "mid_pct",
        "small_net", "small_pct",
    ]),
    ("AdjustEvent", &[
        "code", "ex_date", "t", "bonus_per_share",
        "cash_per_share", "fwd_ratio",
    ]),
    ("IndexDaily", &[
        "index_code", "t", "date", "open", "high", "low",
        "close", "volume", "amount",
    ]),
    ("CompanyProfile", &[
        "code", "name", "former_names", "board", "exchange",
        "list_date", "delist_date", "is_st", "industry",
        "region", "full_name", "total_shares", "float_shares",
        "market_cap_yi", "float_cap_yi", "is_hs300",
        "is_zz500", "is_zz1000", "is_zz2000", "is_finance",
        "company_type", "note",
    ]),
    ("Announcement", &[
        "code", "ann_date", "ann_type", "title", "summary",
        "url", "t",
    ]),
    ("RenameEvent", &[
        "code", "announce_date", "effective_date", "old_name",
        "new_name", "reason", "t",
    ]),
    ("DailySnapshot", &[
        "code", "date", "t", "name", "board", "is_st",
        "price", "prev_close", "chg_pct", "vol_ratio",
        "turnover", "market_cap_yi", "float_cap_yi", "pe",
        "pb", "chg60", "flow_main", "flow_main_pct",
        "flow_xl", "flow_xl_pct", "flow_l", "flow_l_pct",
        "industry", "concepts",
    ]),
];

/// 字段类型分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Present, // 首字节标记
    Bool,
    Str(usize),
    T,    // i64 全局交易日索引
    F64,  // float64（含 NaN 空值）
}

/// 计算某表的单条记录字节长度（含首字节 present）。
/// 与 Python `struct.calcsize("<?" + ...)` 等价。
pub fn record_len(table: &str) -> Option<usize> {
    let fields = TABLE_FIELDS.iter().find(|(t, _)| *t == table)?.1;
    let bools: Vec<&str> = BOOL_FIELDS
        .iter()
        .find(|(t, _)| *t == table)
        .map(|(_, b)| b.to_vec())
        .unwrap_or_default();
    let mut n = 1usize; // present 标记
    for f in fields {
        if bools.contains(f) {
            n += 1;
        } else if let Some((_, w)) = STR_W.iter().find(|(s, _)| *s == *f) {
            n += w;
        } else if *f == "t" {
            n += 8;
        } else {
            n += 8; // f64
        }
    }
    Some(n)
}

/// 解码一行的字段名→类型映射（不含 present）。
pub fn field_kinds(table: &str) -> Option<Vec<(String, FieldKind)>> {
    let fields = TABLE_FIELDS.iter().find(|(t, _)| *t == table)?.1;
    let bools: Vec<&str> = BOOL_FIELDS
        .iter()
        .find(|(t, _)| *t == table)
        .map(|(_, b)| b.to_vec())
        .unwrap_or_default();
    let mut out = Vec::new();
    for f in fields {
        let kind = if bools.contains(f) {
            FieldKind::Bool
        } else if let Some((_, w)) = STR_W.iter().find(|(s, _)| *s == *f) {
            FieldKind::Str(*w)
        } else if *f == "t" {
            FieldKind::T
        } else {
            FieldKind::F64
        };
        out.push((f.to_string(), kind));
    }
    Some(out)
}

/// 解码后的字段值。
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Str(String),
    I64(i64),
    F64(f64),
    Bool(bool),
    Null, // present=0 或 NaN
}

/// 解码单行（含 present 判断）。`buf` 长度须等于 `record_len(table)`。
/// 返回 `None` 表示该槽为空（present=0）。
pub fn decode_row(table: &str, buf: &[u8]) -> Option<HashMap<String, Value>> {
    let kinds = field_kinds(table)?;
    let rlen = record_len(table)?;
    if buf.len() < rlen {
        return None;
    }
    if buf[0] == 0 {
        return None; // 空槽
    }
    let mut off = 1usize;
    let mut map = HashMap::new();
    for (name, kind) in kinds {
        let v = match kind {
            FieldKind::Bool => {
                let b = buf[off];
                off += 1;
                Value::Bool(b != 0)
            }
            FieldKind::Str(w) => {
                let s = String::from_utf8_lossy(&buf[off..off + w])
                    .split('\0')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                off += w;
                Value::Str(s)
            }
            FieldKind::T => {
                let mut arr = [0u8; 8];
                arr.copy_from_slice(&buf[off..off + 8]);
                off += 8;
                Value::I64(i64::from_le_bytes(arr))
            }
            FieldKind::F64 => {
                let mut arr = [0u8; 8];
                arr.copy_from_slice(&buf[off..off + 8]);
                off += 8;
                let f = f64::from_le_bytes(arr);
                if f.is_nan() {
                    Value::Null
                } else {
                    Value::F64(f)
                }
            }
            FieldKind::Present => unreachable!(),
        };
        map.insert(name, v);
    }
    Some(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_lens_match_python() {
        // Python struct.calcsize 实测值(见 engine.py 输出/RawDailyBar 72891/801)
        // 801 * rlen == 72891 => rlen == 91
        assert_eq!(record_len("RawDailyBar"), Some(91));
        // CompanyProfile: 303579 / 801 = 379
        assert_eq!(record_len("CompanyProfile"), Some(379));
        // AdjustEvent: 47259 / 801 = 59
        assert_eq!(record_len("AdjustEvent"), Some(59));
    }
}
