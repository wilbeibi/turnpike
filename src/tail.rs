use crate::attr::{display_tool, unified_tool};
use crate::paths::{calls_db, prices_json};
use crate::pricing::PriceTable;
use crate::record::{has_column, open_db, Usage};
use anyhow::Result;
use rusqlite::params;

struct Row {
    ts: String,
    provider: String,
    model: Option<String>,
    status: Option<u16>,
    latency_ms: u64,
    ttft_ms: Option<u64>,
    input: Option<u64>,
    output: Option<u64>,
    cache_read: Option<u64>,
    cache_creation: Option<u64>,
    error_kind: Option<String>,
    error_message: Option<String>,
    stored_cost: Option<f64>,
    client: Option<String>,
    client_source: Option<String>,
    peer_exe: Option<String>,
    endpoint: Option<String>,
    anomaly: Option<String>,
}

pub fn run(n: usize, since: Option<String>, json: bool) -> Result<()> {
    let path = calls_db();
    if !path.exists() {
        println!("No records yet at {}", path.display());
        return Ok(());
    }

    let conn = open_db(&path)?;
    let prices = PriceTable::load(&prices_json());

    // Every stored ts sorts >= "", so the no-filter case binds an empty bound.
    let lower = match &since {
        Some(spec) => crate::since::lower_bound(spec)?,
        None => String::new(),
    };

    // Readers never migrate (see stats.rs): appended attribution columns
    // are selected behind a has_column guard so old DBs stay readable.
    let src_col = if has_column(&conn, "client_source")? {
        "client_source"
    } else {
        "NULL"
    };
    let exe_col = if has_column(&conn, "peer_exe")? {
        "peer_exe"
    } else {
        "NULL"
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT ts, provider, model, status, latency_ms, ttft_ms,
                input_tokens, output_tokens,
                cache_read_input_tokens, cache_creation_input_tokens,
                error_kind, cost, client, {exe_col} AS peer_exe,
                {src_col} AS client_source, endpoint, anomaly, error_message
         FROM calls
         WHERE ts >= ?1
         ORDER BY rowid DESC
         LIMIT ?2"
    ))?;

    let mut rows: Vec<Row> = stmt
        .query_map(params![lower, n as i64], |r| {
            Ok(Row {
                ts: r.get(0)?,
                provider: r.get(1)?,
                model: r.get(2)?,
                status: r.get(3)?,
                latency_ms: r.get::<_, i64>(4)? as u64,
                ttft_ms: r.get::<_, Option<i64>>(5)?.map(|v| v as u64),
                input: r.get::<_, Option<i64>>(6)?.map(|v| v as u64),
                output: r.get::<_, Option<i64>>(7)?.map(|v| v as u64),
                cache_read: r.get::<_, Option<i64>>(8)?.map(|v| v as u64),
                cache_creation: r.get::<_, Option<i64>>(9)?.map(|v| v as u64),
                error_kind: r.get(10)?,
                stored_cost: r.get(11)?,
                client: r.get(12)?,
                peer_exe: r.get(13)?,
                client_source: r.get(14)?,
                endpoint: r.get(15)?,
                anomaly: r.get(16)?,
                error_message: r.get(17)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    rows.reverse(); // oldest first
    for row in &rows {
        if json {
            print_json_line(row, &prices);
        } else {
            print_row(row, &prices);
        }
    }
    Ok(())
}

/// (cost, source): stored provider-reported cost wins; otherwise computed
/// from the price table at read time.
fn cost_of(r: &Row, prices: &PriceTable) -> (Option<f64>, &'static str) {
    if let Some(c) = r.stored_cost {
        return (Some(c), "provider");
    }
    let usage = Usage {
        input_tokens: r.input,
        output_tokens: r.output,
        cache_read_input_tokens: r.cache_read,
        cache_creation_input_tokens: r.cache_creation,
        ..Default::default()
    };
    // Priced at the row's own timestamp so an old call keeps the price it was
    // billed at after the provider changes rates.
    (
        prices.compute(r.model.as_deref(), &usage, crate::cost::priced_at(&r.ts)),
        "computed",
    )
}

/// One JSON object per line (JSONL) — grep/jq friendly for consumers.
fn print_json_line(r: &Row, prices: &PriceTable) {
    let (cost, cost_source) = cost_of(r, prices);
    let obj = serde_json::json!({
        "ts": r.ts,
        "provider": r.provider,
        "model": r.model,
        "endpoint": r.endpoint,
        "status": r.status,
        "latency_ms": r.latency_ms,
        "ttft_ms": r.ttft_ms,
        "input_tokens": r.input,
        "output_tokens": r.output,
        "cache_read_tokens": r.cache_read,
        "cache_creation_tokens": r.cache_creation,
        "cost_usd": cost,
        "cost_source": cost.map(|_| cost_source),
        "error_kind": r.error_kind,
        "error_message": r.error_message,
        "anomaly": r.anomaly,
        "client": r.client,
        "client_source": r.client_source,
        "peer_exe": r.peer_exe,
        "tool": unified_tool(
            r.client.as_deref(),
            r.client_source.as_deref(),
            r.peer_exe.as_deref(),
        ),
    });
    println!("{obj}");
}

fn print_row(r: &Row, prices: &PriceTable) {
    let model = r.model.as_deref().unwrap_or("?");
    let status = r
        .status
        .map(|s| s.to_string())
        .unwrap_or_else(|| "err".into());
    let tokens = match (r.input, r.output) {
        (Some(i), Some(o)) => format!("{i}→{o}"),
        (Some(i), None) => format!("{i}→?"),
        _ => "?".into(),
    };
    let cache_hit = r.cache_read.is_some_and(|n| n > 0);
    let cache = if cache_hit {
        format!(" cache_read={}", r.cache_read.unwrap_or(0))
    } else {
        String::new()
    };
    let cost = cost_of(r, prices)
        .0
        .map(|c| format!(" ${c:.4}"))
        .unwrap_or_default();
    // Transport errors carry error_kind; HTTP-level failures only a status.
    let err = match (r.error_kind.as_deref(), r.status) {
        (Some(k), _) => format!(" ERROR={k}"),
        (None, Some(429)) => " ERROR=rate_limit".to_string(),
        (None, Some(s)) if s >= 400 => format!(" ERROR=http_{s}"),
        _ => String::new(),
    };
    // Why it failed: a transport error's cause chain or the upstream's
    // `error.message`, cut so the row stays one line.
    let cause = r
        .error_message
        .as_deref()
        .map(|m| format!(" {:?}", short_message(m)))
        .unwrap_or_default();
    let anomaly = r
        .anomaly
        .as_deref()
        .map(|a| format!(" ANOMALY={a}"))
        .unwrap_or_default();
    // The unified attribution chain: declared header, else observed
    // process, else the UA fallback — the same ranking as `stats --by-tool`.
    let tool = unified_tool(
        r.client.as_deref(),
        r.client_source.as_deref(),
        r.peer_exe.as_deref(),
    )
    .map(|t| format!(" tool={}", short_tool(&display_tool(t))))
    .unwrap_or_default();
    println!(
        "[{}] {} {} {} {}ms tokens={}{}{}{}{}{}{}",
        r.ts,
        r.provider,
        model,
        status,
        r.latency_ms,
        tokens,
        cache,
        cost,
        err,
        cause,
        anomaly,
        tool,
    );
}

fn short_message(m: &str) -> String {
    const MAX: usize = 160;
    match m.char_indices().nth(MAX) {
        Some((end, _)) => format!("{}…", &m[..end]),
        None => m.to_string(),
    }
}

/// First whitespace token of the tool label, bounded for one-line display.
fn short_tool(c: &str) -> String {
    let tok = c.split_whitespace().next().unwrap_or(c);
    tok.chars().take(40).collect()
}
