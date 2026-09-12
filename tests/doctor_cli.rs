//! `turnpike doctor` answers "what here spends money without going through
//! turnpike?", so it is exercised as a process against a planted home
//! directory. `env_clear` matters twice over: the developer's real keys would
//! show up as findings, and the developer's real `~/.config` would be walked.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A key value that must never reach stdout or stderr. It sits on the same
/// line as the vendor host in every planted file, exactly where a real key
/// would.
const PLANTED_KEY: &str = "sk-PLANTED-SECRET-0123456789";

struct Home {
    dir: PathBuf,
}

impl Home {
    fn empty() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "turnpike-doctor-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(dir.join(".config")).unwrap();
        Home { dir }
    }

    /// The machine the design doc describes: an agent config pointing at
    /// DeepSeek directly, a browser extension's LevelDB naming OpenRouter,
    /// plus the things the scan must ignore.
    fn leaky() -> Self {
        let h = Self::empty();
        h.write(
            ".config/opencode/opencode.json",
            format!(r#"{{"baseURL": "https://api.deepseek.com/v1", "apiKey": "{PLANTED_KEY}"}}"#)
                .as_bytes(),
        );
        let mut blob = vec![0u8; 2048];
        blob.extend_from_slice(b"\x00\x01https://openrouter.ai/api/v1\x00");
        blob.extend_from_slice(PLANTED_KEY.as_bytes());
        blob.extend(vec![0xffu8; 2048]);
        h.write(
            ".config/chromium/Default/Local Extension Settings/abc/000005.ldb",
            &blob,
        );
        h.write(
            ".config/fixed.toml",
            b"# was https://api.anthropic.com\nbase = \"http://127.0.0.1:4001\"\n",
        );
        // Must be skipped: an installed SDK, prose, a transcript, an oversized file.
        h.write(
            ".config/tool/node_modules/openai/index.js",
            b"const DEFAULT = 'https://api.openai.com/v1'",
        );
        h.write(".config/README.md", b"see https://api.openai.com");
        h.write(
            ".config/session.jsonl",
            b"{\"url\":\"https://api.groq.com\"}",
        );
        let mut big = b"https://api.x.ai".to_vec();
        big.resize((8 << 20) + 1, b' ');
        h.write(".config/big.json", &big);
        h
    }

    fn write(&self, rel: &str, bytes: &[u8]) {
        let path = self.dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::File::create(&path).unwrap().write_all(bytes).unwrap();
    }

    fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn doctor(home: &Home, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_turnpike"));
    cmd.arg("doctor").args(args).env_clear();
    cmd.env("HOME", home.path());
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("run turnpike doctor")
}

fn text(out: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).expect("valid JSON on stdout")
}

#[test]
fn a_clean_home_exits_zero_and_says_none() {
    let home = Home::empty();
    let out = doctor(&home, &[], &[]);
    let (stdout, _) = text(&out);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    assert!(stdout.contains("no unrouted keys"), "{stdout}");
    assert!(stdout.contains("none name a vendor host"), "{stdout}");
    assert!(stdout.contains("scanned"), "{stdout}");
}

#[test]
fn config_hits_are_findings_named_by_path_and_host_only() {
    let home = Home::leaky();
    let out = doctor(&home, &[], &[]);
    let (stdout, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(1), "{stdout}{stderr}");

    // The two real leaks, by path and host.
    assert!(stdout.contains("opencode.json"), "{stdout}");
    assert!(stdout.contains("api.deepseek.com"), "{stdout}");
    assert!(stdout.contains("000005.ldb"), "{stdout}");
    assert!(stdout.contains("openrouter.ai"), "{stdout}");
    // The file that names both is marked, not hidden.
    assert!(stdout.contains("fixed.toml"), "{stdout}");
    assert!(stdout.contains("also names turnpike"), "{stdout}");
    // Paths are shown relative to home.
    assert!(
        stdout.contains("~/.config/opencode/opencode.json"),
        "{stdout}"
    );

    // Skipped on purpose.
    for absent in ["node_modules", "README.md", "session.jsonl", "big.json"] {
        assert!(
            !stdout.contains(absent),
            "{absent} should be skipped: {stdout}"
        );
    }

    // Never the line, so never the key.
    assert!(!stdout.contains(PLANTED_KEY), "key leaked to stdout");
    assert!(!stderr.contains(PLANTED_KEY), "key leaked to stderr");
}

