# StockDB AI 快速入口

这是 `stockdb-rs` 子仓库给 AI/自动化代理的快速入口。完整的项目级数据流、本地优先规则和 Python 业务 API 见：
`../screener_cn/docs/架构/StockDB数据库与本地优先开发指南.md`。

## 先记住的规则

- `calendar.json` 是全库唯一交易日历，`t` 是它的全局下标。
- 标准 `.dat`、`.flow`、`.mtx` 只能由 Rust API/`import_json` 写入；不要用 Python `struct` 编码。
- 运行时读取优先 StockDB，网络只负责补齐缺失或过期数据。
- `RawDailyBar` 是 raw 真相源；复权是读取时视图，不要把 qfq 历史价当成原始数据。
- 原始 JSON 保留，二进制是可重建的运行副本。

## Rust 核心 API

```rust
use stockdb_rs::Store;

let db = Store::open("stockdb/root")?;
let rows = db.read("RawDailyBar", "600519")?;
let one = db.read_at("RawDailyBar", "600519", 120)?;
let range = db.read_range("RawDailyBar", "600519", 100, 300)?;
let many = db.read_many("RawDailyBar", "600519", &[100, 120])?;
let hits = db.query_bin("RawDailyBar", "close>100")?;
db.validate("RawDailyBar", "600519")?;
```

高频访问选择：连续回测用 `read_range`，随机访问用 `read_at/read_many`，宽查询用 `query_bin`，重复读取由内部 mmap 复用。

## Python PyO3 API

```python
from stockdb_rs import StockDB

db = StockDB("stockdb/root")
rows = db.read_rows("RawDailyBar", "600519")
close = db.read_column("RawDailyBar", "600519", "close")
value = db.read_at("RawDailyBar", "600519", 120, "close")
db.read_flow_rows("600519")
db.query_bin("RawDailyBar", "close>100")
```

业务层更推荐 `screener_lib.local_market.kline/profile/snapshot/fund_flow/bulk_snapshot`，它统一了本地二进制、JSON 缓存和联网回退；其中旧版 `DailySnapshot/*.snap` 仅是迁移兼容路径，新代码必须使用 Rust 标准 `.dat`。

## Rust JSON 导入

```powershell
cargo run --release --bin import_json -- snapshot <root> <snapshot-dir>
cargo run --release --bin import_json -- company-profile <root> <snapshot-dir> <industry-history.json>
cargo run --release --bin import_json -- money-flow <root> <d5d6-dir>
cargo run --release --bin import_json -- compact <root> factor <jsonl> <columns>
cargo run --release --bin import_json -- compact <root> label <jsonl> <columns>
```

Python 只生成 JSON/JSONL；Rust 负责 schema 映射、日历校验、锁、原子写入。修改字段或磁盘格式前必须同步 `src/layout.rs`、`CONTRACT.md`、测试和上层读取 API。

Rust 也负责前瞻标签：`build_forward_labels <root> RawDailyBar <dataset> <horizon> [codes]` 写入 `CompactLabel/<dataset>`。标签使用次日开盘、未来 horizon 日收盘、最大回撤、到达高点天数和“收益阈值先于止损阈值”障碍顺序；不得在 Python 中另算一套口径。

PyO3 还提供 `StockDB.build_context_factors(dataset, codes=None, industry_history=None)`，由 Rust 读取历史资金流、`IndustryDaily`、版本化行业归属和 `RawDailyBar`，按点时规则写入 `CompactFactor/<dataset>/{code}.mtx`。上下文缺失保留为 NaN，并带有资金流、行业、板块可用标记；Python 不得自行解码或写入该矩阵。

前瞻标签同时包含次日可成交、涨停、跳空、停牌、窗口完整性和到期跌停不可卖字段。训练层会过滤不可执行样本，组合层的停牌日仍会消耗持仓等待天数。

## 公式计算

公式文件参考 `examples/formulas/basic_technical.json`。Rust 支持：

- 运算：`+ - * / > < >= <= == != && || !`
- 标量：`abs/min/max/sqrt/log/exp/clip`
- 窗口：`ma/ema/sum/std/highest/lowest/roc/ref/rsi/atr`

```powershell
cargo run --release --bin compute_formulas -- <root> RawDailyBar <formulas.json> factor <codes> <dataset>
```

Python 业务层使用 `screener_lib.rust_formulas.compute()` 预览，使用
`materialize()` 全市场并行计算并直接写 `CompactFactor`。禁止在 Python 中增加逐行 fallback。
输出按 dataset 隔离为 `CompactFactor/<dataset>/{code}.mtx`；Python 包装层默认按公式内容生成 hash 命名空间。

## 关键源码

- schema/字段布局：`src/layout.rs`
- Store 读写/mmap：`src/lib.rs`
- Python 原生绑定：`src/pyo3_api.rs`
- C ABI：`src/ffi.rs`
- JSON 导入器：`src/bin/import_json.rs`
- 紧凑矩阵：`src/compact.rs`
- 公式 DSL/批量执行：`src/expr.rs`
- 跨语言契约：`CONTRACT.md`
