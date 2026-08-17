//! stockdb-rs —— A股列式存储数据库的 Rust 实现。
//!
//! 与 Python `stockdb/engine.py` 二进制布局严格兼容: 定长 `.dat`
//! (cal.n × rbytes) + 首字节 present 标记 + 全局交易日历 `t` 对齐。
//! 支持只读 (mmap/读) 与写入 (write/repack/.meta)。
//!
//! 极致性能要点:
//! - `Store` 内部缓存 `Mmap`，`read_at` 仅解码目标行字节，不读全文件。
//! - `Record.fields` 为列式 `Vec<Value>`，下标定位 O(1)，无 HashMap 分配。
//! - `read_mmap` 直接基于 mmap 切片解码，避免整文件堆拷贝。

pub mod calendar;
pub mod ffi;
pub mod layout;
pub mod view;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use memmap2::Mmap;

pub use calendar::TradingCalendar;
pub use layout::{decode_row, encode_row, record_len, field_index, FieldKind, Value};

/// 一条记录: 全局交易日索引 t + 列式字段(按 schema 顺序) + 编码布局。
#[derive(Debug, Clone)]
pub struct Record {
    pub t: i64,
    /// 字段值，按 schema 顺序（与 `field_kinds` 下标一致）。
    pub fields: Vec<Value>,
    /// 编码布局(保序), 供 `write`/`repack` 对称回字节。
    pub layout: Vec<(String, char)>,
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
    cal: TradingCalendar,
    mmaps: RwLock<HashMap<PathBuf, Arc<Mmap>>>,
}

impl Store {
    /// 打开根目录, 加载 `calendar.json`。
    pub fn open<P: AsRef<Path>>(root: P) -> std::io::Result<Self> {
        let root = root.as_ref().to_path_buf();
        let cal_path = root.join("calendar.json");
        let cal = TradingCalendar::load(&cal_path)?;
        Ok(Self {
            root,
            cal,
            mmaps: RwLock::new(HashMap::new()),
        })
    }

    pub fn calendar(&self) -> &TradingCalendar {
        &self.cal
    }

    /// 判断某表某票的数据文件是否存在。
    pub fn exists(&self, table: &str, code: &str) -> bool {
        self.root.join(table).join(format!("{code}.dat")).exists()
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
                if h != self.cal.hash() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "{table}/{code}.meta cal_hash mismatch (file={h}, cal={})",
                            self.cal.hash()
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
        let max_t = records.iter().map(|r| r.t).max().unwrap_or(0);
        let n = target_n.unwrap_or((max_t as usize + 1).max(records.len()));
        let mut buf = vec![0u8; n * rlen];
        for rec in records {
            let t = rec.t as usize;
            if t >= n {
                continue;
            }
            // 按布局顺序重组字段 (列式 Vector 已保序, 直接用 layout 取对应下标)
            let ordered: Vec<Value> = rec
                .layout
                .iter()
                .map(|(name, _)| {
                    rec.get(table, name)
                        .cloned()
                        .unwrap_or(Value::Null)
                })
                .collect();
            let row = encode_row(&layout::Record {
                t: rec.t,
                fields: ordered,
                layout: rec.layout.clone(),
            });
            buf[t * rlen..(t + 1) * rlen].copy_from_slice(&row);
        }
        let dir = self.root.join(table);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join(format!("{code}.dat")), &buf)?;
        // 写后失效该文件缓存
        self.mmaps.write().unwrap().remove(&dir.join(format!("{code}.dat")));
        Ok(n)
    }

    /// 将某表某票的文件重排为 `target_n` 长度 (缺槽 present=0)。
    /// 用于统一不同票的行数/cl 对齐。
    pub fn repack(&self, table: &str, code: &str, target_n: usize) -> std::io::Result<usize> {
        let rlen = record_len(table).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "unknown table")
        })?;
        let path = self.root.join(table).join(format!("{code}.dat"));
        let data = std::fs::read(&path)?;
        let old_n = data.len() / rlen;
        let mut buf = vec![0u8; target_n * rlen];
        let copy = old_n.min(target_n);
        buf[..copy * rlen].copy_from_slice(&data[..copy * rlen]);
        std::fs::write(&path, &buf)?;
        self.mmaps.write().unwrap().remove(&path);
        Ok(target_n)
    }

    /// 写 `.meta` (JSON: cal_len / cal_hash / table)。与 Python 引擎一致。
    pub fn write_meta(&self, table: &str, code: &str) -> std::io::Result<()> {
        let meta = serde_json::json!({
            "cal_len": self.cal.len(),
            "cal_hash": self.cal.hash(),
            "table": table,
        });
        let s = serde_json::to_string_pretty(&meta)?;
        let dir = self.root.join(table);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join(format!("{code}.meta")), s)?;
        Ok(())
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
