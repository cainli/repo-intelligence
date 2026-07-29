use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use repo_intelligence_config::IndexerConfig;
use repo_intelligence_analysis::{ImpactAnalyzer, WorkspaceIndexer};
use repo_intelligence_graph::{GraphStore, SqliteGraphStore};
use repo_intelligence_model::{
    ChangeRequest, Edge, EdgeKind, Entity, EntityId, EntityKind, Evidence, EvidenceClass,
    SearchQuery, TraverseQuery,
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
                    "reads_column", "writes_column", "depends_on", "injects", "submodule_of"
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
            },
            "kind": {
                "type": "array",
                "items": {"type": "string"},
                "description": "Restrict matches to these entity kinds, e.g. [\"class\",\"method\"] or [\"spring_bean\"]. Default: any kind. Use to filter out field/column noise when a wide identifier (an enterprise ID that names many fields of a Req/Resp class) otherwise matches dozens of low-value entities."
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
                        "reads_column", "writes_column", "depends_on", "injects", "submodule_of"
                    ]
                },
                "default": ["calls", "injects"],
                "description": "Edge kinds to follow. Defaults to [\"calls\", \"injects\"] so a call chain includes Spring bean injection (@Autowired/@Resource) — the dominant cross-file link in Java business code, since cross-file method calls are inferred from injected types. Pass e.g. [\"depends_on\"] for table deps or [\"reads_table\",\"writes_table\"] for data flow."
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
            description: "Trace who calls an entity (inbound edges). Defaults to `calls` + `injects` so the chain covers both method calls and Spring bean injection (@Autowired) — the dominant cross-file link in Java business code. Resolves the start point by exact entity name, then BFS inward up to `depth`. Cross-file calls/injections are low-confidence inferences; use verify_edge to ground a `tentative` edge in source before trusting it. Set edge_kinds to follow other dependencies (depends_on, reads_table, ...).",
            input_schema: trace_input.clone(),
            output_schema: trace_output.clone(),
        },
        ToolSpec {
            name: "trace_callees",
            description: "Trace what an entity calls (outbound edges). Defaults to `calls` + `injects` so the chain covers both method calls and Spring bean injection (@Autowired) — the dominant cross-file link in Java business code. Resolves the start point by exact entity name, then BFS outward up to `depth`. Cross-file calls/injections are low-confidence inferences; use verify_edge to ground a `tentative` edge in source before trusting it. Set edge_kinds to follow other dependencies (depends_on, reads_table, ...).",
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
        ToolSpec {
            name: "build_relay_doc",
            description: "Build a structured 'relay doc' skeleton (relay-schema v1) around a target entity, resolved by exact qualified name. Collects inbound (who points at it) and outbound (what it points at) edges, each with a call-site anchor and a machine-mapped edge_type. Fills the structure layer (qn, file:line anchors, tool edge_kind → edge_type); semantic fields are marked `custom:needs-review` for the consuming agent. Known limit: Java `calls` edges are extracted only within the same file, so cross-file inbound callers may be missing.",
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "qn": {
                        "type": "string",
                        "description": "Exact qualified name of the target entity. Resolved by exact qualified_name match (not substring); use search_entities first if unsure of the precise qn."
                    },
                    "depth": {
                        "type": "integer",
                        "minimum": 0,
                        "default": 1,
                        "description": "Edge hops to follow. 1 = direct neighbors only (the relay default)."
                    },
                    "verbose": {
                        "type": "boolean",
                        "default": false,
                        "description": "When true, include full evidence[] on each edge. Default false keeps the skeleton small."
                    }
                },
                "required": ["qn"]
            }),
            output_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "schema_version": {"type": "string"},
                    "target": {"type": "object"},
                    "edges": {
                        "type": "object",
                        "properties": {
                            "inbound": {"type": "array"},
                            "outbound": {"type": "array"}
                        }
                    },
                    "related": {"type": "object"},
                    "hint": {"type": "string"}
                },
                "required": ["schema_version", "target", "edges"]
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
        "build_relay_doc" => {
            let qn = arguments["qn"].as_str().unwrap_or_default();
            let depth = parse_depth(arguments, 1);
            let verbose = arguments["verbose"].as_bool().unwrap_or(false);
            let store = SqliteGraphStore::open(path)?;
            build_relay(&store, qn, depth, verbose)?
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
    // 外部硬编码 filter(find_endpoint 的 endpoint kinds)优先;否则读调用方传入的 `kind`,
    // 让 search_entities 能按 kind 过滤,切掉宽标识符(如企业交易码)命中的 field/column 噪声。
    let parsed_kinds = parse_entity_kinds(arguments)?;
    let (entities, count, has_more) =
        search_with_filter(store, query, limit, offset, filter.or(parsed_kinds.as_deref()))?;
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

/// 解析 `kind` 参数(单个 string 或数组)为 EntityKind 列表;None = 不过滤。复用 EntityKind
/// 的 serde(rename_all="snake_case"),接受 "class"/"method"/"spring_bean" 等 snake_case 名。
fn parse_entity_kinds(arguments: &Value) -> Result<Option<Vec<EntityKind>>> {
    match &arguments["kind"] {
        Value::Null => Ok(None),
        Value::String(_) => Ok(Some(vec![serde_json::from_value(arguments["kind"].clone())?])),
        Value::Array(items) => Ok(Some(
            items
                .iter()
                .map(|item| serde_json::from_value::<EntityKind>(item.clone()))
                .collect::<Result<_, _>>()?,
        )),
        other => anyhow::bail!(
            "`kind` must be a string or array of entity-kind names (e.g. \"class\"), got {other}"
        ),
    }
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
        Value::Null => Ok(vec![EdgeKind::Calls, EdgeKind::Injects]),
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
    let matched: Vec<Entity> = matches.into_iter().map(|m| m.entity).collect();
    // 优先按 qualified_name 精确解析,让调用方能消歧同名实体(mos 的 S27204 入口 vs mes 的
    // S27204 处理器);无 qn 精确命中再回退到 name 精确匹配。
    let mut starts: Vec<Entity> = matched
        .iter()
        .filter(|entity| entity.qualified_name == name)
        .cloned()
        .collect();
    if starts.is_empty() {
        starts = matched
            .into_iter()
            .filter(|entity| entity.name == name)
            .collect();
    }
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
    let edges_empty = edge_views.is_empty();
    let direction = if outbound {
        "outbound (callees)"
    } else {
        "inbound (callers)"
    };
    let mut result = json!({
        "items": serde_json::to_value(&items)?,
        "edges": edge_views,
        "count": items.len(),
        "start_count": starts.len(),
    });
    // 多同名起点是调用链最关键的分叉点(mos 的 S27204 入口 vs mes 的 S27204 处理器)。
    // 合并 trace 之外,显式列出每个起点的 qualified_name/kind/file,让调用方能按 qn 精确重查。
    if starts.len() > 1 {
        let candidates = starts
            .iter()
            .map(|entity| {
                let file = entity
                    .evidence
                    .first()
                    .map(|item| item.file.as_str())
                    .unwrap_or("?");
                format!(
                    "  • {} [{}] @ {}",
                    entity.qualified_name,
                    entity.kind.as_str(),
                    file
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        result["hint"] = json!(format!(
            "Multiple ({n}) entities are named `{name}` — results merge all of them. These are \
             likely different classes sharing an ID; re-run with the exact `qualified_name` to \
             trace just one:\n{candidates}",
            n = starts.len(),
        ));
    } else if edges_empty {
        // 单起点但无边:方向性靠边的朝向体现,零边时 inbound/outbound 退化为同一份起点集。
        result["hint"] = json!(format!(
            "No {direction} edges of kind {kinds} reachable from the {n} start point(s). \
             Direction is carried by edge orientation — with zero edges, inbound and outbound \
             both reduce to the same start set. Pass edge_kinds to follow other dependency types.",
            kinds = kinds_label(&edge_kinds),
            n = starts.len(),
        ));
    }
    Ok(result)
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

/// Relay-schema 生产者(`build_relay_doc` 端点与 `relay` CLI 共用):把目标(按
/// `qualified_name` 精确定位)周围的边聚合成 "agent 接力文档" 骨架(schema v1)。
///
/// 机器填结构层 —— qn、调用点 anchor(`edge.evidence[0]` 的 file:line)、工具原生
/// edge_kind → edge_type 机械映射;语义层(bean 实例 id / interface / business /
/// framework_dispatch / inject_dead / 跨文件调用链)图里没有,留 `custom:needs-review`
/// 给消费 agent 补。**机器填结构,agent 填语义。**
///
/// 入站(outbound=false,谁指向 target)与出站(outbound=true,target 指向谁)各跑一次
/// `traverse`,沿接力相关 edge_kinds,过滤纯结构边(contains/declares/exposes/submodule_of)。
/// 每条边的 peer(对端)qn 经一次 `all_entities()` join 出。
///
/// 已知限制(写入 hint):Java `calls` 边只在同文件内提取 —— 被跨文件调度的目标
/// (如 Spring bean 入口)inbound 可能不全,这正是 agent 要补的部分。
pub fn build_relay(
    store: &SqliteGraphStore,
    qn: &str,
    depth: usize,
    verbose: bool,
) -> Result<Value> {
    let target = store
        .search(SearchQuery::new(qn).with_limit(DEFAULT_SEARCH_LIMIT))?
        .into_iter()
        .map(|matched| matched.entity)
        .find(|entity| entity.qualified_name == qn)
        .ok_or_else(|| {
            anyhow!(
                "No entity has qualified_name == `{qn}`. build_relay_doc resolves the target by \
                 exact qualified name (not substring); run search_entities to find the precise qn."
            )
        })?;

    // 接力相关 edge_kinds:调用/依赖/HTTP/DB/字段传播。排除纯结构边(contains 等)——
    // 它们只表达归属,不表达"谁进来/我调谁",拖进 relay 只是噪音。
    let relay_kinds: Vec<EdgeKind> = vec![
        EdgeKind::Calls,
        EdgeKind::DependsOn,
        EdgeKind::Injects,
        EdgeKind::Declares,
        EdgeKind::Exposes,
        EdgeKind::MatchesEndpoint,
        EdgeKind::SendsHttpRequest,
        EdgeKind::ReadsTable,
        EdgeKind::WritesTable,
        EdgeKind::ReadsColumn,
        EdgeKind::WritesColumn,
        EdgeKind::MappedFrom,
    ];
    let inbound = store.traverse(TraverseQuery {
        start: target.id.clone(),
        outbound: false,
        max_depth: depth,
        edge_kinds: relay_kinds.clone(),
    })?;
    let outbound = store.traverse(TraverseQuery {
        start: target.id.clone(),
        outbound: true,
        max_depth: depth,
        edge_kinds: relay_kinds,
    })?;

    // 一次 all_entities 建 id→entity 索引,为每条边解出对端 qn/short/kind(无批量
    // get_entity,这是 analysis 层 ImpactAnalyzer 同款 join 模式)。
    let index: HashMap<EntityId, Entity> = store
        .all_entities()?
        .into_iter()
        .map(|entity| (entity.id.clone(), entity))
        .collect();

    // inbound 的对端是调用方(edge.source);outbound 的对端是被调方(edge.target)。
    let inbound_edges: Vec<Value> = inbound
        .edges
        .iter()
        .map(|edge| relay_edge_json(edge, &edge.source, &index, verbose))
        .collect();
    let outbound_edges: Vec<Value> = outbound
        .edges
        .iter()
        .map(|edge| relay_edge_json(edge, &edge.target, &index, verbose))
        .collect();

    // related.homonyms:同名(qn 不同)实体,防 agent 把同名不同物误连。
    let homonyms: Vec<Value> = store
        .search_exact_name(&target.name, DEFAULT_SEARCH_LIMIT)?
        .into_iter()
        .filter(|entity| entity.id != target.id)
        .map(|entity| homonym_json(&entity))
        .collect();

    Ok(json!({
        "schema_version": "1",
        "target": target_json(&target),
        "edges": {
            "inbound": inbound_edges,
            "outbound": outbound_edges,
        },
        "related": { "homonyms": homonyms },
        "hint": "Machine-filled skeleton (structure layer). Fields marked `custom:needs-review` \
                 must be completed by the consuming agent. Known limit: Java `calls` edges are \
                 extracted only within the same file, so cross-file callers (inbound) may be \
                 missing; framework_dispatch / inject_dead / business / bean(instance id) are \
                 not derivable from the graph."
    }))
}

/// 一条 relay 边:peer(对端 qn/short)、edge_type(机械映射)、原生 edge_kind、
/// layer、调用点 anchor。`peer` 在 inbound=调用方(source)、outbound=被调方(target)。
fn relay_edge_json(
    edge: &Edge,
    peer: &EntityId,
    index: &HashMap<EntityId, Entity>,
    verbose: bool,
) -> Value {
    let peer_entity = index.get(peer);
    let peer_kind = peer_entity.map(|entity| entity.kind);
    let mut view = json!({
        "peer": {
            "qn": peer_entity.map(|e| e.qualified_name.clone()).unwrap_or_default(),
            "short": peer_entity.map(|e| e.name.clone()).unwrap_or_default(),
        },
        "edge_type": relay_edge_type(edge.kind, peer_kind),
        "edge_kind": edge.kind.as_str(),
        "layer": peer_kind.map(relay_layer).unwrap_or("custom:needs-review"),
        "anchor": anchor_json(edge.evidence.first()),
    });
    if verbose {
        view["evidence"] = serde_json::to_value(&edge.evidence).unwrap_or_default();
    }
    view
}

/// 工具原生 EdgeKind → relay edge_type。能机械映射的映射,映射不了标 needs-review。
fn relay_edge_type(kind: EdgeKind, peer_kind: Option<EntityKind>) -> String {
    use EdgeKind as E;
    use EntityKind as K;
    match kind {
        E::ReadsTable | E::ExecutesSql | E::ReadsColumn | E::BindsToStatement => "db_read",
        E::WritesTable | E::WritesColumn => "db_write",
        E::MatchesEndpoint | E::SendsHttpRequest => "http_out",
        E::MappedFrom => "field_propagate",
        E::Calls => "call",
        E::Declares => "declares",
        E::Exposes => match peer_kind {
            // Controller 方法暴露的 HTTP 端点
            Some(K::HttpEndpoint) => "http_out",
            // @Bean 工厂方法暴露的 SpringBean
            Some(K::SpringBean) => "exposes",
            _ => "custom:needs-review",
        },
        E::Injects => "inject",
        E::DependsOn => match peer_kind {
            // 依赖表/列/库(@TableName、Mapper→Table),视作读侧
            Some(K::Table | K::Column | K::Database) => "db_read",
            _ => "custom:needs-review",
        },
        _ => "custom:needs-review",
    }
    .to_string()
}

/// 被调方(对端)实体类型 → 架构层。粗粒度,消费方可按 layer 聚合。
fn relay_layer(kind: EntityKind) -> &'static str {
    use EntityKind as K;
    match kind {
        K::Table | K::Column | K::Database | K::Datasource | K::Mapper | K::MapperMethod
        | K::XmlStatement | K::ResultMap | K::SqlField => "db_mapper",
        K::HttpEndpoint | K::HttpClientCall | K::ApiField => "remote",
        K::SpringBean | K::Class | K::Interface | K::Method | K::Field => "domain",
        _ => "infra",
    }
}

/// 调用点/定义点 anchor:file + line(单行 int,多行 [start,end])。无 evidence → needs-review。
/// 目前提取器 end_line==start_line,故多为单行;结构支持范围,留待提取器补全。
fn anchor_json(evidence: Option<&Evidence>) -> Value {
    match evidence {
        Some(ev) if ev.end_line > ev.start_line => {
            json!({"file": ev.file, "line": [ev.start_line, ev.end_line]})
        }
        Some(ev) => json!({"file": ev.file, "line": ev.start_line}),
        None => json!({"needs_review": "no evidence recorded"}),
    }
}

fn target_json(target: &Entity) -> Value {
    json!({
        "qn": target.qualified_name,
        "short": target.name,
        "kind": target.kind.as_str(),
        "anchor": anchor_json(target.evidence.first()),
        // 语义层:图里无数据,显式标 needs-review 让 agent 补。
        "bean": "custom:needs-review",
        "interface": "custom:needs-review",
        "business": "custom:needs-review",
    })
}

fn homonym_json(entity: &Entity) -> Value {
    json!({
        "qn": entity.qualified_name,
        "short": entity.name,
        "kind": entity.kind.as_str(),
        "anchor": anchor_json(entity.evidence.first()),
    })
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

    #[test]
    fn build_relay_fills_skeleton_and_filters_structural_edges() {
        use repo_intelligence_model::GraphPatch;
        let mut store = SqliteGraphStore::open_in_memory().unwrap();

        let svc = Entity::new(id("svc"), EntityKind::Class, "Svc", "com.example.Svc")
            .with_evidence("Svc.java", 10, 10, EvidenceClass::Fact, 1.0, "declared");
        let caller =
            Entity::new(id("caller"), EntityKind::Class, "Caller", "com.example.Caller")
                .with_evidence("Caller.java", 1, 1, EvidenceClass::Fact, 1.0, "declared");
        let mapper = Entity::new(id("mapper"), EntityKind::Mapper, "UserMapper", "com.example.UserMapper")
            .with_evidence("UserMapper.java", 1, 1, EvidenceClass::Fact, 1.0, "mapper");
        let field1 = Entity::new(id("field1"), EntityKind::Field, "field1", "com.example.Svc.field1")
            .with_evidence("Svc.java", 20, 20, EvidenceClass::Fact, 1.0, "field");

        let edges = vec![
            // inbound: Caller → Svc(call)
            Edge::new(id("caller"), id("svc"), EdgeKind::Calls)
                .with_evidence("Caller.java", 5, 5, EvidenceClass::Inferred, 0.7, "call"),
            // outbound: Svc → Mapper(db_read)
            Edge::new(id("svc"), id("mapper"), EdgeKind::ReadsTable)
                .with_evidence("Svc.java", 12, 12, EvidenceClass::Fact, 1.0, "reads"),
            // 结构边:应被 relay_kinds 过滤,不进 outbound
            Edge::new(id("svc"), id("field1"), EdgeKind::Contains)
                .with_evidence("Svc.java", 20, 20, EvidenceClass::Fact, 1.0, "contains"),
        ];
        store
            .apply_patch(GraphPatch::add(vec![svc, caller, mapper, field1], edges))
            .unwrap();

        let doc = build_relay(&store, "com.example.Svc", 1, false).unwrap();

        // target 结构层自动填 + 语义层 needs-review
        assert_eq!(doc["target"]["qn"], "com.example.Svc");
        assert_eq!(doc["target"]["bean"], "custom:needs-review");
        assert_eq!(doc["target"]["business"], "custom:needs-review");
        assert_eq!(doc["target"]["anchor"]["file"], "Svc.java");
        assert_eq!(doc["target"]["anchor"]["line"], 10);

        // inbound: 一条 call 边,peer=caller,anchor=调用点
        let inbound = doc["edges"]["inbound"].as_array().unwrap();
        assert_eq!(inbound.len(), 1);
        assert_eq!(inbound[0]["edge_type"], "call");
        assert_eq!(inbound[0]["edge_kind"], "calls");
        assert_eq!(inbound[0]["peer"]["qn"], "com.example.Caller");
        assert_eq!(inbound[0]["anchor"]["file"], "Caller.java");
        assert_eq!(inbound[0]["anchor"]["line"], 5);

        // outbound: 仅 db_read(ReadsTable),Contains 被过滤
        let outbound = doc["edges"]["outbound"].as_array().unwrap();
        assert_eq!(outbound.len(), 1, "Contains 结构边应被过滤");
        assert_eq!(outbound[0]["edge_type"], "db_read");
        assert_eq!(outbound[0]["edge_kind"], "reads_table");
        assert_eq!(outbound[0]["peer"]["qn"], "com.example.UserMapper");
        assert_eq!(outbound[0]["layer"], "db_mapper");

        // hint 标注语义层需 agent 补
        assert!(doc["hint"].as_str().unwrap().contains("needs-review"));
    }

    #[test]
    fn build_relay_errors_on_unknown_qn() {
        let store = SqliteGraphStore::open_in_memory().unwrap();
        let result = build_relay(&store, "does.not.Exist", 1, false);
        assert!(result.is_err(), "未知 qn 应返回 Err 而非空骨架");
    }
}
