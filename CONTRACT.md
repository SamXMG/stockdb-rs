# stockdb-rs 跨语言数据模型契约 (CONTRACT)

本文件是 **stockdb-rs 与任意调用方（C / C++ / Go / Java / Ruby / Node / Python …）之间的唯一权威契约**。

引擎本身与任何宿主语言解耦——语言只是消费者之一。调用方只需遵循本文，即可在不依赖 Python 或其它特定宿主的前提下读写数据、执行 DSL 查询。

契约分两层边界：

| 边界 | 内容 | 何时需要 |
|------|------|---------|
| **C ABI**（内存中调用） | §1 函数签名 + §3 字段表 + §2 查询 DSL/JSON | 所有跨语言调用（推荐路径） |
| **磁盘字节布局**（直接读 `.dat`） | §4 定长 stride 编码 | 仅当调用方绕过 FFI、自行解析落盘文件时 |

> 参考实现：Python `stockdb` 引擎与本文 1:1 对应，但它**不是**契约本身，只是某一种绑定。契约以本文 + `layout.rs` 常量源码为准。

---

## 1. C ABI 接口

### 1.1 动态库产物

| 平台 | 文件名 |
|------|--------|
| Windows | `stockdb_rs.dll` |
| Linux | `libstockdb_rs.so` |
| macOS | `libstockdb_rs.dylib` |

- 编译：`cargo build --release`（crate-type `["lib","cdylib"]`）。
- 导出符号即函数名（`extern "C"`，**无 name mangling**；Windows MSVC 亦为裸名，可用 `GetProcAddress` 直接按名取）。
- 所有导出函数均为 `unsafe`：接收裸指针，调用方须自行保证指针有效。

### 1.2 函数原型（C）

```c
// 打开存储根目录，返回不透明句柄；失败返回 NULL。
StoreHandle* stockdb_open(const char* root);

// 释放句柄。handle 须来自 stockdb_open 且未释放；可传 NULL（no-op）。
void stockdb_free(StoreHandle* handle);

// 读某 code 的某数值列，写入调用方分配的 f64 缓冲。
// 返回写入元素个数；-1 表示错误（未知表/字段、缓冲不足、非数值列）。
// 空值以 f64::NAN 占位；t 列按 i64 转 f64 写入。
int stockdb_read_column_f64(StoreHandle* handle, const char* table,
                            const char* code, const char* field,
                            double* out, size_t cap);

// 按全局交易日索引 t 取单条某数值字段（O(1) 随机读）。
// 成功返回 0，失败返回 -1；结果写入 out（须 ≥ 8 字节可写）。
int stockdb_read_at_f64(StoreHandle* handle, const char* table,
                        const char* code, size_t t,
                        const char* field, double* out);

// 执行 DSL 查询，返回命中行 JSON 数组（C 字符串）。
// 失败（未知表 / 语法错 / 句柄或字符串为 NULL）返回 NULL。
// 调用方【必须】用 stockdb_free_str 释放，否则内存泄漏。
char* stockdb_query(StoreHandle* handle, const char* table, const char* expr);

// 释放 stockdb_query 返回的字符串。可传 NULL（no-op）。
void stockdb_free_str(char* p);

// 执行 DSL，返回命中行【原始二进制】缓冲（零 JSON 序列化）。
// out_len / out_cap 回传缓冲长度 / 容量；失败返回 NULL。
// 调用方【必须】用 stockdb_free_buf(ptr, len, cap) 释放，否则内存泄漏。
uint8_t* stockdb_query_bin(StoreHandle* handle, const char* table, const char* expr,
                           size_t* out_len, size_t* out_cap);

// 释放 stockdb_query_bin 返回的缓冲（须回传当时的 len/cap）。可传 NULL（no-op）。
void stockdb_free_buf(uint8_t* p, size_t len, size_t cap);

// 返回某表字段布局指纹（确定性，跨运行稳定）；用于校验二进制结果的 schema 版本。
uint64_t stockdb_schema_hash(const char* table);
```

### 1.3 生命周期与内存所有权（硬约束）

