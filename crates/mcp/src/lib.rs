use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;

use anyhow::Result;
use repo_intelligence_analysis::{ImpactAnalyzer, WorkspaceIndexer};
use repo_intelligence_graph::{GraphStore, SqliteGraphStore};
use repo_intelligence_model::{ChangeRequest, SearchQuery};
use serde_json::{json, Value};

/// Default row cap for the text-search tools when the caller omits `limit`.
const DEFAULT_SEARCH_LIMIT: usize = 100;

/// A tool advertised over `tools/list`, together with its typed contracts.
///
/// `input_schema` and `output_schema` are JSON Schemas. Declaring them matters
/// for two reasons: clients only transmit arguments that the `inputSchema`
/// declares (an empty schema is a "parameter black hole"), and MCP requires
/// `structuredContent` to be a JSON object, so every `outputSchema` is an
/// object — array-valued results must be wrapped (e.g. `{"items": [...]}`).
struct ToolSpec {
    name: &'static str,
    description: &'static str,
    input_schema: Value,
    output_schema: Value,
}

fn evidence_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "file": {"type": "string"},
            "start_line": {"type": "integer", "minimum": 0},
            "end_line": {"type": "integer", "minimum": 0},
            "classification": {
                "type": "string",
                "enum": ["fact", "resolved", "inferred", "runtime_unknown"]
            },
            "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0},
            "reason": {"type": "string"}
        },
        "required": ["file", "start_line", "end_line", "classification", "confidence", "reason"]
    })
}

fn entity_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "id": {"type": "string"},
            "kind": {
                "type": "string",
                "enum": [
                    "workspace", "repository", "submodule", "file", "package",
                    "class", "interface", "method", "field", "vue_page", "vue_component",
                    "frontend_field", "http_client_call", "http_endpoint", "api_field",
                    "spring_bean", "mapper", "mapper_method", "xml_statement", "result_map",
                    "sql_field", "datasource", "database", "table", "column", "test_case",
                    "config_file"
                ]
            },
            "name": {"type": "string"},
            "qualified_name": {"type": "string"},
            "metadata": {},
            "evidence": {"type": "array", "items": evidence_schema()}
        },
        "required": ["id", "kind", "name", "qualified_name", "metadata", "evidence"]
    })
}

fn finding_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "entity": entity_schema(),
            "plane": {"type": "string"},
            "severity": {"type": "string"},
            "path": {"type": "array", "items": {"type": "string"}},
            "evidence": {"type": "array", "items": evidence_schema()}
        },
        "required": ["entity", "plane", "severity", "path", "evidence"]
    })
}

