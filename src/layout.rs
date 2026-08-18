//! 二进制编码契约 —— 语言中立格式。
//!
//! 每条记录布局（小端 `<`）：
//!   首字节 `<?` 为 present 标记（1=有数据, 0=空槽）；其后按字段序列排布。
//!   - bool 字段：`?` (1 字节 u8)
//!   - 字符串字段：`{w}s` 定宽 utf-8，右截断 + `\x00` 补齐
//!   - `t` 字段：`q` (i64，全局交易日索引)
//!   - 其余数值：`d` (f64，空值用 NaN 占位)

/// 各字符串字段宽度（字节），定宽 utf-8 截断补齐。
pub const STR_W: &[(&str, usize)] = &[
    ("code", 16), ("index_code", 16), ("date", 10), ("ex_date", 10),
    ("list_date", 10), ("delist_date", 10), ("ann_date", 10),
    ("announce_date", 10), ("effective_date", 10), ("board", 16),
    ("exchange", 8), ("industry", 24), ("region", 16), ("company_type", 16),
    ("ann_type", 16), ("name", 32), ("former_names", 64), ("full_name", 64),
    ("old_name", 32), ("new_name", 32), ("title", 128), ("summary", 128),
    ("url", 64), ("reason", 64), ("note", 64), ("concepts", 192),
];

/// 各表的 bool 字段集合。
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

/// 各表字段序列（顺序即落盘顺序）。
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
use std::sync::{Arc, Mutex, OnceLock};

/// 字段类型分类。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FieldKind {
    Present, // 首字节标记
    Bool,
    Str(usize),
    T,    // i64 全局交易日索引
    F64,  // float64（含 NaN 空值）
    Scaled(f64), // 缩放整数：磁盘 i32（4 字节），读时 ÷scale 还原 f64；空值用 SCALED_NULL
}

/// 缩放整数列的空值哨兵（i32::MIN）。写时空值/NaN → 此值；读到此值 → `Value::Null` / NaN。
pub const SCALED_NULL: i32 = i32::MIN;

/// 缩放整数列的「字段名 → 缩放因子」。命中则磁盘存 i32（4 字节），否则按 f64（8 字节）。
/// 仅纳入数值范围安全的字段：
///   - 价格类（元，2 位小数）→ 100（open/high/low/close/prev_close/price）；
///   - 百分比/比率类（4 位小数）→ 10000（turnover/chg_pct/vol_ratio/chg60/pe/pb/*_pct 等）。
/// 成交量/成交额/净流入等大数量级字段可能溢出 i32（`f*scale` 超出 ±2.14e9），保持 f64。
pub const SCALED: &[(&str, f64)] = &[
    // 价格类（2 位小数 → ×100）
    ("open", 100.0),
    ("high", 100.0),
    ("low", 100.0),
    ("close", 100.0),
    ("prev_close", 100.0),
    ("price", 100.0),
    // 百分比/比率类（4 位小数 → ×10000）
    ("turnover", 10000.0),
    ("chg_pct", 10000.0),
    ("vol_ratio", 10000.0),
    ("chg60", 10000.0),
    ("pe", 10000.0),
    ("pb", 10000.0),
    ("flow_main_pct", 10000.0),
    ("flow_xl_pct", 10000.0),
    ("flow_l_pct", 10000.0),
    ("main_pct", 10000.0),
    ("xl_pct", 10000.0),
    ("l_pct", 10000.0),
    ("mid_pct", 10000.0),
    ("small_pct", 10000.0),
];

fn scaled_scale_of(name: &str) -> Option<f64> {
    SCALED.iter().find(|(n, _)| *n == name).map(|(_, s)| *s)
}

