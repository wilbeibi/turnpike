//! Credential custody: tools hold a `tp-` sentinel, turnpike holds the keys.
//!
//! `keys.env` is two independent halves — a set of sentinels, and one key
//! per provider under the vendor's own variable name. A request carrying a
//! sentinel gets the key of the provider it is already routed to, so a key
//! can never be sent to another vendor. Anything that is not a sentinel is
//! forwarded untouched (invariant 1b). The file is re-read whenever it
//! changes; a file that fails to load never replaces a good map. Nothing in
//! here may print or log a key or a sentinel (invariant 9).

use crate::providers::{Provider, PROVIDERS};
use anyhow::{bail, Context, Result};
use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Response, StatusCode};
use log::{error, info, warn};
use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

pub const SENTINEL_PREFIX: &str = "tp-";
/// Random part of a sentinel. Browsers can reach loopback, so a guessable
/// sentinel would let any web page spend the key.
const MIN_SENTINEL_RANDOM: usize = 16;
const SENTINEL_VAR_PREFIX: &str = "TURNPIKE_SENTINEL_";
const PREVIOUS_SUFFIX: &str = "_PREVIOUS";
const MAX_FILE_BYTES: u64 = 64 * 1024;
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Headers a sentinel is looked for (and substituted) in. `authorization`
/// keeps its scheme; the other two carry the bare credential.
const CREDENTIAL_HEADERS: &[&str] = &["authorization", "x-api-key", "x-goog-api-key"];

#[derive(Debug, Default)]
pub struct KeyMap {
    /// Provider name → the key that is sent. A `_PREVIOUS` key is validated
    /// and then dropped: it exists for the user to roll back to, and is
    /// never sent, so it is never held.
    keys: HashMap<&'static str, String>,
    sentinels: HashSet<String>,
}

impl KeyMap {
    pub fn key(&self, provider: &str) -> Option<&str> {
        self.keys.get(provider).map(String::as_str)
    }

    pub fn is_sentinel(&self, token: &str) -> bool {
        self.sentinels.contains(token)
    }
}

/// Parse `keys.env`: a strict subset of POSIX shell, so the user can source
/// the same file by hand when turnpike is down. Errors name a line and a
/// variable, never a value.
pub fn parse(text: &str) -> Result<KeyMap> {
    let mut map = KeyMap::default();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut previous: Vec<(&'static str, usize)> = Vec::new();

    for (i, raw) in text.lines().enumerate() {
        let n = i + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some((name, value)) = line.split_once('=') else {
            bail!("keys.env line {n}: not NAME=value");
        };
        if !is_var_name(name) {
            bail!("keys.env line {n}: not a variable name");
        }
        if !seen.insert(name) {
            bail!("keys.env line {n} ({name}): set twice");
        }
        let Some(value) = unquote(value) else {
            bail!("keys.env line {n} ({name}): value must be bare or quoted [A-Za-z0-9._+/=-]");
        };

        if let Some(label) = name.strip_prefix(SENTINEL_VAR_PREFIX) {
            if label.is_empty() {
                bail!("keys.env line {n} ({name}): sentinel needs a label");
            }
            let random = value.strip_prefix(SENTINEL_PREFIX).unwrap_or("");
            if random.len() < MIN_SENTINEL_RANDOM {
                bail!(
                    "keys.env line {n} ({name}): a sentinel is {SENTINEL_PREFIX} plus at least \
                     {MIN_SENTINEL_RANDOM} random characters — print tp-$(openssl rand -hex 16)"
                );
            }
            if !map.sentinels.insert(value.to_string()) {
                bail!("keys.env line {n} ({name}): same sentinel as another line");
            }
        } else if let Some(provider) = name
            .strip_suffix(PREVIOUS_SUFFIX)
            .and_then(provider_for_var)
        {
            previous.push((provider, n));
        } else if let Some(provider) = provider_for_var(name) {
            if value.starts_with(SENTINEL_PREFIX) {
                bail!("keys.env line {n} ({name}): a key cannot be a sentinel");
            }
            if map.keys.insert(provider, value.to_string()).is_some() {
                bail!("keys.env line {n} ({name}): a second key for {provider}; keep one");
            }
        } else {
            bail!("keys.env line {n} ({name}): unknown variable");
        }
    }

    for (provider, n) in previous {
        if !map.keys.contains_key(provider) {
            bail!("keys.env line {n}: {provider} has a _PREVIOUS key but no current one");
        }
    }
    Ok(map)
}

/// Read, check, and parse the file. Group/other access is refused the way
/// ssh refuses a readable private key.
pub fn load(path: &Path) -> Result<KeyMap> {
    let meta = std::fs::metadata(path).with_context(|| format!("{}", path.display()))?;
    let mode = meta.mode() & 0o777;
    if mode & 0o077 != 0 {
        bail!(
            "{} is mode {mode:03o}; it holds keys — chmod 600 {}",
            path.display(),
            path.display()
        );
    }
    if meta.len() > MAX_FILE_BYTES {
        bail!("{} is over {MAX_FILE_BYTES} bytes", path.display());
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("{}", path.display()))?;
    parse(&text)
}

fn is_var_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_uppercase() || c == '_')
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// A bare, `'single'`, or `"double"` quoted value from a charset the shell
/// reads literally — no expansion, no escapes — so turnpike and `.` agree.
fn unquote(v: &str) -> Option<&str> {
    let inner = match v.as_bytes() {
        [b'\'', .., b'\''] | [b'"', .., b'"'] if v.len() >= 2 => &v[1..v.len() - 1],
        _ => v,
    };
    let ok = !inner.is_empty()
        && inner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+/=-".contains(&b));
    ok.then_some(inner)
}

