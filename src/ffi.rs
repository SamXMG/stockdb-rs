//! C ABI 层 —— 跨语言调用边界（零拷贝 / 不依赖中间件）。
//!
//! 设计原则：计算下沉到 Rust（数据本地），任意有 C FFI 的语言（C/C++/Go/Java/
//! Ruby/Node/Python…）通过这一层直接调用，无需 RPC / 序列化 / 进程间通信。
//! 本库只依赖 Rust 标准库与自身二进制格式，不绑定任何宿主语言。
//!
//! 流程：
//!   1. `stockdb_open(root)` -> 句柄（Store 指针）
//!   2. 只读：
//!      - `stockdb_read_column_f64(...)` 把某数值列抽成连续 `f64` 缓冲
//!      - `stockdb_read_at_f64(...)` 按 t O(1) 取单条某数值字段
//!   3. 查询：`stockdb_query(handle, table, expr)` 执行 DSL，返回命中行 JSON
//!      字符串；调用方用 `stockdb_free_str` 释放该字符串。
//!   4. `stockdb_free(handle)` 释放句柄。
//!
//! 该接口与磁盘二进制格式无关，仅依赖内存中表示；跨语言契约由调用方约定
//! 字段顺序（与 `layout::TABLE_FIELDS` 一致）。DSL 语法见 `expr` 模块——
//! 字符串进、JSON 出，与 Rust `Store::query` 完全同构，任何语言都可构造 DSL
//! 并解析返回的 JSON，无需依赖任何特定宿主语言。

use std::ffi::{c_void, CString};
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

/// 声明式查询：在 `table` 上执行 DSL 表达式，返回命中行的 JSON 数组（C 字符串）。
///
/// 字符串进、JSON 出，与 Rust `Store::query` 完全同构：
/// DSL 语法见 `expr` 模块，命中行以 JSON 数组返回（每个元素含 `code`/`t`/各字段）。
/// 调用方**必须**用 [`stockdb_free_str`] 释放返回的指针，否则内存泄漏。
///
/// 失败（未知表 / 表达式语法错 / 句柄或字符串为空）返回 null。
///
/// # Safety
/// `handle` 必须来自 `stockdb_open` 且有效；`table`/`expr` 为有效 NUL 结尾 UTF-8 字符串。
#[no_mangle]
pub unsafe extern "C" fn stockdb_query(
    handle: *mut StoreHandle,
    table: *const c_char,
    expr: *const c_char,
) -> *mut c_char {
    if handle.is_null() || table.is_null() || expr.is_null() {
        return std::ptr::null_mut();
    }
    let h = &*handle;
    let table = match std::ffi::CStr::from_ptr(table).to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let expr = match std::ffi::CStr::from_ptr(expr).to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    match h.0.query(table, expr) {
        Ok(json) => match CString::new(json) {
            Ok(c) => c.into_raw(),
            Err(_) => std::ptr::null_mut(),
        },
        Err(_) => std::ptr::null_mut(),
    }
}

/// 释放 [`stockdb_query`] 返回的 C 字符串（由 `CString::into_raw` 让出所有权）。
///
/// # Safety
/// `p` 必须来自 `stockdb_query` 且未被释放；可传 null（no-op）。
#[no_mangle]
pub unsafe extern "C" fn stockdb_free_str(p: *mut c_char) {
    if !p.is_null() {
        drop(CString::from_raw(p));
    }
}

/// 声明式查询（零 JSON 序列化）：与 [`stockdb_query`] 同构，但返回命中行的
/// **原始二进制**缓冲（调用方按 CONTRACT §2.4 / §4 自行解码）。
///
/// 缓冲区布局（小端）：
/// ```text
/// [0..4]   magic      = 0x53544231 ("STB1")
/// [4..8]   record_len : u32   单行字节数（= CONTRACT §3.4）
/// [8..16]  n_hits     : u64   命中行数
/// [16..24] schema_hash: u64   字段布局指纹（= stockdb_schema_hash(table)）
/// [24..]   n_hits × record_len 字节，每行即 §4 定长 stride 编码（present + 字段）
/// ```
/// `code` / `t` 已编码在行内（分别为首字段 / 第二字段）。
///
/// 返回数据指针；`out_len` / `out_cap` 分别写出缓冲长度 / 容量。
/// 失败（未知表 / 语法错 / 句柄或字符串为 NULL）返回 NULL。
///
/// **所有权（硬约束）**：返回的 `out_len` / `out_cap` 必须原样传回
/// [`stockdb_free_buf`] 释放，否则内存泄漏。
///
/// # Safety
/// `handle` 来自 `stockdb_open` 且有效；`table`/`expr` 为有效 NUL 结尾 UTF-8；
/// `out_len` / `out_cap` 可为 null。
#[no_mangle]
pub unsafe extern "C" fn stockdb_query_bin(
    handle: *mut StoreHandle,
    table: *const c_char,
    expr: *const c_char,
    out_len: *mut usize,
    out_cap: *mut usize,
) -> *mut u8 {
    if handle.is_null() || table.is_null() || expr.is_null() {
        return std::ptr::null_mut();
    }
    let h = &*handle;
    let table = match std::ffi::CStr::from_ptr(table).to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let expr = match std::ffi::CStr::from_ptr(expr).to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let mut v = match h.0.query_bin(table, expr) {
        Ok(v) => v,
        Err(_) => return std::ptr::null_mut(),
    };
    let p = v.as_mut_ptr();
    let len = v.len();
    let cap = v.capacity();
    std::mem::forget(v); // 所有权移交给调用方，由 stockdb_free_buf 回收
    if !out_len.is_null() {
        *out_len = len;
    }
    if !out_cap.is_null() {
        *out_cap = cap;
    }
    p
}

/// 释放 [`stockdb_query_bin`] 返回的原始缓冲。
///
/// 必须传入与该次调用一致的 `len` / `cap`（即 `out_len` / `out_cap` 的回传值）；
/// `p` 可传 null（no-op）。
///
/// # Safety
/// `p` 须来自 `stockdb_query_bin` 且未释放；`len` / `cap` 须为当时返回的精确值。
#[no_mangle]
pub unsafe extern "C" fn stockdb_free_buf(p: *mut u8, len: usize, cap: usize) {
    if !p.is_null() {
        drop(Vec::from_raw_parts(p, len, cap));
    }
}

/// 返回某表的字段布局指纹（确定性，跨运行稳定）。调用端可在解析二进制结果前
/// 与缓冲 header 的 `schema_hash` 比对，确认布局版本一致。未知表返回 0。
///
/// # Safety
/// `table` 为有效 NUL 结尾 UTF-8。
#[no_mangle]
pub unsafe extern "C" fn stockdb_schema_hash(table: *const c_char) -> u64 {
    if table.is_null() {
        return 0;
    }
    let table = match std::ffi::CStr::from_ptr(table).to_str() {
        Ok(s) => s,
        Err(_) => return 0,
    };
    crate::layout::schema_hash(table)
}