/// 计算某表的单条记录字节长度（含首字节 present）。
/// 等于 1（present）+ 各字段字节之和，与定长结构计算等价。
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
        } else if scaled_scale_of(f).is_some() {
            n += 4; // 缩放整数 i32（4 字节）
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
        } else if let Some(s) = scaled_scale_of(f) {
            FieldKind::Scaled(s)
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

/// 全表共享的 schema：字段类型序列 + 编码布局 + 字段名下标 + 列字节偏移。
/// 同一张表的 schema 是常量，所有行共享同一份 `Arc`：
/// - 解码时不再每行重建 `field_kinds` / 克隆字段名；
/// - 字节级查询内核直接复用 `index` / `offsets`，避免每次查询重建 HashMap 与累加偏移。
pub struct Schema {
    pub kinds: Vec<(String, FieldKind)>,
    pub layout: Arc<[(String, char)]>,
    /// 字段名 -> 在 `Record.fields` / `field_kinds` 中的下标（供 `bind` 直接复用）。
    pub index: HashMap<String, usize>,
    /// 每个字段在定长 stride 内的字节偏移（present 之后）与类型，
    /// 供字节级 eval 直接按列偏移取 `f64`/`i64`/`bool`/`str`。
    pub offsets: Vec<(usize, FieldKind)>,
}

/// 全局 schema 缓存：首次按表构建，之后所有 `decode_row` / `record_layout` 共享。
/// 这消除了原先「每行都重建 `field_kinds` 向量 + 克隆字段名」的千万级堆分配。
static SCHEMAS: OnceLock<Mutex<HashMap<String, Arc<Schema>>>> = OnceLock::new();

fn schema_arc(table: &str) -> Option<Arc<Schema>> {
    let map = SCHEMAS.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(g) = map.lock() {
        if let Some(s) = g.get(table) {
            return Some(s.clone());
        }
    }
    let kinds = field_kinds(table)?;
    // 字段名 -> 下标（供 `bind` 直接复用，避免每次查询重建 HashMap）。
    let index: HashMap<String, usize> = kinds
        .iter()
        .enumerate()
        .map(|(i, (n, _))| (n.clone(), i))
        .collect();
    // 字段在 stride 内的字节偏移（present 之后），供字节级 eval 按列偏移直接取列值。
    let mut off = 1usize; // present 标记占 1 字节
    let mut offsets: Vec<(usize, FieldKind)> = Vec::with_capacity(kinds.len());
    for (_, kind) in &kinds {
        offsets.push((off, *kind));
        off +=             match kind {
                FieldKind::Bool => 1,
                FieldKind::Str(w) => *w,
                FieldKind::T | FieldKind::F64 => 8,
                FieldKind::Scaled(_) => 4,
                FieldKind::Present => 1,
            };
    }
    let layout: Arc<[(String, char)]> = Arc::from(
        kinds
            .iter()
            .map(|(n, k)| (n.clone(), format_char(k)))
            .collect::<Vec<_>>(),
    );
    let s = Arc::new(Schema {
        kinds,
        layout,
        index,
        offsets,
    });
    if let Ok(mut g) = map.lock() {
        g.insert(table.to_string(), s.clone());
    }
    Some(s)
}

/// 返回全表共享 schema（`index` / `offsets` 已预计算），供字节级查询内核直接复用，
/// 避免每次查询重建字段名下标与累加列偏移。
pub fn schema_ref(table: &str) -> Option<Arc<Schema>> {
    schema_arc(table)
}

/// 一行解码结果（行式定长记录，与 schema 声明顺序一致），可直接对称编码回字节。
///
/// `fields` 为行内定长 `Vec<Value>`：第 `i` 个槽位对应 `field_kinds(table)[i]` 的字段，
/// 下标定位 O(1)。`layout`（字段名+格式字符）为全表共享的 `Arc`，解码时只克隆指针、
/// 不复制字段名字符串。
/// 字段名索引由 `field_index(table)` 提供。
#[derive(Debug, Clone, Default)]
pub struct Record {
    pub t: i64,
    /// 字段值，按 schema 顺序排列（与 `field_kinds` 下标一致）。
    pub fields: Vec<Value>,
    /// 编码所需的字段布局：(字段名, format_char) 序列。
    /// 解码行共享全表 `Arc`（仅原子计数，无字符串堆分配）；写入方自建 Record 也复用同一份。
    pub layout: Arc<[(String, char)]>,
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

/// 全表共享的编码布局 `(字段名, format_char)` 序列（首次构建后缓存）。
/// 供写入方（ingest_bridge / backtest 造数）一次性获取，避免逐行重建。
pub fn record_layout(table: &str) -> Option<Arc<[(String, char)]>> {
    schema_arc(table).map(|s| s.layout.clone())
}

/// 字段布局的确定性指纹（FNV-1a 64），跨平台 / 运行稳定。
///
/// 用于二进制查询结果（`query_bin`）的 schema 版本护栏：调用端可比对
/// 缓冲 header 的 `schema_hash` 与本地 `schema_hash(table)`，确认双方
/// 看到的是同一套字段类型布局（顺序 / 类型 / 宽度一致），避免字节漂移读错。
/// 未知表返回 0。
pub fn schema_hash(table: &str) -> u64 {
    let kinds = match field_kinds(table) {
        Some(k) => k,
        None => return 0,
    };
    let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV offset basis
    for (name, kind) in &kinds {
        for &b in name.as_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3); // FNV prime
        }
        h ^= b'=' as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
        let kc = format_char(kind) as u8;
        h ^= kc as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// 解码单行（含 present 判断），返回保序 `Record`。
/// `buf` 长度须等于 `record_len(table)`。返回 `None` 表示该槽为空（present=0）。
pub fn decode_row(table: &str, buf: &[u8]) -> Option<Record> {
    // 复用全表共享 schema：不再每行重建 `field_kinds` / 克隆字段名（共享缓存，零每行堆分配）。
    let schema = schema_arc(table)?;
    let rlen = record_len(table)?;
    if buf.len() < rlen {
        return None;
    }
    if buf[0] == 0 {
        return None; // 空槽
    }
    let mut off = 1usize;
    let mut rec = Record::default();
    rec.fields.reserve(schema.kinds.len());
    for (name, kind) in &schema.kinds {
        let v = match kind {
            FieldKind::Bool => {
                let b = buf[off];
                off += 1;
                Value::Bool(b != 0)
            }
            FieldKind::Str(w) => {
                // 零拷贝友好：先按 \0 截断再 trim，避免扫描整段后多余分配。
                let raw = &buf[off..off + *w];
                off += *w;
                let end = raw.iter().position(|&c| c == 0).unwrap_or(*w);
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
            FieldKind::Scaled(scale) => {
                let mut arr = [0u8; 4];
                arr.copy_from_slice(&buf[off..off + 4]);
                off += 4;
                let raw = i32::from_le_bytes(arr);
                if raw == SCALED_NULL {
                    Value::Null
                } else {
                    Value::F64(raw as f64 / scale)
                }
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
        rec.fields.push(v);
    }
    // 共享全表 layout：仅 Arc 指针克隆（原子计数），零字符串堆分配。
    rec.layout = schema.layout.clone();
    Some(rec)
}

/// 字段类型 -> 编码字符（与 `encode_row` / `decode_row` 对称）。
pub fn format_char(kind: &FieldKind) -> char {
    match kind {
        FieldKind::Bool => '?',
        FieldKind::Str(_) => 's',
        FieldKind::T => 'q',
        FieldKind::F64 => 'd',
        FieldKind::Scaled(_) => 'I',
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
        (FieldKind::Scaled(scale), Value::F64(f)) => {
            let raw = if f.is_nan() {
                SCALED_NULL
            } else {
                (f * scale).round() as i32
            };
            out.extend_from_slice(&raw.to_le_bytes());
        }
        (FieldKind::Scaled(_), Value::Null) => {
            out.extend_from_slice(&SCALED_NULL.to_le_bytes());
        }
        // 缺失/类型不匹配时按空值兜底
        (FieldKind::Bool, _) => out.push(0),
        (FieldKind::Str(w), _) => out.extend_from_slice(&vec![0u8; *w]),
        (FieldKind::F64, _) => out.extend_from_slice(&f64::NAN.to_le_bytes()),
        (FieldKind::Scaled(_), _) => out.extend_from_slice(&SCALED_NULL.to_le_bytes()),
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
        n +=         match k {
            FieldKind::Bool => 1,
            FieldKind::Str(w) => *w,
            FieldKind::T | FieldKind::F64 => 8,
            FieldKind::Scaled(_) => 4,
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
        'I' => FieldKind::Scaled(scaled_scale_of(name).unwrap_or(1.0)),
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
    fn record_lens_canonical() {
        // 实测字节长度：801 行 × rlen == 72891 ⇒ rlen == 91
        // 801 * rlen == 72891 => rlen == 91
        // RawDailyBar: 价格列(open/high/low/close) + turnover 缩放 → 71
        assert_eq!(record_len("RawDailyBar"), Some(71));
        // IndexDaily: 价格列缩放 → 67
        assert_eq!(record_len("IndexDaily"), Some(67));
        // FundFlow: 5 个 *_pct 缩放 → 95
        assert_eq!(record_len("FundFlow"), Some(95));
        // DailySnapshot: price/prev_close + 9 个百分比/比率列缩放 → 384
        assert_eq!(record_len("DailySnapshot"), Some(384));
        // CompanyProfile: 303579 / 801 = 379
        assert_eq!(record_len("CompanyProfile"), Some(379));
        // AdjustEvent: 47259 / 801 = 59
        assert_eq!(record_len("AdjustEvent"), Some(59));
    }
}