fn provider_for_var(name: &str) -> Option<&'static str> {
    PROVIDERS
        .iter()
        .find(|p| p.key_envs.contains(&name))
        .map(|p| p.name)
}

/// Why a sentinel-bearing request was not forwarded. Messages name the
/// cause, never the credential.
#[derive(Debug, PartialEq)]
pub enum Refusal {
    NoMap,
    UnknownSentinel,
    NoKey(&'static str),
}

impl Refusal {
    pub fn message(&self) -> String {
        match self {
            Refusal::NoMap => {
                "turnpike: this request carries a turnpike sentinel, but no keys.env \
                               is loaded (missing or invalid — see turnpike's log)"
                    .to_string()
            }
            Refusal::UnknownSentinel => {
                "turnpike: unknown sentinel — it is not in keys.env".to_string()
            }
            Refusal::NoKey(p) => format!("turnpike: keys.env has no {p} key"),
        }
    }

    /// 401 in the `{"error": {...}}` shape OpenAI and Anthropic SDKs surface.
    pub fn response(&self) -> Response<Body> {
        let body = serde_json::json!({
            "error": {"type": "turnpike", "message": self.message()}
        });
        Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .expect("static response parts")
    }
}

/// Replace every sentinel in the credential headers with `provider`'s key.
/// A request with no sentinel is left exactly as it arrived.
pub fn apply(
    headers: &mut HeaderMap,
    provider: &Provider,
    map: Option<&KeyMap>,
) -> Result<(), Refusal> {
    for name in CREDENTIAL_HEADERS {
        let name = HeaderName::from_static(name);
        let Some((scheme, token)) = headers.get(&name).and_then(|v| split_credential(&name, v))
        else {
            continue;
        };
        if !token.starts_with(SENTINEL_PREFIX) {
            continue;
        }
        let map = map.ok_or(Refusal::NoMap)?;
        if !map.is_sentinel(token) {
            return Err(Refusal::UnknownSentinel);
        }
        let key = map
            .key(provider.name)
            .ok_or(Refusal::NoKey(provider.name))?;
        let value = match scheme {
            Some(scheme) => format!("{scheme} {key}"),
            None => key.to_string(),
        };
        // parse() admits only header-safe bytes, so this cannot fail on a
        // loaded key; refuse rather than forward the sentinel if it ever does.
        let mut value = HeaderValue::from_str(&value).map_err(|_| Refusal::NoKey(provider.name))?;
        value.set_sensitive(true);
        headers.insert(name, value);
    }
    Ok(())
}

/// `(Some(scheme), token)` for `authorization`, `(None, token)` otherwise.
fn split_credential<'a>(
    name: &HeaderName,
    v: &'a HeaderValue,
) -> Option<(Option<&'a str>, &'a str)> {
    let s = v.to_str().ok()?.trim();
    if *name == header::AUTHORIZATION {
        let (scheme, token) = s.split_once(' ')?;
        scheme
            .eq_ignore_ascii_case("bearer")
            .then(|| (Some(scheme), token.trim()))
    } else {
        Some((None, s))
    }
}

