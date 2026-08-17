//! 回测场景示例：用 stockdb-rs 跑一个最小回测循环。
//!
//! 流程：
//!   1. 启动校验 `validate`（防静默错读）
//!   2. 取某票某区间历史 `read_range`（连续切片，零拷贝）
//!   3. 叠加复权事件，前复权（严格前视隔离，回测安全）
//!   4. 遍历每个交易日执行策略
//!
//! 运行: cargo run --example backtest -- /path/to/stockdb_root 600000

use stockdb_rs::layout::Value;
use stockdb_rs::view::{derive_qfq, AdjustEvent, RawBar};
use stockdb_rs::Store;

fn f64_of(v: &Value) -> f64 {
    match v {
        Value::F64(f) => *f,
        Value::Null => f64::NAN,
        _ => 0.0,
    }
}

fn main() -> std::io::Result<()> {
    let root = std::env::args().nth(1).expect("need root arg");
    let code = std::env::args().nth(2).unwrap_or_else(|| "600000".into());

    let store = Store::open(&root)?;

    // 1) 回测前校验数据完整性
    store.validate("RawDailyBar", &code)?;
    store.validate("AdjustEvent", &code)?;

    // 2) 取区间历史（如 t in [100, 200)），连续切片零拷贝
    let bars_raw: Vec<RawBar> = store
        .read_range("RawDailyBar", &code, 100, 200)?
        .into_iter()
        .map(|r| RawBar {
            date: match r.get("RawDailyBar", "date") {
                Some(Value::Str(s)) => s.clone(),
                _ => String::new(),
            },
            open: f64_of(r.get("RawDailyBar", "open").unwrap()),
            high: f64_of(r.get("RawDailyBar", "high").unwrap()),
            low: f64_of(r.get("RawDailyBar", "low").unwrap()),
            close: f64_of(r.get("RawDailyBar", "close").unwrap()),
            volume: f64_of(r.get("RawDailyBar", "volume").unwrap()),
        })
        .collect();

    let events: Vec<AdjustEvent> = store
        .read("AdjustEvent", &code)?
        .into_iter()
        .map(|r| AdjustEvent {
            ex_date: match r.get("AdjustEvent", "ex_date") {
                Some(Value::Str(s)) => s.clone(),
                _ => String::new(),
            },
            bonus_per_share: f64_of(r.get("AdjustEvent", "bonus_per_share").unwrap()),
            cash_per_share: f64_of(r.get("AdjustEvent", "cash_per_share").unwrap()),
        })
        .collect();

    // 3) 严格前视复权（回测安全：只用 [0,T] 窗口）
    let bars = derive_qfq(&bars_raw, &events);

    // 4) 回测主循环：每个交易日执行策略
    let mut prev_close = f64::NAN;
    let mut signals = 0usize;
    for b in &bars {
        // 示例策略：今日收盘 > 昨日收盘 即记一次信号
        if !prev_close.is_nan() && b.close > prev_close {
            signals += 1;
        }
        prev_close = b.close;
    }

    println!(
        "回测 {code}: {}-{} 区间共 {} 根 bar, 产生 {} 次买入信号",
        bars.first().map(|b| b.date.as_str()).unwrap_or("-"),
        bars.last().map(|b| b.date.as_str()).unwrap_or("-"),
        bars.len(),
        signals
    );
    Ok(())
}
