//! Cross-boundary HTTP, SQL, config, event-channel, and GraphQL contract extraction.
//!
//! Normalizes HTTP route templates (`/api/orders/:id` and `/api/orders/${id}` -> `/api/orders/{param}`),
//! SQL DDL vs DML table accesses, root GraphQL operations, event topics, and environment
//! variable readers (recording key names only, never secret values).

use ledger_core::{
    ContractEndpoint, ContractKind, FileBuffer, Language, LineSpan, LogicalSymbolKey, SymbolRecord,
    scope::sanitize_untrusted_text,
};
use regex::Regex;

/// Panic-free precompiled regex wrapper so contract extraction never invokes `unwrap` or `expect`.
#[derive(Debug)]
struct PatternMatcher(Option<Regex>);

impl PatternMatcher {
    fn new(pattern: &str) -> Self {
        Self(Regex::new(pattern).ok())
    }

    fn captures<'h>(&self, haystack: &'h str) -> Option<regex::Captures<'h>> {
        self.0.as_ref()?.captures(haystack)
    }

    fn captures_iter<'r, 'h>(
        &'r self,
        haystack: &'h str,
    ) -> Box<dyn Iterator<Item = regex::Captures<'h>> + 'r>
    where
        'h: 'r,
    {
        match &self.0 {
            Some(re) => Box::new(re.captures_iter(haystack)),
            None => Box::new(std::iter::empty()),
        }
    }
}

/// Stateless cross-boundary contract extractor with precompiled finite regexes.
#[derive(Debug)]
pub struct ContractExtractor {
    // HTTP server route handlers: `#[get("/api/...")]`, `path = "/api/..."`, `.route("/api/...", get(...))`, `@app.get("/api/...")`, `router.get('/api/...')`
    http_attr_route: PatternMatcher,
    http_method_route: PatternMatcher,
    http_utoipa_route: PatternMatcher,
    // HTTP client calls: `fetch("/api/...")`, `fetch(`/api/...`)`, `axios.get("/api/...")`, `client.post("/api/...")`
    http_fetch_call: PatternMatcher,
    http_fetch_method: PatternMatcher,
    http_axios_call: PatternMatcher,
    event_call: PatternMatcher,
    graphql_tag: PatternMatcher,
    graphql_root: PatternMatcher,
    graphql_annotation: PatternMatcher,
    graphql_function: PatternMatcher,
    // SQL DDL & DML
    sql_ddl_table: PatternMatcher,
    sql_dml_table: PatternMatcher,
    // Config / Env key readers (key names ONLY; values never captured!)
    env_reader_call: PatternMatcher,
    env_dot_prop: PatternMatcher,
    env_template_decl: PatternMatcher,
}

