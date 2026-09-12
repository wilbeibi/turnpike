//! The config scan: walk the places a tool keeps its configuration and find
//! every file that names a vendor host. A byte search, deliberately — the
//! leak this exists to find lived in a browser extension's LevelDB, which no
//! format-aware parser would have opened. It does not read any tool's config
//! as config; it reports a path and a hostname and lets the user recognise
//! the file. That is what keeps it from becoming a per-tool tweak generator.
//!
//! Every dimension is bounded (the spirit of DESIGN.md invariant 3): file
//! size, directory depth, and total bytes read. Past the total the scan stops
//! and says so, which the caller turns into an *incomplete* verdict rather
//! than a clean one.

use aho_corasick::{AhoCorasick, AhoCorasickKind, MatchKind};
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

/// Something to look for and the name to report it under.
pub struct Needle {
    pub label: &'static str,
    pub bytes: &'static str,
}

pub struct Limits {
    /// Files larger than this are skipped, never partially read.
    pub max_file_bytes: u64,
    /// Depth below a root at which directories are no longer entered.
    pub max_depth: usize,
    /// Total bytes read across the whole walk before it gives up.
    pub max_total_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_file_bytes: 8 << 20,
            max_depth: 8,
            max_total_bytes: 512 << 20,
        }
    }
}

/// Directory names never entered. Caches and transcripts would each produce
/// hundreds of hits that are copies of someone else's configuration, and
/// installed SDKs (`node_modules`, `site-packages`) carry their vendor's
/// default host by construction.
pub const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "site-packages",
    ".venv",
    "venv",
    "__pycache__",
    "Service Worker",
    "blob_storage",
    "IndexedDB",
    "projects",
    "sessions",
    "file-history",
    "transcripts",
    "checkpoints",
    ".archive",
    "logs",
    "log",
    "tmp",
    ".tmp",
    "temp",
];

/// A directory whose name contains one of these is a copy of something
/// else: `cache`, `Code Cache`, `.curator_backups`, `state-snapshots`.
pub const SKIP_DIR_WORDS: &[&str] = &["cache", "backup", "snapshot"];

/// A file whose name contains one of these (or ends in `~`) is a copy
/// nothing reads: `opencode.json.bak.20260718`, `models_dev_cache.json`.
pub const SKIP_FILE_WORDS: &[&str] = &[".bak", ".orig", ".old", "cache"];

/// Extensions never read: prose, logs, databases, archives, media, and
/// compiled code. A README names every host; a transcript names every host
/// anyone typed; a state database is a transcript with an index.
///
/// `log` is on the list even though LevelDB keeps its newest writes in
/// `NNNNNN.log`: application logs are far more common, and LevelDB folds
/// the log into an `.ldb` on the next open.
pub const SKIP_EXTS: &[&str] = &[
    "md",
    "mdx",
    "rst",
    "txt",
    "html",
    "htm",
    "log",
    "jsonl",
    "db",
    "db-wal",
    "db-shm",
    "db-journal",
    "sqlite",
    "sqlite-wal",
    "sqlite-shm",
    "sqlite3",
    "sqlite3-wal",
    "sqlite3-shm",
    "gz",
    "zip",
    "tar",
    "xz",
    "zst",
    "bz2",
    "png",
    "jpg",
    "jpeg",
    "gif",
    "webp",
    "svg",
    "ico",
    "mp3",
    "mp4",
    "wav",
    "pdf",
    "woff",
    "woff2",
    "ttf",
    "otf",
    "so",
    "dylib",
    "dll",
    "node",
    "wasm",
    "pyc",
    "o",
    "a",
    "rlib",
];

/// A directory with a `.git` and one of these is a source checkout — an
/// SDK, a plugin, an agent's own repository — and names its vendor's host
/// because that is what the code does. Skipped below the roots only: a
/// dotfiles repository at `~/.config` is exactly what should be read.
pub const PROJECT_MARKERS: &[&str] = &[
    "pyproject.toml",
    "setup.py",
    "package.json",
    "Cargo.toml",
    "go.mod",
];

