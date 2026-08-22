//! 数据库视图 —— 语言中立派生/聚合契约（视图能力，类似 SQL VIEW）。
//!
//! 输入 raw 表 + 事件表，输出确定性派生数据，不产生 IO、不碰网络：
//!   - hfq / qfq：前/后复权日K（基于 RawDailyBar + AdjustEvent）
//!   - qfq_at：回测专用严格前视隔离的前复权单根
//!   - weekly/monthly：周/月K 聚合（可选先 qfq 再聚合，价格连续）

use num::BigInt;
use num::BigRational;
use num::Signed;
use num::ToPrimitive;

/// raw 日K 的一根 bar (价格 + 成交量 + 日期 + 全局 t)。
#[derive(Debug, Clone)]
pub struct RawBar {
    pub date: String,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

/// 派生后的 bar (视图输出, 结构同 raw)。
#[derive(Debug, Clone)]
pub struct Bar {
    pub date: String,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

/// 分红送股事件。
#[derive(Debug, Clone)]
pub struct AdjustEvent {
    pub ex_date: String,
    pub bonus_per_share: f64,
    pub cash_per_share: f64,
}

fn sorted_events(events: &[AdjustEvent]) -> Vec<AdjustEvent> {
    let mut v: Vec<AdjustEvent> = events.to_vec();
    v.sort_by(|a, b| a.ex_date.cmp(&b.ex_date));
    v
}

/// 分红/送股系数按 1e-6 精度取精确整数微元：恢复其语义小数（0.3→300000），
/// 而非保留 f64 的 bit 误差。A股分红/送股系数通常 ≤ 2 位小数，1e-6 足够无歧义。
fn to_micros(x: f64) -> i128 {
    (x * 1_000_000.0).round() as i128
}

/// 价格按分(1e-2)取精确整数分。A股价格 2 位小数，×100 后 round 精确。
fn to_cents(x: f64) -> i128 {
    (x * 100.0).round() as i128
}

/// 精确复权：价格(分,整数) × 因子(有理数) → 四舍五入回分 → f64 元。
/// 全程整数运算，round-half-up；不引入任何浮点乘法，确定性按构造保证。
fn adj_to_f64(price_c: i128, f: &BigRational) -> f64 {
    let num = BigInt::from(price_c) * f.numer(); // 精确整数乘积
    let den = f.denom().clone(); // 正
    let sign = num.signum();
    let abs = num.abs();
    let two = BigInt::from(2);
    // round half up：对正数 floor((2*abs + den) / (2*den)) 即四舍五入到最近整数
    let rounded = (&abs * &two + &den) / (&den * &two);
    let cents = sign * rounded;
    cents.to_f64().unwrap_or(f64::NAN) / 100.0
}

/// 构造有理数（i128 入参，自动装箱为 BigInt）。
fn rat(n: i128, d: i128) -> BigRational {
    BigRational::new(BigInt::from(n), BigInt::from(d))
}

/// 前向累积后（前）复权因子序列 (与 bars 等长)，**精确到构造**（BigRational）。
/// cum(t) = Π_{ex<=t}(1+bonus) / Π_{ex<=t}(1 + cash/ex_close)
/// 输入系数先转为精确十进制整数（to_micros），价格 ex_close 转为精确分（to_cents），
/// 故整条因子链无任何 f64 乘法，结果确定且可复现（不受运算次序/并行影响）。
pub fn build_hfq_cum_factors(bars: &[RawBar], events: &[AdjustEvent]) -> Vec<BigRational> {
    let evs = sorted_events(events);
    let close_by_date: std::collections::HashMap<&str, f64> =
        bars.iter().map(|b| (b.date.as_str(), b.close)).collect();

    let mut cum = rat(1, 1);
    let mut factors = Vec::with_capacity(bars.len());
    let mut ev_idx = 0;
    let n_ev = evs.len();
    for b in bars {
        while ev_idx < n_ev && evs[ev_idx].ex_date <= b.date {
            let e = &evs[ev_idx];
            let bonus_m = to_micros(e.bonus_per_share);
            let cash_m = to_micros(e.cash_per_share);
            let ex_close_c = to_cents(
                close_by_date
                    .get(e.ex_date.as_str())
                    .copied()
                    .unwrap_or(b.close),
            );
            // 1 + bonus = (1e6 + bonus_m) / 1e6
            cum *= rat(1_000_000 + bonus_m, 1_000_000);
            if cash_m != 0 && ex_close_c != 0 {
                // 1 + cash/ex_close = 1 + (cash_m/1e6)/(ex_close_c/100)
                // = 1 + cash_m*100/(ex_close_c*1e6)
                // = (ex_close_c*1e6 + cash_m*100) / (ex_close_c*1e6)
                // 注意：复权因子取该式的倒数（原实现 cum /= ...），故分子分母对调。
                let den = ex_close_c * 1_000_000;
                let num = den + cash_m * 100;
                cum *= rat(den, num);
            }
            ev_idx += 1;
        }
        factors.push(cum.clone());
    }
    factors
}

/// raw -> 后复权 (锚定上市日, 历史价反映真实增值)。
pub fn derive_hfq(bars: &[RawBar], events: &[AdjustEvent]) -> Vec<Bar> {
    let factors = build_hfq_cum_factors(bars, events);
    bars.iter()
        .zip(factors.iter())
        .map(|(b, f)| Bar {
            date: b.date.clone(),
            open: adj_to_f64(to_cents(b.open), f),
            high: adj_to_f64(to_cents(b.high), f),
            low: adj_to_f64(to_cents(b.low), f),
            close: adj_to_f64(to_cents(b.close), f),
            volume: b.volume,
        })
        .collect()
}

/// raw -> 前复权 (锚定最新日, 最新价 == raw 最新价; 严格无未来除权)。
pub fn derive_qfq(bars: &[RawBar], events: &[AdjustEvent]) -> Vec<Bar> {
    let factors = build_hfq_cum_factors(bars, events);
    if factors.is_empty() {
        return Vec::new();
    }
    let latest = factors[factors.len() - 1].clone();
    bars.iter()
        .zip(factors.iter())
        .map(|(b, f)| {
            let q = if *latest.numer() != BigInt::from(0) {
                f / &latest
            } else {
                rat(1, 1)
            };
            Bar {
                date: b.date.clone(),
                open: adj_to_f64(to_cents(b.open), &q),
                high: adj_to_f64(to_cents(b.high), &q),
                low: adj_to_f64(to_cents(b.low), &q),
                close: adj_to_f64(to_cents(b.close), &q),
                volume: b.volume,
            }
        })
        .collect()
}

/// 回测专用: 严格只用 [0,T] 窗口派生第 T 根前复权价 (零前视)。
pub fn derive_qfq_at(bars: &[RawBar], events: &[AdjustEvent], t: usize) -> Option<Bar> {
    if t >= bars.len() {
        return None;
    }
    let evs: Vec<AdjustEvent> = events
        .iter()
        .filter(|e| e.ex_date <= bars[t].date)
        .cloned()
        .collect();
    let sub = &bars[..t + 1];
    let factors = build_hfq_cum_factors(sub, &evs);
    let anchor = factors.last().cloned().unwrap_or_else(|| rat(1, 1));
    let b = &bars[t];
    // 锚定 T 日：因子 = anchor/anchor = 1（精确），故返回即 raw 价（已按分四舍五入还原）。
    let q = if *anchor.numer() != BigInt::from(0) {
        &anchor / &anchor
    } else {
        rat(1, 1)
    };
    Some(Bar {
        date: b.date.clone(),
        open: adj_to_f64(to_cents(b.open), &q),
        high: adj_to_f64(to_cents(b.high), &q),
        low: adj_to_f64(to_cents(b.low), &q),
        close: adj_to_f64(to_cents(b.close), &q),
        volume: b.volume,
    })
}

fn period_key(date: &str, period: &str) -> String {
    // 周期键：week→周一 YYYY-MM-DD；month→YYYY-MM（升序分桶，聚合稳定）。
    use chrono::Datelike;
    let d = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").unwrap();
    match period {
        "week" => {
            let weekday = d.weekday().num_days_from_monday() as i64;
            let monday = d - chrono::Days::new(weekday as u64);
            monday.format("%Y-%m-%d").to_string()
        }
        "month" => d.format("%Y-%m").to_string(),
        _ => panic!("未知周期: {period}"),
    }
}

/// raw 日K -> 周K/月K (按自然周期重采样)。
/// 若传 events 先派生 qfq 再聚合(价格连续); 否则用 raw(含除权跳变)。
pub fn aggregate_period(bars: &[RawBar], period: &str, events: Option<&[AdjustEvent]>) -> Vec<Bar> {
    if bars.is_empty() {
        return Vec::new();
    }
    let price: Vec<Bar> = match events {
        Some(ev) => {
            let q = derive_qfq(bars, ev);
            bars.iter()
                .zip(q.iter())
                .map(|(b, s)| Bar {
                    date: b.date.clone(),
                    open: s.open,
                    high: s.high,
                    low: s.low,
                    close: s.close,
                    volume: b.volume,
                })
                .collect()
        }
        None => bars
            .iter()
            .map(|b| Bar {
                date: b.date.clone(),
                open: b.open,
                high: b.high,
                low: b.low,
                close: b.close,
                volume: b.volume,
            })
            .collect(),
    };

    use std::collections::HashMap;
    let mut groups: HashMap<String, Vec<Bar>> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for pb in &price {
        let k = period_key(&pb.date, period);
        if !groups.contains_key(&k) {
            order.push(k.clone());
        }
        groups.entry(k).or_default().push(pb.clone());
    }
    let mut out = Vec::new();
    for k in &order {
        let g = &groups[k];
        out.push(Bar {
            date: k.clone(),
            open: g[0].open,
            high: g.iter().map(|x| x.high).fold(f64::MIN, f64::max),
            low: g.iter().map(|x| x.low).fold(f64::MAX, f64::min),
            close: g[g.len() - 1].close,
            volume: g.iter().map(|x| x.volume).sum(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        if a.is_nan() && b.is_nan() {
            return true;
        }
        (a - b).abs() < 1e-9
    }

    fn bar(date: &str, o: f64, h: f64, l: f64, c: f64, v: f64) -> RawBar {
        RawBar {
            date: date.into(),
            open: o,
            high: h,
            low: l,
            close: c,
            volume: v,
        }
    }

    #[test]
    fn cum_factors_basic() {
        // 无事件: 因子恒 1
        let bars = vec![bar("2023-01-03", 10.0, 10.5, 9.8, 10.2, 1.0)];
        assert_eq!(build_hfq_cum_factors(&bars, &[]), vec![rat(1, 1)]);
    }

    #[test]
    fn qfq_at_zero_lookahead() {
        // 站在 T 日看, T 日价格应 == raw (qfq_at 以 T 为锚)
        let bars = vec![
            bar("2023-01-03", 10.0, 10.5, 9.8, 10.0, 1.0),
            bar("2023-01-04", 11.0, 11.5, 10.8, 11.0, 1.0),
        ];
        let evs = vec![AdjustEvent {
            ex_date: "2023-01-05".into(),
            bonus_per_share: 0.5,
            cash_per_share: 0.0,
        }];
        // T=0: 事件在 01-05, 晚于 01-03, 不应影响 -> q=1
        let at0 = derive_qfq_at(&bars, &evs, 0).unwrap();
        assert!(approx(at0.close, 10.0));
        // T=1: 事件仍在未来(01-05 > 01-04) -> q=1, 价格 = raw
        let at1 = derive_qfq_at(&bars, &evs, 1).unwrap();
        assert!(approx(at1.close, 11.0));
    }

    #[test]
    fn qfq_anchor_latest() {
        // 无事件时 qfq == raw
        let bars = vec![
            bar("2023-01-03", 10.0, 10.5, 9.8, 10.0, 1.0),
            bar("2023-01-04", 11.0, 11.5, 10.8, 11.0, 1.0),
        ];
        let q = derive_qfq(&bars, &[]);
        assert!(approx(q[0].close, 10.0));
        assert!(approx(q[1].close, 11.0));
        // 最新日 qfq == raw 最新
        assert!(approx(q[1].close, bars[1].close));
    }

    // —— 交易系统「优秀」入场券：复权正确性 + 确定性 ——
    // 本仓无交易所/同花顺真值，故以「手写推导的 documented oracle」作真值：
    //   (a) 复权价四舍五入到分与 oracle 一致（正确性，非 1e-9 近似容忍）；
    //   (b) 同输入两次派生逐 bit 相同（回测可重放所需的确定性）。
    // 这暴露出 ①(金额定点) 与 ②(复权对齐) 的耦合：当前复权因子是 f64 累积乘/除，
    // 跨平台确定性已满足（顺序固定 + IEEE754），但「与参考源逐笔一致」必须把因子链改为
    // 定点/有理数——那是 ① 计算层的工作。此测试先把"可证明正确 + 可重放"这条线钉死。

    #[test]
    fn qfq_hfq_cent_exact_oracle() {
        // 场景：01-04 现金分红 1 元/股，ex_close = 11.0
        //   hfq cum: 01-03 = 1.0；01-04 = 1/(1 + 1/11) = 11/12
        //   hfq close: 01-03 = 10.00；01-04 = 11 * 11/12 = 10.083333..
        //   qfq 锚定最新(11/12): 01-03 = 10 * 12/11 = 10.909090..；01-04 = 11.00
        let bars = vec![
            bar("2023-01-03", 10.0, 10.5, 9.8, 10.0, 1.0),
            bar("2023-01-04", 11.0, 11.5, 10.8, 11.0, 1.0),
        ];
        let evs = vec![AdjustEvent {
            ex_date: "2023-01-04".into(),
            bonus_per_share: 0.0,
            cash_per_share: 1.0,
        }];
        let hfq = derive_hfq(&bars, &evs);
        let qfq = derive_qfq(&bars, &evs);
        let cents = |x: f64| (x * 100.0).round() as i64; // 四舍五入到分
        assert_eq!(cents(hfq[0].close), 1000); // 10.00
        assert_eq!(cents(hfq[1].close), 1008); // 10.08
        assert_eq!(cents(qfq[0].close), 1091); // 10.91
        assert_eq!(cents(qfq[1].close), 1100); // 11.00
    }

    #[test]
    fn qfq_hfq_exact_decimal() {
        // 双重事件链：01-04 送股 0.5(10送5)、01-05 现金分红 2 元(ex_close=30)。
        // 精确有理数：cum = 1 → 3/2 → (3/2)*(15/16)=45/32。
        //   hfq 真值：10, 30, 30*45/32=42.1875 → 入分 42.19
        //   qfq 锚 45/32：10*32/45=7.111111..→7.11, 20*16/15=21.3333..→21.33, 30
        // 以下对"入分后的精确价"做 bit 级断言，证明有理数计算 → 正确入分（非仅近似）。
        let bars = vec![
            bar("2023-01-03", 10.0, 10.5, 9.8, 10.0, 1.0),
            bar("2023-01-04", 20.0, 20.5, 19.8, 20.0, 1.0),
            bar("2023-01-05", 30.0, 30.5, 29.8, 30.0, 1.0),
        ];
        let evs = vec![
            AdjustEvent {
                ex_date: "2023-01-04".into(),
                bonus_per_share: 0.5,
                cash_per_share: 0.0,
            },
            AdjustEvent {
                ex_date: "2023-01-05".into(),
                bonus_per_share: 0.0,
                cash_per_share: 2.0,
            },
        ];
        let hfq = derive_hfq(&bars, &evs);
        let qfq = derive_qfq(&bars, &evs);
        let c = |n: i64| (n as f64) / 100.0; // 入分价（与 adj_to_f64 同构，bit 级一致）
        assert_eq!(hfq[0].close, c(1000));
        assert_eq!(hfq[1].close, c(3000));
        assert_eq!(hfq[2].close, c(4219)); // 42.1875 入分
        assert_eq!(qfq[0].close, c(711)); // 7.111111.. 入分
        assert_eq!(qfq[1].close, c(2133)); // 21.3333.. 入分
        assert_eq!(qfq[2].close, c(3000));
    }

    #[test]
    fn adjustment_is_deterministic() {
        // 回测可重放要求：同输入两次派生逐 bit 相同。
        let bars = vec![
            bar("2023-01-03", 10.0, 10.5, 9.8, 10.0, 1.0),
            bar("2023-01-04", 11.0, 11.5, 10.8, 11.0, 1.0),
            bar("2023-01-05", 10.5, 10.7, 10.2, 10.4, 1.0),
        ];
        let evs = vec![
            AdjustEvent {
                ex_date: "2023-01-04".into(),
                bonus_per_share: 0.5,
                cash_per_share: 0.0,
            },
            AdjustEvent {
                ex_date: "2023-01-05".into(),
                bonus_per_share: 0.0,
                cash_per_share: 2.0,
            },
        ];
        let a = derive_hfq(&bars, &evs);
        let b = derive_hfq(&bars, &evs);
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(x.open.to_bits(), y.open.to_bits());
            assert_eq!(x.high.to_bits(), y.high.to_bits());
            assert_eq!(x.low.to_bits(), y.low.to_bits());
            assert_eq!(x.close.to_bits(), y.close.to_bits());
        }
    }
}
