"""stockdb-rs 的 Python ctypes 封装（语言中立 C ABI 的一个消费者示例）。

把 Rust 编出的 cdylib (stockdb_rs.dll / libstockdb_rs.so) 暴露成 Python 类。
同一套 C ABI 符号也可被 C/C++/Go/Java/Ruby/Node 等直接调用。

> 这是「两种 Python 集成方式」之一：ctypes 消费 C ABI（本文件）。
> 另一种是直接 `import stockdb_rs`（pyo3 原生绑定，由 `cargo build --features pyo3`
> 产出，与 C ABI 符号共存于同一 cdylib）；原生绑定无需自行解码字节，优先推荐。
用法::

    from stockdb_rs_ctypes import StockDB
    db = StockDB("/path/to/store")          # 指向含 calendar.json 的根目录
    closes = db.read_column("RawDailyBar", "600000", "close")
    c1 = db.read_at("RawDailyBar", "600000", t=1, field="close")
    hits = db.query("RawDailyBar", "close>10 && ma(close,20)>close")  # DSL -> JSON 字符串
    # 零 JSON 的二进制返回（宽查询/性能关键路径），调用端按 CONTRACT §4 自行解码：
    buf, n_hits, record_len, shash = db.query_bin("RawDailyBar", "close>10")
    rows = db.decode_rows(buf, "RawDailyBar")   # 参考「调用端适配接口」
    db.close()

FFI 覆盖只读 + 查询（read_column / read_at / query / query_bin）。写路径仍在 Rust 侧完成。
"""
import ctypes
import os

__all__ = ["StockDB", "StockDBError"]

# 参考「调用端适配接口」用 layout：(字段名, 类型元字符, 字节宽)。
# 来源 = CONTRACT.md §3（字段类型表）。仅登记常用表作示例；
# 其它表请按 §3 自行提供后调用 decode_rows，或直接按偏移零拷贝解码。
# 价格类列与百分比/比率类列已改为缩放整数 `I`（4 字节 i32，读时 ÷SCALE 还原），见 §3 / §4.2。
_KNOWN_LAYOUTS = {
    "RawDailyBar": [
        ("code", "s", 16), ("t", "q", 8), ("date", "s", 10),
        ("open", "I", 4), ("high", "I", 4), ("low", "I", 4), ("close", "I", 4),
        ("volume", "d", 8), ("amount", "d", 8), ("turnover", "I", 4),
    ],
    "IndexDaily": [
        ("index_code", "s", 16), ("t", "q", 8), ("date", "s", 10),
        ("open", "I", 4), ("high", "I", 4), ("low", "I", 4), ("close", "I", 4),
        ("volume", "d", 8), ("amount", "d", 8),
    ],
    "FundFlow": [
        ("code", "s", 16), ("t", "q", 8), ("date", "s", 10),
        ("main_net", "d", 8), ("main_pct", "I", 4), ("xl_net", "d", 8),
        ("xl_pct", "I", 4), ("l_net", "d", 8), ("l_pct", "I", 4),
        ("mid_net", "d", 8), ("mid_pct", "I", 4), ("small_net", "d", 8),
        ("small_pct", "I", 4),
    ],
    "DailySnapshot": [
        ("code", "s", 16), ("date", "s", 10), ("t", "q", 8), ("name", "s", 32),
        ("board", "s", 16), ("is_st", "?", 1), ("price", "I", 4),
        ("prev_close", "I", 4), ("chg_pct", "I", 4), ("vol_ratio", "I", 4),
        ("turnover", "I", 4), ("market_cap_yi", "d", 8), ("float_cap_yi", "d", 8),
        ("pe", "I", 4), ("pb", "I", 4), ("chg60", "I", 4), ("flow_main", "d", 8),
        ("flow_main_pct", "I", 4), ("flow_xl", "d", 8), ("flow_xl_pct", "I", 4),
        ("flow_l", "d", 8), ("flow_l_pct", "I", 4), ("industry", "s", 24),
        ("concepts", "s", 192),
    ],
}

# 缩放整数列：磁盘 i32（×scale 写入），读时 ÷scale 还原。与 Rust `layout::SCALED` 一致。
SCALE = {
    # 价格类（2 位小数 → ×100）
    "open": 100.0, "high": 100.0, "low": 100.0, "close": 100.0,
    "prev_close": 100.0, "price": 100.0,
    # 百分比/比率类（4 位小数 → ×10000）
    "turnover": 10000.0, "chg_pct": 10000.0, "vol_ratio": 10000.0,
    "chg60": 10000.0, "pe": 10000.0, "pb": 10000.0,
    "flow_main_pct": 10000.0, "flow_xl_pct": 10000.0, "flow_l_pct": 10000.0,
    "main_pct": 10000.0, "xl_pct": 10000.0, "l_pct": 10000.0,
    "mid_pct": 10000.0, "small_pct": 10000.0,
}
_SCALED_NULL = -(2 ** 31)  # i32::MIN，与 Rust `layout::SCALED_NULL` 一致