impl Default for ContractExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl ContractExtractor {
    /// Initializes precompiled contract patterns without panic paths.
    #[must_use]
    pub fn new() -> Self {
        let compile = PatternMatcher::new;

        Self {
            http_attr_route: compile(
                r#"(?i)(?:#\[|@(?:app|router)\.)(get|post|put|patch|delete)\s*\(\s*["'](/[^"']+)["']"#,
            ),
            http_method_route: compile(
                r#"(?i)\.\s*route\s*\(\s*["'](/[^"']+)["']\s*,\s*(get|post|put|patch|delete)?"#,
            ),
            http_utoipa_route: compile(
                r#"(?i)utoipa::path\s*\(\s*(get|post|put|patch|delete)\s*,\s*path\s*=\s*["'](/[^"']+)["']"#,
            ),
            http_fetch_call: compile(r"\bfetch\s*\("),
            http_fetch_method: compile(
                r#"(?i)method\s*:\s*["'](GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS)["']"#,
            ),
            http_axios_call: compile(
                r"(?i)\b(axios|api|http|client|requests|reqwest|router|app|server)\s*(?:\.|::)\s*(get|post|put|patch|delete|head|options)\s*(?:<[^>]*>)?\s*\(",
            ),
            event_call: compile(
                r"(?:\.\s*(emit|publish|dispatch|on|once|subscribe)|\b(dispatch_event|addEventListener)|\bevent_bus\s*\.\s*(send)|(?:#\[|@)\s*(on_event|subscribe))\s*\(",
            ),
            graphql_tag: compile(
                r"\b(?:gql|graphql|useQuery|useMutation|useSubscription)\b(?:\s*\()?",
            ),
            graphql_root: compile(r"\b(Query|Mutation|Subscription)\s*:\s*\{"),
            graphql_annotation: compile(r"(?:@(?:Query|Mutation|Subscription)\b|#\[graphql\b)"),
            graphql_function: compile(
                r"(?s)^[^;{}]*?(?:(?:async\s+)?(?:fn|def|function)\s+([A-Za-z_]\w*)|\b([A-Za-z_]\w*)\s*\()",
            ),
            sql_ddl_table: compile(
                r#"(?i)\b(?:CREATE|ALTER)\s+TABLE\s+(?:IF\s+(?:NOT\s+)?EXISTS\s+)?["`]?([a-zA-Z_][a-zA-Z0-9_]*)["`]?"#,
            ),
            sql_dml_table: compile(
                r#"(?i)\b(?:FROM|INTO|UPDATE|JOIN)\s+["`]?([a-zA-Z_][a-zA-Z0-9_]*)["`]?"#,
            ),
            env_reader_call: compile(
                r#"\b(?:env::var|env::var_os|os\.getenv|os\.Getenv|os\.environ\.get)\s*\(\s*["']([A-Z][A-Z0-9_]{2,})["']\s*\)"#,
            ),
            env_dot_prop: compile(
                r#"\b(?:process\.env|import\.meta\.env|os\.environ)\s*(?:\.\s*([A-Z][A-Z0-9_]{2,})|\[\s*["']([A-Z][A-Z0-9_]{2,})["']\s*\])"#,
            ),
            env_template_decl: compile(r#"^\s*([A-Z][A-Z0-9_]{2,})\s*="#),
        }
    }

    /// Normalizes route parameters to `{param}` and drops dynamic origins and query strings.
    /// Declared `:id`/`{id}` parameters are static; runtime `${id}` interpolation is dynamic.
    #[must_use]
    pub fn normalize_http_path(raw_path: &str) -> (String, bool) {
        let mut path = raw_path.trim();
        let mut is_dynamic = path.contains("${");
        // A dynamic origin contributes no route segment: `${API_BASE}/api/...`.
        if path.starts_with("${")
            && let Some(end) = path.find('}')
            && path[end + 1..].starts_with('/')
        {
            path = &path[end + 1..];
        }
        if let Some((_, rest)) = path.split_once("://") {
            path = rest.find('/').map_or("/", |index| &rest[index..]);
        }
        let without_query = path.split(['?', '#']).next().unwrap_or(path);
        let mut norm_segments = Vec::new();
        for segment in without_query
            .split('/')
            .filter(|segment| !segment.is_empty())
        {
            if segment.starts_with(':') || segment.contains('{') || segment.contains('}') {
                norm_segments.push("{param}");
                is_dynamic |= segment.contains("${");
            } else {
                norm_segments.push(segment);
            }
        }
        (format!("/{}", norm_segments.join("/")), is_dynamic)
    }

    fn enclosing_symbol_for_line(symbols: &[SymbolRecord], line: u32) -> Option<LogicalSymbolKey> {
        symbols
            .iter()
            .filter(|s| line >= s.line_span.start_line && line <= s.line_span.end_line)
            .min_by_key(|s| s.line_span.line_count())
            .map(|s| s.logical_key.clone())
    }

    /// Scans `buffer` line-by-line to extract namespaced [`ContractEndpoint`]s.
    #[must_use]
    pub fn extract_contracts(
        &self,
        buffer: &FileBuffer,
        language: Language,
        symbols: &[SymbolRecord],
    ) -> Vec<ContractEndpoint> {
        let mut out = Vec::new();
        let rel_path = buffer.rel_path();
        let is_env_template = rel_path
            .rsplit('/')
            .next()
            .is_some_and(|n| n.starts_with(".env."));

        for (idx, line_str) in buffer.as_str().lines().enumerate() {
            let line_no = u32::try_from(idx + 1).unwrap_or(1);
            let span = LineSpan::new(line_no, line_no);
            let enclosing = Self::enclosing_symbol_for_line(symbols, line_no);

            if is_env_template {
                if let Some(cap) = self.env_template_decl.captures(line_str)
                    && let Some(key) = cap.get(1)
                {
                    let key_name = key.as_str();
                    out.push(ContractEndpoint {
                        kind: ContractKind::ConfigEnvKey,
                        normalized_key: format!("env:{key_name}"),
                        role: "provider".to_string(),
                        rel_path: rel_path.to_string(),
                        enclosing_symbol: None,
                        line_span: span,
                        // Never copy values even from `.env.example`!
                        snippet: format!("{key_name}=<redacted_template_value>"),
                        is_dynamic: false,
                    });
                }
                continue;
            }

            let safe_snippet = sanitize_untrusted_text(line_str.trim(), 140);

            // 1. HTTP Provider Routes
            for cap in self.http_attr_route.captures_iter(line_str) {
                if let (Some(method), Some(path)) = (cap.get(1), cap.get(2)) {
                    let (norm, is_dyn) = Self::normalize_http_path(path.as_str());
                    out.push(ContractEndpoint {
                        kind: ContractKind::HttpEndpoint,
                        normalized_key: format!(
                            "{} {}",
                            method.as_str().to_ascii_uppercase(),
                            norm
                        ),
                        role: "provider".to_string(),
                        rel_path: rel_path.to_string(),
                        enclosing_symbol: enclosing.clone(),
                        line_span: span,
                        snippet: safe_snippet.clone(),
                        is_dynamic: is_dyn,
                    });
                }
            }
            for cap in self.http_utoipa_route.captures_iter(line_str) {
                if let (Some(method), Some(path)) = (cap.get(1), cap.get(2)) {
                    let (norm, is_dyn) = Self::normalize_http_path(path.as_str());
                    out.push(ContractEndpoint {
                        kind: ContractKind::HttpEndpoint,
                        normalized_key: format!(
                            "{} {}",
                            method.as_str().to_ascii_uppercase(),
                            norm
                        ),
                        role: "provider".to_string(),
                        rel_path: rel_path.to_string(),
                        enclosing_symbol: enclosing.clone(),
                        line_span: span,
                        snippet: safe_snippet.clone(),
                        is_dynamic: is_dyn,
                    });
                }
            }
            for cap in self.http_method_route.captures_iter(line_str) {
                if let Some(path) = cap.get(1) {
                    let method = cap
                        .get(2)
                        .map_or("ANY", |m| m.as_str())
                        .to_ascii_uppercase();
                    let (norm, is_dyn) = Self::normalize_http_path(path.as_str());
                    out.push(ContractEndpoint {
                        kind: ContractKind::HttpEndpoint,
                        normalized_key: format!("{method} {norm}"),
                        role: "provider".to_string(),
                        rel_path: rel_path.to_string(),
                        enclosing_symbol: enclosing.clone(),
                        line_span: span,
                        snippet: safe_snippet.clone(),
                        is_dynamic: is_dyn,
                    });
                }
            }

            // 3. SQL Schema & Query Contracts
            for cap in self.sql_ddl_table.captures_iter(line_str) {
                if let Some(tbl) = cap.get(1) {
                    let t = tbl.as_str().to_ascii_lowercase();
                    if !is_sql_noise_word(&t) {
                        out.push(ContractEndpoint {
                            kind: ContractKind::SqlTableAccess,
                            normalized_key: format!("sql:table:{t}"),
                            role: "provider".to_string(),
                            rel_path: rel_path.to_string(),
                            enclosing_symbol: enclosing.clone(),
                            line_span: span,
                            snippet: safe_snippet.clone(),
                            is_dynamic: false,
                        });
                    }
                }
            }
            if language == Language::Sql
                || line_str.contains("SELECT ")
                || line_str.contains("INSERT INTO ")
                || line_str.contains("UPDATE ")
                || line_str.contains("sqlx::query")
            {
                for cap in self.sql_dml_table.captures_iter(line_str) {
                    if let Some(tbl) = cap.get(1) {
                        let t = tbl.as_str().to_ascii_lowercase();
                        if !is_sql_noise_word(&t) {
                            out.push(ContractEndpoint {
                                kind: ContractKind::SqlTableAccess,
                                normalized_key: format!("sql:table:{t}"),
                                role: "consumer".to_string(),
                                rel_path: rel_path.to_string(),
                                enclosing_symbol: enclosing.clone(),
                                line_span: span,
                                snippet: safe_snippet.clone(),
                                is_dynamic: false,
                            });
                        }
                    }
                }
            }

            // 4. Environment Variable / Config Key Readers
            for cap in self.env_reader_call.captures_iter(line_str) {
                if let Some(key) = cap.get(1) {
                    out.push(ContractEndpoint {
                        kind: ContractKind::ConfigEnvKey,
                        normalized_key: format!("env:{}", key.as_str()),
                        role: "consumer".to_string(),
                        rel_path: rel_path.to_string(),
                        enclosing_symbol: enclosing.clone(),
                        line_span: span,
                        snippet: safe_snippet.clone(),
                        is_dynamic: false,
                    });
                }
            }
            for cap in self.env_dot_prop.captures_iter(line_str) {
                if let Some(key) = cap.get(1).or_else(|| cap.get(2)) {
                    out.push(ContractEndpoint {
                        kind: ContractKind::ConfigEnvKey,
                        normalized_key: format!("env:{}", key.as_str()),
                        role: "consumer".to_string(),
                        rel_path: rel_path.to_string(),
                        enclosing_symbol: enclosing.clone(),
                        line_span: span,
                        snippet: safe_snippet.clone(),
                        is_dynamic: false,
                    });
                }
            }
        }

        if !is_env_template {
            self.extract_call_contracts(buffer, symbols, &mut out);
            self.extract_graphql_contracts(buffer, language, symbols, &mut out);
        }
        let mut seen = std::collections::HashSet::new();
        out.retain(|endpoint| {
            seen.insert((
                endpoint.kind,
                endpoint.role.clone(),
                endpoint.normalized_key.clone(),
                endpoint.line_span.start_line,
            ))
        });
        out
    }
}

impl ContractExtractor {
    fn extract_call_contracts(
        &self,
        buffer: &FileBuffer,
        symbols: &[SymbolRecord],
        out: &mut Vec<ContractEndpoint>,
    ) {
        let source = buffer.as_str();
        let code = mask_code(source);
        for cap in self.http_fetch_call.captures_iter(&code) {
            if let Some(call) = cap.get(0)
                && let Some((path, dynamic, end)) = http_argument(source, call.end())
            {
                let options = &source[end..];
                let options = options.split(')').next().unwrap_or(options);
                let method = self
                    .http_fetch_method
                    .captures(options)
                    .and_then(|cap| cap.get(1).map(|m| m.as_str().to_ascii_uppercase()))
                    .unwrap_or_else(|| "GET".to_string());
                push_endpoint(
                    buffer,
                    symbols,
                    out,
                    EndpointSpec {
                        kind: ContractKind::HttpEndpoint,
                        key: format!("{method} {path}"),
                        role: "consumer",
                        offset: call.start(),
                        dynamic,
                    },
                );
            }
        }
        for cap in self.http_axios_call.captures_iter(&code) {
            if let (Some(call), Some(receiver), Some(method)) = (cap.get(0), cap.get(1), cap.get(2))
                && let Some((path, dynamic, _)) = http_argument(source, call.end())
            {
                let role = if matches!(receiver.as_str(), "router" | "app" | "server") {
                    "provider"
                } else {
                    "consumer"
                };
                push_endpoint(
                    buffer,
                    symbols,
                    out,
                    EndpointSpec {
                        kind: ContractKind::HttpEndpoint,
                        key: format!("{} {path}", method.as_str().to_ascii_uppercase()),
                        role,
                        offset: call.start(),
                        dynamic,
                    },
                );
            }
        }
        for cap in self.event_call.captures_iter(&code) {
            let Some(call) = cap.get(0) else { continue };
            let Some(literal) = read_literal(source, call.end()) else {
                continue;
            };
            let (topic, mut dynamic) = normalize_topic(literal.text);
            dynamic |= literal.dynamic || source[literal.end..].trim_start().starts_with('+');
            if topic.is_empty()
                || is_event_noise(&topic)
                || !topic.chars().any(char::is_alphanumeric)
            {
                continue;
            }
            let operation = (1..=4)
                .find_map(|index| cap.get(index))
                .map_or("", |m| m.as_str());
            let role = if matches!(
                operation,
                "emit" | "publish" | "dispatch" | "dispatch_event" | "send"
            ) {
                "provider"
            } else {
                "consumer"
            };
            push_endpoint(
                buffer,
                symbols,
                out,
                EndpointSpec {
                    kind: ContractKind::EventChannel,
                    key: format!("EVENT {topic}"),
                    role,
                    offset: call.start(),
                    dynamic,
                },
            );
        }
    }

