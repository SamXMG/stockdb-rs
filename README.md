# stockdb-rs

A 股列式存储数据库的 **Rust 实现**：定长二进制 `.dat` + 全局交易日历 `t` 对齐 + mmap 零拷贝随机读。
与 Python 版 [`stockdb`](https://github.com/your-org/Screener) 二进制布局 **严格 1:1 兼容**，可互相读写同一份 `.dat` 文件。

> 定位：**纯存储库**。只负责列式存储、读写、`repack`、`.meta`，以及作为「数据库视图」的复权 / 周期聚合能力。
> 不负责网络采集、因子计算、选股策略等上层业务逻辑。

## 特性

- **定长二进制布局**：每条记录 `<?` present 标记 + 字段序列（bool `?` / str `{w}s` 定宽 / `t` 为 `q` i64 全局交易日索引 / 数值 `d` f64，NaN 表示空值）。
- **全局交易日历对齐**：所有表共用同一根 `calendar.json`，记录按 `t`（交易日索引）对齐，支持 O(1) 随机读 `read_at`。
- **零拷贝读**：`read_mmap` 基于 `memmap2` 直接映射文件。
- **对称读写**：`write` / `repack` / `write_meta` 与读路径字节级对称。
- **数据库视图（VIEW）**：复权（前 / 后复权）、回测专用严格前视隔离前复权、周 / 月 K 聚合——确定性派生，不产生 IO。
- **兼容性保证**：所有读写 / 视图逻辑均有「Rust 输出 vs Python 读回」的字节级对齐测试，作为跨语言契约的回归保护。

## 安装

```toml
[dependencies]
stockdb-rs = "0.1"
```

要求 Rust 1.74+。

## 快速开始

### 打开存储、读取

```rust
use stockdb_rs::Store;

let store = Store::open("/path/to/stockdb_root").unwrap();

// 读某只票全部非空记录（按 t 升序）
let bars = store.read("RawDailyBar", "600000").unwrap();
for r in &bars {
    println!("t={} close={:?}", r.t, r.fields.get("close"));
}

// O(1) 按交易日索引取单条
let rec = store.read_at("RawDailyBar", "600000", 42).unwrap();
```

### 写入 / 重排 / 写元数据

```rust
use stockdb_rs::{Store, Value};

let store = Store::open("/path/to/stockdb_root").unwrap();
let records = store.read("RawDailyBar", "600000").unwrap();

// 覆盖写回（target_n 可选，缺省取 max(t)+1）
store.write("RawDailyBar", "600000", &records, None).unwrap();
// 统一行数到 801（缺槽 present=0）
store.repack("RawDailyBar", "600000", 801).unwrap();
// 写 .meta（cal_len / cal_hash / table）
store.write_meta("RawDailyBar", "600000").unwrap();
```

### 数据库视图：复权 / 周期聚合

```rust
use stockdb_rs::view::{derive_qfq, aggregate_period, AdjustEvent, RawBar};

let bars: Vec<RawBar> = /* 从 RawDailyBar 读出 */ vec![];
let events: Vec<AdjustEvent> = /* 从 AdjustEvent 读出 */ vec![];

let qfq = derive_qfq(&bars, &events);          // 前复权日 K
let weekly = aggregate_period(&bars, "week", Some(&events)); // 周 K（先 qfq 再聚合）
```

## 二进制布局契约

每条记录（小端 `<`）：

```
[ present: u8=1 ][ field_0 ][ field_1 ] ... [ field_k ]
```

字段类型映射（与 Python `stockdb.schema._TABLE_FIELDS` / `_BOOL_FIELDS` / `_STR_W` 一致）：

| 类型            | struct | 字节 | 说明                      |
| --------------- | ------ | ---- | ------------------------- |
| `present` 标记  | `?`    | 1    | 1=有数据, 0=空槽          |
| bool            | `?`    | 1    |                           |
| 字符串          | `{w}s` | `w`  | utf-8 定宽, 右截断+\x00 补齐 |
| `t`（交易日索引）| `q`   | 8    | i64                       |
| 数值            | `d`    | 8    | f64, NaN = 空值           |

文件长度 = `cal_n × rbytes`，`cal_n` 为交易日历长度。

各表字段序列见 `src/layout.rs` 的 `TABLE_FIELDS` / `BOOL_FIELDS` / `STR_W`。

## `.meta` 格式

JSON：`{ "cal_len": usize, "cal_hash": str, "table": str }`。
`cal_hash = md5("{first}|{last}|{len}")[:12]`，用于校验数据与日历一致。

## 兼容性测试

所有读写 / 视图逻辑均有「Rust 输出 vs Python 读回」的字节级对齐测试，作为跨语言契约的回归保护。
基准数据 `testdata/` 由 `tests/gen_testdata.py`（调用本地 `Screener/stockdb` 的 `engine`）落盘，
覆盖全部 8 张列式表（RawDailyBar / FundFlow / AdjustEvent / IndexDaily / CompanyProfile / Announcement / RenameEvent / DailySnapshot）。

- `tests/align_with_python.rs` — 读路径逐表字段级对齐（8 表）
- `tests/view_align.rs` — 视图数值对齐（qfq / hfq / weekly / monthly）
- `tests/write_align.rs` — 写路径对齐（Rust 写 → Python 读回一致；repack；.meta）

```bash
# 1) (可选) 重新生成基准数据
python3 tests/gen_testdata.py testdata /path/to/Screener
# 2) 运行全部对齐测试
cargo test
```

> 测试依赖 `python3` 与本地 `Screener/stockdb`，路径硬编码于测试脚本中（仅测试用，不影响库本身）。

## 项目结构

```
src/
  lib.rs        Store: open/read/read_mmap/read_at/write/repack/write_meta
  layout.rs     二进制编码契约 + decode/encode（与 Python 1:1）
  calendar.rs   交易日历加载 + hash 指纹
  view.rs       数据库视图：复权 / 周期聚合
tests/          与 Python 的跨语言对齐测试
```

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your option.
