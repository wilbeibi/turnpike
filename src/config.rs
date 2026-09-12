use crate::providers::{Provider, PROVIDERS};
use crate::routing::{base_url_env, routing, Routing};
use anyhow::Result;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ConfigFormat {
    Table,
    Url,
    Shell,
    Fish,
    Json,
}

pub struct ConfigOpts {
    pub provider: Option<String>,
    /// The provider was named with the legacy `--provider` flag rather than
    /// positionally. It keeps the old default (shell exports) so
    /// `eval $(turnpike config --provider openrouter)` from a published
    /// README goes on working; the positional form prints a bare URL.
    pub legacy_provider_flag: bool,
    pub format: Option<ConfigFormat>,
    /// Emit exports in the syntax of `$SHELL`.
    pub shell_syntax: bool,
}

/// Any turnpike listener routes `<provider>.localhost` by name, so the alias
/// form needs just one well-known port. 4000 is the canonical choice.
const ALIAS_PORT: u16 = 4000;

pub fn run(opts: ConfigOpts) -> Result<()> {
    let providers = select_providers(opts.provider.as_deref())?;
    let single = opts.provider.is_some();
    let env: HashMap<String, String> = std::env::vars().collect();
    match resolve_format(&opts) {
        ConfigFormat::Table => print_table(&providers, &env),
        ConfigFormat::Url => print_urls(&providers, single),
        ConfigFormat::Shell => print_exports(&providers, single, shell_line),
        ConfigFormat::Fish => print_exports(&providers, single, fish_line),
        ConfigFormat::Json => print_json(&providers, &env),
    }
    Ok(())
}

/// No `--format` means the output is chosen by what was asked for: a named
/// provider wants one pasteable URL, no provider wants the whole picture.
fn resolve_format(opts: &ConfigOpts) -> ConfigFormat {
    if let Some(f) = opts.format {
        return f;
    }
    if opts.shell_syntax {
        return shell_syntax_for(std::env::var("SHELL").ok().as_deref());
    }
    match (opts.provider.is_some(), opts.legacy_provider_flag) {
        (true, true) => ConfigFormat::Shell,
        (true, false) => ConfigFormat::Url,
        (false, _) => ConfigFormat::Table,
    }
}

fn shell_syntax_for(shell: Option<&str>) -> ConfigFormat {
    match shell.and_then(|s| s.rsplit('/').next()) {
        Some("fish") => ConfigFormat::Fish,
        _ => ConfigFormat::Shell,
    }
}

/// The default surface: every provider's address, and whether this shell is
/// pointed at it. Ordered by port so it reads like the README's table.
fn print_table(providers: &[&Provider], env: &HashMap<String, String>) {
    let mut rows: Vec<(&Provider, Routing)> =
        providers.iter().map(|p| (*p, routing(p, env))).collect();
    rows.sort_by_key(|(p, _)| p.default_port);

    let w_name = rows
        .iter()
        .map(|(p, _)| p.name.len())
        .max()
        .unwrap_or(8)
        .max("provider".len());
    let w_url = rows
        .iter()
        .map(|(p, _)| base_url(p).len())
        .max()
        .unwrap_or(8)
        .max("base URL".len());
    let w_status = "this shell".len();

    println!(
        "{:<w_name$}  {:<w_url$}  this shell",
        "provider", "base URL"
    );
    println!(
        "{}  {}  {}",
        "-".repeat(w_name),
        "-".repeat(w_url),
        "-".repeat(w_status)
    );
    for (p, status) in &rows {
        println!(
            "{:<w_name$}  {:<w_url$}  {}",
            p.name,
            base_url(p),
            status.label()
        );
    }

    let leaking: Vec<&str> = rows
        .iter()
        .filter(|(_, s)| *s == Routing::Direct)
        .map(|(p, _)| p.name)
        .collect();
    // The example names a provider that is actually unrouted here, so the two
    // lines below are ones the reader can run exactly as printed.
    let example = leaking.first().copied().unwrap_or("openai");
    println!();
    if !leaking.is_empty() {
        // Deliberately not "unmetered": a tool that sets its own base URL is
        // routed whatever the environment says. What is true from out here is
        // that nothing in *this* environment points these keys at turnpike.
        println!(
            "{} key{} here, none routed: {}",
            leaking.len(),
            if leaking.len() == 1 { "" } else { "s" },
            leaking.join(", ")
        );
        println!("Anything that reads its base URL from the environment bypasses turnpike.");
        println!();
    }
    let paste = format!("turnpike config {example}");
    let route = format!("eval $(turnpike config {example} --shell)");
    let w = paste.len().max(route.len());
    println!("  {paste:<w$}   the base URL, to paste into an app");
    println!("  {route:<w$}   point this shell at it");
    println!();
    println!("glm is Zhipu/BigModel, kimi is Moonshot. gemini takes a base_url in code, not a");
    println!("variable, so turnpike cannot tell from out here whether it is routed. Anything");
    println!("that resolves *.localhost can use http://<provider>.localhost:{ALIAS_PORT}<path>.");
}

