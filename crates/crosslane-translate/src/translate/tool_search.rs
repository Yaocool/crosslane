use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use fancy_regex::RegexBuilder;
use regex::RegexBuilder as LinearRegexBuilder;
use serde_json::Value;

use crate::{ClaudeRequest, ClaudeTool, KiroTool, KiroToolUse};

use super::common::{kiro_tool, kiro_tool_named, ToolNameRegistry};
use super::web_search::ClaudeServerToolEmission;

const DEFAULT_SEARCH_RESULTS: usize = 5;
const MAX_SEARCH_RESULTS: usize = 10_000;
const MAX_REGEX_PATTERN_CHARS: usize = 200;
const MAX_BM25_QUERY_CHARS: usize = 500;
const MAX_FANCY_REGEX_SEARCH_MILLIS: u128 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchKind {
    Regex,
    Bm25,
}

#[derive(Debug, Clone)]
struct SearchTool {
    kind: SearchKind,
    response_name: String,
}

#[derive(Debug, Clone)]
struct DeferredTool {
    source_name: String,
    document: String,
    term_frequencies: HashMap<String, u32>,
    document_length: usize,
    translated: KiroTool,
    documentation: Option<String>,
    load_bytes: usize,
}

impl DeferredTool {
    fn kiro_name(&self) -> &str {
        self.translated
            .specification()
            .map(|tool| tool.name.as_str())
            .unwrap_or_default()
    }
}

/// Remaining capacity for one Tool Search result. The caller-provided `limit`
/// is only an upper bound; this budget keeps the selected definitions within
/// the next Kiro request's actual capacity.
#[derive(Debug, Clone, Copy)]
pub struct ClaudeToolSearchBudget {
    pub max_tools: usize,
    pub max_bytes: usize,
}

impl Default for ClaudeToolSearchBudget {
    fn default() -> Self {
        Self {
            max_tools: usize::MAX,
            max_bytes: usize::MAX,
        }
    }
}

/// A server-side Tool Search operation synthesized by crosslane.
#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeToolSearchTrace {
    pub id: String,
    pub name: String,
    pub input: Value,
    pub references: Vec<String>,
    pub error: Option<ClaudeToolSearchError>,
    pub requested_limit: usize,
    pub matched_count: usize,
    pub budget_truncated: bool,
    pub emission: ClaudeServerToolEmission,
}

impl ClaudeToolSearchTrace {
    pub fn pending(id: String, name: String, input: Value) -> Self {
        Self {
            id,
            name,
            input,
            references: Vec::new(),
            error: None,
            requested_limit: DEFAULT_SEARCH_RESULTS,
            matched_count: 0,
            budget_truncated: false,
            emission: ClaudeServerToolEmission::Pending,
        }
    }

