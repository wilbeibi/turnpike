//! Local credential operations. Secrets enter through a terminal or pipe;
//! only file revisions and state leave through stdout or the status socket.
use crate::cli::{KeySetArgs, KeysCmd};
use crate::custody::{self, Custody, Status, MAX_FILE_BYTES};
use crate::providers::{Provider, PROVIDERS};
use anyhow::{bail, Context, Result};
use log::warn;
use rustix::fs::{flock, FlockOperation, Mode, OFlags};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, IsTerminal, Read, Write};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

fn private_dir(path: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    Ok(())
}

fn lock(path: &Path) -> Result<File> {
    let fd = rustix::fs::open(
        path,
        OFlags::CREATE | OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::RUSR | Mode::WUSR,
    )?;
    let f = File::from(fd);
    if !f.metadata()?.is_file() {
        bail!("lock must be a regular file");
    }
    flock(&f, FlockOperation::NonBlockingLockExclusive)
        .context("another key operation is running; retry when it finishes")?;
    Ok(f)
}

/// A connect-and-read protocol: no commands, request parsing, or secret values.
/// Its failure never stops the proxy from forwarding.
pub(crate) fn serve_status(custody: Arc<Custody>, path: &Path) -> Result<()> {
    private_dir(path.parent().context("status socket needs a directory")?)?;
    let guard = lock(&path.with_extension("sock.lock"))?;
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_socket() => fs::remove_file(path)?,
        Ok(_) => bail!("status socket path is occupied by a non-socket file"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    tokio::spawn(async move {
        let _guard = guard;
        loop {
            // An accept error (EMFILE, say) must not end status for the
            // daemon's lifetime; back off and keep serving.
            let mut stream = match listener.accept().await {
                Ok((stream, _)) => stream,
                Err(e) => {
                    warn!("turnpike: key status socket accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let bytes = serde_json::to_vec(&custody.status()).expect("status is serializable");
            let _ = tokio::time::timeout(Duration::from_secs(1), stream.write_all(&bytes)).await;
        }
    });
    Ok(())
}

async fn daemon_status(path: &Path) -> Option<Status> {
    tokio::time::timeout(Duration::from_millis(500), async {
        let stream = UnixStream::connect(path).await.ok()?;
        let mut bytes = Vec::new();
        stream.take(16 * 1024).read_to_end(&mut bytes).await.ok()?;
        serde_json::from_slice(&bytes).ok()
    })
    .await
    .ok()
    .flatten()
}

fn random_hex() -> Result<String> {
    let mut bytes = [0; 16];
    getrandom::getrandom(&mut bytes).map_err(|_| anyhow::anyhow!("random source unavailable"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

struct PendingFile(PathBuf);
impl Drop for PendingFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn disk(path: &Path) -> Result<Option<(String, String)>> {
    match fs::symlink_metadata(path) {
        Ok(_) => custody::read_file(path).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn save(path: &Path, text: &str, expected: Option<&str>) -> Result<String> {
    if text.len() as u64 > MAX_FILE_BYTES {
        bail!("candidate key file exceeds 64 KiB");
    }
    custody::parse(text)?;
    let parent = path.parent().context("key file needs a directory")?;
    let pending = PendingFile(parent.join(format!(".keys-{}.tmp", random_hex()?)));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&pending.0)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    let current = disk(path)?;
    if current.as_ref().map(|(_, rev)| rev.as_str()) != expected {
        bail!("keys.env changed; inspect key status and retry");
    }
    fs::rename(&pending.0, path)?;
    File::open(parent)?
        .sync_all()
        .context("key file saved but directory sync failed; inspect key status")?;
    // Rename can change ctime. Read metadata from the installed inode.
    Ok(custody::revision(&file.metadata()?))
}

fn assignment(line: &str) -> Option<(&str, &str)> {
    line.trim()
        .strip_prefix("export ")
        .unwrap_or(line.trim())
        .trim_start()
        .split_once('=')
}

/// Edit only this provider's slots; keep the shell-sourceable file and comments.
fn candidate(text: &str, provider: &Provider, new: Option<&str>) -> Result<Option<String>> {
    let map = custody::parse(text)?;
    let old = map.key(provider.name);
    let primary = text
        .lines()
        .filter_map(assignment)
        .find(|(name, _)| provider.key_envs.contains(name))
        .map(|(name, _)| name)
        .unwrap_or(provider.key_envs[0]);
    let previous = text
        .lines()
        .filter_map(assignment)
        .find_map(|(name, value)| {
            name.strip_suffix("_PREVIOUS")
                .filter(|name| provider.key_envs.contains(name))
                .and_then(|_| custody::unquote(value))
        });
    let key = match new {
        Some(key) => key,
        None => {
            previous.context("no previous key for this provider; supply a new key with set-key")?
        }
    };
    if old == Some(key) {
        return Ok(None);
    }
    let mut out = String::new();
    for line in text.lines() {
        let is_slot = assignment(line).is_some_and(|(name, _)| {
            provider
                .key_envs
                .contains(&name.strip_suffix("_PREVIOUS").unwrap_or(name))
        });
        if !is_slot {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push_str(&format!("{primary}={key}\n"));
    if let Some(old) = old {
        out.push_str(&format!("{primary}_PREVIOUS={old}\n"));
    }
    custody::parse(&out)?;
    Ok(Some(out))
}

struct EchoGuard(rustix::termios::Termios);
impl Drop for EchoGuard {
    fn drop(&mut self) {
        let _ = rustix::termios::tcsetattr(
            std::io::stdin(),
            rustix::termios::OptionalActions::Now,
            &self.0,
        );
    }
}

async fn input(from_stdin: bool) -> Result<String> {
    let stdin = std::io::stdin();
    let mut text = String::new();
    if from_stdin {
        if stdin.is_terminal() {
            bail!("omit --stdin for a hidden terminal prompt, or pipe the key");
        }
        stdin
            .lock()
            .take(MAX_FILE_BYTES + 1)
            .read_to_string(&mut text)?;
    } else {
        if !stdin.is_terminal() {
            bail!("pipe a key with --stdin, or run in a terminal");
        }
        // Ctrl-C's default action would exit past EchoGuard and leave the
        // terminal silent, so take SIGINT before echo goes off.
        let mut interrupt =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        let original = rustix::termios::tcgetattr(&stdin)?;
        let mut hidden = original.clone();
        hidden.local_modes.remove(rustix::termios::LocalModes::ECHO);
        rustix::termios::tcsetattr(&stdin, rustix::termios::OptionalActions::Now, &hidden)?;
        let _guard = EchoGuard(original);
        eprint!("New provider key: ");
        std::io::stderr().flush()?;
        let line = tokio::task::spawn_blocking(|| {
            let mut line = String::new();
            std::io::stdin()
                .lock()
                .take(MAX_FILE_BYTES + 1)
                .read_line(&mut line)
                .map(|_| line)
        });
        tokio::select! {
            line = line => text = line??,
            _ = interrupt.recv() => {
                eprintln!();
                bail!("cancelled; nothing was written");
            }
        }
        eprintln!();
    }
    let key = text.trim_end_matches(['\r', '\n']);
    // Admit only the literal value, not quoting, assignments, or multiple keys.
    if key.len() as u64 > MAX_FILE_BYTES
        || key.is_empty()
        || key
            .bytes()
            .any(|b| !b.is_ascii_alphanumeric() && !b"._+/=-".contains(&b))
        || key.starts_with(custody::SENTINEL_PREFIX)
    {
        bail!("input must be one provider key using [A-Za-z0-9._+/=-], without quotes");
    }
    Ok(key.to_string())
}

#[derive(serde::Serialize)]
struct Report {
    action: &'static str,
    provider: Option<String>,
    revision: Option<String>,
    changed: bool,
    activation: &'static str,
    providers: Vec<&'static str>,
    rollback_available: Vec<&'static str>,
    daemon: Option<Status>,
    file_error: Option<String>,
    upstream_verification: &'static str,
    old_key_revocation: &'static str,
}

async fn execute(cmd: KeysCmd, path: &Path, socket: &Path) -> Result<Report> {
    let (action, provider, from_stdin, expected, wait) = match cmd {
        KeysCmd::Status => ("status", None, false, None, 0),
        KeysCmd::Set(KeySetArgs {
            provider,
            stdin,
            expect_revision,
            wait,
        }) => ("set", Some(provider), stdin, expect_revision, wait),
        KeysCmd::Rollback {
            provider,
            expect_revision,
            wait,
        } => (
            "rollback",
            Some(provider),
            false,
            Some(expect_revision),
            wait,
        ),
    };
    let key = if action == "set" {
        Some(input(from_stdin).await?)
    } else {
        None
    };
    let _guard = if provider.is_some() {
        private_dir(path.parent().context("key file needs a directory")?)?;
        Some(lock(&path.with_extension("lock"))?)
    } else {
        None
    };
    let (existing, mut file_error) = match disk(path) {
        Ok(existing) => (existing, None),
        Err(e) if action == "status" => (None, Some(format!("{e:#}"))),
        Err(e) => return Err(e),
    };
    let mut revision = existing.as_ref().map(|(_, rev)| rev.clone());
    if expected
        .as_ref()
        .is_some_and(|e| Some(e) != revision.as_ref())
    {
        bail!("revision conflict; inspect key status before retrying");
    }
    let mut text = existing.map(|(text, _)| text).unwrap_or_default();
    let mut changed = false;
    if let Some(name) = &provider {
        let p = PROVIDERS
            .iter()
            .find(|p| p.name == name)
            .context("unknown provider")?;
        if revision.is_none() && action == "set" {
            text = format!("TURNPIKE_SENTINEL_DEFAULT=tp-{}\n", random_hex()?);
        }
        if let Some(next) = candidate(&text, p, key.as_deref())? {
            revision = Some(save(path, &next, revision.as_deref())?);
            text = next;
            changed = true;
        }
    }
    drop(_guard);
    let map = match custody::parse(&text) {
        Ok(map) => map,
        Err(e) if action == "status" => {
            file_error = Some(format!("{e:#}"));
            custody::KeyMap::default()
        }
        Err(e) => return Err(e),
    };
    if revision.is_none() && file_error.is_none() {
        file_error = Some("keys.env is missing".into());
    }
    let providers = PROVIDERS
        .iter()
        .filter(|p| map.key(p.name).is_some())
        .map(|p| p.name)
        .collect();
    let mut rollback_available: Vec<_> = text
        .lines()
        .filter_map(assignment)
        .filter_map(|(name, _)| {
            name.strip_suffix("_PREVIOUS")
                .and_then(custody::provider_for_var)
        })
        .collect();
    rollback_available.sort_unstable();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait);
    let mut daemon;
    loop {
        daemon = daemon_status(socket).await;
        if file_error.is_none()
            && revision.is_some()
            && daemon
                .as_ref()
                .is_some_and(|s| s.loaded_revision == revision)
            || tokio::time::Instant::now() >= deadline
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let activation = if file_error.is_none()
        && revision.is_some()
        && daemon
            .as_ref()
            .is_some_and(|s| s.loaded_revision == revision)
    {
        "loaded"
    } else {
        "unconfirmed"
    };
    Ok(Report {
        action,
        provider,
        revision,
        changed,
        activation,
        providers,
        rollback_available,
        daemon,
        file_error,
        upstream_verification: "not_checked",
        old_key_revocation: "not_checked",
    })
}

pub async fn run(cmd: KeysCmd, json: bool) -> i32 {
    match execute(cmd, &crate::paths::keys_env(), &crate::paths::keys_socket()).await {
        Ok(report) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&report).expect("report is serializable")
                );
            } else {
                println!(
                    "key {}: {}; revision {}; providers [{}]",
                    report.action,
                    report.activation,
                    report.revision.as_deref().unwrap_or("none"),
                    report.providers.join(", ")
                );
                if let Some(error) = &report.file_error {
                    eprintln!("Key file: {error}");
                }
                if let Some(error) = report
                    .daemon
                    .as_ref()
                    .and_then(|d| d.last_reload_error.as_ref())
                {
                    eprintln!("Daemon reload: {error}");
                }
                if report.activation != "loaded" {
                    eprintln!("Activation unconfirmed. Run `turnpike key status` after the daemon reloads.");
                }
                eprintln!("Upstream verification and old-key revocation have not been checked.");
            }
            if report.activation == "loaded" {
                0
            } else {
                3
            }
        }
        Err(e) => {
            if json {
                println!("{}", serde_json::json!({"error": format!("{e:#}")}));
            } else {
                eprintln!("turnpike key: {e:#}");
            }
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::custody::test_support::{write_keys, TempDir};

    #[tokio::test]
    async fn status_confirms_startup_edit_and_reports_failed_reload_without_losing_loaded_revision()
    {
        let dir = TempDir::new("status");
        let path = dir.0.join("keys.env");
        let socket = dir.0.join("keys.sock");
        write_keys(&path, "DEEPSEEK_API_KEY=sk-old\n", 0o600);
        let custody = Arc::new(Custody::open(path.clone()));
        // The reported startup race: edit after open, before watching.
        write_keys(&path, "DEEPSEEK_API_KEY=sk-new\n", 0o600);
        let expected = custody::read_file(&path).unwrap().1;
        custody.clone().spawn_watch();
        serve_status(custody.clone(), &socket).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let report = execute(KeysCmd::Status, &path, &socket).await.unwrap();
                if report.activation == "loaded" {
                    assert_eq!(report.revision.as_deref(), Some(expected.as_str()));
                    assert_eq!(report.providers, ["deepseek"]);
                    let output = serde_json::to_string(&report).unwrap();
                    assert!(!output.contains("sk-old") && !output.contains("sk-new"));
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("startup edit never became active");
        write_keys(&path, "DEEPSEEK_API_KEY=sk-secret\nBROKEN", 0o600);
        assert!(custody.reload().is_err());
        let report = execute(KeysCmd::Status, &path, &socket).await.unwrap();
        assert_eq!(report.activation, "unconfirmed");
        assert!(report.file_error.is_some());
        assert_eq!(
            report.daemon.as_ref().unwrap().loaded_revision.as_deref(),
            Some(expected.as_str())
        );
        assert!(report.daemon.as_ref().unwrap().last_reload_error.is_some());
        assert!(!serde_json::to_string(&report)
            .unwrap()
            .contains("sk-secret"));
    }

    #[test]
    fn atomic_save_rejects_a_changed_source_and_removes_its_temporary_file() {
        let dir = TempDir::new("conflict");
        let path = dir.0.join("keys.env");
        write_keys(&path, "DEEPSEEK_API_KEY=sk-old\n", 0o600);
        let old_revision = custody::read_file(&path).unwrap().1;
        write_keys(&path, "DEEPSEEK_API_KEY=sk-edited\n", 0o600);
        assert!(save(&path, "DEEPSEEK_API_KEY=sk-new\n", Some(&old_revision)).is_err());
        assert_eq!(
            custody::load(&path).unwrap().key("deepseek"),
            Some("sk-edited")
        );
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 1);
    }
}