/// One bare URL per line for a single provider (pipeable); `name<TAB>url`
/// rows for the full listing. Always the numeric form: it is the one that
/// resolves everywhere, including macOS and slim containers.
fn print_urls(providers: &[&Provider], single_provider: bool) {
    for p in providers {
        if single_provider {
            println!("{}", base_url(p));
        } else {
            println!("{}\t{}", p.name, base_url(p));
        }
    }
}

/// `http://127.0.0.1:<port><provider path>` — what a client should be given.
pub fn base_url(p: &Provider) -> String {
    format!("http://127.0.0.1:{}{}", p.default_port, path_suffix(p))
}

/// The memorable base URL: `http://<name>.localhost:4000<provider path>`.
fn alias_url(p: &Provider) -> String {
    format!(
        "http://{}.localhost:{}{}",
        p.name,
        ALIAS_PORT,
        path_suffix(p)
    )
}

/// The vendor's own path prefix, carried in the export template after the port.
fn path_suffix(p: &Provider) -> &'static str {
    p.env_template
        .and_then(|t| t.split_once("{port}").map(|(_, s)| s))
        .unwrap_or("")
}

fn select_providers(provider: Option<&str>) -> Result<Vec<&'static Provider>> {
    match provider {
        Some(name) => PROVIDERS
            .iter()
            .find(|p| p.name == name)
            .map(|p| vec![p])
            .ok_or_else(|| {
                let names: Vec<&str> = PROVIDERS.iter().map(|p| p.name).collect();
                anyhow::anyhow!("unknown provider {name:?}; known: {}", names.join(", "))
            }),
        None => Ok(PROVIDERS.iter().collect()),
    }
}

fn print_exports(
    providers: &[&Provider],
    single_provider: bool,
    render: fn(&str, u16) -> Option<String>,
) {
    if !single_provider {
        println!("# Eight providers share OPENAI_BASE_URL, so only one can be set at a time.");
        println!("# Name the one you want: eval $(turnpike config <name> --shell)");
    }
    // A live export line never carries a trailing comment: zsh does not treat
    // `#` as a comment unless `interactivecomments` is set, so `eval $(...)`
    // would hand `#` to `export` as an argument. The name goes to the left of
    // the `#` that already disables the line, or nowhere.
    let w = providers
        .iter()
        .map(|p| p.name.len() + 1)
        .max()
        .unwrap_or(0);
    for p in providers {
        let label = format!("{}:", p.name);
        // A provider with no base-URL variable still has an address. Saying so
        // keeps every provider answerable on every surface, and a comment is
        // safe to `eval`.
        let absent = format!(
            "no base-URL variable — pass {} as base_url in code",
            base_url(p)
        );
        match (
            p.env_template.and_then(|t| render(t, p.default_port)),
            single_provider,
        ) {
            (Some(line), true) => println!("{line}"),
            (Some(line), false) => println!("# {label:<w$}  {line}"),
            (None, true) => println!("# {absent}"),
            (None, false) => println!("# {label:<w$}  {absent}"),
        }
    }
}