    fn extract_graphql_contracts(
        &self,
        buffer: &FileBuffer,
        language: Language,
        symbols: &[SymbolRecord],
        out: &mut Vec<ContractEndpoint>,
    ) {
        let source = buffer.as_str();
        if language == Language::Graphql {
            extract_graphql_document(buffer, symbols, source, 0, out);
            return;
        }
        let code = mask_code(source);
        let mut documents = std::collections::HashSet::new();
        // Only explicit GraphQL APIs permit shorthand query documents.
        for cap in self.graphql_tag.captures_iter(&code) {
            if let Some(call) = cap.get(0)
                && let Some(literal) = read_literal(source, call.end())
                && documents.insert(literal.start)
            {
                extract_graphql_document(buffer, symbols, literal.text, literal.start, out);
            }
        }
        // SDL can be embedded in ordinary raw/triple-quoted strings as well as gql tags.
        for literal in source_literals(source) {
            let tokens = graphql_tokens(literal.text);
            if tokens
                .windows(2)
                .any(|pair| pair[0].text == "type" && root_operation(pair[1].text).is_some())
                && documents.insert(literal.start)
            {
                extract_graphql_document(buffer, symbols, literal.text, literal.start, out);
            }
        }
        if matches!(language, Language::TypeScript | Language::JavaScript) {
            for cap in self.graphql_root.captures_iter(&code) {
                if let (Some(block), Some(root)) = (cap.get(0), cap.get(1)) {
                    extract_resolver_map(buffer, symbols, block.end() - 1, root.as_str(), out);
                }
            }
        }
        for cap in self.graphql_annotation.captures_iter(&code) {
            let Some(annotation) = cap.get(0) else {
                continue;
            };
            let declaration = &code[annotation.end()..];
            let Some(function) = self.graphql_function.captures(declaration) else {
                continue;
            };
            let Some(name) = function.get(1).or_else(|| function.get(2)) else {
                continue;
            };
            let operation = if annotation.as_str().contains("Mutation") {
                "MUTATION"
            } else if annotation.as_str().contains("Subscription") {
                "SUBSCRIPTION"
            } else {
                // Rust's #[graphql] methods inherit their enclosing impl's root type.
                let prefix = &code[..annotation.start()];
                let context = prefix.rsplit("impl ").next().unwrap_or("");
                if context.starts_with("Mutation") {
                    "MUTATION"
                } else if context.starts_with("Subscription") {
                    "SUBSCRIPTION"
                } else {
                    "QUERY"
                }
            };
            push_endpoint(
                buffer,
                symbols,
                out,
                EndpointSpec {
                    kind: ContractKind::GraphqlSchemaOp,
                    key: format!("GQL {operation}.{}", name.as_str()),
                    role: "provider",
                    offset: annotation.end() + name.start(),
                    dynamic: false,
                },
            );
        }
    }
}

#[derive(Debug)]
struct EndpointSpec<'a> {
    kind: ContractKind,
    key: String,
    role: &'a str,
    offset: usize,
    dynamic: bool,
}

