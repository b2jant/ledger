//! Composable per-language Tree-sitter AST extractors (`Rust`, `TypeScript`/`JavaScript`,
//! `Python`, `Go`) with graceful syntax-error recovery and fallback coverage tracking.

use std::collections::HashMap;

use ledger_core::tokens::{infer_architectural_layer, split_identifier_tokens};
use ledger_core::{
    ByteSpan, ContentHash, ContractEndpoint, FileBuffer, FileRecord, InterfaceHash, Language,
    LineSpan, LogicalSymbolKey, ParseCoverage, SymbolKind, SymbolRecord,
    scope::sanitize_untrusted_text,
};
use thiserror::Error;
use tree_sitter::{Node, Parser};

use crate::contracts::ContractExtractor;

/// Errors emitted if a Tree-sitter grammar cannot initialize.
#[derive(Debug, Error)]
pub enum ParserError {
    /// Failed to load language grammar into Tree-sitter.
    #[error("failed to configure tree-sitter grammar for {0}")]
    GrammarLoad(Language),
    /// Tree-sitter returned no syntax tree (timeout or cancellation).
    #[error("tree-sitter produced no syntax tree for '{0}'")]
    NoSyntaxTree(String),
}

/// Raw unresolved call expression recorded inside a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawCallSite {
    /// Enclosing symbol key where the call occurs, if inside a symbol body.
    pub caller_symbol: Option<LogicalSymbolKey>,
    /// Simple callee identifier (`decode_cursor`, `apply_predicates`).
    pub callee_name: String,
    /// Optional receiver/module qualifier (`cursor` in `cursor::decode_cursor` or `api.fetchOrders`).
    pub qualifier: Option<String>,
    /// 1-indexed line number of the call expression.
    pub line: u32,
    /// Compact 1-line call-site snippet.
    pub snippet: String,
}

/// Complete extraction output for a single [`FileBuffer`].
#[derive(Debug, Clone)]
pub struct ExtractedFile {
    /// Summary file metadata record (with `ContentHash` and `InterfaceHash`).
    pub file: FileRecord,
    /// Extracted AST symbols ordered by appearance.
    pub symbols: Vec<SymbolRecord>,
    /// Raw import strings/specifiers (`crate::orders::query::apply_predicates`, `../api/orders`).
    pub imports: Vec<(u32, String)>,
    /// Unresolved call expressions inside the file.
    pub calls: Vec<RawCallSite>,
    /// Cross-boundary HTTP, SQL, and Config/Env contract endpoints.
    pub contracts: Vec<ContractEndpoint>,
}

/// Language-specific AST extraction output before cross-boundary contract and file enrichment.
#[derive(Debug, Clone)]
pub struct AstExtraction {
    /// Parse fidelity achieved for the file.
    pub coverage: ParseCoverage,
    /// Extracted AST symbols ordered by appearance.
    pub symbols: Vec<SymbolRecord>,
    /// Raw import strings/specifiers (`(line, specifier)`).
    pub imports: Vec<(u32, String)>,
    /// Unresolved call expressions inside the file.
    pub calls: Vec<RawCallSite>,
}

/// Composable trait implemented by each language grammar extractor.
pub trait LanguageExtractor: Send + Sync + std::fmt::Debug {
    /// Supported [`Language`].
    fn language(&self) -> Language;

    /// Parses `buffer` and populates AST symbols, imports, and call sites.
    fn extract_ast(&self, buffer: &FileBuffer) -> Result<AstExtraction, ParserError>;
}

/// Registry composing all registered [`LanguageExtractor`]s alongside [`ContractExtractor`].
#[derive(Debug)]
pub struct ParserRegistry {
    extractors: HashMap<Language, Box<dyn LanguageExtractor>>,
    contracts: ContractExtractor,
}

impl Default for ParserRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ParserRegistry {
    /// Builds a `ParserRegistry` equipped with Rust, TypeScript, JavaScript, Python, and Go extractors.
    #[must_use]
    pub fn new() -> Self {
        let mut extractors: HashMap<Language, Box<dyn LanguageExtractor>> = HashMap::new();
        extractors.insert(Language::Rust, Box::new(RustExtractor));
        extractors.insert(
            Language::TypeScript,
            Box::new(TypeScriptExtractor::new(false)),
        );
        extractors.insert(
            Language::JavaScript,
            Box::new(TypeScriptExtractor::new(true)),
        );
        extractors.insert(Language::Python, Box::new(PythonExtractor));
        extractors.insert(Language::Go, Box::new(GoExtractor));

        Self {
            extractors,
            contracts: ContractExtractor::new(),
        }
    }

