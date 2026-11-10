//! Phase 2 cross-language contract extraction, resolution, and test evidence.

use ledger_core::{ContractKind, EdgeConfidence, FileBuffer, RelationKind, TestProvenance};
use ledger_parser::{
    ContractExtractor, ExtractedFile, ParserRegistry, ScopedResolver, detect_manifest,
};

fn extract(registry: &ParserRegistry, path: &str, source: &str) -> ExtractedFile {
    registry.extract_file(&FileBuffer::from_string(path, source))
}

#[test]
fn event_channels_pair_across_rust_python_typescript_and_go() {
    let registry = ParserRegistry::new();
    let files = [
        extract(
            &registry,
            "src/events.rs",
            r#"
pub fn emit_events() {
    bus.emit("billing:invoice_voided", invoice);
    bus.publish("order.created", order);
    bus.dispatch("orders_updated", order);
    dispatch_event("audit/invoice", invoice);
    event_bus.send("billing-invoice", invoice);
}
"#,
        ),
        extract(
            &registry,
            "workers/invoices.py",
            r#"
@on_event("billing:invoice_voided")
def invoice_voided(invoice):
    pass
@subscribe("order.created")
def order_created(order):
    pass
"#,
        ),
        extract(
            &registry,
            "web/events.ts",
            r#"
bus.once("orders_updated", callback);
bus.subscribe("audit/invoice", callback);
addEventListener("billing-invoice", callback);
"#,
        ),
        extract(
            &registry,
            "workers/events.go",
            r#"
package workers
func listen() { bus.on("billing:invoice_voided", callback) }
"#,
        ),
    ];
    let graph = ScopedResolver::new().resolve(&files, &[]);
    let edges: Vec<_> = graph
        .edges
        .iter()
        .filter(|edge| edge.relation == RelationKind::EventChannel)
        .collect();
    assert_eq!(edges.len(), 6, "{edges:#?}");
    assert!(
        edges
            .iter()
            .all(|edge| edge.confidence == EdgeConfidence::VerifiedPair)
    );
    assert!(
        edges
            .iter()
            .any(|edge| edge.src_file == "workers/invoices.py")
    );
    assert!(
        edges
            .iter()
            .any(|edge| edge.src_file == "workers/events.go")
    );
}

#[test]
fn generic_dom_and_stream_events_produce_zero_contract_edges() {
    let registry = ParserRegistry::new();
    let noise = [
        "click",
        "change",
        "input",
        "submit",
        "focus",
        "blur",
        "keydown",
        "keyup",
        "mousedown",
        "mouseup",
        "load",
        "unload",
        "resize",
        "scroll",
        "error",
        "data",
        "end",
        "close",
        "open",
        "connect",
        "disconnect",
        "message",
        "ready",
    ];
    let provider = noise
        .iter()
        .map(|name| format!("bus.emit(\"{name}\", payload);\n"))
        .collect::<String>();
    let consumer = noise
        .iter()
        .map(|name| format!("node.addEventListener(\"{name}\", cb); stream.on(\"{name}\", cb);\n"))
        .collect::<String>();
    let files = [
        extract(&registry, "src/noise.ts", &provider),
        extract(&registry, "web/noise.ts", &consumer),
    ];
    assert!(files.iter().all(|file| {
        file.contracts
            .iter()
            .all(|contract| contract.kind != ContractKind::EventChannel)
    }));
    let graph = ScopedResolver::new().resolve(&files, &[]);
    assert_eq!(
        graph
            .edges
            .iter()
            .filter(|edge| edge.relation == RelationKind::EventChannel)
            .count(),
        0
    );
}

#[test]
fn event_namespaces_dynamic_prefixes_and_comments_are_preserved() {
    let registry = ParserRegistry::new();
    let source = r##"
#[on_event("billing:ready")]
fn handle() {}
bus.publish("billing:", payload);
bus.emit(`${ns}:invoice_voided`, payload);
// bus.emit("comment:fake", payload);
let example = "bus.emit('string:fake', payload)";
"##;
    let file = extract(&registry, "src/events.rs", source);
    let contracts: Vec<_> = file
        .contracts
        .iter()
        .filter(|endpoint| endpoint.kind == ContractKind::EventChannel)
        .collect();
    assert_eq!(contracts.len(), 3, "{contracts:#?}");
    assert!(
        contracts
            .iter()
            .any(|endpoint| endpoint.normalized_key == "EVENT billing:ready"
                && endpoint.role == "consumer")
    );
    assert!(
        contracts
            .iter()
            .any(|endpoint| endpoint.normalized_key == "EVENT billing:" && endpoint.is_dynamic)
    );
    assert!(contracts.iter().any(|endpoint| endpoint.normalized_key
        == "EVENT {param}:invoice_voided"
        && endpoint.is_dynamic));
}

