//! `turnpike check` — answer one question with an exit code: is spend in a
//! window at or over a budget? It reads the call database exactly like `stats`
//! and touches no daemon state. Delivery is not turnpike's job: callers inspect
//! the exit code from a coding-agent hook, shell prompt, or notifier wrapper, so
//! turnpike stays a meter, not a notifier.
//!
//! The exit code has four values, not three, because "I can't tell you the
//! answer" and "something is broken" are different signals that call for
//! different caller behavior:
//!
//! - `0` under, `1` at/over — the two answers, from known-priced spend.
//! - `2` error — the caller did something wrong (bad `--budget`), or the data
//!   itself is corrupt. Fix the invocation or go investigate.
//! - `3` unknown — nothing is broken, turnpike just can't vouch for the
//!   number right now: no calls recorded yet (fresh install, or the proxy
//!   isn't running), or some calls in the window have no price. This is
//!   routine and often self-resolving (`turnpike prices pull`, or wait for
//!   data), so a caller shouldn't treat it with the same severity as `2`.
//!
//! Known spend at or over the ceiling is always decisive and reported as `1`
//! even when other calls are unpriced — missing data can't make an
//! already-exceeded budget un-exceeded.

use crate::cost::{call_cost, usage_from_counts};
use crate::paths::{calls_db, prices_json};
use crate::pricing::PriceTable;
use crate::record::open_db;
use anyhow::{bail, Context, Result};
use jiff::{Span, Zoned};
use rusqlite::Connection;

pub struct CheckOpts {
    /// Budget ceiling in USD (always finite and > 0; see [`parse_budget`]).
    pub budget: f64,
    /// Window label: `day` / `week` / `month` (calendar) or any `--since` form.
    pub period: String,
    pub json: bool,
    pub quiet: bool,
}

/// Split `AMOUNT` or `AMOUNT/PERIOD` (`50`, `50/day`, `300/7d`, `500/month`)
/// into a positive budget and a window label (defaulting to `day`).
pub fn parse_budget(spec: &str) -> Result<(f64, String)> {
    let (amount, period) = match spec.split_once('/') {
        Some((a, p)) => (a.trim(), p.trim()),
        None => (spec.trim(), "day"),
    };
    if period.is_empty() {
        bail!("budget period is empty in {spec:?}; try 50/day");
    }
    let budget: f64 = amount
        .parse()
        .with_context(|| format!("bad budget amount {amount:?}; expected a number like 50"))?;
    if !budget.is_finite() || budget <= 0.0 {
        bail!("budget must be a positive number, got {amount:?}");
    }
    Ok((budget, period.to_string()))
}

/// The budget verdict — `over`, `remaining`, and `pct` — derived purely from
/// spend and budget with no IO. Extracted from [`run`] so the at/over boundary
/// (equal spend counts as over — the whole exit-code contract) is unit-testable
/// without a database or captured stdout.
struct Verdict {
    over: bool,
    remaining: f64,
    pct: f64,
}

impl Verdict {
    fn new(spent: f64, budget: f64) -> Self {
        // `budget` is guaranteed finite and > 0 by `parse_budget`, so the
        // division and comparison are always well-defined.
        Self {
            over: spent >= budget,
            remaining: budget - spent,
            pct: spent / budget * 100.0,
        }
    }
}

/// What `turnpike check` answered — see the module doc for the exit-code
/// mapping and why `Unknown` is not folded into the `Err` path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    Under,
    Over,
    Unknown,
}

/// Decide the outcome from the verdict math plus what data was available.
/// Pulled out of [`run`] so the priority rule — over beats missing data,
/// missing data beats under — is unit-testable without a database.
fn classify(v: &Verdict, has_data: bool, unpriced: i64) -> Outcome {
    if v.over {
        Outcome::Over
    } else if !has_data || unpriced > 0 {
        Outcome::Unknown
    } else {
        Outcome::Under
    }
}

