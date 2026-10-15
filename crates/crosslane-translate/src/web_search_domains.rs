//! Domain restrictions for proxy-executed web search.
//!
//! Kiro MCP only accepts a query. Search operators improve relevance, while
//! filtering returned URLs enforces the caller's restriction before any result
//! enters a model continuation, citation, or encrypted replay record.

use regex::Regex;
use url::Url;

use crate::WebSearchResults;

#[derive(Debug, Clone, Default)]
pub struct WebSearchDomainFilter {
    allowed: Vec<DomainPattern>,
    blocked: Vec<DomainPattern>,
}

#[derive(Debug, Clone)]
struct DomainPattern {
    host: String,
    path: Option<Regex>,
    site: String,
}

impl WebSearchDomainFilter {
    pub fn new(allowed: Option<&[String]>, blocked: Option<&[String]>) -> Result<Self, String> {
        if allowed.is_some() && blocked.is_some() {
            return Err("use either allowed_domains or blocked_domains, not both".into());
        }
        let parse = |domains: Option<&[String]>| {
            domains
                .unwrap_or_default()
                .iter()
                .map(|domain| DomainPattern::parse(domain))
                .collect::<Result<Vec<_>, _>>()
        };
        Ok(Self {
            allowed: parse(allowed)?,
            blocked: parse(blocked)?,
        })
    }

    pub fn search_query(&self, query: &str) -> String {
        let mut query = query.to_owned();
        if !self.allowed.is_empty() {
            query.push_str(" (");
            query.push_str(
                &self
                    .allowed
                    .iter()
                    .map(|domain| format!("site:{}", domain.site))
                    .collect::<Vec<_>>()
                    .join(" OR "),
            );
            query.push(')');
        }
        for domain in &self.blocked {
            query.push_str(" -site:");
            query.push_str(&domain.site);
        }
        query
    }

    pub fn filter_results(&self, query: &str, results: &mut WebSearchResults) {
        if self.allowed.is_empty() && self.blocked.is_empty() {
            return;
        }
        results.results.retain(|result| {
            let Ok(url) = Url::parse(&result.url) else {
                return false;
            };
            matches!(url.scheme(), "http" | "https")
                && url.username().is_empty()
                && url.password().is_none()
                && (self.allowed.is_empty()
                    || self.allowed.iter().any(|domain| domain.matches(&url)))
                && !self.blocked.iter().any(|domain| domain.matches(&url))
        });
        results.query = query.to_owned();
        results.total_results = results.results.len() as u64;
    }
}

