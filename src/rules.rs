use anyhow::{Context as _, Result};
use globset::Glob;
use regex::Regex;

use crate::config::{MatchMode, MatchTarget, Rule};

#[derive(Debug, Clone)]
pub struct RequestFacts {
    pub method: String,
    pub host: String,
    pub url: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
}

pub fn matches(rule: &Rule, request: &RequestFacts) -> Result<bool> {
    if !rule.enabled {
        return Ok(false);
    }
    if !rule.methods.is_empty()
        && !rule
            .methods
            .iter()
            .any(|method| method.eq_ignore_ascii_case(&request.method))
    {
        return Ok(false);
    }

    let values: Vec<String> = match rule.target {
        MatchTarget::Host => vec![request.host.clone()],
        MatchTarget::Url => vec![request.url.clone()],
        MatchTarget::Path => vec![request.path.clone()],
        MatchTarget::Header => request
            .headers
            .iter()
            .filter(|(name, _)| {
                rule.header_name
                    .as_ref()
                    .is_none_or(|expected| expected.eq_ignore_ascii_case(name))
            })
            .map(|(name, value)| format!("{name}: {value}"))
            .collect(),
    };

    values.iter().try_fold(false, |matched, value| {
        Ok(matched || match_value(rule, value)?)
    })
}

fn match_value(rule: &Rule, value: &str) -> Result<bool> {
    let insensitive_value = value.to_lowercase();
    let insensitive_pattern = rule.pattern.to_lowercase();
    match rule.mode {
        MatchMode::Exact => Ok(insensitive_value == insensitive_pattern),
        MatchMode::Contains => Ok(insensitive_value.contains(&insensitive_pattern)),
        MatchMode::Glob => Ok(Glob::new(&insensitive_pattern)
            .with_context(|| format!("规则 {} 的 glob 无效", rule.id))?
            .compile_matcher()
            .is_match(&insensitive_value)),
        MatchMode::Regex => Ok(Regex::new(&rule.pattern)
            .with_context(|| format!("规则 {} 的正则表达式无效", rule.id))?
            .is_match(value)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{MatchMode, MatchTarget, Rule};

    fn request() -> RequestFacts {
        RequestFacts {
            method: "GET".into(),
            host: "news.example.com".into(),
            url: "http://news.example.com/private?a=1".into(),
            path: "/private?a=1".into(),
            headers: vec![("User-Agent".into(), "TestBot".into())],
        }
    }

    fn rule(target: MatchTarget, mode: MatchMode, pattern: &str) -> Rule {
        Rule {
            id: "test".into(),
            enabled: true,
            target,
            mode,
            pattern: pattern.into(),
            methods: vec![],
            header_name: None,
            actions: vec![],
        }
    }

    #[test]
    fn host_glob_matches() {
        assert!(
            matches(
                &rule(MatchTarget::Host, MatchMode::Glob, "*.example.com"),
                &request()
            )
            .unwrap()
        );
    }

    #[test]
    fn method_filter_is_respected() {
        let mut candidate = rule(MatchTarget::Path, MatchMode::Contains, "private");
        candidate.methods = vec!["POST".into()];
        assert!(!matches(&candidate, &request()).unwrap());
    }

    #[test]
    fn named_header_matches() {
        let mut candidate = rule(MatchTarget::Header, MatchMode::Contains, "testbot");
        candidate.header_name = Some("user-agent".into());
        assert!(matches(&candidate, &request()).unwrap());
    }
}
