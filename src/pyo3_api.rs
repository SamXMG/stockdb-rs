//! pyo3 原生绑定（feature-gated，默认不编译）。
//!
//! 仅 `cargo build --features pyo3` 时生效：在 cdylib 中额外导出 `PyInit_stockdb_rs`，
//! 使 Python 端可 `import stockdb_rs` 直接获得原生 `StockDB` 类。与 `ffi.rs` 的 C ABI
//! 符号（`stockdb_open` 等）共存于同一动态库，二者互不干扰。
//!
//! 设计取向：Python 端只消费 Rust 计算，**不自己实现解码器**（消除「双 decoder」漂移）。
//! `read_rows` 直接复用 `Store::read` + `Record::iter`，按 CONTRACT §3 字段顺序产出 dict，
//! 与 Python 参考实现 `ColumnStore.read` 同构；i32 缩放列由 Rust 侧已还原为 f64，无需 Python 处理。
#![cfg(feature = "pyo3")]

use pyo3::exceptions::{PyIOError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyDictMethods, PyList, PyListMethods};
use std::sync::Arc;

use crate::layout::FieldKind;
use crate::minute::{MinuteBar, MinuteStore};
use crate::{Record, Store, Value};
use std::path::PathBuf;

#[pyclass]
pub struct StockDB {
    inner: Store,
    root: PathBuf,
}

#[pymethods]
impl StockDB {
    #[new]
    fn new(root: &str) -> PyResult<Self> {
        Store::open(root)
            .map(|s| StockDB {
                inner: s,
                root: PathBuf::from(root),
            })
            .map_err(|e| PyErr::new::<PyIOError, _>(format!("stockdb open failed: {e}")))
    }

    /// 读某 code 全部非空行，返回 list[dict]，与 Python 参考实现 ColumnStore.read 同构。
    /// 每个 dict 含 §3.1 全部字段（code/date/t/open/...），空值/NaN 为 None。
    fn read_rows(&self, table: &str, code: &str, py: Python<'_>) -> PyResult<Vec<Py<PyDict>>> {
        let recs = self
            .inner
            .read(table, code)
            .map_err(|e| PyErr::new::<PyIOError, _>(format!("read failed: {e}")))?;
        let mut out = Vec::with_capacity(recs.len());
        for r in &recs {
            let d = PyDict::new_bound(py);
            for (name, val) in r.iter() {
                d.set_item(name, value_to_py(py, val))?;
            }
            out.push(d.unbind());
        }
        Ok(out)
    }

    /// 某 code 某数值列 -> list[float|None]（空值/NaN 为 None）。
    fn read_column(&self, table: &str, code: &str, field: &str) -> PyResult<Vec<Option<f64>>> {
        let recs = self
            .inner
            .read(table, code)
            .map_err(|e| PyErr::new::<PyIOError, _>(format!("read failed: {e}")))?;
        let idx = crate::layout::field_index(table).and_then(|m| m.get(field).copied());
        let mut out = Vec::with_capacity(recs.len());
        for r in &recs {
            match idx.and_then(|i| r.fields.get(i)) {
                Some(Value::F64(f)) => out.push(Some(*f)),
                Some(Value::I64(x)) => out.push(Some(*x as f64)),
                _ => out.push(None),
            }
        }
        Ok(out)
    }

    /// 按全局交易日索引 t O(1) 取单字段 -> float|None。
    fn read_at(&self, table: &str, code: &str, t: usize, field: &str) -> PyResult<Option<f64>> {
        let rec = self
            .inner
            .read_at(table, code, t)
            .map_err(|e| PyErr::new::<PyIOError, _>(format!("read_at failed: {e}")))?;
        match rec.and_then(|r| r.get(table, field).cloned()) {
            Some(Value::F64(f)) => Ok(Some(f)),
            Some(Value::I64(x)) => Ok(Some(x as f64)),
            _ => Ok(None),
        }
    }

    /// 执行 DSL 查询，返回命中行 JSON 字符串（与 Store::query 同构）。
    fn query(&self, table: &str, expr: &str) -> PyResult<String> {
        self.inner
            .query(table, expr)
            .map_err(|e| PyErr::new::<PyValueError, _>(e))
    }

    /// 执行 DSL 查询，返回命中行原始二进制缓冲（零 JSON）。调用端按 CONTRACT §4 解码。
    fn query_bin(&self, table: &str, expr: &str, py: Python<'_>) -> PyResult<Py<PyBytes>> {
        let buf = self
            .inner
            .query_bin(table, expr)
            .map_err(|e| PyErr::new::<PyValueError, _>(e))?;
        Ok(PyBytes::new_bound(py, &buf).unbind())
    }

    /// 某表字段布局指纹（= CONTRACT schema_hash）。
    fn schema_hash(&self, table: &str) -> u64 {
        crate::layout::schema_hash(table)
    }

