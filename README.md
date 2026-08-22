# stockdb-rs

> AI/自动化代理先读 [`AI_GUIDE.md`](AI_GUIDE.md)；项目级本地优先开发规则见 `../screener_cn/docs/架构/StockDB数据库与本地优先开发指南.md`。

A 股列式存储数据库的 **Rust 实现（语言中立，无宿主语言绑定）**：定长二进制 `.dat` + 全局交易日历 `t` 对齐 + mmap 零拷贝随机读。
二进制布局为**语言中立契约**——最初以 Python 版 [`stockdb`](https://github.com/your-org/Screener) 为参考实现，二者严格 1:1 兼容，可互相读写同一份 `.dat` 文件；任何语言只要按此契约编码即可直接读写。

> 定位：**纯存储库（语言中立）**。只负责列式存储、读写、`repack`、`.meta`，以及作为「数据库视图」的复权 / 周期聚合能力。对外暴露 Rust crate 与 C ABI 两层接口，任意语言均可通过 C ABI 调用。
> 不负责网络采集、因子计算、选股策略等上层业务逻辑。

## 特性

- **定长二进制布局**：每条记录 `<?` present 标记 + 字段序列（bool `?` / str `{w}s` 定宽 / `t` 为 `q` i64 全局交易日索引 / 数值 `d` f64，NaN 表示空值）。
- **全局交易日历对齐**：所有表共用同一根 `calendar.json`，记录按 `t`（交易日索引）对齐，支持 O(1) 随机读 `read_at`。
- **零拷贝读**：`read_mmap` 基于 `memmap2` 直接映射文件。
- **对称读写**：`write` / `repack` / `write_meta` 与读路径字节级对称。
- **并发写安全**：写路径（`write` / `repack` / `write_meta` / `save_calendar`）在目标文件的 sidecar `.lock` **咨询锁**保护下，走 `temp + fsync + 原子 rename` 写入——杜绝两个 writer 交错覆盖导致的数据损坏 / 丢失，且进程写中途崩溃不会留下半截文件。读路径保持无锁，依靠原子 rename 保证 reader 不会读到撕裂数据（最终一致）。**不改变磁盘格式、不破坏字节级兼容**。
- **数据库视图（VIEW）**：复权（前 / 后复权）、回测专用严格前视隔离前复权、周 / 月 K 聚合——确定性派生，不产生 IO。
- **兼容性保证**：所有读写 / 视图逻辑均有「Rust 输出 vs 参考引擎（原 Python）读回」的字节级对齐测试，作为跨语言契约的回归保护。

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

字段类型映射（与参考实现 Python `stockdb.schema._TABLE_FIELDS` 等一致）：

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

所有读写 / 视图逻辑均有「Rust 输出 vs 参考引擎（原 Python）读回」的字节级对齐测试，作为跨语言契约的回归保护。
基准数据 `testdata/` 由 `tests/gen_testdata.py`（调用本地 `Screener/stockdb` 的 `engine`）落盘，
覆盖核心列式表（RawDailyBar / FundFlow / AdjustEvent / IndexDaily / CompanyProfile / Announcement / RenameEvent / DailySnapshot / IndustryDaily），以及回测缓存表 FactorDaily / LabelDaily / SignalDaily。

- `tests/align_with_python.rs` — 读路径逐表字段级对齐（历史 8 表）
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
  layout.rs     二进制编码契约 + decode/encode（语言中立，参考 Python）
  calendar.rs   交易日历加载 + hash 指纹
  view.rs       数据库视图：复权 / 周期聚合
  minute.rs     MinuteBar 分时块（独立 JSON 格式, 语言中立契约）
tests/          跨语言对齐测试（参考实现为 Python）
```

### 两张存储体系

- **列式定长 `.dat`**（`ColumnStore` / `Store`）：RawDailyBar / FundFlow / AdjustEvent /
  IndexDaily / CompanyProfile / Announcement / RenameEvent / DailySnapshot / IndustryDaily /
  FactorDaily / LabelDaily / SignalDaily
  均与参考实现（原 Python）字节级对齐。
- **分时 JSON 块**（`MinuteStore` / `minute::MinuteStore`）：每个 `(code, date)` 一块，存于
  `root/minute/{code}/{date}.min`，字段与参考实现（Python `schema.MinuteBar`）一致。

## 跨语言调用（C ABI 动态库）

`stockdb-rs` 编译出的 cdylib 是**语言中立**的 C ABI 边界：任意有 C FFI 的语言
（C/C++/Go/Java/Ruby/Node/Python…）均可直接加载调用，无需 RPC / 序列化 / 进程间通信。
Rust 侧负责全部计算（数据本地、零拷贝），宿主语言只传字符串、收回结果。

> **权威契约文档**：[`CONTRACT.md`](./CONTRACT.md) —— 跨语言 binding 作者的唯一依据。
> **查询语法速查**：[`QUERY-SYNTAX.md`](./QUERY-SYNTAX.md) —— 所有符号的逐一用法、示例与常见错误。
> 涵盖 C ABI 函数原型与内存所有权、DSL 语法与窗口函数语义、查询返回的 JSON schema、
> 字段类型表（数据模型）、磁盘字节布局，以及兼容性与版本约定。下文为该契约的速览。

```bash
# 编译出动态库（同时产出 rlib 供测试 + cdylib 供任意语言加载）
cargo build --release
# 产物: target/release/stockdb_rs.dll (Windows) / libstockdb_rs.so (Linux)
```

### C ABI 契约

| 函数 | 说明 |
|------|------|
| `stockdb_open(root)` | 打开根目录，返回句柄指针（失败 null） |
| `stockdb_read_column_f64(handle, table, code, field, out, cap)` | 某数值列抽成连续 `f64` 缓冲，返回元素数（-1 错） |
| `stockdb_read_at_f64(handle, table, code, t, field, out)` | 按 t O(1) 取单条某数值字段（0 成功 / -1 失败） |
| `stockdb_query(handle, table, expr)` | 执行 DSL，返回命中行 JSON 字符串（**调用方须用 `stockdb_free_str` 释放**） |
| `stockdb_free_str(p)` | 释放 `stockdb_query` 返回的字符串（可传 null） |
| `stockdb_free(handle)` | 释放句柄 |

`stockdb_query` 字符串进、JSON 出，与 Rust `Store::query` 完全同构；DSL 语法见 `expr`
模块，返回的 JSON 数组每个元素含 `code` / `t` / 各字段，宿主语言自行解析。

**性能路径**：宽查询 / 性能关键场景改用 `stockdb_query_bin` —— 返回命中行的**原始二进制**
缓冲（零 JSON 序列化、类型保真、体积更小），调用端按 [CONTRACT.md](./CONTRACT.md) §2.4 / §4
自行解码。返回的缓冲须用 `stockdb_free_buf` 释放；可用 `stockdb_schema_hash` 做 schema 版本护栏。

> ### 性能建议（查询路径选型）
> - **热路径 / 宽查询 / 回测全市场扫描 → 一律走 `query_bin`（`stockdb_query_bin`）**。
>   命中后仅 memcpy 定长原行字节，零 `decode_row`、零 `serde_json` 物化；
>   实测相对 JSON 路径：宽查询 ~32×、窗口谓词 ~21×、选择性查询 ~8×。
> - **`query`（`stockdb_query`，JSON 出）→ 仅作便利 / 调试 / 外部快速接入**。
>   其耗时瓶颈在「命中行逐行 `decode_row` + `serde_json` 序列化」，与 eval 内核无关，
>   属 JSON 返回方式的固有成本，**不可通过优化 eval 消除**。需要在 JSON 路径再榨性能时，
>   才考虑绕过 `serde_json::Map`/`Value` 中间表示、直接从字节拼 JSON（边际收益 ~1.5–2.5×）。
> - 工程取舍：性能关键路径让调用端吃 `query_bin` 二进制、自行按 §4 解码；
>   JSON 留给需要人类可读 / 临时排查的场景。

### 示例：Python ctypes（仅示其一，Go/Java/C 等同签名调用）

```python
import sys
sys.path.insert(0, "python")
from stockdb_rs import StockDB   # ctypes 薄壳（已封装 stockdb_query / stockdb_free_str）

db = StockDB("/path/to/store")            # 指向含 calendar.json 的根目录
closes = db.read_column("RawDailyBar", "600000", "close")   # list[float]，NaN 占位空值
c1     = db.read_at("RawDailyBar", "600000", t=1, field="close")  # O(1) 随机读
hits   = db.query("RawDailyBar", "close>10 && ma(close,20)>close")  # DSL -> JSON 字符串
db.close()
```

- 封装源码：`python/stockdb_rs.py`（ctypes 薄壳，`StockDB` 类，含 `read_column`/`read_at`/`query`）。
- 端到端自检：先 `cargo run --example make_fixture` 造 `fixture/`，再 `cd python && python _selftest.py`。
- FFI 覆盖**只读 + 查询**（`read_column` / `read_at` / `query`）；写路径仍在 Rust 侧完成，保证磁盘格式字节级兼容不被破坏。
- 若 cdylib 不在默认路径，可用环境变量 `STOCKDB_RS_DLL` 指定绝对路径。

### 可选：PyO3 原生扩展（仅 Python 用户的可选便利层）

若想获得更地道的 Python 体验（`import stockdb_rs` 直接拿到原生对象、类型友好），
可改用 [PyO3](https://pyo3.rs/) + [maturin](https://github.com/PyO3/maturin) 生成
`.pyd` / `.so` 扩展模块。这是**可选的 Python 专属便利层**，与上面语言中立的 C ABI
互不排斥；FFI 层的 `extern "C"` 函数可逐步迁移为 `#[pyfunction]` / `#[pymethods]`。

## 稀疏 D5/D6 历史资金流

近期资金流缓存不适合写入按完整交易日历展开的 `FundFlow` 表。项目额外提供
`MoneyFlowHistory/<code>.flow` 稀疏定长格式：只保存实际存在的交易日，日期通过
根目录 `calendar.json` 的全局 `t` 还原。Python 可通过 PyO3
`StockDB.read_flow_rows(code)` 读取；没有原生扩展时，上层也可以按公开文件头解码。

```bash
# 从 RawDailyBar 日期重建全局日历，并修复各股票物理槽位
cargo run --release --bin rebuild_calendar -- /path/to/stockdb/root

# 将 data/d5d6/*.json 迁移到 MoneyFlowHistory（原 JSON 不删除）
cargo run --release --bin migrate_d5d6 -- /path/to/stockdb/root /path/to/data/d5d6
```

### Rust 原生 JSON 导入

大体量 JSON 的解析、schema 映射、日历对齐和原子二进制落盘必须由 Rust 完成，
Python 采集端不得自行编码 `.dat`/`.flow`。统一入口：

```bash
cargo run --release --bin import_json -- snapshot <root> <snapshot-json-dir>
cargo run --release --bin import_json -- company-profile <root> <snapshot-json-dir> <industry-history.json>
cargo run --release --bin import_json -- money-flow <root> <d5d6-json-dir>
cargo run --release --bin import_json -- cache <root> FactorDaily <features.jsonl>
cargo run --release --bin import_json -- cache <root> LabelDaily <labels.jsonl>
cargo run --release --bin import_json -- cache <root> SignalDaily <signals.jsonl>
cargo run --release --bin import_json -- compact <root> factor <wide-factor.jsonl> <factor-1,factor-2,...>
cargo run --release --bin import_json -- compact <root> label <wide-label.jsonl> <label-1,label-2,...>
```

`Store::write` 对静态表不触碰交易日历；日历表只允许已存在日期或向尾部追加，
禁止隐式中间插入导致全库 `t` 错位。原始 JSON 不会删除。

回测缓存 JSONL 必须按 `维度ID + code` 分组排序。Rust 会流式解析并写入：
`FactorDaily/<factor_id>__<code>.dat`、`LabelDaily/<label_id>__<code>.dat`、
`SignalDaily/<strategy_id>__<model_version>__<code>.dat`。维度 ID 只存于文件名，
不在每行重复保存；每个动态因子/标签/策略独立文件，新增因子不会覆盖其他因子。

大规模研究缓存推荐使用 `compact`：每只股票一个 `.mtx` 文件，文件头只存一次列名，
数据行仅为 `t(u32)+f32[]`，缺失值用 NaN。标签矩阵按 horizon 保存收益、最大回撤和到达高点交易日数；同一 `t` 重复时最后一条覆盖。该格式避免标准日历表的空槽和重复字符串。

### Rust 公式引擎

Python/CLI 可提交受限 DSL，Rust 直接 mmap 读取 StockDB、按股票并行计算并写 `Compact*`：

```bash
cargo run --release --bin compute_formulas -- \
  <root> RawDailyBar examples/formulas/basic_technical.json factor 600519,000001 basic_v1
```

支持算术/比较/逻辑、标量函数 `abs/min/max/sqrt/log/exp/clip`，以及严格只使用当前及历史数据的窗口函数：
`ma/ema/sum/std/highest/lowest/roc/ref/rsi/atr`。多个公式共享相同窗口的预计算结果。

PyO3：

```python
db.compute_formulas("RawDailyBar", "600519", formulas_json, 0, None)
db.compute_formulas_to_compact("RawDailyBar", formulas_json, None, "factor", "basic_v1")
```

第二个接口在计算期间释放 GIL，并直接写 `CompactFactor/basic_v1/`，不把全量中间数组搬回 Python。不同公式集合必须使用不同 dataset，避免覆盖其他矩阵。

当前格式：32 字节文件头 + 56 字节/行；文件头包含 magic、版本、行宽和行数。
写入采用 sidecar 咨询锁和原子替换，可重复执行迁移。

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your option.