fn tool_specs() -> Vec<ToolSpec> {
    // The three text-search tools share one contract: a `query` (and optional
    // `limit`) in, and a wrapped `{items, count}` object out.
    let search_input = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "query": {
                "type": "string",
                "description": "Search text: an entity name, qualified name, or substring."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "default": 100,
                "description": "Maximum number of entities to return."
            }
        },
        "required": ["query"]
    });
    let search_output = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "items": {"type": "array", "items": entity_schema()},
            "count": {"type": "integer", "minimum": 0}
        },
        "required": ["items", "count"]
    });

    vec![
        ToolSpec {
            name: "scan_workspace",
            description: "Index a workspace into the local system graph.",
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "workspace": {
                        "type": "string",
                        "default": ".",
                        "description": "Path to the workspace to index."
                    }
                }
            }),
            output_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "files_indexed": {"type": "integer", "minimum": 0},
                    "entities_indexed": {"type": "integer", "minimum": 0},
                    "edges_indexed": {"type": "integer", "minimum": 0}
                },
                "required": ["files_indexed", "entities_indexed", "edges_indexed"]
            }),
        },
        ToolSpec {
            name: "search_entities",
            description: "Search indexed entities.",
            input_schema: search_input.clone(),
            output_schema: search_output.clone(),
        },
        ToolSpec {
            name: "find_endpoint",
            description: "Find an HTTP endpoint.",
            input_schema: search_input.clone(),
            output_schema: search_output.clone(),
        },
        ToolSpec {
            name: "analyze_change",
            description: "Analyze the impact of a structured change.",
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "target_kind": {
                        "type": "string",
                        "description": "Kind of entity being changed (e.g. \"field\")."
                    },
                    "operation": {
                        "type": "string",
                        "enum": [
                            "add", "remove", "rename", "change_type",
                            "change_nullable", "change_format", "change_semantics"
                        ]
                    },
                    "from": {
                        "type": "string",
                        "description": "Current name of the target entity (required to resolve impact)."
                    },
                    "to": {
                        "type": "string",
                        "description": "New name for a rename, or target for an add."
                    }
                },
                "required": ["target_kind", "operation", "from"]
            }),
            output_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "findings": {"type": "array", "items": finding_schema()},
                    "open_questions": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["findings", "open_questions"]
            }),
        },
        ToolSpec {
            name: "analyze_requirement",
            description: "Find candidate code locations for requirement text.",
            input_schema: search_input,
            output_schema: search_output,
        },
        ToolSpec {
            name: "show_system_view",
            description: "Show a repository, API, or data system view.",
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "view": {
                        "type": "string",
                        "default": "repositories",
                        "description": "Requested view name (e.g. repositories, api, data)."
                    }
                }
            }),
            output_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "view": {"type": "string"},
                    "entity_count": {"type": "integer", "minimum": 0},
                    "edge_count": {"type": "integer", "minimum": 0},
                    "entities_by_kind": {
                        "type": "object",
                        "additionalProperties": {"type": "integer"}
                    }
                },
                "required": ["view", "entity_count", "edge_count", "entities_by_kind"]
            }),
        },
        ToolSpec {
            name: "get_index_status",
            description: "Read local index status.",
            input_schema: json!({"type": "object", "additionalProperties": false, "properties": {}}),
            output_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "database": {"type": "string"},
                    "entity_count": {"type": "integer", "minimum": 0},
                    "edge_count": {"type": "integer", "minimum": 0}
                },
                "required": ["database", "entity_count", "edge_count"]
            }),
        },
    ]
}

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
                let tools: Vec<Value> = tool_specs()
                    .iter()
                    .map(|spec| {
                        json!({
                            "name": spec.name,
                            "description": spec.description,
                            "inputSchema": spec.input_schema.clone(),
                            "outputSchema": spec.output_schema.clone()
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
            let query = arguments["query"].as_str().unwrap_or_default();
            let limit = arguments["limit"]
                .as_u64()
                .map(|value| value.max(1) as usize)
                .unwrap_or(DEFAULT_SEARCH_LIMIT);
            let store = SqliteGraphStore::open(path)?;
            let matches = store.search(SearchQuery::new(query).with_limit(limit))?;
            let entities: Vec<_> = matches
                .into_iter()
                .map(|matched| matched.entity)
                .collect();
            let count = entities.len();
            // Wrap the array in an object: MCP forbids a bare array as
            // structuredContent ("expected record, received array").
            json!({
                "items": serde_json::to_value(&entities)?,
                "count": count,
            })
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
            let mut entities_by_kind = store.counts_by_kind()?;
            // `repositories` and any unrecognized view return the full overview;
            // `api`/`data` focus on a single plane's kinds so the `view` argument
            // actually changes the result instead of being echoed back ignored.
            let plane_kinds: Option<&[&str]> = match view {
                "api" => Some(&["http_endpoint", "api_field", "http_client_call"]),
                "data" => Some(&[
                    "table",
                    "column",
                    "sql_field",
                    "xml_statement",
                    "result_map",
                    "mapper",
                    "mapper_method",
                    "datasource",
                    "database",
                ]),
                _ => None,
            };
            if let Some(kinds) = plane_kinds {
                entities_by_kind.retain(|kind, _| kinds.contains(&kind.as_str()));
            }
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
