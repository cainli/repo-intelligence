use std::io::Cursor;

use repo_intelligence_graph::{GraphStore, SqliteGraphStore};
use repo_intelligence_model::{Entity, EntityId, EntityKind, GraphPatch};

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