/// The live map, swapped whole on reload. A request clones the `Arc` once,
/// so a reload never changes the key of a request already on its way.
pub struct Custody {
    path: PathBuf,
    map: RwLock<Option<Arc<KeyMap>>>,
}

/// What changes when the file is edited, replaced, or chmod-ed.
type Fingerprint = (u64, u64, i64, i64, i64, i64);

fn fingerprint(path: &Path) -> Option<Fingerprint> {
    let m = std::fs::metadata(path).ok()?;
    Some((
        m.ino(),
        m.len(),
        m.mtime(),
        m.mtime_nsec(),
        m.ctime(),
        m.ctime_nsec(),
    ))
}

impl Custody {
    /// Startup load. A missing file means custody is off; a broken one is
    /// logged and also leaves it off, so tools still holding real keys keep
    /// working and the next good save turns it on.
    pub fn open(path: PathBuf) -> Self {
        let map = if path.exists() {
            match load(&path) {
                Ok(m) => {
                    info!("turnpike: {}", summary(&m));
                    Some(Arc::new(m))
                }
                Err(e) => {
                    error!("turnpike: keys.env not loaded; sentinels will be refused: {e:#}");
                    None
                }
            }
        } else {
            None
        };
        Self {
            path,
            map: RwLock::new(map),
        }
    }

    #[cfg(test)]
    pub fn with_map(map: Option<KeyMap>) -> Self {
        Self {
            path: PathBuf::new(),
            map: RwLock::new(map.map(Arc::new)),
        }
    }

    pub fn snapshot(&self) -> Option<Arc<KeyMap>> {
        self.map.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Re-read the file. On any failure the previous map stays in force.
    pub fn reload(&self) -> Result<()> {
        let m = load(&self.path)?;
        info!("turnpike: reloaded {}", summary(&m));
        *self.map.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(m));
        Ok(())
    }

    /// One watch tick: reload if the file changed since `last`.
    fn poll(&self, last: &mut Option<Fingerprint>) {
        let now = fingerprint(&self.path);
        if now == *last {
            return;
        }
        *last = now;
        if now.is_none() {
            warn!("turnpike: keys.env is gone; keeping the keys already loaded");
            return;
        }
        if let Err(e) = self.reload() {
            error!("turnpike: keys.env not reloaded; keeping the previous keys: {e:#}");
        }
    }

    /// Poll the file's metadata every couple of seconds. Saving the file is
    /// the whole reload procedure — no signal, no unit change, no restart.
    pub fn spawn_watch(self: Arc<Self>) {
        let mut last = fingerprint(&self.path);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(POLL_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                self.poll(&mut last);
            }
        });
    }
}

