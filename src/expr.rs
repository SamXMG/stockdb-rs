//! 声明式查询 DSL：把字符串表达式解析为 AST，在列式数据上逐行求值，返回所有
//! 为 `true` 的行（命中数据）。DSL 字符串是**语言中立契约**——任何宿主语言都可
//! 直接构造并传入（手写同构字符串，或借宿主侧转译器生成），无需绑定特定语言。
//!
//! 语法（中缀，贴近用户直觉）：
//!   字段 / 字面量 / 函数 经 比较(`> < >= <= == !=`) 与 逻辑(`&& || !`) 组合，
//!   支持括号与算术(`+ - * /`) 以及字符串字面量(`"cyb"`)。
//!   窗口函数（需序列上下文，按 code 预计算指标数组）：
//!     - `ma(field, n)`   滑动均值，窗口含当前行 [t-n+1 ..= t]
//!     - `roc(field, n)`  动量 = field[t]/field[t-n] - 1
//!     - `ref(field[, k])` 前 k 日值（默认 k=1）
//!   标量函数：`abs(x)` `min(a,b)` `max(a,b)`
//!
//! 例：`close>10 && volume<=1e6 || ma(close,20)>close`
//!
//! 设计要点：表达式在 Rust 侧被解析成 AST 并求值，**绝不跨 FFI 回调宿主语言**；
//! 调用方只传一次字符串、收回一次命中结果（语言中立的 request/response 契约）。

use std::collections::HashMap;

use serde_json::Value as J;

use crate::layout::FieldKind;
use crate::{Store, Value};

// ---------------- AST ----------------

#[derive(Debug, Clone)]
enum Expr {
    Num(f64),
    Str(String),
    Field(String),
    /// 解析后绑定：列下标(在 `Record.fields` 中的位置) + 原字段名。
    /// 窗口函数(ma/roc/ref)仍需原字段名去查预计算数组，故保留名字。
    Col(usize, String),
    /// 窗口函数引用（绑定后）：`win_maps` 数组下标，直接取预计算值，零每行哈希/分配。
    Win(usize),
    Fun(String, Vec<Expr>),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, Copy)]
enum UnOp {
    Neg,
    Not,
}

#[derive(Debug, Clone, Copy)]
enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Gt,
    Lt,
    Ge,
    Le,
    Eq,
    Ne,
    And,
    Or,
}

// ---------------- 词法分析 ----------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Str(String),
    Ident(String),
    Op(&'static str),
}

fn tokenize(src: &str) -> Result<Vec<Tok>, String> {
    let b = src.as_bytes();
    let n = b.len();
    let mut i = 0;
    let mut out = Vec::new();
    while i < n {
        let c = b[i] as char;
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '"' || c == '\'' {
            let q = c;
            i += 1;
            let start = i;
            while i < n && (b[i] as char) != q {
                i += 1;
            }
            let s = std::str::from_utf8(&b[start..i])
                .map_err(|_| "bad string literal".to_string())?
                .to_string();
            if i < n {
                i += 1;
            }
            out.push(Tok::Str(s));
            continue;
        }
        if c.is_ascii_digit() || (c == '.' && i + 1 < n && b[i + 1].is_ascii_digit()) {
            let start = i;
            while i < n {
                let ch = b[i] as char;
                if ch.is_ascii_digit() || ch == '.' {
                    i += 1;
                } else if (ch == 'e' || ch == 'E')
                    && i + 1 < n
                    && (b[i + 1].is_ascii_digit() || b[i + 1] == b'+' || b[i + 1] == b'-')
                {
                    // 科学计数法：吃掉 e/E，可选正负号，再吃掉指数数字
                    i += 1;
                    if b[i] == b'+' || b[i] == b'-' {
                        i += 1;
                    }
                    while i < n && b[i].is_ascii_digit() {
                        i += 1;
                    }
                } else {
                    break;
                }
            }
            let s = std::str::from_utf8(&b[start..i]).map_err(|_| "bad number".to_string())?;
            let v: f64 = s.parse().map_err(|_| format!("bad number: {s}"))?;
            out.push(Tok::Num(v));
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < n && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            let s = std::str::from_utf8(&b[start..i]).unwrap().to_string();
            out.push(Tok::Ident(s));
            continue;
        }
        // 运算符
        let two = if i + 1 < n { &b[i..i + 2] } else { &b[i..i + 1] };
        let op: &'static str = match two {
            b">=" => ">=",
            b"<=" => "<=",
            b"==" => "==",
            b"!=" => "!=",
            b"&&" => "&&",
            b"||" => "||",
            _ => match c {
                '>' => ">",
                '<' => "<",
                '!' => "!",
                '+' => "+",
                '-' => "-",
                '*' => "*",
                '/' => "/",
                '(' => "(",
                ')' => ")",
                ',' => ",",
                _ => return Err(format!("unexpected character: {c}")),
            },
        };
        out.push(Tok::Op(op));
        i += if matches!(two, b">=" | b"<=" | b"==" | b"!=" | b"&&" | b"||") && two.len() == 2 {
            2
        } else {
            1
        };
    }
    Ok(out)
}

