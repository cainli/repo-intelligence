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