    /// Extracts AST symbols, imports, calls, cross-boundary contracts, concepts, and
    /// `InterfaceHash` from an immutable [`FileBuffer`].
    pub fn extract_file(&self, buffer: &FileBuffer) -> ExtractedFile {
        let rel_path = buffer.rel_path();
        let path_obj = std::path::Path::new(rel_path);
        let language = Language::from_path(path_obj);
        let layer = infer_architectural_layer(rel_path, language);

        let AstExtraction {
            mut coverage,
            symbols,
            imports,
            calls,
        } = if let Some(ext) = self.extractors.get(&language) {
            ext.extract_ast(buffer).unwrap_or_else(|_| AstExtraction {
                coverage: ParseCoverage::LexicalOnly,
                symbols: Vec::new(),
                imports: Vec::new(),
                calls: Vec::new(),
            })
        } else {
            let cov = if matches!(
                language,
                Language::Sql | Language::Config | Language::Markdown | Language::Graphql
            ) {
                ParseCoverage::ContractsAndLexical
            } else {
                ParseCoverage::LexicalOnly
            };
            AstExtraction {
                coverage: cov,
                symbols: Vec::new(),
                imports: Vec::new(),
                calls: Vec::new(),
            }
        };

        let contracts = self.contracts.extract_contracts(buffer, language, &symbols);
        if coverage == ParseCoverage::LexicalOnly && !contracts.is_empty() {
            coverage = ParseCoverage::ContractsAndLexical;
        }

        // Compute deterministic InterfaceHash over exported signatures, imports, and contracts.
        let mut interface_descriptors = Vec::new();
        for sym in &symbols {
            if sym.exported {
                interface_descriptors.push(format!("sym:{}:{}", sym.kind, sym.signature));
            }
        }
        for (_, imp) in &imports {
            interface_descriptors.push(format!("imp:{imp}"));
        }
        for c in &contracts {
            interface_descriptors.push(format!(
                "contract:{}:{}:{}",
                c.kind, c.role, c.normalized_key
            ));
        }
        interface_descriptors.sort();
        let interface_hash =
            InterfaceHash::of_descriptors(interface_descriptors.iter().map(String::as_str));

        // Assemble semantic concept tokens from path segments, layer, and symbol names.
        let mut concepts = vec![layer.as_str().to_string(), language.as_str().to_string()];
        for seg in rel_path.split('/') {
            let stem = seg.split('.').next().unwrap_or(seg);
            concepts.extend(split_identifier_tokens(stem));
        }
        for sym in &symbols {
            concepts.extend(split_identifier_tokens(&sym.name));
        }
        concepts.sort();
        concepts.dedup();

        let file = FileRecord {
            rel_path: rel_path.to_string(),
            language,
            layer,
            content_hash: buffer.content_hash().clone(),
            interface_hash,
            coverage,
            byte_len: buffer.as_bytes().len(),
            line_count: buffer.line_count(),
            concepts,
        };

        ExtractedFile {
            file,
            symbols,
            imports,
            calls,
            contracts,
        }
    }
}

/// Helper state during single-file AST traversal.
struct WalkContext<'a> {
    buffer: &'a FileBuffer,
    symbols: Vec<SymbolRecord>,
    imports: Vec<(u32, String)>,
    calls: Vec<RawCallSite>,
    disambiguators: HashMap<String, u32>,
}

impl<'a> WalkContext<'a> {
    fn new(buffer: &'a FileBuffer) -> Self {
        Self {
            buffer,
            symbols: Vec::new(),
            imports: Vec::new(),
            calls: Vec::new(),
            disambiguators: HashMap::new(),
        }
    }