#[test]
fn graphql_schema_pairs_only_root_fields_with_tagged_client_operations() {
    let registry = ParserRegistry::new();
    let schema = extract(
        &registry,
        "api/schema.graphql",
        r#"
directive @auth(role: String = "reader") repeatable on FIELD_DEFINITION | OBJECT
type Invoice { ok: Boolean! status: String! }
type Query {
    "List all orders"
    orders(filter: Filter = {status: "open"}): [Order!]!
    invoice(id: ID!): Invoice
}
extend type Query { health: Boolean! }
type Mutation { voidInvoice(id: ID!): Invoice! }
type Subscription { orderUpdated: Order! }
"#,
    );
    let client = extract(
        &registry,
        "web/invoices.ts",
        r#"
const orders = gql`query GetOrders($filter: Filter) {
    recent: orders(filter: $filter) { id invoice { status } }
    __typename
}`;
const invoice = graphql(`mutation VoidInvoice($id: ID!) {
    voidInvoice(id: $id) { ok status }
}`);
const subscription = useQuery(`subscription OnOrder { orderUpdated { id } }`);
const health = gql`{ health }`;
"#,
    );
    let client_keys: Vec<_> = client
        .contracts
        .iter()
        .filter(|endpoint| endpoint.kind == ContractKind::GraphqlSchemaOp)
        .map(|endpoint| endpoint.normalized_key.as_str())
        .collect();
    assert_eq!(
        client_keys,
        [
            "GQL QUERY.orders",
            "GQL MUTATION.voidInvoice",
            "GQL SUBSCRIPTION.orderUpdated",
            "GQL QUERY.health"
        ]
    );
    assert!(
        !schema
            .contracts
            .iter()
            .any(|endpoint| endpoint.normalized_key.ends_with(".status"))
    );
    let graph = ScopedResolver::new().resolve(&[schema, client], &[]);
    let edges: Vec<_> = graph
        .edges
        .iter()
        .filter(|edge| edge.relation == RelationKind::GraphqlSchemaOp)
        .collect();
    assert_eq!(edges.len(), 4, "{edges:#?}");
    assert!(
        edges
            .iter()
            .all(|edge| edge.confidence == EdgeConfidence::VerifiedPair)
    );
    assert!(
        !edges
            .iter()
            .any(|edge| edge.evidence_snippet.starts_with("GQL MUTATION.ok"))
    );
}

#[test]
fn graphql_embedded_sdl_code_first_maps_and_annotations_are_providers() {
    let registry = ParserRegistry::new();
    let sdl = extract(
        &registry,
        "api/schema.py",
        r#"
schema = """
type Query { orders: [Order!]! }
extend type Mutation { voidInvoice(id: ID!): Invoice! }
"""
"#,
    );
    let resolvers = extract(
        &registry,
        "api/resolvers.ts",
        r#"
const resolvers = {
    Query: {
        orders(parent, args) { return { nested: { wrong() {} } }; },
        invoice: async (_, args) => ({ id: args.id }),
    },
    Mutation: { async voidInvoice(_, args) { return { ok: true }; } },
};
class Root {
    @Query(() => String)
    health() { return "ok"; }
    @Mutation(() => Boolean)
    archiveInvoice() { return true; }
}
"#,
    );
    let rust = extract(
        &registry,
        "api/graphql.rs",
        r#"
impl QueryRoot {
    #[graphql]
    async fn account(&self) -> Account { account() }
}
impl MutationRoot {
    #[graphql]
    async fn cancelInvoice(&self) -> bool { true }
}
"#,
    );
    for (file, keys) in [
        (&sdl, vec!["GQL QUERY.orders", "GQL MUTATION.voidInvoice"]),
        (
            &resolvers,
            vec![
                "GQL QUERY.orders",
                "GQL QUERY.invoice",
                "GQL MUTATION.voidInvoice",
                "GQL QUERY.health",
                "GQL MUTATION.archiveInvoice",
            ],
        ),
        (
            &rust,
            vec!["GQL QUERY.account", "GQL MUTATION.cancelInvoice"],
        ),
    ] {
        let actual: Vec<_> = file
            .contracts
            .iter()
            .filter(|endpoint| endpoint.kind == ContractKind::GraphqlSchemaOp)
            .map(|endpoint| endpoint.normalized_key.as_str())
            .collect();
        assert_eq!(actual, keys, "{}", file.file.rel_path);
        assert!(
            file.contracts
                .iter()
                .filter(|endpoint| endpoint.kind == ContractKind::GraphqlSchemaOp)
                .all(|endpoint| endpoint.role == "provider")
        );
    }
}

