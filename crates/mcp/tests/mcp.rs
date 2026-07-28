use std::fs;
use std::io::Cursor;

use repo_intelligence_graph::{GraphStore, SqliteGraphStore};
use repo_intelligence_model::{
    Edge, EdgeKind, Entity, EntityId, EntityKind, EvidenceClass, GraphPatch,
};

#[test]
fn mcp_lists_supported_tools_over_json_rpc() {
    let input = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}
"#;
    let mut output = Vec::new();
    repo_intelligence_mcp::serve(Cursor::new(input), &mut output, None).unwrap();
    let response: serde_json::Value =
        serde_json::from_slice(output.split(|byte| *byte == b'\n').next().unwrap()).unwrap();
    assert_eq!(response["id"], 1);
    assert!(
        response["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "analyze_change")
    );
}

#[test]
fn mcp_searches_the_persistent_graph() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("graph.sqlite");
    let mut store = SqliteGraphStore::open(&database).unwrap();
    let entity = Entity::new(
        EntityId::stable("repo", "Order.java", EntityKind::Field, "customerName", ""),
        EntityKind::Field,
        "customerName",
        "Order.customerName",
    );
    store
        .apply_patch(GraphPatch::add(vec![entity], vec![]))
        .unwrap();
    drop(store);

    let input = br#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search_entities","arguments":{"query":"customerName"}}}
"#;
    let mut output = Vec::new();
    repo_intelligence_mcp::serve(Cursor::new(input), &mut output, Some(&database)).unwrap();
    let response: serde_json::Value = serde_json::from_slice(output.trim_ascii()).unwrap();
    assert_eq!(response["id"], 2);
    assert!(
        response["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("customerName")
    );
}

#[test]
fn malformed_message_does_not_close_the_mcp_session() {
    let input = br#"not-json
{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":9}}
{"jsonrpc":"2.0","id":10,"method":"ping","params":{}}
{"jsonrpc":"2.0","id":11,"method":"tools/list","params":{}}
"#;
    let mut output = Vec::new();

    repo_intelligence_mcp::serve(Cursor::new(input), &mut output, None).unwrap();

    let responses: Vec<serde_json::Value> = output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(responses.len(), 3);
    assert_eq!(responses[0]["error"]["code"], -32700);
    assert_eq!(responses[1]["id"], 10);
    assert_eq!(responses[1]["result"], serde_json::json!({}));
    assert_eq!(responses[2]["id"], 11);
    assert!(responses[2]["result"]["tools"].is_array());
}

#[test]
fn index_status_is_bounded_for_large_graphs() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("graph.sqlite");
    let mut store = SqliteGraphStore::open(&database).unwrap();
    let entities = (0..1_000)
        .map(|index| {
            Entity::new(
                EntityId::stable(
                    "repo",
                    "Generated.java",
                    EntityKind::Field,
                    &format!("field{index}"),
                    "",
                ),
                EntityKind::Field,
                format!("field{index}"),
                format!("Generated.field{index}"),
            )
        })
        .collect();
    store
        .apply_patch(GraphPatch::add(entities, vec![]))
        .unwrap();
    drop(store);

    let input = br#"{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"get_index_status","arguments":{}}}
"#;
    let mut output = Vec::new();
    repo_intelligence_mcp::serve(Cursor::new(input), &mut output, Some(&database)).unwrap();

    let response: serde_json::Value = serde_json::from_slice(output.trim_ascii()).unwrap();
    assert_eq!(
        response["result"]["structuredContent"]["entity_count"],
        1_000
    );
    assert!(output.len() < 2_000);
}

