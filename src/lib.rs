//! stockdb-rs —— A 股列式存储引擎（Rust 实现，语言中立）。
//!
//! 二进制布局为语言中立契约，与参考实现保持字节级 1:1 兼容：
//! 定长 `.dat`（`cal_len × rlen`）+ 首字节 present 标记 + 全局交易日历 `t` 对齐。
//! 支持只读（mmap / 随机读）与写入（write / repack / .meta）。
//!
//! 性能要点：
//! - `Store` 内部缓存 `Mmap`，`read_at` 仅解码目标行字节，不读全文件。
//! - `Record.fields` 为行式定长 `Vec<Value>`，下标定位 O(1)，无 HashMap 分配。
//! - `read_mmap` 基于 mmap 切片解码，避免整文件堆拷贝。
//! - 查询（`expr`）直接在 mmap 字节上逐行求值，未命中行零解码、零分配
//!   （见 `expr::scan_eval`）；命中行的 JSON 物化 / 二进制 memcpy 由调用方按需选择。

pub mod calendar;
pub mod compact;
pub mod context;
pub mod expr;
pub mod ffi;
pub mod flow;
pub mod labels;
pub mod layout;
pub mod lock;
pub mod minute;
pub mod risk_gate;
pub mod view;

// pyo3 原生绑定（feature-gated）：仅 `cargo build --features pyo3` 时编译，
// 提供 Python 原生 `import stockdb_rs`，与 ffi.rs 的 C ABI 符号共存于同一 cdylib。
#[cfg(feature = "pyo3")]
pub mod pyo3_api;

// 模块入口必须位于 crate 根：cdylib 的导出表只可靠地收纳 crate 根层级的
// `#[export_name]`/`#[no_mangle]` 符号；若 `#[pymodule]` 写在嵌套模块里，
// `PyInit_stockdb_rs` 会被 rustc 当作无 Rust 调用方的死代码消除，导致
// `import stockdb_rs` 报 "does not define module export function"。
// `StockDB` 类本身定义在 `pyo3_api` 子模块（feature-gated，保持隔离）。
#[cfg(feature = "pyo3")]
use pyo3::types::PyModuleMethods;

#[cfg(feature = "pyo3")]
#[pyo3::pymodule]
fn stockdb_rs(m: &pyo3::Bound<'_, pyo3::types::PyModule>) -> pyo3::PyResult<()> {
    m.add_class::<pyo3_api::StockDB>()?;
    m.add_class::<pyo3_api::RiskGate>()?;
    Ok(())
}

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use memmap2::Mmap;

use crate::lock::{atomic_write, with_exclusive_lock};

pub use calendar::TradingCalendar;
pub use layout::{
    decode_row, encode_row, field_index, field_kinds, record_layout, record_len, FieldKind, Value,
};

/// 是否为"按全局交易日历对齐"的时序表。
/// 时序表按 `cal.len()` 展开 (缺槽 present=0)，非时序/事件表按记录数展开，
/// 避免 CompanyProfile/Announcement/AdjustEvent/RenameEvent 被撑成 cal.len() 条空壳。
pub fn is_calendar_table(table: &str) -> bool {
    matches!(
        table,
        "RawDailyBar"
            | "FundFlow"
            | "IndexDaily"
            | "DailySnapshot"
            | "IndustryDaily"
            | "ThemeFlow"
            | "VolumeProfileDaily"
            | "FactorDaily"
            | "LabelDaily"
            | "SignalDaily"
    )
}

/// 一条记录: 全局交易日索引 t + 行式定长字段(按 schema 顺序) + 编码布局。
#[derive(Debug, Clone)]
pub struct Record {
    /// 交易日索引 (由 `write` 内部按 `date` 经日历 ensure 得到, 落盘时定稿)。
    pub t: i64,
    /// 交易日字符串 (yyyy-mm-dd), 供 `write` 内部 ensure 扩展日历并计算 t。
    pub date: String,
    /// 字段值，按 schema 顺序（与 `field_kinds` 下标一致）。
    pub fields: Vec<Value>,
    /// 编码布局(保序), 全表共享的 `Arc`，解码时仅克隆指针。供 `write`/`repack` 对称回字节。
    pub layout: std::sync::Arc<[(String, char)]>,
}

impl Record {
    /// 按字段名取列值（O(1) 下标定位，首次调用按表建索引缓存由调用方负责）。
    pub fn get(&self, table: &str, name: &str) -> Option<&Value> {
        let idx = field_index(table)?;
        idx.get(name).and_then(|&i| self.fields.get(i))
    }

