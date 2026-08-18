# stockdb-rs 查询语法（QUERY-SYNTAX）

> 本文逐符号设计 DSL 的全部用法，与 `CONTRACT.md §2` 互为补充：
> **CONTRACT 是跨语言契约**（侧重边界语义、JSON / 二进制返回格式、schema 护栏）；
> **本文侧重语法符号的逐一用法与示例**（怎么写、写错会怎样）。
> 所有内容均以 `src/expr.rs`（tokenizer / 递归下降 parser / eval）真实实现为准，不臆造。

---

## 1. 总览

表达式是**中缀 DSL**：`字段 / 字面量 / 函数` 经 `比较 → 逻辑` 组合，可选括号与算术，可选窗口/标量函数。

```
close > 10 && volume <= 1e6 || ma(close, 20) > close
```

一次查询对该表每个 `code` 的每一行 `t` 求值，结果为「真」的行即命中，返回由 `stockdb_query`（JSON）或 `stockdb_query_bin`（二进制）取回。

---

## 2. 符号速查表

| 类别 | 符号 | 含义 | 可否作字段比较 |
|------|------|------|------|
| 数字字面量 | `10` `1.5` `1e6` `-3.2` `1.5e-3` | f64 | — |
| 字符串字面量 | `"cyb"` `'cyb'` | str | 仅 `==` `!=` |
| 字段引用 | `close` `code` `t` | 取自该表 schema | 视类型 |
| 比较 | `>` `<` `>=` `<=` `==` `!=` | 关系判断 | 数值/`==!=`字符串/布尔 |
| 逻辑 | `&&` `\|\|` `!` | 与/或/非 | — |
| 逻辑(关键字等价) | `and` `or` `not` | 同 `&&`/`\|\|`/`!` | — |
| 算术 | `+` `-` `*` `/` | 加减乘除（仅数字） | — |
| 一元 | `-x` `+x` | 正负号（仅数字） | — |
| 分组 | `( )` | 改变优先级 / 函数参数 | — |
| 分隔 | `,` | 函数实参分隔 | — |
| 窗口函数 | `ma(f,n)` `roc(f,n)` `ref(f,k)` | 序列指标（按 code 预计算） | — |
| 标量函数 | `abs(x)` `min(a,b)` `max(a,b)` | 数值函数 | — |

> **无布尔字面量**：`true` / `false` 不是关键字，会被当作字段名 → 报 `unknown field`。布尔字段的用法见 §7.4。

---

## 3. 字面量

### 3.1 数字
- 整数、小数、科学计数法均支持：`42` `3.14` `1e6` `1.5e-3` `2E10` `.5` `5.`
- 负数用一元负号 `-` 表示（如 `-5`、`-close`），**没有**「负号数字字面量」这一独立 token。
- 解析为 `f64`。大整数（如 `volume` 量级）作为 f64 时可能有精度损失，必要请用 `==`/`!=` 谨慎比较或走二进制返回（`query_bin`）保真。

```
close > 10
amount > 1e8
close < 1.5e2
```

### 3.2 字符串
- 双引号或单引号均可：`"cyb"` `'cyb'`
- 仅能用于 `==` / `!=` 与**字符串类型字段**比较（`> < >= <=` 对字符串报错）。
- 常见字符串字段：`code`（如 `"600000"`）、`date`、`board`（CompanyProfile 表）、`name`、`industry`。

```
code == "600000"
board == "cyb" || board == '创业板'
date == "2024-01-02"
```

---

## 4. 字段引用

- 直接写字段名，取自**当前查询表**的 schema（字段清单见 `CONTRACT.md §3`）。
- **小写敏感**：`Close` ≠ `close`，写错 → `unknown field: Close`。
- RawDailyBar 数值字段：`open` `high` `low` `close` `volume` `amount` `turnover` `t`；字符串字段：`code` `date`。
- `t` 是**全局交易日索引（整数）**，不是日期（详见 §7.3）。

```
close > open
volume > 1000000
```

---

## 5. 运算符

### 5.1 优先级（从低到高）

| 优先级 | 符号 | 结合性 | 说明 |
|--------|------|--------|------|
| 1（最低） | `\|\|` `or` | 左 | 逻辑或 |
| 2 | `&&` `and` | 左 | 逻辑与 |
| 3 | `!` `not` | 前缀（右） | 逻辑非，绑定其后一个比较表达式 |
| 4 | `>` `<` `>=` `<=` `==` `!=` | 左 | 比较 |
| 5 | `+` `-` | 左 | 二元加减 |
| 6 | `*` `/` | 左 | 二元乘除 |
| 7 | 一元 `-` `+` | 前缀（右） | 正负号，**比 `*` `/` 更紧** |
| 8（最高） | 字面量 / 字段 / 函数 / `( )` | — | 原子 |

