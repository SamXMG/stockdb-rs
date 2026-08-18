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

use pyo3::exceptions::{PyIOError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyDictMethods};

use crate::{Store, Value};

#[pyclass]
pub struct StockDB {
    inner: Store,
}

#[pymethods]
impl StockDB {
    #[new]
    fn new(root: &str) -> PyResult<Self> {
        Store::open(root)
            .map(|s| StockDB { inner: s })
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

    /// 全局交易日历长度（= cal_len）。
    fn cal_len(&self) -> usize {
        self.inner.calendar().len()
    }

    /// 显式释放底层 mmap（Store drop 亦会释放；保留为对称 API）。
    fn close(&mut self) {
        // Store 无独立 close；drop 即释放。
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

// 注意：真正的模块入口 `#[pymodule] fn stockdb_rs` 放在 crate 根 (lib.rs)，不在此处。
// 原因：cdylib 的导出表只可靠地收纳 crate 根层级的 #[export_name]/#[no_mangle] 符号；
// 嵌套模块里的 PyInit_* 会被 rustc 当作无 Rust 调用方的死代码消除，导致 import 时报
// "does not define module export function (PyInit_stockdb_rs)"。本文件仅承载 StockDB 类
// 与其方法（feature-gated）。
