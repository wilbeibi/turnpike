//! `turnpike config` is the answer to "which endpoint do I use for provider X?",
//! so these tests exercise it the way a person or an agent would: as a process,
//! with a clean environment. `env_clear` matters — the developer running the
//! suite has real API keys set, and the status column reads them.

use std::process::{Command, Output};

/// Every provider name, and the port each one must answer with. Restated by
/// hand: a test that imported PROVIDERS would agree with the binary even if
/// both were wrong.
const PROVIDERS: &[(&str, u16)] = &[
    ("openai", 4000),
    ("anthropic", 4001),
    ("gemini", 4002),
    ("deepseek", 4003),
    ("openrouter", 4004),
    ("kimi", 4005),
    ("minimax", 4006),
    ("glm", 4007),
    ("xai", 4008),
    ("groq", 4009),
];

fn config(args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_turnpike"));
    cmd.arg("config").args(args).env_clear();
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("run turnpike config")
}

fn stdout(args: &[&str], env: &[(&str, &str)]) -> String {
    let out = config(args, env);
    assert!(
        out.status.success(),
        "turnpike config {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf-8 stdout")
}

#[test]
fn every_provider_answers_on_every_format() {
    for (name, port) in PROVIDERS {
        for format in ["table", "url", "shell", "fish", "json"] {
            let out = stdout(&[name, "--format", format], &[]);
            assert!(
                out.contains(&port.to_string()),
                "config {name} --format {format} never mentions port {port}: {out:?}"
            );
        }
    }
}

#[test]
fn a_named_provider_prints_the_url_and_nothing_else() {
    // The pasteable form: `curl $(turnpike config deepseek)/chat/completions`
    // must work, so no header, no label, no trailing prose.
    assert_eq!(stdout(&["deepseek"], &[]), "http://127.0.0.1:4003/v1\n");
    assert_eq!(stdout(&["anthropic"], &[]), "http://127.0.0.1:4001\n");
    // Including the one provider with no environment variable to set.
    assert_eq!(stdout(&["gemini"], &[]), "http://127.0.0.1:4002\n");
}

#[test]
fn bare_config_lists_every_provider() {
    let out = stdout(&[], &[]);
    for (name, port) in PROVIDERS {
        assert!(out.contains(name), "table omits {name}: {out}");
        assert!(out.contains(&port.to_string()), "table omits port {port}");
    }
}

#[test]
fn legacy_provider_flag_still_prints_an_export() {
    // A published README says `eval $(turnpike config --provider openrouter)`.
    // The positional spelling now prints a bare URL; the flag must not.
    let out = stdout(&["--provider", "openrouter"], &[]);
    assert_eq!(
        out.trim(),
        "export OPENAI_BASE_URL=http://127.0.0.1:4004/api/v1"
    );
}

#[test]
fn shell_output_is_safe_to_eval() {
    // zsh leaves `interactivecomments` off, so a `#` on a live line is an
    // argument to `export`, not a comment. Only disabled lines may carry one.
    for (name, _) in PROVIDERS {
        for format in ["shell", "fish"] {
            for out in [
                stdout(&[name, "--format", format], &[]),
                stdout(&["--format", format], &[]),
            ] {
                for line in out.lines().filter(|l| !l.trim_start().starts_with('#')) {
                    assert!(
                        !line.contains('#'),
                        "live {format} line would break eval in zsh: {line:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn fish_syntax_replaces_export() {
    let out = stdout(&["xai", "--format", "fish"], &[]);
    assert_eq!(
        out.trim(),
        "set -gx OPENAI_BASE_URL http://127.0.0.1:4008/v1"
    );
}

#[test]
fn shell_flag_follows_the_shell_in_the_environment() {
    let fish = stdout(&["xai", "--shell"], &[("SHELL", "/opt/homebrew/bin/fish")]);
    assert!(fish.starts_with("set -gx "), "expected fish syntax: {fish}");
    let zsh = stdout(&["xai", "--shell"], &[("SHELL", "/usr/bin/zsh")]);
    assert!(zsh.starts_with("export "), "expected posix syntax: {zsh}");
}

#[test]
fn an_unknown_provider_names_the_ones_that_exist() {
    let out = config(&["zhipu"], &[]);
    assert!(!out.status.success(), "unknown provider exited zero");
    let err = String::from_utf8_lossy(&out.stderr);
    for (name, _) in PROVIDERS {
        assert!(err.contains(name), "error never offers {name}: {err}");
    }
}

#[test]
fn status_says_direct_when_a_key_is_set_and_nothing_is_routed() {
    let out = stdout(&["--format", "json"], &[("DEEPSEEK_API_KEY", "sk-test")]);
    assert!(
        out.contains("\"status\": \"direct\""),
        "a key with no base URL should read as direct: {out}"
    );
    // And only for the provider whose key it is.
    assert_eq!(out.matches("\"status\": \"direct\"").count(), 1);
}

#[test]
fn the_port_decides_which_sharer_of_openai_base_url_is_routed() {
    // Eight providers read OPENAI_BASE_URL. Pointing it at 4003 routes
    // deepseek and leaves openai unrouted, which is the whole confusion
    // issue #10 was about.
    let env = [
        ("OPENAI_BASE_URL", "http://127.0.0.1:4003/v1"),
        ("DEEPSEEK_API_KEY", "sk-test"),
        ("OPENAI_API_KEY", "sk-test"),
    ];
    let json = stdout(&["--format", "json"], &env);
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
    let status = |name: &str| {
        parsed["providers"][name]["status"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(status("deepseek"), "routed");
    assert_eq!(status("openai"), "direct");
    assert_eq!(status("groq"), "-");
}

#[test]
fn gemini_is_never_reported_as_direct() {
    // python-genai takes a base URL in code only, so from out here turnpike
    // cannot know whether gemini traffic is routed. Guessing would be a lie.
    let json = stdout(&["--format", "json"], &[("GEMINI_API_KEY", "test")]);
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["providers"]["gemini"]["status"], "in code");
    assert!(parsed["providers"]["gemini"]["env"].is_null());
}

#[test]
fn json_carries_what_a_script_needs_to_configure_a_client() {
    let json = stdout(&["--format", "json"], &[]);
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
    for (name, port) in PROVIDERS {
        let p = &parsed["providers"][name];
        assert_eq!(p["port"], *port, "{name} port");
        assert!(
            p["base_url"].as_str().unwrap().contains(&port.to_string()),
            "{name} base_url"
        );
        assert!(
            p["alias_url"]
                .as_str()
                .unwrap()
                .starts_with(&format!("http://{name}.localhost:4000")),
            "{name} alias_url"
        );
        assert!(
            !p["key_envs"].as_array().unwrap().is_empty(),
            "{name} key_envs"
        );
        assert!(
            p["upstream"].as_str().unwrap().starts_with("https://"),
            "{name} upstream"
        );
    }
}
