//! What an environment does with each provider's key: the one classification
//! `turnpike config` prints as its `this shell` column and `turnpike doctor`
//! promotes to a finding. Shared so the two can never disagree about what
//! "routed" means.

use crate::providers::Provider;
use std::collections::HashMap;

/// What this environment does with a provider right now. The scope is the
/// process turnpike was launched from — the shell you are about to start
/// tools in — and nothing else on the machine.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Routing {
    /// A base URL variable points at this provider's turnpike listener.
    Routed,
    /// A key is set here and nothing points at turnpike: spend this meter
    /// will never see.
    Direct,
    /// A key is set here and the provider has no base-URL variable to read,
    /// so the answer lives in code turnpike cannot inspect. Never guessed as
    /// `Direct` — it is genuinely unknown from out here.
    InCode,
    /// No key found, so there is nothing to route.
    Absent,
}

impl Routing {
    pub fn label(self) -> &'static str {
        match self {
            Routing::Routed => "routed",
            Routing::Direct => "direct",
            Routing::InCode => "in code",
            Routing::Absent => "-",
        }
    }
}

pub fn routing(p: &Provider, env: &HashMap<String, String>) -> Routing {
    let var = base_url_env(p);
    if let Some(value) = var.and_then(|v| env.get(v)) {
        if points_at(value, p) {
            return Routing::Routed;
        }
    }
    match (key_var(p, env), var) {
        (None, _) => Routing::Absent,
        (Some(_), Some(_)) => Routing::Direct,
        (Some(_), None) => Routing::InCode,
    }
}

/// The first of the provider's key variables that is set to something, in
/// the vendor's own spelling first. Whitespace is not a key.
pub fn key_var(p: &Provider, env: &HashMap<String, String>) -> Option<&'static str> {
    p.key_envs
        .iter()
        .copied()
        .find(|k| env.get(*k).is_some_and(|v| !v.trim().is_empty()))
}

/// The variable a client reads for this provider's base URL, taken from the
/// export template so the two can never disagree.
pub fn base_url_env(p: &Provider) -> Option<&'static str> {
    p.env_template?
        .strip_prefix("export ")?
        .split_once('=')
        .map(|(name, _)| name)
}

/// Does this base URL reach *this* provider's listener? Eight providers share
/// `OPENAI_BASE_URL`, so the port (or the alias label) is the only thing that
/// says which one the variable currently names.
pub fn points_at(value: &str, p: &Provider) -> bool {
    let Some((host, port)) = host_port(value) else {
        return false;
    };
    if let Some(label) = host.strip_suffix(".localhost") {
        return label == p.name;
    }
    matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1") && port == Some(p.default_port)
}

fn host_port(url: &str) -> Option<(String, Option<u16>)> {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit('@').next()?;
    if let Some(bracketed) = authority.strip_prefix('[') {
        let (host, tail) = bracketed.split_once(']')?;
        let port = tail.strip_prefix(':').and_then(|p| p.parse().ok());
        return Some((host.to_ascii_lowercase(), port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => Some((host.to_ascii_lowercase(), port.parse().ok())),
        None => Some((authority.to_ascii_lowercase(), None)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::PROVIDERS;

    fn provider(name: &str) -> &'static Provider {
        PROVIDERS.iter().find(|p| p.name == name).unwrap()
    }

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn base_url_env_comes_from_the_template() {
        assert_eq!(base_url_env(provider("openai")), Some("OPENAI_BASE_URL"));
        assert_eq!(
            base_url_env(provider("anthropic")),
            Some("ANTHROPIC_BASE_URL")
        );
        assert_eq!(base_url_env(provider("gemini")), None);
    }

    #[test]
    fn the_port_decides_which_sharer_of_openai_base_url_is_routed() {
        let deepseek = env(&[("OPENAI_BASE_URL", "http://127.0.0.1:4003/v1")]);
        assert!(points_at("http://127.0.0.1:4003/v1", provider("deepseek")));
        assert!(!points_at("http://127.0.0.1:4003/v1", provider("openai")));
        assert_eq!(routing(provider("deepseek"), &deepseek), Routing::Routed);
    }

    #[test]
    fn alias_host_routes_by_name_from_any_port() {
        assert!(points_at("http://kimi.localhost:4000/v1", provider("kimi")));
        assert!(points_at("http://kimi.localhost:4009/v1", provider("kimi")));
        assert!(!points_at(
            "http://kimi.localhost:4000/v1",
            provider("groq")
        ));
    }

    #[test]
    fn ipv6_loopback_and_bare_hosts_are_understood() {
        assert!(points_at("http://[::1]:4001", provider("anthropic")));
        assert!(points_at("http://localhost:4001", provider("anthropic")));
        // No port is port 80, which is nobody's listener.
        assert!(!points_at("http://localhost", provider("anthropic")));
    }

    #[test]
    fn an_upstream_url_is_never_mistaken_for_turnpike() {
        assert!(!points_at("https://api.openai.com/v1", provider("openai")));
        assert!(!points_at(
            "https://openrouter.ai/api/v1",
            provider("openrouter")
        ));
    }

    #[test]
    fn a_key_with_no_base_url_is_reported_as_direct() {
        let e = env(&[("GROQ_API_KEY", "gsk_x")]);
        assert_eq!(routing(provider("groq"), &e), Routing::Direct);
        assert_eq!(routing(provider("xai"), &e), Routing::Absent);
    }

    #[test]
    fn an_empty_key_variable_is_not_a_key() {
        let e = env(&[("GROQ_API_KEY", "  ")]);
        assert_eq!(routing(provider("groq"), &e), Routing::Absent);
    }

    #[test]
    fn key_var_prefers_the_vendors_own_spelling() {
        let both = env(&[("MOONSHOT_API_KEY", "a"), ("KIMI_API_KEY", "b")]);
        assert_eq!(key_var(provider("kimi"), &both), Some("MOONSHOT_API_KEY"));
        let alias = env(&[("KIMI_API_KEY", "b")]);
        assert_eq!(key_var(provider("kimi"), &alias), Some("KIMI_API_KEY"));
    }

    #[test]
    fn gemini_is_never_guessed_to_be_direct() {
        // It has no base-URL variable, so the truth lives in code turnpike
        // cannot read. Claiming `direct` would be a guess, and a wrong one on
        // any machine that passes base_url in its client.
        let e = env(&[("GEMINI_API_KEY", "k")]);
        assert_eq!(routing(provider("gemini"), &e), Routing::InCode);
    }
}
