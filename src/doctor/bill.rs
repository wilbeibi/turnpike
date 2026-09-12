//! Bill vs meter: the provider's own spend figure for a key against what
//! turnpike metered for that provider over the same window. Only two
//! providers expose such a figure to the key itself:
//!
//! | provider   | endpoint            | figure                        | kind    |
//! | ---------- | ------------------- | ----------------------------- | ------- |
//! | DeepSeek   | `/user/balance`     | `balance_infos[].total_balance`, per currency | level   |
//! | OpenRouter | `/api/v1/auth/key`  | `data.usage`, lifetime USD    | counter |
//!
//! A level (prepaid balance) and a counter (lifetime usage) both need a
//! previous reading to become spend, so readings persist in
//! `$data_dir/doctor.json` — a JSON file the user can delete to reset the
//! baseline, not a table. The first run records a baseline and cannot compare;
//! every later run compares the movement since the stored reading against
//! `calls.db` over exactly that window. The reading rolls forward once it is
//! a day old, so a hook that runs doctor every session still yields daily
//! windows instead of one-minute ones, and a fix shows up as a closed gap
//! the next day rather than diluting a lifetime total.
//!
//! Everything here except [`fetch`] is a pure function, so the delta rules —
//! top-ups, currencies, unpriced calls — are tested without a network.

use crate::cost::Spend;
use anyhow::{anyhow, bail, Context, Result};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

/// A gap is flagged when it exceeds both: 5% of the bill and five cents.
/// Below that it is pricing drift (off-peak windows, rounding), not a leak.
pub const THRESHOLD_PCT: f64 = 5.0;
pub const THRESHOLD_USD: f64 = 0.05;
pub const TIMEOUT: Duration = Duration::from_secs(10);
/// A stored reading younger than this is kept as the baseline; older, it is
/// replaced by the new one after the comparison.
pub const ROLL_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A prepaid balance: spend is the drop between readings.
    Level,
    /// A lifetime usage total: spend is the rise between readings.
    Counter,
}

/// One reading of a provider's figure, per currency code (`USD`, `CNY`).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Reading {
    pub kind: Kind,
    pub amounts: BTreeMap<String, f64>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Snapshot {
    /// RFC-3339 UTC, the same form `calls.db` stores, so it bounds a query.
    pub ts: String,
    #[serde(flatten)]
    pub reading: Reading,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct State {
    #[serde(default)]
    pub snapshots: BTreeMap<String, Snapshot>,
}

impl State {
    /// A missing file is an empty state. A file that will not parse is an
    /// error naming the path: it is user data, and deleting it is the fix.
    pub fn load(path: &Path) -> Result<State> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| {
                format!("{} is not doctor state; delete it to reset", path.display())
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
    }
}

/// Where a provider's figure comes from and how to read it.
pub struct Source {
    pub provider: &'static str,
    pub url: &'static str,
    pub parse: fn(&serde_json::Value) -> Result<Reading>,
}

pub const SOURCES: &[Source] = &[
    Source {
        provider: "deepseek",
        url: "https://api.deepseek.com/user/balance",
        parse: parse_deepseek,
    },
    Source {
        provider: "openrouter",
        url: "https://openrouter.ai/api/v1/auth/key",
        parse: parse_openrouter,
    },
];

/// `{"balance_infos":[{"currency":"CNY","total_balance":"317.56",...}, ...]}`.
/// `total_balance` is a string. An account may hold more than one currency;
/// `granted_balance` spends first, so the total is what moves either way.
pub fn parse_deepseek(v: &serde_json::Value) -> Result<Reading> {
    let infos = v["balance_infos"]
        .as_array()
        .ok_or_else(|| anyhow!("no balance_infos in response"))?;
    let mut amounts = BTreeMap::new();
    for info in infos {
        let currency = info["currency"]
            .as_str()
            .ok_or_else(|| anyhow!("balance without a currency"))?;
        amounts.insert(currency.to_string(), number(&info["total_balance"])?);
    }
    if amounts.is_empty() {
        bail!("no balances in response");
    }
    Ok(Reading {
        kind: Kind::Level,
        amounts,
    })
}

/// `{"data":{"usage":131.27,...}}` — lifetime USD spent through this key.
pub fn parse_openrouter(v: &serde_json::Value) -> Result<Reading> {
    let usage = number(&v["data"]["usage"]).context("data.usage")?;
    Ok(Reading {
        kind: Kind::Counter,
        amounts: BTreeMap::from([("USD".to_string(), usage)]),
    })
}

fn number(v: &serde_json::Value) -> Result<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64().ok_or_else(|| anyhow!("not a float")),
        serde_json::Value::String(s) => s
            .trim()
            .parse()
            .with_context(|| format!("{s:?} is not a number")),
        other => bail!("expected a number, got {other}"),
    }
}

