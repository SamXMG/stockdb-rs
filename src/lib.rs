//! stockdb-rs —— A股列式存储数据库的 Rust 只读内核。
//!
//! 与 Python `stockdb/engine.py` 二进制布局严格兼容: 定长 `.dat`
//! (cal.n × rbytes) + 首字节 present 标记 + 全局交易日历 `t` 对齐。
//! 当前实现只读 (mmap 零拷贝); 写入/repack 见后续阶段。

pub mod calendar;
pub mod layout;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use memmap2::Mmap;

pub use calendar::TradingCalendar;
pub use layout::{decode_row, record_len, FieldKind, Value};

/// 一条记录: 全局交易日索引 t + 解码后的字段。
#[derive(Debug, Clone)]
pub struct Record {
    pub t: i64,
    pub fields: HashMap<String, Value>,
}

/// 列式存储只读视图。
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
        Ok(decode_row(table, row).map(|fields| Record { t: t as i64, fields }))
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
            if let Some(fields) = decode_row(table, row) {
                out.push(Record { t: t as i64, fields });
            }
        }
        out
    }
}
