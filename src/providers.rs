use crate::parsers::{
    merge_anthropic_sse, merge_gemini_sse, merge_openai_sse, parse_anthropic, parse_gemini,
    parse_openai,
};
use crate::record::Usage;
use serde_json::Value;

pub type ParseJson = fn(&Value) -> Usage;
pub type MergeSse = fn(&str, &Value, &mut Usage);

pub struct Provider {
    pub name: &'static str,
    pub upstream_url: &'static str,
    /// Variables that hold this provider's API key, vendor's own spelling
    /// first. A key present with nothing pointing at turnpike is spend this
    /// meter will never see, which is what `turnpike config` reports; the
    /// list is a best effort at the vendor's documented names, never a claim
    /// that a key exists only where turnpike can see it.
    pub key_envs: &'static [&'static str],
    pub default_port: u16,
    pub parse_json: ParseJson,
    /// Top-level non-streaming response field that carries usage accounting.
    pub json_usage_key: &'static str,
    pub merge_sse: MergeSse,
    /// Extract model from the request path. Returns None for most providers;
    /// Gemini encodes it as `/models/<name>:method`.
    pub model_from_path: fn(&str) -> Option<String>,
    /// Shell export template. `{port}` is substituted at print time.
    pub env_template: Option<&'static str>,
    /// Inject `stream_options: {include_usage: true}` into streaming requests so
    /// the final SSE chunk carries token counts. True for OpenAI-compatible APIs;
    /// false for Anthropic (reports via message_start/delta) and Gemini.
    pub inject_stream_options: bool,
}

fn no_model_from_path(_: &str) -> Option<String> {
    None
}

pub fn gemini_model_from_path(path: &str) -> Option<String> {
    let idx = path.find("/models/")?;
    let rest = &path[idx + "/models/".len()..];
    let end = rest.find([':', '/', '?']).unwrap_or(rest.len());
    if end == 0 {
        None
    } else {
        Some(rest[..end].to_string())
    }
}