/// GET the figure with the key as a bearer token and nothing else. The key
/// never appears in a URL, a log line, or an error.
pub async fn fetch(client: &reqwest::Client, src: &Source, key: &str) -> Result<Reading> {
    let resp = client
        .get(src.url)
        .bearer_auth(key)
        .send()
        .await
        .map_err(|e| anyhow!("{}", e.without_url()))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("HTTP {status}");
    }
    let text = resp.text().await.context("reading response")?;
    let body: serde_json::Value = serde_json::from_str(&text).context("response is not JSON")?;
    (src.parse)(&body)
}

/// What the figure did between two readings.
#[derive(Debug, PartialEq)]
pub enum Movement {
    /// The figure moved the wrong way — a top-up, or a counter that was
    /// reset — so the window holds no spend to compare. Baseline restarts.
    Reset(&'static str),
    /// Spend per currency since the previous reading.
    Spent(BTreeMap<String, f64>),
}

pub fn movement(prev: &Reading, now: &Reading) -> Movement {
    let mut spent = BTreeMap::new();
    for code in prev.amounts.keys().chain(now.amounts.keys()) {
        let before = prev.amounts.get(code).copied().unwrap_or(0.0);
        let after = now.amounts.get(code).copied().unwrap_or(0.0);
        let delta = match now.kind {
            Kind::Level => before - after,
            Kind::Counter => after - before,
        };
        if delta < -0.005 {
            return Movement::Reset(match now.kind {
                Kind::Level => "topped up",
                Kind::Counter => "counter went backwards",
            });
        }
        spent.insert(code.clone(), delta.max(0.0));
    }
    Movement::Spent(spent)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Comparison {
    pub since: String,
    pub until: String,
    /// USD the provider says was spent in the window.
    pub billed: f64,
    /// Spend in other currencies. Any nonzero entry means no verdict: turnpike
    /// meters in USD and does no exchange-rate lookup.
    pub foreign: BTreeMap<String, f64>,
    pub metered: f64,
    /// `provider-reported` when every priced call carried the provider's own
    /// cost, else `price table`.
    pub metered_source: &'static str,
    pub unpriced: i64,
    pub gap: f64,
    pub gap_pct: Option<f64>,
    /// Why no verdict, when there is none: `currency_mismatch` or
    /// `unpriced_calls`. The numbers are still printed. A currency mismatch
    /// with nothing metered at all is still flagged: that gap exists in any
    /// currency.
    pub no_verdict: Option<&'static str>,
    pub flagged: bool,
}

pub fn compare(
    since: &str,
    until: &str,
    spent: &BTreeMap<String, f64>,
    meter: &Spend,
) -> Comparison {
    let billed = spent.get("USD").copied().unwrap_or(0.0);
    let foreign: BTreeMap<String, f64> = spent
        .iter()
        .filter(|(code, amt)| code.as_str() != "USD" && **amt > 0.0)
        .map(|(c, a)| (c.clone(), *a))
        .collect();
    let gap = billed - meter.total;
    let gap_pct = (billed > 0.0).then(|| gap / billed * 100.0);
    let no_verdict = if !foreign.is_empty() {
        Some("currency_mismatch")
    } else if meter.unpriced > 0 {
        Some("unpriced_calls")
    } else {
        None
    };
    let threshold = (billed * THRESHOLD_PCT / 100.0).max(THRESHOLD_USD);
    let spent_unmetered =
        meter.total == 0.0 && meter.unpriced == 0 && foreign.values().any(|a| *a > THRESHOLD_USD);
    Comparison {
        since: since.to_string(),
        until: until.to_string(),
        billed,
        foreign,
        metered: meter.total,
        metered_source: if meter.computed == 0 {
            "provider-reported"
        } else {
            "price table"
        },
        unpriced: meter.unpriced,
        gap,
        gap_pct,
        no_verdict,
        flagged: (no_verdict.is_none() && gap > threshold) || spent_unmetered,
    }
}

/// One provider's outcome.
#[derive(Debug)]
pub enum Status {
    NoKey,
    Offline,
    Unreachable(String),
    /// The provider answered but the local side could not: `calls.db` would
    /// not open, or the reading could not be kept.
    Failed(String),
    Baseline,
    Reset(&'static str),
    Compared(Comparison),
}

impl Status {
    pub fn label(&self) -> &'static str {
        match self {
            Status::NoKey => "no_key",
            Status::Offline => "offline",
            Status::Unreachable(_) => "unreachable",
            Status::Failed(_) => "failed",
            Status::Baseline => "baseline",
            Status::Reset(_) => "reset",
            Status::Compared(_) => "compared",
        }
    }

    /// True when this row could not deliver a verdict for a reason that is
    /// not the user's choice. `NoKey` and `Offline` are not incomplete, and
    /// neither is a currency mismatch: that is a limit of the meter stated
    /// on the line, not a question left unanswered, and an account funded in
    /// CNY would otherwise never exit 0 again.
    pub fn incomplete(&self) -> bool {
        match self {
            Status::NoKey | Status::Offline => false,
            Status::Compared(c) => c.unpriced > 0,
            _ => true,
        }
    }
}

/// Turn a fresh reading into a status, given the previous snapshot and a way
/// to meter a window. Returns the snapshot to store next: a first reading or
/// a reset starts a new baseline, a comparison keeps the old one until it is
/// [`ROLL_AFTER`] old.
pub fn assess(
    prev: Option<&Snapshot>,
    now: Reading,
    until: &str,
    meter: impl FnOnce(&str, &str) -> Result<Spend>,
) -> Result<(Status, Snapshot)> {
    let next = Snapshot {
        ts: until.to_string(),
        reading: now,
    };
    let Some(prev) = prev else {
        return Ok((Status::Baseline, next));
    };
    match movement(&prev.reading, &next.reading) {
        Movement::Reset(why) => Ok((Status::Reset(why), next)),
        Movement::Spent(spent) => {
            let m = meter(&prev.ts, until)?;
            let status = Status::Compared(compare(&prev.ts, until, &spent, &m));
            let keep = age(&prev.ts, until).is_some_and(|a| a < ROLL_AFTER);
            Ok((status, if keep { prev.clone() } else { next }))
        }
    }
}

/// How long a window is, when both ends parse (an edited state file may not;
/// then the reading rolls, which is the safe direction).
pub fn age(since: &str, until: &str) -> Option<Duration> {
    let a: Timestamp = since.parse().ok()?;
    let b: Timestamp = until.parse().ok()?;
    b.duration_since(a).try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(kind: Kind, pairs: &[(&str, f64)]) -> Reading {
        Reading {
            kind,
            amounts: pairs.iter().map(|(c, a)| (c.to_string(), *a)).collect(),
        }
    }

    fn spend(total: f64, unpriced: i64, computed: i64) -> Spend {
        Spend {
            total,
            unpriced,
            reported: 1,
            computed,
        }
    }

    #[test]
    fn deepseek_balance_is_a_level_per_currency_with_string_amounts() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"is_available":true,"balance_infos":[
                {"currency":"CNY","total_balance":"317.56","granted_balance":"0.00","topped_up_balance":"317.56"},
                {"currency":"USD","total_balance":"1.95","granted_balance":"0.00","topped_up_balance":"1.95"}]}"#,
        )
        .unwrap();
        let r = parse_deepseek(&v).unwrap();
        assert_eq!(r, reading(Kind::Level, &[("CNY", 317.56), ("USD", 1.95)]));
        assert!(parse_deepseek(&serde_json::json!({"balance_infos": []})).is_err());
        assert!(parse_deepseek(&serde_json::json!({"error": "x"})).is_err());
    }

    #[test]
    fn openrouter_usage_is_a_lifetime_usd_counter() {
        let v = serde_json::json!({"data": {"label": "x", "usage": 131.265382032, "usage_daily": 0.02}});
        assert_eq!(
            parse_openrouter(&v).unwrap(),
            reading(Kind::Counter, &[("USD", 131.265382032)])
        );
        assert!(parse_openrouter(&serde_json::json!({"data": {}})).is_err());
    }

    #[test]
    fn spend_is_the_drop_of_a_level_and_the_rise_of_a_counter() {
        let level = movement(
            &reading(Kind::Level, &[("CNY", 300.0), ("USD", 2.0)]),
            &reading(Kind::Level, &[("CNY", 281.6), ("USD", 2.0)]),
        );
        let Movement::Spent(s) = level else {
            panic!("{level:?}")
        };
        assert!((s["CNY"] - 18.4).abs() < 1e-9);
        assert_eq!(s["USD"], 0.0);

        let counter = movement(
            &reading(Kind::Counter, &[("USD", 100.0)]),
            &reading(Kind::Counter, &[("USD", 102.93)]),
        );
        let Movement::Spent(s) = counter else {
            panic!("{counter:?}")
        };
        assert!((s["USD"] - 2.93).abs() < 1e-9);
    }

    #[test]
    fn a_rise_in_balance_or_a_drop_in_usage_resets_the_baseline() {
        assert_eq!(
            movement(
                &reading(Kind::Level, &[("USD", 1.0)]),
                &reading(Kind::Level, &[("USD", 50.0)])
            ),
            Movement::Reset("topped up")
        );
        // A currency that appears with a balance is a top-up too.
        assert_eq!(
            movement(
                &reading(Kind::Level, &[("USD", 1.0)]),
                &reading(Kind::Level, &[("USD", 1.0), ("CNY", 100.0)])
            ),
            Movement::Reset("topped up")
        );
        assert_eq!(
            movement(
                &reading(Kind::Counter, &[("USD", 10.0)]),
                &reading(Kind::Counter, &[("USD", 3.0)])
            ),
            Movement::Reset("counter went backwards")
        );
        // Rounding noise is not a top-up.
        assert!(matches!(
            movement(
                &reading(Kind::Level, &[("USD", 1.0)]),
                &reading(Kind::Level, &[("USD", 1.001)])
            ),
            Movement::Spent(_)
        ));
    }

    #[test]
    fn gap_is_flagged_past_five_percent_or_five_cents_whichever_is_larger() {
        let usd = |b: f64| BTreeMap::from([("USD".to_string(), b)]);
        // 44% gap on a small bill: flagged, provider-reported meter.
        let c = compare("a", "b", &usd(2.93), &spend(1.65, 0, 0));
        assert!(c.flagged);
        assert_eq!(c.metered_source, "provider-reported");
        assert!((c.gap - 1.28).abs() < 1e-9);
        assert!((c.gap_pct.unwrap() - 43.686).abs() < 0.01);
        // Under five cents on a tiny bill: not flagged even though it is 40%.
        assert!(!compare("a", "b", &usd(0.10), &spend(0.06, 0, 0)).flagged);
        // Under 5% on a big bill: not flagged even though it is dollars.
        assert!(!compare("a", "b", &usd(100.0), &spend(96.0, 0, 0)).flagged);
        // Over both: flagged, and the meter used the price table.
        let c = compare("a", "b", &usd(100.0), &spend(90.0, 0, 3));
        assert!(c.flagged);
        assert_eq!(c.metered_source, "price table");
        // Meter above bill is drift, never a finding.
        let c = compare("a", "b", &usd(1.0), &spend(1.5, 0, 3));
        assert!(!c.flagged);
        assert!(c.gap < 0.0);
        // No bill: no percentage.
        assert_eq!(
            compare("a", "b", &usd(0.0), &spend(0.0, 0, 0)).gap_pct,
            None
        );
    }

    #[test]
    fn foreign_currency_or_unpriced_calls_withhold_the_verdict() {
        let spent = BTreeMap::from([("CNY".to_string(), 18.4), ("USD".to_string(), 0.0)]);
        let c = compare("a", "b", &spent, &spend(2.31, 0, 3));
        assert_eq!(c.no_verdict, Some("currency_mismatch"));
        assert!(!c.flagged);
        // A mismatch is a stated limit, not an open question.
        assert!(!Status::Compared(c.clone()).incomplete());
        // ...unless nothing was metered at all: that gap exists in any currency.
        let c = compare("a", "b", &spent, &spend(0.0, 0, 0));
        assert_eq!(c.no_verdict, Some("currency_mismatch"));
        assert!(c.flagged);
        // Nothing metered because nothing was priced is not that, and
        // unpriced calls keep the run incomplete whatever the currency.
        let c = compare("a", "b", &spent, &spend(0.0, 2, 0));
        assert!(!c.flagged);
        assert!(Status::Compared(c.clone()).incomplete());
        assert!(Status::Failed("x".into()).incomplete());
        assert_eq!(c.foreign["CNY"], 18.4);
        assert_eq!(c.billed, 0.0);

        let usd = BTreeMap::from([("USD".to_string(), 10.0)]);
        let c = compare("a", "b", &usd, &spend(1.0, 2, 3));
        assert_eq!(c.no_verdict, Some("unpriced_calls"));
        assert!(
            !c.flagged,
            "a huge gap with unpriced calls is not a verdict"
        );
    }

    #[test]
    fn assess_records_a_baseline_then_compares_then_resets() {
        let now = reading(Kind::Counter, &[("USD", 100.0)]);
        let (status, snap) = assess(None, now, "t1", |_, _| unreachable!()).unwrap();
        assert!(matches!(status, Status::Baseline));
        assert!(status.incomplete());
        assert_eq!(snap.ts, "t1");

        let later = reading(Kind::Counter, &[("USD", 103.0)]);
        let (status, snap2) = assess(Some(&snap), later, "t2", |since, until| {
            assert_eq!((since, until), ("t1", "t2"));
            Ok(spend(2.95, 0, 0))
        })
        .unwrap();
        let Status::Compared(c) = &status else {
            panic!("{status:?}")
        };
        assert!(!c.flagged);
        assert!(!status.incomplete());
        assert_eq!(snap2.reading.amounts["USD"], 103.0);

        let rotated = reading(Kind::Counter, &[("USD", 0.5)]);
        let (status, snap3) = assess(Some(&snap2), rotated, "t3", |_, _| unreachable!()).unwrap();
        assert!(matches!(status, Status::Reset("counter went backwards")));
        assert_eq!(snap3.ts, "t3");
    }

    #[test]
    fn the_baseline_rolls_forward_only_once_it_is_a_day_old() {
        let base = Snapshot {
            ts: "2026-09-04T14:02:00Z".into(),
            reading: reading(Kind::Counter, &[("USD", 100.0)]),
        };
        // An hour later: compared against the baseline, baseline kept.
        let (status, next) = assess(
            Some(&base),
            reading(Kind::Counter, &[("USD", 101.0)]),
            "2026-09-04T15:02:00Z",
            |_, _| Ok(spend(1.0, 0, 0)),
        )
        .unwrap();
        assert!(matches!(status, Status::Compared(_)));
        assert_eq!(next.ts, base.ts);
        assert_eq!(next.reading.amounts["USD"], 100.0);
        // A day and a minute later: compared over the whole day, then rolled.
        let (status, next) = assess(
            Some(&base),
            reading(Kind::Counter, &[("USD", 103.0)]),
            "2026-09-05T14:03:00Z",
            |since, _| {
                assert_eq!(since, "2026-09-04T14:02:00Z");
                Ok(spend(3.0, 0, 0))
            },
        )
        .unwrap();
        let Status::Compared(c) = status else {
            panic!()
        };
        assert_eq!(c.since, base.ts);
        assert_eq!(next.ts, "2026-09-05T14:03:00Z");
        assert_eq!(next.reading.amounts["USD"], 103.0);
        // A reset always starts over, however young the baseline.
        let (_, next) = assess(
            Some(&base),
            reading(Kind::Counter, &[("USD", 1.0)]),
            "2026-09-04T14:03:00Z",
            |_, _| unreachable!(),
        )
        .unwrap();
        assert_eq!(next.ts, "2026-09-04T14:03:00Z");
    }

    #[test]
    fn state_round_trips_and_a_missing_file_is_empty() {
        let dir = std::env::temp_dir().join(format!("turnpike-bill-{}", std::process::id()));
        let path = dir.join("doctor.json");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(State::load(&path).unwrap().snapshots.is_empty());

        let mut state = State::default();
        state.snapshots.insert(
            "deepseek".into(),
            Snapshot {
                ts: "2026-09-04T14:02:00Z".into(),
                reading: reading(Kind::Level, &[("CNY", 317.56)]),
            },
        );
        state.save(&path).unwrap();
        let back = State::load(&path).unwrap();
        assert_eq!(
            back.snapshots["deepseek"].reading,
            state.snapshots["deepseek"].reading
        );
        assert_eq!(back.snapshots["deepseek"].ts, "2026-09-04T14:02:00Z");
        // The stored form is what a person expects to see and edit.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(r#""kind": "level""#), "{text}");
        assert!(text.contains(r#""CNY": 317.56"#), "{text}");

        std::fs::write(&path, b"{ not json").unwrap();
        let err = State::load(&path).unwrap_err().to_string();
        assert!(err.contains("delete it to reset"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