    /// 命名迭代 `(字段名, &值)`,基于 `layout` 字段名,与 `fields` 顺序一致。
    /// 用于兼容需要按名字遍历的场景（如对齐测试），无需额外表名参数。
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.layout
            .iter()
            .zip(self.fields.iter())
            .map(|((n, _), v)| (n.as_str(), v))
    }
}

/// 一列按物理槽位对齐的值向量：长度 = 请求的 `[t0, t1)`，空槽为 `None`。
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnData {
    F64(Vec<Option<f64>>),
    Str(Vec<Option<String>>),
}

impl ColumnData {
    #[inline]
    fn push_none(&mut self) {
        match self {
            ColumnData::F64(v) => v.push(None),
            ColumnData::Str(v) => v.push(None),
        }
    }
}

/// 具名列：`Store::read_columns` 的返回单元。
#[derive(Debug, Clone, PartialEq)]
pub struct NamedColumn {
    pub name: String,
    pub data: ColumnData,
}

/// 按列偏移读数值：F64 直读 / T 转 f64 / Scaled 反缩放（哨兵→None）/ Bool 转 0-1。
#[inline]
fn col_f64(row: &[u8], off: usize, kind: FieldKind) -> Option<f64> {
    match kind {
        FieldKind::F64 => {
            let b: [u8; 8] = row.get(off..off + 8)?.try_into().ok()?;
            Some(f64::from_le_bytes(b))
        }
        FieldKind::T => {
            let b: [u8; 8] = row.get(off..off + 8)?.try_into().ok()?;
            Some(i64::from_le_bytes(b) as f64)
        }
        FieldKind::Scaled(scale) => {
            let b: [u8; 4] = row.get(off..off + 4)?.try_into().ok()?;
            let raw = i32::from_le_bytes(b);
            if raw == crate::layout::SCALED_NULL {
                None
            } else {
                Some(raw as f64 / scale)
            }
        }
        FieldKind::Bool => Some(if *row.get(off)? != 0 { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// 按列偏移读定宽字符串：遇 `\0` 截断后 trim。
#[inline]
fn col_str(row: &[u8], off: usize, w: usize) -> Option<String> {
    let raw = row.get(off..off + w)?;
    let end = raw.iter().position(|&c| c == 0).unwrap_or(w);
    std::str::from_utf8(&raw[..end])
        .ok()
        .map(|s| s.trim().to_string())
}

/// 列式存储视图 (读写均可)。
///
/// `mmaps` 缓存已映射文件，使 `read_at` 在多次随机读时零系统调用、零全量拷贝。
pub struct Store {
    root: PathBuf,
    cal: RwLock<TradingCalendar>,
    mmaps: RwLock<HashMap<PathBuf, Arc<Mmap>>>,
    cal_path: PathBuf,
}

impl Store {
    /// 打开根目录, 加载 `calendar.json`；目录或日历缺失时按空库处理（首次打开即写入场景）。
    pub fn open<P: AsRef<Path>>(root: P) -> std::io::Result<Self> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;
        let cal_path = root.join("calendar.json");
        let cal = if cal_path.exists() {
            TradingCalendar::load(&cal_path)?
        } else {
            TradingCalendar::empty()
        };
        Ok(Self {
            root,
            cal: RwLock::new(cal),
            mmaps: RwLock::new(HashMap::new()),
            cal_path,
        })
    }

    /// Root path for in-crate derived-table builders.  Public language bindings
    /// continue to expose only table/query APIs.
    pub(crate) fn root_dir(&self) -> &Path {
        &self.root
    }

    pub fn calendar(&self) -> std::sync::RwLockReadGuard<'_, TradingCalendar> {
        self.cal.read().unwrap()
    }

    /// 把当前(可能已扩展的)日历写回 `calendar.json`。
    /// 在日历 sidecar 锁保护下做原子写，保证跨进程/跨票一致、且崩溃不留半截文件。
    pub fn save_calendar(&self) -> std::io::Result<()> {
        with_exclusive_lock(&self.cal_path, || self.save_calendar_inner())
    }

    /// 回写日历的实际逻辑（假设调用方已持有日历锁）。
    ///
    /// 先合并磁盘上其他进程已 `ensure` 过的日期（防互相覆盖丢失），再原子写。
    fn save_calendar_inner(&self) -> std::io::Result<()> {
        if self.cal_path.exists() {
            if let Ok(on_disk) = TradingCalendar::load(&self.cal_path) {
                let mut cal = self.cal.write().unwrap();
                cal.merge(&on_disk);
            }
        }
        let json = {
            let cal = self.cal.read().unwrap();
            cal.to_json()
        };
        atomic_write(&self.cal_path, json.as_bytes())
    }

    /// 判断某表某票的数据文件是否存在。
    pub fn exists(&self, table: &str, code: &str) -> bool {
        self.root.join(table).join(format!("{code}.dat")).exists()
    }

    /// 判断某只股票是否有稀疏 D5/D6 历史资金流。
    pub fn flow_exists(&self, code: &str) -> bool {
        self.root
            .join("MoneyFlowHistory")
            .join(format!("{code}.flow"))
            .exists()
    }

    /// 读取某只股票的稀疏 D5/D6 历史资金流。
    pub fn read_flow(&self, code: &str) -> std::io::Result<Vec<flow::FlowRow>> {
        flow::read_file(
            &self
                .root
                .join("MoneyFlowHistory")
                .join(format!("{code}.flow")),
        )
    }

    /// 读取整张表某票的全部非空记录(按 t 升序)。
    pub fn read(&self, table: &str, code: &str) -> std::io::Result<Vec<Record>> {
        let path = self.root.join(table).join(format!("{code}.dat"));
        let data = std::fs::read(&path)?;
        Ok(self.decode_all(table, &data))
    }

    /// mmap 只读整张表某票。适合大文件零拷贝场景（共享映射，不复制进堆）。
    pub fn read_mmap(&self, table: &str, code: &str) -> std::io::Result<Vec<Record>> {
        let mmap = self.mmap_of(table, code)?;
        Ok(self.decode_all(table, &mmap))
    }

    /// 按 t O(1) 取单条记录。**真正零拷贝**: 仅映射文件一次并解码目标行，
    /// 不读全文件、不物化其他行。越界或空槽返回 None。
    pub fn read_at(&self, table: &str, code: &str, t: usize) -> std::io::Result<Option<Record>> {
        let rlen = record_len(table).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "unknown table")
        })?;
        let mmap = self.mmap_of(table, code)?;
        let n = mmap.len() / rlen;
        if t >= n {
            return Ok(None);
        }
        let row = &mmap[t * rlen..(t + 1) * rlen];
        Ok(self.row_to_record(table, row, t as i64))
    }

    /// 批量随机读：给定若干 t，返回对应的记录（缺失/空槽跳过）。
    /// 单次映射复用，比 N 次 `read_at` 更省映射开销。
    pub fn read_many(&self, table: &str, code: &str, ts: &[usize]) -> std::io::Result<Vec<Record>> {
        let rlen = record_len(table).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "unknown table")
        })?;
        let mmap = self.mmap_of(table, code)?;
        let n = mmap.len() / rlen;
        let mut out = Vec::with_capacity(ts.len());
        for &t in ts {
            if t >= n {
                continue;
            }
            if let Some(rec) = self.row_to_record(table, &mmap[t * rlen..(t + 1) * rlen], t as i64)
            {
                out.push(rec);
            }
        }
        Ok(out)
    }

    /// 连续区间读：[t0, t1) 内的记录（按 t 升序）。
    /// 回测最核心的访问模式：取某段历史区间，单次映射、连续切片解码。
    pub fn read_range(
        &self,
        table: &str,
        code: &str,
        t0: usize,
        t1: usize,
    ) -> std::io::Result<Vec<Record>> {
        let rlen = record_len(table).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "unknown table")
        })?;
        let mmap = self.mmap_of(table, code)?;
        let n = mmap.len() / rlen;
        if t0 >= n {
            return Ok(Vec::new());
        }
        let end = t1.min(n);
        let mut out = Vec::with_capacity(end.saturating_sub(t0));
        for t in t0..end {
            if let Some(rec) = self.row_to_record(table, &mmap[t * rlen..(t + 1) * rlen], t as i64)
            {
                out.push(rec);
            }
        }
        Ok(out)
    }

    /// **列式区间读（按物理槽位对齐）** —— 回测取数主路径应走这里。
    ///
    /// 与 `read` / `read_column` 的关键差异：**返回向量长度恒为 `t1-t0`，空槽为 `None`**，
    /// 因此第 `i` 个元素严格对应交易日索引 `t0+i`，可与全局日历对齐。
    /// 旧 `read_column` 会跳过 present=0 的空槽导致长度与物理记录不对齐（Python 侧
    /// 因此弃用它、退回纯 Python struct 解码），本接口即为此而设。
    ///
    /// 行优先单次扫描：一行读入后填充所有请求列，多列共享同一 cache line，
    /// 且全程不物化 `Record` / `Vec<Value>` / `String`（除请求的 Str 列外）。
    pub fn read_columns(
        &self,
        table: &str,
        code: &str,
        fields: &[&str],
        t0: usize,
        t1: usize,
    ) -> std::io::Result<Vec<NamedColumn>> {
        let schema = crate::layout::schema_ref(table).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("unknown table: {table}"),
            )
        })?;
        let rlen = schema.rlen;
        let mmap = self.mmap_of(table, code)?;
        let n = mmap.len() / rlen;
        let start = t0.min(n);
        let end = t1.min(n);
        let rows = end.saturating_sub(start);

        // 预解析列偏移：未知字段名直接报错，绝不静默跳过（否则列会错位）。
        let mut sel: Vec<(usize, FieldKind)> = Vec::with_capacity(fields.len());
        for &f in fields {
            let i = schema.index.get(f).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("unknown field '{f}' in table {table}"),
                )
            })?;
            sel.push(schema.offsets[*i]);
        }

        let mut cols: Vec<ColumnData> = sel
            .iter()
            .map(|(_, k)| match k {
                FieldKind::Str(_) => ColumnData::Str(Vec::with_capacity(rows)),
                _ => ColumnData::F64(Vec::with_capacity(rows)),
            })
            .collect();

        for t in start..end {
            let base = t * rlen;
            let row = &mmap[base..base + rlen];
            let present = row[0] != 0;
            for (ci, &(off, kind)) in sel.iter().enumerate() {
                if !present {
                    cols[ci].push_none();
                    continue;
                }
                match (&mut cols[ci], kind) {
                    (ColumnData::F64(v), _) => v.push(col_f64(row, off, kind)),
                    (ColumnData::Str(v), FieldKind::Str(w)) => v.push(col_str(row, off, w)),
                    // 按 kind 分配容器，数值列不会落到 Str 分支；兜底给 None 而不是 panic
                    (ColumnData::Str(v), _) => v.push(None),
                }
            }
        }

        Ok(fields
            .iter()
            .zip(cols)
            .map(|(&f, data)| NamedColumn {
                name: f.to_string(),
                data,
            })
            .collect())
    }

    /// 列出某表下所有票代码（目录内的 `*.dat` 文件名，去后缀）。
    /// 回测遍历全市场时使用，免去上层自己 read_dir。
    pub fn codes(&self, table: &str) -> std::io::Result<Vec<String>> {
        let dir = self.root.join(table);
        let mut out = Vec::new();
        if !dir.exists() {
            return Ok(out);
        }
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("dat") {
                if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                    out.push(stem.to_string());
                }
            }
        }
        out.sort();
        Ok(out)
    }

    /// 声明式查询：在 `table` 上执行 DSL 表达式，返回所有命中行的 JSON 数组字符串。
    ///
    /// 表达式语法见 `expr` 模块。引擎在列式数据内逐行求值，零拷贝、
    /// 不回传原始数据，仅回传命中行（`code`/`t`/ 全部字段）。DSL 字符串是语言中立
    /// 契约：任何宿主语言只需构造该字符串、解析返回的 JSON 即可，无需回调宿主。
    /// 例：`store.query("RawDailyBar", "close>10 && ma(close,20)>close")`
    /// 跨语言入口见 `ffi::stockdb_query`（C ABI，同构）。
    pub fn query(&self, table: &str, expr: &str) -> Result<String, String> {
        crate::expr::query(self, table, expr)
    }

    /// 跨语言入口见 `ffi::stockdb_query_bin`（C ABI，同构，零 JSON）。
    /// 返回 `[magic][record_len][n_hits][schema_hash][raw rows]` 二进制缓冲，
    /// 调用端按 CONTRACT §4 自行解码，适合宽查询 / 性能关键路径。
    pub fn query_bin(&self, table: &str, expr: &str) -> Result<Vec<u8>, String> {
        crate::expr::query_bin(self, table, expr)
    }

    /// 校验数据完整性（回测前调用，防静默错读）：
    /// - `.dat` 长度须为 `rlen` 整数倍（否则截断/损坏）
    /// - 若 `.meta` 存在，其 `cal_hash` 须与当前日历一致
    /// 返回首个错误；全部通过返回 Ok(())。
    pub fn validate(&self, table: &str, code: &str) -> std::io::Result<()> {
        let rlen = record_len(table).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "unknown table")
        })?;
        let path = self.root.join(table).join(format!("{code}.dat"));
        let len = std::fs::metadata(&path)?.len() as usize;
        if len % rlen != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{table}/{code}.dat length {len} not multiple of rlen {rlen} (corrupt?)"),
            ));
        }
        let meta_path = self.root.join(table).join(format!("{code}.meta"));
        if meta_path.exists() {
            let txt = std::fs::read_to_string(&meta_path)?;
            let meta: serde_json::Value = serde_json::from_str(&txt)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            if let Some(h) = meta.get("cal_hash").and_then(|v| v.as_str()) {
                let cur = self.calendar().hash();
                if h != cur {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("{table}/{code}.meta cal_hash mismatch (file={h}, cal={cur})",),
                    ));
                }
            }
        }
        Ok(())
    }

    /// 将一组记录写入定长 .dat (覆盖写, present 自动标记)。
    /// 每条记录按其 `t` 放入 `t * rlen` 槽位; 缺槽填 present=0 空字节。
    /// `target_n` 不传时取 `max(t)+1` 与记录数较大者。
    ///
    /// 并发安全：整段在「日历排他锁」内完成（合并磁盘日历 → ensure 算 t → 写 .dat →
    /// 回写日历）。`.dat` 的读写改写另在自身 sidecar 锁内做原子写。两锁配合：
    /// - 杜绝两个 writer 交错覆盖导致的数据损坏/丢失；
    /// - 日历锁贯穿 ensure→持久化，保证并发 ingest 时 `t` 全局索引稳定、且不同进程
    ///   不会因各自回写 `calendar.json` 而互相丢失交易日。
    pub fn write(
        &self,
        table: &str,
        code: &str,
        records: &[Record],
        target_n: Option<usize>,
    ) -> std::io::Result<usize> {
        let rlen = record_len(table).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "unknown table")
        })?;
        let path = self.root.join(table).join(format!("{code}.dat"));

        // 静态/事件表没有统一交易日历槽位。尤其 CompanyProfile 没有 date，
        // 绝不能把空字符串送进 calendar.ensure，否则会污染全局日历并造成错位。
        if !is_calendar_table(table) {
            return self.write_non_calendar(table, &path, records, target_n, rlen);
        }

        with_exclusive_lock(&self.cal_path, || {
            // 1) 合并磁盘上其他进程已 ensure 的日期，保证 t 基于最新全局日历计算
            if self.cal_path.exists() {
                if let Ok(on_disk) = TradingCalendar::load(&self.cal_path) {
                    let mut cal = self.cal.write().unwrap();
                    cal.merge(&on_disk);
                }
            }
            // 2) append-only 扩展日历: 所有行的 date 纳入, 返回其全局 t
            let mut cal = self.cal.write().unwrap();
            let mut recs: Vec<Record> = Vec::with_capacity(records.len());
            for r in records {
                if r.date.is_empty() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("{table}/{code}: calendar row has empty date"),
                    ));
                }
                // 日历只能向末尾追加。向历史中间/开头插入会改变既有 t，
                // 但不会自动重排全库 .dat，因此必须显式拒绝。
                if cal.date_to_t(&r.date).is_none() {
                    if let Some(last) = cal.t_to_date(cal.len().saturating_sub(1)) {
                        if r.date.as_str() <= last {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidInput,
                                format!("{table}/{code}: date {} is before/equal calendar tail {}; repack required", r.date, last),
                            ));
                        }
                    }
                }
                let t = cal.ensure(&r.date) as i64;
                if t < 0 {
                    // ensure 周末守卫命中: 该记录无合法槽位, 跳过(不写入,
                    // 不污染日历)。2026-09-16 起日历拒绝周末幽灵日。
                    continue;
                }
                recs.push(Record {
                    t,
                    date: r.date.clone(),
                    fields: r.fields.clone(),
                    layout: r.layout.clone(),
                });
            }
            recs.sort_by_key(|r| r.t);
            let cal_len = cal.len();
            drop(cal);
            // 3) 目标长度:
            //    - 时序表(按全局交易日历对齐): 显式指定 > 日历长度 > max(t)+1
            //    - 非时序/事件表(CompanyProfile/Announcement/AdjustEvent/RenameEvent):
            //      仅按实际记录展开 (max_t+1)，不撑满日历，避免 cal.len() 条空壳爆炸
            let max_t = recs.iter().map(|r| r.t).max().unwrap_or(0);
            let n = if is_calendar_table(table) {
                target_n.unwrap_or_else(|| (max_t as usize + 1).max(cal_len))
            } else {
                target_n.unwrap_or_else(|| max_t as usize + 1)
            };
            // 4) 在 `.dat` 排他锁内读旧 + 原子写新（杜绝两 writer 交错覆盖）
            let result = with_exclusive_lock(&path, || {
                let mut buf = vec![0u8; n * rlen];
                if path.exists() {
                    let old = std::fs::read(&path)?;
                    let old_n = old.len() / rlen;
                    for t in 0..old_n {
                        let off = t * rlen;
                        if old[off] != 1 {
                            continue;
                        }
                        if (t as usize) < n {
                            buf[t as usize * rlen..(t as usize + 1) * rlen]
                                .copy_from_slice(&old[off..off + rlen]);
                        }
                    }
                }
                // 写入新记录(覆盖同槽位)
                for rec in &recs {
                    let t = rec.t as usize;
                    if t >= n {
                        continue;
                    }
                    // 按 layout 顺序重建编码字段；t 字段用重算后的 rec.t（而非 fields 里可能
                    // 残留的旧值），保证落盘字节中 t 为正确全局交易日索引（CONTRACT §4）。
                    let ordered: Vec<Value> = rec
                        .layout
                        .iter()
                        .map(|(name, _)| {
                            if name == "t" {
                                Value::I64(rec.t)
                            } else {
                                rec.get(table, name).cloned().unwrap_or(Value::Null)
                            }
                        })
                        .collect();
                    let row = encode_row(&layout::Record {
                        t: rec.t,
                        fields: ordered,
                        layout: rec.layout.clone(),
                    });
                    buf[t * rlen..(t + 1) * rlen].copy_from_slice(&row);
                }
                atomic_write(&path, &buf)?;
                Ok::<usize, std::io::Error>(n)
            })?;
            self.mmaps.write().unwrap().remove(&path);
            // 5) 日历已扩展, 回写（仍在日历锁内；save_calendar_inner 不再重复加锁）
            self.save_calendar_inner()?;
            Ok(result)
        })
    }

    /// 写入非日历表：使用调用方提供的 `Record.t`，不触碰 calendar.json。
    /// CompanyProfile 通常只有 t=0 的一条静态记录；事件表则由导入器预先把
    /// announce/effective/ex-date 映射为已有交易日索引。
    fn write_non_calendar(
        &self,
        table: &str,
        path: &std::path::Path,
        records: &[Record],
        target_n: Option<usize>,
        rlen: usize,
    ) -> std::io::Result<usize> {
        let max_t = records
            .iter()
            .map(|r| r.t.max(0) as usize)
            .max()
            .unwrap_or(0);
        let n = target_n.unwrap_or(max_t + 1).max(1);
        let result = with_exclusive_lock(path, || {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut buf = vec![0u8; n * rlen];
            if path.exists() {
                let old = std::fs::read(path)?;
                if old.len() % rlen != 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "{} length is not a multiple of record length",
                            path.display()
                        ),
                    ));
                }
                let copy = (old.len() / rlen).min(n);
                buf[..copy * rlen].copy_from_slice(&old[..copy * rlen]);
            }
            for rec in records {
                if rec.t < 0 || rec.t as usize >= n {
                    continue;
                }
                let t = rec.t as usize;
                let ordered: Vec<Value> = rec
                    .layout
                    .iter()
                    .map(|(name, _)| {
                        if name == "t" {
                            Value::I64(rec.t)
                        } else {
                            rec.get(table, name).cloned().unwrap_or(Value::Null)
                        }
                    })
                    .collect();
                let row = encode_row(&layout::Record {
                    t: rec.t,
                    fields: ordered,
                    layout: rec.layout.clone(),
                });
                buf[t * rlen..(t + 1) * rlen].copy_from_slice(&row);
            }
            atomic_write(path, &buf)?;
            Ok::<usize, std::io::Error>(n)
        })?;
        self.mmaps.write().unwrap().remove(path);
        Ok(result)
    }

    /// 将某表某票的文件重排为 `target_n` 长度 (缺槽 present=0)。
    /// 用于统一不同票的行数/cl 对齐。
    ///
    /// 并发安全：在 `.dat` sidecar 锁内做原子写，避免并发 repack/write 交错覆盖。
    pub fn repack(&self, table: &str, code: &str, target_n: usize) -> std::io::Result<usize> {
        let rlen = record_len(table).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "unknown table")
        })?;
        let path = self.root.join(table).join(format!("{code}.dat"));
        let result = with_exclusive_lock(&path, || {
            let data = std::fs::read(&path)?;
            let old_n = data.len() / rlen;
            let mut buf = vec![0u8; target_n * rlen];
            let copy = old_n.min(target_n);
            buf[..copy * rlen].copy_from_slice(&data[..copy * rlen]);
            atomic_write(&path, &buf)?;
            Ok::<usize, std::io::Error>(target_n)
        })?;
        self.mmaps.write().unwrap().remove(&path);
        Ok(result)
    }

    /// 写 `.meta`（JSON: cal_len / cal_hash / table），与列式落盘布局一致。
    ///
    /// 并发安全：在 `.meta` sidecar 锁内做原子写，避免并发写 meta 交错覆盖。
    pub fn write_meta(&self, table: &str, code: &str) -> std::io::Result<()> {
        let meta = serde_json::json!({
            "cal_len": self.calendar().len(),
            "cal_hash": self.calendar().hash(),
            "table": table,
        });
        let s = serde_json::to_string_pretty(&meta)?;
        let dir = self.root.join(table);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{code}.meta"));
        with_exclusive_lock(&path, || atomic_write(&path, s.as_bytes()))
    }

    /// 获取（并缓存）某 .dat 的 mmap。命中缓存直接返回 `Arc`，不重复映射。
    fn mmap_of(&self, table: &str, code: &str) -> std::io::Result<Arc<Mmap>> {
        let path = self.root.join(table).join(format!("{code}.dat"));
        // 先读锁快速命中
        if let Some(m) = self.mmaps.read().unwrap().get(&path) {
            return Ok(Arc::clone(m));
        }
        // 未命中：建映射并写入缓存
        let file = std::fs::File::open(&path)?;
        let mmap = Arc::new(unsafe { Mmap::map(&file)? });
        self.mmaps.write().unwrap().insert(path, Arc::clone(&mmap));
        Ok(mmap)
    }

    fn row_to_record(&self, table: &str, row: &[u8], t: i64) -> Option<Record> {
        let lr = decode_row(table, row)?;
        Some(Record {
            t,
            date: String::new(),
            fields: lr.fields,
            layout: lr.layout,
        })
    }

    fn decode_all(&self, table: &str, data: &[u8]) -> Vec<Record> {
        let rlen = match record_len(table) {
            Some(n) => n,
            None => return Vec::new(),
        };
        if rlen == 0 || data.len() % rlen != 0 {
            return Vec::new();
        }
        let n = data.len() / rlen;
        let mut out = Vec::with_capacity(n);
        for t in 0..n {
            let row = &data[t * rlen..(t + 1) * rlen];
            if let Some(rec) = self.row_to_record(table, row, t as i64) {
                out.push(rec);
            }
        }
        out
    }
}