fn push_endpoint(
    buffer: &FileBuffer,
    symbols: &[SymbolRecord],
    out: &mut Vec<ContractEndpoint>,
    spec: EndpointSpec<'_>,
) {
    let source = buffer.as_str();
    let offset = spec.offset.min(source.len());
    let line = u32::try_from(
        source[..offset]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count()
            + 1,
    )
    .unwrap_or(u32::MAX);
    let start = source[..offset].rfind('\n').map_or(0, |index| index + 1);
    let end = source[offset..]
        .find('\n')
        .map_or(source.len(), |index| offset + index);
    out.push(ContractEndpoint {
        kind: spec.kind,
        normalized_key: spec.key,
        role: spec.role.to_string(),
        rel_path: buffer.rel_path().to_string(),
        enclosing_symbol: ContractExtractor::enclosing_symbol_for_line(symbols, line),
        line_span: LineSpan::new(line, line),
        snippet: sanitize_untrusted_text(source[start..end].trim(), 140),
        is_dynamic: spec.dynamic,
    });
}

#[derive(Debug)]
struct SourceLiteral<'a> {
    text: &'a str,
    start: usize,
    end: usize,
    dynamic: bool,
}

/// Read a literal without treating nested interpolation as separate source code.
fn read_literal(source: &str, offset: usize) -> Option<SourceLiteral<'_>> {
    let mut start = offset;
    let bytes = source.as_bytes();
    while bytes.get(start).is_some_and(u8::is_ascii_whitespace) {
        start += 1;
    }
    let mut dynamic = false;
    if matches!(bytes.get(start), Some(b'f' | b'F')) {
        dynamic = true;
        start += 1;
    }
    let mut hashes = 0;
    if bytes.get(start) == Some(&b'r') {
        start += 1;
        while bytes.get(start) == Some(&b'#') {
            hashes += 1;
            start += 1;
        }
    }
    let quote = *bytes.get(start)?;
    if !matches!(quote, b'\'' | b'"' | b'`') {
        return None;
    }
    let triple = bytes
        .get(start..start + 3)
        .is_some_and(|s| s == [quote, quote, quote]);
    let width = if triple { 3 } else { 1 };
    let content_start = start + width;
    let mut index = content_start;
    while index < bytes.len() {
        if bytes[index] == b'\n' && quote == b'\'' && !triple {
            return None;
        }
        if bytes[index] == b'\\' && hashes == 0 {
            index += 2;
            continue;
        }
        if bytes
            .get(index..index + width)
            .is_some_and(|s| s.iter().all(|byte| *byte == quote))
            && bytes
                .get(index + width..index + width + hashes)
                .is_some_and(|s| s.iter().all(|byte| *byte == b'#'))
        {
            let text = &source[content_start..index];
            return Some(SourceLiteral {
                text,
                start: content_start,
                end: index + width + hashes,
                dynamic: (dynamic && text.contains('{')) || text.contains("${"),
            });
        }
        index += 1;
    }
    None
}

