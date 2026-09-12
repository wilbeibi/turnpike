//! `turnpike doctor` — answer "what on this machine spends money without going
//! through turnpike?" without configuring anything. It reads the environment
//! and walks configuration files, and asks the two providers that expose a
//! spend figure how much a key really spent; it changes nothing and never
//! touches the forward path. Three signals:
//!
//! 1. **This shell.** Every provider whose key is set here with nothing
//!    pointing at turnpike — the `this shell` column of `turnpike config`,
//!    promoted to a finding. Same scope, same caveat: it is the environment
//!    turnpike was launched from, not the whole machine.
//! 2. **Config files.** Every file under the usual configuration roots that
//!    names a vendor host (`api.deepseek.com`, ...). Path and host only —
//!    never the line, because the line is where the key sits.
//! 3. **Bill vs meter.** DeepSeek's balance and OpenRouter's key usage,
//!    against what turnpike metered for that provider since the previous
//!    reading (see [`bill`]). The one signal that needs a network; `--offline`
//!    turns it off.
//!
//! Exit codes follow `check`'s shape, because callers already know it:
//! `0` nothing found, `1` findings, `2` error, `3` incomplete — a signal could
//! not answer (scan cut short, provider unreachable, no baseline yet), so a
//! clean report cannot be vouched for. Findings beat incomplete: a leak found
//! in a partial scan is still a leak.

pub mod bill;
pub mod scan;

use crate::config::base_url;
use crate::cost::spend_between;
use crate::paths::{calls_db, doctor_json, prices_json};
use crate::pricing::PriceTable;
use crate::providers::{Provider, PROVIDERS};
use crate::record::open_db;
use crate::routing::{base_url_env, key_var, routing, Routing};
use anyhow::Result;
use bill::{Comparison, Status};
use jiff::tz::TimeZone;
use jiff::Timestamp;
use scan::{Hit, Limits, Needle, Report};
use std::collections::HashMap;
use std::path::Path;

pub struct DoctorOpts {
    pub json: bool,
    pub offline: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    Clean,
    Findings,
    Incomplete,
}

impl Outcome {
    fn label(self) -> &'static str {
        match self {
            Outcome::Clean => "clean",
            Outcome::Findings => "findings",
            Outcome::Incomplete => "incomplete",
        }
    }
}

/// A provider whose key is set in this environment and is either unrouted
/// (`Direct`, a finding) or unknowable from out here (`InCode`, reported but
/// not counted). Routed and absent providers do not appear: the report is
/// what needs attention, not the whole table.
struct EnvRow {
    provider: &'static Provider,
    status: Routing,
    key_var: &'static str,
    base_url_var: Option<&'static str>,
}

fn env_rows(env: &HashMap<String, String>) -> Vec<EnvRow> {
    let mut rows: Vec<EnvRow> = PROVIDERS
        .iter()
        .filter_map(|p| {
            let status = routing(p, env);
            if !matches!(status, Routing::Direct | Routing::InCode) {
                return None;
            }
            Some(EnvRow {
                provider: p,
                status,
                key_var: key_var(p, env)?,
                base_url_var: base_url_env(p),
            })
        })
        .collect();
    rows.sort_by_key(|r| r.provider.default_port);
    rows
}

/// Addresses turnpike answers on, as they appear in a config file. The port
/// prefix covers 4000-4009 in one needle.
const TURNPIKE_MARKERS: &[&str] = &["127.0.0.1:400", "localhost:400", "[::1]:400"];

fn needles() -> Vec<Needle> {
    PROVIDERS
        .iter()
        .map(|p| {
            let host = p
                .upstream_url
                .strip_prefix("https://")
                .unwrap_or(p.upstream_url);
            Needle {
                label: host,
                bytes: host,
            }
        })
        .collect()
}

/// Findings beat incomplete beats clean — the same priority `check` gives
/// over-budget against unknown. A partial scan that already found something
/// has answered the question.
fn classify(direct: usize, hits: usize, flagged: usize, incomplete: bool) -> Outcome {
    if direct > 0 || hits > 0 || flagged > 0 {
        Outcome::Findings
    } else if incomplete {
        Outcome::Incomplete
    } else {
        Outcome::Clean
    }
}