fn shell_line(template: &str, port: u16) -> Option<String> {
    Some(template.replace("{port}", &port.to_string()))
}

/// Templates are `export NAME=VALUE`; fish spells that `set -gx NAME VALUE`.
fn fish_line(template: &str, port: u16) -> Option<String> {
    let line = template.replace("{port}", &port.to_string());
    let body = line.strip_prefix("export ")?;
    let (name, value) = body.split_once('=')?;
    Some(format!("set -gx {name} {value}"))
}

fn print_json(providers: &[&Provider], env: &HashMap<String, String>) {
    let map: serde_json::Map<String, serde_json::Value> = providers
        .iter()
        .map(|p| {
            (
                p.name.to_string(),
                serde_json::json!({
                    "base_url": base_url(p),
                    "alias_url": alias_url(p),
                    "port": p.default_port,
                    "path": path_suffix(p),
                    "env": base_url_env(p),
                    "key_envs": p.key_envs,
                    "upstream": p.upstream_url,
                    "status": routing(p, env).label(),
                }),
            )
        })
        .collect();
    let out = serde_json::to_string_pretty(&serde_json::json!({"providers": map}))
        .expect("serializing known-valid JSON structure");
    println!("{out}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(name: &str) -> &'static Provider {
        PROVIDERS.iter().find(|p| p.name == name).unwrap()
    }

    #[test]
    fn fish_line_translates_export() {
        assert_eq!(
            fish_line("export OPENAI_BASE_URL=http://127.0.0.1:{port}/v1", 4008).as_deref(),
            Some("set -gx OPENAI_BASE_URL http://127.0.0.1:4008/v1")
        );
    }

    #[test]
    fn fish_line_rejects_non_export_templates() {
        assert_eq!(fish_line("FOO=bar", 1), None);
    }

    #[test]
    fn alias_url_keeps_provider_path_suffix() {
        assert_eq!(
            alias_url(provider("openrouter")),
            "http://openrouter.localhost:4000/api/v1"
        );
        assert_eq!(
            alias_url(provider("gemini")),
            "http://gemini.localhost:4000"
        );
    }

    #[test]
    fn base_url_is_the_numeric_form_every_client_resolves() {
        assert_eq!(
            base_url(provider("glm")),
            "http://127.0.0.1:4007/api/paas/v4"
        );
        assert_eq!(base_url(provider("gemini")), "http://127.0.0.1:4002");
    }

    #[test]
    fn format_defaults_follow_how_the_provider_was_named() {
        let opts = |provider: Option<&str>, legacy: bool| ConfigOpts {
            provider: provider.map(str::to_string),
            legacy_provider_flag: legacy,
            format: None,
            shell_syntax: false,
        };
        assert_eq!(resolve_format(&opts(None, false)), ConfigFormat::Table);
        assert_eq!(
            resolve_format(&opts(Some("openai"), false)),
            ConfigFormat::Url
        );
        // The legacy flag keeps the legacy default: `eval $(turnpike config
        // --provider openai)` in a published README must keep working.
        assert_eq!(
            resolve_format(&opts(Some("openai"), true)),
            ConfigFormat::Shell
        );
    }

    #[test]
    fn shell_syntax_follows_the_login_shell() {
        assert_eq!(shell_syntax_for(Some("/usr/bin/fish")), ConfigFormat::Fish);
        assert_eq!(shell_syntax_for(Some("/bin/zsh")), ConfigFormat::Shell);
        assert_eq!(shell_syntax_for(None), ConfigFormat::Shell);
    }

    #[test]
    fn unknown_provider_error_lists_every_name() {
        let err = select_providers(Some("zhipu")).err().unwrap().to_string();
        for p in PROVIDERS {
            assert!(err.contains(p.name), "error never names {}: {err}", p.name);
        }
    }
}