fn comment_end(source: &str, index: usize) -> Option<usize> {
    let rest = &source[index..];
    if rest.starts_with("//") || (rest.starts_with('#') && !rest.starts_with("#[")) {
        Some(index + rest.find('\n').unwrap_or(rest.len()))
    } else if rest.starts_with("/*") {
        Some(index + rest.find("*/").map_or(rest.len(), |end| end + 2))
    } else {
        None
    }
}

fn source_literals(source: &str) -> Vec<SourceLiteral<'_>> {
    let mut literals = Vec::new();
    let mut index = 0;
    while index < source.len() {
        if let Some(end) = comment_end(source, index) {
            index = end;
        } else if let Some(literal) = read_literal(source, index) {
            index = literal.end;
            literals.push(literal);
        } else {
            index += source[index..].chars().next().map_or(1, char::len_utf8);
        }
    }
    literals
}

/// Preserve byte offsets while removing literals and comments from call-site matching.
fn mask_code(source: &str) -> String {
    let mut masked = source.as_bytes().to_vec();
    let mut index = 0;
    while index < source.len() {
        let end = comment_end(source, index)
            .or_else(|| read_literal(source, index).map(|literal| literal.end));
        if let Some(end) = end {
            for byte in &mut masked[index..end] {
                if *byte != b'\n' {
                    *byte = b' ';
                }
            }
            index = end;
        } else {
            index += source[index..].chars().next().map_or(1, char::len_utf8);
        }
    }
    String::from_utf8(masked).unwrap_or_default()
}