    /// 某表某 code 的数据文件是否存在。
    fn exists(&self, table: &str, code: &str) -> bool {
        self.inner.exists(table, code)
    }

    /// 读取稀疏 D5/D6 历史资金流，日期由全局交易日历还原。
    fn read_flow_rows(&self, code: &str, py: Python<'_>) -> PyResult<Vec<Py<PyDict>>> {
        let rows = self
            .inner
            .read_flow(code)
            .map_err(|e| PyErr::new::<PyIOError, _>(format!("read flow failed: {e}")))?;
        let cal = self.inner.calendar();
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let Some(date) = cal.t_to_date(row.t as usize) else {
                continue;
            };
            let d = PyDict::new_bound(py);
            d.set_item("date", date)?;
            d.set_item("source", crate::flow::source_name(row.source))?;
            for (name, value) in [
                ("main_net", row.main_net),
                ("main_pct", row.main_pct),
                ("xl_net", row.xl_net),
                ("xl_pct", row.xl_pct),
                ("r0_net", row.r0_net),
                ("r0_pct", row.r0_pct),
                ("turnover", row.turnover),
                ("vol_ratio", row.vol_ratio),
            ] {
                if !value.is_nan() {
                    d.set_item(name, value)?;
                }
            }
            out.push(d.unbind());
        }
        Ok(out)
    }

    /// 某只股票是否存在稀疏 D5/D6 历史资金流。
    fn flow_exists(&self, code: &str) -> bool {
        self.inner.flow_exists(code)
    }

    /// 全局交易日历长度（= cal_len）。
    fn cal_len(&self) -> usize {
        self.inner.calendar().len()
    }

    /// 写入一组记录（覆盖写，按 `date` 幂等 upsert；缺失日期保留旧值）。
    ///
    /// `rows`: `list[dict]`，每个 dict 键为 §3.1 字段名。数值列支持 int/float，
    /// 缺失键或 None 视为空值（Null/NaN）。`date` 必填（用于计算全局交易日索引 t）。
    /// 缩放整数列（价格/百分比）由内核按 CONTRACT 自动 ×scale 编码，调用方传入真实 f64 即可。
    /// 返回写入后的目标长度 n。
    fn write(
        &self,
        _py: Python<'_>,
        table: &str,
        code: &str,
        rows: &Bound<PyList>,
    ) -> PyResult<usize> {
        let kinds = crate::layout::field_kinds(table)
            .ok_or_else(|| PyValueError::new_err(format!("unknown table: {table}")))?;
        let layout: Arc<[(String, char)]> = kinds
            .iter()
            .map(|(n, k)| (n.clone(), crate::layout::format_char(k)))
            .collect();
        let mut records: Vec<Record> = Vec::with_capacity(rows.len());
        for row_obj in rows.iter() {
            let dict = row_obj
                .downcast::<PyDict>()
                .map_err(|_| PyTypeError::new_err("each row must be a dict"))?;
            let mut fields: Vec<Value> = Vec::with_capacity(kinds.len());
            let mut date = String::new();
            for (name, kind) in &kinds {
                let val = match dict.get_item(name) {
                    Ok(Some(item)) => pyobj_to_value(&item, *kind)?,
                    _ => Value::Null,
                };
                if name == "date" {
                    if let Value::Str(s) = &val {
                        date = s.clone();
                    }
                }
                fields.push(val);
            }
            records.push(Record {
                t: 0,
                date,
                fields,
                layout: layout.clone(),
            });
        }
        self.inner
            .write(table, code, &records, None)
            .map_err(|e| PyErr::new::<PyIOError, _>(format!("write failed: {e}")))
    }

    /// 显式释放底层 mmap（Store drop 亦会释放；保留为对称 API）。
    fn close(&mut self) {
        // Store 无独立 close；drop 即释放。
    }

    // ---- 分时（MinuteStore，独立于列式引擎）----
    /// 写入单只票单日分时块（trends2 形态：现价 + 均价 + 量）。
    /// `bar`: dict，键含 code/date/minutes/opens/highs/lows/closes/volumes/amounts/avgs。
    /// avgs 为分时均价序列（经典分时图第二条线）；缺失则空序列。
    fn write_minute(&self, bar: &Bound<PyDict>) -> PyResult<()> {
        let b = dict_to_minute_bar(bar)?;
        MinuteStore::new(&self.root)
            .write(&b)
            .map_err(|e| PyErr::new::<PyIOError, _>(format!("write_minute failed: {e}")))
    }

    /// 读取单只票单日分时块；缺块返回 None。
    fn read_minute(&self, code: &str, date: &str, py: Python<'_>) -> PyResult<Option<Py<PyDict>>> {
        let bar = MinuteStore::new(&self.root)
            .read(code, date)
            .map_err(|e| PyErr::new::<PyIOError, _>(format!("read_minute failed: {e}")))?;
        Ok(bar.map(|b| minute_bar_to_dict(py, &b)))
    }

    /// 某只票已有分时日期列表（升序）。
    fn minute_dates(&self, code: &str) -> PyResult<Vec<String>> {
        MinuteStore::new(&self.root)
            .dates_of(code)
            .map_err(|e| PyErr::new::<PyIOError, _>(format!("minute_dates failed: {e}")))
    }
}