pub struct Hit {
    pub path: PathBuf,
    /// Needle labels found in the file, in needle order.
    pub hosts: Vec<&'static str>,
    pub modified: Option<SystemTime>,
    /// The file also names a turnpike address, so the vendor host may be a
    /// comment, a fallback, or an already-fixed line. Worth a glance, not an
    /// alarm.
    pub names_turnpike: bool,
}

pub struct Report {
    /// The roots that existed and were walked.
    pub roots: Vec<PathBuf>,
    pub hits: Vec<Hit>,
    pub files: u64,
    pub bytes: u64,
    /// Files that could not be read (permissions, races). Counted, never
    /// fatal — a scan is a best effort over someone else's files.
    pub unreadable: u64,
    /// Source checkouts found below a root and not entered.
    pub checkouts: Vec<PathBuf>,
    pub truncated: bool,
    pub elapsed_ms: u128,
}

/// Where configuration lives, given an environment. Shell files are single
/// files; the rest are trees. Paths that do not exist are dropped by
/// [`scan`], so this can name everything a machine might have.
pub fn default_roots(env: &HashMap<String, String>) -> Vec<PathBuf> {
    let Some(home) = env.get("HOME").filter(|h| !h.is_empty()).map(PathBuf::from) else {
        return Vec::new();
    };
    let config = env
        .get("XDG_CONFIG_HOME")
        .filter(|c| !c.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));

    let mut roots = vec![config];
    // Shell startup files: what `eval $(turnpike config ...)` was pasted into,
    // or a hard-coded upstream export.
    for f in [
        ".zshenv",
        ".zshrc",
        ".zprofile",
        ".bashrc",
        ".bash_profile",
        ".profile",
        ".env",
    ] {
        roots.push(home.join(f));
    }
    // Agent and tool homes outside XDG.
    for d in [
        ".claude",
        ".codex",
        ".hermes",
        ".gemini",
        ".continue",
        ".cursor",
        ".aider",
        ".aider.conf.yml",
        ".opencode",
        ".local/bin",
    ] {
        roots.push(home.join(d));
    }
    // macOS: GUI apps, extension storage, and login agents.
    for d in [
        "Library/Application Support",
        "Library/LaunchAgents",
        "Library/Preferences",
    ] {
        roots.push(home.join(d));
    }
    roots
}

/// Walk `roots`, report every file that contains any `hosts` needle. A file
/// is also checked against `turnpike_markers` so the report can say when the
/// vendor host sits next to a turnpike address.
pub fn scan(
    roots: &[PathBuf],
    hosts: &[Needle],
    turnpike_markers: &[&str],
    limits: &Limits,
) -> Report {
    let started = Instant::now();
    let patterns: Vec<&str> = hosts
        .iter()
        .map(|n| n.bytes)
        .chain(turnpike_markers.iter().copied())
        .collect();
    // No needle is a substring of another, so a non-overlapping search finds
    // the same set and lets the automaton use its fast paths. The DFA is
    // tiny for a dozen hosts and turns the scan from CPU-bound to IO-bound.
    let ac = AhoCorasick::builder()
        .kind(Some(AhoCorasickKind::DFA))
        .match_kind(MatchKind::LeftmostFirst)
        .build(&patterns)
        .expect("static patterns compile");
    let host_count = hosts.len();

    let mut report = Report {
        roots: Vec::new(),
        hits: Vec::new(),
        files: 0,
        bytes: 0,
        unreadable: 0,
        checkouts: Vec::new(),
        truncated: false,
        elapsed_ms: 0,
    };

    let mut walker = Walker {
        ac: &ac,
        hosts,
        host_count,
        limits,
        report: &mut report,
    };
    'roots: for root in roots {
        // Symlinked roots are followed on purpose (`~/.config` itself may be
        // one); symlinks *inside* a tree are not, so a link back up cannot
        // loop the walk and a link out of it cannot widen it.
        let Ok(meta) = fs::metadata(root) else {
            continue;
        };
        walker.report.roots.push(root.clone());
        let stop = if meta.is_dir() {
            walker.dir(root, 0)
        } else {
            walker.file(root, &meta)
        };
        if stop {
            walker.report.truncated = true;
            break 'roots;
        }
    }
    report.elapsed_ms = started.elapsed().as_millis();
    report
}

