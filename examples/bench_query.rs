//! 查询引擎基准测试：用 Store::write 造 N 只 × M 天合成 RawDailyBar，
//! 计时三类代表性查询，打印命中数与耗时（毫秒）。
//!
//! 运行：`cargo run --release --example bench_query [small|full]`
//!   默认 small = 300×1500 ≈ 45 万行；`full` = 710×2500 ≈ 180 万行（贴近真实规模）。
use std::time::Instant;

use stockdb_rs::{Store, Record, Value};

fn main() {
    let full = std::env::args().any(|a| a == "full");
    let (n_codes, m_days) = if full { (710, 2500) } else { (300, 1500) };
    println!("== bench_query: N={n_codes} codes × M={m_days} days = {} rows ==", n_codes * m_days);

    let dir = std::env::temp_dir().join("sb_bench_query");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Store::open 要求已存在 calendar.json（空数组即可，write 会按需扩展）。
    std::fs::write(dir.join("calendar.json"), "[]").unwrap();
    let store = Store::open(&dir).unwrap();

    // RawDailyBar 字段顺序（与 layout::TABLE_FIELDS 严格一致）
    let layout: std::sync::Arc<[(String, char)]> = std::sync::Arc::from(vec![
        ("code".to_string(), 's'), ("t".to_string(), 'q'), ("date".to_string(), 's'),
        ("open".to_string(), 'd'), ("high".to_string(), 'd'), ("low".to_string(), 'd'),
        ("close".to_string(), 'd'), ("volume".to_string(), 'd'), ("amount".to_string(), 'd'),
        ("turnover".to_string(), 'd'),
    ]);

    // 确定性伪随机（LCG），保证可复现。
    // 注意：必须用全 64 位做归一化，不能用 `rng >> 33` 当分子（会被压进 [0, 2^-33]≈0，
    // 导致数据全挤在字段下限，使选择性查询退化为 0 命中、基准失真）。
    let mut rng: u64 = 0x9E3779B97F4A7C15;
    let mut rnd = || {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (rng as f64) / (u64::MAX as f64)
    };

    let t0 = Instant::now();
    for c in 0..n_codes {
        let code = format!("SZ{:06}", c);
        let mut recs = Vec::with_capacity(m_days);
        for d in 0..m_days {
            let close = 5.0 + rnd() * 45.0;          // 5..50
            let open = close * (0.98 + rnd() * 0.04);
            let high = close * (1.0 + rnd() * 0.03);
            let low = close * (0.97 + rnd() * 0.03);
            let volume = 1e5 + rnd() * 1e7;           // 1e5..1.01e7
            let amount = volume * close;
            let turnover = rnd() * 0.1;               // 0..0.1
            let date = format!("D{:06}", d);
            recs.push(Record {
                t: 0,
                date: date.clone(),
                fields: vec![
                    Value::Str(code.clone()),
                    Value::I64(d as i64),
                    Value::Str(date),
                    Value::F64(open),
                    Value::F64(high),
                    Value::F64(low),
                    Value::F64(close),
                    Value::F64(volume),
                    Value::F64(amount),
                    Value::F64(turnover),
                ],
                layout: layout.clone(),
            });
        }
        store.write("RawDailyBar", &code, &recs, Some(m_days)).unwrap();
    }
    println!("[setup] wrote {} files in {:.1?}", n_codes, t0.elapsed());

    let run = |_name: &str, expr: &str| {
        let t = Instant::now();
        let json = store.query("RawDailyBar", expr).unwrap();
        let el = t.elapsed();
        // 用 json 数组长度粗算命中数（避免解析整段，仅数顶层逗号 + 1）
        let hits = if json.starts_with('[') {
            json[1..].split("},").count()
        } else { 0 };
        println!("[query JSON] {:<42} => {:<8} hits in {:.2?}", expr, hits, el);
        (hits, el)
    };

    let run_bin = |_name: &str, expr: &str| {
        let rlen = stockdb_rs::layout::record_len("RawDailyBar").unwrap();
        let t = Instant::now();
        let buf = store.query_bin("RawDailyBar", expr).unwrap();
        let el = t.elapsed();
        let hits = ((buf.len() - 24) / rlen) as usize;
        println!("[query BIN ] {:<42} => {:<8} hits in {:.2?}", expr, hits, el);
        (hits, el)
    };

    println!("-- selective (few hits): eval/scan cost dominates --");
    run("Q1", "close>45 && ma(volume,5)>1e6");
    run_bin("Q1", "close>45 && ma(volume,5)>1e6");
    println!("-- broad (most rows hit): JSON materialization cost --");
    run("Q2", "close>0");
    run_bin("Q2", "close>0");
    println!("-- window-dependent predicate --");
    run("Q3", "ma(close,20)>close");
    run_bin("Q3", "ma(close,20)>close");

    println!("-- DIAG: Q1 子句拆解（BIN 精确计数，查 0 命中是否 bug） --");
    run_bin("D1 close>45", "close>45");
    run_bin("D2 ma(volume,5)>1e6", "ma(volume,5)>1e6");
    run_bin("D3 volume>1e6", "volume>1e6");
    run_bin("D4 close>45 && ma(volume,5)>1e6", "close>45 && ma(volume,5)>1e6");

    let _ = std::fs::remove_dir_all(&dir);
}
