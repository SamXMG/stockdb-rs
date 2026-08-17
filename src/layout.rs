//! 二进制编码契约 —— 与 Python `stockdb/engine.py` 的 `_TABLE_FIELDS` /
//! `_BOOL_FIELDS` / `_STR_W` / `_build_layout` 严格 1:1 对应。
//!
//! 每条记录布局（小端 `<`）：
//!   首字节 `<?` 为 present 标记（1=有数据, 0=空槽）；其后按字段序列排布。
//!   - bool 字段：`?` (1 字节 u8)
//!   - 字符串字段：`{w}s` 定宽 utf-8，右截断 + `\x00` 补齐
//!   - `t` 字段：`q` (i64，全局交易日索引)
//!   - 其余数值：`d` (f64，空值用 NaN 占位)

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

use std::collections::HashMap;

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

/// 一行解码结果（列式，与 schema 声明顺序一致），可直接对称编码回字节。
///
/// 为极致性能，`fields` 采用**列式定长 `Vec<Value>`**：第 `i` 个槽位对应
/// `field_kinds(table)[i]` 的字段，下标定位 O(1)，无字符串 key 散列与堆分配。
/// 字段名索引由 `field_index(table)` 提供。
#[derive(Debug, Clone, Default)]
pub struct Record {
    pub t: i64,
    /// 字段值，按 schema 顺序排列（与 `field_kinds` 下标一致）。
    pub fields: Vec<Value>,
    /// 编码所需的字段布局：(字段名, format_char) 序列。
    /// 仅在 `decode_row` 解码出的 Record 上填充，用于对称 `encode_row`。
    pub layout: Vec<(String, char)>,
}

/// 字段名 -> 在 `Record.fields` / `field_kinds` 中的下标。
/// 供列式访问按名定位（仅在需要时调用，热路径建议直接按下标）。
pub fn field_index(table: &str) -> Option<HashMap<String, usize>> {
    let fields = TABLE_FIELDS.iter().find(|(t, _)| *t == table)?.1;
    let mut m = HashMap::with_capacity(fields.len());
    for (i, f) in fields.iter().enumerate() {
        m.insert((*f).to_string(), i);
    }
    Some(m)
}

/// 解码单行（含 present 判断），返回保序 `Record`。
/// `buf` 长度须等于 `record_len(table)`。返回 `None` 表示该槽为空（present=0）。
pub fn decode_row(table: &str, buf: &[u8]) -> Option<Record> {
    let kinds = field_kinds(table)?;
    let rlen = record_len(table)?;
    if buf.len() < rlen {
        return None;
    }
    if buf[0] == 0 {
        return None; // 空槽
    }
    let mut off = 1usize;
    let mut rec = Record::default();
    rec.fields.reserve(kinds.len());
    for (name, kind) in kinds {
        let v = match kind {
            FieldKind::Bool => {
                let b = buf[off];
                off += 1;
                Value::Bool(b != 0)
            }
            FieldKind::Str(w) => {
                // 零拷贝友好：先按 \0 截断再 trim，避免扫描整段后多余分配。
                let raw = &buf[off..off + w];
                off += w;
                let end = raw.iter().position(|&c| c == 0).unwrap_or(w);
                let s = std::str::from_utf8(&raw[..end])
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
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
        if name == "t" {
            if let Value::I64(t) = &v {
                rec.t = *t;
            }
        }
        rec.layout.push((name.clone(), format_char(&kind)));
        rec.fields.push(v);
    }
    Some(rec)
}

/// 字段类型 -> struct format char（与 Python `_build_layout` 一致）。
fn format_char(kind: &FieldKind) -> char {
    match kind {
        FieldKind::Bool => '?',
        FieldKind::Str(_) => 's',
        FieldKind::T => 'q',
        FieldKind::F64 => 'd',
        FieldKind::Present => '?',
    }
}

/// 将单个 `Value` 编码为字节（不含 present 标记），写入 `out`。
/// `kind` 决定宽度与编码方式，与 `decode_row` 完全对称。
fn encode_value(out: &mut Vec<u8>, v: &Value, kind: &FieldKind) {
    match (kind, v) {
        (FieldKind::Bool, Value::Bool(b)) => {
            out.push(if *b { 1 } else { 0 });
        }
        (FieldKind::Str(w), Value::Str(s)) => {
            let mut b = s.as_bytes().to_vec();
            if b.len() > *w {
                b.truncate(*w); // 右截断
            } else {
                b.resize(*w, 0); // 右补 \x00
            }
            out.extend_from_slice(&b);
        }
        (FieldKind::T, Value::I64(i)) => {
            out.extend_from_slice(&i.to_le_bytes());
        }
        (FieldKind::T, Value::Null) => {
            out.extend_from_slice(&0i64.to_le_bytes());
        }
        (FieldKind::F64, Value::F64(f)) => {
            out.extend_from_slice(&f.to_le_bytes());
        }
        (FieldKind::F64, Value::Null) => {
            out.extend_from_slice(&f64::NAN.to_le_bytes());
        }
        // 缺失/类型不匹配时按空值兜底
        (FieldKind::Bool, _) => out.push(0),
        (FieldKind::Str(w), _) => out.extend_from_slice(&vec![0u8; *w]),
        (FieldKind::F64, _) => out.extend_from_slice(&f64::NAN.to_le_bytes()),
        (FieldKind::T, _) => out.extend_from_slice(&0i64.to_le_bytes()),
        (FieldKind::Present, _) => unreachable!(),
    }
}

/// 将 `Record` 编码为一行定长字节（含首字节 present=1）。
/// 与 `decode_row` 对称；可直接落盘。
pub fn encode_row(rec: &Record) -> Vec<u8> {
    let kinds: Vec<FieldKind> = rec
        .layout
        .iter()
        .map(|(name, c)| kind_from_char(c, name))
        .collect();
    let mut out = Vec::with_capacity(record_len_of(&kinds));
    out.push(1u8); // present
    for (v, k) in rec.fields.iter().zip(kinds.iter()) {
        encode_value(&mut out, v, k);
    }
    out
}

/// 按字段布局计算 record 字节长度。
fn record_len_of(kinds: &[FieldKind]) -> usize {
    let mut n = 1usize;
    for k in kinds {
        n += match k {
            FieldKind::Bool => 1,
            FieldKind::Str(w) => *w,
            FieldKind::T | FieldKind::F64 => 8,
            FieldKind::Present => 1,
        };
    }
    n
}

/// layout 中的 's' 需要解析真实宽度；这里用全局 STR_W + 字段名反查。
fn kind_from_char(c: &char, name: &str) -> FieldKind {
    match c {
        '?' => FieldKind::Bool,
        'q' => FieldKind::T,
        'd' => FieldKind::F64,
        's' => {
            let w = STR_W
                .iter()
                .find(|(s, _)| *s == name)
                .map(|(_, w)| *w)
                .unwrap_or(0);
            FieldKind::Str(w)
        }
        _ => FieldKind::F64,
    }
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