    pub fn result_only(mut self) -> Self {
        self.emission = ClaudeServerToolEmission::ResultOnly;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeToolSearchError {
    pub code: String,
    pub message: String,
}

/// Search result plus the Kiro definitions that should be loaded next round.
#[derive(Debug, Clone)]
pub struct ClaudeToolSearchOutcome {
    pub trace: ClaudeToolSearchTrace,
    pub tools: Vec<KiroTool>,
    pub documentation: Vec<String>,
    pub truncated: bool,
}

/// Complete deferred catalog retained by the proxy but omitted from Kiro requests.
#[derive(Debug, Clone)]
pub struct ClaudeToolSearchCatalog {
    search_tools: BTreeMap<String, SearchTool>,
    deferred: Vec<DeferredTool>,
    document_frequencies: HashMap<String, usize>,
    average_document_length: f64,
}

impl ClaudeToolSearchCatalog {
    pub fn from_request(request: &ClaudeRequest) -> Option<Self> {
        let names = ToolNameRegistry::new(request.tools.iter().map(|tool| tool.name.as_str()));
        let deferred = request
            .tools
            .iter()
            .filter(|tool| tool.defer_loading && !is_tool_search_tool(tool))
            .cloned()
            .collect::<Vec<_>>();
        let search_tools = request
            .tools
            .iter()
            .filter_map(|tool| {
                let kind = search_kind(tool.r#type.as_deref())?;
                Some((
                    names.kiro_name(&tool.name),
                    SearchTool {
                        kind,
                        response_name: tool.name.clone(),
                    },
                ))
            })
            .collect::<BTreeMap<_, _>>();
        build_catalog(search_tools, deferred, names)
    }

    /// Moves deferred definitions out of a parsed request after translation so
    /// large catalogs are not duplicated for the lifetime of an upstream call.
    pub fn take_from_request(request: &mut ClaudeRequest) -> Option<Self> {
        let names = ToolNameRegistry::new(request.tools.iter().map(|tool| tool.name.as_str()));
        let search_tools = request
            .tools
            .iter()
            .filter_map(|tool| {
                let kind = search_kind(tool.r#type.as_deref())?;
                Some((
                    names.kiro_name(&tool.name),
                    SearchTool {
                        kind,
                        response_name: tool.name.clone(),
                    },
                ))
            })
            .collect::<BTreeMap<_, _>>();
        if search_tools.is_empty() {
            return None;
        }
        let mut deferred = Vec::new();
        let mut loaded = Vec::with_capacity(request.tools.len());
        for tool in std::mem::take(&mut request.tools) {
            if tool.defer_loading && !is_tool_search_tool(&tool) {
                deferred.push(tool);
            } else {
                loaded.push(tool);
            }
        }
        request.tools = loaded;
        build_catalog(search_tools, deferred, names)
    }

    pub fn deferred_len(&self) -> usize {
        self.deferred.len()
    }

    pub fn is_search_tool(&self, name: &str) -> bool {
        self.search_tools.contains_key(name)
    }

    pub fn pending_trace(&self, tool_use: &KiroToolUse) -> ClaudeToolSearchTrace {
        let name = self
            .search_tools
            .get(&tool_use.name)
            .map(|search| search.response_name.clone())
            .unwrap_or_else(|| tool_use.name.clone());
        ClaudeToolSearchTrace::pending(tool_use.tool_use_id.clone(), name, tool_use.input.clone())
    }

    /// Produce an in-band server-tool failure without scanning the catalog.
    /// This is used when a request has exhausted the proxy's aggregate Tool
    /// Search execution budget.
    pub fn unavailable_outcome(
        &self,
        tool_use: &KiroToolUse,
        message: impl Into<String>,
    ) -> ClaudeToolSearchOutcome {
        let response_name = self
            .search_tools
            .get(&tool_use.name)
            .map(|search| search.response_name.clone())
            .unwrap_or_else(|| tool_use.name.clone());
        error_outcome(tool_use, response_name, "unavailable", message)
    }

    pub fn search(&self, tool_use: &KiroToolUse) -> ClaudeToolSearchOutcome {
        self.search_with_budget(tool_use, ClaudeToolSearchBudget::default())
    }

    pub fn search_with_budget(
        &self,
        tool_use: &KiroToolUse,
        budget: ClaudeToolSearchBudget,
    ) -> ClaudeToolSearchOutcome {
        self.search_with_budget_excluding(tool_use, budget, &HashSet::new())
    }

    /// Search while omitting definitions that are already present in the Kiro
    /// working set. This prevents repeated searches from consuming budget or
    /// driving an internal continuation loop without discovering anything.
    pub fn search_with_budget_excluding(
        &self,
        tool_use: &KiroToolUse,
        budget: ClaudeToolSearchBudget,
        loaded_kiro_names: &HashSet<String>,
    ) -> ClaudeToolSearchOutcome {
        let Some(search) = self.search_tools.get(&tool_use.name) else {
            return error_outcome(
                tool_use,
                tool_use.name.clone(),
                "unavailable",
                "unknown tool search implementation",
            );
        };
        let result = match search.kind {
            SearchKind::Regex => self.regex_search(&tool_use.input, loaded_kiro_names),
            SearchKind::Bm25 => self.bm25_search(&tool_use.input, loaded_kiro_names),
        };
        match result {
            Ok((matches, requested_limit)) => self.success_outcome(
                tool_use,
                &search.response_name,
                matches,
                requested_limit,
                budget,
                loaded_kiro_names,
            ),
            Err((code, message)) => {
                error_outcome(tool_use, search.response_name.clone(), code, message)
            }
        }
    }

    /// Runs catalog matching away from the async HTTP runtime. Large BM25
    /// catalogs and bounded fancy-regex evaluation are CPU work and must not
    /// stall unrelated requests on a Tokio worker.
    pub async fn search_with_budget_excluding_async(
        self: &Arc<Self>,
        tool_use: KiroToolUse,
        budget: ClaudeToolSearchBudget,
        loaded_kiro_names: HashSet<String>,
    ) -> Result<ClaudeToolSearchOutcome, String> {
        let catalog = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            catalog.search_with_budget_excluding(&tool_use, budget, &loaded_kiro_names)
        })
        .await
        .map_err(|error| format!("Tool Search worker failed: {error}"))
    }

    fn regex_search(
        &self,
        input: &Value,
        loaded_kiro_names: &HashSet<String>,
    ) -> Result<(Vec<usize>, usize), (&'static str, String)> {
        let pattern = input
            .get("pattern")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                (
                    "invalid_tool_input",
                    "tool search requires a string pattern".to_owned(),
                )
            })?;
        if pattern.chars().count() > MAX_REGEX_PATTERN_CHARS {
            return Err((
                "invalid_tool_input",
                format!("tool search pattern exceeds {MAX_REGEX_PATTERN_CHARS} characters"),
            ));
        }
        let requested_limit = validate_search_input(input, "pattern")?;
        let mut matches = Vec::new();
        if let Ok(expression) = LinearRegexBuilder::new(pattern)
            .case_insensitive(true)
            .build()
        {
            for (index, tool) in self.deferred.iter().enumerate() {
                if loaded_kiro_names.contains(tool.kiro_name()) {
                    continue;
                }
                if expression.is_match(&tool.document) {
                    matches.push(index);
                    if matches.len() >= requested_limit {
                        break;
                    }
                }
            }
            return Ok((matches, requested_limit));
        }

        // Advanced constructs such as look-around use a bounded backtracking
        // engine. Apply both a per-match backtrack limit and a whole-search
        // wall-clock budget so a pattern cannot multiply worst-case work by a
        // 10,000-entry catalog.
        let expression = RegexBuilder::new(&format!("(?i:{pattern})"))
            .backtrack_limit(100_000)
            .build()
            .map_err(|error| {
                (
                    "invalid_tool_input",
                    format!("invalid regex pattern: {error}"),
                )
            })?;
        let started = std::time::Instant::now();
        for (index, tool) in self.deferred.iter().enumerate() {
            if loaded_kiro_names.contains(tool.kiro_name()) {
                continue;
            }
            if started.elapsed().as_millis() > MAX_FANCY_REGEX_SEARCH_MILLIS {
                return Err((
                    "execution_time_exceeded",
                    "regex matching exceeded the safe execution time limit".into(),
                ));
            }
            let matched = expression.is_match(&tool.document).map_err(|error| {
                (
                    "execution_time_exceeded",
                    format!("regex matching exceeded the safe execution limit: {error}"),
                )
            })?;
            if matched {
                matches.push(index);
                if matches.len() >= requested_limit {
                    break;
                }
            }
        }
        Ok((matches, requested_limit))
    }

