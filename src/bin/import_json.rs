//! Rust 原生 JSON -> StockDB 导入器。
//!
//! Python/其他采集端只负责提供原始 JSON；本命令负责解析、schema 映射、日历对齐
//! 和通过 `Store`/`flow` 接口原子落盘。不会由宿主语言拼装 `.dat/.flow` 字节。
//!
//! 用法：
//!   stockdb_rs import_json snapshot <root> <snapshot-dir>
//!   stockdb_rs import_json company-profile <root> <snapshot-dir> <industry-history.json>
//!   stockdb_rs import_json money-flow <root> <d5d6-dir>
//!   stockdb_rs import_json cache <root> <FactorDaily|LabelDaily|SignalDaily> <jsonl>

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value as J};
use stockdb_rs::{compact, flow, layout, Record, Store, Value};

fn err(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn as_f64(v: Option<&J>) -> Value {
    match v {
        Some(J::Number(n)) => n.as_f64().map(Value::F64).unwrap_or(Value::Null),
        Some(J::String(s)) => s.parse::<f64>().map(Value::F64).unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

fn as_bool(v: Option<&J>) -> Value {
    match v {
        Some(J::Bool(b)) => Value::Bool(*b),
        Some(J::Number(n)) => Value::Bool(n.as_i64().unwrap_or(0) != 0),
        Some(J::String(s)) => Value::Bool(matches!(
            s.to_ascii_lowercase().as_str(),
            "1" | "true" | "是"
        )),
        _ => Value::Bool(false),
    }
}

fn as_str(v: Option<&J>) -> Value {
    match v {
        Some(J::String(s)) => Value::Str(s.clone()),
        Some(J::Null) | None => Value::Str(String::new()),
        Some(other) => Value::Str(other.to_string()),
    }
}

fn value_for(kind: layout::FieldKind, v: Option<&J>) -> Value {
    match kind {
        layout::FieldKind::Bool => as_bool(v),
        layout::FieldKind::Str(_) => as_str(v),
        layout::FieldKind::F64 | layout::FieldKind::Scaled(_) => as_f64(v),
        layout::FieldKind::T => match v {
            Some(J::Number(n)) => n
                .as_i64()
                .map(Value::I64)
                .or_else(|| n.as_f64().map(|x| Value::I64(x as i64)))
                .unwrap_or(Value::Null),
            Some(J::String(s)) => s.parse::<i64>().map(Value::I64).unwrap_or(Value::Null),
            _ => Value::Null,
        },
        layout::FieldKind::Present => Value::Null,
    }
}

fn record(table: &str, obj: &Map<String, J>, date: Option<&str>, t: i64) -> io::Result<Record> {
    let layout =
        layout::record_layout(table).ok_or_else(|| err(format!("unknown table {table}")))?;
    let kinds = layout::field_kinds(table).ok_or_else(|| err(format!("unknown table {table}")))?;
    let mut fields = Vec::with_capacity(kinds.len());
    for (name, kind) in kinds {
        fields.push(if name == "t" {
            Value::I64(t)
        } else {
            value_for(kind, obj.get(&name))
        });
    }
    Ok(Record {
        t,
        date: date.unwrap_or_default().to_string(),
        fields,
        layout,
    })
}

fn json_files(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir)? {
        let p = e?.path();
        if p.extension().and_then(|x| x.to_str()) == Some("json")
            && p.file_name().and_then(|x| x.to_str()) != Some("manifest.json")
        {
            out.push(p);
        }
    }
    out.sort();
    Ok(out)
}

fn date_from_file(path: &Path) -> io::Result<String> {
    path.file_stem()
        .and_then(|x| x.to_str())
        .map(str::to_string)
        .filter(|s| s.len() == 10 && s.as_bytes()[4] == b'-' && s.as_bytes()[7] == b'-')
        .ok_or_else(|| {
            err(format!(
                "snapshot filename must be YYYY-MM-DD: {}",
                path.display()
            ))
        })
}

fn snapshot_rows(raw: &J) -> Vec<&Map<String, J>> {
    let mut out = Vec::new();
    if let Some(arr) = raw.get("snaps").and_then(J::as_array) {
        out.extend(arr.iter().filter_map(J::as_object));
        return out;
    }
    if let Some(groups) = raw.as_object() {
        for value in groups.values() {
            if let Some(arr) = value.get("snaps").and_then(J::as_array) {
                out.extend(arr.iter().filter_map(J::as_object));
            }
        }
    }
    out
}

fn import_snapshot(root: &Path, dir: &Path) -> io::Result<()> {
    let mut by_code: BTreeMap<String, Vec<Record>> = BTreeMap::new();
    let mut rows = 0usize;
    for path in json_files(dir)? {
        let date = date_from_file(&path)?;
        let raw: J = serde_json::from_str(&fs::read_to_string(&path)?)
            .map_err(|e| err(format!("{}: {e}", path.display())))?;
        for obj in snapshot_rows(&raw) {
            let code = obj
                .get("code")
                .and_then(J::as_str)
                .unwrap_or("")
                .to_string();
            if code.is_empty() {
                continue;
            }
            by_code
                .entry(code)
                .or_default()
                .push(record("DailySnapshot", obj, Some(&date), 0)?);
            rows += 1;
        }
    }
    let store = Store::open(root)?;
    let mut files = 0usize;
    for (code, rows_for_code) in by_code {
        store.write("DailySnapshot", &code, &rows_for_code, None)?;
        store.write_meta("DailySnapshot", &code)?;
        files += 1;
    }
    println!(
        "snapshot import complete: files={} rows={} root={}",
        files,
        rows,
        root.display()
    );
    Ok(())
}

fn import_company_profile(root: &Path, snap_dir: &Path, history_path: &Path) -> io::Result<()> {
    let mut latest: HashMap<String, Map<String, J>> = HashMap::new();
    let mut latest_date = String::new();
    for path in json_files(snap_dir)? {
        let date = date_from_file(&path)?;
        if date < latest_date {
            continue;
        }
        let raw: J = serde_json::from_str(&fs::read_to_string(&path)?)
            .map_err(|e| err(format!("{}: {e}", path.display())))?;
        for obj in snapshot_rows(&raw) {
            if let Some(code) = obj.get("code").and_then(J::as_str) {
                latest.insert(code.to_string(), obj.clone());
            }
        }
        latest_date = date;
    }
    let hist: J = serde_json::from_str(&fs::read_to_string(history_path)?)
        .map_err(|e| err(format!("{}: {e}", history_path.display())))?;
    let records = hist
        .get("records")
        .and_then(J::as_object)
        .cloned()
        .unwrap_or_default();
    let store = Store::open(root)?;
    let mut codes: BTreeMap<String, Map<String, J>> = BTreeMap::new();
    for (code, rows) in records {
        let mut obj = latest.remove(&code).unwrap_or_default();
        if let Some(arr) = rows.as_array() {
            if let Some(last) = arr
                .iter()
                .filter_map(J::as_object)
                .max_by_key(|x| x.get("effective_from").and_then(J::as_str).unwrap_or(""))
            {
                if obj
                    .get("industry")
                    .and_then(J::as_str)
                    .unwrap_or("")
                    .is_empty()
                {
                    if let Some(ind) = last.get("industry") {
                        obj.insert("industry".into(), ind.clone());
                    }
                }
            }
        }
        obj.insert("code".into(), J::String(code.clone()));
        codes.insert(code, obj);
    }
    for (code, obj) in latest {
        codes.insert(code.clone(), obj);
    }
    let profile_count = codes.len();
    for (code, obj) in codes {
        let r = record("CompanyProfile", &obj, None, 0)?;
        store.write("CompanyProfile", &code, &[r], Some(1))?;
    }
    println!(
        "company-profile import complete: files={} root={}",
        profile_count,
        root.display()
    );
    Ok(())
}

fn import_money_flow(root: &Path, dir: &Path) -> io::Result<()> {
    let store = Store::open(root)?;
    let cal = store.calendar().clone();
    let out_dir = root.join("MoneyFlowHistory");
    fs::create_dir_all(&out_dir)?;
    let mut files = 0usize;
    let mut rows = 0usize;
    let mut skipped = 0usize;
    for path in json_files(dir)? {
        let code = path.file_stem().and_then(|x| x.to_str()).unwrap_or("");
        if code.is_empty() {
            continue;
        }
        let raw: J = serde_json::from_str(&fs::read_to_string(&path)?)
            .map_err(|e| err(format!("{}: {e}", path.display())))?;
        let mut out = Vec::new();
        if let Some(obj) = raw.as_object() {
            for (date, row) in obj {
                let Some(t) = cal.date_to_t(date) else {
                    skipped += 1;
                    continue;
                };
                let Some(m) = row.as_object() else {
                    continue;
                };
                let source = m
                    .get("source")
                    .and_then(J::as_str)
                    .unwrap_or("legacy_unknown");
                let n = |k: &str| m.get(k).and_then(J::as_f64).unwrap_or(f64::NAN);
                out.push(flow::FlowRow {
                    t: t as i64,
                    main_net: n("main_net"),
                    main_pct: n("main_pct"),
                    xl_net: n("xl_net"),
                    xl_pct: n("xl_pct"),
                    r0_net: n("r0_net"),
                    r0_pct: n("r0_pct"),
                    turnover: n("turnover"),
                    vol_ratio: n("vol_ratio"),
                    source: flow::source_id(source),
                });
            }
        }
        out.sort_by_key(|r| r.t);
        if !out.is_empty() {
            rows += out.len();
            files += 1;
            flow::write_file(&out_dir.join(format!("{code}.flow")), &out)?;
        }
    }
    println!(
        "money-flow import complete: files={} rows={} skipped_dates={} root={}",
        files,
        rows,
        skipped,
        root.display()
    );
    Ok(())
}

/// 导入回测缓存 JSONL。每行格式：
/// {"code":"600000", "date":"2026-08-21", ...字段...}
/// 记录按 code 分组后调用 Store::write；输入可由 Python/其他语言流式生成，
/// 但 schema 编码和 `.dat` 写入始终由 Rust 完成。
fn import_cache(root: &Path, table: &str, jsonl: &Path) -> io::Result<()> {
    if !matches!(table, "FactorDaily" | "LabelDaily" | "SignalDaily") {
        return Err(err(
            "cache table must be FactorDaily, LabelDaily or SignalDaily",
        ));
    }
    let input = BufReader::new(fs::File::open(jsonl)?);
    let mut skipped = 0usize;
    let store = Store::open(root)?;
    let cal = store.calendar().clone();
    let mut current_key = String::new();
    let mut current_rows: Vec<Record> = Vec::new();
    let mut files = 0usize;
    let mut rows = 0usize;

    let flush = |key: &str, records: &mut Vec<Record>| -> io::Result<()> {
        if key.is_empty() || records.is_empty() {
            return Ok(());
        }
        store.write(table, key, records, None)?;
        store.write_meta(table, key)?;
        records.clear();
        Ok(())
    };
    for (line_no, line) in input.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let obj: J = serde_json::from_str(&line)
            .map_err(|e| err(format!("{}:{}: {e}", jsonl.display(), line_no + 1)))?;
        let map = obj
            .as_object()
            .ok_or_else(|| err(format!("{}:{} is not object", jsonl.display(), line_no + 1)))?;
        let code = map.get("code").and_then(J::as_str).unwrap_or("");
        let date = map.get("date").and_then(J::as_str).unwrap_or("");
        if code.is_empty() || date.is_empty() {
            skipped += 1;
            continue;
        }
        let Some(t) = cal.date_to_t(date) else {
            // 缓存不能自行扩展历史日历，避免回测 t 漂移。
            skipped += 1;
            continue;
        };
        // 同一股票同一天会有多个动态因子/标签/策略。每个 ID 使用独立文件，
        // 保持每个文件仍是“一天一个槽位”，新增因子时无需改 schema 或重写其他因子。
        let dimension = match table {
            "FactorDaily" => map.get("factor_id").and_then(J::as_str).unwrap_or(""),
            "LabelDaily" => map.get("label_id").and_then(J::as_str).unwrap_or(""),
            "SignalDaily" => map.get("strategy_id").and_then(J::as_str).unwrap_or(""),
            _ => "",
        };
        if dimension.is_empty() {
            skipped += 1;
            continue;
        }
        let safe_id: String = dimension
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let version_suffix = if table == "SignalDaily" {
            let v = map.get("model_version").and_then(J::as_str).unwrap_or("");
            let safe_v: String = v
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            format!("__{}", safe_v)
        } else {
            String::new()
        };
        let file_key = format!("{}{}__{}", safe_id, version_suffix, code);
        if current_key != file_key {
            if !current_key.is_empty() && file_key < current_key {
                return Err(err(format!(
                    "{} is not sorted by dimension+code; key {} appeared after {}",
                    jsonl.display(),
                    file_key,
                    current_key
                )));
            }
            if !current_key.is_empty() {
                rows += current_rows.len();
                flush(&current_key, &mut current_rows)?;
                files += 1;
            }
            current_key = file_key;
        }
        current_rows.push(record(table, map, Some(date), t as i64)?);
    }
    if !current_key.is_empty() {
        rows += current_rows.len();
        flush(&current_key, &mut current_rows)?;
        files += 1;
    }
    println!(
        "cache import complete: table={} files={} rows={} skipped={} root={}",
        table,
        files,
        rows,
        skipped,
        root.display()
    );
    Ok(())
}

fn import_compact(root: &Path, kind: &str, jsonl: &Path, columns_arg: &str) -> io::Result<()> {
    let dir_name = match kind {
        "factor" => "CompactFactor",
        "label" => "CompactLabel",
        "signal" => "CompactSignal",
        _ => return Err(err("compact kind must be factor, label or signal")),
    };
    let columns: Vec<String> = columns_arg
        .split(',')
        .filter(|x| !x.is_empty())
        .map(str::to_string)
        .collect();
    if columns.is_empty() {
        return Err(err("compact matrix requires columns"));
    }
    let input = BufReader::new(fs::File::open(jsonl)?);
    let store = Store::open(root)?;
    let cal = store.calendar().clone();
    let out_dir = root.join(dir_name);
    let mut current_code = String::new();
    let mut current_rows: Vec<(u32, Vec<f32>)> = Vec::new();
    let mut files = 0usize;
    let mut rows = 0usize;
    let mut skipped = 0usize;
    let flush = |code: &str, rows_buf: &mut Vec<(u32, Vec<f32>)>| -> io::Result<()> {
        if code.is_empty() || rows_buf.is_empty() {
            return Ok(());
        }
        compact::write_file(&out_dir.join(format!("{code}.mtx")), &columns, rows_buf)?;
        rows_buf.clear();
        Ok(())
    };
    for (line_no, line) in input.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let obj: J = serde_json::from_str(&line)
            .map_err(|e| err(format!("{}:{}: {e}", jsonl.display(), line_no + 1)))?;
        let map = obj
            .as_object()
            .ok_or_else(|| err(format!("{}:{} is not object", jsonl.display(), line_no + 1)))?;
        let code = map.get("code").and_then(J::as_str).unwrap_or("");
        let date = map.get("date").and_then(J::as_str).unwrap_or("");
        if code.is_empty() || date.is_empty() {
            skipped += 1;
            continue;
        }
        let Some(t) = cal.date_to_t(date) else {
            skipped += 1;
            continue;
        };
        if !current_code.is_empty() && code < current_code.as_str() {
            return Err(err(format!("{} is not sorted by code", jsonl.display())));
        }
        if current_code != code {
            if !current_code.is_empty() {
                rows += current_rows.len();
                flush(&current_code, &mut current_rows)?;
                files += 1;
            }
            current_code = code.to_string();
        }
        let values = columns
            .iter()
            .map(|name| match map.get(name) {
                Some(J::Bool(v)) => {
                    if *v {
                        1.0
                    } else {
                        0.0
                    }
                }
                Some(J::Number(n)) => n.as_f64().unwrap_or(f64::NAN) as f32,
                Some(J::String(s)) => s.parse::<f32>().unwrap_or(f32::NAN),
                _ => f32::NAN,
            })
            .collect();
        current_rows.push((t as u32, values));
    }
    if !current_code.is_empty() {
        rows += current_rows.len();
        flush(&current_code, &mut current_rows)?;
        files += 1;
    }
    println!(
        "compact import complete: kind={} files={} rows={} skipped={} root={}",
        kind,
        files,
        rows,
        skipped,
        root.display()
    );
    Ok(())
}

