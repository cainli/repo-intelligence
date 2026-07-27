use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;

use anyhow::Result;
use repo_intelligence_analysis::{ImpactAnalyzer, WorkspaceIndexer};
use repo_intelligence_graph::{GraphStore, SqliteGraphStore};
use repo_intelligence_model::{ChangeRequest, EntityKind, SearchQuery};
use serde_json::{Value, json};

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
    // `limit`) in, and a wrapped `{items, count}` object out. `hint` is set only
    // when a search comes back empty, explaining why — so an empty result reads
    // as "nothing matched, here's what to try" rather than "tool broken".
    let search_input = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "query": {
                "type": "string",
                "description": "Search text: an entity name, qualified name, or substring. Matches indexed entity names/qualified names (case-insensitive substring); not annotations, comments, or natural language."
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
            "count": {"type": "integer", "minimum": 0},
            "hint": {"type": "string"}
        },
        "required": ["items", "count"]
    });

    vec![
        ToolSpec {
            name: "scan_workspace",
            description: "Index a workspace into the local system graph. Returns a health report (entity counts by kind and the excluded-directory list).",
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
                    "edges_indexed": {"type": "integer", "minimum": 0},
                    "entities_by_kind": {
                        "type": "object",
                        "additionalProperties": {"type": "integer"}
                    },
                    "excluded_dirs": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["files_indexed", "entities_indexed", "edges_indexed", "entities_by_kind", "excluded_dirs"]
            }),
        },
        ToolSpec {
            name: "search_entities",
            description: "Search indexed entities by name or qualified name (case-insensitive substring). Returns matches of any kind.",
            input_schema: search_input.clone(),
            output_schema: search_output.clone(),
        },
        ToolSpec {
            name: "find_endpoint",
            description: "Find HTTP/RPC endpoints by path or name. Only returns endpoint kinds (http_endpoint, http_client_call, api_field). Recognizes Spring MVC mappings and configured RPC annotations (@RmbMap, @DubboService).",
            input_schema: search_input.clone(),
            output_schema: search_output.clone(),
        },
        ToolSpec {
            name: "analyze_change",
            description: "Analyze the impact of a structured change. Returns a paginated window of findings (use limit/offset) with bounded traversal depth; total + has_more indicate whether more findings exist.",
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
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "default": 100,
                        "description": "Maximum findings to return."
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "default": 0,
                        "description": "Number of findings to skip (pagination)."
                    },
                    "depth": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Graph traversal depth around each finding. Defaults to an operation-appropriate value (destructive ops stay shallow) when omitted."
                    }
                },
                "required": ["target_kind", "operation", "from"]
            }),
            output_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "findings": {"type": "array", "items": finding_schema()},
                    "open_questions": {"type": "array", "items": {"type": "string"}},
                    "total": {"type": "integer", "minimum": 0},
                    "limit": {"type": "integer", "minimum": 0},
                    "offset": {"type": "integer", "minimum": 0},
                    "has_more": {"type": "boolean"}
                },
                "required": ["findings", "open_questions", "total", "limit", "offset", "has_more"]
            }),
        },
        ToolSpec {
            name: "analyze_requirement",
            description: "Find candidate code entities for a requirement keyword by matching indexed entity names/qualified names. Substring match only — not semantic or free-text search.",
            input_schema: search_input,
            output_schema: search_output,
        },
        ToolSpec {
            name: "show_system_view",
            description: "Show a repository, API, or data system view as bounded counts grouped by entity kind (never full entities).",
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "view": {
                        "type": "string",
                        "enum": ["repositories", "api", "data"],
                        "default": "repositories",
                        "description": "repositories: full overview. api: http_endpoint/api_field/http_client_call only. data: table/column/sql_field/xml_statement/result_map/mapper/mapper_method/datasource/database only."
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
                    },
                    "hint": {"type": "string"}
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
                    "indexed": {"type": "boolean"},
                    "entity_count": {"type": "integer", "minimum": 0},
                    "edge_count": {"type": "integer", "minimum": 0},
                    "hint": {"type": "string"}
                },
                "required": ["database", "indexed", "entity_count", "edge_count"]
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
            "tools/call" => dispatch_tool_call(&request, database, &id),
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

/// Runs a single `tools/call`, isolating panics so one failing tool never aborts
/// the session or starves its siblings.
///
/// `serve` is a single-threaded loop reading requests line by line; an
/// unwinding panic inside `call_tool` would otherwise unwind straight out of
/// `serve`, killing the process. Every call still queued behind the panic
/// (i.e. every parallel sibling the client fired in the same batch) then gets
/// no response at all and the client reports each as a generic "internal
/// error" — while the one call already answered returns fine. Catching the
/// unwind turns a panic into a normal MCP `isError` response, so the loop
/// continues. The default panic hook still writes the panic + location to
/// stderr for diagnosis.
fn dispatch_tool_call(request: &Value, database: Option<&Path>, id: &Value) -> Value {
    let tool_name = request["params"]["name"]
        .as_str()
        .unwrap_or("unknown")
        .to_string();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        call_tool(request, database)
    }));
    match outcome {
        Ok(Ok(result)) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Ok(Err(error)) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "isError": true,
                "content": [{"type": "text", "text": format!("{error:#}")}]
            }
        }),
        Err(panic) => {
            let message = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&'static str>().copied())
                .unwrap_or("panic with no message");
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "isError": true,
                    "content": [{
                        "type": "text",
                        "text": format!("tool `{tool_name}` panicked: {message}")
                    }]
                }
            })
        }
    }
}