#[test]
fn index_status_reports_an_uninitialized_index() {
    // An empty index must not masquerade as a healthy server: a zero-entity
    // result otherwise looks like "it works", until every other tool quietly
    // returns empty results. get_index_status flags it with `indexed: false`
    // and a hint pointing at scan_workspace.
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("graph.sqlite");
    // Materialize the schema but index nothing.
    drop(SqliteGraphStore::open(&database).unwrap());

    let input = br#"{"jsonrpc":"2.0","id":41,"method":"tools/call","params":{"name":"get_index_status","arguments":{}}}
"#;
    let mut output = Vec::new();
    repo_intelligence_mcp::serve(Cursor::new(input), &mut output, Some(&database)).unwrap();

    let response: serde_json::Value = serde_json::from_slice(output.trim_ascii()).unwrap();
    let structured = &response["result"]["structuredContent"];
    assert_eq!(structured["entity_count"], 0);
    assert_eq!(structured["indexed"], false);
    assert!(
        structured["hint"]
            .as_str()
            .unwrap()
            .contains("scan_workspace"),
        "empty index should hint at scan_workspace, got: {structured}"
    );
}

#[test]
fn index_status_reports_absolute_database_path() {
    // 默认 database 是相对路径(.repo-intelligence/workspace.sqlite),依赖 cwd。
    // get_index_status 应规范化为绝对路径,让调用方一眼看出连的是哪个库。
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("graph.sqlite");
    drop(SqliteGraphStore::open(&database).unwrap());

    let input = br#"{"jsonrpc":"2.0","id":42,"method":"tools/call","params":{"name":"get_index_status","arguments":{}}}
"#;
    let mut output = Vec::new();
    repo_intelligence_mcp::serve(Cursor::new(input), &mut output, Some(&database)).unwrap();
    let response: serde_json::Value = serde_json::from_slice(output.trim_ascii()).unwrap();
    let reported = response["result"]["structuredContent"]["database"]
        .as_str()
        .unwrap();
    assert!(
        reported.starts_with('/'),
        "database path should be absolute, got: {reported}"
    );
    assert!(reported.ends_with("graph.sqlite"));
}

#[test]
fn analyze_change_warns_when_index_is_empty() {
    // 空库时影响分析无意义——应在 open_questions 显式提示,避免"零影响"被误读为"安全"。
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("graph.sqlite");
    drop(SqliteGraphStore::open(&database).unwrap());

    let input = br#"{"jsonrpc":"2.0","id":43,"method":"tools/call","params":{"name":"analyze_change","arguments":{"target_kind":"field","operation":"rename","from":"x","to":"y"}}}
"#;
    let mut output = Vec::new();
    repo_intelligence_mcp::serve(Cursor::new(input), &mut output, Some(&database)).unwrap();
    let response: serde_json::Value = serde_json::from_slice(output.trim_ascii()).unwrap();
    let questions = response["result"]["structuredContent"]["open_questions"]
        .as_array()
        .expect("open_questions array");
    assert!(
        questions
            .iter()
            .any(|question| question.as_str().unwrap_or("").contains("index is empty")),
        "empty index should be flagged in open_questions, got: {questions:?}"
    );
}

#[test]
fn system_view_is_bounded_and_groups_by_kind() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("graph.sqlite");
    let mut store = SqliteGraphStore::open(&database).unwrap();
    let entities = (0..500)
        .flat_map(|index| {
            [
                Entity::new(
                    EntityId::stable(
                        "repo",
                        "A.java",
                        EntityKind::Field,
                        &format!("f{index}"),
                        "",
                    ),
                    EntityKind::Field,
                    format!("f{index}"),
                    format!("A.f{index}"),
                ),
                Entity::new(
                    EntityId::stable(
                        "repo",
                        "A.java",
                        EntityKind::Method,
                        &format!("m{index}"),
                        "",
                    ),
                    EntityKind::Method,
                    format!("m{index}"),
                    format!("A.m{index}"),
                ),
            ]
        })
        .collect::<Vec<_>>();
    store
        .apply_patch(GraphPatch::add(entities, vec![]))
        .unwrap();
    drop(store);

    let input = br#"{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"show_system_view","arguments":{"view":"repositories"}}}
"#;
    let mut output = Vec::new();
    repo_intelligence_mcp::serve(Cursor::new(input), &mut output, Some(&database)).unwrap();

    let response: serde_json::Value = serde_json::from_slice(output.trim_ascii()).unwrap();
    let structured = &response["result"]["structuredContent"];
    assert_eq!(structured["entity_count"], 1_000);
    assert_eq!(structured["entities_by_kind"]["field"], 500);
    assert_eq!(structured["entities_by_kind"]["method"], 500);
    assert!(output.len() < 2_000);
}

