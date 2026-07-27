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