pub fn run(opts: CheckOpts) -> Result<Outcome> {
    let lower = window_bound(&opts.period)?;

    let path = calls_db();
    let has_data = path.exists();
    let (spent, unpriced) = if has_data {
        let conn = open_db(&path)?;
        let prices = PriceTable::load(&prices_json());
        sum_spend(&conn, &prices, &lower)?
    } else {
        (0.0, 0)
    };

    let v = Verdict::new(spent, opts.budget);
    let outcome = classify(&v, has_data, unpriced);

    if opts.json {
        let status = match outcome {
            Outcome::Under => "under",
            Outcome::Over => "over",
            Outcome::Unknown => "unknown",
        };
        let reason = match (outcome, has_data) {
            (Outcome::Unknown, false) => Some("no_data"),
            (Outcome::Unknown, true) => Some("unpriced_calls"),
            _ => None,
        };
        let out = serde_json::json!({
            "window": opts.period,
            "since": lower,
            "status": status,
            "reason": reason,
            "spent": spent,
            "budget": opts.budget,
            "pct": v.pct,
            "remaining": v.remaining,
            "unpriced_calls": unpriced,
        });
        println!(
            "{}",
            serde_json::to_string(&out).expect("serializing known-valid JSON")
        );
    } else if !opts.quiet {
        let label = match outcome {
            Outcome::Over => format!("OVER by ${:.2}", -v.remaining),
            Outcome::Under => "ok".to_string(),
            Outcome::Unknown => "unknown".to_string(),
        };
        println!(
            "{}: ${:.2} / ${:.2} ({:.0}%) — {}",
            opts.period, spent, opts.budget, v.pct, label
        );
    }

    // The stdout label says *that* the answer is unknown; stderr says *why*,
    // regardless of --quiet, since this is the part a caller (or the human
    // debugging one) needs to act on.
    if !has_data {
        eprintln!("warning: no calls recorded yet; cannot determine spend — is turnpike running?");
    } else if unpriced > 0 {
        eprintln!(
            "warning: {unpriced} calls with tokens had no price and count as $0 — \
             real spend may be higher; run `turnpike prices pull`"
        );
    }

    Ok(outcome)
}

/// Total USD spent since `lower`, and the count of token-bearing calls that had
/// no price (summed as $0, so a caller can warn that spend may be higher).
fn sum_spend(conn: &Connection, prices: &PriceTable, lower: &str) -> Result<(f64, i64)> {
    let mut stmt = conn.prepare(
        "SELECT model,
                COALESCE(input_tokens, 0),
                COALESCE(output_tokens, 0),
                COALESCE(cache_read_input_tokens, 0),
                COALESCE(cache_creation_input_tokens, 0),
                cost
         FROM calls
         WHERE ts >= ?1",
    )?;
    let rows = stmt.query_map([lower], |r| {
        Ok((
            r.get::<_, Option<String>>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, Option<f64>>(5)?,
        ))
    })?;

    let mut spent = 0.0;
    let mut unpriced = 0i64;
    for row in rows {
        let (model, input, output, cache_read, cache_write, stored) = row?;
        let usage = usage_from_counts(input, output, cache_read, cache_write);
        match call_cost(prices, model.as_deref(), stored, &usage) {
            Some(c) => spent += c,
            None => unpriced += 1,
        }
    }
    Ok((spent, unpriced))
}

/// Resolve a budget period to an RFC-3339 UTC lower bound. `day` / `week` /
/// `month` are calendar windows in local time (today's midnight, this ISO
/// week's Monday, this month's 1st — matching how a provider bills). Anything
/// else is handed to the `--since` grammar, so rolling windows like `7d` or
/// `24h` work too.
fn window_bound(period: &str) -> Result<String> {
    let now = Zoned::now();
    window_bound_at(period, &now)
}