#[test]
fn search_returns_structured_content_as_an_object() {
    // Regression guard: the search-style tools used to put a bare JSON array
    // into `structuredContent`, which MCP rejects ("expected record, received
    // array"). The result must be wrapped in an object such as {items, count}.
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("graph.sqlite");
    let mut store = SqliteGraphStore::open(&database).unwrap();
    let entity = Entity::new(
        EntityId::stable("repo", "Order.java", EntityKind::Field, "customerName", ""),
        EntityKind::Field,
        "customerName",
        "Order.customerName",
    );
    store
        .apply_patch(GraphPatch::add(vec![entity], vec![]))
        .unwrap();
    drop(store);

    let input = br#"{"jsonrpc":"2.0","id":21,"method":"tools/call","params":{"name":"search_entities","arguments":{"query":"customerName"}}}
"#;
    let mut output = Vec::new();
    repo_intelligence_mcp::serve(Cursor::new(input), &mut output, Some(&database)).unwrap();

    let response: serde_json::Value = serde_json::from_slice(output.trim_ascii()).unwrap();
    let structured = &response["result"]["structuredContent"];
    assert!(
        structured.is_object(),
        "structuredContent must be an object, was: {structured}"
    );
    assert!(!structured.is_array());
    assert!(structured["items"].is_array());
    assert_eq!(structured["count"], 1);
}

#[test]
fn system_view_filters_to_the_requested_plane() {
    // The `view` argument must actually change the result: a focused view
    // (api/data) returns only that plane's kinds, while `repositories` (and
    // any unrecognized view) returns the full overview.
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("graph.sqlite");
    let mut store = SqliteGraphStore::open(&database).unwrap();
    let entities = vec![
        Entity::new(
            EntityId::stable("repo", "T.sql", EntityKind::Table, "orders", ""),
            EntityKind::Table,
            "orders",
            "orders",
        ),
        Entity::new(
            EntityId::stable("repo", "O.java", EntityKind::Field, "customerName", ""),
            EntityKind::Field,
            "customerName",
            "Order.customerName",
        ),
        Entity::new(
            EntityId::stable(
                "repo",
                "Api.java",
                EntityKind::HttpEndpoint,
                "GET /orders",
                "",
            ),
            EntityKind::HttpEndpoint,
            "GET /orders",
            "GET /orders",
        ),
    ];
    store
        .apply_patch(GraphPatch::add(entities, vec![]))
        .unwrap();
    drop(store);

    let input = br#"{"jsonrpc":"2.0","id":31,"method":"tools/call","params":{"name":"show_system_view","arguments":{"view":"data"}}}
"#;
    let mut output = Vec::new();
    repo_intelligence_mcp::serve(Cursor::new(input), &mut output, Some(&database)).unwrap();
    let response: serde_json::Value = serde_json::from_slice(output.trim_ascii()).unwrap();
    let by_kind = &response["result"]["structuredContent"]["entities_by_kind"];
    assert_eq!(by_kind["table"], 1);
    assert!(
        by_kind.get("field").is_none(),
        "code-plane kinds must be filtered out of the data view"
    );
    assert!(
        by_kind.get("http_endpoint").is_none(),
        "api-plane kinds must be filtered out of the data view"
    );
}

#[test]
fn tools_list_declares_typed_input_and_output_schemas() {
    // Regression guard: tools/list used to advertise an empty input schema
    // (`{type:object, additionalProperties:true}`) with no declared properties,
    // so clients never transmitted arguments — the "parameter black hole".
    let input = br#"{"jsonrpc":"2.0","id":22,"method":"tools/list","params":{}}
"#;
    let mut output = Vec::new();
    repo_intelligence_mcp::serve(Cursor::new(input), &mut output, None).unwrap();

    let response: serde_json::Value =
        serde_json::from_slice(output.split(|byte| *byte == b'\n').next().unwrap()).unwrap();
    let tools = response["result"]["tools"].as_array().unwrap();

    let analyze_change = tools
        .iter()
        .find(|tool| tool["name"] == "analyze_change")
        .unwrap();
    let input_props = &analyze_change["inputSchema"]["properties"];
    assert!(input_props.is_object());
    assert!(
        input_props["target_kind"].is_object(),
        "analyze_change must declare target_kind"
    );
    assert!(input_props["operation"].is_object());
    assert_eq!(
        analyze_change["inputSchema"]["additionalProperties"], false,
        "input schemas should be closed"
    );
    assert_eq!(analyze_change["outputSchema"]["type"], "object");
}

