use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDataDir(PathBuf);

impl TestDataDir {
    fn new() -> Self {
        let id = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("turnpike-check-test-{}-{id}", std::process::id()));
        std::fs::create_dir_all(path.join("turnpike")).unwrap();
        Self(path)
    }

    fn db_path(&self) -> PathBuf {
        self.0.join("turnpike/calls.db")
    }
}

impl Drop for TestDataDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn create_db(path: &Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE calls (
            ts TEXT NOT NULL,
            model TEXT,
            input_tokens INTEGER,
            output_tokens INTEGER,
            cache_read_input_tokens INTEGER,
            cache_creation_input_tokens INTEGER,
            cost REAL
        );",
    )
    .unwrap();
    conn
}

fn run_check(data: &TestDataDir, budget: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_turnpike"))
        .args(["check", "--budget", budget, "--quiet"])
        .env("XDG_DATA_HOME", &data.0)
        .output()
        .unwrap()
}

fn run_check_json(data: &TestDataDir, budget: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_turnpike"))
        .args(["check", "--budget", budget, "--json"])
        .env("XDG_DATA_HOME", &data.0)
        .output()
        .unwrap()
}

fn data_with_cost(cost: f64) -> TestDataDir {
    let data = TestDataDir::new();
    let conn = create_db(&data.db_path());
    conn.execute(
        "INSERT INTO calls VALUES ('2026-07-20T10:00:00Z', 'gpt-x', 100, 50, 0, 0, ?1)",
        [cost],
    )
    .unwrap();
    data
}

#[test]
fn known_spend_below_budget_exits_zero() {
    let data = data_with_cost(2.0);
    let output = run_check(&data, "3/2020-01-01");

    assert_eq!(output.status.code(), Some(0));
}

#[test]
fn known_spend_equal_to_budget_exits_one() {
    let data = data_with_cost(2.0);
    let output = run_check(&data, "2/2020-01-01");

    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn known_spend_over_budget_exits_one() {
    let data = data_with_cost(2.0);
    let output = run_check(&data, "1/2020-01-01");

    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn unpriced_spend_below_budget_exits_three_unknown() {
    let data = TestDataDir::new();
    let conn = create_db(&data.db_path());
    conn.execute_batch(
        "INSERT INTO calls VALUES
            ('2026-07-20T10:00:00Z', 'unknown-model', 100, 50, 0, 0, NULL);",
    )
    .unwrap();
    let output = run_check(&data, "3/2020-01-01");

    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("real spend may be higher"));
}

#[test]
fn unpriced_spend_reports_unknown_status_and_reason_in_json() {
    let data = TestDataDir::new();
    let conn = create_db(&data.db_path());
    conn.execute_batch(
        "INSERT INTO calls VALUES
            ('2026-07-20T10:00:00Z', 'unknown-model', 100, 50, 0, 0, NULL);",
    )
    .unwrap();
    let output = run_check_json(&data, "3/2020-01-01");

    assert_eq!(output.status.code(), Some(3));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(r#""status":"unknown"#));
    assert!(stdout.contains(r#""reason":"unpriced_calls"#));
}

#[test]
fn known_spend_over_budget_remains_decisive_with_unpriced_calls() {
    let data = data_with_cost(2.0);
    let conn = Connection::open(data.db_path()).unwrap();
    conn.execute_batch(
        "INSERT INTO calls VALUES
            ('2026-07-20T11:00:00Z', 'unknown-model', 100, 50, 0, 0, NULL);",
    )
    .unwrap();
    let output = run_check(&data, "1/2020-01-01");

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("real spend may be higher"));
}

#[test]
fn malformed_database_row_exits_two() {
    let data = TestDataDir::new();
    let conn = create_db(&data.db_path());
    conn.execute_batch(
        "INSERT INTO calls VALUES
            ('2026-07-20T10:00:00Z', 'gpt-x', 'not-an-integer', 50, 0, 0, 0.12);",
    )
    .unwrap();
    let output = run_check(&data, "3/2020-01-01");

    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn missing_database_exits_three_unknown() {
    let data = TestDataDir::new();
    let output = run_check(&data, "3/2020-01-01");

    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("is turnpike running"));
}

#[test]
fn missing_database_reports_unknown_status_and_reason_in_json() {
    let data = TestDataDir::new();
    let output = run_check_json(&data, "50/2020-01-01");

    assert_eq!(output.status.code(), Some(3));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(r#""status":"unknown"#));
    assert!(stdout.contains(r#""reason":"no_data"#));
}

#[test]
fn malformed_budget_exits_two() {
    let data = TestDataDir::new();
    let output = run_check(&data, "nonsense");

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("bad budget amount"));
}