fn http_argument(source: &str, offset: usize) -> Option<(String, bool, usize)> {
    let rest = source[offset..].trim_start();
    let offset = source.len() - rest.len();
    let format_prefix = rest
        .strip_prefix("format!")
        .and_then(|rest| rest.trim_start().strip_prefix('('));
    let literal_offset = format_prefix.map_or(offset, |rest| source.len() - rest.len());
    let literal = read_literal(source, literal_offset)?;
    if !literal.text.starts_with('/')
        && !literal.text.starts_with("${")
        && !literal.text.contains("://")
    {
        return None;
    }
    let (mut path, dynamic) = ContractExtractor::normalize_http_path(literal.text);
    let concatenated = source[literal.end..].trim_start().starts_with('+');
    if concatenated && !literal.text.contains(['?', '#']) {
        path.push_str("/{param}");
    }
    Some((
        path,
        dynamic
            || literal.dynamic
            || (format_prefix.is_some() && literal.text.contains('{'))
            || concatenated,
        literal.end,
    ))
}

fn normalize_topic(topic: &str) -> (String, bool) {
    let mut normalized = String::new();
    let mut remaining = topic;
    let mut dynamic = false;
    while let Some(start) = remaining.find("${") {
        normalized.push_str(&remaining[..start]);
        let Some(end) = remaining[start..].find('}') else {
            break;
        };
        normalized.push_str("{param}");
        remaining = &remaining[start + end + 1..];
        dynamic = true;
    }
    normalized.push_str(remaining);
    dynamic |= topic.ends_with([':', '.', '/', '-', '_']);
    (normalized, dynamic)
}

fn is_event_noise(topic: &str) -> bool {
    if topic.contains([':', '.', '/', '-', '_']) {
        return false;
    }
    matches!(
        topic.to_ascii_lowercase().as_str(),
        "click"
            | "change"
            | "input"
            | "submit"
            | "focus"
            | "blur"
            | "keydown"
            | "keyup"
            | "mousedown"
            | "mouseup"
            | "load"
            | "unload"
            | "resize"
            | "scroll"
            | "error"
            | "data"
            | "end"
            | "close"
            | "open"
            | "connect"
            | "disconnect"
            | "message"
            | "ready"
    )
}