/// Counts and provider names only.
fn summary(m: &KeyMap) -> String {
    let mut providers: Vec<&str> = m.keys.keys().copied().collect();
    providers.sort_unstable();
    format!(
        "keys.env: keys for [{}], {} sentinel(s)",
        providers.join(", "),
        m.sentinels.len()
    )
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    /// A private scratch dir under the system temp dir, removed on drop.
    pub struct TempDir(pub PathBuf);
    impl TempDir {
        pub fn new(tag: &str) -> Self {
            let mut buf = [0u8; 8];
            getrandom::getrandom(&mut buf).unwrap();
            let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
            let p = std::env::temp_dir().join(format!("turnpike-custody-{tag}-{hex}"));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub fn write_keys(path: &Path, text: &str, mode: u32) {
        std::fs::write(path, text).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{write_keys, TempDir};
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const S1: &str = "tp-0123456789abcdef0123";
    const S2: &str = "tp-fedcba9876543210fedc";

    fn provider(name: &str) -> &'static Provider {
        PROVIDERS.iter().find(|p| p.name == name).unwrap()
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().unwrap());
        }
        h
    }

    fn map(text: &str) -> KeyMap {
        parse(text).unwrap()
    }

    fn base() -> String {
        format!(
            "# comment\n\nDEEPSEEK_API_KEY=sk-fake-deepseek\nexport OPENROUTER_API_KEY='sk-or-fake'\n\
             ANTHROPIC_API_KEY=\"sk-ant-fake\"\nTURNPIKE_SENTINEL_OPENCODE={S1}\n"
        )
    }

    #[test]
    fn sentinel_is_swapped_for_the_destination_providers_key() {
        let m = map(&base());
        for (prov, key) in [
            ("deepseek", "sk-fake-deepseek"),
            ("openrouter", "sk-or-fake"),
        ] {
            let mut h = headers(&[("authorization", &format!("Bearer {S1}"))]);
            apply(&mut h, provider(prov), Some(&m)).unwrap();
            let v = &h["authorization"];
            assert_eq!(v.to_str().unwrap(), format!("Bearer {key}"));
            assert!(v.is_sensitive());
        }
        // Bare-credential headers, and the client's scheme spelling is kept.
        let mut h = headers(&[("x-api-key", S1), ("x-goog-api-key", "AIza-real")]);
        apply(&mut h, provider("anthropic"), Some(&m)).unwrap();
        assert_eq!(h["x-api-key"], "sk-ant-fake");
        assert_eq!(h["x-goog-api-key"], "AIza-real");
        let mut h = headers(&[("authorization", &format!("bearer {S1}"))]);
        apply(&mut h, provider("deepseek"), Some(&m)).unwrap();
        assert_eq!(h["authorization"], "bearer sk-fake-deepseek");
    }

    #[test]
    fn real_key_passes_through_untouched() {
        let m = map(&base());
        let original = headers(&[
            ("authorization", "Bearer sk-real-client-key"),
            ("x-api-key", "sk-ant-real"),
            ("x-other", "tp-not-a-credential-header-0000"),
        ]);
        for state in [Some(&m), None] {
            let mut h = original.clone();
            apply(&mut h, provider("deepseek"), state).unwrap();
            assert_eq!(h, original);
            assert!(!h["authorization"].is_sensitive());
        }
    }

    #[test]
    fn unmapped_sentinel_is_refused() {
        let m = map(&base());
        let mut h = headers(&[("authorization", &format!("Bearer {S2}"))]);
        assert_eq!(
            apply(&mut h, provider("deepseek"), Some(&m)),
            Err(Refusal::UnknownSentinel)
        );
        let mut h = headers(&[("authorization", &format!("Bearer {S1}"))]);
        assert_eq!(
            apply(&mut h, provider("kimi"), Some(&m)),
            Err(Refusal::NoKey("kimi"))
        );
        let mut h = headers(&[("x-api-key", S1)]);
        assert_eq!(
            apply(&mut h, provider("anthropic"), None),
            Err(Refusal::NoMap)
        );
        for r in [
            Refusal::NoMap,
            Refusal::UnknownSentinel,
            Refusal::NoKey("x"),
        ] {
            assert!(!r.message().contains(S1) && !r.message().contains(S2));
        }
    }

    #[test]
    fn rotation_sends_the_newest_key_and_never_the_previous() {
        let m = map(&format!(
            "DEEPSEEK_API_KEY=sk-new\nDEEPSEEK_API_KEY_PREVIOUS=sk-old\nTURNPIKE_SENTINEL_A={S1}\n"
        ));
        let mut h = headers(&[("authorization", &format!("Bearer {S1}"))]);
        apply(&mut h, provider("deepseek"), Some(&m)).unwrap();
        assert_eq!(h["authorization"], "Bearer sk-new");
        assert!(!format!("{m:?}").contains("sk-old"));
    }

    #[test]
    fn malformed_files_are_rejected() {
        let s = S1;
        for bad in [
            "DEEPSEK_API_KEY=sk-x".to_string(),
            "DEEPSEEK_API_KEY=sk-a\nDEEPSEEK_API_KEY=sk-b".into(),
            "ANTHROPIC_API_KEY=sk-a\nANTHROPIC_AUTH_TOKEN=sk-b".into(),
            "DEEPSEEK_API_KEY_PREVIOUS=sk-old".into(),
            "DEEPSEEK_API_KEY=$(cat /etc/passwd)".into(),
            "DEEPSEEK_API_KEY=\"sk-a b\"".into(),
            "DEEPSEEK_API_KEY=".into(),
            format!("DEEPSEEK_API_KEY={s}"),
            "TURNPIKE_SENTINEL_A=tp-short".into(),
            format!("TURNPIKE_SENTINEL_A={s}\nTURNPIKE_SENTINEL_B={s}"),
            format!("TURNPIKE_SENTINEL_={s}"),
            "sk-pasted-key-on-its-own-line".into(),
            "DEEPSEEK_API_KEY=sk-a # trailing comment".into(),
        ] {
            assert!(parse(&bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn parse_errors_never_contain_secret_values() {
        let secret = "sk-SECRETVALUE123";
        for bad in [
            format!("DEEPSEK_API_KEY={secret}"),
            format!("DEEPSEEK_API_KEY={secret}\nDEEPSEEK_API_KEY={secret}"),
            format!("DEEPSEEK_API_KEY=\"{secret} x\""),
            secret.to_string(),
            format!("{secret}=1"),
            format!("TURNPIKE_SENTINEL_A={secret}"),
        ] {
            let e = format!("{:#}", parse(&bad).unwrap_err());
            assert!(!e.contains("SECRETVALUE"), "{e}");
        }
    }

    #[test]
    fn group_readable_keys_file_is_refused() {
        let dir = TempDir::new("mode");
        let path = dir.0.join("keys.env");
        write_keys(&path, &base(), 0o640);
        let e = format!("{:#}", load(&path).unwrap_err());
        assert!(e.contains("chmod 600"), "{e}");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(load(&path).is_ok());
    }

    #[test]
    fn malformed_reload_keeps_the_previous_map() {
        let dir = TempDir::new("reload");
        let path = dir.0.join("keys.env");
        write_keys(&path, &base(), 0o600);
        let c = Custody::open(path.clone());
        let before = c.snapshot().unwrap();

        write_keys(&path, "DEEPSEEK_API_KEY=sk-a\nGARBAGE", 0o600);
        assert!(c.reload().is_err());
        assert!(Arc::ptr_eq(&c.snapshot().unwrap(), &before));

        // Deleting the file keeps the loaded map too.
        let mut last = fingerprint(&path);
        std::fs::remove_file(&path).unwrap();
        c.poll(&mut last);
        assert!(Arc::ptr_eq(&c.snapshot().unwrap(), &before));
    }

    #[test]
    fn a_saved_edit_is_picked_up_by_the_watch_tick() {
        let dir = TempDir::new("watch");
        let path = dir.0.join("keys.env");
        write_keys(&path, &base(), 0o600);
        let c = Custody::open(path.clone());
        let mut last = fingerprint(&path);

        c.poll(&mut last); // unchanged: no reload
        write_keys(
            &path,
            &base().replace("sk-fake-deepseek", "sk-rotated"),
            0o600,
        );
        c.poll(&mut last);
        assert_eq!(c.snapshot().unwrap().key("deepseek"), Some("sk-rotated"));
    }

    #[test]
    fn broken_or_missing_file_at_startup_leaves_custody_off() {
        let dir = TempDir::new("startup");
        let path = dir.0.join("keys.env");
        assert!(Custody::open(path.clone()).snapshot().is_none());
        write_keys(&path, "NOPE=1", 0o600);
        let c = Custody::open(path.clone());
        assert!(c.snapshot().is_none());
        // The next good save turns it on without a restart.
        let mut last = fingerprint(&path);
        write_keys(&path, &base(), 0o600);
        c.poll(&mut last);
        assert!(c.snapshot().is_some());
    }
}
