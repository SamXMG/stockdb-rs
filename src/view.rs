//! 数据库视图 —— 语言中立派生/聚合契约（视图能力，类似 SQL VIEW）。
//!
//! 输入 raw 表 + 事件表，输出确定性派生数据，不产生 IO、不碰网络：
//!   - hfq / qfq：前/后复权日K（基于 RawDailyBar + AdjustEvent）
//!   - qfq_at：回测专用严格前视隔离的前复权单根
//!   - weekly/monthly：周/月K 聚合（可选先 qfq 再聚合，价格连续）

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

/// 前向累积后复权因子序列 (与 bars 等长)。
/// cum(t) = Π_{ex<=t}(1+bonus) / Π_{ex<=t}(1 + cash/ex_close)
pub fn build_hfq_cum_factors(bars: &[RawBar], events: &[AdjustEvent]) -> Vec<f64> {
    let evs = sorted_events(events);
    let close_by_date: std::collections::HashMap<&str, f64> =
        bars.iter().map(|b| (b.date.as_str(), b.close)).collect();

    let mut cum = 1.0f64;
    let mut factors = Vec::with_capacity(bars.len());
    let mut ev_idx = 0;
    let n_ev = evs.len();
    for b in bars {
        while ev_idx < n_ev && evs[ev_idx].ex_date <= b.date {
            let e = &evs[ev_idx];
            let bonus = e.bonus_per_share;
            let cash = e.cash_per_share;
            let ex_close = close_by_date.get(e.ex_date.as_str()).copied().unwrap_or(b.close);
            cum *= 1.0 + bonus;
            if cash != 0.0 && ex_close != 0.0 {
                cum /= 1.0 + cash / ex_close;
            }
            ev_idx += 1;
        }
        factors.push(cum);
    }
    factors
}

/// raw -> 后复权 (锚定上市日, 历史价反映真实增值)。
pub fn derive_hfq(bars: &[RawBar], events: &[AdjustEvent]) -> Vec<Bar> {
    let factors = build_hfq_cum_factors(bars, events);
    bars.iter().zip(factors.iter()).map(|(b, f)| Bar {
        date: b.date.clone(),
        open: b.open * f,
        high: b.high * f,
        low: b.low * f,
        close: b.close * f,
        volume: b.volume,
    }).collect()
}

/// raw -> 前复权 (锚定最新日, 最新价 == raw 最新价; 严格无未来除权)。
pub fn derive_qfq(bars: &[RawBar], events: &[AdjustEvent]) -> Vec<Bar> {
    let factors = build_hfq_cum_factors(bars, events);
    if factors.is_empty() {
        return Vec::new();
    }
    let latest = factors[factors.len() - 1];
    bars.iter().zip(factors.iter()).map(|(b, f)| {
        let q = if latest != 0.0 { f / latest } else { 1.0 };
        Bar {
            date: b.date.clone(),
            open: b.open * q,
            high: b.high * q,
            low: b.low * q,
            close: b.close * q,
            volume: b.volume,
        }
    }).collect()
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
    let anchor = *factors.last().unwrap_or(&1.0);
    let b = &bars[t];
    let q = if anchor != 0.0 { anchor / anchor } else { 1.0 };
    Some(Bar {
        date: b.date.clone(),
        open: b.open * q,
        high: b.high * q,
        low: b.low * q,
        close: b.close * q,
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
pub fn aggregate_period(
    bars: &[RawBar],
    period: &str,
    events: Option<&[AdjustEvent]>,
) -> Vec<Bar> {
    if bars.is_empty() {
        return Vec::new();
    }
    let price: Vec<Bar> = match events {
        Some(ev) => {
            let q = derive_qfq(bars, ev);
            bars.iter().zip(q.iter()).map(|(b, s)| Bar {
                date: b.date.clone(),
                open: s.open, high: s.high, low: s.low, close: s.close,
                volume: b.volume,
            }).collect()
        }
        None => bars.iter().map(|b| Bar {
            date: b.date.clone(),
            open: b.open, high: b.high, low: b.low, close: b.close,
            volume: b.volume,
        }).collect(),
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
        RawBar { date: date.into(), open: o, high: h, low: l, close: c, volume: v }
    }

    #[test]
    fn cum_factors_basic() {
        // 无事件: 因子恒 1
        let bars = vec![bar("2023-01-03", 10.0, 10.5, 9.8, 10.2, 1.0)];
        assert_eq!(build_hfq_cum_factors(&bars, &[]), vec![1.0]);
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
}

