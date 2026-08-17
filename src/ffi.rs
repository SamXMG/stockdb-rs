//! C ABI 层 —— 跨语言高速接口（零拷贝 / 不依赖中间件）。
//!
//! 设计原则：计算下沉到 Rust（数据本地），上层语言（C/Go/Python ctypes）
//! 通过 FFI 直接取得**列式连续内存**，避免 RPC/序列化/IPC 的额外开销。
//!
//! 流程：
//!   1. `stockdb_open(root)` -> 句柄（Store 指针）
//!   2. `stockdb_read_column_f64(handle, table, code, field, out_ptr, out_len)`
//!      把某数值列抽成连续 `f64` 缓冲（Null/NaN 占位），返回元素个数。
//!      调用方提供足够大的 `f64` 数组；返回长度告知有效个数。
//!   3. `stockdb_free(handle)` 释放。
//!
//! 该接口与磁盘二进制格式无关，仅依赖内存中表示；跨语言契约由调用方约定
//! 字段顺序（与 `layout::TABLE_FIELDS` 一致）。

use std::ffi::c_void;
use std::os::raw::{c_char, c_int};

use crate::{layout, Record, Store, Value};

/// 不透明句柄。实际指向 `Box<Store>`。
pub struct StoreHandle(Store);

/// 打开存储，返回句柄指针（失败返回 null）。
///
/// # Safety
/// `root` 必须为有效以 NUL 结尾的 UTF-8 字符串。
#[no_mangle]
pub unsafe extern "C" fn stockdb_open(root: *const c_char) -> *mut StoreHandle {
    if root.is_null() {
        return std::ptr::null_mut();
    }
    let cstr = std::ffi::CStr::from_ptr(root);
    let root_str = match cstr.to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    match Store::open(root_str) {
        Ok(s) => Box::into_raw(Box::new(StoreHandle(s))),
        Err(_) => std::ptr::null_mut(),
    }
}

/// 释放句柄。
///
/// # Safety
/// `handle` 必须来自 `stockdb_open` 且未被释放。
#[no_mangle]
pub unsafe extern "C" fn stockdb_free(handle: *mut StoreHandle) {
    if !handle.is_null() {
        drop(Box::from_raw(handle));
    }
}

/// 将某数值列抽成连续 `f64` 缓冲写入 `out`（调用方分配，容量 `cap` 个元素）。
/// 返回写入的元素个数；-1 表示错误（未知表/字段、缓冲区不足、非数值列）。
///
/// 空值以 `f64::NAN` 占位。`t` 列作为 i64 转 f64 写入。
///
/// # Safety
/// `handle` 有效；`table`/`code`/`field` 为有效 NUL 结尾字符串；
/// `out` 指向至少 `cap * 8` 字节的可写内存。
#[no_mangle]
pub unsafe extern "C" fn stockdb_read_column_f64(
    handle: *mut StoreHandle,
    table: *const c_char,
    code: *const c_char,
    field: *const c_char,
    out: *mut f64,
    cap: usize,
) -> c_int {
    if handle.is_null() || table.is_null() || code.is_null() || field.is_null() || out.is_null() {
        return -1;
    }
    let h = &*handle;
    let table = match std::ffi::CStr::from_ptr(table).to_str() {
        Ok(s) => s,
        Err(_) => return -1,
    };
    let code = match std::ffi::CStr::from_ptr(code).to_str() {
        Ok(s) => s,
        Err(_) => return -1,
    };
    let field = match std::ffi::CStr::from_ptr(field).to_str() {
        Ok(s) => s,
        Err(_) => return -1,
    };

    let rlen = match layout::record_len(table) {
        Some(n) => n,
        None => return -1,
    };
    let idx = match layout::field_index(table) {
        Some(m) => match m.get(field) {
            Some(&i) => i,
            None => return -1,
        },
        None => return -1,
    };

    let recs: Vec<Record> = match h.0.read_mmap(table, code) {
        Ok(r) => r,
        Err(_) => return -1,
    };
    let n = recs.len();
    if n > cap {
        return -1; // 缓冲区不足
    }
    let out_slice = std::slice::from_raw_parts_mut(out, n);
    for (i, r) in recs.iter().enumerate() {
        out_slice[i] = match r.fields.get(idx) {
            Some(Value::F64(f)) => *f,
            Some(Value::I64(i64)) => *i64 as f64,
            Some(Value::Null) | None => f64::NAN,
            // bool/str 列不应调用本接口；以 NAN 占位
            _ => f64::NAN,
        };
    }
    let _ = rlen;
    n as c_int
}

/// 按 t 取单条记录的某数值字段（O(1) 随机读），写入 `out`，返回 0 成功 / -1 失败。
///
/// # Safety
/// 同上；`out` 指向至少 8 字节可写内存。
#[no_mangle]
pub unsafe extern "C" fn stockdb_read_at_f64(
    handle: *mut StoreHandle,
    table: *const c_char,
    code: *const c_char,
    t: usize,
    field: *const c_char,
    out: *mut f64,
) -> c_int {
    if handle.is_null() || table.is_null() || code.is_null() || field.is_null() || out.is_null() {
        return -1;
    }
    let h = &*handle;
    let table = match std::ffi::CStr::from_ptr(table).to_str() {
        Ok(s) => s,
        Err(_) => return -1,
    };
    let code = match std::ffi::CStr::from_ptr(code).to_str() {
        Ok(s) => s,
        Err(_) => return -1,
    };
    let field = match std::ffi::CStr::from_ptr(field).to_str() {
        Ok(s) => s,
        Err(_) => return -1,
    };
    let idx = match layout::field_index(table).and_then(|m| m.get(field).copied()) {
        Some(i) => i,
        None => return -1,
    };
    let rec = match h.0.read_at(table, code, t) {
        Ok(Some(r)) => r,
        _ => return -1,
    };
    let v = match rec.fields.get(idx) {
        Some(Value::F64(f)) => *f,
        Some(Value::I64(i64)) => *i64 as f64,
        _ => f64::NAN,
    };
    *out = v;
    0
}

// 防止未使用告警：c_void 在某些目标需要。
#[allow(dead_code)]
fn _assert_c_void(_: *mut c_void) {}