    fn bm25_search(
        &self,
        input: &Value,
        loaded_kiro_names: &HashSet<String>,
    ) -> Result<(Vec<usize>, usize), (&'static str, String)> {
        let query = input.get("query").and_then(Value::as_str).ok_or_else(|| {
            (
                "invalid_tool_input",
                "tool search requires a string query".to_owned(),
            )
        })?;
        if query.chars().count() > MAX_BM25_QUERY_CHARS {
            return Err((
                "invalid_tool_input",
                format!("tool search query exceeds {MAX_BM25_QUERY_CHARS} characters"),
            ));
        }
        let requested_limit = validate_search_input(input, "query")?;
        let query_terms = tokenize(query).into_iter().collect::<HashSet<_>>();
        if query_terms.is_empty() {
            return Ok((Vec::new(), requested_limit));
        }
        let document_count = self.deferred.len() as f64;
        let mut scores = self
            .deferred
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                if loaded_kiro_names.contains(entry.kiro_name()) {
                    return None;
                }
                let length = entry.document_length.max(1) as f64;
                let mut score = 0.0;
                for term in &query_terms {
                    let term_frequency = entry
                        .term_frequencies
                        .get(term)
                        .copied()
                        .unwrap_or_default() as f64;
                    if term_frequency == 0.0 {
                        continue;
                    }
                    let containing = self
                        .document_frequencies
                        .get(term)
                        .copied()
                        .unwrap_or_default() as f64;
                    let inverse =
                        ((document_count - containing + 0.5) / (containing + 0.5) + 1.0).ln();
                    let normalized = term_frequency * 2.2
                        / (term_frequency
                            + 1.2 * (0.25 + 0.75 * length / self.average_document_length));
                    score += inverse * normalized;
                }
                (score > 0.0).then_some((index, score))
            })
            .collect::<Vec<_>>();
        scores.sort_by(|left, right| {
            right
                .1
                .partial_cmp(&left.1)
                .unwrap_or(Ordering::Equal)
                .then_with(|| left.0.cmp(&right.0))
        });
        Ok((
            scores
                .into_iter()
                .take(requested_limit)
                .map(|(index, _)| index)
                .collect(),
            requested_limit,
        ))
    }

    fn success_outcome(
        &self,
        tool_use: &KiroToolUse,
        response_name: &str,
        matches: Vec<usize>,
        requested_limit: usize,
        budget: ClaudeToolSearchBudget,
        loaded_kiro_names: &HashSet<String>,
    ) -> ClaudeToolSearchOutcome {
        debug_assert!(matches
            .iter()
            .all(|index| !loaded_kiro_names.contains(self.deferred[*index].kiro_name())));
        let match_count = matches.len();
        let mut references = Vec::with_capacity(match_count.min(budget.max_tools));
        let mut tools = Vec::with_capacity(match_count.min(budget.max_tools));
        let mut documentation = Vec::new();
        let mut loaded_bytes = 0usize;
        for index in matches {
            let tool = &self.deferred[index];
            if tools.len() >= budget.max_tools
                || loaded_bytes.saturating_add(tool.load_bytes) > budget.max_bytes
            {
                break;
            }
            loaded_bytes = loaded_bytes.saturating_add(tool.load_bytes);
            references.push(tool.source_name.clone());
            tools.push(tool.translated.clone());
            documentation.extend(tool.documentation.clone());
        }
        let truncated = references.len() < match_count;
        let error = (match_count > 0 && references.is_empty()).then(|| ClaudeToolSearchError {
            code: "unavailable".into(),
            message: "matching tool definitions exceed the remaining request budget".into(),
        });
        ClaudeToolSearchOutcome {
            trace: ClaudeToolSearchTrace {
                id: tool_use.tool_use_id.clone(),
                name: response_name.to_owned(),
                input: tool_use.input.clone(),
                references,
                error,
                requested_limit,
                matched_count: match_count,
                budget_truncated: truncated,
                emission: ClaudeServerToolEmission::Complete,
            },
            tools,
            documentation,
            truncated,
        }
    }
}

