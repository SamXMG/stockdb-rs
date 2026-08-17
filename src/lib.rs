//! stockdb-rs —— A股列式存储数据库的 Rust 实现。
//!
//! 与 Python `stockdb/engine.py` 二进制布局严格兼容: 定长 `.dat`
//! (cal.n × rbytes) + 首字节 present 标记 + 全局交易日历 `t` 对齐。
//! 支持只读 (mmap/读) 与写入 (write/repack/.meta)。

pub mod calendar;
pub mod layout;
pub mod view;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use memmap2::Mmap;

pub use calendar::TradingCalendar;
pub use layout::{decode_row, encode_row, record_len, FieldKind, Value};

/// 一条记录: 全局交易日索引 t + 解码后的字段(无序 HashMap) + 编码布局。
#[derive(Debug, Clone)]
pub struct Record {
    pub t: i64,
    /// 字段名 -> 值。
    pub fields: HashMap<String, Value>,
    /// 编码布局(保序), 供 `write`/`repack` 对称回字节。
    pub layout: Vec<(String, char)>,
}

/// 列式存储视图 (读写均可)。
pub struct Store {
    root: PathBuf,
    cal: TradingCalendar,
}

impl Store {
    /// 打开根目录, 加载 `calendar.json`。
    pub fn open<P: AsRef<Path>>(root: P) -> std::io::Result<Self> {
        let root = root.as_ref().to_path_buf();
        let cal_path = root.join("calendar.json");
        let cal = TradingCalendar::load(&cal_path)?;
        Ok(Self { root, cal })
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

    /// mmap 只读整张表某票。适合大文件零拷贝场景。
    pub fn read_mmap(&self, table: &str, code: &str) -> std::io::Result<Vec<Record>> {
        let path = self.root.join(table).join(format!("{code}.dat"));
        let file = std::fs::File::open(&path)?;
        let mmap = unsafe { Mmap::map(&file)? };
        Ok(self.decode_all(table, &mmap))
    }

    /// 按 t O(1) 取单条记录。越界或空槽返回 None。
    pub fn read_at(&self, table: &str, code: &str, t: usize) -> std::io::Result<Option<Record>> {
        let rlen = record_len(table).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "unknown table")
        })?;
        let path = self.root.join(table).join(format!("{code}.dat"));
        let data = std::fs::read(&path)?;
        let n = data.len() / rlen;
        if t >= n {
            return Ok(None);
        }
        let row = &data[t * rlen..(t + 1) * rlen];
        Ok(self.row_to_record(table, row, t as i64))
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
            // 按布局顺序重组字段 (HashMap 迭代顺序不确定, 必须按 layout 取序)
            let ordered: Vec<(String, Value)> = rec
                .layout
                .iter()
                .map(|(name, _)| {
                    (
                        name.clone(),
                        rec.fields.get(name).cloned().unwrap_or(Value::Null),
                    )
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

    fn row_to_record(&self, table: &str, row: &[u8], t: i64) -> Option<Record> {
        let lr = decode_row(table, row)?;
        let fields = lr
            .fields
            .into_iter()
            .map(|(k, v)| (k, v))
            .collect::<HashMap<_, _>>();
        Some(Record {
            t,
            fields,
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