#[test]
fn find_endpoint_returns_only_endpoint_kinds() {
    // find_endpoint must filter to endpoint kinds — previously it shared the
    // search_entities path verbatim and returned any matching class/field.
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("graph.sqlite");
    let mut store = SqliteGraphStore::open(&database).unwrap();
    let entities = vec![
        Entity::new(
            EntityId::stable("repo", "Dto.java", EntityKind::Class, "OrderDto", ""),
            EntityKind::Class,
            "OrderDto",
            "OrderDto",
        ),
        Entity::new(
            EntityId::stable(
                "repo",
                "Api.java",
                EntityKind::HttpEndpoint,
                "GET /orders",
                "",
            ),
            EntityKind::HttpEndpoint,
            "GET /orders",
            "GET /orders",
        ),
    ];
    store
        .apply_patch(GraphPatch::add(entities, vec![]))
        .unwrap();
    drop(store);

    // "order" matches both OrderDto (class) and GET /orders (endpoint).
    let input = br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"find_endpoint","arguments":{"query":"order"}}}
"#;
    let mut output = Vec::new();
    repo_intelligence_mcp::serve(Cursor::new(input), &mut output, Some(&database)).unwrap();
    let response: serde_json::Value = serde_json::from_slice(output.trim_ascii()).unwrap();
    let items = response["result"]["structuredContent"]["items"]
        .as_array()
        .unwrap();
    assert_eq!(items.len(), 1, "only the endpoint should be returned");
    assert_eq!(items[0]["kind"], "http_endpoint");
}

#[test]
fn analyze_change_paginates_findings_and_reports_total() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("graph.sqlite");
    let mut store = SqliteGraphStore::open(&database).unwrap();
    let entities: Vec<_> = (0..5)
        .map(|index| {
            Entity::new(
                EntityId::stable(
                    "repo",
                    &format!("F{index}.java"),
                    EntityKind::Field,
                    "sharedName",
                    &format!("{index}"),
                ),
                EntityKind::Field,
                "sharedName",
                format!("F{index}.sharedName"),
            )
        })
        .collect();
    store
        .apply_patch(GraphPatch::add(entities, vec![]))
        .unwrap();
    drop(store);

    let input = br#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"analyze_change","arguments":{"target_kind":"field","operation":"remove","from":"sharedName","limit":2,"offset":0}}}
"#;
    let mut output = Vec::new();
    repo_intelligence_mcp::serve(Cursor::new(input), &mut output, Some(&database)).unwrap();
    let response: serde_json::Value = serde_json::from_slice(output.trim_ascii()).unwrap();
    let structured = &response["result"]["structuredContent"];
    assert_eq!(structured["findings"].as_array().unwrap().len(), 2);
    assert_eq!(structured["total"], 5);
    assert_eq!(structured["limit"], 2);
    assert_eq!(structured["offset"], 0);
    assert_eq!(structured["has_more"], true);
}

#[test]
fn scan_workspace_reports_kind_distribution_and_excluded_dirs() {
    let workspace = tempfile::tempdir().unwrap();
    fs::write(
        workspace.path().join("Dto.java"),
        "class Dto { private String name; }",
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("graph.sqlite");
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {"name": "scan_workspace", "arguments": {"workspace": workspace.path()}}
    });
    let input = serde_json::to_string(&request).unwrap();

    let mut output = Vec::new();
    repo_intelligence_mcp::serve(Cursor::new(input.as_bytes()), &mut output, Some(&database))
        .unwrap();
    let response: serde_json::Value = serde_json::from_slice(output.trim_ascii()).unwrap();
    let structured = &response["result"]["structuredContent"];
    assert!(structured["entities_by_kind"].is_object());
    assert!(
        structured["entities_by_kind"]["class"]
            .as_u64()
            .is_some_and(|count| count >= 1)
    );
    let excluded = structured["excluded_dirs"]
        .as_array()
        .expect("excluded_dirs reported");
    assert!(
        excluded.iter().any(|value| value == ".repo-intelligence"),
        "excluded_dirs must list .repo-intelligence, got: {excluded:?}"
    );
}

