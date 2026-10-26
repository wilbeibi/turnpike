//! Fake credentials only. Commands run with isolated config and no live daemon.
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::PathBuf;
use std::process::{Command, Stdio};

struct Home(PathBuf);
impl Home {
    fn new() -> Self {
        let mut id = [0; 8];
        getrandom::getrandom(&mut id).unwrap();
        let h = Self(std::env::temp_dir().join(format!("tp-keys-{:x}", u64::from_ne_bytes(id))));
        fs::create_dir_all(h.0.join("turnpike")).unwrap();
        h
    }
    fn file(&self) -> PathBuf {
        self.0.join("turnpike/keys.env")
    }
    fn write(&self, text: &str) {
        fs::write(self.file(), text).unwrap();
        fs::set_permissions(self.file(), fs::Permissions::from_mode(0o600)).unwrap();
    }
    fn run(&self, args: &[&str], input: &str) -> (i32, Value) {
        let mut command = Command::new(env!("CARGO_BIN_EXE_turnpike"));
        if args[0] == "set" {
            command.args(["set-key", "--json"]).args(&args[1..]);
        } else {
            command.args(["key", "--json"]).args(args);
        }
        let mut child = command
            .env_clear()
            .env("XDG_CONFIG_HOME", &self.0)
            .env("HOME", &self.0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        for secret in [
            "sk-OLD-SECRET",
            "sk-NEW-SECRET",
            "tp-0123456789abcdef",
            "sk-UNRELATED-SECRET",
        ] {
            assert!(
                !stdout.contains(secret) && !stderr.contains(secret),
                "credential leaked"
            );
        }
        assert!(
            stderr.is_empty(),
            "JSON commands should not require stderr parsing: {stderr}"
        );
        (
            output.status.code().unwrap(),
            serde_json::from_str(&stdout).expect(&stdout),
        )
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn rotation_is_private_repeatable_and_rollback_requires_its_revision() {
    let h = Home::new();
    h.write("# preserve me\nexport GOOGLE_API_KEY='sk-OLD-SECRET'\nDEEPSEEK_API_KEY=sk-UNRELATED-SECRET\nTURNPIKE_SENTINEL_DEFAULT=tp-0123456789abcdef\n");
    let (code, set) = h.run(
        &["set", "gemini", "--stdin", "--wait", "0"],
        "sk-NEW-SECRET\n",
    );
    assert_eq!(code, 3); // saved, but no daemon has acknowledged it
    assert_eq!(set["changed"], true);
    assert_eq!(set["activation"], "unconfirmed");
    assert_eq!(set["upstream_verification"], "not_checked");
    assert_eq!(set["rollback_available"], serde_json::json!(["gemini"]));
    let rev = set["revision"].as_str().unwrap();
    let after = fs::read_to_string(h.file()).unwrap();
    assert!(after.contains("GOOGLE_API_KEY=sk-NEW-SECRET\nGOOGLE_API_KEY_PREVIOUS=sk-OLD-SECRET\n"));
    assert!(after.contains("# preserve me\n"));
    assert!(after.contains("DEEPSEEK_API_KEY=sk-UNRELATED-SECRET\n"));
    assert!(after.contains("TURNPIKE_SENTINEL_DEFAULT=tp-0123456789abcdef\n"));
    assert_eq!(
        fs::metadata(h.file()).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let (_, repeat) = h.run(
        &["set", "gemini", "--stdin", "--wait", "0"],
        "sk-NEW-SECRET",
    );
    assert_eq!(repeat["changed"], false);
    assert_eq!(repeat["revision"], set["revision"]);
    assert_eq!(fs::read_to_string(h.file()).unwrap(), after);

    let (code, _) = h.run(
        &[
            "rollback",
            "gemini",
            "--expect-revision",
            "stale",
            "--wait",
            "0",
        ],
        "",
    );
    assert_eq!(code, 2);
    assert_eq!(fs::read_to_string(h.file()).unwrap(), after);
    let (code, rolled) = h.run(
        &[
            "rollback",
            "gemini",
            "--expect-revision",
            rev,
            "--wait",
            "0",
        ],
        "",
    );
    assert_eq!(code, 3);
    assert_ne!(rolled["revision"], set["revision"]);
    let after_rollback = fs::read_to_string(h.file()).unwrap();
    assert!(after_rollback
        .contains("GOOGLE_API_KEY=sk-OLD-SECRET\nGOOGLE_API_KEY_PREVIOUS=sk-NEW-SECRET\n"));
    let (code, _) = h.run(
        &[
            "rollback",
            "gemini",
            "--expect-revision",
            rev,
            "--wait",
            "0",
        ],
        "",
    );
    assert_eq!(code, 2);
    assert_eq!(fs::read_to_string(h.file()).unwrap(), after_rollback);
}

#[test]
fn first_install_creates_one_stable_sentinel_and_rejects_bad_input_without_writing() {
    let h = Home::new();
    let (_, first) = h.run(
        &["set", "deepseek", "--stdin", "--wait", "0"],
        "sk-OLD-SECRET",
    );
    assert_eq!(first["changed"], true);
    let original = fs::read_to_string(h.file()).unwrap();
    let sentinel = original
        .lines()
        .find(|s| s.starts_with("TURNPIKE_SENTINEL_DEFAULT=tp-"))
        .unwrap();
    assert_eq!(sentinel.split_once("=tp-").unwrap().1.len(), 32);
    assert!(!first.to_string().contains(sentinel));
    for input in [
        "",
        "tp-0123456789abcdef",
        "sk-NEW-SECRET\nOTHER=sk-OLD-SECRET",
        "'sk-NEW-SECRET'",
    ] {
        let (code, error) = h.run(&["set", "deepseek", "--stdin", "--wait", "0"], input);
        assert_eq!(code, 2);
        assert!(error["error"].is_string());
        assert_eq!(fs::read_to_string(h.file()).unwrap(), original);
    }
    let (_, second) = h.run(&["set", "groq", "--stdin", "--wait", "0"], "sk-NEW-SECRET");
    assert_eq!(second["changed"], true);
    assert!(fs::read_to_string(h.file()).unwrap().contains(sentinel));
    assert_eq!(fs::read_dir(h.file().parent().unwrap()).unwrap().count(), 2); // keys.env and lock; no secret temp files
}

#[test]
fn bad_files_and_concurrent_writers_cannot_replace_working_credentials() {
    let h = Home::new();
    let text = "DEEPSEEK_API_KEY=sk-OLD-SECRET\n";
    h.write(text);
    let lock = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(h.0.join("turnpike/keys.lock"))
        .unwrap();
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive).unwrap();
    let (code, _) = h.run(
        &["set", "deepseek", "--stdin", "--wait", "0"],
        "sk-NEW-SECRET",
    );
    assert_eq!(code, 2);
    assert_eq!(fs::read_to_string(h.file()).unwrap(), text);
    drop(lock);

    fs::set_permissions(h.file(), fs::Permissions::from_mode(0o644)).unwrap();
    let (code, status) = h.run(&["status"], "");
    assert_eq!(code, 3);
    assert!(status["file_error"].as_str().unwrap().contains("chmod 600"));
    let (code, _) = h.run(
        &["set", "deepseek", "--stdin", "--wait", "0"],
        "sk-NEW-SECRET",
    );
    assert_eq!(code, 2);
    assert_eq!(fs::read_to_string(h.file()).unwrap(), text);

    let ambiguous = "GEMINI_API_KEY=sk-OLD-SECRET\nGEMINI_API_KEY_PREVIOUS=sk-NEW-SECRET\nGOOGLE_API_KEY_PREVIOUS=sk-UNRELATED-SECRET\n";
    h.write(ambiguous);
    let (code, _) = h.run(
        &["set", "gemini", "--stdin", "--wait", "0"],
        "sk-NEW-SECRET",
    );
    assert_eq!(code, 2);
    assert_eq!(fs::read_to_string(h.file()).unwrap(), ambiguous);

    fs::remove_file(h.file()).unwrap();
    let target = h.0.join("actual");
    fs::write(&target, text).unwrap();
    symlink(&target, h.file()).unwrap();
    let (code, _) = h.run(
        &["set", "deepseek", "--stdin", "--wait", "0"],
        "sk-NEW-SECRET",
    );
    assert_eq!(code, 2);
    assert_eq!(fs::read_to_string(target).unwrap(), text);
}