// ---------------- 语法分析（递归下降） ----------------

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn new(toks: Vec<Tok>) -> Self {
        Parser { toks, pos: 0 }
    }

    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn expect_op(&mut self, op: &str) -> Result<(), String> {
        match self.next() {
            Some(Tok::Op(o)) if o == op => Ok(()),
            other => Err(format!("expected '{op}', got {other:?}")),
        }
    }

    fn parse(&mut self) -> Result<Expr, String> {
        let e = self.parse_or()?;
        if self.pos != self.toks.len() {
            return Err(format!("trailing tokens at {}", self.pos));
        }
        Ok(e)
    }

    fn parse_or(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_and()?;
        loop {
            let is_or = match self.peek() {
                Some(Tok::Op("||")) => true,
                Some(Tok::Ident(s)) => *s == "or",
                _ => false,
            };
            if is_or {
                self.next();
                let right = self.parse_and()?;
                left = Expr::Binary(BinOp::Or, Box::new(left), Box::new(right));
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_not()?;
        loop {
            let is_and = match self.peek() {
                Some(Tok::Op("&&")) => true,
                Some(Tok::Ident(s)) => *s == "and",
                _ => false,
            };
            if is_and {
                self.next();
                let right = self.parse_not()?;
                left = Expr::Binary(BinOp::And, Box::new(left), Box::new(right));
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> Result<Expr, String> {
        match self.peek() {
            Some(Tok::Op("!")) => {
                self.next();
                Ok(Expr::Unary(UnOp::Not, Box::new(self.parse_not()?)))
            }
            Some(Tok::Ident(s)) if *s == "not" => {
                self.next();
                Ok(Expr::Unary(UnOp::Not, Box::new(self.parse_not()?)))
            }
            _ => self.parse_cmp(),
        }
    }

    fn parse_cmp(&mut self) -> Result<Expr, String> {
        let left = self.parse_add()?;
        let op = match self.peek() {
            Some(Tok::Op(">")) => {
                self.next();
                BinOp::Gt
            }
            Some(Tok::Op("<")) => {
                self.next();
                BinOp::Lt
            }
            Some(Tok::Op(">=")) => {
                self.next();
                BinOp::Ge
            }
            Some(Tok::Op("<=")) => {
                self.next();
                BinOp::Le
            }
            Some(Tok::Op("==")) => {
                self.next();
                BinOp::Eq
            }
            Some(Tok::Op("!=")) => {
                self.next();
                BinOp::Ne
            }
            _ => return Ok(left),
        };
        let right = self.parse_add()?;
        Ok(Expr::Binary(op, Box::new(left), Box::new(right)))
    }

    fn parse_add(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_mul()?;
        loop {
            match self.peek() {
                Some(Tok::Op("+")) => {
                    self.next();
                    let r = self.parse_mul()?;
                    left = Expr::Binary(BinOp::Add, Box::new(left), Box::new(r));
                }
                Some(Tok::Op("-")) => {
                    self.next();
                    let r = self.parse_mul()?;
                    left = Expr::Binary(BinOp::Sub, Box::new(left), Box::new(r));
                }
                _ => break,
            }
        }
        Ok(left)
    }

    fn parse_mul(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_unary()?;
        loop {
            match self.peek() {
                Some(Tok::Op("*")) => {
                    self.next();
                    let r = self.parse_unary()?;
                    left = Expr::Binary(BinOp::Mul, Box::new(left), Box::new(r));
                }
                Some(Tok::Op("/")) => {
                    self.next();
                    let r = self.parse_unary()?;
                    left = Expr::Binary(BinOp::Div, Box::new(left), Box::new(r));
                }
                _ => break,
            }
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expr, String> {
        match self.peek() {
            Some(Tok::Op("-")) => {
                self.next();
                Ok(Expr::Unary(UnOp::Neg, Box::new(self.parse_unary()?)))
            }
            Some(Tok::Op("+")) => {
                self.next();
                self.parse_unary()
            }
            _ => self.parse_primary(),
        }
    }

    fn parse_primary(&mut self) -> Result<Expr, String> {
        match self.next() {
            Some(Tok::Num(v)) => Ok(Expr::Num(v)),
            Some(Tok::Str(s)) => Ok(Expr::Str(s)),
            Some(Tok::Op("(")) => {
                let e = self.parse_or()?;
                self.expect_op(")")?;
                Ok(e)
            }
            Some(Tok::Ident(name)) => {
                if let Some(Tok::Op("(")) = self.peek() {
                    self.next();
                    let mut args = Vec::new();
                    if !matches!(self.peek(), Some(Tok::Op(")"))) {
                        loop {
                            args.push(self.parse_or()?);
                            match self.peek() {
                                Some(Tok::Op(",")) => {
                                    self.next();
                                }
                                _ => break,
                            }
                        }
                    }
                    self.expect_op(")")?;
                    Ok(Expr::Fun(name, args))
                } else {
                    Ok(Expr::Field(name))
                }
            }
            other => Err(format!("unexpected token: {other:?}")),
        }
    }
}

/// 解析表达式字符串为 AST（公开，供 `Store::query` 与测试使用）。
pub fn parse_expr(src: &str) -> Result<Expr, String> {
    let toks = tokenize(src)?;
    let mut p = Parser::new(toks);
    p.parse()
}

// ---------------- 求值 ----------------

#[derive(Debug, Clone)]
enum Val {
    Num(f64),
    Str(String),
    Bool(bool),
}

fn truthy(v: &Val) -> bool {
    match v {
        Val::Num(f) => !f.is_nan() && *f != 0.0,
        Val::Str(s) => !s.is_empty(),
        Val::Bool(b) => *b,
    }
}

fn num(v: &Val) -> Result<f64, String> {
    match v {
        Val::Num(f) => Ok(*f),
        _ => Err("expected number".to_string()),
    }
}

fn arith(op: BinOp, a: Val, b: Val) -> Result<Val, String> {
    let x = num(&a)?;
    let y = num(&b)?;
    Ok(Val::Num(match op {
        BinOp::Add => x + y,
        BinOp::Sub => x - y,
        BinOp::Mul => x * y,
        BinOp::Div => x / y,
        _ => unreachable!(),
    }))
}

fn compare(op: BinOp, a: Val, b: Val) -> Result<Val, String> {
    let res = match op {
        BinOp::Eq => match (&a, &b) {
            (Val::Num(x), Val::Num(y)) => x == y,
            (Val::Str(x), Val::Str(y)) => x == y,
            (Val::Bool(x), Val::Bool(y)) => x == y,
            _ => return Err("== type mismatch".to_string()),
        },
        BinOp::Ne => match (&a, &b) {
            (Val::Num(x), Val::Num(y)) => x != y,
            (Val::Str(x), Val::Str(y)) => x != y,
            (Val::Bool(x), Val::Bool(y)) => x != y,
            _ => return Err("!= type mismatch".to_string()),
        },
        BinOp::Gt | BinOp::Lt | BinOp::Ge | BinOp::Le => {
            let x = num(&a)?;
            let y = num(&b)?;
            match op {
                BinOp::Gt => x > y,
                BinOp::Lt => x < y,
                BinOp::Ge => x >= y,
                BinOp::Le => x <= y,
                _ => false,
            }
        }
        _ => unreachable!(),
    };
    Ok(Val::Bool(res))
}

/// 窗口函数规格（绑定阶段收集，查询阶段按此顺序预计算数组）。
struct WinSpec {
    fun: String,
    field: usize, // 列下标
    k: usize,
}

/// 把 AST 中的 `Field(name)` 解析为 `Col(idx, name)`，并把窗口函数
/// `ma/roc/ref(field, k)` 解析为 `Win(win_idx)`（同时收集规格到 `wins`）。
/// 绑定只在解析后做一次：之后 `eval` 直接按下标取列值 / 取预计算窗口数组，
/// 彻底消除每行的 HashMap 查找与键分配。
fn bind(e: &mut Expr, idx: &HashMap<String, usize>, wins: &mut Vec<WinSpec>) -> Result<(), String> {
    match e {
        Expr::Field(name) => {
            let i = *idx
                .get(name)
                .ok_or_else(|| format!("unknown field: {name}"))?;
            *e = Expr::Col(i, name.clone());
            Ok(())
        }
        Expr::Fun(name, args) => {
            if matches!(name.as_str(), "ma" | "roc" | "ref") {
                let field = match args.first() {
                    Some(Expr::Field(f)) => {
                        *idx.get(f).ok_or_else(|| format!("unknown field: {f}"))?
                    }
                    _ => return Err(format!("{name} first argument must be a field")),
                };
                let k = if args.len() >= 2 {
                    match args.get(1) {
                        Some(Expr::Num(v)) => *v as usize,
                        _ => return Err(format!("{name} window size must be a number")),
                    }
                } else {
                    1
                };
                // 相同 (fun, field, k) 只预计算一次（去重）。
                let wi = match wins
                    .iter()
                    .position(|w| w.fun == *name && w.field == field && w.k == k)
                {
                    Some(p) => p,
                    None => {
                        wins.push(WinSpec {
                            fun: name.clone(),
                            field,
                            k,
                        });
                        wins.len() - 1
                    }
                };
                *e = Expr::Win(wi);
                Ok(())
            } else {
                for a in args {
                    bind(a, idx, wins)?;
                }
                Ok(())
            }
        }
        Expr::Unary(_, x) => bind(x, idx, wins),
        Expr::Binary(_, l, r) => {
            bind(l, idx, wins)?;
            bind(r, idx, wins)
        }
        _ => Ok(()),
    }
}

// ---------------- 字节级取值（eval 直接在 mmap 字节上取列值） ----------------

/// 从整行字节 `row`（len == rlen）按列偏移+类型取出一个查询值 `Val`。
/// 与 `decode_row` 的类型处理严格一致：F64 的 NaN 保留为 `Val::Num(NaN)`；
/// 空槽行不会进入 eval，故此处无需 Null 分支。
fn read_col(row: &[u8], off_kind: &(usize, FieldKind)) -> Val {
    let (off, kind) = *off_kind;
    match kind {
        FieldKind::F64 => {
            let mut a = [0u8; 8];
            a.copy_from_slice(&row[off..off + 8]);
            Val::Num(f64::from_le_bytes(a))
        }
        FieldKind::T => {
            let mut a = [0u8; 8];
            a.copy_from_slice(&row[off..off + 8]);
            Val::Num(i64::from_le_bytes(a) as f64)
        }
        FieldKind::Bool => Val::Bool(row[off] != 0),
        FieldKind::Str(w) => {
            let raw = &row[off..off + w];
            let end = raw.iter().position(|&c| c == 0).unwrap_or(w);
            let s = std::str::from_utf8(&raw[..end])
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            Val::Str(s)
        }
        FieldKind::Present => unreachable!(),
    }
}

/// 从「列起始」字节切片（len >= 8）按类型读出 `f64`，供窗口函数序列构建。
/// F64 -> 原值；T(i64) -> as f64；其余(Bool/Str) -> NaN（与旧 `compute_windows` 的
/// `match Value` 分支等价：F64->f, I64->as f64, else NaN）。
fn read_col_f64(row: &[u8], kind: FieldKind) -> f64 {
    match kind {
        FieldKind::F64 | FieldKind::T => {
            let mut a = [0u8; 8];
            a.copy_from_slice(&row[..8]);
            match kind {
                FieldKind::F64 => f64::from_le_bytes(a),
                _ => i64::from_le_bytes(a) as f64,
            }
        }
        _ => f64::NAN,
    }
}

/// 字节级 eval 的上下文：当前行字节 + 列偏移表 + 预计算窗口数组 + 非空行序号。
struct ByteCtx<'a> {
    row: &'a [u8],
    cols: &'a [(usize, FieldKind)],
    windows: &'a [Vec<f64>],
    t: usize, // 非空行序号（与窗口数组索引对齐，与旧实现语义一致）
}

/// 字节级求值：与 `eval` 逻辑完全一致，唯一区别是 `Col(i)` 直接读行字节（零每行 `Vec<Value>` 分配），
/// 字符串列仅在谓词引用时才按需解码。绑定后不存在 `Field` 节点。
fn eval_byte(e: &Expr, ctx: &ByteCtx) -> Result<Val, String> {
    match e {
        Expr::Num(v) => Ok(Val::Num(*v)),
        Expr::Str(s) => Ok(Val::Str(s.clone())),
        Expr::Field(name) => Err(format!("internal: unbound field {name} in byte eval")),
        Expr::Col(i, _) => Ok(read_col(ctx.row, &ctx.cols[*i])),
        Expr::Win(wi) => Ok(Val::Num(
            ctx.windows
                .get(*wi)
                .and_then(|a| a.get(ctx.t))
                .copied()
                .unwrap_or(f64::NAN),
        )),
        Expr::Unary(op, x) => {
            let v = eval_byte(x, ctx)?;
            match op {
                UnOp::Neg => match v {
                    Val::Num(f) => Ok(Val::Num(-f)),
                    _ => Err("neg on non-number".to_string()),
                },
                UnOp::Not => Ok(Val::Bool(!truthy(&v))),
            }
        }
        Expr::Binary(op, l, r) => match op {
            BinOp::And => Ok(Val::Bool(truthy(&eval_byte(l, ctx)?) && truthy(&eval_byte(r, ctx)?))),
            BinOp::Or => Ok(Val::Bool(truthy(&eval_byte(l, ctx)?) || truthy(&eval_byte(r, ctx)?))),
            BinOp::Eq | BinOp::Ne | BinOp::Gt | BinOp::Lt | BinOp::Ge | BinOp::Le => {
                let a = eval_byte(l, ctx)?;
                let b = eval_byte(r, ctx)?;
                compare(*op, a, b)
            }
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => {
                let a = eval_byte(l, ctx)?;
                let b = eval_byte(r, ctx)?;
                arith(*op, a, b)
            }
        },
        Expr::Fun(name, args) => match name.as_str() {
            "abs" => {
                let v = eval_byte(&args[0], ctx)?;
                match v {
                    Val::Num(f) => Ok(Val::Num(f.abs())),
                    _ => Err("abs needs a number".to_string()),
                }
            }
            "min" => {
                let a = eval_byte(&args[0], ctx)?;
                let b = eval_byte(&args[1], ctx)?;
                Ok(Val::Num(num(&a)?.min(num(&b)?)))
            }
            "max" => {
                let a = eval_byte(&args[0], ctx)?;
                let b = eval_byte(&args[1], ctx)?;
                Ok(Val::Num(num(&a)?.max(num(&b)?)))
            }
            other => Err(format!("unknown function: {other}")),
        },
    }
}

// ---------------- 窗口函数预计算 ----------------

/// 对一个 code 的某字段序列预计算窗口函数（ma/roc/ref）。
fn compute_windows(series: &[f64], fun: &str, k: usize) -> Vec<f64> {
    let len = series.len();
    let mut out = vec![f64::NAN; len];
    match fun {
        "ma" => {
            let mut sum = 0.0;
            for i in 0..len {
                sum += series[i];
                if i >= k {
                    sum -= series[i - k];
                }
                if i >= k - 1 {
                    out[i] = sum / (k as f64);
                }
            }
        }
        "roc" => {
            for i in k..len {
                let base = series[i - k];
                if base != 0.0 && !base.is_nan() {
                    out[i] = series[i] / base - 1.0;
                }
            }
        }
        "ref" => {
            for i in k..len {
                out[i] = series[i - k];
            }
        }
        _ => {}
    }
    out
}

// ---------------- 查询入口 ----------------

/// 字节级共享内核：在 `store` 上对 `table` 执行 DSL，**直接在 mmap 字节上逐行 eval**，
/// 对每个命中行调用 `on_hit(code, k, row_bytes)`（`row_bytes` 即该行的定长 stride 字节）。
/// [`query`]（JSON，按需 decode 命中行）与 [`query_bin`]（二进制，直接 memcpy 原行）共用此循环。
///
/// 相比旧实现（先 `decode_all` 把整文件解成 `Vec<Value>` 再 eval）：
/// - 彻底消除「每行 `Vec<Value>` + 字符串字段堆分配」，eval 只按列偏移读 `f64`/`i64`/`bool`；
/// - 命中后 `query_bin` 零编码（一次 memcpy 原行），`query`（JSON）才 decode 命中行；
/// - 选择性查询（命中极少却要扫全表）收益最大：省掉全部未命中行的解码与分配。
/// 窗口序列按「非空行序号」索引（与旧实现语义一致：空槽被视为不存在，不参与滑动）。
fn scan_eval<F>(store: &Store, table: &str, expr: &str, mut on_hit: F) -> Result<(), String>
where
    F: FnMut(&str, usize, &[u8]),
{
    let mut ast = parse_expr(expr)?;

    let codes = store.codes(table).map_err(|e| e.to_string())?;
    // 复用全表共享 schema：字段名下标 + 列字节偏移（避免每次查询重建 HashMap 与累加偏移）。
    let schema = crate::layout::schema_ref(table)
        .ok_or_else(|| format!("unknown table: {table}"))?;
    let idx = &schema.index;
    let cols = &schema.offsets;
    let rlen = crate::layout::record_len(table)
        .ok_or_else(|| format!("unknown table: {table}"))?;
    // 解析后把字段名绑定成列下标、把窗口函数绑定成数组下标（eval 不再每行做 HashMap 查找）。
    let mut win_specs: Vec<WinSpec> = Vec::new();
    bind(&mut ast, idx, &mut win_specs)?;

    for code in &codes {
        let mmap = store.mmap_of(table, code).map_err(|e| e.to_string())?;
        let n = mmap.len() / rlen;
        if n == 0 {
            continue;
        }

        // 一遍扫描：收集非空行（present=1）的全局 t，并同时构建被引用窗口列的 f64 序列
        //（直接从字节读列值，零 `Vec<Value>` 中间表示）。
        let mut present_ts: Vec<usize> = Vec::with_capacity(n);
        let mut win_maps: Vec<Vec<f64>> = win_specs.iter().map(|_| Vec::with_capacity(n)).collect();
        for t in 0..n {
            if mmap[t * rlen] == 0 {
                continue; // 空槽跳过（与旧 decode_all 行为一致）
            }
            present_ts.push(t);
            for (wi, w) in win_specs.iter().enumerate() {
                let (off, kind) = cols[w.field];
                win_maps[wi].push(read_col_f64(&mmap[t * rlen + off..], kind));
            }
        }
        // 按绑定顺序预计算每个窗口函数数组（每个 code 仅算一次，序列按非空行序号索引）。
        for wi in 0..win_specs.len() {
            let spec = &win_specs[wi];
            win_maps[wi] = compute_windows(&win_maps[wi], &spec.fun, spec.k);
        }

        // 逐非空行字节级 eval（k = 非空行序号，与窗口数组索引对齐）。
        for (k, &gt) in present_ts.iter().enumerate() {
            let row = &mmap[gt * rlen..(gt + 1) * rlen];
            let ctx = ByteCtx {
                row,
                cols,
                windows: &win_maps,
                t: k,
            };
            match eval_byte(&ast, &ctx) {
                Ok(v) if truthy(&v) => on_hit(code, k, row),
                Ok(_) => {}
                Err(e) => return Err(format!("{code} @t{gt}: {e}")),
            }
        }
    }
    Ok(())
}

/// 在 `store` 上对 `table` 执行 DSL 表达式，返回所有命中行的 JSON 数组字符串。
///
/// 每个命中行是一个 JSON 对象，含 `code` / `t` / 以及该表全部字段值（空槽/NaN 以 `null` 表示）。
/// 字节级内核只在命中的少数行上 `decode_row` 物化，未命中行零解码、零分配。
pub fn query(store: &Store, table: &str, expr: &str) -> Result<String, String> {
    let mut results: Vec<J> = Vec::new();
    scan_eval(store, table, expr, |code, _k, row| {
        let rec = crate::layout::decode_row(table, row)
            .expect("present row must decode");
        let mut obj = serde_json::Map::new();
        obj.insert("code".to_string(), J::String(code.to_string()));
        obj.insert("t".to_string(), J::Number((rec.t as i64).into()));
        for (i, (name, _)) in rec.layout.iter().enumerate() {
            if name == "code" || name == "t" {
                continue;
            }
            let jv = match &rec.fields[i] {
                Value::F64(f) => {
                    if f.is_nan() {
                        J::Null
                    } else {
                        J::Number(
                            serde_json::Number::from_f64(*f)
                                .unwrap_or_else(|| serde_json::Number::from_f64(0.0).unwrap()),
                        )
                    }
                }
                Value::I64(x) => J::Number((*x).into()),
                Value::Str(s) => J::String(s.clone()),
                Value::Bool(b) => J::Bool(*b),
                Value::Null => J::Null,
            };
            obj.insert(name.clone(), jv);
        }
        results.push(J::Object(obj));
    })?;
    serde_json::to_string(&results).map_err(|e| e.to_string())
}

/// 在 `store` 上对 `table` 执行 DSL，返回命中行的**原始二进制**缓冲。
///
/// 缓冲区布局（小端）：
/// ```text
/// [0..4]   magic      = 0x53544231 ("STB1")
/// [4..8]   record_len : u32   单行字节数（= CONTRACT §3.4）
/// [8..16]  n_hits     : u64   命中行数
/// [16..24] schema_hash: u64   字段布局指纹（= `crate::layout::schema_hash(table)`）
/// [24..]   n_hits × record_len 字节，每行即 CONTRACT §4 定长 stride 编码
///         （present + 字段，与 `.dat` 单行同构）
/// ```
///
/// `code` / `t` 已编码在行内（分别为首字段 / 第二字段），调用端按 §4 自行解码，
/// 无需再经 JSON。相比 [`query`]：零 serde 序列化、类型保真（f64/i64/bool 原样）、
/// 体积更小，适合宽查询 / 性能关键路径。
///
/// 字节级内核命中后**直接 memcpy 原行定长字节**（零 `Vec<Value>` 解码、零 `encode_row`
/// 重编码）——相比旧实现把已解码的 `Record` 再 `clone` + 重新 `encode` 回字节，省掉整段
/// 中间表示。跨语言入口见 `ffi::stockdb_query_bin`（C ABI，同构；调用方须用
/// `stockdb_free_buf` 释放）。
pub fn query_bin(store: &Store, table: &str, expr: &str) -> Result<Vec<u8>, String> {
    let rlen = crate::layout::record_len(table)
        .ok_or_else(|| format!("unknown table: {table}"))?;
    let shash = crate::layout::schema_hash(table);
    let mut buf: Vec<u8> = Vec::with_capacity(24 + 4096);
    // header 占位（record_len / n_hits / schema_hash 后置回填）
    let mut header = [0u8; 24];
    header[0..4].copy_from_slice(&0x5354_4231u32.to_le_bytes()); // "STB1"
    buf.extend_from_slice(&header);

    scan_eval(store, table, expr, |_code, _k, row| {
        // 命中行即定长 stride 字节，直接 memcpy（与 `.dat` 单行同构，调用端按 §4 解码）。
        buf.extend_from_slice(row);
    })?;

    // 回填 header
    let n_hits = ((buf.len() - 24) / rlen) as u64;
    buf[4..8].copy_from_slice(&(rlen as u32).to_le_bytes());
    buf[8..16].copy_from_slice(&n_hits.to_le_bytes());
    buf[16..24].copy_from_slice(&shash.to_le_bytes());
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Record, Store, Value};
    use std::sync::Arc;

    /// 造一个临时 root（进程唯一，避免并行测试互相污染），返回路径。
    fn tmp_root(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("stockdb_test_{}_{}_{}", tag, std::process::id(), line!()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 造 2 只票各 3 天 RawDailyBar：close = A[10,20,30] / B[40,50,60]，其余字段填占位。
    /// 落盘后日历含 3 个交易日，每只票 .dat 撑满 3 行。
    fn make_store(root: &std::path::Path) {
        // Store::open 要求 calendar.json 已存在；先 seed 一个空日历，
        // write 内部的 cal.ensure 会把用到的日期补进全局日历。
        std::fs::write(root.join("calendar.json"), "[]").unwrap();
        let store = Store::open(root).unwrap();
        let layout: Arc<[(String, char)]> = crate::layout::record_layout("RawDailyBar").unwrap();
        let dates = ["2024-01-01", "2024-01-02", "2024-01-03"];
        let specs: &[(&str, [f64; 3])] =
            &[("000001", [10.0, 20.0, 30.0]), ("000002", [40.0, 50.0, 60.0])];
        for (code, closes) in specs {
            let mut recs = Vec::with_capacity(3);
            for (i, d) in dates.iter().enumerate() {
                let c = closes[i];
                let fields = vec![
                    Value::Str(code.to_string()),
                    Value::I64(0), // t 由 write 按 date 经日历 ensure 重算
                    Value::Str(d.to_string()),
                    Value::F64(c),
                    Value::F64(c),
                    Value::F64(c),
                    Value::F64(c),
                    Value::F64(1000.0),
                    Value::F64(c * 1000.0),
                    Value::F64(0.5),
                ];
                recs.push(Record {
                    t: 0,
                    date: d.to_string(),
                    fields,
                    layout: layout.clone(),
                });
            }
            store.write("RawDailyBar", code, &recs, None).unwrap();
        }
    }

    /// 从 query（JSON）结果数命中行数。
    fn hit_count_json(store: &Store, expr: &str) -> usize {
        let json = store.query("RawDailyBar", expr).unwrap();
        serde_json::from_str::<serde_json::Value>(&json)
            .unwrap()
            .as_array()
            .unwrap()
            .len()
    }

    /// 从 query_bin 缓冲 header [8..16] 读 n_hits（u64 小端）。
    fn hit_count_bin(store: &Store, expr: &str) -> usize {
        let buf = store.query_bin("RawDailyBar", expr).unwrap();
        u64::from_le_bytes(buf[8..16].try_into().unwrap()) as usize
    }

    #[test]
    fn parse_and_collect_windows() {
        let e = parse_expr("close>10 && ma(volume,5)>100 || roc(close,20)<-0.1").unwrap();
        let mut idx = HashMap::new();
        idx.insert("close".to_string(), 0);
        idx.insert("volume".to_string(), 1);
        let mut wins: Vec<WinSpec> = Vec::new();
        let mut ast = e;
        bind(&mut ast, &idx, &mut wins).unwrap();
        assert!(wins.iter().any(|w| w.fun == "ma" && w.field == 1 && w.k == 5));
        assert!(wins.iter().any(|w| w.fun == "roc" && w.field == 0 && w.k == 20));
    }

    /// 生产查询路径（scan_eval）的自包含回归：不依赖 python / 外部 fixture。
    /// 同时校验 query（JSON）与 query_bin（字节）命中数完全一致——二者共用同一内核。
    #[test]
    fn scan_eval_self_contained() {
        let root = tmp_root("scan");
        make_store(&root);
        let store = Store::open(&root).unwrap();

        // 简单谓词：close>25 -> A(30) + B(40,50,60) = 4 行
        assert_eq!(hit_count_json(&store, "close>25"), 4);
        assert_eq!(hit_count_bin(&store, "close>25"), 4);

        // 窗口函数：ma(close,2)<close（单调递增 -> 每票 t1,t2 命中）= 4 行
        assert_eq!(hit_count_json(&store, "ma(close,2)<close"), 4);
        assert_eq!(hit_count_bin(&store, "ma(close,2)<close"), 4);

        // 组合：close>25 && ma(close,2)<close = 3 行（A.t2 + B.t1 + B.t2）
        assert_eq!(hit_count_json(&store, "close>25 && ma(close,2)<close"), 3);

        // JSON 与 BIN 必须同源一致（同一 scan_eval 内核，仅回写方式不同）
        assert_eq!(
            hit_count_json(&store, "close>25 && ma(close,2)<close"),
            hit_count_bin(&store, "close>25 && ma(close,2)<close")
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 校验命中行的字段值被字节级 eval 正确读出（close / code）。
    #[test]
    fn scan_eval_field_values() {
        let root = tmp_root("field");
        make_store(&root);
        let store = Store::open(&root).unwrap();

        // close>47 && close<53 仅命中 000002 的 d1（close=50）
        let json = store.query("RawDailyBar", "close>47 && close<53").unwrap();
        let arr = serde_json::from_str::<Vec<serde_json::Value>>(&json).unwrap();
        assert_eq!(arr.len(), 1);
        let obj = arr[0].as_object().unwrap();
        assert_eq!(obj.get("code").unwrap().as_str().unwrap(), "000002");
        assert!((obj.get("close").unwrap().as_f64().unwrap() - 50.0).abs() < 1e-9);

        let _ = std::fs::remove_dir_all(&root);
    }
}