#[test]
fn an_unrouted_key_in_this_shell_is_a_finding_with_the_fix() {
    let home = Home::empty();
    let out = doctor(&home, &[], &[("DEEPSEEK_API_KEY", PLANTED_KEY)]);
    let (stdout, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(1), "{stdout}{stderr}");
    assert!(stdout.contains("1 key not routed"), "{stdout}");
    assert!(stdout.contains("DEEPSEEK_API_KEY set"), "{stdout}");
    assert!(stdout.contains("http://127.0.0.1:4003/v1"), "{stdout}");
    assert!(!stdout.contains(PLANTED_KEY));
}

#[test]
fn a_routed_key_is_not_a_finding() {
    let home = Home::empty();
    let out = doctor(
        &home,
        &["--json"],
        &[
            ("DEEPSEEK_API_KEY", "k"),
            ("OPENAI_BASE_URL", "http://127.0.0.1:4003/v1"),
        ],
    );
    assert_eq!(out.status.code(), Some(0));
    let v = json(&out);
    assert_eq!(v["status"], "clean");
    assert_eq!(v["env"].as_array().unwrap().len(), 0);
}

#[test]
fn gemini_is_reported_as_unknown_not_flagged() {
    // No base-URL variable exists for it, so a key alone proves nothing.
    let home = Home::empty();
    let out = doctor(&home, &["--json"], &[("GEMINI_API_KEY", "k")]);
    assert_eq!(out.status.code(), Some(0));
    let v = json(&out);
    assert_eq!(v["status"], "clean");
    assert_eq!(v["env"][0]["provider"], "gemini");
    assert_eq!(v["env"][0]["status"], "in code");
    assert!(v["env"][0]["base_url_var"].is_null());
}

#[test]
fn json_carries_what_an_agent_needs() {
    let home = Home::leaky();
    let out = doctor(&home, &["--json"], &[("OPENROUTER_API_KEY", PLANTED_KEY)]);
    assert_eq!(out.status.code(), Some(1));
    let v = json(&out);
    assert_eq!(v["status"], "findings");

    let env = v["env"].as_array().unwrap();
    assert_eq!(env.len(), 1);
    assert_eq!(env[0]["provider"], "openrouter");
    assert_eq!(env[0]["status"], "direct");
    assert_eq!(env[0]["key_var"], "OPENROUTER_API_KEY");
    assert_eq!(env[0]["base_url_var"], "OPENAI_BASE_URL");
    assert_eq!(env[0]["fix"], "http://127.0.0.1:4004/api/v1");

    let config = v["config"].as_array().unwrap();
    assert_eq!(config.len(), 3, "{config:?}");
    let by_name = |suffix: &str| {
        config
            .iter()
            .find(|c| c["path"].as_str().unwrap().ends_with(suffix))
            .unwrap_or_else(|| panic!("no hit for {suffix}: {config:?}"))
    };
    assert_eq!(by_name("opencode.json")["hosts"][0], "api.deepseek.com");
    assert_eq!(by_name("opencode.json")["names_turnpike"], false);
    assert_eq!(by_name("000005.ldb")["hosts"][0], "openrouter.ai");
    assert_eq!(by_name("fixed.toml")["names_turnpike"], true);
    assert!(by_name("fixed.toml")["modified"].is_string());

    let scan = &v["scan"];
    assert_eq!(scan["truncated"], false);
    assert!(scan["files"].as_u64().unwrap() >= 3);
    assert!(scan["roots"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r.as_str().unwrap().ends_with(".config")));
    assert!(scan["skip_dirs"].as_array().unwrap().len() > 5);

    let raw = String::from_utf8_lossy(&out.stdout);
    assert!(!raw.contains(PLANTED_KEY), "key leaked into JSON");
}

#[test]
fn xdg_config_home_replaces_dot_config() {
    let home = Home::empty();
    let xdg = home.path().join("xdg");
    fs::create_dir_all(&xdg).unwrap();
    fs::write(xdg.join("tool.json"), b"https://api.groq.com").unwrap();
    // ~/.config also has a hit, which must NOT be found when XDG is set.
    home.write(".config/other.json", b"https://api.x.ai");

    let out = doctor(
        &home,
        &["--json"],
        &[("XDG_CONFIG_HOME", xdg.to_str().unwrap())],
    );
    let v = json(&out);
    let hosts: Vec<&str> = v["config"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["hosts"][0].as_str().unwrap())
        .collect();
    assert_eq!(hosts, vec!["api.groq.com"]);
}

#[test]
fn no_home_at_all_still_answers() {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_turnpike"));
    cmd.arg("doctor").env_clear();
    let out = cmd.output().unwrap();
    let (stdout, _) = text(&out);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
}