| 资源 | 创建 | 释放 | 规则 |
|------|------|------|------|
| 句柄 `StoreHandle*` | `stockdb_open` | `stockdb_free` | 一一配对；`free` 后指针即失效，不得再传 |
| 查询字符串 `char*` | `stockdb_query` | `stockdb_free_str` | **必须释放**，否则泄漏；可重复释放？不可——仅释放一次 |
| 二进制结果缓冲 `uint8_t*` | `stockdb_query_bin` | `stockdb_free_buf(ptr, len, cap)` | **必须释放**，且须回传 `out_len`/`out_cap` 的精确值；否则泄漏 |

其它约定：

- 所有 `const char*` 参数必须是 **有效、以 NUL 结尾的 UTF-8** 字符串。
- `stockdb_read_column_f64` 的 `out` 缓冲须可写且容量 ≥ `cap * 8` 字节；返回元素个数 `n` 若 `n > cap` 则函数返回 `-1`（缓冲不足），调用方应据表长度预分配。
- 失败时：`stockdb_open` / `stockdb_query` 返回 `NULL`；`stockdb_read_*` 返回 `-1`。

### 1.4 错误处理

当前版本：`open` / `query` 失败仅返回 `NULL`，**不提供错误字符串**（后续可加 `stockdb_last_error()`）。调用方务必判空。

---

## 2. 查询 DSL 契约

DSL 是**语言中立的字符串契约**：调用方构造表达式字符串传入 `stockdb_query`，引擎返回 JSON 字符串。查询语义与 Rust `Store::query` / PyO3 `query` 完全相同。

### 2.1 语法

```
expr      := or_expr
or_expr   := and_expr ( ('||'|'or') and_expr )*
and_expr  := not_expr ( ('&&'|'and') not_expr )*
not_expr  := '!' not_expr | 'not' not_expr | cmp_expr
cmp_expr  := add_expr ( ('>'|'<'|'>='|'<='|'=='|'!=') add_expr )*
add_expr  := mul_expr ( ('+'|'-') mul_expr )*
mul_expr  := unary ( ('*'|'/') unary )*
unary     := '-' unary | '+' unary | primary
primary   := field | number | string | func_call | '(' expr ')'

field     := identifier                 // 表字段名，见 §3
number    := [0-9]+ ('.' [0-9]*)? ([eE] [+-]? [0-9]+)?   // 支持科学计数法 1e6 / 1.5e-3
string    := '"' ... '"' | '\'' ... '\''                // 字符串字面量，双/单引号
func_call := name '(' arg (',' arg)* ')'                 // 见 §2.2 / 标量函数
```

- 字段名直接写（如 `close`、`volume`），无需引号。
- 运算符优先级（从高到低）：primary(字段/字面量/函数/括号) > 一元 `-`/`+` > `*`/`/` > `+`/`-` > 比较(`> < >= <= == !=`) > `!`/`not` > `&&`/`and` > `||`/`or`。
- 逻辑提供符号 `&&`/`||`/`!` 与关键字 `and`/`or`/`not`（小写等价）。
- 比较返回布尔；逻辑 `&&`/`||` 短路；`!` 取反。
- 标量函数：`abs(x)`、`min(a,b)`、`max(a,b)`。
- **无布尔字面量** `true`/`false`：会被当字段名报 `unknown field`；布尔字段用法见 QUERY-SYNTAX.md §7.4。

### 2.2 窗口函数（按 code 序列预计算）

仅在单只 code 的时间序列上下文有意义（`t` 为全局交易日索引，按 `t` 递增排列）：

| 函数 | 语义 | 边界 |
|------|------|------|
| `ma(field, n)` | 截至当前行的 n 日滑动均值：`mean(field[t-n+1 .. t])` | `t < n-1` 时为 `NaN`（需 ≥ n 个历史点） |
| `roc(field, n)` | 动量：`field[t] / field[t-n] - 1` | `t < n` 时为 `NaN`；除数为 0 或 `NaN` 时结果 `NaN` |
| `ref(field, k=1)` | 前 k 日值：`field[t-k]` | `t < k` 时为 `NaN` |

`ma` 为**含当前行**的后向窗口（trailing，含 t）；`ref(close)` 等价于 `close[t-1]`。

### 2.3 返回 JSON 结构

