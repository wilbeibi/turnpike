use anyhow::Result;
use log::warn;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Default, Clone)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub reasoning_output_tokens: Option<u64>,
    /// Provider-reported billed cost in USD (e.g. OpenRouter's `usage.cost`).
    pub cost: Option<f64>,
}

impl Usage {
    pub fn merge(&mut self, other: &Usage) {
        macro_rules! take {
            ($field:ident) => {
                if let Some(v) = other.$field {
                    self.$field = Some(v);
                }
            };
        }
        take!(input_tokens);
        take!(output_tokens);
        take!(cache_read_input_tokens);
        take!(cache_creation_input_tokens);
        take!(reasoning_output_tokens);
        take!(cost);
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub ts: String,
    pub provider: String,
    pub model: Option<String>,
    pub status: Option<u16>,
    pub latency_ms: u64,
    pub ttft_ms: Option<u64>,
    pub stream: bool,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub reasoning_output_tokens: Option<u64>,
    pub error_kind: Option<String>,
    pub error_message: Option<String>,
    pub cost: Option<f64>,
    /// Calling tool identity: `x-turnpike-client` header when set, else the
    /// request `User-Agent`. Stored verbatim (truncated at capture).
    #[serde(default)]
    pub client: Option<String>,
    /// Request path without query, e.g. `/v1/responses`. Distinguishes
    /// chat vs responses vs embeddings vs transcriptions per row.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Set when observation was degraded (`sse_overflow`,
    /// `observation_dropped`) or when a successful inference reported no usage
    /// at all (`no_usage`): the token fields are untrustworthy/absent for a
    /// turnpike-side or provider-shape reason, not a genuine zero-cost call.
    #[serde(default)]
    pub anomaly: Option<String>,
    /// Verbatim provider usage object(s) as sent on the wire, before turnpike
    /// normalized them into the typed columns — a single JSON object for
    /// most providers, a JSON array when usage arrives split across events
    /// (Anthropic `message_start` + `message_delta`). Audit/backfill only;
    /// preserves fields turnpike does not parse. `None` when no usage was seen.
    #[serde(default)]
    pub raw_usage: Option<String>,
    /// Absolute path of the local process that opened the connection, resolved
    /// passively from the peer socket via `/proc` (Linux only; `None` off
    /// Linux, or when the process exited before the record was written).
    /// Distinct from `client`: this is what turnpike *observed*, where `client` is
    /// what the caller *declared*.
    #[serde(default)]
    pub peer_exe: Option<String>,
    /// Provenance of `client`: `"header"` when it came from
    /// `x-turnpike-client` (identity the caller *declared*), `"ua"` when it
    /// is the `User-Agent` fallback. Read-time attribution ranks the two;
    /// `None` on rows written before this column existed (legacy order:
    /// client, else peer_exe).
    #[serde(default)]
    pub client_source: Option<String>,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        let store = Self { conn };
        store.init()?;
        Ok(store)
    }

    pub fn checkpoint(&self) {
        if let Err(e) = self
            .conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA optimize;")
        {
            warn!("turnpike: shutdown checkpoint failed: {e}");
        }
    }