struct BillRow {
    provider: &'static str,
    status: Status,
}

/// Ask each source that has a key here, compare against the meter, and
/// advance the stored reading. The state file is written once, at the end,
/// and only if something in it changed.
async fn bill_rows(env: &HashMap<String, String>, offline: bool) -> Result<Vec<BillRow>> {
    let state_path = doctor_json();
    let mut state = bill::State::load(&state_path)?;
    let client = reqwest::Client::builder().timeout(bill::TIMEOUT).build()?;
    let now = Timestamp::now().to_string();
    let mut rows = Vec::new();
    let mut changed = false;
    // Opened on first use: a baseline run never touches calls.db.
    let mut meter: Option<(rusqlite::Connection, PriceTable)> = None;

    for src in bill::SOURCES {
        let provider = PROVIDERS
            .iter()
            .find(|p| p.name == src.provider)
            .expect("bill sources name known providers");
        let key = key_var(provider, env).and_then(|v| env.get(v));
        let status = match (offline, key) {
            (true, _) => Status::Offline,
            (false, None) => Status::NoKey,
            (false, Some(key)) => match bill::fetch(&client, src, key).await {
                Err(e) => Status::Unreachable(format!("{e:#}")),
                Ok(reading) => {
                    let prev = state.snapshots.get(src.provider);
                    let (status, next) = bill::assess(prev, reading, &now, |since, until| {
                        if meter.is_none() {
                            meter = Some((open_db(&calls_db())?, PriceTable::load(&prices_json())));
                        }
                        let (conn, prices) = meter.as_ref().expect("just set");
                        spend_between(conn, prices, Some(src.provider), since, Some(until))
                    })?;
                    if !prev.is_some_and(|p| p.ts == next.ts) {
                        state.snapshots.insert(src.provider.to_string(), next);
                        changed = true;
                    }
                    status
                }
            },
        };
        rows.push(BillRow {
            provider: src.provider,
            status,
        });
    }
    if changed {
        state.save(&state_path)?;
    }
    Ok(rows)
}

pub async fn run(opts: DoctorOpts) -> Result<Outcome> {
    let env: HashMap<String, String> = std::env::vars().collect();
    let rows = env_rows(&env);
    let roots = scan::default_roots(&env);
    let mut report = scan::scan(&roots, &needles(), TURNPIKE_MARKERS, &Limits::default());
    // Newest first: the leak is usually the thing installed last.
    report.hits.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.path.cmp(&b.path))
    });
    let bills = bill_rows(&env, opts.offline).await?;

    let direct = rows.iter().filter(|r| r.status == Routing::Direct).count();
    let flagged = bills
        .iter()
        .filter(|b| matches!(&b.status, Status::Compared(c) if c.flagged))
        .count();
    let incomplete = report.truncated || bills.iter().any(|b| b.status.incomplete());
    let outcome = classify(direct, report.hits.len(), flagged, incomplete);
    let home = env.get("HOME").map(String::as_str);

    if opts.json {
        print_json(&rows, &report, &bills, outcome);
    } else {
        print_text(&rows, &report, &bills, home);
    }
    if report.truncated {
        eprintln!(
            "warning: scan stopped after {} — the report is incomplete",
            megabytes(report.bytes)
        );
    }
    Ok(outcome)
}

