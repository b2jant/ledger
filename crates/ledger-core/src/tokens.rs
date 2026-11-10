//! Pure identifier splitting, architectural layer inference, and query term extraction.

use crate::model::{ArchitecturalLayer, Language};

/// Splits `camelCase`, `PascalCase`, `snake_case`, `SCREAMING_SNAKE`, and `kebab-case`
/// identifiers into normalized lowercase semantic tokens.
///
/// For example, `"applyOrderFilterPredicates"` yields
/// `["apply", "order", "filter", "predicates"]`.
#[must_use]
pub fn split_identifier_tokens(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = input.chars().collect();

    for i in 0..chars.len() {
        let ch = chars[i];
        if !ch.is_alphanumeric() {
            if current.len() >= 2 {
                tokens.push(current.to_ascii_lowercase());
            }
            current.clear();
            continue;
        }

        let prev = if i > 0 { Some(chars[i - 1]) } else { None };
        let next = chars.get(i + 1).copied();

        let boundary = match (prev, ch.is_uppercase(), next) {
            (Some(p), true, _) if p.is_lowercase() || p.is_ascii_digit() => true,
            (Some(p), true, Some(n)) if p.is_uppercase() && n.is_lowercase() => true,
            _ => false,
        };

        if boundary && current.len() >= 2 {
            tokens.push(current.to_ascii_lowercase());
            current.clear();
        }
        current.push(ch);
    }

    if current.len() >= 2 {
        tokens.push(current.to_ascii_lowercase());
    }

    tokens.sort();
    tokens.dedup();
    tokens
}

/// Extracts meaningful search keywords from a natural-language task description,
/// filtering common English stop-words while preserving domain concepts like `"backend"`,
/// `"filtering"`, `"orders"`, `"cursor"`.
#[must_use]
pub fn extract_query_terms(task: &str) -> Vec<String> {
    const STOP_WORDS: &[&str] = &[
        "the", "and", "for", "with", "that", "this", "from", "into", "when", "where", "what",
        "which", "how", "why", "can", "could", "should", "would", "will", "make", "made", "need",
        "needs", "want", "please", "update", "change", "modify", "fix", "add", "create", "remove",
        "use", "using", "used", "only", "also", "unless", "true", "false", "null", "none", "all",
        "any", "some", "have", "has", "had", "are", "was", "were", "not", "but", "does", "doesn",
    ];

    let mut out = Vec::new();
    for raw_word in task.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-') {
        if raw_word.len() < 2 {
            continue;
        }
        let lower = raw_word.to_ascii_lowercase();
        if !STOP_WORDS.contains(&lower.as_str()) {
            out.push(lower.clone());
        }
        for sub in split_identifier_tokens(raw_word) {
            if sub.len() >= 2 && !STOP_WORDS.contains(&sub.as_str()) {
                out.push(sub);
            }
        }
        // Lightweight suffix stemming so "filtering" <-> "filter", "orders" <-> "order", "archived" <-> "archive".
        for stem in light_stems(&lower) {
            if stem.len() >= 3 && !STOP_WORDS.contains(&stem.as_str()) {
                out.push(stem);
            }
        }
    }

    let mut deduped = Vec::with_capacity(out.len());
    for term in out {
        if !deduped.contains(&term) {
            deduped.push(term);
        }
    }
    deduped
}

/// Deterministic lightweight suffix stemmer so morphological variants (`"filtering"` -> `"filter"`,
/// `"archived"` -> `"archiv"` / `"archive"`) match without requiring heavy NLP models.
#[must_use]
pub fn light_stems(word: &str) -> Vec<String> {
    let mut stems = Vec::new();
    if let Some(base) = word.strip_suffix("ing")
        && base.len() >= 3
    {
        stems.push(base.to_string());
        stems.push(format!("{base}e"));
    }
    if let Some(base) = word.strip_suffix("ed")
        && base.len() >= 3
    {
        stems.push(base.to_string());
        stems.push(format!("{base}e"));
    }
    if let Some(base) = word.strip_suffix("es")
        && base.len() >= 3
    {
        stems.push(base.to_string());
    } else if let Some(base) = word.strip_suffix('s')
        && base.len() >= 3
    {
        stems.push(base.to_string());
    }
    stems
}