fn call_tool(request: &Value, database: Option<&Path>) -> Result<Value> {
    let params = &request["params"];
    let name = params["name"].as_str().unwrap_or_default();
    let arguments = &params["arguments"];
    let path = database.ok_or_else(|| anyhow::anyhow!("MCP server has no database configured"))?;
    let data = match name {
        "search_entities" => {
            let query = arguments["query"].as_str().unwrap_or_default();
            let limit = parse_limit(arguments);
            let store = SqliteGraphStore::open(path)?;
            let (entities, count) = search_with_filter(&store, query, limit, None)?;
            json!({
                "items": serde_json::to_value(&entities)?,
                "count": count,
            })
        }
        "find_endpoint" => {
            let query = arguments["query"].as_str().unwrap_or_default();
            let limit = parse_limit(arguments);
            let store = SqliteGraphStore::open(path)?;
            let endpoint_kinds = [
                EntityKind::HttpEndpoint,
                EntityKind::HttpClientCall,
                EntityKind::ApiField,
            ];
            let (entities, count) =
                search_with_filter(&store, query, limit, Some(&endpoint_kinds))?;
            let mut result = json!({
                "items": serde_json::to_value(&entities)?,
                "count": count,
            });
            if count == 0 {
                result["hint"] = json!(
                    "No endpoints matched. Endpoints are recognized from Spring MVC mappings \
                     (@RequestMapping/@GetMapping/...) or configured RPC annotations (@RmbMap, \
                     @DubboService). If this service uses another framework, its entry points \
                     are not indexed."
                );
            }
            result
        }
        "analyze_requirement" => {
            let query = arguments["query"].as_str().unwrap_or_default();
            let limit = parse_limit(arguments);
            let store = SqliteGraphStore::open(path)?;
            let (entities, count) = search_with_filter(&store, query, limit, None)?;
            let mut result = json!({
                "items": serde_json::to_value(&entities)?,
                "count": count,
            });
            if count == 0 {
                result["hint"] = json!(
                    "No indexed entity name contains this text. This tool matches entity \
                     names/qualified names (classes, fields, tables, endpoints) by substring, \
                     not free text or natural language; try a concrete identifier."
                );
            }
            result
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
            // Echo the resulting kind distribution and the exclusion list so a
            // caller can spot a polluted index (e.g. a worktree copy doubling
            // every kind) from the scan result alone, without re-querying.
            let entities_by_kind = store.counts_by_kind()?;
            json!({
                "files_indexed": summary.files_indexed,
                "entities_indexed": summary.entities_indexed,
                "edges_indexed": summary.edges_indexed,
                "entities_by_kind": entities_by_kind,
                "excluded_dirs": repo_intelligence_source::EXCLUDED_DIRS,
            })
        }
        "get_index_status" => {
            let store = SqliteGraphStore::open(path)?;
            let (entity_count, edge_count) = store.counts()?;
            // Surface an uninitialized index explicitly: a zero-entity result
            // otherwise looks indistinguishable from a healthy server, so a
            // "never scanned" database reads as "working" until every other
            // tool quietly returns empty results.
            let indexed = entity_count > 0;
            let mut status = json!({
                "database": path.display().to_string(),
                "indexed": indexed,
                "entity_count": entity_count,
                "edge_count": edge_count,
            });
            if !indexed {
                status["hint"] =
                    json!("index is empty; run `scan_workspace` to populate it before querying");
            }
            status
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
            let plane_total: u64 = entities_by_kind.values().sum();
            let hint = if plane_kinds.is_some() && plane_total == 0 {
                Some(json!(match view {
                    "api" =>
                        "No API-plane entities indexed. Endpoints are recognized from Spring MVC mappings or configured RPC annotations (@RmbMap, @DubboService); a service using another framework has no indexed entry points.",
                    _ =>
                        "No data-plane entities indexed. Data-plane entities come from MyBatis XML / SQL; a project without them has none.",
                }))
            } else {
                None
            };
            let mut result = json!({
                "view": view,
                "entity_count": entity_count,
                "edge_count": edge_count,
                "entities_by_kind": entities_by_kind,
            });
            if let Some(hint) = hint {
                result["hint"] = hint;
            }
            result
        }
        _ => return Err(anyhow::anyhow!("tool not implemented: {name}")),
    };
    Ok(json!({
        "content": [{"type": "text", "text": serde_json::to_string(&data)?}],
        "structuredContent": data
    }))
}

fn parse_limit(arguments: &Value) -> usize {
    arguments["limit"]
        .as_u64()
        .map(|value| value.max(1) as usize)
        .unwrap_or(DEFAULT_SEARCH_LIMIT)
}

/// Runs a name/qualified-name search and optionally narrows to a set of kinds.
/// `find_endpoint` uses the filter so it only ever returns endpoint entities —
/// never the unrelated classes/fields a plain `search_entities` would surface.
fn search_with_filter(
    store: &SqliteGraphStore,
    query: &str,
    limit: usize,
    filter: Option<&[EntityKind]>,
) -> Result<(Vec<repo_intelligence_model::Entity>, usize)> {
    let matches = store.search(SearchQuery::new(query).with_limit(limit))?;
    let entities: Vec<_> = matches
        .into_iter()
        .map(|matched| matched.entity)
        .filter(|entity| filter.is_none_or(|kinds| kinds.contains(&entity.kind)))
        .collect();
    let count = entities.len();
    Ok((entities, count))
}