fn print_text(rows: &[EnvRow], report: &Report, bills: &[BillRow], home: Option<&str>) {
    let direct: Vec<&EnvRow> = rows
        .iter()
        .filter(|r| r.status == Routing::Direct)
        .collect();
    match direct.len() {
        0 => println!("this shell — no unrouted keys"),
        1 => println!("this shell — 1 key not routed"),
        n => println!("this shell — {n} keys not routed"),
    }
    let w_name = rows
        .iter()
        .map(|r| r.provider.name.len())
        .max()
        .unwrap_or(0);
    let mut reasons: Vec<(&EnvRow, String)> = rows
        .iter()
        .map(|r| {
            let reason = match (r.status, r.base_url_var) {
                (Routing::Direct, Some(var)) if std::env::var_os(var).is_some() => {
                    format!("{} set; {var} does not point here", r.key_var)
                }
                (Routing::Direct, Some(var)) => format!("{} set; {var} not set", r.key_var),
                _ => format!(
                    "{} set; base URL lives in code — unknown from here",
                    r.key_var
                ),
            };
            (r, reason)
        })
        .collect();
    let w_reason = reasons.iter().map(|(_, s)| s.len()).max().unwrap_or(0);
    for (r, reason) in reasons.drain(..) {
        match r.status {
            Routing::Direct => println!(
                "  {:<w_name$}  {reason:<w_reason$}  {}",
                r.provider.name,
                base_url(r.provider)
            ),
            _ => println!("  {:<w_name$}  {reason}", r.provider.name),
        }
    }
    if direct.len() > 1
        && direct
            .iter()
            .filter(|r| r.base_url_var == Some("OPENAI_BASE_URL"))
            .count()
            > 1
    {
        println!("  (one OPENAI_BASE_URL routes one provider; the others take base_url in code)");
    }
    println!();

    match report.hits.len() {
        0 => println!("config files — none name a vendor host"),
        1 => println!("config files — 1 names a vendor host"),
        n => println!("config files — {n} name a vendor host"),
    }
    let shown: Vec<(String, String)> = report
        .hits
        .iter()
        .map(|h| (date(h), display_path(&h.path, home)))
        .collect();
    let w_path = shown.iter().map(|(_, p)| p.len()).max().unwrap_or(0);
    for (h, (date, path)) in report.hits.iter().zip(&shown) {
        let mut hosts = h.hosts.join(", ");
        if h.names_turnpike {
            hosts.push_str("  (also names turnpike)");
        }
        println!("  {date}  {path:<w_path$}  {hosts}");
    }
    println!();

    print_bills(bills);

    let roots: Vec<String> = report.roots.iter().map(|r| display_path(r, home)).collect();
    println!(
        "scanned {} — {} files, {}, {:.1} s",
        roots.join(" "),
        report.files,
        megabytes(report.bytes),
        report.elapsed_ms as f64 / 1000.0
    );
    if !report.checkouts.is_empty() {
        let list: Vec<String> = report
            .checkouts
            .iter()
            .map(|c| display_path(c, home))
            .collect();
        println!("skipped source checkouts: {}", list.join(" "));
    }
    println!(
        "this shell is the environment turnpike doctor ran in, not every process on the machine;"
    );
    println!("a file naming a vendor host is where to look, not proof of a leak.");
    if bills
        .iter()
        .any(|b| matches!(b.status, Status::Compared(_)))
    {
        println!("usage is per key; a key used on more than one host over-reports here.");
    }
}

fn print_bills(bills: &[BillRow]) {
    if bills.iter().all(|b| matches!(b.status, Status::Offline)) {
        println!("bill vs meter — skipped (--offline)\n");
        return;
    }
    if bills.iter().all(|b| matches!(b.status, Status::NoKey)) {
        let names: Vec<&str> = bills.iter().map(|b| b.provider).collect();
        println!(
            "bill vs meter — no {} key in this shell\n",
            names.join(" or ")
        );
        return;
    }
    let flagged = bills
        .iter()
        .filter(|b| matches!(&b.status, Status::Compared(c) if c.flagged))
        .count();
    match flagged {
        0 => println!("bill vs meter"),
        1 => println!("bill vs meter — 1 gap flagged"),
        n => println!("bill vs meter — {n} gaps flagged"),
    }
    let w = bills.iter().map(|b| b.provider.len()).max().unwrap_or(0);
    for b in bills {
        let line = match &b.status {
            Status::NoKey => "no key in this shell".to_string(),
            Status::Offline => "skipped (--offline)".to_string(),
            Status::Unreachable(e) => format!("unreachable: {e}"),
            Status::Baseline => "baseline recorded; run again after some spend".to_string(),
            Status::Reset(why) => format!("{why} since last reading; baseline reset"),
            Status::Compared(c) => comparison_line(c),
        };
        println!("  {:<w$}  {line}", b.provider);
    }
    if bills
        .iter()
        .any(|b| matches!(b.status, Status::Compared(_)))
    {
        println!(
            "  (flagged when the gap exceeds {}% of the bill or ${:.2}, whichever is larger)",
            bill::THRESHOLD_PCT,
            bill::THRESHOLD_USD
        );
    }
    println!();
}