struct Walker<'a> {
    ac: &'a AhoCorasick,
    hosts: &'a [Needle],
    host_count: usize,
    limits: &'a Limits,
    report: &'a mut Report,
}

impl Walker<'_> {
    /// Returns true when the total-bytes limit was hit and the walk must stop.
    fn dir(&mut self, dir: &Path, depth: usize) -> bool {
        if depth > self.limits.max_depth {
            return false;
        }
        let Ok(entries) = fs::read_dir(dir) else {
            self.report.unreadable += 1;
            return false;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            // `symlink_metadata` so a link is seen as a link and skipped.
            let Ok(meta) = fs::symlink_metadata(&path) else {
                self.report.unreadable += 1;
                continue;
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if meta.is_dir() {
                if skip_dir(&name) {
                    continue;
                }
                if is_checkout(&path) {
                    self.report.checkouts.push(path);
                    continue;
                }
                if self.dir(&path, depth + 1) {
                    return true;
                }
            } else if meta.is_file() {
                if skip_file(&name) {
                    continue;
                }
                if self.file(&path, &meta) {
                    return true;
                }
            }
        }
        false
    }

    fn file(&mut self, path: &Path, meta: &fs::Metadata) -> bool {
        if skip_ext(path) || meta.len() > self.limits.max_file_bytes {
            return false;
        }
        if self.report.bytes + meta.len() > self.limits.max_total_bytes {
            return true;
        }
        let Some(data) = read_unless_executable(path) else {
            self.report.unreadable += 1;
            return false;
        };
        let Some(data) = data else {
            return false;
        };
        self.report.files += 1;
        self.report.bytes += data.len() as u64;

        let mut seen = vec![false; self.host_count];
        let mut names_turnpike = false;
        for m in self.ac.find_iter(&data) {
            let id = m.pattern().as_usize();
            if id < self.host_count {
                seen[id] = true;
            } else {
                names_turnpike = true;
            }
        }
        let hosts: Vec<&'static str> = seen
            .iter()
            .enumerate()
            .filter(|(_, s)| **s)
            .map(|(i, _)| self.hosts[i].label)
            .collect();
        if !hosts.is_empty() {
            self.report.hits.push(Hit {
                path: path.to_path_buf(),
                hosts,
                modified: meta.modified().ok(),
                names_turnpike,
            });
        }
        false
    }
}

fn skip_dir(name: &str) -> bool {
    if SKIP_DIRS.contains(&name) {
        return true;
    }
    let lower = name.to_ascii_lowercase();
    SKIP_DIR_WORDS.iter().any(|w| lower.contains(w))
}

fn skip_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with('~') || SKIP_FILE_WORDS.iter().any(|w| lower.contains(w))
}

fn is_checkout(dir: &Path) -> bool {
    dir.join(".git").exists() && PROJECT_MARKERS.iter().any(|m| dir.join(m).exists())
}

/// `Ok(None)` for a compiled executable — an SDK's or turnpike's own binary
/// names every host it can talk to and nobody edits it — `Err` for a file
/// that could not be read.
fn read_unless_executable(path: &Path) -> Option<Option<Vec<u8>>> {
    let mut f = fs::File::open(path).ok()?;
    let mut head = [0u8; 4];
    let n = f.read(&mut head).ok()?;
    if n == 4 && is_executable_image(&head) {
        return Some(None);
    }
    let mut data = head[..n].to_vec();
    f.read_to_end(&mut data).ok()?;
    Some(Some(data))
}