    fn node_text(&self, node: Node<'_>) -> &'a str {
        let bytes = self.buffer.as_bytes();
        let start = node.start_byte().min(bytes.len());
        let end = node.end_byte().min(bytes.len());
        std::str::from_utf8(&bytes[start..end]).unwrap_or("")
    }

    fn line_span(&self, node: Node<'_>) -> LineSpan {
        let start = u32::try_from(node.start_position().row + 1).unwrap_or(1);
        let mut end = u32::try_from(node.end_position().row + 1).unwrap_or(start);
        if node.end_position().column == 0 && end > start {
            end -= 1;
        }
        LineSpan::new(start, end)
    }

    fn first_line_signature(&self, node: Node<'_>) -> String {
        let raw = self.node_text(node);
        let first = raw
            .lines()
            .find(|l| {
                let t = l.trim();
                !t.is_empty() && !t.starts_with("#[") && !t.starts_with('@') && !t.starts_with("//")
            })
            .unwrap_or_else(|| raw.lines().next().unwrap_or(""));
        let sig = first
            .trim()
            .trim_end_matches('{')
            .trim_end_matches(':')
            .trim();
        sanitize_untrusted_text(sig, 180)
    }

    fn leading_doc_comment(&self, node: Node<'_>) -> Option<String> {
        let mut docs = Vec::new();
        let mut cur = node.prev_sibling();
        while let Some(prev) = cur {
            let kind = prev.kind();
            if kind.contains("comment") {
                let text = self.node_text(prev).trim();
                let cleaned = text
                    .trim_start_matches("///")
                    .trim_start_matches("//!")
                    .trim_start_matches("//")
                    .trim_start_matches('#')
                    .trim_start_matches("/**")
                    .trim_end_matches("*/")
                    .trim();
                if !cleaned.is_empty() {
                    docs.push(cleaned.to_string());
                }
                cur = prev.prev_sibling();
            } else if kind == "attribute_item" || kind == "decorator" {
                cur = prev.prev_sibling();
            } else {
                break;
            }
        }
        if docs.is_empty() {
            None
        } else {
            docs.reverse();
            Some(sanitize_untrusted_text(&docs.join(" "), 320))
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn push_symbol(
        &mut self,
        node: Node<'_>,
        kind: SymbolKind,
        name: &str,
        scope: Option<&str>,
        exported: bool,
        override_doc: Option<String>,
    ) -> LogicalSymbolKey {
        let scope_str = scope.unwrap_or("");
        let slot_id = format!(
            "{}:{}:{scope_str}:{name}",
            self.buffer.rel_path(),
            kind.as_str()
        );
        let count = self.disambiguators.entry(slot_id).or_insert(0);
        let disambig = *count;
        *count += 1;

        let logical_key = LogicalSymbolKey::derive(
            self.buffer.rel_path(),
            kind.as_str(),
            scope_str,
            name,
            disambig,
        );
        let signature = self.first_line_signature(node);
        let body_text = self.node_text(node);
        let docstring = override_doc.or_else(|| self.leading_doc_comment(node));

        self.symbols.push(SymbolRecord {
            logical_key: logical_key.clone(),
            rel_path: self.buffer.rel_path().to_string(),
            kind,
            name: name.to_string(),
            scope: scope.map(ToString::to_string),
            signature_hash: ContentHash::of_bytes(signature.as_bytes()),
            body_hash: ContentHash::of_bytes(body_text.as_bytes()),
            signature,
            docstring,
            exported,
            byte_span: ByteSpan::new(node.start_byte(), node.end_byte()),
            line_span: self.line_span(node),
        });
        logical_key
    }

    fn push_call(
        &mut self,
        node: Node<'_>,
        enclosing: Option<&LogicalSymbolKey>,
        callee_raw: &str,
    ) {
        let cleaned = callee_raw.trim();
        if cleaned.is_empty() {
            return;
        }
        let (qualifier, callee_name) = if let Some((q, name)) = cleaned.rsplit_once("::") {
            (Some(q.trim().to_string()), name.trim().to_string())
        } else if let Some((q, name)) = cleaned.rsplit_once('.') {
            (Some(q.trim().to_string()), name.trim().to_string())
        } else {
            (None, cleaned.to_string())
        };

        if callee_name.len() < 2
            || !callee_name
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '!')
        {
            return;
        }
        let line = u32::try_from(node.start_position().row + 1).unwrap_or(1);
        let snippet = sanitize_untrusted_text(
            self.node_text(node).lines().next().unwrap_or("").trim(),
            140,
        );
        self.calls.push(RawCallSite {
            caller_symbol: enclosing.cloned(),
            callee_name: callee_name.trim_end_matches('!').to_string(),
            qualifier,
            line,
            snippet,
        });
    }
}

// ============================================================================
// 1. Rust Extractor
// ============================================================================

#[derive(Debug)]
struct RustExtractor;

impl RustExtractor {
    fn has_test_attr(&self, ctx: &WalkContext<'_>, node: Node<'_>) -> bool {
        let mut cur = node.prev_sibling();
        while let Some(prev) = cur {
            if prev.kind() == "attribute_item" {
                let txt = ctx.node_text(prev);
                if txt.contains("test") {
                    return true;
                }
                cur = prev.prev_sibling();
            } else if prev.kind().contains("comment") {
                cur = prev.prev_sibling();
            } else {
                break;
            }
        }
        false
    }

    fn walk(
        &self,
        ctx: &mut WalkContext<'_>,
        node: Node<'_>,
        scope: Option<&str>,
        enclosing_sym: Option<&LogicalSymbolKey>,
    ) {
        match node.kind() {
            "use_declaration" => {
                let line = u32::try_from(node.start_position().row + 1).unwrap_or(1);
                let text = ctx
                    .node_text(node)
                    .trim()
                    .trim_start_matches("pub ")
                    .trim_start_matches("use ")
                    .trim_end_matches(';')
                    .trim()
                    .to_string();
                ctx.imports.push((line, text));
            }
            "function_item" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = ctx.node_text(name_node).to_string();
                    let exported = ctx.node_text(node).trim_start().starts_with("pub");
                    let kind = if self.has_test_attr(ctx, node) || name.starts_with("test_") {
                        SymbolKind::Test
                    } else if scope.is_some() {
                        SymbolKind::Method
                    } else {
                        SymbolKind::Function
                    };
                    let key = ctx.push_symbol(node, kind, &name, scope, exported, None);
                    let mut cursor = node.walk();
                    for child in node.children(&mut cursor) {
                        self.walk(ctx, child, scope, Some(&key));
                    }
                    return;
                }
            }
            "struct_item" | "enum_item" | "trait_item" | "type_item" | "const_item"
            | "mod_item" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = ctx.node_text(name_node).to_string();
                    let exported = ctx.node_text(node).trim_start().starts_with("pub");
                    let kind = match node.kind() {
                        "struct_item" => SymbolKind::StructOrClass,
                        "enum_item" => SymbolKind::Enum,
                        "trait_item" => SymbolKind::TraitOrInterface,
                        "type_item" => SymbolKind::TypeAlias,
                        "const_item" => SymbolKind::Constant,
                        _ => SymbolKind::Module,
                    };
                    let key = ctx.push_symbol(node, kind, &name, scope, exported, None);
                    let next_scope = if kind == SymbolKind::Module {
                        Some(name)
                    } else {
                        scope.map(ToString::to_string)
                    };
                    let mut cursor = node.walk();
                    for child in node.children(&mut cursor) {
                        self.walk(ctx, child, next_scope.as_deref(), Some(&key));
                    }
                    return;
                }
            }
            "impl_item" => {
                let impl_type = node
                    .child_by_field_name("type")
                    .map(|n| ctx.node_text(n).to_string());
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    self.walk(ctx, child, impl_type.as_deref().or(scope), enclosing_sym);
                }
                return;
            }
            "call_expression" => {
                if let Some(func_node) = node.child_by_field_name("function") {
                    let callee = ctx.node_text(func_node).to_string();
                    ctx.push_call(node, enclosing_sym, &callee);
                }
            }
            _ => {}
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.walk(ctx, child, scope, enclosing_sym);
        }
    }
}