#[test]
fn empty_search_attaches_a_hint_instead_of_silent_zero() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("graph.sqlite");
    drop(SqliteGraphStore::open(&database).unwrap());

    let input = br#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"analyze_requirement","arguments":{"query":"nonexistent"}}}
"#;
    let mut output = Vec::new();
    repo_intelligence_mcp::serve(Cursor::new(input), &mut output, Some(&database)).unwrap();
    let response: serde_json::Value = serde_json::from_slice(output.trim_ascii()).unwrap();
    let structured = &response["result"]["structuredContent"];
    assert_eq!(structured["count"], 0);
    assert!(
        structured["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("substring")),
        "empty result should hint at substring matching, got: {structured}"
    );
}

/// Seed a call chain S27501 --calls--> S27204 --calls--> S27000 plus an
/// S27204 --depends_on--> S28000 edge, mirroring the RMB project scenario from
/// the 0.1.6 comparison: the call chain must now be recoverable without grep.
fn index_call_chain(database: &std::path::Path) {
    let mut store = SqliteGraphStore::open(database).unwrap();
    let make = |name: &str| {
        Entity::new(
            EntityId::stable("repo", name, EntityKind::Method, name, ""),
            EntityKind::Method,
            name,
            format!("mes.{name}"),
        )
    };
    let s27501 = make("S27501");
    let s27204 = make("S27204");
    let s27000 = make("S27000");
    let s28000 = make("S28000");
    let edges = vec![
        Edge::new(s27501.id.clone(), s27204.id.clone(), EdgeKind::Calls),
        Edge::new(s27204.id.clone(), s27000.id.clone(), EdgeKind::Calls),
        Edge::new(s27204.id.clone(), s28000.id.clone(), EdgeKind::DependsOn),
    ];
    store
        .apply_patch(GraphPatch::add(vec![s27501, s27204, s27000, s28000], edges))
        .unwrap();
}

/// Drive a single `tools/call` and return its `structuredContent`.
fn call_tool(
    database: &std::path::Path,
    name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 99,
        "method": "tools/call",
        "params": {"name": name, "arguments": arguments}
    });
    let mut output = Vec::new();
    let line = format!("{request}\n");
    repo_intelligence_mcp::serve(Cursor::new(line.as_bytes()), &mut output, Some(database))
        .unwrap();
    let response: serde_json::Value = serde_json::from_slice(output.trim_ascii()).unwrap();
    response["result"]["structuredContent"].clone()
}