/// ELF, Mach-O (both byte orders, 32- and 64-bit), and a Mach-O fat binary.
fn is_executable_image(head: &[u8; 4]) -> bool {
    matches!(
        head,
        [0x7f, b'E', b'L', b'F']
            | [0xfe, 0xed, 0xfa, 0xce]
            | [0xfe, 0xed, 0xfa, 0xcf]
            | [0xce, 0xfa, 0xed, 0xfe]
            | [0xcf, 0xfa, 0xed, 0xfe]
            | [0xca, 0xfe, 0xba, 0xbe]
    )
}

fn skip_ext(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        let lower = e.to_ascii_lowercase();
        SKIP_EXTS.contains(&lower.as_str())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn needles() -> Vec<Needle> {
        vec![
            Needle {
                label: "api.deepseek.com",
                bytes: "api.deepseek.com",
            },
            Needle {
                label: "openrouter.ai",
                bytes: "openrouter.ai",
            },
        ]
    }

    fn tmp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "turnpike-scan-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::File::create(path).unwrap().write_all(bytes).unwrap();
    }

    fn labels(r: &Report) -> Vec<(String, Vec<&'static str>)> {
        r.hits
            .iter()
            .map(|h| {
                (
                    h.path.file_name().unwrap().to_string_lossy().into_owned(),
                    h.hosts.clone(),
                )
            })
            .collect()
    }

    #[test]
    fn finds_hosts_in_text_and_mid_binary_files() {
        let root = tmp();
        write(
            &root.join("opencode.json"),
            br#"{"baseURL": "https://api.deepseek.com/v1"}"#,
        );
        let mut blob = vec![0u8; 4096];
        blob.extend_from_slice(b"\x00\x01https://openrouter.ai/api/v1\xff\xfe");
        blob.extend(vec![7u8; 4096]);
        write(&root.join("ext/000005.ldb"), &blob);
        write(
            &root.join("clean.json"),
            br#"{"baseURL": "http://127.0.0.1:4003/v1"}"#,
        );

        let r = scan(
            std::slice::from_ref(&root),
            &needles(),
            &["127.0.0.1:400"],
            &Limits::default(),
        );
        let mut got = labels(&r);
        got.sort();
        assert_eq!(
            got,
            vec![
                ("000005.ldb".to_string(), vec!["openrouter.ai"]),
                ("opencode.json".to_string(), vec!["api.deepseek.com"]),
            ]
        );
        assert!(!r.truncated);
        assert_eq!(r.files, 3);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_file_naming_both_is_marked_as_naming_turnpike() {
        let root = tmp();
        write(
            &root.join("mixed.toml"),
            b"# was https://api.deepseek.com\nbase = \"http://127.0.0.1:4003/v1\"\n",
        );
        let r = scan(
            std::slice::from_ref(&root),
            &needles(),
            &["127.0.0.1:400"],
            &Limits::default(),
        );
        assert_eq!(r.hits.len(), 1);
        assert!(r.hits[0].names_turnpike);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn skips_denied_dirs_extensions_symlinks_and_oversized_files() {
        let root = tmp();
        let host = b"https://api.deepseek.com";
        write(&root.join("node_modules/openai/index.js"), host);
        write(&root.join("README.md"), host);
        write(&root.join("session.jsonl"), host);
        write(&root.join("state.db-wal"), host);
        write(&root.join("cache/models.json"), host);
        write(&root.join(".curator_backups/blob"), host);
        write(&root.join("models_dev_cache.json"), host);
        write(&root.join("real.json.bak.20260718"), host);
        write(&root.join("real.json~"), host);
        let mut elf = b"\x7fELF".to_vec();
        elf.extend_from_slice(host);
        write(&root.join("bin/tool"), &elf);
        let mut big = host.to_vec();
        big.resize(200, b' ');
        write(&root.join("big.json"), &big);
        write(&root.join("real.json"), host);
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("real.json"), root.join("link.json")).unwrap();

        let limits = Limits {
            max_file_bytes: 100,
            ..Limits::default()
        };
        let r = scan(std::slice::from_ref(&root), &needles(), &[], &limits);
        assert_eq!(
            labels(&r),
            vec![("real.json".to_string(), vec!["api.deepseek.com"])]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn source_checkouts_below_a_root_are_listed_not_entered() {
        let root = tmp();
        let host = b"https://api.deepseek.com";
        // A dotfiles repository at the root: read.
        write(&root.join(".git/HEAD"), b"ref: refs/heads/main");
        write(&root.join("package.json"), b"{}");
        write(&root.join("tool.json"), host);
        // An SDK checkout below it: skipped and named.
        write(&root.join("sdk/.git/HEAD"), b"ref: refs/heads/main");
        write(&root.join("sdk/pyproject.toml"), b"");
        write(&root.join("sdk/client.py"), host);
        // A git directory with no project manifest is configuration.
        write(&root.join("units/.git/HEAD"), b"ref: refs/heads/main");
        write(&root.join("units/x.service"), host);

        let r = scan(
            std::slice::from_ref(&root),
            &needles(),
            &[],
            &Limits::default(),
        );
        let mut got = labels(&r);
        got.sort();
        assert_eq!(
            got,
            vec![
                ("tool.json".to_string(), vec!["api.deepseek.com"]),
                ("x.service".to_string(), vec!["api.deepseek.com"]),
            ]
        );
        assert_eq!(r.checkouts, vec![root.join("sdk")]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn executables_are_recognised_by_magic_not_name() {
        assert!(is_executable_image(b"\x7fELF"));
        assert!(is_executable_image(&[0xcf, 0xfa, 0xed, 0xfe]));
        assert!(is_executable_image(&[0xca, 0xfe, 0xba, 0xbe]));
        assert!(!is_executable_image(b"#!/b"));
        assert!(!is_executable_image(b"{\n  "));
    }

    #[test]
    fn depth_limit_stops_the_walk() {
        let root = tmp();
        let host = b"https://api.deepseek.com";
        write(&root.join("a/b/c/deep.json"), host);
        write(&root.join("a/shallow.json"), host);
        let limits = Limits {
            max_depth: 1,
            ..Limits::default()
        };
        let r = scan(std::slice::from_ref(&root), &needles(), &[], &limits);
        assert_eq!(
            labels(&r),
            vec![("shallow.json".to_string(), vec!["api.deepseek.com"])]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn total_byte_limit_truncates_and_says_so() {
        let root = tmp();
        for i in 0..5 {
            write(&root.join(format!("{i}.json")), &[b'x'; 100]);
        }
        let limits = Limits {
            max_total_bytes: 250,
            ..Limits::default()
        };
        let r = scan(std::slice::from_ref(&root), &needles(), &[], &limits);
        assert!(r.truncated);
        assert_eq!(r.files, 2);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_roots_are_dropped_and_single_files_are_roots_too() {
        let root = tmp();
        write(
            &root.join(".zshenv"),
            b"export OPENAI_BASE_URL=https://api.deepseek.com/v1",
        );
        let r = scan(
            &[root.join("nope"), root.join(".zshenv")],
            &needles(),
            &[],
            &Limits::default(),
        );
        assert_eq!(r.roots, vec![root.join(".zshenv")]);
        assert_eq!(r.hits.len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn default_roots_follow_home_and_xdg() {
        let mut env = HashMap::new();
        assert!(default_roots(&env).is_empty());
        env.insert("HOME".to_string(), "/h".to_string());
        let roots = default_roots(&env);
        assert_eq!(roots[0], PathBuf::from("/h/.config"));
        assert!(roots.contains(&PathBuf::from("/h/.zshenv")));
        env.insert("XDG_CONFIG_HOME".to_string(), "/x".to_string());
        assert_eq!(default_roots(&env)[0], PathBuf::from("/x"));
    }
}