impl LanguageExtractor for RustExtractor {
    fn language(&self) -> Language {
        Language::Rust
    }

    fn extract_ast(&self, buffer: &FileBuffer) -> Result<AstExtraction, ParserError> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .map_err(|_| ParserError::GrammarLoad(Language::Rust))?;
        let tree = parser
            .parse(buffer.as_bytes(), None)
            .ok_or_else(|| ParserError::NoSyntaxTree(buffer.rel_path().to_string()))?;

        let root = tree.root_node();
        let coverage = if root.has_error() {
            ParseCoverage::PartialAstWithSyntaxErrors
        } else {
            ParseCoverage::ScopedAst
        };

        let mut ctx = WalkContext::new(buffer);
        self.walk(&mut ctx, root, None, None);
        Ok(AstExtraction {
            coverage,
            symbols: ctx.symbols,
            imports: ctx.imports,
            calls: ctx.calls,
        })
    }
}

// ============================================================================
// 2. TypeScript / JavaScript Extractor
// ============================================================================

#[derive(Debug)]
struct TypeScriptExtractor {
    is_javascript: bool,
}

impl TypeScriptExtractor {
    const fn new(is_javascript: bool) -> Self {
        Self { is_javascript }
    }

    fn walk(
        &self,
        ctx: &mut WalkContext<'_>,
        node: Node<'_>,
        scope: Option<&str>,
        enclosing_sym: Option<&LogicalSymbolKey>,
        parent_exported: bool,
    ) {
        match node.kind() {
            "export_statement" => {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    self.walk(ctx, child, scope, enclosing_sym, true);
                }
                return;
            }
            "import_statement" => {
                let line = u32::try_from(node.start_position().row + 1).unwrap_or(1);
                let txt = ctx.node_text(node).trim().trim_end_matches(';').to_string();
                ctx.imports.push((line, txt));
            }
            "function_declaration" | "method_definition" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = ctx.node_text(name_node).to_string();
                    let kind = if node.kind() == "method_definition" {
                        SymbolKind::Method
                    } else {
                        SymbolKind::Function
                    };
                    let key = ctx.push_symbol(
                        node,
                        kind,
                        &name,
                        scope,
                        parent_exported || kind == SymbolKind::Method,
                        None,
                    );
                    let mut cursor = node.walk();
                    for child in node.children(&mut cursor) {
                        self.walk(ctx, child, scope, Some(&key), false);
                    }
                    return;
                }
            }
            "class_declaration" | "interface_declaration" | "type_alias_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = ctx.node_text(name_node).to_string();
                    let kind = match node.kind() {
                        "class_declaration" => SymbolKind::StructOrClass,
                        "interface_declaration" => SymbolKind::TraitOrInterface,
                        _ => SymbolKind::TypeAlias,
                    };
                    let key = ctx.push_symbol(node, kind, &name, scope, parent_exported, None);
                    let next_scope = if kind == SymbolKind::StructOrClass {
                        Some(name)
                    } else {
                        scope.map(ToString::to_string)
                    };
                    let mut cursor = node.walk();
                    for child in node.children(&mut cursor) {
                        self.walk(ctx, child, next_scope.as_deref(), Some(&key), false);
                    }
                    return;
                }
            }
            "variable_declarator" => {
                if let (Some(name_node), Some(val_node)) = (
                    node.child_by_field_name("name"),
                    node.child_by_field_name("value"),
                ) && matches!(val_node.kind(), "arrow_function" | "function_expression")
                {
                    let name = ctx.node_text(name_node).to_string();
                    if name
                        .chars()
                        .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
                    {
                        let key = ctx.push_symbol(
                            node,
                            SymbolKind::Function,
                            &name,
                            scope,
                            parent_exported,
                            None,
                        );
                        let mut cursor = val_node.walk();
                        for child in val_node.children(&mut cursor) {
                            self.walk(ctx, child, scope, Some(&key), false);
                        }
                        return;
                    }
                }
            }
            "call_expression" => {
                if let Some(func_node) = node.child_by_field_name("function") {
                    let callee = ctx.node_text(func_node).to_string();
                    ctx.push_call(node, enclosing_sym, &callee);
                }
            }
            _ => {}
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.walk(ctx, child, scope, enclosing_sym, false);
        }
    }
}

