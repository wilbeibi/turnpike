//! The per-call cost kernel shared by `stats` and `check`, so the two can
//! never drift on how a call is priced. Both prefer the provider-reported cost
//! and fall back to the local price table; both treat a token-bearing call with
//! no price as *unknown*, never a confident $0.

use crate::pricing::PriceTable;
use crate::record::Usage;
use anyhow::Result;
use jiff::Timestamp;
use rusqlite::Connection;

/// Build a `Usage` for cost accounting from the four token columns as they are
/// stored (`i64`, `0` meaning absent). `cost` is deliberately left `None` so
/// pricing computes from tokens; any provider-reported cost is supplied
/// separately to [`call_cost`] as `stored`.
pub fn usage_from_counts(input: i64, output: i64, cache_read: i64, cache_write: i64) -> Usage {
    Usage {
        input_tokens: (input > 0).then_some(input as u64),
        output_tokens: (output > 0).then_some(output as u64),
        cache_read_input_tokens: (cache_read > 0).then_some(cache_read as u64),
        cache_creation_input_tokens: (cache_write > 0).then_some(cache_write as u64),
        ..Default::default()
    }
}

/// The instant a stored row was recorded, for selecting the price revision in
/// force when the call was made. A row whose `ts` will not parse is priced at
/// current rates: unparseable is corruption, and today's table is the best
/// available guess.
pub fn priced_at(ts: &str) -> Timestamp {
    ts.parse().unwrap_or_else(|_| Timestamp::now())
}

/// The billable cost of one call. Prefer the provider-reported `stored` cost;
/// otherwise price the tokens from the local table *as of `at`*, so a provider
/// price change does not retroactively reprice already-recorded calls.
///
/// `None` means the call carried tokens but no price was found — the caller
/// decides how to surface that, and it must **not** be summed as a confident
/// $0 (which would silently under-report spend). A call with no tokens costs a
/// definite `Some(0.0)`.
pub fn call_cost(
    prices: &PriceTable,
    model: Option<&str>,
    stored: Option<f64>,
    usage: &Usage,
    at: Timestamp,
) -> Option<f64> {
    if let Some(c) = stored {
        return Some(c);
    }
    if let Some(c) = prices.compute(model, usage, at) {
        return Some(c);
    }
    if usage.input_tokens.is_some() || usage.output_tokens.is_some() {
        None
    } else {
        Some(0.0)
    }
}

/// A window's spend and how it was priced.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Spend {
    /// USD, with unpriced calls counted as $0 — see `unpriced`.
    pub total: f64,
    /// Token-bearing calls that had no price. Summed as $0, so a caller must
    /// say the real figure may be higher rather than pass this off as exact.
    pub unpriced: i64,
    /// Calls priced from the provider's own reported cost.
    pub reported: i64,
    /// Calls priced from the local table.
    pub computed: i64,
}

/// Spend for calls with `lower <= ts` (and `ts < upper`, when given),
/// optionally for one provider. `check` and `doctor` both sum through here so
/// a window means the same thing to each.
pub fn spend_between(
    conn: &Connection,
    prices: &PriceTable,
    provider: Option<&str>,
    lower: &str,
    upper: Option<&str>,
) -> Result<Spend> {
    let mut sql = String::from(
        "SELECT model,
                COALESCE(input_tokens, 0),
                COALESCE(output_tokens, 0),
                COALESCE(cache_read_input_tokens, 0),
                COALESCE(cache_creation_input_tokens, 0),
                cost,
                ts
         FROM calls
         WHERE ts >= ?1",
    );
    let mut params: Vec<String> = vec![lower.to_string()];
    if let Some(p) = provider {
        params.push(p.to_string());
        sql.push_str(&format!(" AND provider = ?{}", params.len()));
    }
    if let Some(u) = upper {
        params.push(u.to_string());
        sql.push_str(&format!(" AND ts < ?{}", params.len()));
    }
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
        Ok((
            r.get::<_, Option<String>>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, Option<f64>>(5)?,
            r.get::<_, String>(6)?,
        ))
    })?;

    let mut spend = Spend::default();
    for row in rows {
        let (model, input, output, cache_read, cache_write, stored, ts) = row?;
        let usage = usage_from_counts(input, output, cache_read, cache_write);
        match call_cost(prices, model.as_deref(), stored, &usage, priced_at(&ts)) {
            Some(c) => {
                spend.total += c;
                if stored.is_some() {
                    spend.reported += 1;
                } else if c > 0.0 {
                    spend.computed += 1;
                }
            }
            None => spend.unpriced += 1,
        }
    }
    Ok(spend)
}