/// Infers the [`ArchitecturalLayer`] of a file from its workspace-relative path and [`Language`].
#[must_use]
pub fn infer_architectural_layer(rel_path: &str, language: Language) -> ArchitecturalLayer {
    let lower = rel_path.to_ascii_lowercase();
    let segments: Vec<&str> = lower.split('/').collect();

    if language == Language::Markdown
        || segments
            .iter()
            .any(|s| matches!(*s, "docs" | "doc" | "adr" | "adrs"))
    {
        return ArchitecturalLayer::Docs;
    }

    if segments
        .iter()
        .any(|s| matches!(*s, "tests" | "test" | "__tests__" | "e2e" | "benches"))
        || lower.ends_with("_test.go")
        || lower.ends_with("_test.rs")
        || lower.ends_with(".test.ts")
        || lower.ends_with(".spec.ts")
        || lower.ends_with(".test.tsx")
        || lower
            .rsplit('/')
            .next()
            .is_some_and(|f| f.starts_with("test_"))
    {
        return ArchitecturalLayer::Tests;
    }

    if language == Language::Sql
        || segments.iter().any(|s| {
            matches!(
                *s,
                "migrations" | "migration" | "schema" | "sql" | "db" | "prisma" | "queries"
            )
        })
    {
        return ArchitecturalLayer::Database;
    }

    if segments.iter().any(|s| {
        matches!(
            *s,
            ".github" | "infra" | "deploy" | "k8s" | "terraform" | "docker" | "scripts" | "ci"
        )
    }) || lower.ends_with("dockerfile")
    {
        return ArchitecturalLayer::Infra;
    }

    if lower.ends_with(".tsx")
        || lower.ends_with(".jsx")
        || segments.iter().any(|s| {
            matches!(
                *s,
                "web"
                    | "ui"
                    | "frontend"
                    | "client"
                    | "components"
                    | "pages"
                    | "views"
                    | "hooks"
                    | "app"
            )
        })
    {
        // Disambiguate `app` in Rust/Python/Go backend repos vs JS/TS frontend repos.
        if matches!(language, Language::TypeScript | Language::JavaScript)
            || segments
                .iter()
                .any(|s| matches!(*s, "web" | "ui" | "frontend" | "components" | "hooks"))
        {
            return ArchitecturalLayer::Frontend;
        }
    }

    if matches!(language, Language::Rust | Language::Go | Language::Python)
        || segments.iter().any(|s| {
            matches!(
                *s,
                "api"
                    | "server"
                    | "backend"
                    | "handlers"
                    | "controllers"
                    | "services"
                    | "routes"
                    | "middleware"
                    | "domain"
                    | "workers"
            )
        })
    {
        return ArchitecturalLayer::Backend;
    }

    ArchitecturalLayer::Shared
}

/// Detects whether a natural-language task mentions a specific [`ArchitecturalLayer`].
#[must_use]
pub fn detect_requested_layer(task: &str) -> Option<ArchitecturalLayer> {
    let lower = task.to_ascii_lowercase();
    for word in lower.split(|c: char| !c.is_alphanumeric()) {
        match word {
            "backend" | "server" | "endpoint" | "handler" | "middleware" => {
                return Some(ArchitecturalLayer::Backend);
            }
            "frontend" | "ui" | "component" | "react" | "hook" | "browser" => {
                return Some(ArchitecturalLayer::Frontend);
            }
            "migration" | "sql" | "schema" | "postgres" | "sqlite" => {
                return Some(ArchitecturalLayer::Database);
            }
            _ => {}
        }
    }
    None
}