impl LanguageExtractor for TypeScriptExtractor {
    fn language(&self) -> Language {
        if self.is_javascript {
            Language::JavaScript
        } else {
            Language::TypeScript
        }
    }

    fn extract_ast(&self, buffer: &FileBuffer) -> Result<AstExtraction, ParserError> {
        let mut parser = Parser::new();
        let is_tsx = buffer.rel_path().ends_with(".tsx");
        let lang_res = if self.is_javascript {
            parser.set_language(&tree_sitter_javascript::LANGUAGE.into())
        } else if is_tsx {
            parser.set_language(&tree_sitter_typescript::LANGUAGE_TSX.into())
        } else {
            parser.set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
        };
        lang_res.map_err(|_| ParserError::GrammarLoad(self.language()))?;

        let tree = parser
            .parse(buffer.as_bytes(), None)
            .ok_or_else(|| ParserError::NoSyntaxTree(buffer.rel_path().to_string()))?;

        let root = tree.root_node();
        let coverage = if root.has_error() {
            ParseCoverage::PartialAstWithSyntaxErrors
        } else {
            ParseCoverage::ScopedAst
        };

        let mut ctx = WalkContext::new(buffer);
        self.walk(&mut ctx, root, None, None, false);
        Ok(AstExtraction {
            coverage,
            symbols: ctx.symbols,
            imports: ctx.imports,
            calls: ctx.calls,
        })
    }
}