fn window_bound_at(period: &str, now: &Zoned) -> Result<String> {
    let today = now.datetime().date();
    let start = match period {
        "day" => today,
        "week" => {
            let back = i64::from(today.weekday().to_monday_zero_offset());
            today
                .checked_sub(Span::new().try_days(back)?)
                .context("resolving start of week")?
        }
        "month" => today.first_of_month(),
        other => return crate::since::lower_bound(other),
    };
    Ok(start
        .to_zoned(now.time_zone().clone())
        .context("resolving local midnight")?
        .timestamp()
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_budget_amount_and_period() {
        assert_eq!(parse_budget("50").unwrap(), (50.0, "day".into()));
        assert_eq!(parse_budget("50/day").unwrap(), (50.0, "day".into()));
        assert_eq!(parse_budget(" 300 / 7d ").unwrap(), (300.0, "7d".into()));
        assert_eq!(parse_budget("500/month").unwrap(), (500.0, "month".into()));
        assert_eq!(parse_budget("12.50/week").unwrap(), (12.5, "week".into()));
    }

    #[test]
    fn parse_budget_rejects_nonpositive_and_garbage() {
        for bad in ["0", "-5", "abc", "", "nan", "inf", "50/"] {
            assert!(parse_budget(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn verdict_counts_equal_spend_as_over() {
        // The exit-code contract is "at or over": under stays under, but spend
        // exactly equal to the budget must trip to over (the boundary the whole
        // `check … || alert` idiom hinges on).
        assert!(!Verdict::new(49.99, 50.0).over);
        assert!(
            Verdict::new(50.0, 50.0).over,
            "spend == budget must be over"
        );
        assert!(Verdict::new(63.0, 50.0).over);
    }

    #[test]
    fn verdict_remaining_is_signed_and_pct_scales() {
        let under = Verdict::new(20.0, 50.0);
        assert!((under.remaining - 30.0).abs() < 1e-9, "{}", under.remaining);
        assert!((under.pct - 40.0).abs() < 1e-9, "{}", under.pct);

        // Over budget, remaining goes negative; the summary prints `-remaining`
        // as the "OVER by $X" amount, so the sign carries meaning.
        let over = Verdict::new(60.0, 50.0);
        assert!((over.remaining + 10.0).abs() < 1e-9, "{}", over.remaining);
        assert!((over.pct - 120.0).abs() < 1e-9, "{}", over.pct);
    }

    #[test]
    fn classify_prefers_over_even_without_full_information() {
        // An already-exceeded budget stays exceeded no matter what data is
        // missing — that's the whole point of checking `over` first.
        let v = Verdict::new(60.0, 50.0);
        assert_eq!(classify(&v, true, 3), Outcome::Over);
        assert_eq!(classify(&v, false, 0), Outcome::Over);
    }

    #[test]
    fn classify_is_unknown_without_data_or_with_unpriced_calls() {
        let no_data = Verdict::new(0.0, 50.0);
        assert_eq!(classify(&no_data, false, 0), Outcome::Unknown);

        let has_unpriced = Verdict::new(10.0, 50.0);
        assert_eq!(classify(&has_unpriced, true, 2), Outcome::Unknown);
    }

    #[test]
    fn classify_is_under_only_with_complete_priced_data() {
        let v = Verdict::new(10.0, 50.0);
        assert_eq!(classify(&v, true, 0), Outcome::Under);
    }

    #[test]
    fn calendar_windows_use_local_day_week_and_month_boundaries() {
        let now: Zoned = "2026-07-08T12:00:00-07:00[America/Los_Angeles]"
            .parse()
            .unwrap();

        assert_eq!(
            window_bound_at("day", &now).unwrap(),
            "2026-07-08T07:00:00Z"
        );
        assert_eq!(
            window_bound_at("week", &now).unwrap(),
            "2026-07-06T07:00:00Z"
        );
        assert_eq!(
            window_bound_at("month", &now).unwrap(),
            "2026-07-01T07:00:00Z"
        );
    }

    #[test]
    fn calendar_window_resolves_midnight_across_dst() {
        let now: Zoned = "2026-03-09T12:00:00-07:00[America/Los_Angeles]"
            .parse()
            .unwrap();

        assert_eq!(
            window_bound_at("week", &now).unwrap(),
            "2026-03-09T07:00:00Z"
        );
    }

    #[test]
    fn nonkeyword_period_delegates_to_since() {
        // A rolling spec falls through to the --since grammar; garbage there
        // still errors rather than being treated as a window.
        assert!(window_bound("7d").is_ok());
        assert!(window_bound("today").is_ok());
        assert!(window_bound("yesterday").is_err());
    }

    /// Minimal fixture holding only the columns `sum_spend` reads.
    fn fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE calls (
                ts TEXT NOT NULL, model TEXT,
                input_tokens INTEGER, output_tokens INTEGER,
                cache_read_input_tokens INTEGER, cache_creation_input_tokens INTEGER,
                cost REAL
            );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn sum_prefers_stored_cost_flags_unpriced_and_honors_window() {
        let conn = fixture();
        // Empty price table: token-bearing rows without a stored cost are
        // unpriceable, so they must be flagged, not summed as $0.
        let prices = PriceTable::load(std::path::Path::new("/definitely/not/here.json"));

        conn.execute_batch(
            "INSERT INTO calls VALUES
                -- in window, provider-reported cost wins
                ('2026-07-20T10:00:00Z', 'gpt-x',  100, 50, 0, 0, 0.12),
                -- in window, tokens but no price -> unpriced, adds $0
                ('2026-07-20T11:00:00Z', 'mystery', 100, 50, 0, 0, NULL),
                -- in window, pure error, no tokens -> a definite $0, not unpriced
                ('2026-07-20T12:00:00Z', NULL,       0,  0, 0, 0, NULL),
                -- before the window -> excluded entirely
                ('2026-07-01T09:00:00Z', 'gpt-x',  999, 99, 0, 0, 5.00);",
        )
        .unwrap();

        let (spent, unpriced) = sum_spend(&conn, &prices, "2026-07-15T00:00:00Z").unwrap();
        assert!((spent - 0.12).abs() < 1e-9, "spent was {spent}");
        assert_eq!(unpriced, 1);
    }

    #[test]
    fn sum_propagates_bad_row_data() {
        let conn = fixture();
        let prices = PriceTable::load(std::path::Path::new("/definitely/not/here.json"));
        conn.execute_batch(
            "INSERT INTO calls VALUES
                ('2026-07-20T10:00:00Z', 'gpt-x', 'not-an-integer', 50, 0, 0, 0.12);",
        )
        .unwrap();

        assert!(sum_spend(&conn, &prices, "2026-07-15T00:00:00Z").is_err());
    }
}