#[derive(Debug, Clone, Copy)]
struct GraphqlToken<'a> {
    text: &'a str,
    offset: usize,
}

/// Small GraphQL lexer: descriptions, strings, and comments cannot become fields.
fn graphql_tokens(document: &str) -> Vec<GraphqlToken<'_>> {
    let mut tokens = Vec::new();
    let bytes = document.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index].is_ascii_whitespace() {
            index += 1;
        } else if bytes[index] == b'#' || document[index..].starts_with("//") {
            index += document[index..]
                .find('\n')
                .unwrap_or(document.len() - index);
        } else if matches!(bytes[index], b'"' | b'\'' | b'`') {
            index = read_literal(document, index).map_or(document.len(), |literal| literal.end);
        } else {
            let start = index;
            if bytes[index].is_ascii_alphabetic() || bytes[index] == b'_' {
                index += 1;
                while bytes
                    .get(index)
                    .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
                {
                    index += 1;
                }
            } else {
                index += document[index..].chars().next().map_or(1, char::len_utf8);
            }
            tokens.push(GraphqlToken {
                text: &document[start..index],
                offset: start,
            });
        }
    }
    tokens
}

fn token_text<'a>(tokens: &[GraphqlToken<'a>], index: usize) -> &'a str {
    tokens.get(index).map_or("", |token| token.text)
}

fn root_operation(root: &str) -> Option<&'static str> {
    match root {
        "Query" | "query" => Some("QUERY"),
        "Mutation" | "mutation" => Some("MUTATION"),
        "Subscription" | "subscription" => Some("SUBSCRIPTION"),
        _ => None,
    }
}

fn is_graphql_name(name: &str) -> bool {
    name.as_bytes()
        .first()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
}

/// Skip a balanced group, including nested object-valued arguments and selections.
fn skip_group(tokens: &[GraphqlToken<'_>], start: usize) -> usize {
    let (open, close) = match token_text(tokens, start) {
        "(" => ("(", ")"),
        "{" => ("{", "}"),
        "[" => ("[", "]"),
        _ => return start + 1,
    };
    let mut depth = 0_usize;
    for (index, token) in tokens.iter().enumerate().skip(start) {
        if token.text == open {
            depth += 1;
        } else if token.text == close {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return index + 1;
            }
        }
    }
    tokens.len()
}