推论（对照实现验证）：
- `!a > b` ⇒ `(!a) > b`（`!` 比比较紧）
- `a + b > c` ⇒ `(a + b) > c`（加减比比较松）
- `-x * y` ⇒ `(-x) * y`（一元负号比 `*` 紧）
- `!-x` ⇒ `!( -x )`（`!` 比一元负号松）

### 5.2 比较 `> < >= <= == !=`
- `==` / `!=`：支持 **同类型** 比较 —— 数/数、字符串/字符串、布尔/布尔；**类型不同报错**（`== type mismatch`）。
- `>` `<` `>=` `<=`：两侧必须是数字；字符串或布尔参与报错（`expected number`）。
- 比较产生布尔值，供逻辑运算或最终命中判定使用。

```text
close > 10           # 数值比较
close == open        # 数/数 == 
code == "600000"     # 字符串 ==
is_st == is_st       # 布尔/布尔 == （DailySnapshot 表）
close > "10"         # ❌ 类型不同 → 报错
board > "a"          # ❌ 字符串不能 >  → 报错
```

### 5.3 逻辑 `&& || !` 与关键字 `and or not`
- 三者可混用，且提供 **`and` / `or` / `not` 全称关键字**（小写，等价于符号）。
- 操作数为「真值」：见 §7.1 truthy 规则。

```text
close > 10 && volume <= 1e6
close > 10 and volume <= 1e6      # 等价
!(close > 10) || volume > 1e6
not (close > 10) or volume > 1e6  # 等价
```

### 5.4 算术 `+ - * /`
- 仅数字；任一侧为字符串/布尔 → `expected number`。
- 用于构造派生量后参与比较：

```text
(close - open) / open > 0.05      # 日内涨幅
(high - low) / open > 0.03
amount / volume > 10              # 均价（注意除零 → NaN，见 §7.3）
```

### 5.5 一元正负号 `-x` `+x`
- 仅数字。`-x` = 取负；`+x` = 恒等（保留语法，便于写 `+close`）。
- 优先级高于 `*` `/`（见 §5.1）。

```text
-close < -open
abs(close - open) / open > 0.05
```

### 5.6 括号 `( )` 与逗号 `,`
- `( )`：改变优先级、包裹子表达式、函数实参列表。
- `,`：仅用于函数实参分隔（见 §6）。

```text
(close > 10 || close < 5) && volume > 1e6
ma(close, 20)
min(open, close)
```

---

## 6. 函数

### 6.1 窗口函数（按 `code` 序列上下文，查询时逐 code 预计算）

| 函数 | 签名 | 语义 | 前导不足 |
|------|------|------|----------|
| `ma` | `ma(field, n)` | 含当前行的滑动均值 `[t-n+1 ..= t]` | 返回 `NaN` |
| `roc` | `roc(field, n)` | 动量：`field[t] / field[t-n] - 1` | 返回 `NaN` |
| `ref` | `ref(field, k)` | 前 `k` 日值（默认 `k=1`） | 返回 `NaN` |

约束（来自 `bind` 实现）：
- **第一参数必须是字段名**（不能写算术、不能写常量）；第二参数为数字窗口大小。
- 不能嵌套窗口：`ma(close, ma(...))` ❌；`ref` 不能包其它函数。
- `roc` / `ref` 的 `n`/`k` 必须为正整数字面量或常量；非数字 → `window size must be a number`。
- 相同 `(fun, field, k)` 只预计算一次（自动去重）。
- 前导 `NaN` 在比较中恒为假（见 §7.3），因此「上市不足 n 日」的行不会命中 `ma(...)>x`。

```text
ma(close, 20) > close          # 收盘价低于 20 日均线
roc(close, 5) > 0.1            # 5 日动量 > 10%
close > ref(close, 1)          # 今日收盘 > 昨日收盘
ma(volume, 5) > 1e6            # 5 日均量超百万
ref(close, 1) > 0              # 昨日有收盘（过滤上市首日）
```

### 6.2 标量函数

| 函数 | 签名 | 语义 |
|------|------|------|
| `abs` | `abs(x)` | 绝对值（一元，数字） |
| `min` | `min(a, b)` | 较小值（二元，数字） |
| `max` | `max(a, b)` | 较大值（二元，数字） |

- 函数名**小写敏感**（大写 `MA`/`Abs` 报 `unknown function`）。
- 实参可嵌套任意表达式（含窗口函数、算术）：

```text
abs(close - open) / open > 0.05
max(open, close) < 10
abs(ma(close, 5) - ma(close, 20)) < 1
```

---

## 7. 类型与求值规则

### 7.1 真值（truthy）
表达式最终「是否为真」决定该行是否命中，规则：