pub static PROVIDERS: &[Provider] = &[
    Provider {
        name: "anthropic",
        key_envs: &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"],
        upstream_url: "https://api.anthropic.com",
        default_port: 4001,
        parse_json: parse_anthropic,
        json_usage_key: "usage",
        merge_sse: merge_anthropic_sse,
        model_from_path: no_model_from_path,
        env_template: Some("export ANTHROPIC_BASE_URL=http://127.0.0.1:{port}"),
        inject_stream_options: false,
    },
    Provider {
        name: "openai",
        key_envs: &["OPENAI_API_KEY"],
        upstream_url: "https://api.openai.com",
        default_port: 4000,
        parse_json: parse_openai,
        json_usage_key: "usage",
        merge_sse: merge_openai_sse,
        model_from_path: no_model_from_path,
        env_template: Some("export OPENAI_BASE_URL=http://127.0.0.1:{port}/v1"),
        inject_stream_options: true,
    },
    Provider {
        name: "deepseek",
        key_envs: &["DEEPSEEK_API_KEY"],
        upstream_url: "https://api.deepseek.com",
        default_port: 4003,
        parse_json: parse_openai,
        json_usage_key: "usage",
        merge_sse: merge_openai_sse,
        model_from_path: no_model_from_path,
        env_template: Some("export OPENAI_BASE_URL=http://127.0.0.1:{port}/v1"),
        inject_stream_options: true,
    },
    Provider {
        name: "openrouter",
        key_envs: &["OPENROUTER_API_KEY"],
        upstream_url: "https://openrouter.ai",
        default_port: 4004,
        parse_json: parse_openai,
        json_usage_key: "usage",
        merge_sse: merge_openai_sse,
        model_from_path: no_model_from_path,
        env_template: Some("export OPENAI_BASE_URL=http://127.0.0.1:{port}/api/v1"),
        inject_stream_options: true,
    },
    Provider {
        name: "gemini",
        key_envs: &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        upstream_url: "https://generativelanguage.googleapis.com",
        default_port: 4002,
        parse_json: parse_gemini,
        json_usage_key: "usageMetadata",
        merge_sse: merge_gemini_sse,
        model_from_path: gemini_model_from_path,
        env_template: None,
        inject_stream_options: false,
    },
    Provider {
        name: "kimi",
        key_envs: &["MOONSHOT_API_KEY", "KIMI_API_KEY"],
        upstream_url: "https://api.moonshot.ai",
        default_port: 4005,
        parse_json: parse_openai,
        json_usage_key: "usage",
        merge_sse: merge_openai_sse,
        model_from_path: no_model_from_path,
        env_template: Some("export OPENAI_BASE_URL=http://127.0.0.1:{port}/v1"),
        inject_stream_options: true,
    },
    Provider {
        name: "minimax",
        key_envs: &["MINIMAX_API_KEY"],
        upstream_url: "https://api.minimaxi.com",
        default_port: 4006,
        parse_json: parse_openai,
        json_usage_key: "usage",
        merge_sse: merge_openai_sse,
        model_from_path: no_model_from_path,
        env_template: Some("export OPENAI_BASE_URL=http://127.0.0.1:{port}/v1"),
        inject_stream_options: true,
    },
    Provider {
        name: "glm",
        key_envs: &["ZHIPUAI_API_KEY", "GLM_API_KEY"],
        upstream_url: "https://open.bigmodel.cn",
        default_port: 4007,
        parse_json: parse_openai,
        json_usage_key: "usage",
        merge_sse: merge_openai_sse,
        model_from_path: no_model_from_path,
        env_template: Some("export OPENAI_BASE_URL=http://127.0.0.1:{port}/api/paas/v4"),
        inject_stream_options: true,
    },
    Provider {
        name: "xai",
        key_envs: &["XAI_API_KEY"],
        upstream_url: "https://api.x.ai",
        default_port: 4008,
        parse_json: parse_openai,
        json_usage_key: "usage",
        merge_sse: merge_openai_sse,
        model_from_path: no_model_from_path,
        env_template: Some("export OPENAI_BASE_URL=http://127.0.0.1:{port}/v1"),
        inject_stream_options: true,
    },
    Provider {
        name: "groq",
        key_envs: &["GROQ_API_KEY"],
        upstream_url: "https://api.groq.com",
        default_port: 4009,
        parse_json: parse_openai,
        json_usage_key: "usage",
        merge_sse: merge_openai_sse,
        model_from_path: no_model_from_path,
        env_template: Some("export OPENAI_BASE_URL=http://127.0.0.1:{port}/openai/v1"),
        // Groq streams already carry usage in the final chunk's `x_groq`
        // object without being asked; no need to touch the request.
        inject_stream_options: false,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gemini_model_standard_path() {
        assert_eq!(
            gemini_model_from_path("/v1beta/models/gemini-1.5-pro:generateContent"),
            Some("gemini-1.5-pro".to_string())
        );
    }

    #[test]
    fn gemini_model_path_with_query() {
        // `?` must also terminate the model name
        assert_eq!(
            gemini_model_from_path("/v1beta/models/gemini-2.0-flash:streamGenerateContent?alt=sse"),
            Some("gemini-2.0-flash".to_string())
        );
    }
}

/// The provider roster is restated by hand on three documented surfaces: the
/// README's "Works with ..." list, the README's ports table, and the turnpike
/// skill's frontmatter description — the sentence that decides whether an
/// agent invokes the skill at all, so a name missing there fails silently.
/// These tests pin each surface to PROVIDERS so the roster can't change
/// without the docs moving with it.
#[cfg(test)]
mod roster_tests {
    use super::PROVIDERS;
    use std::collections::BTreeSet;

    /// Provider id -> the name the docs use in prose. A deliberate hand
    /// restatement: only a person can decide that `xai` reads as "xAI". The
    /// panic arm makes a new provider fail here first, forcing both this
    /// table and the doc surfaces to grow with the registry.
    fn display_name(id: &str) -> &'static str {
        match id {
            "openai" => "OpenAI",
            "anthropic" => "Anthropic",
            "gemini" => "Gemini",
            "deepseek" => "DeepSeek",
            "openrouter" => "OpenRouter",
            "kimi" => "Kimi",
            "minimax" => "MiniMax",
            "glm" => "GLM",
            "xai" => "xAI",
            "groq" => "Groq",
            other => panic!(
                "provider {other:?} has no display name; add it here, then to the README list, the ports table, and the skill description"
            ),
        }
    }

    fn repo_file(rel: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }

    fn expected_names() -> BTreeSet<String> {
        PROVIDERS
            .iter()
            .map(|p| display_name(p.name).to_string())
            .collect()
    }

    #[test]
    fn readme_intro_lists_exactly_the_providers() {
        let readme = repo_file("README.md");
        let start = readme
            .find("Works with")
            .expect("README lost its 'Works with' provider list");
        let paragraph = readme[start..].split("\n\n").next().unwrap();
        // Bold spans are the names: **OpenAI**, **Anthropic**, ...
        let listed: BTreeSet<String> = paragraph
            .split("**")
            .skip(1)
            .step_by(2)
            .map(str::to_string)
            .collect();
        assert_eq!(
            listed,
            expected_names(),
            "README 'Works with' list is out of sync with PROVIDERS"
        );
    }

    #[test]
    fn readme_ports_table_matches_the_registry() {
        let readme = repo_file("README.md");
        let start = readme
            .find("### Providers and ports")
            .expect("README lost its 'Providers and ports' section");
        let section = &readme[start..];
        let section = &section[..section.find("\n## ").unwrap_or(section.len())];

        let mut listed = BTreeSet::new();
        for line in section.lines().filter(|l| l.starts_with('|')) {
            let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
            if cells.len() != 4 || cells[0] == "Provider" || cells[0].starts_with("---") {
                continue;
            }
            let provider = PROVIDERS
                .iter()
                .find(|p| display_name(p.name) == cells[0])
                .unwrap_or_else(|| {
                    panic!("ports table row {:?} names no known provider", cells[0])
                });
            assert_eq!(
                cells[1],
                provider.default_port.to_string(),
                "{} port drifted from the registry",
                cells[0]
            );
            assert_eq!(
                cells[3],
                format!("`{}`", provider.upstream_url),
                "{} upstream drifted from the registry",
                cells[0]
            );
            listed.insert(cells[0].to_string());
        }
        assert_eq!(
            listed,
            expected_names(),
            "ports table rows are out of sync with PROVIDERS"
        );
    }

    #[test]
    fn skill_description_names_every_provider() {
        let skill = repo_file("skills/turnpike/SKILL.md");
        let desc = skill
            .lines()
            .find(|l| l.starts_with("description:"))
            .expect("skills/turnpike/SKILL.md lost its frontmatter description");
        for p in PROVIDERS {
            let name = display_name(p.name);
            assert!(
                desc.contains(name),
                "skill description never says {name:?}, so the skill will not fire for {} traffic",
                p.name
            );
        }
    }
}
