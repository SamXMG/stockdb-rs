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
pub mod expr;
pub mod ffi;
pub mod flow;
pub mod layout;
pub mod lock;
pub mod minute;
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
    Ok(())
}

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use memmap2::Mmap;

use crate::lock::{atomic_write, with_exclusive_lock};

pub use calendar::TradingCalendar;
pub use layout::{
    decode_row, encode_row, record_len, field_index, field_kinds, record_layout, FieldKind, Value,
};

/// 是否为"按全局交易日历对齐"的时序表。
/// 时序表按 `cal.len()` 展开 (缺槽 present=0)，非时序/事件表按记录数展开，
/// 避免 CompanyProfile/Announcement/AdjustEvent/RenameEvent 被撑成 cal.len() 条空壳。
pub fn is_calendar_table(table: &str) -> bool {
    matches!(
        table,
        "RawDailyBar" | "FundFlow" | "IndexDaily" | "DailySnapshot" | "IndustryDaily"
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

    pub fn calendar(&self) -> std::sync::RwLockReadGuard<TradingCalendar> {
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
    pub fn read_many(
        &self,
        table: &str,
        code: &str,
        ts: &[usize],
    ) -> std::io::Result<Vec<Record>> {
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
                format!(
                    "{table}/{code}.dat length {len} not multiple of rlen {rlen} (corrupt?)"
                ),
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
                        format!(
                            "{table}/{code}.meta cal_hash mismatch (file={h}, cal={cur})",
                        ),
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
            let mut recs: Vec<Record> = records
                .iter()
                .map(|r| {
                    let t = cal.ensure(&r.date) as i64;
                    Record {
                        t,
                        date: r.date.clone(),
                        fields: r.fields.clone(),
                        layout: r.layout.clone(),
                    }
                })
                .collect();
            recs.sort_by_key(|r| r.t);
            let cal_len = cal.len();
            drop(cal);
            // 3) 目标长度:
            //    - 时序表(按全局交易日历对齐): 显式指定 > 日历长度 > max(t)+1
            //    - 非时序/事件表(CompanyProfile/Announcement/AdjustEvent/RenameEvent):
            //      仅按实际记录展开 (max_t+1)，不撑满日历，避免 cal.len() 条空壳爆炸
            let max_t = recs.iter().map(|r| r.t).max().unwrap_or(0);
            let n = if is_calendar_table(table) {
                target_n
                    .unwrap_or_else(|| (max_t as usize + 1).max(cal_len))
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