顶层为**数组**；每个命中元素是一个 **JSON 对象**：

```json
{
  "code": "600000",          // string，股票/指数代码
  "t": 1234,                 // integer，全局交易日索引（非日期字符串）
  "open": 10.5,              // f64 -> number
  "high": 11.2,
  "volume": 1.5e6,           // 支持科学计数法
  "is_st": false,            // bool -> boolean（仅 bool 字段）
  "board": "main",           // 字符串字段 -> string
  "amount": null             // 空值 / NaN -> null
}
```

字段值类型映射（命中行内除 `code` / `t` 外的每个字段）：

| Rust `Value` | JSON 类型 | 备注 |
|------|----------|------|
| `F64(f)`（`f.is_nan()`） | `null` | 空值 |
| `F64(f)` | `number` | 有限浮点 |
| `I64(x)` | `number` | `t` 字段及整数列；注意 `read_column_f64` 会将其转为 `f64` |
| `Str(s)` | `string` | 定宽字符串，已去尾部 `\0` 与空白 |
| `Bool(b)` | `boolean` | 仅 `BOOL_FIELDS` 中的字段 |
| `Null` | `null` | 空槽 / 空值 |

> **关键约定**：`t` 是**全局交易日索引（整数）**，不是日期。要得到日期，用 `date` 字段（多数表自带字符串日期），或按 `t` 下标查根目录 `calendar.json`（见 §4.3）。

### 2.4 二进制返回（零序列化，`stockdb_query_bin`）

`stockdb_query` 走 JSON，便于调试 / 小结果集；但 JSON 有序列化 + 解析双重开销、且
数值类型会被统一成 float（`t` / 大整数失真、NaN 与 null 同形）。**宽查询 / 性能关键路径**
应改走 `stockdb_query_bin`：它返回命中行的**原始 stride 字节**，调用端按 §4 自行解码。

**缓冲区布局（小端）**：

```
偏移      字段           类型      说明
[0..4]    magic          u32       0x53544231 ("STB1")，校验魔数
[4..8]    record_len     u32       单行字节数（= §3.4，如 RawDailyBar=71）
[8..16]   n_hits         u64       命中行数
[16..24]  schema_hash    u64       字段布局指纹（= stockdb_schema_hash(table)）
[24..]    rows           bytes     n_hits × record_len，每行即 §4 定长 stride 编码
```

- 每行与 `.dat` 单行**同构**：首字节 `present`（1/0），其后字段按 §3.1 顺序、小端排布
  （bool `?` / 字符串 `{w}s` / `t` 为 `q` i64 / 数值 `d` f64，NaN=空）。`code` 是第一字段、
  `t` 是第二字段，解码后即为该行的代码与交易日索引。
- **schema 护栏**：解码前用 `stockdb_schema_hash(table)` 取本地指纹，与 header 的
  `schema_hash` 比对；不一致说明双方布局版本漂移，应拒绝解析以免读错字节。
- **所有权**：返回的 `out_len` / `out_cap` 必须原样回传 `stockdb_free_buf(ptr, len, cap)`
  释放，否则泄漏（纪律同 §1.3）。

**调用端适配（示例，Python 参考实现见 `python/stockdb_rs.py::decode_rows`）**：

```python
import struct
buf, n_hits, rlen, shash = db.query_bin("RawDailyBar", "close>10")
assert shash == db.schema_hash("RawDailyBar")          # schema 版本护栏
assert buf[0:4] == (0x53544231).to_bytes(4, "little")  # magic
off = 24
for _ in range(n_hits):
    present = buf[off]; off += 1
    if not present:
        off += rlen - 1; continue
    code = buf[off:off+16].split(b"\x00")[0].decode(); off += 16   # 首字段 code
    t    = struct.unpack_from("<q", buf, off+16)[0]                # 第二字段 t
    # ... 其余字段按 §3.1 / §3.2 顺序与宽度逐字段解码 ...
```

> 二进制路径把"如何把字节变成对象"的责任完全交给调用端——正是"调用端自己写适配接口"
> 的取向：Rust 侧只负责高效地选出命中行并拷贝原始字节，不做任何类型装箱。

---

## 3. 字段类型表（数据模型）