// ============================================================================
// 3. Python Extractor
// ============================================================================

#[derive(Debug)]
struct PythonExtractor;

impl PythonExtractor {
    fn extract_python_docstring(&self, ctx: &WalkContext<'_>, body: Node<'_>) -> Option<String> {
        let first = body.named_child(0)?;
        if first.kind() == "expression_statement" {
            let expr = first.named_child(0)?;
            if expr.kind() == "string" {
                let raw = ctx.node_text(expr).trim();
                let stripped = raw.trim_matches('"').trim_matches('\'').trim();
                if !stripped.is_empty() {
                    return Some(sanitize_untrusted_text(stripped, 320));
                }
            }
        }
        None
    }

    fn walk(
        &self,
        ctx: &mut WalkContext<'_>,
        node: Node<'_>,
        scope: Option<&str>,
        enclosing_sym: Option<&LogicalSymbolKey>,
    ) {
        match node.kind() {
            "import_statement" | "import_from_statement" => {
                let line = u32::try_from(node.start_position().row + 1).unwrap_or(1);
                let txt = ctx.node_text(node).trim().to_string();
                ctx.imports.push((line, txt));
            }
            "function_definition" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = ctx.node_text(name_node).to_string();
                    let exported = !name.starts_with('_');
                    let kind = if name.starts_with("test_") {
                        SymbolKind::Test
                    } else if scope.is_some() {
                        SymbolKind::Method
                    } else {
                        SymbolKind::Function
                    };
                    let doc = node
                        .child_by_field_name("body")
                        .and_then(|b| self.extract_python_docstring(ctx, b));
                    let key = ctx.push_symbol(node, kind, &name, scope, exported, doc);
                    let mut cursor = node.walk();
                    for child in node.children(&mut cursor) {
                        self.walk(ctx, child, scope, Some(&key));
                    }
                    return;
                }
            }
            "class_definition" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = ctx.node_text(name_node).to_string();
                    let exported = !name.starts_with('_');
                    let doc = node
                        .child_by_field_name("body")
                        .and_then(|b| self.extract_python_docstring(ctx, b));
                    let key = ctx.push_symbol(
                        node,
                        SymbolKind::StructOrClass,
                        &name,
                        scope,
                        exported,
                        doc,
                    );
                    let mut cursor = node.walk();
                    for child in node.children(&mut cursor) {
                        self.walk(ctx, child, Some(&name), Some(&key));
                    }
                    return;
                }
            }
            "call" => {
                if let Some(func_node) = node.child_by_field_name("function") {
                    let callee = ctx.node_text(func_node).to_string();
                    ctx.push_call(node, enclosing_sym, &callee);
                }
            }
            _ => {}
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.walk(ctx, child, scope, enclosing_sym);
        }
    }
}

impl LanguageExtractor for PythonExtractor {
    fn language(&self) -> Language {
        Language::Python
    }

    fn extract_ast(&self, buffer: &FileBuffer) -> Result<AstExtraction, ParserError> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .map_err(|_| ParserError::GrammarLoad(Language::Python))?;
        let tree = parser
            .parse(buffer.as_bytes(), None)
            .ok_or_else(|| ParserError::NoSyntaxTree(buffer.rel_path().to_string()))?;