fn value_to_py(py: Python<'_>, v: &Value) -> PyObject {
    match v {
        Value::F64(f) => {
            if f.is_nan() {
                py.None()
            } else {
                f.into_py(py)
            }
        }
        Value::I64(x) => x.into_py(py),
        Value::Str(s) => s.into_py(py),
        Value::Bool(b) => b.into_py(py),
        Value::Null => py.None(),
    }
}

/// 将 Python 对象按字段类型转为 `Value`：None/缺失 -> Null；数值列接受 int/float。
fn pyobj_to_value(obj: &Bound<'_, pyo3::PyAny>, kind: FieldKind) -> PyResult<Value> {
    if obj.is_none() {
        return Ok(Value::Null);
    }
    match kind {
        FieldKind::Bool => Ok(Value::Bool(obj.is_truthy()?)),
        FieldKind::T => {
            if let Ok(i) = obj.extract::<i64>() {
                Ok(Value::I64(i))
            } else {
                Ok(Value::Null)
            }
        }
        FieldKind::Str(_) => {
            if let Ok(s) = obj.extract::<String>() {
                Ok(Value::Str(s))
            } else {
                Ok(Value::Null)
            }
        }
        FieldKind::F64 | FieldKind::Scaled(_) => {
            if let Ok(f) = obj.extract::<f64>() {
                Ok(Value::F64(f))
            } else {
                Ok(Value::Null)
            }
        }
        FieldKind::Present => Ok(Value::Null),
    }
}

/// 将 Python dict 转为 `MinuteBar`（字段缺失视为空序列）。
fn dict_to_minute_bar(dict: &Bound<'_, PyDict>) -> PyResult<MinuteBar> {
    fn get_vec(dict: &Bound<'_, PyDict>, key: &str) -> PyResult<Vec<f64>> {
        match dict.get_item(key)? {
            Some(v) => {
                let list = v
                    .downcast::<PyList>()
                    .map_err(|_| PyTypeError::new_err(format!("{key} must be a list")))?;
                let mut out = Vec::with_capacity(list.len());
                for it in list.iter() {
                    if it.is_none() {
                        out.push(0.0);
                    } else {
                        out.push(it.extract::<f64>().unwrap_or(0.0));
                    }
                }
                Ok(out)
            }
            None => Ok(Vec::new()),
        }
    }
    let code = dict
        .get_item("code")?
        .and_then(|v| v.extract::<String>().ok())
        .unwrap_or_default();
    let date = dict
        .get_item("date")?
        .and_then(|v| v.extract::<String>().ok())
        .unwrap_or_default();
    Ok(MinuteBar {
        code,
        date,
        minutes: get_vec(dict, "minutes")?,
        opens: get_vec(dict, "opens")?,
        highs: get_vec(dict, "highs")?,
        lows: get_vec(dict, "lows")?,
        closes: get_vec(dict, "closes")?,
        volumes: get_vec(dict, "volumes")?,
        amounts: get_vec(dict, "amounts")?,
        avgs: get_vec(dict, "avgs")?,
    })
}

/// 将 `MinuteBar` 转为 Python dict（与 `dict_to_minute_bar` 对称）。
fn minute_bar_to_dict(py: Python<'_>, b: &MinuteBar) -> Py<PyDict> {
    let d = PyDict::new_bound(py);
    let _ = d.set_item("code", b.code.clone());
    let _ = d.set_item("date", b.date.clone());
    let _ = d.set_item("minutes", b.minutes.clone());
    let _ = d.set_item("opens", b.opens.clone());
    let _ = d.set_item("highs", b.highs.clone());
    let _ = d.set_item("lows", b.lows.clone());
    let _ = d.set_item("closes", b.closes.clone());
    let _ = d.set_item("volumes", b.volumes.clone());
    let _ = d.set_item("amounts", b.amounts.clone());
    let _ = d.set_item("avgs", b.avgs.clone());
    d.unbind()
}

// 注意：真正的模块入口 `#[pymodule] fn stockdb_rs` 放在 crate 根 (lib.rs)，不在此处。
// 原因：cdylib 的导出表只可靠地收纳 crate 根层级的 #[export_name]/#[no_mangle] 符号；
// 嵌套模块里的 PyInit_* 会被 rustc 当作无 Rust 调用方的死代码消除，导致 import 时报
// "does not define module export function (PyInit_stockdb_rs)"。本文件仅承载 StockDB 类
// 与其方法（feature-gated）。