fn comparison_line(c: &Comparison) -> String {
    let mut billed: Vec<String> = Vec::new();
    if c.billed > 0.0 || c.foreign.is_empty() {
        billed.push(money("USD", c.billed));
    }
    for (code, amt) in &c.foreign {
        billed.push(money(code, *amt));
    }
    let mut line = format!(
        "{} billed since {}{}   metered ${:.2} ({})",
        billed.join(" + "),
        local_minute(&c.since),
        bill::age(&c.since, &c.until)
            .map(|d| format!(" ({})", window(d)))
            .unwrap_or_default(),
        c.metered,
        c.metered_source
    );
    match c.no_verdict {
        Some("currency_mismatch") => line.push_str("   currencies differ; no verdict"),
        Some(_) => {
            line.push_str(&format!(
                "   {} unpriced calls; no verdict — run `turnpike prices pull`",
                c.unpriced
            ));
        }
        None => {
            line.push_str(&format!("   gap ${:.2}", c.gap));
            if let Some(pct) = c.gap_pct {
                line.push_str(&format!(" ({pct:.0}%)"));
            }
            if c.flagged {
                line.push_str("  flagged");
            }
        }
    }
    line
}

/// A window length at the precision a person reads it: minutes under an
/// hour, hours under two days, days after.
fn window(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 2 * 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

fn money(code: &str, amount: f64) -> String {
    match code {
        "USD" => format!("${amount:.2}"),
        "CNY" => format!("¥{amount:.2}"),
        _ => format!("{amount:.2} {code}"),
    }
}

/// An RFC-3339 instant as a local wall-clock minute; the raw string if it
/// will not parse, which only happens to an edited state file.
fn local_minute(ts: &str) -> String {
    ts.parse::<Timestamp>()
        .map(|t| {
            t.to_zoned(TimeZone::system())
                .strftime("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| ts.to_string())
}

fn print_json(rows: &[EnvRow], report: &Report, bills: &[BillRow], outcome: Outcome) {
    let env: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "provider": r.provider.name,
                "status": r.status.label(),
                "key_var": r.key_var,
                "base_url_var": r.base_url_var,
                "fix": base_url(r.provider),
            })
        })
        .collect();
    let config: Vec<serde_json::Value> = report
        .hits
        .iter()
        .map(|h| {
            serde_json::json!({
                "path": h.path,
                "hosts": h.hosts,
                "modified": h.modified.and_then(|m| Timestamp::try_from(m).ok()).map(|t| t.to_string()),
                "names_turnpike": h.names_turnpike,
            })
        })
        .collect();
    let bill: Vec<serde_json::Value> = bills
        .iter()
        .map(|b| {
            let mut row = serde_json::json!({
                "provider": b.provider,
                "status": b.status.label(),
            });
            match &b.status {
                Status::Unreachable(e) => row["error"] = e.as_str().into(),
                Status::Reset(why) => row["reason"] = (*why).into(),
                Status::Compared(c) => {
                    row["since"] = c.since.as_str().into();
                    row["until"] = c.until.as_str().into();
                    row["billed"] = c.billed.into();
                    row["foreign"] = serde_json::to_value(&c.foreign).expect("map of floats");
                    row["metered"] = c.metered.into();
                    row["metered_source"] = c.metered_source.into();
                    row["unpriced_calls"] = c.unpriced.into();
                    row["gap"] = c.gap.into();
                    row["gap_pct"] = c.gap_pct.into();
                    row["no_verdict"] = c.no_verdict.into();
                    row["flagged"] = c.flagged.into();
                }
                _ => {}
            }
            row
        })
        .collect();
    let out = serde_json::json!({
        "status": outcome.label(),
        "env": env,
        "config": config,
        "bill": bill,
        "scan": {
            "roots": report.roots,
            "skip_dirs": scan::SKIP_DIRS,
            "skip_dir_words": scan::SKIP_DIR_WORDS,
            "skip_file_words": scan::SKIP_FILE_WORDS,
            "skip_exts": scan::SKIP_EXTS,
            "checkouts": report.checkouts,
            "files": report.files,
            "bytes": report.bytes,
            "unreadable": report.unreadable,
            "truncated": report.truncated,
            "elapsed_ms": report.elapsed_ms as u64,
        },
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&out).expect("serializing known-valid JSON")
    );
}