    fn init(&self) -> Result<()> {
        self.conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA busy_timeout=5000;
             PRAGMA cache_size=-8000;
             PRAGMA temp_store=MEMORY;
             PRAGMA mmap_size=67108864;",
        )?;
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS calls (
                id                          TEXT PRIMARY KEY,
                ts                          TEXT NOT NULL,
                provider                    TEXT NOT NULL,
                model                       TEXT,
                status                      INTEGER,
                latency_ms                  INTEGER NOT NULL,
                ttft_ms                     INTEGER,
                stream                      INTEGER NOT NULL DEFAULT 0,
                input_tokens                INTEGER,
                output_tokens               INTEGER,
                cache_read_input_tokens     INTEGER,
                cache_creation_input_tokens INTEGER,
                reasoning_output_tokens     INTEGER,
                error_kind                  TEXT,
                error_message               TEXT,
                cost                        REAL,
                client                      TEXT,
                endpoint                    TEXT,
                anomaly                     TEXT,
                raw_usage                   TEXT,
                peer_exe                    TEXT,
                client_source               TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_ts       ON calls(ts);
            CREATE INDEX IF NOT EXISTS idx_provider ON calls(provider);
            CREATE INDEX IF NOT EXISTS idx_model    ON calls(model);",
        )?;
        // Forward-migrate databases created before a column existed
        // (invariant 4: new fields are optional, appended, migrated).
        self.add_column("ALTER TABLE calls ADD COLUMN client TEXT")?;
        self.add_column("ALTER TABLE calls ADD COLUMN endpoint TEXT")?;
        self.add_column("ALTER TABLE calls ADD COLUMN anomaly TEXT")?;
        self.add_column("ALTER TABLE calls ADD COLUMN raw_usage TEXT")?;
        self.add_column("ALTER TABLE calls ADD COLUMN peer_exe TEXT")?;
        self.add_column("ALTER TABLE calls ADD COLUMN client_source TEXT")?;
        Ok(())
    }

    /// Apply an ADD COLUMN migration; "duplicate column name" means the
    /// column already exists and is success, not failure.
    fn add_column(&self, ddl: &str) -> Result<()> {
        match self.conn.execute_batch(ddl) {
            Ok(()) => Ok(()),
            Err(e) if e.to_string().contains("duplicate column name") => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn insert(&self, r: &Record) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO calls (
                id, ts, provider, model, status,
                latency_ms, ttft_ms, stream,
                input_tokens, output_tokens,
                cache_read_input_tokens, cache_creation_input_tokens,
                reasoning_output_tokens,
                error_kind, error_message, cost, client, endpoint, anomaly, raw_usage,
                peer_exe, client_source
            ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22)",
            params![
                r.id,
                r.ts,
                r.provider,
                r.model,
                r.status,
                r.latency_ms as i64,
                r.ttft_ms.map(|v| v as i64),
                r.stream as i64,
                r.input_tokens.map(|v| v as i64),
                r.output_tokens.map(|v| v as i64),
                r.cache_read_input_tokens.map(|v| v as i64),
                r.cache_creation_input_tokens.map(|v| v as i64),
                r.reasoning_output_tokens.map(|v| v as i64),
                r.error_kind,
                r.error_message,
                r.cost,
                r.client,
                r.endpoint,
                r.anomaly,
                r.raw_usage,
                r.peer_exe,
                r.client_source,
            ],
        )?;
        Ok(())
    }
}

/// Open a read-only connection for stats/tail. Intentionally uses a smaller
/// pragma set than Store::init — no write-tuning (cache_size, temp_store,
/// mmap_size) needed for query-only paths.
pub fn open_db(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA busy_timeout=5000;",
    )?;
    Ok(conn)
}

/// Whether the `calls` table has `col`. Read paths (stats/tail) never
/// migrate, so a column appended after a reader's DB was created must be
/// referenced behind this guard — old databases stay readable (invariant 4).
pub fn has_column(conn: &Connection, col: &str) -> Result<bool> {
    Ok(conn
        .prepare("SELECT 1 FROM pragma_table_info('calls') WHERE name = ?1")?
        .exists([col])?)
}

const ERROR_PATTERNS: &[(&str, &[&str])] = &[
    ("upstream_tls", &["tls", "ssl", "certificate", "handshake"]),
    ("upstream_timeout", &["timeout", "timed out", "deadline"]),
    (
        "client_disconnect",
        &["client disconnect", "connection reset", "broken pipe"],
    ),
    (
        "upstream_connect",
        &["connect", "refused", "unreachable", "no route", "dns"],
    ),
];

pub fn classify_error(message: &str) -> &'static str {
    let low = message.to_lowercase();
    for (kind, needles) in ERROR_PATTERNS {
        if needles.iter().any(|n| low.contains(n)) {
            return kind;
        }
    }
    "other"
}

