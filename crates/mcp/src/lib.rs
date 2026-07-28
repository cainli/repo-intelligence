use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use repo_intelligence_config::IndexerConfig;
use repo_intelligence_analysis::{ImpactAnalyzer, WorkspaceIndexer};
use repo_intelligence_graph::{GraphStore, SqliteGraphStore};
use repo_intelligence_model::{
    ChangeRequest, Edge, EdgeKind, Entity, EntityId, EntityKind, EvidenceClass, SearchQuery,
    TraverseQuery,
};
use serde_json::{Value, json};

/// Default row cap for the text-search tools when the caller omits `limit`.
const DEFAULT_SEARCH_LIMIT: usize = 100;
/// Default hop depth for `trace_callers`/`trace_callees` when the caller omits
/// `depth`. Two hops answers "who calls this, and who calls them" without
/// flooding the result; raise it for deeper chains.
const DEFAULT_TRACE_DEPTH: usize = 2;

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
            "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0},
            "path": {"type": "array", "items": {"type": "string"}},
            "evidence": {"type": "array", "items": evidence_schema()}
        },
        "required": ["entity", "plane", "severity", "confidence", "path", "evidence"]
    })
}

fn edge_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "source": {"type": "string"},
            "target": {"type": "string"},
            "kind": {
                "type": "string",
                "enum": [
                    "contains", "declares", "calls", "exposes", "sends_http_request",
                    "matches_endpoint", "has_response_field", "serialized_from", "mapped_from",
                    "binds_to_statement", "executes_sql", "reads_table", "writes_table",
                    "reads_column", "writes_column", "depends_on", "submodule_of"
                ]
            },
            "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0},
            "tentative": {"type": "boolean"},
            "evidence": {"type": "array", "items": evidence_schema()}
        },
        "required": ["source", "target", "kind", "confidence", "tentative", "evidence"]
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
                "description": "Maximum number of entities to return in this page."
            },
            "offset": {
                "type": "integer",
                "minimum": 0,
                "default": 0,
                "description": "Number of matches to skip before the first returned row. Page through wide matches (e.g. an enterprise ID naming dozens of entities) with limit/offset instead of one huge batch."
            },
            "verbose": {
                "type": "boolean",
                "default": false,
                "description": "When true, return full entities (metadata + evidence[] with reason strings). Default false returns a compact {id, kind, name, qualified_name, evidence_count} view so a wide match stays small; pass verbose=true only for the few items you want to inspect."
            }
        },
        "required": ["query"]
    });
    let search_output = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "items": {"type": "array", "description": "Compact entity views by default; full entities when verbose=true."},
            "count": {"type": "integer", "minimum": 0, "description": "Number of items in this page (not the total match count)."},
            "limit": {"type": "integer", "minimum": 0},
            "offset": {"type": "integer", "minimum": 0},
            "has_more": {"type": "boolean", "description": "true if another page likely exists at offset+limit."},
            "hint": {"type": "string"}
        },
        "required": ["items", "count", "limit", "offset", "has_more"]
    });
    // The trace tools share one contract: an exact `name` to start from (with
    // optional `depth` and `edge_kinds`) in, and a `{items, edges, count,
    // start_count}` object out. Unlike the substring search tools, the start
    // point is resolved by exact name — `trace_callers("S27204")` traces that
    // entity, not the union of every `*27204*` substring hit. `hint` is set
    // only when no exact match exists, so an empty result reads as "no such
    // entity" rather than "tool broken".
    let trace_input = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "name": {
                "type": "string",
                "description": "Exact entity name to trace from (e.g. a class or method name). The start point is resolved by exact name match, not substring; use search_entities first if unsure of the exact name."
            },
            "depth": {
                "type": "integer",
                "minimum": 0,
                "default": 2,
                "description": "How many edge hops to follow. 0 returns only the start entity itself."
            },
            "edge_kinds": {
                "type": "array",
                "items": {
                    "type": "string",
                    "enum": [
                        "contains", "declares", "calls", "exposes", "sends_http_request",
                        "matches_endpoint", "has_response_field", "serialized_from", "mapped_from",
                        "binds_to_statement", "executes_sql", "reads_table", "writes_table",
                        "reads_column", "writes_column", "depends_on", "submodule_of"
                    ]
                },
                "default": ["calls"],
                "description": "Edge kinds to follow. Defaults to [\"calls\"] for a call chain; pass e.g. [\"depends_on\"] or [\"reads_table\",\"writes_table\"] to trace other dependency types."
            },
            "min_confidence": {
                "type": "number",
                "minimum": 0.0,
                "maximum": 1.0,
                "default": 0.0,
                "description": "Drop edges whose confidence is below this. Default 0 returns all edges (low-confidence ones still returned but marked `tentative`). Use e.g. 0.8 to keep only well-evidenced edges."
            }
        },
        "required": ["name"]
    });
    let trace_output = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "items": {"type": "array", "items": entity_schema()},
            "edges": {"type": "array", "items": edge_schema()},
            "count": {"type": "integer", "minimum": 0},
            "start_count": {"type": "integer", "minimum": 0},
            "hint": {"type": "string"}
        },
        "required": ["items", "edges", "count", "start_count"]
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
                    "files_extracted": {"type": "integer", "minimum": 0},
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
            name: "trace_callers",
            description: "Trace who calls an entity (inbound edges). Defaults to the `calls` edge for a call chain; set edge_kinds to follow other dependencies (depends_on, reads_table, ...). Resolves the start point by exact entity name, then BFS inward up to `depth`.",
            input_schema: trace_input.clone(),
            output_schema: trace_output.clone(),
        },
        ToolSpec {
            name: "trace_callees",
            description: "Trace what an entity calls (outbound edges). Defaults to the `calls` edge for a call chain; set edge_kinds to follow other dependencies (depends_on, reads_table, ...). Resolves the start point by exact entity name, then BFS outward up to `depth`.",
            input_schema: trace_input,
            output_schema: trace_output,
        },
        ToolSpec {
            name: "verify_edge",
            description: "Verify a graph edge against source code: read the source entity's file and grep for the target name. Returns matched lines (verified=true) or reports the edge is likely a cross-file inference (verified=false). Use to independently check a `tentative` edge from trace_callers/trace_callees before trusting it — graph edges are inferred; this grounds one in source.",
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "source": {
                        "type": "string",
                        "description": "Exact name of the edge's source entity. The file that gets grep'd is this entity's declared (evidence) file."
                    },
                    "target": {
                        "type": "string",
                        "description": "Name (or substring) of the target entity; matched literally inside the source file."
                    },
                    "workspace": {
                        "type": "string",
                        "default": ".",
                        "description": "Workspace root to resolve the source entity's file path. Same as scan_workspace."
                    }
                },
                "required": ["source", "target"]
            }),
            output_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "source": {"type": "string"},
                    "source_file": {"type": "string"},
                    "target": {"type": "string"},
                    "verified": {"type": "boolean"},
                    "match_count": {"type": "integer", "minimum": 0},
                    "matches": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "line": {"type": "integer", "minimum": 1},
                                "snippet": {"type": "string"}
                            }
                        }
                    },
                    "note": {"type": "string"}
                },
                "required": ["source", "target", "verified", "match_count", "matches"]
            }),
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
            let store = SqliteGraphStore::open(path)?;
            let mut result = run_search(&store, arguments, None)?;
            // On zero hits, distinguish "not found" from "empty index": the latter
            // points at a wrong database connection. Counts are only read on miss.
            if result["count"].as_u64() == Some(0) {
                let (total, _) = store.counts()?;
                if total == 0 {
                    result["hint"] = json!(
                        "no matches and the index is empty (entity_count == 0); \
                         run `scan_workspace` or verify the database path with `get_index_status`."
                    );
                }
            }
            result
        }
        "find_endpoint" => {
            let store = SqliteGraphStore::open(path)?;
            let endpoint_kinds = [
                EntityKind::HttpEndpoint,
                EntityKind::HttpClientCall,
                EntityKind::ApiField,
            ];
            let mut result = run_search(&store, arguments, Some(&endpoint_kinds))?;
            if result["count"].as_u64() == Some(0) {
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
            let store = SqliteGraphStore::open(path)?;
            let mut result = run_search(&store, arguments, None)?;
            if result["count"].as_u64() == Some(0) {
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
            let mut report = ImpactAnalyzer::new(&store).analyze(&change)?;
            // An empty index makes impact analysis meaningless — surface it so a
            // "zero findings" result isn't misread as "this change is safe".
            let (entity_count, _) = store.counts()?;
            if entity_count == 0 {
                report.open_questions.push(
                    "index is empty (entity_count == 0); this impact analysis is meaningless \
                     until `scan_workspace` populates the database — verify the connected \
                     database path with `get_index_status`."
                        .into(),
                );
            }
            serde_json::to_value(report)?
        }
        "trace_callers" => {
            let name = arguments["name"].as_str().unwrap_or_default();
            let depth = parse_depth(arguments, DEFAULT_TRACE_DEPTH);
            let kinds = parse_edge_kinds(arguments)?;
            let min_confidence = arguments["min_confidence"]
                .as_f64()
                .unwrap_or(0.0)
                .clamp(0.0, 1.0) as f32;
            let store = SqliteGraphStore::open(path)?;
            trace_graph(&store, name, depth, kinds, false, min_confidence)?
        }
        "trace_callees" => {
            let name = arguments["name"].as_str().unwrap_or_default();
            let depth = parse_depth(arguments, DEFAULT_TRACE_DEPTH);
            let kinds = parse_edge_kinds(arguments)?;
            let min_confidence = arguments["min_confidence"]
                .as_f64()
                .unwrap_or(0.0)
                .clamp(0.0, 1.0) as f32;
            let store = SqliteGraphStore::open(path)?;
            trace_graph(&store, name, depth, kinds, true, min_confidence)?
        }
        "verify_edge" => {
            let source = arguments["source"].as_str().unwrap_or_default();
            let target = arguments["target"].as_str().unwrap_or_default();
            let workspace = arguments["workspace"].as_str().unwrap_or(".");
            let store = SqliteGraphStore::open(path)?;
            verify_edge(&store, source, target, workspace)?
        }
        "scan_workspace" => {
            let workspace = arguments["workspace"].as_str().unwrap_or(".");
            let workspace_path = Path::new(workspace);
            // 配置跟 workspace 走:从 workspace 根目录发现 .repo-intelligence.toml,
            // 无文件则 builtin default(scan 行为与历史一致)。
            let config = IndexerConfig::load(workspace_path)?;
            let mut store = SqliteGraphStore::open(path)?;
            let summary =
                WorkspaceIndexer.scan_with_config(workspace_path, &mut store, &config, |_| {})?;
            // Echo the resulting kind distribution and the effective exclusion
            // list (builtin + configured extras) so a caller can spot a polluted
            // index (e.g. a worktree copy doubling every kind) from the scan
            // result alone, without re-querying.
            let entities_by_kind = store.counts_by_kind()?;
            json!({
                "files_indexed": summary.files_indexed,
                "files_extracted": summary.files_extracted,
                "files_added": summary.files_added,
                "files_changed": summary.files_changed,
                "files_deleted": summary.files_deleted,
                "files_unchanged": summary.files_unchanged,
                "entities_indexed": summary.entities_indexed,
                "edges_indexed": summary.edges_indexed,
                "entities_by_kind": entities_by_kind,
                "excluded_dirs": config.discovery.effective_excluded_dirs(),
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
            // Show a canonicalized absolute path so the caller can tell at a glance
            // whether the connected database is the one they expect. The default
            // (`.repo-intelligence/workspace.sqlite`) is relative to cwd, so a wrong
            // working dir silently lands on an empty database.
            let database = std::fs::canonicalize(path)
                .map(|resolved| resolved.display().to_string())
                .unwrap_or_else(|_| path.display().to_string());
            let mut status = json!({
                "database": database,
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

fn parse_offset(arguments: &Value) -> usize {
    arguments["offset"].as_u64().unwrap_or(0) as usize
}

/// Runs a name/qualified-name search and optionally narrows to a set of kinds.
/// `find_endpoint` uses the filter so it only ever returns endpoint entities —
/// never the unrelated classes/fields a plain `search_entities` would surface.
///
/// Peeks `limit + 1` rows so `has_more` is decided without a COUNT(*) round-trip:
/// if the (filtered) window holds more than `limit`, another page likely exists.
/// For `search_entities` (no filter) this is exact; for `find_endpoint` the window
/// is over pre-filter candidates, so `has_more` may under-report — but it never
/// drops data, it only stops paging a step early.
fn search_with_filter(
    store: &SqliteGraphStore,
    query: &str,
    limit: usize,
    offset: usize,
    filter: Option<&[EntityKind]>,
) -> Result<(Vec<repo_intelligence_model::Entity>, usize, bool)> {
    let peek = limit.saturating_add(1);
    let matches = store.search(
        SearchQuery::new(query)
            .with_limit(peek)
            .with_offset(offset),
    )?;
    let mut entities: Vec<_> = matches
        .into_iter()
        .map(|matched| matched.entity)
        .filter(|entity| filter.is_none_or(|kinds| kinds.contains(&entity.kind)))
        .collect();
    let has_more = entities.len() > limit;
    if has_more {
        entities.truncate(limit);
    }
    let count = entities.len();
    Ok((entities, count, has_more))
}

/// Shared body of the three substring-search tools (search_entities,
/// find_endpoint, analyze_requirement): parse query/limit/offset/verbose, run the
/// filtered search, and serialize items through the compact-or-verbose view. The
/// caller still owns the empty-result `hint` — each tool explains a miss differently
/// (empty index vs. non-Spring endpoint vs. non-substring query).
fn run_search(
    store: &SqliteGraphStore,
    arguments: &Value,
    filter: Option<&[EntityKind]>,
) -> Result<Value> {
    let query = arguments["query"].as_str().unwrap_or_default();
    let limit = parse_limit(arguments);
    let offset = parse_offset(arguments);
    let verbose = arguments["verbose"].as_bool().unwrap_or(false);
    let (entities, count, has_more) = search_with_filter(store, query, limit, offset, filter)?;
    let items: Vec<Value> = entities
        .iter()
        .map(|entity| entity_to_json(entity, verbose))
        .collect();
    Ok(json!({
        "items": items,
        "count": count,
        "limit": limit,
        "offset": offset,
        "has_more": has_more,
    }))
}

/// Serialize an entity for search results.
///
/// `verbose=false` (the default) emits a compact view: a wide substring match —
/// e.g. an enterprise ID that names dozens of fields/tables/methods — must not
/// dump every evidence `reason` (long natural-language strings) and the metadata
/// blob into one response. `verbose=true` returns the full entity for the few
/// items the caller actually wants to inspect.
fn entity_to_json(entity: &repo_intelligence_model::Entity, verbose: bool) -> Value {
    if verbose {
        serde_json::to_value(entity).unwrap_or(Value::Null)
    } else {
        // Compact view:id/kind/name/qualified_name + evidence_count(不展开 evidence[]
        // 的 reason 长串与 metadata blob)。一个宽匹配(如企业 ID 命中数十个实体)不致
        // 把响应撑到上万 token;verbose=true 时才返回完整实体。
        json!({
            "id": entity.id,
            "kind": entity.kind.as_str(),
            "name": entity.name,
            "qualified_name": entity.qualified_name,
            "evidence_count": entity.evidence.len(),
        })
    }
}

fn parse_depth(arguments: &Value, default: usize) -> usize {
    arguments["depth"]
        .as_u64()
        .map(|value| value as usize)
        .unwrap_or(default)
}

/// Resolve `edge_kinds` from arguments. Defaults to the `Calls` edge so the
/// trace tools answer "call chain" out of the box instead of dragging in
/// containment or table-read edges; an explicit array overrides it.
fn parse_edge_kinds(arguments: &Value) -> Result<Vec<EdgeKind>> {
    match &arguments["edge_kinds"] {
        Value::Null => Ok(vec![EdgeKind::Calls]),
        value => Ok(serde_json::from_value(value.clone())?),
    }
}

fn kinds_label(kinds: &[EdgeKind]) -> String {
    if kinds.is_empty() {
        "any kind".to_string()
    } else {
        kinds
            .iter()
            .copied()
            .map(EdgeKind::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Edge view with top-level `confidence` + `tentative` so an agent can tell Fact
/// edges (trust) from inferred ones at a glance, without digging into `evidence[]`.
/// `tentative` = not Fact, and (confidence < 0.8 or classification is
/// Inferred/RuntimeUnknown/missing). Full `evidence[]` is still preserved.
fn edge_view(edge: &Edge) -> Value {
    let evidence = edge.evidence.first();
    let confidence = evidence.map(|item| item.confidence).unwrap_or(1.0);
    let tentative = match evidence.map(|item| item.classification) {
        Some(EvidenceClass::Fact) => false,
        Some(EvidenceClass::Resolved) => confidence < 0.8,
        Some(_) | None => true,
    };
    json!({
        "source": edge.source.0,
        "target": edge.target.0,
        "kind": edge.kind.as_str(),
        "confidence": confidence,
        "tentative": tentative,
        "evidence": serde_json::to_value(&edge.evidence).unwrap_or_default(),
    })
}

/// Shared engine for `trace_callers`/`trace_callees`: resolve every entity
/// whose name equals `name` exactly as a start point, then BFS along
/// `outbound` edges of the requested kinds and return the visited entities and
/// edges.
///
/// Exact-name (not substring) matching keeps `trace_callers("S27204")` from
/// pulling in `S27204Req`/`S27204Resp` callers — it mirrors how
/// `analyze_change` resolves its target and matches the "trace *this* entity"
/// intent. Multiple same-named entities each seed the walk; results merge and
/// dedupe so a polymorphic/overloaded name still resolves in one call.
fn trace_graph(
    store: &SqliteGraphStore,
    name: &str,
    depth: usize,
    edge_kinds: Vec<EdgeKind>,
    outbound: bool,
    min_confidence: f32,
) -> Result<Value> {
    let matches = store.search(SearchQuery::new(name).with_limit(DEFAULT_SEARCH_LIMIT))?;
    let starts: Vec<Entity> = matches
        .into_iter()
        .map(|matched| matched.entity)
        .filter(|entity| entity.name == name)
        .collect();
    if starts.is_empty() {
        return Ok(json!({
            "items": [],
            "edges": [],
            "count": 0,
            "start_count": 0,
            "hint": format!(
                "No entity is exactly named `{name}`. The start point is resolved by exact \
                 name (not substring), then the {direction} edges of kind {kinds} are walked. \
                 Run search_entities to find the precise identifier.",
                direction = if outbound { "outbound (callees)" } else { "inbound (callers)" },
                kinds = kinds_label(&edge_kinds),
            ),
        }));
    }
    let mut entities: HashMap<EntityId, Entity> = HashMap::new();
    let mut edges: Vec<Edge> = Vec::new();
    let mut seen_edge: HashSet<(EntityId, EntityId, EdgeKind)> = HashSet::new();
    for start in &starts {
        let traversal = store.traverse(TraverseQuery {
            start: start.id.clone(),
            outbound,
            max_depth: depth,
            edge_kinds: edge_kinds.clone(),
        })?;
        for entity in traversal.entities {
            entities.insert(entity.id.clone(), entity);
        }
        for edge in traversal.edges {
            let key = (edge.source.clone(), edge.target.clone(), edge.kind);
            if seen_edge.insert(key) {
                edges.push(edge);
            }
        }
    }
    // Deterministic ordering: HashMap/HashSet iteration order is random, which
    // would make the JSON (and any test asserting on it) non-reproducible.
    let mut items = entities.into_values().collect::<Vec<_>>();
    items.sort_by(|a, b| {
        a.qualified_name
            .cmp(&b.qualified_name)
            .then_with(|| a.id.0.cmp(&b.id.0))
    });
    edges.sort_by(|a, b| {
        (a.source.0.as_str(), a.target.0.as_str(), a.kind.as_str()).cmp(&(
            b.source.0.as_str(),
            b.target.0.as_str(),
            b.kind.as_str(),
        ))
    });
    // Wrap with edge_view (top-level confidence/tentative) and drop edges below
    // `min_confidence`. Conservative: default 0 keeps everything, low-confidence
    // edges are marked `tentative` rather than silently hidden.
    let edge_views: Vec<Value> = edges
        .iter()
        .map(edge_view)
        .filter(|view| view["confidence"].as_f64().unwrap_or(1.0) >= min_confidence as f64)
        .collect();
    Ok(json!({
        "items": serde_json::to_value(&items)?,
        "edges": edge_views,
        "count": items.len(),
        "start_count": starts.len(),
    }))
}

/// `verify_edge`: ground a graph edge in source code. Resolve the source entity
/// (exact name) → its declared file → read it from `workspace` → grep the target
/// name line by line. A hit (`verified=true`) is independent evidence the
/// reference exists; a miss honestly signals the edge is a cross-file inference
/// (resolved by name, e.g. matches_endpoint/mapped_from) or indirect — not a
/// literal source reference.
fn verify_edge(
    store: &SqliteGraphStore,
    source: &str,
    target: &str,
    workspace: &str,
) -> Result<Value> {
    let source_entity = store
        .search_exact_name(source, 1)?
        .into_iter()
        .next()
        .ok_or_else(|| {
            anyhow!(
                "No entity is exactly named `{source}`. verify_edge resolves the source by exact \
                 name; run search_entities first to find the precise identifier."
            )
        })?;
    let Some(source_file) = source_entity.evidence.first().map(|item| item.file.clone()) else {
        return Ok(json!({
            "source": source,
            "target": target,
            "verified": false,
            "match_count": 0,
            "matches": [],
            "note": format!("Source `{source}` has no evidence file recorded; cannot read its source.")
        }));
    };
    let absolute = Path::new(workspace).join(&source_file);
    let content = std::fs::read_to_string(&absolute)
        .with_context(|| format!("read source file {}", absolute.display()))?;
    let mut matches = Vec::new();
    for (index, line) in content.lines().enumerate() {
        if line.contains(target) {
            matches.push(json!({"line": index + 1, "snippet": line.trim()}));
        }
    }
    let verified = !matches.is_empty();
    let note = if verified {
        format!(
            "`{target}` appears in `{source_file}` — consistent with the edge. Substring match is \
             a heuristic; still confirm the reference is semantic, not a comment or string literal."
        )
    } else {
        format!(
            "`{target}` not found in `{source_file}` — the edge is likely a cross-file inference \
             (e.g. matches_endpoint/mapped_from resolved by name) or an indirect reference. Treat \
             it as unverified."
        )
    };
    Ok(json!({
        "source": source,
        "source_file": source_file,
        "target": target,
        "verified": verified,
        "match_count": matches.len(),
        "matches": matches,
        "note": note
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use repo_intelligence_model::GraphPatch;
    use std::fs;

    fn id(value: &str) -> EntityId {
        EntityId(value.to_string())
    }

    #[test]
    fn edge_view_marks_tentative_by_classification_and_confidence() {
        // Fact → 不 tentative
        let fact = Edge::new(id("a"), id("b"), EdgeKind::Calls)
            .with_evidence("F.java", 1, 1, EvidenceClass::Fact, 1.0, "fact");
        assert_eq!(edge_view(&fact)["tentative"], false);
        assert_eq!(edge_view(&fact)["confidence"], 1.0);
        // Inferred → 无论置信度都 tentative
        let inferred = Edge::new(id("a"), id("b"), EdgeKind::MappedFrom)
            .with_evidence("F.java", 1, 1, EvidenceClass::Inferred, 0.9, "inferred");
        assert_eq!(edge_view(&inferred)["tentative"], true);
        // Resolved 高置信 → 不 tentative
        let resolved_hi = Edge::new(id("a"), id("b"), EdgeKind::MatchesEndpoint)
            .with_evidence("F.java", 1, 1, EvidenceClass::Resolved, 0.95, "resolved");
        assert_eq!(edge_view(&resolved_hi)["tentative"], false);
        // Resolved 低置信 → tentative
        let resolved_lo = Edge::new(id("a"), id("b"), EdgeKind::MatchesEndpoint)
            .with_evidence("F.java", 1, 1, EvidenceClass::Resolved, 0.6, "low");
        assert_eq!(edge_view(&resolved_lo)["tentative"], true);
        // 无证据 → tentative
        let no_evidence = Edge::new(id("a"), id("b"), EdgeKind::Calls);
        assert_eq!(edge_view(&no_evidence)["tentative"], true);
    }

    #[test]
    fn verify_edge_greps_source_file_for_target() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("Svc.java"),
            "class Svc {\n  void doIt() { helper.run(); }\n}\n",
        )
        .unwrap();
        let mut store = SqliteGraphStore::open_in_memory().unwrap();
        // 手动塞一个 Svc 实体,evidence.file 指向真实文件(不依赖 extractor 细节)。
        let svc = Entity::new(
            EntityId::stable("w", "Svc.java", EntityKind::Class, "Svc", ""),
            EntityKind::Class,
            "Svc",
            "Svc",
        )
        .with_evidence("Svc.java", 1, 1, EvidenceClass::Fact, 1.0, "declared");
        store
            .apply_patch(GraphPatch::add(vec![svc], vec![]))
            .unwrap();

        let root = dir.path().to_str().unwrap();
        let hit = verify_edge(&store, "Svc", "helper", root).unwrap();
        assert_eq!(hit["verified"], true, "helper 出现在 Svc.java");
        assert!(hit["match_count"].as_u64().unwrap() >= 1);
        let miss = verify_edge(&store, "Svc", "nonexistentXYZ", root).unwrap();
        assert_eq!(miss["verified"], false, "不存在的 target 未命中");
    }
}