fn main() -> io::Result<()> {
    let mut a = std::env::args().skip(1);
    let cmd = a.next().ok_or_else(|| {
        err("usage: import_json <snapshot|company-profile|money-flow|compact> ...")
    })?;
    match cmd.as_str() {
        "snapshot" => import_snapshot(
            Path::new(&a.next().ok_or_else(|| err("missing root"))?),
            Path::new(&a.next().ok_or_else(|| err("missing snapshot dir"))?),
        ),
        "company-profile" => import_company_profile(
            Path::new(&a.next().ok_or_else(|| err("missing root"))?),
            Path::new(&a.next().ok_or_else(|| err("missing snapshot dir"))?),
            Path::new(&a.next().ok_or_else(|| err("missing industry history"))?),
        ),
        "money-flow" => import_money_flow(
            Path::new(&a.next().ok_or_else(|| err("missing root"))?),
            Path::new(&a.next().ok_or_else(|| err("missing d5d6 dir"))?),
        ),
        "cache" => import_cache(
            Path::new(&a.next().ok_or_else(|| err("missing root"))?),
            &a.next().ok_or_else(|| err("missing cache table"))?,
            Path::new(&a.next().ok_or_else(|| err("missing jsonl"))?),
        ),
        "compact" => import_compact(
            Path::new(&a.next().ok_or_else(|| err("missing root"))?),
            &a.next().ok_or_else(|| err("missing compact kind"))?,
            Path::new(&a.next().ok_or_else(|| err("missing jsonl"))?),
            &a.next().ok_or_else(|| err("missing columns"))?,
        ),
        _ => Err(err(format!("unknown command {cmd}"))),
    }
}