fn build_catalog(
    search_tools: BTreeMap<String, SearchTool>,
    deferred: Vec<ClaudeTool>,
    names: ToolNameRegistry,
) -> Option<ClaudeToolSearchCatalog> {
    if search_tools.is_empty() {
        return None;
    }
    let needs_regex_index = search_tools
        .values()
        .any(|search| search.kind == SearchKind::Regex);
    let needs_bm25_index = search_tools
        .values()
        .any(|search| search.kind == SearchKind::Bm25);
    let deferred = deferred
        .into_iter()
        .map(|source| {
            let source_name = source.name.clone();
            let searchable_text = search_document(&source);
            let terms = if needs_bm25_index {
                tokenize(&searchable_text)
            } else {
                Vec::new()
            };
            let document_length = terms.len();
            let mut term_frequencies = HashMap::new();
            for term in terms {
                *term_frequencies.entry(term).or_insert(0u32) += 1;
            }
            let document = if needs_regex_index {
                searchable_text
            } else {
                String::new()
            };
            let (translated, documentation) =
                translate_deferred_tool(&source, &names.kiro_name(&source.name));
            let load_bytes = serde_json::to_vec(&translated)
                .map_or(usize::MAX, |value| value.len())
                .saturating_add(documentation.as_ref().map_or(0, String::len));
            DeferredTool {
                source_name,
                document,
                term_frequencies,
                document_length,
                translated,
                documentation,
                load_bytes,
            }
        })
        .collect::<Vec<_>>();
    let mut document_frequencies = HashMap::new();
    for entry in &deferred {
        for term in entry.term_frequencies.keys() {
            *document_frequencies.entry(term.clone()).or_default() += 1;
        }
    }
    let average_document_length = deferred
        .iter()
        .map(|entry| entry.document_length)
        .sum::<usize>()
        .max(1) as f64
        / deferred.len().max(1) as f64;
    Some(ClaudeToolSearchCatalog {
        search_tools,
        deferred,
        document_frequencies,
        average_document_length,
    })
}

fn translate_deferred_tool(tool: &ClaudeTool, kiro_name: &str) -> (KiroTool, Option<String>) {
    match tool.r#type.as_deref() {
        Some(kind) if crate::matches_type_family(kind, "web_search") => kiro_tool(
            "web_search",
            "Search the web for real-time information. Returns relevant search results with titles, URLs, and snippets.",
            &serde_json::json!({
                "type":"object",
                "properties":{"query":{"type":"string","description":"The search query"}},
                "required":["query"]
            }),
        ),
        Some(kind) if crate::matches_type_family(kind, "web_fetch") => kiro_tool(
            "web_fetch",
            "Fetch and read content from a specific URL. Returns the page content in readable text format.",
            &serde_json::json!({
                "type":"object",
                "properties":{"url":{"type":"string","description":"The URL to fetch content from"}},
                "required":["url"]
            }),
        ),
        _ => kiro_tool_named(
            &tool.name,
            kiro_name,
            &tool_description(tool),
            &tool.input_schema,
        ),
    }
}