        let root = tree.root_node();
        let coverage = if root.has_error() {
            ParseCoverage::PartialAstWithSyntaxErrors
        } else {
            ParseCoverage::ScopedAst
        };

        let mut ctx = WalkContext::new(buffer);
        self.walk(&mut ctx, root, None, None);
        Ok(AstExtraction {
            coverage,
            symbols: ctx.symbols,
            imports: ctx.imports,
            calls: ctx.calls,
        })
    }
}

// ============================================================================
// 4. Go Extractor
// ============================================================================

#[derive(Debug)]
struct GoExtractor;

impl GoExtractor {
    fn walk(
        &self,
        ctx: &mut WalkContext<'_>,
        node: Node<'_>,
        enclosing_sym: Option<&LogicalSymbolKey>,
    ) {
        match node.kind() {
            "import_spec" => {
                let line = u32::try_from(node.start_position().row + 1).unwrap_or(1);
                let txt = ctx.node_text(node).trim().trim_matches('"').to_string();
                ctx.imports.push((line, txt));
            }
            "function_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = ctx.node_text(name_node).to_string();
                    let exported = name.chars().next().is_some_and(char::is_uppercase);
                    let kind =
                        if name.starts_with("Test") && buffer_is_go_test(ctx.buffer.rel_path()) {
                            SymbolKind::Test
                        } else {
                            SymbolKind::Function
                        };
                    let key = ctx.push_symbol(node, kind, &name, None, exported, None);
                    let mut cursor = node.walk();
                    for child in node.children(&mut cursor) {
                        self.walk(ctx, child, Some(&key));
                    }
                    return;
                }
            }
            "method_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = ctx.node_text(name_node).to_string();
                    let exported = name.chars().next().is_some_and(char::is_uppercase);
                    let recv = node
                        .child_by_field_name("receiver")
                        .map(|r| {
                            ctx.node_text(r)
                                .trim_matches(|c| c == '(' || c == ')' || c == '*')
                                .split_whitespace()
                                .last()
                                .unwrap_or("")
                                .trim_start_matches('*')
                                .to_string()
                        })
                        .filter(|s| !s.is_empty());
                    let key = ctx.push_symbol(
                        node,
                        SymbolKind::Method,
                        &name,
                        recv.as_deref(),
                        exported,
                        None,
                    );
                    let mut cursor = node.walk();
                    for child in node.children(&mut cursor) {
                        self.walk(ctx, child, Some(&key));
                    }
                    return;
                }
            }
            "type_spec" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = ctx.node_text(name_node).to_string();
                    let exported = name.chars().next().is_some_and(char::is_uppercase);
                    let kind = match node.child_by_field_name("type").map(|t| t.kind()) {
                        Some("interface_type") => SymbolKind::TraitOrInterface,
                        Some("struct_type") => SymbolKind::StructOrClass,
                        _ => SymbolKind::TypeAlias,
                    };
                    ctx.push_symbol(node, kind, &name, None, exported, None);
                }
            }
            "call_expression" => {
                if let Some(func_node) = node.child_by_field_name("function") {
                    let callee = ctx.node_text(func_node).to_string();
                    ctx.push_call(node, enclosing_sym, &callee);
                }
            }
            _ => {}
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.walk(ctx, child, enclosing_sym);
        }
    }
}

fn buffer_is_go_test(rel_path: &str) -> bool {
    rel_path.ends_with("_test.go")
}

impl LanguageExtractor for GoExtractor {
    fn language(&self) -> Language {
        Language::Go
    }

    fn extract_ast(&self, buffer: &FileBuffer) -> Result<AstExtraction, ParserError> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
            .map_err(|_| ParserError::GrammarLoad(Language::Go))?;
        let tree = parser
            .parse(buffer.as_bytes(), None)
            .ok_or_else(|| ParserError::NoSyntaxTree(buffer.rel_path().to_string()))?;

        let root = tree.root_node();
        let coverage = if root.has_error() {
            ParseCoverage::PartialAstWithSyntaxErrors
        } else {
            ParseCoverage::ScopedAst
        };

        let mut ctx = WalkContext::new(buffer);
        self.walk(&mut ctx, root, None);
        Ok(AstExtraction {
            coverage,
            symbols: ctx.symbols,
            imports: ctx.imports,
            calls: ctx.calls,
        })
    }
}
