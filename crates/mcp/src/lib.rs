use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;

use anyhow::Result;
use repo_intelligence_analysis::{ImpactAnalyzer, WorkspaceIndexer};
use repo_intelligence_graph::{GraphStore, SqliteGraphStore};
use repo_intelligence_model::{ChangeRequest, SearchQuery};
use serde_json::{Value, json};

const TOOLS: &[(&str, &str)] = &[
    (
        "scan_workspace",
        "Index a workspace into the local system graph",
    ),
    ("search_entities", "Search indexed entities"),
    ("find_endpoint", "Find an HTTP endpoint"),
    (
        "analyze_change",
        "Analyze the impact of a structured change",
    ),
    (
        "analyze_requirement",
        "Find candidate code locations for requirement text",
    ),
    (
        "show_system_view",
        "Show a repository, API, or data system view",
    ),
    ("get_index_status", "Read local index status"),
];

pub fn serve<R: Read, W: Write>(reader: R, mut writer: W, database: Option<&Path>) -> Result<()> {
    for line in BufReader::new(reader).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                eprintln!("[repo-intelligence:mcp] invalid JSON-RPC message: {error}");
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": {"code": -32700, "message": "Parse error"}
                });
                serde_json::to_writer(&mut writer, &response)?;
                writer.write_all(b"\n")?;
                writer.flush()?;
                continue;
            }
        };
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if request.get("id").is_none() {
            continue;
        }
        let response = match method {
            "initialize" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "repo-intelligence", "version": env!("CARGO_PKG_VERSION")}
                }
            }),
            "tools/list" => {
                let tools: Vec<Value> = TOOLS
                    .iter()
                    .map(|(name, description)| {
                        json!({
                            "name": name,
                            "description": description,
                            "inputSchema": {"type": "object", "additionalProperties": true}
                        })
                    })
                    .collect();
                json!({"jsonrpc": "2.0", "id": id, "result": {"tools": tools}})
            }
            "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
            "tools/call" => match call_tool(&request, database) {
                Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                Err(error) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "isError": true,
                        "content": [{"type": "text", "text": format!("{error:#}")}]
                    }
                }),
            },
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": format!("method not implemented: {method}")}
            }),
        };
        serde_json::to_writer(&mut writer, &response)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
    }
    Ok(())
}

fn call_tool(request: &Value, database: Option<&Path>) -> Result<Value> {
    let params = &request["params"];
    let name = params["name"].as_str().unwrap_or_default();
    let arguments = &params["arguments"];
    let path = database.ok_or_else(|| anyhow::anyhow!("MCP server has no database configured"))?;
    let data = match name {
        "search_entities" | "find_endpoint" | "analyze_requirement" => {
            let query = arguments["query"]
                .as_str()
                .or_else(|| arguments["text"].as_str())
                .unwrap_or_default();
            let store = SqliteGraphStore::open(path)?;
            let matches = store.search(SearchQuery::new(query).with_limit(100))?;
            serde_json::to_value(
                matches
                    .into_iter()
                    .map(|matched| matched.entity)
                    .collect::<Vec<_>>(),
            )?
        }
        "analyze_change" => {
            let change: ChangeRequest = serde_json::from_value(arguments.clone())?;
            let store = SqliteGraphStore::open(path)?;
            serde_json::to_value(ImpactAnalyzer::new(&store).analyze(&change)?)?
        }
        "scan_workspace" => {
            let workspace = arguments["workspace"].as_str().unwrap_or(".");
            let mut store = SqliteGraphStore::open(path)?;
            let summary = WorkspaceIndexer.scan(Path::new(workspace), &mut store)?;
            json!({
                "files_indexed": summary.files_indexed,
                "entities_indexed": summary.entities_indexed,
                "edges_indexed": summary.edges_indexed
            })
        }
        "get_index_status" => {
            let store = SqliteGraphStore::open(path)?;
            let (entity_count, edge_count) = store.counts()?;
            json!({
                "database": path.display().to_string(),
                "entity_count": entity_count,
                "edge_count": edge_count,
            })
        }
        "show_system_view" => {
            let store = SqliteGraphStore::open(path)?;
            let view = arguments["view"].as_str().unwrap_or("repositories");
            let (entity_count, edge_count) = store.counts()?;
            let entities_by_kind = store.counts_by_kind()?;
            json!({
                "view": view,
                "entity_count": entity_count,
                "edge_count": edge_count,
                "entities_by_kind": entities_by_kind,
            })
        }
        _ => return Err(anyhow::anyhow!("tool not implemented: {name}")),
    };
    Ok(json!({
        "content": [{"type": "text", "text": serde_json::to_string(&data)?}],
        "structuredContent": data
    }))
}