pub fn is_tool_search_type(kind: &str) -> bool {
    search_kind(Some(kind)).is_some()
}

pub fn is_tool_search_tool(tool: &ClaudeTool) -> bool {
    tool.r#type.as_deref().is_some_and(is_tool_search_type)
}

pub fn tool_search_kiro_tool(tool: &ClaudeTool) -> Option<KiroTool> {
    tool_search_kiro_tool_named(tool, &super::common::tool_name(&tool.name))
}

pub(crate) fn tool_search_kiro_tool_named(tool: &ClaudeTool, kiro_name: &str) -> Option<KiroTool> {
    let kind = search_kind(tool.r#type.as_deref())?;
    let (description, schema) = match kind {
        SearchKind::Regex => (
            "Search deferred tools by regular expression. Call this tool alone when the required tool is not currently loaded. The optional limit defaults to 5 and may be set from 1 to 10000; the proxy may return fewer definitions when the request budget is exhausted.",
            serde_json::json!({
                "type":"object",
                "properties":{
                    "pattern":{"type":"string","maxLength":MAX_REGEX_PATTERN_CHARS},
                    "limit":{"type":"integer","minimum":1,"maximum":MAX_SEARCH_RESULTS,"default":DEFAULT_SEARCH_RESULTS}
                },
                "additionalProperties":false,
                "required":["pattern"]
            }),
        ),
        SearchKind::Bm25 => (
            "Search deferred tools using a natural-language query. Call this tool alone when the required tool is not currently loaded. The optional limit defaults to 5 and may be set from 1 to 10000; the proxy may return fewer definitions when the request budget is exhausted.",
            serde_json::json!({
                "type":"object",
                "properties":{
                    "query":{"type":"string","maxLength":MAX_BM25_QUERY_CHARS},
                    "limit":{"type":"integer","minimum":1,"maximum":MAX_SEARCH_RESULTS,"default":DEFAULT_SEARCH_RESULTS}
                },
                "additionalProperties":false,
                "required":["query"]
            }),
        ),
    };
    Some(kiro_tool_named(&tool.name, kiro_name, description, &schema).0)
}

fn search_kind(kind: Option<&str>) -> Option<SearchKind> {
    let kind = kind?;
    if crate::matches_type_family(kind, "tool_search_tool_regex") {
        Some(SearchKind::Regex)
    } else if crate::matches_type_family(kind, "tool_search_tool_bm25") {
        Some(SearchKind::Bm25)
    } else {
        None
    }
}

fn search_document(tool: &ClaudeTool) -> String {
    format!(
        "{}\n{}\n{}",
        tool.name,
        tool.description,
        serde_json::to_string(&tool.input_schema).unwrap_or_default()
    )
}

fn validate_search_input(
    input: &Value,
    query_field: &str,
) -> Result<usize, (&'static str, String)> {
    let Some(object) = input.as_object() else {
        return Err((
            "invalid_tool_input",
            "tool search input must be an object".into(),
        ));
    };
    if let Some(field) = object
        .keys()
        .find(|field| field.as_str() != query_field && field.as_str() != "limit")
    {
        return Err((
            "invalid_tool_input",
            format!("unsupported tool search input field '{field}'"),
        ));
    }
    let Some(limit) = object.get("limit") else {
        return Ok(DEFAULT_SEARCH_RESULTS);
    };
    let Some(limit) = limit.as_u64() else {
        return Err((
            "invalid_tool_input",
            "tool search limit must be an integer from 1 to 10000".into(),
        ));
    };
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    if !(1..=MAX_SEARCH_RESULTS).contains(&limit) {
        return Err((
            "invalid_tool_input",
            "tool search limit must be an integer from 1 to 10000".into(),
        ));
    }
    Ok(limit)
}

fn tokenize(input: &str) -> Vec<String> {
    input
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn tool_description(tool: &ClaudeTool) -> String {
    let Some(examples) = tool
        .input_examples
        .as_ref()
        .filter(|examples| !examples.is_empty())
    else {
        return tool.description.clone();
    };
    let examples = serde_json::to_string_pretty(examples).unwrap_or_else(|_| "[]".into());
    if tool.description.trim().is_empty() {
        format!("Input examples:\n{examples}")
    } else {
        format!("{}\n\nInput examples:\n{examples}", tool.description.trim())
    }
}

fn error_outcome(
    tool_use: &KiroToolUse,
    response_name: String,
    code: impl Into<String>,
    message: impl Into<String>,
) -> ClaudeToolSearchOutcome {
    ClaudeToolSearchOutcome {
        trace: ClaudeToolSearchTrace {
            id: tool_use.tool_use_id.clone(),
            name: response_name,
            input: tool_use.input.clone(),
            references: Vec::new(),
            error: Some(ClaudeToolSearchError {
                code: code.into(),
                message: message.into(),
            }),
            requested_limit: DEFAULT_SEARCH_RESULTS,
            matched_count: 0,
            budget_truncated: false,
            emission: ClaudeServerToolEmission::Complete,
        },
        tools: Vec::new(),
        documentation: Vec::new(),
        truncated: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(kind: &str) -> ClaudeRequest {
        let name = match search_kind(Some(kind)).expect("search type") {
            SearchKind::Regex => "tool_search_tool_regex",
            SearchKind::Bm25 => "tool_search_tool_bm25",
        };
        serde_json::from_value(serde_json::json!({
            "model":"claude-sonnet-4-5",
            "max_tokens":256,
            "messages":[{"role":"user","content":"find an issue"}],
            "tools":[
                {"type":kind,"name":name},
                {"name":"mcp__github__list_issues","description":"List GitHub issues by repository","input_schema":{"type":"object"},"defer_loading":true},
                {"name":"mcp__slack__search","description":"Search Slack messages","input_schema":{"type":"object"},"defer_loading":true}
            ]
        }))
        .expect("request")
    }

    #[test]
    fn accepts_opaque_tool_search_version_suffixes() {
        assert!(is_tool_search_type("tool_search_tool_regex_next"));
        assert!(is_tool_search_type("tool_search_tool_bm25_v2-preview"));
        assert!(!is_tool_search_type("tool_search_tool_vector_next"));

        ClaudeToolSearchCatalog::from_request(&request("tool_search_tool_regex_next"))
            .expect("opaque regex version");
        ClaudeToolSearchCatalog::from_request(&request("tool_search_tool_bm25_v2-preview"))
            .expect("opaque BM25 version");
    }

    fn request_with_matching_tools(kind: &str, count: usize) -> ClaudeRequest {
        let mut request = request(kind);
        request.tools.truncate(1);
        request.tools.extend((0..count).map(|index| ClaudeTool {
            r#type: None,
            name: format!("mcp__catalog__searchable_{index}"),
            description: format!("Search the shared catalog item {index}"),
            input_schema: serde_json::json!({"type":"object"}),
            cache_control: None,
            strict: None,
            input_examples: None,
            defer_loading: true,
            allowed_callers: None,
            eager_input_streaming: None,
            max_uses: None,
            allowed_domains: None,
            blocked_domains: None,
            user_location: None,
            response_inclusion: None,
            extra: Default::default(),
        }));
        request
    }

    #[test]
    fn regex_search_loads_only_matching_deferred_tools() {
        let catalog =
            ClaudeToolSearchCatalog::from_request(&request("tool_search_tool_regex_20251119"))
                .expect("catalog");
        let outcome = catalog.search(&KiroToolUse {
            tool_use_id: "srvtoolu_1".into(),
            name: "tool_search_tool_regex".into(),
            input: serde_json::json!({"pattern":"(?i)github|issue"}),
        });
        assert_eq!(outcome.trace.references, ["mcp__github__list_issues"]);
        assert_eq!(outcome.tools.len(), 1);
        assert!(outcome.trace.error.is_none());

        let lookahead = catalog.search(&KiroToolUse {
            tool_use_id: "srvtoolu_lookahead".into(),
            name: "tool_search_tool_regex".into(),
            input: serde_json::json!({"pattern":"GITHUB(?=__LIST)"}),
        });
        assert_eq!(lookahead.trace.references, ["mcp__github__list_issues"]);
    }

    #[test]
    fn tool_search_without_deferred_tools_still_returns_an_empty_server_result() {
        let mut request = request("tool_search_tool_regex_20251119");
        request.tools.truncate(1);
        let catalog = ClaudeToolSearchCatalog::from_request(&request)
            .expect("the built-in search tool remains executable");
        let outcome = catalog.search(&KiroToolUse {
            tool_use_id: "srvtoolu_empty".into(),
            name: "tool_search_tool_regex".into(),
            input: serde_json::json!({"pattern":"anything"}),
        });

        assert!(outcome.trace.references.is_empty());
        assert!(outcome.trace.error.is_none());
        assert_eq!(outcome.trace.requested_limit, DEFAULT_SEARCH_RESULTS);
    }

    #[test]
    fn repeated_search_omits_tools_already_loaded_in_the_working_set() {
        let catalog =
            ClaudeToolSearchCatalog::from_request(&request("tool_search_tool_regex_20251119"))
                .expect("catalog");
        let loaded = HashSet::from(["mcp__github__list_issues".to_owned()]);
        let outcome = catalog.search_with_budget_excluding(
            &KiroToolUse {
                tool_use_id: "srvtoolu_repeat".into(),
                name: "tool_search_tool_regex".into(),
                input: serde_json::json!({"pattern":"github|slack"}),
            },
            ClaudeToolSearchBudget::default(),
            &loaded,
        );

        assert_eq!(outcome.trace.references, ["mcp__slack__search"]);
        assert_eq!(outcome.tools.len(), 1);
    }

    #[test]
    fn bm25_search_ranks_relevant_tools() {
        let catalog =
            ClaudeToolSearchCatalog::from_request(&request("tool_search_tool_bm25_20251119"))
                .expect("catalog");
        let outcome = catalog.search(&KiroToolUse {
            tool_use_id: "srvtoolu_1".into(),
            name: "tool_search_tool_bm25".into(),
            input: serde_json::json!({"query":"github repository issues"}),
        });
        assert_eq!(outcome.trace.references[0], "mcp__github__list_issues");
    }

    #[test]
    fn search_defaults_to_five_and_honors_the_official_limit() {
        let catalog = ClaudeToolSearchCatalog::from_request(&request_with_matching_tools(
            "tool_search_tool_regex_20251119",
            12,
        ))
        .expect("catalog");

        let default = catalog.search(&KiroToolUse {
            tool_use_id: "srvtoolu_default".into(),
            name: "tool_search_tool_regex".into(),
            input: serde_json::json!({"pattern":"searchable"}),
        });
        assert_eq!(default.trace.references.len(), 5);

        let expanded = catalog.search(&KiroToolUse {
            tool_use_id: "srvtoolu_expanded".into(),
            name: "tool_search_tool_regex".into(),
            input: serde_json::json!({"pattern":"searchable", "limit":9}),
        });
        assert_eq!(expanded.trace.references.len(), 9);
        assert_eq!(expanded.trace.requested_limit, 9);
        assert!(expanded.trace.error.is_none());

        for input in [
            serde_json::json!({"pattern":"searchable", "limit":0}),
            serde_json::json!({"pattern":"searchable", "limit":10_001}),
            serde_json::json!({"pattern":"searchable", "limit":2.5}),
            serde_json::json!({"pattern":"searchable", "unexpected":1}),
        ] {
            let invalid = catalog.search(&KiroToolUse {
                tool_use_id: "srvtoolu_invalid".into(),
                name: "tool_search_tool_regex".into(),
                input,
            });
            assert!(invalid.trace.references.is_empty());
            assert_eq!(
                invalid
                    .trace
                    .error
                    .as_ref()
                    .map(|error| error.code.as_str()),
                Some("invalid_tool_input")
            );
        }
    }

    #[test]
    fn catalog_scales_to_ten_thousand_and_packs_by_remaining_budget() {
        let empty = request_with_matching_tools("tool_search_tool_regex_20251119", 0);
        assert_eq!(
            ClaudeToolSearchCatalog::from_request(&empty)
                .expect("empty searchable catalog")
                .deferred_len(),
            0
        );

        for count in [5, 32, 50, 128, 256, 1_000] {
            let catalog = ClaudeToolSearchCatalog::from_request(&request_with_matching_tools(
                "tool_search_tool_regex_20251119",
                count,
            ))
            .expect("catalog");
            assert_eq!(catalog.deferred_len(), count);
        }

        let catalog = ClaudeToolSearchCatalog::from_request(&request_with_matching_tools(
            "tool_search_tool_regex_20251119",
            10_000,
        ))
        .expect("10k catalog");
        let outcome = catalog.search_with_budget(
            &KiroToolUse {
                tool_use_id: "srvtoolu_large".into(),
                name: "tool_search_tool_regex".into(),
                input: serde_json::json!({"pattern":"SEARCHABLE", "limit":10_000}),
            },
            ClaudeToolSearchBudget {
                max_tools: 128,
                max_bytes: usize::MAX,
            },
        );
        assert_eq!(outcome.trace.references.len(), 128);
        assert!(outcome.truncated);
        assert_eq!(outcome.trace.requested_limit, 10_000);
        assert!(outcome.trace.error.is_none());
    }

    #[test]
    fn budget_that_cannot_fit_one_match_returns_an_explicit_search_error() {
        let catalog = ClaudeToolSearchCatalog::from_request(&request_with_matching_tools(
            "tool_search_tool_regex_20251119",
            1,
        ))
        .expect("catalog");
        let outcome = catalog.search_with_budget(
            &KiroToolUse {
                tool_use_id: "srvtoolu_small".into(),
                name: "tool_search_tool_regex".into(),
                input: serde_json::json!({"pattern":"searchable"}),
            },
            ClaudeToolSearchBudget {
                max_tools: 0,
                max_bytes: 0,
            },
        );
        assert!(outcome.truncated);
        assert_eq!(
            outcome
                .trace
                .error
                .as_ref()
                .map(|error| error.code.as_str()),
            Some("unavailable")
        );
    }

    #[test]
    fn aggregate_operation_limit_returns_an_in_band_error_without_results() {
        let catalog = ClaudeToolSearchCatalog::from_request(&request_with_matching_tools(
            "tool_search_tool_regex_20251119",
            10_000,
        ))
        .expect("catalog");
        let outcome = catalog.unavailable_outcome(
            &KiroToolUse {
                tool_use_id: "srvtoolu_limited".into(),
                name: "tool_search_tool_regex".into(),
                input: serde_json::json!({"pattern":"searchable","limit":10000}),
            },
            "operation limit reached",
        );
        assert!(outcome.tools.is_empty());
        assert!(outcome.trace.references.is_empty());
        assert_eq!(
            outcome
                .trace
                .error
                .as_ref()
                .map(|error| error.code.as_str()),
            Some("unavailable")
        );
    }

    #[test]
    fn bm25_uses_official_input_shape() {
        let catalog = ClaudeToolSearchCatalog::from_request(&request_with_matching_tools(
            "tool_search_tool_bm25_20251119",
            12,
        ))
        .expect("catalog");

        let default = catalog.search(&KiroToolUse {
            tool_use_id: "srvtoolu_default".into(),
            name: "tool_search_tool_bm25".into(),
            input: serde_json::json!({"query":"shared catalog search"}),
        });
        assert_eq!(default.trace.references.len(), DEFAULT_SEARCH_RESULTS);

        let expanded = catalog.search(&KiroToolUse {
            tool_use_id: "srvtoolu_expanded".into(),
            name: "tool_search_tool_bm25".into(),
            input: serde_json::json!({"query":"shared catalog search", "limit":8}),
        });
        assert_eq!(expanded.trace.references.len(), 8);
        assert_eq!(expanded.trace.requested_limit, 8);

        for unexpected in [
            serde_json::json!(0),
            serde_json::json!(10_001),
            serde_json::json!(2.5),
        ] {
            let invalid = catalog.search(&KiroToolUse {
                tool_use_id: "srvtoolu_invalid".into(),
                name: "tool_search_tool_bm25".into(),
                input: serde_json::json!({"query":"shared catalog search", "limit":unexpected}),
            });
            assert_eq!(
                invalid
                    .trace
                    .error
                    .as_ref()
                    .map(|error| error.code.as_str()),
                Some("invalid_tool_input")
            );
            assert!(invalid.trace.references.is_empty());
        }
    }

    #[test]
    fn loaded_matches_do_not_consume_the_requested_result_window() {
        let catalog = ClaudeToolSearchCatalog::from_request(&request_with_matching_tools(
            "tool_search_tool_regex_20251119",
            7,
        ))
        .expect("catalog");
        let loaded = (0..5)
            .map(|index| format!("mcp__catalog__searchable_{index}"))
            .collect::<HashSet<_>>();
        let outcome = catalog.search_with_budget_excluding(
            &KiroToolUse {
                tool_use_id: "srvtoolu_remaining".into(),
                name: "tool_search_tool_regex".into(),
                input: serde_json::json!({"pattern":"searchable", "limit":5}),
            },
            ClaudeToolSearchBudget::default(),
            &loaded,
        );

        assert_eq!(
            outcome.trace.references,
            ["mcp__catalog__searchable_5", "mcp__catalog__searchable_6"]
        );
    }

    #[test]
    fn taking_catalog_removes_deferred_request_copies() {
        let mut request = request("tool_search_tool_regex_20251119");
        let catalog = ClaudeToolSearchCatalog::take_from_request(&mut request).expect("catalog");
        assert_eq!(catalog.deferred_len(), 2);
        assert_eq!(request.tools.len(), 1);
        assert!(is_tool_search_tool(&request.tools[0]));
    }
}