fn skip_directives(tokens: &[GraphqlToken<'_>], mut index: usize) -> usize {
    while token_text(tokens, index) == "@" {
        index += 2;
        if token_text(tokens, index) == "(" {
            index = skip_group(tokens, index);
        }
    }
    index
}

fn selection_start(tokens: &[GraphqlToken<'_>], mut index: usize) -> Option<usize> {
    while index < tokens.len() {
        match token_text(tokens, index) {
            "{" => return Some(index),
            "(" | "[" => index = skip_group(tokens, index),
            "}" => return None,
            _ => index += 1,
        }
    }
    None
}

fn extract_graphql_document(
    buffer: &FileBuffer,
    symbols: &[SymbolRecord],
    document: &str,
    base: usize,
    out: &mut Vec<ContractEndpoint>,
) {
    let tokens = graphql_tokens(document);
    let mut index = 0;
    while index < tokens.len() {
        let current = token_text(&tokens, index);
        if current == "type" {
            if let Some(operation) = root_operation(token_text(&tokens, index + 1))
                && let Some(start) = selection_start(&tokens, index + 2)
            {
                extract_graphql_fields(buffer, symbols, &tokens, start, base, operation, true, out);
                index = skip_group(&tokens, start);
            } else if let Some(start) = selection_start(&tokens, index + 2) {
                index = skip_group(&tokens, start);
            } else {
                break;
            }
        } else if matches!(current, "query" | "mutation" | "subscription") {
            let Some(start) = selection_start(&tokens, index + 1) else {
                break;
            };
            if let Some(operation) = root_operation(current) {
                extract_graphql_fields(
                    buffer, symbols, &tokens, start, base, operation, false, out,
                );
            }
            index = skip_group(&tokens, start);
        } else if current == "{" {
            extract_graphql_fields(buffer, symbols, &tokens, index, base, "QUERY", false, out);
            index = skip_group(&tokens, index);
        } else if current == "directive" {
            index += 1;
            if token_text(&tokens, index) == "@" {
                index += 2;
            }
            if token_text(&tokens, index) == "(" {
                index = skip_group(&tokens, index);
            }
            if token_text(&tokens, index) == "repeatable" {
                index += 1;
            }
            if token_text(&tokens, index) == "on" {
                index += 1;
            }
            while token_text(&tokens, index) == "|"
                || token_text(&tokens, index)
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_uppercase)
            {
                index += 1;
            }
        } else if matches!(
            current,
            "fragment" | "enum" | "input" | "interface" | "schema"
        ) {
            // Fragment definitions and non-root SDL never supply operation fields.
            let Some(start) = selection_start(&tokens, index + 1) else {
                break;
            };
            index = skip_group(&tokens, start);
        } else {
            index += 1;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn extract_graphql_fields(
    buffer: &FileBuffer,
    symbols: &[SymbolRecord],
    tokens: &[GraphqlToken<'_>],
    start: usize,
    base: usize,
    operation: &str,
    schema: bool,
    out: &mut Vec<ContractEndpoint>,
) {
    let mut index = start + 1;
    while index < tokens.len() && token_text(tokens, index) != "}" {
        if token_text(tokens, index) == "." {
            // Spreads need fragment expansion to be evidence; skip them conservatively.
            index += 3;
            if token_text(tokens, index) == "on" {
                index += 2;
            } else if is_graphql_name(token_text(tokens, index)) {
                index += 1;
            }
            index = skip_directives(tokens, index);
            if token_text(tokens, index) == "{" {
                index = skip_group(tokens, index);
            }
            continue;
        }
        if !is_graphql_name(token_text(tokens, index)) {
            index += 1;
            continue;
        }
        let mut field = index;
        index += 1;
        if !schema && token_text(tokens, index) == ":" {
            field = index + 1; // Use the real field behind an alias.
            index += 2;
        }
        if token_text(tokens, index) == "(" {
            index = skip_group(tokens, index);
        }
        if schema {
            if token_text(tokens, index) != ":" {
                continue;
            }
            index += 1;
            while token_text(tokens, index) == "[" {
                index += 1;
            }
            if is_graphql_name(token_text(tokens, index)) {
                index += 1;
            }
            while matches!(token_text(tokens, index), "]" | "!") {
                index += 1;
            }
        }
        let name = token_text(tokens, field);
        if name != "__typename" && is_graphql_name(name) {
            push_endpoint(
                buffer,
                symbols,
                out,
                EndpointSpec {
                    kind: ContractKind::GraphqlSchemaOp,
                    key: format!("GQL {operation}.{name}"),
                    role: if schema { "provider" } else { "consumer" },
                    offset: base + tokens[field].offset,
                    dynamic: false,
                },
            );
        }
        index = skip_directives(tokens, index);
        if token_text(tokens, index) == "{" {
            index = skip_group(tokens, index);
        }
    }
}

fn extract_resolver_map(
    buffer: &FileBuffer,
    symbols: &[SymbolRecord],
    start: usize,
    root: &str,
    out: &mut Vec<ContractEndpoint>,
) {
    let Some(operation) = root_operation(root) else {
        return;
    };
    let tokens = graphql_tokens(&buffer.as_str()[start..]);
    let mut index = 1;
    while index < tokens.len() && token_text(&tokens, index) != "}" {
        if token_text(&tokens, index) == "async" {
            index += 1;
        }
        let name = token_text(&tokens, index);
        if is_graphql_name(name) && matches!(token_text(&tokens, index + 1), "(" | ":") {
            push_endpoint(
                buffer,
                symbols,
                out,
                EndpointSpec {
                    kind: ContractKind::GraphqlSchemaOp,
                    key: format!("GQL {operation}.{name}"),
                    role: "provider",
                    offset: start + tokens[index].offset,
                    dynamic: false,
                },
            );
        }
        index += 1;
        // Skip entire method/property values so nested return objects cannot be resolvers.
        while index < tokens.len() && !matches!(token_text(&tokens, index), "," | "}") {
            if matches!(token_text(&tokens, index), "(" | "{" | "[") {
                index = skip_group(&tokens, index);
            } else {
                index += 1;
            }
        }
        if token_text(&tokens, index) == "," {
            index += 1;
        }
    }
}

fn is_sql_noise_word(word: &str) -> bool {
    matches!(
        word,
        "select"
            | "where"
            | "set"
            | "values"
            | "null"
            | "stdin"
            | "stdout"
            | "each"
            | "only"
            | "dual"
    )
}