def table_layout(table):
    """返回 CONTRACT §3 登记的 (name, kind, width) 序列；未登记则报错引导查阅契约。"""
    try:
        return _KNOWN_LAYOUTS[table]
    except KeyError:
        raise StockDBError(
            f"未登记 {table} 的 layout；请按 CONTRACT §3 提供 (name, kind, width) 序列后解码"
        )


class StockDBError(RuntimeError):
    pass


class StockDB:
    _lib = None

    @classmethod
    def _load(cls):
        if cls._lib is None:
            here = os.path.dirname(os.path.abspath(__file__))
            # 默认到 crate 的 target/release 找 cdylib；可用 STOCKDB_RS_DLL 覆盖。
            dll = os.environ.get("STOCKDB_RS_DLL") or os.path.join(
                here, "..", "target", "release", "stockdb_rs"
            )
            if os.name == "nt":
                dll += ".dll"
            else:
                dll += ".so"
            dll = os.path.abspath(dll)
            if not os.path.exists(dll):
                raise StockDBError(f"找不到 cdylib: {dll}")
            lib = ctypes.CDLL(dll)
            lib.stockdb_open.argtypes = [ctypes.c_char_p]
            lib.stockdb_open.restype = ctypes.c_void_p
            lib.stockdb_free.argtypes = [ctypes.c_void_p]
            lib.stockdb_free.restype = None
            lib.stockdb_read_column_f64.argtypes = [
                ctypes.c_void_p,
                ctypes.c_char_p,
                ctypes.c_char_p,
                ctypes.c_char_p,
                ctypes.POINTER(ctypes.c_double),
                ctypes.c_size_t,
            ]
            lib.stockdb_read_column_f64.restype = ctypes.c_int
            lib.stockdb_read_at_f64.argtypes = [
                ctypes.c_void_p,
                ctypes.c_char_p,
                ctypes.c_char_p,
                ctypes.c_size_t,
                ctypes.c_char_p,
                ctypes.POINTER(ctypes.c_double),
            ]
            lib.stockdb_read_at_f64.restype = ctypes.c_int
            lib.stockdb_query.argtypes = [
                ctypes.c_void_p,
                ctypes.c_char_p,
                ctypes.c_char_p,
            ]
            lib.stockdb_query.restype = ctypes.c_void_p  # *mut c_char
            lib.stockdb_free_str.argtypes = [ctypes.c_void_p]
            lib.stockdb_free_str.restype = None
            lib.stockdb_query_bin.argtypes = [
                ctypes.c_void_p,
                ctypes.c_char_p,
                ctypes.c_char_p,
                ctypes.POINTER(ctypes.c_size_t),
                ctypes.POINTER(ctypes.c_size_t),
            ]
            lib.stockdb_query_bin.restype = ctypes.c_void_p  # *mut u8
            lib.stockdb_free_buf.argtypes = [
                ctypes.c_void_p,
                ctypes.c_size_t,
                ctypes.c_size_t,
            ]
            lib.stockdb_free_buf.restype = None
            lib.stockdb_schema_hash.argtypes = [ctypes.c_char_p]
            lib.stockdb_schema_hash.restype = ctypes.c_uint64
            cls._lib = lib
        return cls._lib

    def __init__(self, root):
        lib = self._load()
        self._h = lib.stockdb_open(root.encode("utf-8"))
        if not self._h:
            raise StockDBError(f"stockdb_open 失败: {root}")

    def read_column(self, table, code, field, cap=1_000_000):
        """读取某数值列为 Python list[float]，空值以 float('nan') 占位。"""
        lib = self._load()
        buf = (ctypes.c_double * cap)()
        n = lib.stockdb_read_column_f64(
            self._h,
            table.encode("utf-8"),
            code.encode("utf-8"),
            field.encode("utf-8"),
            buf,
            cap,
        )
        if n < 0:
            raise StockDBError(f"read_column_f64 失败: {table}/{code}/{field}")
        return [buf[i] for i in range(n)]

    def read_at(self, table, code, t, field):
        """按全局交易日索引 t O(1) 随机读单字段。"""
        lib = self._load()
        out = ctypes.c_double(0.0)
        r = lib.stockdb_read_at_f64(
            self._h,
            table.encode("utf-8"),
            code.encode("utf-8"),
            t,
            field.encode("utf-8"),
            ctypes.byref(out),
        )
        if r != 0:
            raise StockDBError(f"read_at_f64 失败: {table}/{code}/t={t}/{field}")
        return out.value

    def query(self, table, expr):
        """执行 DSL 查询，返回命中行的 JSON 数组字符串（与 Rust ``Store::query`` 同构）。

        命中行元素含 ``code`` / ``t`` / 各字段；可用 ``json.loads`` 解析。
        例: ``db.query("RawDailyBar", "close>10 && ma(close,20)>close")``
        """
        lib = self._load()
        ptr = lib.stockdb_query(
            self._h,
            table.encode("utf-8"),
            expr.encode("utf-8"),
        )
        if not ptr:
            raise StockDBError(f"stockdb_query 失败: {table}/{expr}")
        raw = ctypes.cast(ptr, ctypes.c_char_p).value  # bytes，赋值即拷出，随后释放
        lib.stockdb_free_str(ptr)
        return raw.decode("utf-8")

    def query_bin(self, table, expr):
        """执行 DSL 查询，返回原始二进制结果 ``(buf, n_hits, record_len, schema_hash)``。

        零 JSON 序列化；``buf`` 前 24 字节为 header（magic/record_len/n_hits/schema_hash），
        其后为 ``n_hits × record_len`` 的定长 stride 行，调用端按 CONTRACT §4 自行解码
        （参考 ``decode_rows``）。结果缓冲已在本函数内拷出并释放，无泄漏。
        """
        lib = self._load()
        out_len = ctypes.c_size_t(0)
        out_cap = ctypes.c_size_t(0)
        ptr = lib.stockdb_query_bin(
            self._h,
            table.encode("utf-8"),
            expr.encode("utf-8"),
            ctypes.byref(out_len),
            ctypes.byref(out_cap),
        )
        if not ptr:
            raise StockDBError(f"stockdb_query_bin 失败: {table}/{expr}")
        n = out_len.value
        buf = ctypes.string_at(ptr, n)  # 拷出后释放堆缓冲
        lib.stockdb_free_buf(ptr, out_len.value, out_cap.value)
        record_len = int.from_bytes(buf[4:8], "little")
        n_hits = int.from_bytes(buf[8:16], "little")
        schema_hash = int.from_bytes(buf[16:24], "little")
        return buf, n_hits, record_len, schema_hash

    def schema_hash(self, table):
        """返回某表字段布局指纹（与二进制结果 header 的 schema_hash 比对用）。"""
        return self._load().stockdb_schema_hash(table.encode("utf-8"))

    def decode_rows(self, buf, table):
        """参考「调用端适配接口」：按 CONTRACT §4 解码 ``query_bin`` 结果 -> ``list[dict|None]``。

        仅支持 CONTRACT §3 已登记 layout 的表（见 ``table_layout``）；其它表请按其 §3 自行
        提供 ``(name, kind, width)`` 序列后调用，或直接按偏移零拷贝解码。
        """
        import struct

        layout = table_layout(table)
        record_len = int.from_bytes(buf[4:8], "little")
        n_hits = int.from_bytes(buf[8:16], "little")
        schema_hash = int.from_bytes(buf[16:24], "little")
        if schema_hash != self.schema_hash(table):
            raise StockDBError(
                f"schema_hash 不匹配: 结果={schema_hash:#x} 本地={self.schema_hash(table):#x}"
                "，布局版本不一致"
            )
        rows = []
        off = 24
        for _ in range(n_hits):
            if not buf[off]:  # 空槽 present=0
                off += record_len
                rows.append(None)
                continue
            off += 1
            rec = {}
            for (name, kind, width) in layout:
                if kind in ("b", "?"):  # bool（CONTRACT 元字符 `?`）
                    rec[name] = bool(buf[off])
                    off += 1
                elif kind == "s":
                    raw = buf[off : off + width]
                    end = raw.find(b"\x00")
                    rec[name] = raw[: end if end != -1 else width].decode(
                        "utf-8", "ignore"
                    ).strip()
                    off += width
                elif kind == "q":
                    rec[name] = struct.unpack_from("<q", buf, off)[0]
                    off += 8
                elif kind == "I":  # 缩放整数 i32（4 字节），÷scale 还原；哨兵=None
                    raw = struct.unpack_from("<i", buf, off)[0]
                    off += 4
                    rec[name] = None if raw == _SCALED_NULL else raw / SCALE.get(name, 1.0)
                else:  # d
                    v = struct.unpack_from("<d", buf, off)[0]
                    rec[name] = None if v != v else v  # NaN 检查
                    off += 8
            rows.append(rec)
        return rows

    def close(self):
        h = getattr(self, "_h", None)
        if h:
            self._load().stockdb_free(h)
            self._h = None

    def __del__(self):
        self.close()