fn date(h: &Hit) -> String {
    h.modified
        .and_then(|m| Timestamp::try_from(m).ok())
        .map(|t| t.strftime("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "          ".to_string())
}

fn display_path(path: &Path, home: Option<&str>) -> String {
    let s = path.to_string_lossy();
    match home {
        Some(h) if !h.is_empty() => match s.strip_prefix(h) {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{rest}"),
            _ => s.into_owned(),
        },
        _ => s.into_owned(),
    }
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn findings_beat_incomplete_beat_clean() {
        assert_eq!(classify(0, 0, 0, false), Outcome::Clean);
        assert_eq!(classify(0, 0, 0, true), Outcome::Incomplete);
        assert_eq!(classify(1, 0, 0, true), Outcome::Findings);
        assert_eq!(classify(0, 1, 0, false), Outcome::Findings);
        assert_eq!(classify(0, 0, 1, true), Outcome::Findings);
    }

    #[test]
    fn only_direct_and_in_code_providers_are_rows() {
        let e = env(&[
            ("DEEPSEEK_API_KEY", "k"),
            ("GEMINI_API_KEY", "k"),
            ("XAI_API_KEY", "k"),
            ("OPENAI_BASE_URL", "http://127.0.0.1:4008/v1"),
        ]);
        let rows = env_rows(&e);
        let got: Vec<(&str, Routing)> = rows.iter().map(|r| (r.provider.name, r.status)).collect();
        // Port order: gemini 4002, deepseek 4003; xai (4008) is routed and absent.
        assert_eq!(
            got,
            vec![("gemini", Routing::InCode), ("deepseek", Routing::Direct)]
        );
        assert_eq!(rows[1].key_var, "DEEPSEEK_API_KEY");
        assert_eq!(rows[1].base_url_var, Some("OPENAI_BASE_URL"));
    }

    #[test]
    fn needles_are_bare_upstream_hosts() {
        let n = needles();
        let labels: Vec<&str> = n.iter().map(|n| n.label).collect();
        assert!(labels.contains(&"api.deepseek.com"));
        assert!(labels.contains(&"openrouter.ai"));
        assert!(labels.iter().all(|l| !l.contains("://")));
    }

    #[test]
    fn a_comparison_line_names_the_meter_source_and_the_verdict() {
        let mut c = bill::compare(
            "2026-09-04T14:02:00Z",
            "2026-09-11T00:00:00Z",
            &std::collections::BTreeMap::from([("USD".to_string(), 2.93)]),
            &crate::cost::Spend {
                total: 1.65,
                unpriced: 0,
                reported: 4,
                computed: 0,
            },
        );
        let line = comparison_line(&c);
        assert!(line.starts_with("$2.93 billed since 2026-09-04"), "{line}");
        assert!(line.contains(" (6d)   metered"), "{line}");
        assert!(line.contains("metered $1.65 (provider-reported)"), "{line}");
        assert!(line.ends_with("gap $1.28 (44%)  flagged"), "{line}");

        c.foreign.insert("CNY".to_string(), 18.4);
        c.billed = 0.0;
        c.no_verdict = Some("currency_mismatch");
        let line = comparison_line(&c);
        assert!(line.starts_with("¥18.40 billed"), "{line}");
        assert!(line.ends_with("currencies differ; no verdict"), "{line}");
    }

    #[test]
    fn home_is_shown_as_tilde() {
        assert_eq!(
            display_path(Path::new("/h/.config/x"), Some("/h")),
            "~/.config/x"
        );
        assert_eq!(display_path(Path::new("/h"), Some("/h")), "~");
        // A sibling that merely shares the prefix is not home.
        assert_eq!(
            display_path(Path::new("/home2/x"), Some("/home")),
            "/home2/x"
        );
        assert_eq!(display_path(Path::new("/h/x"), None), "/h/x");
    }
}