#[test]
fn graphql_operation_documents_ignore_nested_arguments_fragments_and_typename() {
    let registry = ParserRegistry::new();
    let file = extract(
        &registry,
        "web/operations.gql",
        r#"
fragment OrderFields on Order { id status }
query Orders($filter: Filter = {state: "ready"}) {
    first: orders(filter: $filter) @include(if: true) { ...OrderFields }
    invoice(id: "abc") { ok }
    __typename
    ... on Query { ignored { id } }
}
mutation Void { voidInvoice(input: {id: "abc", flags: [true]}) { ok status } }
{ health }
"#,
    );
    let keys: Vec<_> = file
        .contracts
        .iter()
        .filter(|endpoint| endpoint.kind == ContractKind::GraphqlSchemaOp)
        .map(|endpoint| endpoint.normalized_key.as_str())
        .collect();
    assert_eq!(
        keys,
        [
            "GQL QUERY.orders",
            "GQL QUERY.invoice",
            "GQL MUTATION.voidInvoice",
            "GQL QUERY.health"
        ]
    );
}

#[test]
fn dynamic_http_literals_normalize_without_dynamic_origin_segments() {
    let registry = ParserRegistry::new();
    let cases = [
        (
            "client.rs",
            r#"client.get(format!("/api/orders/{}", id)); client.get(format!("/api/orders/{id}"));"#,
            vec!["GET /api/orders/{param}", "GET /api/orders/{param}"],
        ),
        (
            "client.py",
            r#"requests.get(f"/api/orders/{order_id}")"#,
            vec!["GET /api/orders/{param}"],
        ),
        (
            "client.ts",
            "fetch(`${API_BASE}/api/orders/${id}`); fetch(`${baseUrl}/api/v1/orders`); fetch(\"/api/orders/\" + id);",
            vec![
                "GET /api/orders/{param}",
                "GET /api/v1/orders",
                "GET /api/orders/{param}",
            ],
        ),
    ];
    for (path, source, expected) in cases {
        let file = extract(&registry, path, source);
        let contracts: Vec<_> = file
            .contracts
            .iter()
            .filter(|endpoint| endpoint.kind == ContractKind::HttpEndpoint)
            .collect();
        // Equivalent contracts on one line intentionally collapse to one endpoint.
        let actual: Vec<_> = contracts
            .iter()
            .map(|endpoint| endpoint.normalized_key.as_str())
            .collect();
        let mut expected = expected;
        expected.dedup();
        let mut seen = std::collections::HashSet::new();
        expected.retain(|key| seen.insert(*key));
        assert_eq!(actual, expected, "{path}: {contracts:#?}");
        assert!(contracts.iter().all(|endpoint| endpoint.is_dynamic));
    }
    assert_eq!(
        ContractExtractor::normalize_http_path("${API_BASE}/api/orders/${id}?page=1"),
        ("/api/orders/{param}".to_string(), true)
    );
    assert_eq!(
        ContractExtractor::normalize_http_path("/api/orders/:id"),
        ("/api/orders/{param}".to_string(), false)
    );
}