| 值 | 真值 |
|----|------|
| 数值 `f` | `!NaN && f != 0` |
| 字符串 `s` | 非空 |
| 布尔 `b` | `b` 本身 |

用于 `&&` / `||` / `!` 及最终命中判定。

### 7.2 类型不匹配
- 比较 `==` / `!=` 两侧类型不同 → 报错（`== type mismatch` / `!= type mismatch`）。
- 算术 `+ - * /`、大小比较 `> < >= <=` 要求数字，否则报错（`expected number`）。

### 7.3 空值 / NaN
- 空槽（present=0）在查询中被解为 `NaN`（f64）。
- `NaN` 的 truthy = **false**（不命中）。
- `NaN` 比较遵循 f64 语义：`NaN > 5` = false，`NaN == NaN` = false，`NaN != NaN` = **true**。
- 推论：`close > 10` 自动排除空值行；但 `close != close` 对空值行会为真（一般避免这样写）。

### 7.4 布尔字段（没有布尔字面量）
- `true` / `false` **不是合法字面量**，写作它们会被当字段名 → `unknown field: true`。
- 布尔字段（如 DailySnapshot / CompanyProfile 的 `is_st`）这样用：
  - 直接作逻辑值：`is_st`（真值取字段布尔值）
  - 取反：`!is_st`
  - 与数值谓词组合：`is_st && close > 10`
  - **不能**写 `is_st == true`（无布尔字面量）；如需「为真」直接写 `is_st`，「为假」写 `!is_st`。

```text
is_st && close > 10           # DailySnapshot 表：ST 股且收盘 >10
!is_st || close > 5           # 非 ST，或收盘 >5
```

---

## 8. 完整示例集

### 基础比较
```text
close > 10
close >= open
close == 10.0
```

### 多条件组合
```text
close > 10 && volume <= 1e6
(close > 10 || close < 5) && volume > 1e6
close > 10 and volume <= 1e6          # 关键字等价
not (close > 10) or volume > 1e6      # 关键字等价
```

### 算术派生
```text
(close - open) / open > 0.05          # 日内涨幅 > 5%
(high - low) / open > 0.03            # 振幅 > 3%
amount / volume > 10                  # 均价（除零行得 NaN，不命中）
```

### 字符串
```text
code == "600000"
board == "cyb"                        # CompanyProfile 表
date >= "2024-01-01"                  # ❌ 字符串不能 >= → 报错（用 t 比较）
```

### 窗口函数
```text
ma(close, 20) > close
roc(close, 5) > 0.1
close > ref(close, 1)
ma(volume, 5) > 1e6 && close > ref(close, 1)
```

### 标量函数
```text
abs(close - open) / open > 0.05
max(open, close) < 10
abs(ma(close, 5) - ma(close, 20)) < 1
```

### 布尔字段（DailySnapshot 表）
```text
is_st && close > 10
!is_st || close > 5
```

### 综合
```text
close > 10
&& volume <= 1e6
&& ma(close, 20) > close
&& (close - open) / open > 0.05
&& code != "600519"
```

---

## 9. 常见错误

| 写法 | 结果 | 正确写法 |
|------|------|----------|
| `close > 10 or volume > 1e6` | ✅（or 是关键字） | — |
| `OR` / `AND` / `NOT` | ❌ 仅小写关键字 | 用小写 `or`/`and`/`not` 或符号 |
| `true` / `false` | ❌ `unknown field` | 布尔字段用 `is_st` / `!is_st` |
| `MA(close,20)` / `Abs(x)` | ❌ `unknown function` | 小写 `ma`/`abs` |
| `ma(close+open, 5)` | ❌ 窗口第一参数须为字段 | `ma(close, 5)` |
| `ma(close, vol)` | ❌ 窗口大小须为数字 | `ma(close, 5)` |
| `board > "a"` | ❌ 字符串不能大小比较 | 用 `==`/`!=`：`board == "a"` |
| `close == "10"` | ❌ 类型不同 | `close == 10` |
| `Close > 10` | ❌ 字段名小写敏感 | `close > 10` |
| `date > "2024-01-01"` | ❌ 字符串不能 `>` | 用 `t` 整数索引比较 |
| `ref(close)` | ✅ 默认 k=1 | — |
| `-5` 作为字面量 | ✅（一元负号 + 数字） | — |

---

## 10. 与 CONTRACT.md 的关系

- `CONTRACT.md §2`：DSL 的**契约权威**（边界语义、返回 JSON schema、二进制返回格式、schema 护栏），供 binding 作者实现调用端。
- 本文：语法符号的**用法速查与示例**，供写查询的人参考。
- 返回结果格式、内存释放（`free_str` / `free_buf`）、`t` 的全局索引含义，以 `CONTRACT.md` 为准。