类型元字符（与 §4 字节布局一一对应）：

| 元字符 | 含义 | 字节宽度 | 内存类型 |
|--------|------|---------|---------|
| `?` | bool | 1 | boolean |
| `s` | 定宽字符串 | `STR_W[name]` | string |
| `q` | i64（即 `t` 字段） | 8 | integer |
| `d` | f64（NaN 为空） | 8 | number |
| `I` | 缩放整数 i32（空值哨兵 `i32::MIN`，读时 ÷scale 还原 f64） | 4 | number |

### 3.1 各表字段序列（落盘顺序 = 下表顺序）

**RawDailyBar**: `code, t, date, open, high, low, close, volume, amount, turnover`

**FundFlow**: `code, t, date, main_net, main_pct, xl_net, xl_pct, l_net, l_pct, mid_net, mid_pct, small_net, small_pct`

**AdjustEvent**: `code, ex_date, t, bonus_per_share, cash_per_share, fwd_ratio`

**IndexDaily**: `index_code, t, date, open, high, low, close, volume, amount`

**CompanyProfile**: `code, name, former_names, board, exchange, list_date, delist_date, is_st, industry, region, full_name, total_shares, float_shares, market_cap_yi, float_cap_yi, is_hs300, is_zz500, is_zz1000, is_zz2000, is_finance, company_type, note`

**Announcement**: `code, ann_date, ann_type, title, summary, url, t`

**RenameEvent**: `code, announce_date, effective_date, old_name, new_name, reason, t`

**DailySnapshot**: `code, date, t, name, board, is_st, price, prev_close, chg_pct, vol_ratio, turnover, market_cap_yi, float_cap_yi, pe, pb, chg60, flow_main, flow_main_pct, flow_xl, flow_xl_pct, flow_l, flow_l_pct, industry, concepts`

### 3.2 字符串字段宽度 `STR_W`（字节，`\0` 右补齐）

```
code=16, index_code=16, date=10, ex_date=10, list_date=10, delist_date=10,
ann_date=10, announce_date=10, effective_date=10, board=16, exchange=8,
industry=24, region=16, company_type=16, ann_type=16, name=32,
former_names=64, full_name=64, old_name=32, new_name=32, title=128,
summary=128, url=64, reason=64, note=64, concepts=192
```

### 3.3 bool 字段集 `BOOL_FIELDS`

| 表 | bool 字段 |
|----|-----------|
| RawDailyBar / FundFlow / AdjustEvent / IndexDaily / Announcement / RenameEvent | （无） |
| CompanyProfile | `is_st, is_hs300, is_zz500, is_zz1000, is_zz2000, is_finance` |
| DailySnapshot | `is_st` |

### 3.4 单条记录字节长度 `record_len`（含首字节 present）

已对 Python `struct.calcsize` 实测对齐：

- `RawDailyBar` = 71（价格列 open/high/low/close + turnover 改为 4 字节缩放整数 `I`）
- `IndexDaily` = 67（价格列 open/high/low/close 改为 `I`）
- `FundFlow` = 95（5 个 `*_pct` 改为 `I`）
- `DailySnapshot` = 384（price/prev_close + 9 个百分比/比率列改为 `I`）
- `CompanyProfile` = 379（未缩放）
- `AdjustEvent` = 59（未缩放）
- 其余各表按 §3.1 字段序列 + §3.2/§3.3 宽度累加（公式见 §4.1）；缩放列按 `I`(4B) 计，其余数值按 `d`(8B)。

---

## 4. 磁盘字节布局（直接读 `.dat` 时遵循）

> 仅当调用方**不通过 C ABI**、自行解析落盘文件时才需遵守本节。经由 FFI 调用无需了解字节细节。

### 4.1 文件结构

```
<root>/
  calendar.json              # 全局交易日历（date 字符串数组，下标 = t）
  <table>/<code>.dat        # 定长二进制数据
  <table>/<code>.meta       # 元数据（JSON sidecar）
  <table>/<code>.dat.lock   # 写时咨询锁（sidecar，勿依赖其内容）
```

`.dat` 为**定长 stride** 文件：总长度 = `cal_len × record_len(table)` 字节，第 `t` 行偏移 = `t × record_len(table)`。