#[cfg(test)]
impl Store {
    pub(crate) fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let store = Self { conn };
        store.init()?;
        Ok(store)
    }

    fn count(&self) -> i64 {
        self.conn
            .query_row("SELECT COUNT(*) FROM calls", [], |r| r.get(0))
            .unwrap_or(0)
    }

    fn get_by_id(&self, id: &str) -> Option<Record> {
        self.conn
            .query_row(
                "SELECT id, ts, provider, model, status,
                        latency_ms, ttft_ms, stream,
                        input_tokens, output_tokens,
                        cache_read_input_tokens, cache_creation_input_tokens,
                        reasoning_output_tokens,
                        error_kind, error_message, cost, client, endpoint, anomaly, raw_usage,
                        peer_exe, client_source
                 FROM calls WHERE id = ?1",
                [id],
                |row| {
                    Ok(Record {
                        id: row.get(0)?,
                        ts: row.get(1)?,
                        provider: row.get(2)?,
                        model: row.get(3)?,
                        status: row.get::<_, Option<u16>>(4)?,
                        latency_ms: row.get::<_, i64>(5)? as u64,
                        ttft_ms: row.get::<_, Option<i64>>(6)?.map(|v| v as u64),
                        stream: row.get::<_, i64>(7)? != 0,
                        input_tokens: row.get::<_, Option<i64>>(8)?.map(|v| v as u64),
                        output_tokens: row.get::<_, Option<i64>>(9)?.map(|v| v as u64),
                        cache_read_input_tokens: row.get::<_, Option<i64>>(10)?.map(|v| v as u64),
                        cache_creation_input_tokens: row
                            .get::<_, Option<i64>>(11)?
                            .map(|v| v as u64),
                        reasoning_output_tokens: row.get::<_, Option<i64>>(12)?.map(|v| v as u64),
                        error_kind: row.get(13)?,
                        error_message: row.get(14)?,
                        cost: row.get::<_, Option<f64>>(15)?,
                        client: row.get(16)?,
                        endpoint: row.get(17)?,
                        anomaly: row.get(18)?,
                        raw_usage: row.get(19)?,
                        peer_exe: row.get(20)?,
                        client_source: row.get(21)?,
                    })
                },
            )
            .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_error_kinds_and_ordering() {
        // Covers every pattern family plus the ordering traps: a connection
        // reset must classify as client_disconnect even though it contains
        // "connect", and a refusal must reach upstream_connect.
        for (message, kind) in [
            ("tls handshake failed", "upstream_tls"),
            ("request timed out", "upstream_timeout"),
            ("connection reset by peer", "client_disconnect"),
            ("broken pipe", "client_disconnect"),
            ("connection refused", "upstream_connect"),
            ("no route to host", "upstream_connect"),
            ("unexplained failure", "other"),
        ] {
            assert_eq!(classify_error(message), kind, "{message:?}");
        }
    }

    #[test]
    fn usage_merge_fills_none_fields() {
        let mut base = Usage {
            input_tokens: Some(100),
            output_tokens: None,
            ..Default::default()
        };
        let delta = Usage {
            output_tokens: Some(50),
            ..Default::default()
        };
        base.merge(&delta);
        assert_eq!(base.input_tokens, Some(100));
        assert_eq!(base.output_tokens, Some(50));
    }

    #[test]
    fn usage_merge_last_write_wins() {
        let mut base = Usage {
            input_tokens: Some(100),
            ..Default::default()
        };
        let delta = Usage {
            input_tokens: Some(200),
            ..Default::default()
        };
        base.merge(&delta);
        assert_eq!(base.input_tokens, Some(200));
    }

    fn sample_record(id: &str) -> Record {
        Record {
            id: id.into(),
            ts: "2024-01-01T00:00:00Z".into(),
            provider: "openai".into(),
            model: Some("gpt-4o".into()),
            status: Some(200),
            latency_ms: 800,
            ttft_ms: Some(120),
            stream: false,
            input_tokens: Some(50),
            output_tokens: Some(25),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            reasoning_output_tokens: None,
            error_kind: None,
            error_message: None,
            cost: Some(0.000375),
            client: Some("test-agent/1.0".into()),
            endpoint: Some("/v1/chat/completions".into()),
            anomaly: None,
            raw_usage: Some(r#"{"prompt_tokens":50,"completion_tokens":25}"#.into()),
            peer_exe: Some("/usr/bin/test-tool".into()),
            client_source: Some("header".into()),
        }
    }

    #[test]
    fn store_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        let rec = sample_record("abc123");
        store.insert(&rec).unwrap();

        let back = store.get_by_id("abc123").unwrap();
        assert_eq!(back.id, rec.id);
        assert_eq!(back.ts, rec.ts);
        assert_eq!(back.provider, rec.provider);
        assert_eq!(back.model, rec.model);
        assert_eq!(back.status, rec.status);
        assert_eq!(back.latency_ms, rec.latency_ms);
        assert_eq!(back.ttft_ms, rec.ttft_ms);
        assert_eq!(back.stream, rec.stream);
        assert_eq!(back.input_tokens, rec.input_tokens);
        assert_eq!(back.output_tokens, rec.output_tokens);
        assert_eq!(back.cache_read_input_tokens, rec.cache_read_input_tokens);
        assert_eq!(
            back.cache_creation_input_tokens,
            rec.cache_creation_input_tokens
        );
        assert_eq!(back.reasoning_output_tokens, rec.reasoning_output_tokens);
        assert_eq!(back.error_kind, rec.error_kind);
        assert_eq!(back.error_message, rec.error_message);
        assert_eq!(back.cost, rec.cost);
        assert_eq!(back.client, rec.client);
        assert_eq!(back.endpoint, rec.endpoint);
        assert_eq!(back.anomaly, rec.anomaly);
        assert_eq!(back.raw_usage, rec.raw_usage);
        assert_eq!(back.peer_exe, rec.peer_exe);
        assert_eq!(back.client_source, rec.client_source);
    }

    #[test]
    fn migration_adds_client_column_to_pre_client_db() {
        // A database created by a build that predates the client column
        // must gain it on open, and init must stay idempotent.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE calls (
                id TEXT PRIMARY KEY, ts TEXT NOT NULL, provider TEXT NOT NULL,
                model TEXT, status INTEGER, latency_ms INTEGER NOT NULL,
                ttft_ms INTEGER, stream INTEGER NOT NULL DEFAULT 0,
                input_tokens INTEGER, output_tokens INTEGER,
                cache_read_input_tokens INTEGER, cache_creation_input_tokens INTEGER,
                reasoning_output_tokens INTEGER, error_kind TEXT,
                error_message TEXT, cost REAL
            );",
        )
        .unwrap();
        let store = Store { conn };
        store.init().unwrap();
        store.init().unwrap();
        store.insert(&sample_record("migrated")).unwrap();
        let back = store.get_by_id("migrated").unwrap();
        assert_eq!(back.client.as_deref(), Some("test-agent/1.0"));
        // The newest appended column must also forward-migrate onto a DB
        // created before it existed (invariant 4).
        assert_eq!(back.peer_exe.as_deref(), Some("/usr/bin/test-tool"));
        assert_eq!(back.client_source.as_deref(), Some("header"));
    }

    #[test]
    fn has_column_reports_schema_state() {
        let store = Store::open_in_memory().unwrap();
        assert!(has_column(&store.conn, "client_source").unwrap());
        assert!(has_column(&store.conn, "peer_exe").unwrap());
        assert!(!has_column(&store.conn, "does_not_exist").unwrap());

        // A table created by an older build lacks appended columns entirely.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE calls (id TEXT PRIMARY KEY)")
            .unwrap();
        assert!(!has_column(&conn, "client_source").unwrap());
    }

    #[test]
    fn store_insert_or_ignore_duplicate() {
        let store = Store::open_in_memory().unwrap();
        store.insert(&sample_record("dup")).unwrap();
        store.insert(&sample_record("dup")).unwrap();
        assert_eq!(store.count(), 1);
    }
}