fn entity_names(structured: &serde_json::Value) -> Vec<String> {
    structured["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entity| entity["name"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn trace_callers_walks_the_inbound_call_chain() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("graph.sqlite");
    index_call_chain(&database);

    let structured = call_tool(
        &database,
        "trace_callers",
        serde_json::json!({"name": "S27204"}),
    );

    assert_eq!(structured["start_count"], 1);
    let names = entity_names(&structured);
    assert!(names.contains(&"S27204".to_owned()));
    assert!(
        names.contains(&"S27501".to_owned()),
        "upstream caller must be reachable: {structured}"
    );
    assert!(
        !names.contains(&"S27000".to_owned()),
        "downstream callee must not appear in callers: {structured}"
    );
    // Exactly one inbound call edge, both endpoints among the visited entities.
    let item_ids: Vec<&str> = structured["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entity| entity["id"].as_str().unwrap())
        .collect();
    assert_eq!(structured["edges"].as_array().unwrap().len(), 1);
    let edge = &structured["edges"][0];
    assert_eq!(edge["kind"], "calls");
    assert!(item_ids.contains(&edge["source"].as_str().unwrap()));
    assert!(item_ids.contains(&edge["target"].as_str().unwrap()));
}

#[test]
fn trace_callees_walks_the_outbound_call_chain() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("graph.sqlite");
    index_call_chain(&database);

    let structured = call_tool(
        &database,
        "trace_callees",
        serde_json::json!({"name": "S27204"}),
    );

    assert_eq!(structured["start_count"], 1);
    let names = entity_names(&structured);
    assert!(names.contains(&"S27204".to_owned()));
    assert!(
        names.contains(&"S27000".to_owned()),
        "downstream callee must be reachable: {structured}"
    );
    assert!(
        !names.contains(&"S28000".to_owned()),
        "depends_on neighbor must not appear with the default calls filter: {structured}"
    );
    assert_eq!(structured["edges"].as_array().unwrap().len(), 1);
    assert_eq!(structured["edges"][0]["kind"], "calls");
}

#[test]
fn trace_follows_a_non_default_edge_kind() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("graph.sqlite");
    index_call_chain(&database);

    // Asking for depends_on reaches S28000, which the default calls filter hides.
    let structured = call_tool(
        &database,
        "trace_callees",
        serde_json::json!({"name": "S27204", "edge_kinds": ["depends_on"]}),
    );
    let names = entity_names(&structured);
    assert!(
        names.contains(&"S28000".to_owned()),
        "depends_on neighbor must be reachable: {structured}"
    );
    assert!(!names.contains(&"S27000".to_owned()));
    assert_eq!(
        structured["edges"].as_array().unwrap()[0]["kind"],
        "depends_on"
    );
}

#[test]
fn trace_respects_depth() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("graph.sqlite");
    index_call_chain(&database);

    // depth 1 from S27501 reaches S27204 but not the second hop S27000.
    let structured = call_tool(
        &database,
        "trace_callees",
        serde_json::json!({"name": "S27501", "depth": 1}),
    );
    let names = entity_names(&structured);
    assert!(names.contains(&"S27501".to_owned()));
    assert!(names.contains(&"S27204".to_owned()));
    assert!(
        !names.contains(&"S27000".to_owned()),
        "depth 1 must not reach the second hop: {structured}"
    );
}

#[test]
fn trace_unknown_name_attaches_a_hint() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("graph.sqlite");
    index_call_chain(&database);

    let structured = call_tool(
        &database,
        "trace_callers",
        serde_json::json!({"name": "NoSuchService"}),
    );
    assert_eq!(structured["start_count"], 0);
    assert_eq!(structured["count"], 0);
    assert!(
        structured["hint"].as_str().is_some(),
        "unknown start name should attach a hint, got: {structured}"
    );
}