### 4.2 单行编码（小端 `<`）

```
[ present: u8 ][ field_0 ][ field_1 ] ... [ field_{n-1} ]
```

- `present`：首字节，`1` = 有数据，`0` = 空槽（整行跳过）。
- 字段按 §3.1 顺序排布，类型见 §3 类型元字符：
  - bool：`?`，1 字节 u8（`0/1`）。
  - 字符串：`{w}s`，定宽 `w` 字节 UTF-8，**右截断** + `\x00` 右补齐；读时按首个 `\0` 截断并 `trim`。
  - `t`：`q`，i64 全局交易日索引。
  - 其余数值：`d`，f64；**空值用 `f64::NAN` 占位**。
  - 缩放整数：`I`，i32（4 字节）；写时 `(f64 × scale).round()`、读时 `÷ scale` 还原；
    **空值用哨兵 `i32::MIN` 占位**（见 §3 类型表）；价格类列（2 位小数，scale=100）与
    百分比/比率类列（4 位小数，scale=10000）启用，字段清单见 `layout::SCALED`（Rust 侧常量）。
- `encode_row`（`layout.rs`）与 `decode_row` 完全对称，可直接落盘 / 回读。

### 4.3 交易日历 `calendar.json`

根目录 `calendar.json` 为 date 字符串数组（`["2024-01-02", ...]`）。`t` 即数组下标；`read_at_f64(table, code, t, ...)` 的 `t` 与此对齐。写操作可能扩展该数组（append 新交易日），原子写保证多写者不互相丢失。

### 4.4 写入原子性与并发

- 写路径（`write` / `repack` / `write_meta` / `save_calendar`）在目标文件的 sidecar `.lock` **咨询锁**保护下，走 `temp 文件 + fsync + 原子 rename`。
- 进程写中途崩溃不会留下半截文件；读者靠原子 rename 读到完整文件（最终一致），读路径无锁。
- **不改变磁盘格式**，故直接读 `.dat` 的调用方与经 FFI 的调用方看到的字节完全一致。

---

## 5. 兼容性与版本约定

- **稳定承诺（变更须 MAJOR 版本）**：C ABI 函数签名、§2 JSON schema、§3/§4 磁盘字节布局与字段表。这些是跨语言契约，不可破坏性变更。
- 字段**新增**通常向后兼容（旧文件多出的字段按 `null`/`NaN` 处理），但字段**重排 / 改名 / 改宽度**属破坏性变更。
- 参考实现 Python `stockdb` 与本文 1:1；任何偏离以本文 + `layout.rs` 常量为准。
- 本次格式演进：价格类列（open/high/low/close/prev_close/price，scale=100）与百分比/比率类列（turnover/chg_pct/vol_ratio/chg60/pe/pb/*_pct 等，scale=10000）由 `d`(f64,8B) 改为 `I`(缩放整数,4B)，属破坏性变更，须升 MAJOR 版本并提供 `.dat` 迁移；既有数据须重新写入。
- 仓库内含「Rust 输出 vs 参考实现回读」的字节级对齐测试，作为契约回归保护——修改 §3/§4 须同步更新。

---

## 6. 最小调用示例（C）

```c
#include <stdio.h>
#include <stdlib.h>

typedef struct StoreHandle StoreHandle;  // 不透明，无需展开

StoreHandle* stockdb_open(const char* root);
void         stockdb_free(StoreHandle* h);
char*        stockdb_query(StoreHandle* h, const char* table, const char* expr);
void         stockdb_free_str(char* p);

int main(void) {
    StoreHandle* db = stockdb_open("C:/data/store");
    if (!db) { fprintf(stderr, "open failed\n"); return 1; }

    char* json = stockdb_query(db, "RawDailyBar",
                               "close>10 && ma(volume,5)>1e6");
    if (json) {
        printf("%s\n", json);
        stockdb_free_str(json);   // 【必须】释放查询结果
    }

    stockdb_free(db);             // 释放句柄
    return 0;
}
```

---

*本契约随 `stockdb-rs` 维护。任何语言绑定（Go/Java/Node…）只需对接 §1 + §2，必要时参考 §3/§4。*