#[test]
fn dynamic_http_prefix_pairs_respect_methods_segments_and_exact_deduplication() {
    let registry = ParserRegistry::new();
    let provider = extract(
        &registry,
        "src/routes.rs",
        r#"
#[get("/api/orders/:id")]
pub fn get_order() {}
#[get("/api/orders/:id/items")]
pub fn get_items() {}
#[post("/api/orders/:id")]
pub fn post_order() {}
#[get("/api/orders-archive/:id")]
pub fn archive() {}
"#,
    );
    let consumer = extract(
        &registry,
        "web/orders.ts",
        r#"
fetch(`${API_BASE}/api/orders/${id}`);
fetch(`${API_BASE}/api/orders`);
fetch("/api/orders/:id", { method: "POST" });
"#,
    );
    let graph = ScopedResolver::new().resolve(&[provider, consumer], &[]);
    let edges: Vec<_> = graph
        .edges
        .iter()
        .filter(|edge| edge.relation == RelationKind::HttpEndpoint)
        .collect();
    assert_eq!(edges.len(), 5, "{edges:#?}");
    assert_eq!(
        edges
            .iter()
            .filter(|edge| edge.confidence == EdgeConfidence::DynamicPrefix)
            .count(),
        4
    );
    assert_eq!(
        edges
            .iter()
            .filter(|edge| edge.confidence == EdgeConfidence::VerifiedPair)
            .count(),
        1
    );
    assert!(
        !edges
            .iter()
            .any(|edge| edge.evidence_snippet.contains("orders-archive"))
    );
}

#[test]
fn shared_http_event_and_graphql_contracts_derive_test_candidates() {
    let registry = ParserRegistry::new();
    let files = [
        extract(
            &registry,
            "src/routes.rs",
            "#[get(\"/api/orders/:id/items\")]\npub fn items() {}",
        ),
        extract(
            &registry,
            "tests/routes.rs",
            "#[test]\nfn routes() { client.get(format!(\"/api/orders/{}\", id)); }",
        ),
        extract(
            &registry,
            "src/events.rs",
            "pub fn send() { bus.emit(\"billing:invoice_voided\", payload); }",
        ),
        extract(
            &registry,
            "workers/test_events.py",
            "@subscribe(\"billing:invoice_voided\")\ndef test_events():\n    pass\n",
        ),
        extract(
            &registry,
            "api/schema.graphql",
            "type Mutation { voidInvoice(id: ID!): Invoice! }",
        ),
        extract(
            &registry,
            "web/invoice.spec.ts",
            "const operation = gql`mutation Void { voidInvoice(id: 1) { ok } }`;",
        ),
    ];
    let manifests: Vec<_> = [
        detect_manifest("Cargo.toml", "[package]\nname = \"demo\"\n"),
        detect_manifest("pyproject.toml", "[project]"),
        detect_manifest("package.json", "{}"),
    ]
    .into_iter()
    .flatten()
    .collect();
    let resolver = ScopedResolver::new();
    let graph = resolver.resolve(&files, &manifests);
    for (target, test) in [
        ("src/routes.rs", "tests/routes.rs"),
        ("src/events.rs", "workers/test_events.py"),
        ("api/schema.graphql", "web/invoice.spec.ts"),
    ] {
        let candidates = graph.tests_by_target_file.get(target);
        assert!(
            candidates.is_some_and(|candidates| candidates
                .iter()
                .any(|candidate| candidate.test_file == test
                    && candidate.provenance == TestProvenance::ContractRouteRef)),
            "{target}: {candidates:#?}"
        );
    }
    assert_eq!(
        graph.tests_by_target_file,
        resolver.derive_test_candidates(&files, &graph.edges, &manifests)
    );
}

#[test]
fn test_provenance_prioritizes_direct_reference_over_contract_and_proximity() {
    let registry = ParserRegistry::new();
    let files = [
        extract(
            &registry,
            "src/orders.rs",
            r#"
#[get("/api/orders")]
pub fn list_orders() {}
#[test]
fn orders_test() {
    list_orders();
    client.get("/api/orders");
}
"#,
        ),
        extract(
            &registry,
            "tests/orders.rs",
            "#[test]\nfn check_orders() { client.get(\"/api/orders\"); }",
        ),
    ];
    let manifests: Vec<_> = detect_manifest("Cargo.toml", "[package]\nname = \"demo\"\n")
        .into_iter()
        .collect();
    let graph = ScopedResolver::new().resolve(&files, &manifests);
    let candidates = graph.tests_by_target_file.get("src/orders.rs");
    assert!(
        candidates.is_some_and(|list| list
            .first()
            .is_some_and(|candidate| candidate.provenance == TestProvenance::DirectSymbolRef)),
        "{candidates:#?}"
    );
    assert!(candidates.is_some_and(|list| {
        list.iter()
            .any(|candidate| candidate.provenance == TestProvenance::ContractRouteRef)
    }));
    assert!(candidates.is_some_and(|list| {
        list.iter()
            .filter(|candidate| candidate.command.ends_with(" orders_test"))
            .count()
            == 1
    }));
}
