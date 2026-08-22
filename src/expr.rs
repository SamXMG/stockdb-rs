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

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::time::Instant;

use rayon::prelude::*;
use serde_json::Value as J;

use crate::layout::FieldKind;
use crate::{Store, Value};

// ---------------- AST ----------------

#[derive(Debug, Clone)]
enum Expr {
    Num(f64),
    Str(String),
    Field(String),
    /// 解析后绑定：列下标（在 schema 字段序列中的位置）。
    Col(usize),
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
        let two = if i + 1 < n {
            &b[i..i + 2]
        } else {
            &b[i..i + 1]
        };
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

/// 解析表达式字符串为 AST（内部供查询和公式编译复用）。
fn parse_expr(src: &str) -> Result<Expr, String> {
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
#[derive(Debug, Clone, PartialEq, Eq)]
struct WinSpec {
    fun: String,
    fields: Vec<usize>, // 一个字段，或 atr 的 high/low/close 三字段
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
            *e = Expr::Col(i);
            Ok(())
        }
        Expr::Fun(name, args) => {
            let single_window = matches!(
                name.as_str(),
                "ma" | "ema" | "sum" | "std" | "highest" | "lowest" | "roc" | "ref" | "rsi"
            );
            if single_window || name == "atr" {
                let (fields, k_arg, min_args, max_args) = if name == "atr" {
                    let mut fields = Vec::with_capacity(3);
                    for pos in 0..3 {
                        let field = match args.get(pos) {
                            Some(Expr::Field(f)) => {
                                *idx.get(f).ok_or_else(|| format!("unknown field: {f}"))?
                            }
                            _ => {
                                return Err(
                                    "atr arguments must be atr(high,low,close,n)".to_string()
                                )
                            }
                        };
                        fields.push(field);
                    }
                    (fields, 3usize, 4usize, 4usize)
                } else {
                    let field = match args.first() {
                        Some(Expr::Field(f)) => {
                            *idx.get(f).ok_or_else(|| format!("unknown field: {f}"))?
                        }
                        _ => return Err(format!("{name} first argument must be a field")),
                    };
                    let min_args = if name == "ref" { 1 } else { 2 };
                    (vec![field], 1usize, min_args, 2usize)
                };
                if args.len() < min_args || args.len() > max_args {
                    return Err(format!("invalid argument count for {name}"));
                }
                let k = if args.len() > k_arg {
                    match args.get(k_arg) {
                        Some(Expr::Num(v)) if v.is_finite() && *v >= 1.0 && v.fract() == 0.0 => {
                            *v as usize
                        }
                        _ => return Err(format!("{name} window size must be a positive integer")),
                    }
                } else {
                    1
                };
                // 相同 (fun, field, k) 只预计算一次（去重）。
                let wi = match wins
                    .iter()
                    .position(|w| w.fun == *name && w.fields == fields && w.k == k)
                {
                    Some(p) => p,
                    None => {
                        wins.push(WinSpec {
                            fun: name.clone(),
                            fields,
                            k,
                        });
                        wins.len() - 1
                    }
                };
                *e = Expr::Win(wi);
                Ok(())
            } else {
                let expected = match name.as_str() {
                    "abs" | "sqrt" | "log" | "exp" => 1,
                    "min" | "max" => 2,
                    "clip" => 3,
                    other => return Err(format!("unknown function: {other}")),
                };
                if args.len() != expected {
                    return Err(format!("{name} expects {expected} arguments"));
                }
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
        FieldKind::Scaled(scale) => {
            let mut a = [0u8; 4];
            a.copy_from_slice(&row[off..off + 4]);
            let raw = i32::from_le_bytes(a);
            if raw == crate::layout::SCALED_NULL {
                Val::Num(f64::NAN)
            } else {
                Val::Num(raw as f64 / scale)
            }
        }
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
        FieldKind::Scaled(scale) => {
            let mut a = [0u8; 4];
            a.copy_from_slice(&row[..4]);
            let raw = i32::from_le_bytes(a);
            if raw == crate::layout::SCALED_NULL {
                f64::NAN
            } else {
                raw as f64 / scale
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
        Expr::Col(i) => Ok(read_col(ctx.row, &ctx.cols[*i])),
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
            BinOp::And => Ok(Val::Bool(
                truthy(&eval_byte(l, ctx)?) && truthy(&eval_byte(r, ctx)?),
            )),
            BinOp::Or => Ok(Val::Bool(
                truthy(&eval_byte(l, ctx)?) || truthy(&eval_byte(r, ctx)?),
            )),
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
            "sqrt" => Ok(Val::Num(num(&eval_byte(&args[0], ctx)?)?.sqrt())),
            "log" => Ok(Val::Num(num(&eval_byte(&args[0], ctx)?)?.ln())),
            "exp" => Ok(Val::Num(num(&eval_byte(&args[0], ctx)?)?.exp())),
            "clip" => {
                let value = num(&eval_byte(&args[0], ctx)?)?;
                let low = num(&eval_byte(&args[1], ctx)?)?;
                let high = num(&eval_byte(&args[2], ctx)?)?;
                Ok(Val::Num(value.clamp(low, high)))
            }
            other => Err(format!("unknown function: {other}")),
        },
    }
}

// ---------------- 窗口函数预计算 ----------------

fn rolling_sum_stats(series: &[f64], k: usize) -> (Vec<f64>, Vec<f64>, Vec<usize>) {
    let len = series.len();
    let mut sums = vec![0.0; len];
    let mut squares = vec![0.0; len];
    let mut invalids = vec![0usize; len];
    let mut sum = 0.0;
    let mut square = 0.0;
    let mut invalid = 0usize;
    for i in 0..len {
        let value = series[i];
        if value.is_finite() {
            sum += value;
            square += value * value;
        } else {
            invalid += 1;
        }
        if i >= k {
            let old = series[i - k];
            if old.is_finite() {
                sum -= old;
                square -= old * old;
            } else {
                invalid -= 1;
            }
        }
        sums[i] = sum;
        squares[i] = square;
        invalids[i] = invalid;
    }
    (sums, squares, invalids)
}

fn rolling_extreme(series: &[f64], k: usize, highest: bool) -> Vec<f64> {
    let mut out = vec![f64::NAN; series.len()];
    let mut deque: VecDeque<usize> = VecDeque::new();
    let (_, _, invalids) = rolling_sum_stats(series, k);
    for i in 0..series.len() {
        while deque.front().is_some_and(|&j| j + k <= i) {
            deque.pop_front();
        }
        if series[i].is_finite() {
            while let Some(&j) = deque.back() {
                let replace = if highest {
                    series[j] <= series[i]
                } else {
                    series[j] >= series[i]
                };
                if !replace {
                    break;
                }
                deque.pop_back();
            }
            deque.push_back(i);
        }
        if i + 1 >= k && invalids[i] == 0 {
            if let Some(&j) = deque.front() {
                out[i] = series[j];
            }
        }
    }
    out
}

/// 对一个 code 的字段序列预计算窗口函数。
fn compute_window(series: &[&[f64]], fun: &str, k: usize) -> Vec<f64> {
    let len = series.first().map_or(0, |x| x.len());
    let mut out = vec![f64::NAN; len];
    match fun {
        "ma" | "sum" | "std" => {
            let (sums, squares, invalids) = rolling_sum_stats(series[0], k);
            for i in k.saturating_sub(1)..len {
                if invalids[i] == 0 {
                    out[i] = match fun {
                        "ma" => sums[i] / k as f64,
                        "sum" => sums[i],
                        _ => {
                            let mean = sums[i] / k as f64;
                            (squares[i] / k as f64 - mean * mean).max(0.0).sqrt()
                        }
                    };
                }
            }
        }
        "ema" => {
            let alpha = 2.0 / (k as f64 + 1.0);
            let mut ema = f64::NAN;
            for (i, &value) in series[0].iter().enumerate() {
                if !value.is_finite() {
                    continue;
                }
                ema = if ema.is_finite() {
                    alpha * value + (1.0 - alpha) * ema
                } else {
                    value
                };
                out[i] = ema;
            }
        }
        "roc" => {
            for i in k..len {
                let base = series[0][i - k];
                let value = series[0][i];
                if base != 0.0 && base.is_finite() && value.is_finite() {
                    out[i] = value / base - 1.0;
                }
            }
        }
        "ref" => {
            for i in k..len {
                out[i] = series[0][i - k];
            }
        }
        "highest" => return rolling_extreme(series[0], k, true),
        "lowest" => return rolling_extreme(series[0], k, false),
        "rsi" => {
            if len <= k {
                return out;
            }
            let mut gain = 0.0;
            let mut loss = 0.0;
            for i in 1..=k {
                let change = series[0][i] - series[0][i - 1];
                if !change.is_finite() {
                    return out;
                }
                if change >= 0.0 {
                    gain += change;
                } else {
                    loss -= change;
                }
            }
            let mut avg_gain = gain / k as f64;
            let mut avg_loss = loss / k as f64;
            out[k] = if avg_loss == 0.0 {
                100.0
            } else {
                100.0 - 100.0 / (1.0 + avg_gain / avg_loss)
            };
            for i in k + 1..len {
                let change = series[0][i] - series[0][i - 1];
                if !change.is_finite() {
                    continue;
                }
                let g = change.max(0.0);
                let l = (-change).max(0.0);
                avg_gain = (avg_gain * (k as f64 - 1.0) + g) / k as f64;
                avg_loss = (avg_loss * (k as f64 - 1.0) + l) / k as f64;
                out[i] = if avg_loss == 0.0 {
                    100.0
                } else {
                    100.0 - 100.0 / (1.0 + avg_gain / avg_loss)
                };
            }
        }
        "atr" => {
            if series.len() != 3 || len <= k {
                return out;
            }
            let (high, low, close) = (series[0], series[1], series[2]);
            let mut tr = vec![f64::NAN; len];
            for i in 1..len {
                if high[i].is_finite() && low[i].is_finite() && close[i - 1].is_finite() {
                    tr[i] = (high[i] - low[i])
                        .max((high[i] - close[i - 1]).abs())
                        .max((low[i] - close[i - 1]).abs());
                }
            }
            let seed = &tr[1..=k];
            if seed.iter().any(|v| !v.is_finite()) {
                return out;
            }
            let mut atr = seed.iter().sum::<f64>() / k as f64;
            out[k] = atr;
            for i in k + 1..len {
                if tr[i].is_finite() {
                    atr = (atr * (k as f64 - 1.0) + tr[i]) / k as f64;
                    out[i] = atr;
                }
            }
        }
        _ => {}
    }
    out
}

// ---------------- 批量公式计算 ----------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormulaSpec {
    pub name: String,
    pub expression: String,
}

#[derive(Debug)]
struct CompiledFormulas {
    names: Vec<String>,
    asts: Vec<Expr>,
    wins: Vec<WinSpec>,
}

/// 公式 JSON 支持两种形式：
/// `{"formulas":[{"name":"ma20","expression":"ma(close,20)"}]}` 或
/// `{"ma20":"ma(close,20)"}`。
pub fn parse_formula_specs(raw: &str) -> Result<Vec<FormulaSpec>, String> {
    let root: J = serde_json::from_str(raw).map_err(|e| format!("invalid formulas json: {e}"))?;
    let value = root.get("formulas").unwrap_or(&root);
    let mut specs = Vec::new();
    match value {
        J::Array(items) => {
            for item in items {
                let obj = item
                    .as_object()
                    .ok_or_else(|| "formula item must be an object".to_string())?;
                let name = obj.get("name").and_then(J::as_str).unwrap_or("").trim();
                let expression = obj
                    .get("expression")
                    .or_else(|| obj.get("expr"))
                    .and_then(J::as_str)
                    .unwrap_or("")
                    .trim();
                if name.is_empty() || expression.is_empty() {
                    return Err("formula requires non-empty name and expression".to_string());
                }
                specs.push(FormulaSpec {
                    name: name.to_string(),
                    expression: expression.to_string(),
                });
            }
        }
        J::Object(map) => {
            for (name, expression) in map {
                let expression = expression
                    .as_str()
                    .ok_or_else(|| format!("formula {name} must be a string"))?;
                if name.trim().is_empty() || expression.trim().is_empty() {
                    return Err("formula requires non-empty name and expression".to_string());
                }
                specs.push(FormulaSpec {
                    name: name.clone(),
                    expression: expression.to_string(),
                });
            }
        }
        _ => return Err("formulas must be an object or array".to_string()),
    }
    if specs.is_empty() {
        return Err("at least one formula is required".to_string());
    }
    let mut names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
    names.sort_unstable();
    if names.windows(2).any(|w| w[0] == w[1]) {
        return Err("formula names must be unique".to_string());
    }
    Ok(specs)
}

fn compile_formulas(table: &str, specs: &[FormulaSpec]) -> Result<CompiledFormulas, String> {
    let schema =
        crate::layout::schema_ref(table).ok_or_else(|| format!("unknown table: {table}"))?;
    let mut wins = Vec::new();
    let mut asts = Vec::with_capacity(specs.len());
    for spec in specs {
        let mut ast = parse_expr(&spec.expression).map_err(|e| format!("{}: {e}", spec.name))?;
        bind(&mut ast, &schema.index, &mut wins).map_err(|e| format!("{}: {e}", spec.name))?;
        asts.push(ast);
    }
    Ok(CompiledFormulas {
        names: specs.iter().map(|s| s.name.clone()).collect(),
        asts,
        wins,
    })
}

fn formula_rows_compiled(
    store: &Store,
    table: &str,
    code: &str,
    compiled: &CompiledFormulas,
    t0: usize,
    t1: Option<usize>,
) -> Result<Vec<(u32, Vec<f32>)>, String> {
    let schema =
        crate::layout::schema_ref(table).ok_or_else(|| format!("unknown table: {table}"))?;
    let cols = &schema.offsets;
    let rlen = crate::layout::record_len(table).ok_or_else(|| format!("unknown table: {table}"))?;
    let mmap = store
        .mmap_of(table, code)
        .map_err(|e| format!("{code}: {e}"))?;
    let n = mmap.len() / rlen;
    let end = t1.unwrap_or(n).min(n);
    if t0 >= end {
        return Ok(Vec::new());
    }

    let mut present_ts = Vec::with_capacity(end - t0);
    let mut source_fields: Vec<usize> = compiled
        .wins
        .iter()
        .flat_map(|w| w.fields.iter().copied())
        .collect();
    source_fields.sort_unstable();
    source_fields.dedup();
    let source_pos: HashMap<usize, usize> = source_fields
        .iter()
        .enumerate()
        .map(|(pos, field)| (*field, pos))
        .collect();
    let mut sources: Vec<Vec<f64>> = source_fields
        .iter()
        .map(|_| Vec::with_capacity(n))
        .collect();

    // 窗口预热必须从文件起点开始，输出再按 [t0,t1) 截取，避免区间首部改变公式语义。
    for t in 0..end {
        if mmap[t * rlen] == 0 {
            continue;
        }
        present_ts.push(t);
        for (pos, field) in source_fields.iter().enumerate() {
            let (off, kind) = cols[*field];
            sources[pos].push(read_col_f64(&mmap[t * rlen + off..], kind));
        }
    }
    let windows: Vec<Vec<f64>> = compiled
        .wins
        .iter()
        .map(|spec| {
            let inputs: Vec<&[f64]> = spec
                .fields
                .iter()
                .map(|field| sources[source_pos[field]].as_slice())
                .collect();
            compute_window(&inputs, &spec.fun, spec.k)
        })
        .collect();

    let mut out = Vec::with_capacity(present_ts.len());
    for (k, &t) in present_ts.iter().enumerate() {
        if t < t0 {
            continue;
        }
        let row = &mmap[t * rlen..(t + 1) * rlen];
        let ctx = ByteCtx {
            row,
            cols,
            windows: &windows,
            t: k,
        };
        let mut values = Vec::with_capacity(compiled.asts.len());
        for (name, ast) in compiled.names.iter().zip(&compiled.asts) {
            let value =
                match eval_byte(ast, &ctx).map_err(|e| format!("{code} @t{t} {name}: {e}"))? {
                    Val::Num(v) if v.is_finite() => v as f32,
                    Val::Bool(v) => {
                        if v {
                            1.0
                        } else {
                            0.0
                        }
                    }
                    Val::Num(_) => f32::NAN,
                    Val::Str(_) => {
                        return Err(format!(
                            "{code} @t{t} {name}: formula result must be numeric"
                        ))
                    }
                };
            values.push(value);
        }
        out.push((t as u32, values));
    }
    Ok(out)
}

pub fn compute_formula_rows(
    store: &Store,
    table: &str,
    code: &str,
    specs: &[FormulaSpec],
    t0: usize,
    t1: Option<usize>,
) -> Result<Vec<(u32, Vec<f32>)>, String> {
    let compiled = compile_formulas(table, specs)?;
    formula_rows_compiled(store, table, code, &compiled, t0, t1)
}

pub fn compute_formula_rows_json(
    store: &Store,
    table: &str,
    code: &str,
    formulas_json: &str,
    t0: usize,
    t1: Option<usize>,
) -> Result<String, String> {
    let specs = parse_formula_specs(formulas_json)?;
    let rows = compute_formula_rows(store, table, code, &specs, t0, t1)?;
    let cal = store.calendar();
    let items: Vec<J> = rows
        .into_iter()
        .map(|(t, values)| {
            let mut obj = serde_json::Map::new();
            obj.insert("t".to_string(), J::Number((t as u64).into()));
            obj.insert(
                "date".to_string(),
                cal.t_to_date(t as usize)
                    .map_or(J::Null, |d| J::String(d.to_string())),
            );
            for (spec, value) in specs.iter().zip(values) {
                obj.insert(
                    spec.name.clone(),
                    if value.is_nan() {
                        J::Null
                    } else {
                        J::Number(serde_json::Number::from_f64(value as f64).unwrap())
                    },
                );
            }
            J::Object(obj)
        })
        .collect();
    serde_json::to_string(&items).map_err(|e| e.to_string())
}

/// 多股票并行计算并直接写紧凑矩阵。Rust 全程读取、计算和写盘，宿主不搬运中间行。
pub fn compute_formulas_to_compact(
    store: &Store,
    table: &str,
    specs: &[FormulaSpec],
    codes: Option<&[String]>,
    out_dir: &Path,
) -> Result<String, String> {
    let started = Instant::now();
    let compiled = compile_formulas(table, specs)?;
    let selected = match codes {
        Some(items) => {
            let mut values = items.to_vec();
            values.sort();
            values.dedup();
            if let Some(bad) = values.iter().find(|code| {
                code.is_empty()
                    || !code
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            }) {
                return Err(format!("invalid code for compact output: {bad}"));
            }
            values
        }
        None => store.codes(table).map_err(|e| e.to_string())?,
    };
    std::fs::create_dir_all(out_dir).map_err(|e| e.to_string())?;
    let results: Result<Vec<(usize, u64)>, String> = selected
        .par_iter()
        .map(|code| {
            let rows = formula_rows_compiled(store, table, code, &compiled, 0, None)?;
            let path = out_dir.join(format!("{code}.mtx"));
            crate::compact::write_file(&path, &compiled.names, &rows)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            let bytes = std::fs::metadata(&path).map_err(|e| e.to_string())?.len();
            Ok((rows.len(), bytes))
        })
        .collect();
    let results = results?;
    let rows: usize = results.iter().map(|x| x.0).sum();
    let bytes: u64 = results.iter().map(|x| x.1).sum();
    serde_json::to_string(&serde_json::json!({
        "table": table,
        "files": results.len(),
        "rows": rows,
        "columns": compiled.names,
        "bytes": bytes,
        "elapsed_ms": started.elapsed().as_millis(),
        "output": out_dir.to_string_lossy(),
    }))
    .map_err(|e| e.to_string())
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
    let schema =
        crate::layout::schema_ref(table).ok_or_else(|| format!("unknown table: {table}"))?;
    let idx = &schema.index;
    let cols = &schema.offsets;
    let rlen = crate::layout::record_len(table).ok_or_else(|| format!("unknown table: {table}"))?;
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
        let mut source_fields: Vec<usize> = win_specs
            .iter()
            .flat_map(|w| w.fields.iter().copied())
            .collect();
        source_fields.sort_unstable();
        source_fields.dedup();
        let source_pos: HashMap<usize, usize> = source_fields
            .iter()
            .enumerate()
            .map(|(pos, field)| (*field, pos))
            .collect();
        let mut sources: Vec<Vec<f64>> = source_fields
            .iter()
            .map(|_| Vec::with_capacity(n))
            .collect();
        for t in 0..n {
            if mmap[t * rlen] == 0 {
                continue; // 空槽跳过（与旧 decode_all 行为一致）
            }
            present_ts.push(t);
            for (pos, field) in source_fields.iter().enumerate() {
                let (off, kind) = cols[*field];
                sources[pos].push(read_col_f64(&mmap[t * rlen + off..], kind));
            }
        }
        // 按绑定顺序预计算每个窗口函数数组（每个 code 仅算一次，序列按非空行序号索引）。
        let win_maps: Vec<Vec<f64>> = win_specs
            .iter()
            .map(|spec| {
                let inputs: Vec<&[f64]> = spec
                    .fields
                    .iter()
                    .map(|field| sources[source_pos[field]].as_slice())
                    .collect();
                compute_window(&inputs, &spec.fun, spec.k)
            })
            .collect();

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
        let rec = crate::layout::decode_row(table, row).expect("present row must decode");
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
    let rlen = crate::layout::record_len(table).ok_or_else(|| format!("unknown table: {table}"))?;
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
        let dir = std::env::temp_dir().join(format!(
            "stockdb_test_{}_{}_{}",
            tag,
            std::process::id(),
            line!()
        ));
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
        let specs: &[(&str, [f64; 3])] = &[
            ("000001", [10.0, 20.0, 30.0]),
            ("000002", [40.0, 50.0, 60.0]),
        ];
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
        assert!(wins
            .iter()
            .any(|w| w.fun == "ma" && w.fields == vec![1] && w.k == 5));
        assert!(wins
            .iter()
            .any(|w| w.fun == "roc" && w.fields == vec![0] && w.k == 20));
    }

    #[test]
    fn formula_engine_computes_shared_windows() {
        let root = tmp_root("formula");
        make_store(&root);
        let store = Store::open(&root).unwrap();
        let specs = vec![
            FormulaSpec {
                name: "ma2".into(),
                expression: "ma(close,2)".into(),
            },
            FormulaSpec {
                name: "roc1".into(),
                expression: "roc(close,1)".into(),
            },
            FormulaSpec {
                name: "atr2".into(),
                expression: "atr(high,low,close,2)".into(),
            },
            FormulaSpec {
                name: "z".into(),
                expression: "(close-ma(close,2))/std(close,2)".into(),
            },
        ];
        let rows = compute_formula_rows(&store, "RawDailyBar", "000001", &specs, 0, None).unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows[0].1[0].is_nan());
        assert!((rows[1].1[0] - 15.0).abs() < 1e-6);
        assert!((rows[2].1[0] - 25.0).abs() < 1e-6);
        assert!((rows[2].1[1] - 0.5).abs() < 1e-6);
        assert!((rows[2].1[2] - 10.0).abs() < 1e-6);
        assert!((rows[2].1[3] - 1.0).abs() < 1e-6);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn formula_engine_rejects_unsafe_windows() {
        let specs = vec![FormulaSpec {
            name: "bad".into(),
            expression: "ma(close,0)".into(),
        }];
        assert!(compile_formulas("RawDailyBar", &specs)
            .unwrap_err()
            .contains("positive integer"));
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