#[test]
fn trace_tools_declare_typed_schemas() {
    let input = br#"{"jsonrpc":"2.0","id":60,"method":"tools/list","params":{}}
"#;
    let mut output = Vec::new();
    repo_intelligence_mcp::serve(Cursor::new(input), &mut output, None).unwrap();
    let response: serde_json::Value =
        serde_json::from_slice(output.split(|byte| *byte == b'\n').next().unwrap()).unwrap();
    let tools = response["result"]["tools"].as_array().unwrap();

    for tool_name in ["trace_callers", "trace_callees"] {
        let tool = tools
            .iter()
            .find(|tool| tool["name"] == tool_name)
            .unwrap_or_else(|| panic!("missing {tool_name} in tools/list"));
        let input_props = &tool["inputSchema"]["properties"];
        assert_eq!(
            input_props["name"]["type"], "string",
            "{tool_name} must take a name"
        );
        assert_eq!(input_props["depth"]["default"], 2);
        assert_eq!(
            input_props["edge_kinds"]["default"],
            serde_json::json!(["calls"])
        );
        assert_eq!(tool["inputSchema"]["additionalProperties"], false);
        assert_eq!(tool["outputSchema"]["type"], "object");
        let required: Vec<&str> = tool["outputSchema"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        for field in ["items", "edges", "count", "start_count"] {
            assert!(
                required.contains(&field),
                "{tool_name} output must require {field}"
            );
        }
    }
}

/// A field carrying two evidence rows, so a test can tell the compact view
/// (evidence_count only) from the verbose view (evidence[] bodies expanded).
fn field_with_evidence(name: &str) -> Entity {
    Entity::new(
        EntityId::stable("repo", "E.java", EntityKind::Field, name, ""),
        EntityKind::Field,
        name,
        format!("E.{name}"),
    )
    .with_evidence("E.java", 10, 12, EvidenceClass::Fact, 1.0, "field declared here")
    .with_evidence("Mapper.xml", 5, 5, EvidenceClass::Inferred, 0.7, "mapped in result map")
}

#[test]
fn search_compact_omits_evidence_bodies_by_default() {
    // 默认 compact 视图:扫几十条命中时不能把每条 evidence.reason 长串和
    // metadata blob 都塞进响应 —— 只给识别用的 id/kind/name/qualified_name +
    // evidence_count。这正是当初 search_entities("S27204") 撑到 ~11k token 的元凶。
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("graph.sqlite");
    let mut store = SqliteGraphStore::open(&database).unwrap();
    store
        .apply_patch(GraphPatch::add(vec![field_with_evidence("s27204_code")], vec![]))
        .unwrap();
    drop(store);

    let structured = call_tool(
        &database,
        "search_entities",
        serde_json::json!({"query": "s27204"}),
    );
    let item = &structured["items"][0];
    assert_eq!(item["name"], "s27204_code");
    assert_eq!(item["kind"], "field");
    assert_eq!(item["evidence_count"], 2);
    assert!(
        item.get("evidence").is_none(),
        "compact view must not include the evidence[] bodies, got: {item}"
    );
    assert!(
        item.get("metadata").is_none(),
        "compact view must not include metadata, got: {item}"
    );
    assert_eq!(structured["has_more"], false);
}

#[test]
fn search_verbose_expands_the_full_entity() {
    // verbose=true 才展开完整实体:LLM 拿到 compact 列表后,对想细看的条目
    // 显式请求 verbose 取 evidence[].reason / metadata。
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("graph.sqlite");
    let mut store = SqliteGraphStore::open(&database).unwrap();
    store
        .apply_patch(GraphPatch::add(vec![field_with_evidence("s27204_code")], vec![]))
        .unwrap();
    drop(store);

    let structured = call_tool(
        &database,
        "search_entities",
        serde_json::json!({"query": "s27204", "verbose": true}),
    );
    let item = &structured["items"][0];
    assert_eq!(
        item["evidence"].as_array().unwrap().len(),
        2,
        "verbose view must expand evidence[], got: {item}"
    );
    assert!(item.get("metadata").is_some());
}

#[test]
fn search_paginates_with_offset_and_reports_has_more() {
    // 宽匹配(企业 ID 命名数十实体)用 limit/offset 翻页,has_more 由 peek
    // limit+1 推断,无需 COUNT(*)。5 条命中、limit=2:前两页各 2 条且 has_more=true,
    // 末页 1 条且 has_more=false。
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("graph.sqlite");
    let mut store = SqliteGraphStore::open(&database).unwrap();
    let entities: Vec<_> = (0..5)
        .map(|index| {
            Entity::new(
                EntityId::stable(
                    "repo",
                    "P.java",
                    EntityKind::Field,
                    &format!("page_{index}"),
                    "",
                ),
                EntityKind::Field,
                format!("page_{index}"),
                format!("P.page_{index}"),
            )
        })
        .collect();
    store.apply_patch(GraphPatch::add(entities, vec![])).unwrap();
    drop(store);

    let page0 = call_tool(
        &database,
        "search_entities",
        serde_json::json!({"query": "page_", "limit": 2, "offset": 0}),
    );
    let page2 = call_tool(
        &database,
        "search_entities",
        serde_json::json!({"query": "page_", "limit": 2, "offset": 4}),
    );

    assert_eq!(page0["count"], 2);
    assert_eq!(page0["limit"], 2);
    assert_eq!(page0["offset"], 0);
    assert_eq!(page0["has_more"], true);
    assert_eq!(page2["count"], 1);
    assert_eq!(page2["has_more"], false);
}