impl DomainPattern {
    fn parse(value: &str) -> Result<Self, String> {
        let invalid = || {
            "domain filters must contain ASCII host names with an optional path; schemes, ports, credentials, query strings, fragments and host wildcards are not allowed".to_owned()
        };
        if !value.is_ascii()
            || value
                .bytes()
                .any(|ch| ch.is_ascii_whitespace() || ch.is_ascii_control())
            || value.contains([':', '@', '?', '#', '\\', '"', '\''])
        {
            return Err(invalid());
        }
        let (host, path) = value
            .split_once('/')
            .map_or((value, None), |(host, path)| (host, Some(path)));
        let host = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
        if host.is_empty()
            || host.len() > 253
            || host.split('.').any(|label| {
                label.is_empty()
                    || label.len() > 63
                    || label.starts_with('-')
                    || label.ends_with('-')
                    || !label
                        .bytes()
                        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'-' | b'_'))
            })
        {
            return Err(invalid());
        }
        let mut site = host.clone();
        if let Some(path) = path {
            site.push('/');
            site.push_str(path);
        }
        let parsed = Url::parse(&format!("https://{site}")).map_err(|_| invalid())?;
        if parsed.host_str() != Some(host.as_str()) {
            return Err(invalid());
        }
        let path = if path.is_some() {
            // URL parsing normalizes dot segments and escaping on both sides.
            // A rule is a prefix; '*' additionally matches any path characters.
            site = format!("{host}{}", parsed.path());
            let expression = format!(
                "^{}",
                parsed
                    .path()
                    .split('*')
                    .map(regex::escape)
                    .collect::<Vec<_>>()
                    .join(".*")
            );
            Some(Regex::new(&expression).map_err(|_| invalid())?)
        } else {
            None
        };
        Ok(Self { host, path, site })
    }

    fn matches(&self, url: &Url) -> bool {
        let Some(host) = url.host_str() else {
            return false;
        };
        let host = host.strip_suffix('.').unwrap_or(host);
        (host == self.host || host.ends_with(&format!(".{}", self.host)))
            && self
                .path
                .as_ref()
                .is_none_or(|path| path.is_match(url.path()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WebSearchResult;

    fn filtered(filter: &WebSearchDomainFilter, urls: &[&str]) -> WebSearchResults {
        let mut results = WebSearchResults {
            query: "upstream query".into(),
            total_results: 900,
            results: urls
                .iter()
                .map(|url| WebSearchResult {
                    url: (*url).into(),
                    ..Default::default()
                })
                .collect(),
        };
        filter.filter_results("original query", &mut results);
        results
    }

    #[test]
    fn allowlist_matches_host_boundaries_and_canonical_hosts() {
        let filter = WebSearchDomainFilter::new(Some(&["EXAMPLE.com".into()]), None).unwrap();
        let results = filtered(
            &filter,
            &[
                "https://example.com/one",
                "https://docs.example.com/two",
                "https://EXAMPLE.com./three",
                "https://example.com.evil.test/four",
                "https://evilexample.com/five",
                "https://example.com@evil.test/six",
                "https://evil.test@example.com/seven",
                "ftp://example.com/eight",
            ],
        );
        assert_eq!(results.results.len(), 3);
        assert_eq!(results.total_results, 3);
        assert_eq!(results.query, "original query");
        assert_eq!(filter.search_query("rust"), "rust (site:example.com)");
    }

    #[test]
    fn blocklist_excludes_subdomains_without_blocking_sibling_hosts() {
        let filter = WebSearchDomainFilter::new(None, Some(&["docs.example.com".into()])).unwrap();
        let results = filtered(
            &filter,
            &[
                "https://docs.example.com/a",
                "https://sub.docs.example.com/b",
                "https://example.com/c",
                "https://other.example.com/d",
                "not a URL",
            ],
        );
        assert_eq!(results.results.len(), 2);
        assert_eq!(filter.search_query("rust"), "rust -site:docs.example.com");
    }

    #[test]
    fn search_paths_support_prefixes_and_wildcards() {
        let filter =
            WebSearchDomainFilter::new(Some(&["example.com/*/articles".into()]), None).unwrap();
        let results = filtered(
            &filter,
            &[
                "https://docs.example.com/rust/articles/post",
                "https://example.com/other",
                "https://example.com/rust/articles?next=/other",
                "https://example.com/?q=/rust/articles",
            ],
        );
        assert_eq!(results.results.len(), 2);
        let filter =
            WebSearchDomainFilter::new(None, Some(&["example.com/private".into()])).unwrap();
        assert_eq!(
            filtered(
                &filter,
                &[
                    "https://example.com/private/one",
                    "https://example.com/public",
                ]
            )
            .results
            .len(),
            1
        );
    }

    #[test]
    fn malformed_rules_and_simultaneous_lists_are_rejected() {
        for rule in [
            "",
            "https://example.com",
            "example.com:443",
            "*.example.com",
            "example..com",
            "-bad.example",
            "example.com/a?b",
            "example.com/a#b",
            "user@example.com",
            "example.com\\evil",
            "example.com OR evil.test",
            "аmazon.com",
            "example.com/\n",
            "example.com/\"x\"",
        ] {
            assert!(
                WebSearchDomainFilter::new(Some(&[rule.into()]), None).is_err(),
                "{rule}"
            );
        }
        assert!(WebSearchDomainFilter::new(Some(&[]), Some(&[])).is_err());
        assert_eq!(
            WebSearchDomainFilter::default().search_query("rust"),
            "rust"
        );
    }
}