#[cfg(test)]
mod read_columns_tests {
    use super::*;

    fn rec_with(table: &str, date: &str, close: f64) -> Record {
        let kinds = layout::field_kinds(table).unwrap();
        let layout_arc = layout::record_layout(table).unwrap();
        let fields = kinds
            .iter()
            .map(|(n, k)| match (n.as_str(), k) {
                ("t", _) => Value::I64(0),
                ("date", _) => Value::Str(date.to_string()),
                ("close", _) => Value::F64(close),
                (_, layout::FieldKind::Bool) => Value::Bool(false),
                (_, layout::FieldKind::Str(_)) => Value::Str(String::new()),
                _ => Value::Null,
            })
            .collect();
        Record {
            t: 0,
            date: date.to_string(),
            fields,
            layout: layout_arc,
        }
    }

    /// 这两个断言分别对应旧 `read_column` 的两个致命 bug（Python 侧因此弃用它）：
    /// ① date 等 Str 列恒为 None；② 跳过空槽导致返回长度 ≠ 物理槽位数、无法按 t 对齐。
    #[test]
    fn read_columns_is_slot_aligned_and_returns_str() {
        let root = std::env::temp_dir().join(format!("stockdb-cols-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = Store::open(&root).unwrap();
        store
            .write(
                "RawDailyBar",
                "000001",
                &[
                    rec_with("RawDailyBar", "2026-01-05", 11.70),
                    rec_with("RawDailyBar", "2026-01-06", 99.99),
                    rec_with("RawDailyBar", "2026-01-07", 12.34),
                ],
                Some(4),
            )
            .unwrap();

        // 人为把 t=1 的 present 字节清零，制造「中间空槽」（真实场景=停牌/缺数据）
        let p = root.join("RawDailyBar").join("000001.dat");
        let rlen = layout::record_len("RawDailyBar").unwrap();
        let mut buf = std::fs::read(&p).unwrap();
        buf[rlen] = 0;
        std::fs::write(&p, &buf).unwrap();
        let store = Store::open(&root).unwrap(); // 重开，丢弃旧 mmap 缓存

        let cols = store
            .read_columns("RawDailyBar", "000001", &["date", "close"], 0, 4)
            .unwrap();
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0].name, "date");
        assert_eq!(cols[1].name, "close");

        // ① Str 列必须正常返回，不是 None
        let date = match &cols[0].data {
            ColumnData::Str(v) => v.clone(),
            _ => panic!("date must be decoded as Str"),
        };
        assert_eq!(date[0].as_deref(), Some("2026-01-05"));
        // ② 中间空槽必须占位 None，而不是被跳过导致后续整体前移
        assert_eq!(date[1], None, "t=1 空槽必须占位 None（否则长度不对齐）");
        assert_eq!(date[2].as_deref(), Some("2026-01-07"));
        assert_eq!(date[3], None, "越界槽补 None");
        assert_eq!(date.len(), 4, "长度必须恒等于 t1-t0");

        let close = match &cols[1].data {
            ColumnData::F64(v) => v.clone(),
            _ => panic!("close must be decoded as F64"),
        };
        assert_eq!(close.len(), 4);
        assert!((close[0].unwrap() - 11.70).abs() < 1e-6);
        assert_eq!(close[1], None, "空槽的数值列同样占位 None");
        assert!((close[2].unwrap() - 12.34).abs() < 1e-6);

        // 区间读：长度仍等于区间宽度，且下标相对 t0
        let sub = store
            .read_columns("RawDailyBar", "000001", &["close"], 1, 3)
            .unwrap();
        let c = match &sub[0].data {
            ColumnData::F64(v) => v.clone(),
            _ => unreachable!(),
        };
        assert_eq!(c, vec![None, Some(12.34)], "区间读下标相对 t0=1");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn read_columns_rejects_unknown_field() {
        let root = std::env::temp_dir().join(format!("stockdb-cols2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = Store::open(&root).unwrap();
        store
            .write(
                "RawDailyBar",
                "000001",
                &[rec_with("RawDailyBar", "2026-01-05", 1.0)],
                None,
            )
            .unwrap();
        let e = store
            .read_columns("RawDailyBar", "000001", &["no_such_field"], 0, 1)
            .unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 与逐行物化的 `decode_row` 交叉验证：同一槽位两者必须给出同一数值，
    /// 保证新接口不是"快但错"。
    #[test]
    fn read_columns_matches_decode_row() {
        let root = std::env::temp_dir().join(format!("stockdb-cols3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = Store::open(&root).unwrap();
        store
            .write(
                "RawDailyBar",
                "000001",
                &[
                    rec_with("RawDailyBar", "2026-01-05", 11.70),
                    rec_with("RawDailyBar", "2026-01-06", 12.34),
                ],
                None,
            )
            .unwrap();

        let cols = store
            .read_columns("RawDailyBar", "000001", &["close"], 0, 2)
            .unwrap();
        let got = match &cols[0].data {
            ColumnData::F64(v) => v.clone(),
            _ => unreachable!(),
        };
        for t in 0..2 {
            let rec = store.read_at("RawDailyBar", "000001", t).unwrap().unwrap();
            let v = match rec.get("RawDailyBar", "close").unwrap() {
                Value::F64(x) => Some(*x),
                _ => None,
            };
            assert_eq!(got[t], v, "t={t} 列式与行式解码不一致");
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod write_safety_tests {
    use super::*;

    fn blank_record(table: &str, date: &str, t: i64) -> Record {
        let kinds = layout::field_kinds(table).unwrap();
        let fields = kinds
            .iter()
            .map(|(_, k)| match k {
                layout::FieldKind::Bool => Value::Bool(false),
                layout::FieldKind::Str(_) => Value::Str(String::new()),
                layout::FieldKind::T => Value::I64(t),
                _ => Value::Null,
            })
            .collect();
        Record {
            t,
            date: date.to_string(),
            fields,
            layout: layout::record_layout(table).unwrap(),
        }
    }

    #[test]
    fn static_write_does_not_create_calendar() {
        let root = std::env::temp_dir().join(format!("stockdb-static-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = Store::open(&root).unwrap();
        store
            .write(
                "CompanyProfile",
                "600000",
                &[blank_record("CompanyProfile", "", 0)],
                Some(1),
            )
            .unwrap();
        assert_eq!(store.calendar().len(), 0);
        assert!(!root.join("calendar.json").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn calendar_rejects_historical_insertion() {
        let root = std::env::temp_dir().join(format!("stockdb-calendar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = Store::open(&root).unwrap();
        store
            .write(
                "RawDailyBar",
                "600000",
                &[blank_record("RawDailyBar", "2024-01-02", 0)],
                None,
            )
            .unwrap();
        let e = store
            .write(
                "RawDailyBar",
                "600000",
                &[blank_record("RawDailyBar", "2024-01-01", 0)],
                None,
            )
            .unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
        let _ = std::fs::remove_dir_all(&root);
    }
}
