use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use repo_intelligence_analysis::{ImpactAnalyzer, ScanPhase, ScanProgress, WorkspaceIndexer};
use repo_intelligence_config::IndexerConfig;
use repo_intelligence_graph::{GraphStore, SqliteGraphStore};
use repo_intelligence_mcp::build_relay;
use repo_intelligence_model::{
    ChangeOperation, ChangeRequest, Entity, EntityId, EntityKind, SearchQuery,
};
use repo_intelligence_protocol::Envelope;

#[derive(Parser)]
#[command(
    name = "repo-intelligence",
    version,
    about = "Local cross-stack repository intelligence"
)]
struct Cli {
    #[arg(
        long,
        default_value = ".repo-intelligence/workspace.sqlite",
        global = true
    )]
    database: PathBuf,
    /// 多仓库 base 目录(一个 MCP server 管多库);默认 ~/.repo-intelligence/。
    /// Mcp 模式下,工具带 repository 参数时路由到 <base>/repos/<id>.sqlite。
    #[arg(long, global = true)]
    base: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Init {
        #[arg(default_value = ".")]
        workspace: PathBuf,
        #[arg(long, value_enum, default_value = "text")]
        format: OutputFormat,
    },
    Scan {
        #[arg(default_value = ".")]
        workspace: PathBuf,
        #[arg(long, value_enum, default_value = "text")]
        format: OutputFormat,
    },
    Search {
        query: String,
        #[arg(long, value_enum, default_value = "text")]
        format: OutputFormat,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    Impact {
        /// ChangeRequest JSON 文件(示例见 README「变更影响面」);与 --entity 二选一。
        #[arg(long, conflicts_with = "entity")]
        request: Option<PathBuf>,
        /// 快捷入口:直接给实体精确名,免手写 JSON。默认按 change_semantics 评估影响面。
        #[arg(long)]
        entity: Option<String>,
        /// 与 --entity 搭配的变更操作(snake_case,默认 change_semantics)。
        #[arg(long, default_value = "change_semantics")]
        operation: String,
        #[arg(long, value_enum, default_value = "text")]
        format: OutputFormat,
        /// 输出完整 report(实体带 metadata、全量 evidence);默认紧凑档
        /// (对齐 MCP trace/query 紧凑视图,ruoyi 实测 27.8KB → 约 2KB)。
        #[arg(long)]
        verbose: bool,
    },
    Status {
        #[arg(long, value_enum, default_value = "text")]
        format: OutputFormat,
    },
    Overview {
        #[arg(long, default_value = "repositories")]
        view: String,
        #[arg(long, value_enum, default_value = "text")]
        format: OutputFormat,
    },
    Doctor {
        #[arg(long, value_enum, default_value = "text")]
        format: OutputFormat,
    },
    /// Build a relay-schema skeleton around an entity, resolved by exact qualified
    /// name. Same skeleton as the `build_relay_doc` MCP tool; semantic fields are
    /// `custom:needs-review` for a consuming agent to fill.
    Relay {
        qn: String,
        #[arg(long, default_value_t = 1)]
        depth: usize,
        #[arg(long, value_enum, default_value = "json")]
        format: OutputFormat,
    },
    /// 语义检索:本地 embedding 模型找语义相近的实体(对标 MCP semantic_search)。
    /// 对"按意思找"的查询(如搜 authenticate 命中 login 方法)比子串 search 更准。
    SemanticSearch {
        query: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
        #[arg(long, value_enum, default_value = "text")]
        format: OutputFormat,
    },
    /// 找复杂度热点(对标 codebase-memory Q4):按 metric 排序的 top-N 方法。
    /// metric 默认 transitive_loop_depth(调用链最坏嵌套度,跨函数 O(n²) 探测器)。
    Hotspots {
        /// 排序指标:transitive_loop_depth(默认)/ complexity / loop_depth / linear_scan_in_loop。
        metric: Option<String>,
        #[arg(long, default_value_t = 10)]
        limit: usize,
        #[arg(long)]
        min_value: Option<u64>,
        #[arg(long, value_enum, default_value = "json")]
        format: OutputFormat,
    },
    /// 跨库 HTTP 链路对齐(P2):前端库 http_client_call ↔ 后端库 http_endpoint。
    /// (method, path) 精确匹配——两侧提取时已把 {}/${}/数字参数归一为 `{}`。
    /// 输出匹配对(同端点的前端调用位置聚合)与未匹配清单。
    HttpJoin {
        /// 前端库(sqlite,含 http_client_call 实体)
        frontend: PathBuf,
        /// 后端库(sqlite,含 http_endpoint 实体)
        backend: PathBuf,
        #[arg(long, value_enum, default_value = "text")]
        format: OutputFormat,
        /// 未匹配前端调用的展示上限
        #[arg(long, default_value_t = 20)]
        show_unmatched: usize,
    },
    Mcp,
}

#[derive(Clone, Copy, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
    Yaml,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("repo-intelligence: {error:#}");
        std::process::exit(1);
    }
}

/// 默认多仓库 base 目录:~/.repo-intelligence/(从 HOME 派生,跨用户隔离)。
fn default_base() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".repo-intelligence")
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    // 裸文件名(如 --database my.sqlite)的 parent() 返回 Some(""),create_dir_all("")
    // 会失败且错误信息是空串。仅当 parent 非空(确有目录成分)时才建目录。
    if let Some(parent) = cli.database.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("create data directory {}", parent.display()))?;
    }
    match cli.command {
        Command::Mcp => {
            let base = cli.base.clone().unwrap_or_else(default_base);
            repo_intelligence_mcp::serve(
                io::stdin().lock(),
                io::stdout().lock(),
                Some(&cli.database),
                &base,
            )
        }
        Command::Init { workspace, format } => {
            let _store = SqliteGraphStore::open(&cli.database)?;
            emit(
                format,
                serde_json::json!({
                    "workspace": workspace,
                    "database": cli.database,
                    "initialized": true
                }),
            )
        }
        Command::Scan { workspace, format } => {
            // 配置跟 workspace 走:从 workspace 根目录发现 .repo-intelligence.toml,
            // 无文件则 builtin default(scan 行为与历史一致)。
            let config = IndexerConfig::load(&workspace)?;
            let mut store =
                SqliteGraphStore::open_with_fts(&cli.database, config.index.fts5_fulltext)?;
            let summary = WorkspaceIndexer.scan_with_config(
                &workspace,
                &mut store,
                &config,
                log_scan_progress,
            )?;
            emit(
                format,
                serde_json::json!({
                    "files_indexed": summary.files_indexed,
                    "files_extracted": summary.files_extracted,
                    "files_added": summary.files_added,
                    "files_changed": summary.files_changed,
                    "files_deleted": summary.files_deleted,
                    "files_unchanged": summary.files_unchanged,
                    "entities_indexed": summary.entities_indexed,
                    "edges_indexed": summary.edges_indexed,
                    "embedded_count": summary.embedded_count,
                    "embedding_ms": summary.embedding_ms,
                    "ambiguous_skipped": summary.ambiguous_skipped
                }),
            )
        }
        Command::Search {
            query,
            format,
            limit,
        } => {
            let store = SqliteGraphStore::open(&cli.database)?;
            let matches = store.search(SearchQuery::new(query).with_limit(limit))?;
            let entities: Vec<_> = matches.into_iter().map(|matched| matched.entity).collect();
            emit(format, entities)
        }
        Command::Hotspots {
            metric,
            limit,
            min_value,
            format,
        } => {
            let store = SqliteGraphStore::open(&cli.database)?;
            let metric = metric.as_deref().unwrap_or("transitive_loop_depth");
            let hits = hotspots_items(&store, metric, min_value, limit)?;
            emit(format, hits)
        }
        Command::HttpJoin {
            frontend,
            backend,
            format,
            show_unmatched,
        } => {
            let report = http_join_report(&frontend, &backend, show_unmatched)?;
            emit(format, report)
        }
        Command::Impact {
            request,
            entity,
            operation,
            format,
            verbose,
        } => {
            let change = match (entity, request) {
                (Some(name), _) => {
                    let op: ChangeOperation = serde_json::from_str(&format!("\"{operation}\""))
                        .with_context(|| {
                            format!(
                                "未知 operation '{operation}'(可选 add/remove/rename/\
                                 change_type/change_nullable/change_format/change_semantics)"
                            )
                        })?;
                    ChangeRequest {
                        // analyzer 只按 from 的精确实体名定位,target_kind 仅为描述字段。
                        target_kind: String::new(),
                        operation: op,
                        from: Some(name),
                        to: None,
                        limit: None,
                        offset: None,
                        depth: None,
                    }
                }
                (None, Some(path)) => {
                    let bytes =
                        fs::read(&path).with_context(|| format!("read {}", path.display()))?;
                    if bytes.first() != Some(&b'{') {
                        anyhow::bail!(
                            "--request 需要 ChangeRequest JSON 文件(以 {{ 开头,示例见 README \
                             「变更影响面」);要直接分析实体请改用 --entity <NAME>"
                        );
                    }
                    serde_json::from_slice(&bytes)?
                }
                (None, None) => anyhow::bail!(
                    "impact 需要 --entity <NAME>(快捷)或 --request <change.json>(完整请求, \
                     示例见 README)"
                ),
            };
            let store = SqliteGraphStore::open(&cli.database)?;
            // [analysis] 分页上限等从 .repo-intelligence.toml 读(库文件上两级为项目根);
            // 无配置文件时 IndexerConfig::load 返回默认值。
            let analysis = cli
                .database
                .parent()
                .and_then(std::path::Path::parent)
                .map(|root| IndexerConfig::load(root).map(|c| c.analysis))
                .unwrap_or_else(|| Ok(Default::default()))?;
            let report = ImpactAnalyzer::with_config(&store, analysis).analyze(&change)?;
            if verbose {
                emit(format, report)
            } else {
                emit(format, report.compact_value())
            }
        }
        Command::Status { format } => {
            let store = SqliteGraphStore::open(&cli.database)?;
            let (entities, edges) = store.counts()?;
            emit(
                format,
                serde_json::json!({
                    "database": cli.database,
                    "entities": entities,
                    "edges": edges
                }),
            )
        }
        Command::Overview { view, format } => {
            let store = SqliteGraphStore::open(&cli.database)?;
            // Return a bounded distribution, not every entity: a large index
            // would otherwise dump megabytes of JSON to stdout for a command
            // whose purpose is a quick summary.
            let (entity_count, edge_count) = store.counts()?;
            emit(
                format,
                serde_json::json!({
                    "view": view,
                    "entity_count": entity_count,
                    "edge_count": edge_count,
                    "entities_by_kind": store.counts_by_kind()?,
                }),
            )
        }
        Command::Doctor { format } => {
            let sqlite = SqliteGraphStore::open(&cli.database).is_ok();
            emit(
                format,
                serde_json::json!({
                    "sqlite": sqlite,
                    "tree_sitter_java": true,
                    "mcp_stdio": true
                }),
            )
        }
        Command::Relay { qn, depth, format } => {
            let store = SqliteGraphStore::open(&cli.database)?;
            let doc = build_relay(&store, &qn, depth, true)?;
            emit(format, doc)
        }
        Command::SemanticSearch {
            query,
            limit,
            format,
        } => {
            let store = SqliteGraphStore::open(&cli.database)?;
            let items = semantic_search_items(&store, &query, limit)?;
            emit(format, items)
        }
    }
}

fn log_scan_progress(progress: ScanProgress) {
    // RI_LOG_EVERY 控制 Parsing 阶段日志频率(默认每 100 个文件)。诊断卡死时设
    // RI_LOG_EVERY=1 让每个文件都打,卡住时最后一行的 file= 即为元凶文件。
    let every: usize = std::env::var("RI_LOG_EVERY")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(100);
    if progress.phase == ScanPhase::Parsing
        && progress.current_path.is_some()
        && progress.processed != 0
        && every > 1
        && !progress.processed.is_multiple_of(every)
    {
        return;
    }
    let current = progress
        .current_path
        .as_deref()
        .map(|path| format!(" file={path}"))
        .unwrap_or_default();
    eprintln!(
        "[repo-intelligence] phase={} progress={}/{} elapsed_ms={}{}",
        progress.phase, progress.processed, progress.total, progress.elapsed_ms, current
    );
}

fn emit<T: serde::Serialize>(format: OutputFormat, data: T) -> Result<()> {
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string(&Envelope::success(data))?),
        OutputFormat::Text => println!("{}", serde_json::to_string_pretty(&data)?),
        OutputFormat::Yaml => println!("{}", serde_yaml::to_string(&data)?),
    }
    Ok(())
}

#[derive(serde::Serialize)]
struct SemanticHit {
    id: String,
    kind: String,
    name: String,
    qualified_name: String,
    evidence_count: usize,
    score: f32,
}

/// 语义检索编排:embed query → 遍历全库 embedding → 余弦打分 → 取 topN 实体。
/// 复刻 mcp semantic_search 的核心,但 cli 是一次性进程,Embedder 直接 new(无需单例)。
fn semantic_search_items(
    store: &SqliteGraphStore,
    query: &str,
    limit: usize,
) -> Result<Vec<SemanticHit>> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let all = store.get_all_embeddings()?;
    if all.is_empty() {
        // 对齐 MCP semantic_search 的降级:输出空结果 + 提示,而非硬报错(失败语义不同:
        // 索引存在但无向量是配置态,不是查询失败)。
        eprintln!("hint: 无 embedding——未 scan 或配置 [index] embedding=false。scan 后再查。");
        return Ok(Vec::new());
    }
    let mut embedder =
        repo_intelligence_embedding::Embedder::new().context("加载 embedding 模型失败")?;
    let qvec = embedder
        .embed(vec![query.to_string()])?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("query embedding 为空"))?;
    let mut scored: Vec<(EntityId, f32)> = all
        .into_iter()
        .map(|(id, v)| (id, repo_intelligence_embedding::cosine(&qvec, &v)))
        .filter(|(_, s)| s.is_finite())
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut hits = Vec::new();
    for (id, score) in scored.into_iter().take(limit) {
        if let Some(entity) = store.get_entity(&id)? {
            // 紧凑形(对齐 MCP semantic_search 的 compact):不展开 evidence[]/metadata,
            // 用户要完整实体可用 search 命令。
            hits.push(SemanticHit {
                id: entity.id.0.clone(),
                kind: entity.kind.as_str().to_string(),
                name: entity.name.clone(),
                qualified_name: entity.qualified_name.clone(),
                evidence_count: entity.evidence.len(),
                score,
            });
        }
    }
    Ok(hits)
}

#[derive(serde::Serialize)]
struct HotspotHit {
    qualified_name: String,
    name: String,
    metric: String,
    score: u64,
    complexity: u64,
    loop_depth: u64,
    transitive_loop_depth: u64,
    linear_scan_in_loop: u64,
}

/// 找复杂度热点(对标 codebase-memory Q4):全图 method 按 metric 降序取 top-N。
/// 复刻 mcp find_hotspots 的核心;cli 一次性进程,直接 all_entities 内存排序。
fn hotspots_items(
    store: &SqliteGraphStore,
    metric: &str,
    min_value: Option<u64>,
    limit: usize,
) -> Result<Vec<HotspotHit>> {
    let entities = store.all_entities()?;
    let pick = |e: &Entity, key: &str| e.metadata.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
    let mut items: Vec<(Entity, u64)> = entities
        .iter()
        .filter(|e| e.kind == EntityKind::Method)
        .filter_map(|e| {
            let v = pick(e, metric);
            if v == 0 {
                return None;
            }
            if let Some(m) = min_value
                && v < m
            {
                return None;
            }
            Some((e.clone(), v))
        })
        .collect();
    items.sort_by_key(|b| std::cmp::Reverse(b.1));
    Ok(items
        .into_iter()
        .take(limit)
        .map(|(e, v)| HotspotHit {
            qualified_name: e.qualified_name.clone(),
            name: e.name.clone(),
            metric: metric.to_string(),
            score: v,
            complexity: pick(&e, "complexity"),
            loop_depth: pick(&e, "loop_depth"),
            transitive_loop_depth: pick(&e, "transitive_loop_depth"),
            linear_scan_in_loop: pick(&e, "linear_scan_in_loop"),
        })
        .collect())
}

/// http-join 输出:一条匹配对聚合同一 (method, path) 的多个前端调用位置。
#[derive(serde::Serialize)]
struct HttpJoinMatch {
    method: String,
    path: String,
    endpoint: String,
    client_calls: Vec<String>,
}

#[derive(serde::Serialize)]
struct HttpJoinReport {
    frontend_db: String,
    backend_db: String,
    client_calls_total: usize,
    endpoints_total: usize,
    matched_calls: usize,
    matched_pairs: usize,
    matches: Vec<HttpJoinMatch>,
    unmatched_shown: usize,
    unmatched_clients: Vec<String>,
}

/// 从库中取 HTTP 类实体的 (method, path, qualified_name);缺 metadata 的丢弃。
fn http_calls_of(
    store: &SqliteGraphStore,
    kind: EntityKind,
) -> Result<Vec<(String, String, String)>> {
    Ok(store
        .all_entities()?
        .into_iter()
        .filter(|e| e.kind == kind)
        .filter_map(|e| {
            let method = e.metadata.get("method")?.as_str()?.to_uppercase();
            let path = e.metadata.get("path")?.as_str()?.to_string();
            Some((method, path, e.qualified_name))
        })
        .collect())
}

/// 匹配核心(纯函数便于测试):(method, path) 精确对齐,同端点的多调用点聚合。
/// 路径归一不在此处——两侧提取时均过 semantics::normalize_path(参数段 → `{}`)。
fn match_http_calls(
    clients: &[(String, String, String)],
    endpoints: &[(String, String, String)],
) -> (Vec<HttpJoinMatch>, Vec<String>) {
    let mut by_key: HashMap<(&str, &str), &str> = HashMap::new();
    for (method, path, qn) in endpoints {
        by_key.entry((method.as_str(), path.as_str())).or_insert(qn);
    }
    let mut matches: Vec<HttpJoinMatch> = Vec::new();
    let mut match_index: HashMap<(&str, &str), usize> = HashMap::new();
    let mut unmatched: Vec<String> = Vec::new();
    for (method, path, qn) in clients {
        match by_key.get(&(method.as_str(), path.as_str())) {
            Some(endpoint) => match match_index.get(&(method.as_str(), path.as_str())) {
                Some(&index) => matches[index].client_calls.push(qn.clone()),
                None => {
                    match_index.insert((method.as_str(), path.as_str()), matches.len());
                    matches.push(HttpJoinMatch {
                        method: method.clone(),
                        path: path.clone(),
                        endpoint: (*endpoint).to_string(),
                        client_calls: vec![qn.clone()],
                    });
                }
            },
            None => unmatched.push(qn.clone()),
        }
    }
    (matches, unmatched)
}

fn http_join_report(
    frontend: &std::path::Path,
    backend: &std::path::Path,
    show_unmatched: usize,
) -> Result<HttpJoinReport> {
    let fe = SqliteGraphStore::open(frontend)?;
    let be = SqliteGraphStore::open(backend)?;
    let clients = http_calls_of(&fe, EntityKind::HttpClientCall)?;
    let endpoints = http_calls_of(&be, EntityKind::HttpEndpoint)?;
    let (matches, unmatched) = match_http_calls(&clients, &endpoints);
    let matched_calls = matches.iter().map(|m| m.client_calls.len()).sum();
    Ok(HttpJoinReport {
        frontend_db: frontend.display().to_string(),
        backend_db: backend.display().to_string(),
        client_calls_total: clients.len(),
        endpoints_total: endpoints.len(),
        matched_calls,
        matched_pairs: matches.len(),
        matches,
        unmatched_shown: unmatched.len().min(show_unmatched),
        unmatched_clients: unmatched.into_iter().take(show_unmatched).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_join_matches_and_aggregates_same_endpoint_calls() {
        let clients = vec![
            (
                "GET".into(),
                "/system/user/{}".into(),
                "src/api/user.ts#GET /system/user/{}".into(),
            ),
            (
                "GET".into(),
                "/system/user/{}".into(),
                "src/api/other.ts#GET /system/user/{}".into(),
            ),
            (
                "DELETE".into(),
                "/system/user/{}".into(),
                "src/api/user.ts#DELETE /system/user/{}".into(),
            ),
        ];
        let endpoints = vec![
            (
                "GET".into(),
                "/system/user/{}".into(),
                "GET /system/user/{}".into(),
            ),
            (
                "DELETE".into(),
                "/system/user/{}".into(),
                "DELETE /system/user/{}".into(),
            ),
        ];
        let (matches, unmatched) = match_http_calls(&clients, &endpoints);
        assert_eq!(matches.len(), 2, "两个 (method,path) 键");
        assert_eq!(
            matches.iter().map(|m| m.client_calls.len()).sum::<usize>(),
            3,
            "全部调用点聚合到匹配对"
        );
        assert_eq!(matches[0].client_calls.len(), 2, "同端点多调用点");
        assert!(unmatched.is_empty());
    }

    #[test]
    fn http_join_method_mismatch_stays_unmatched() {
        let clients = vec![("POST".into(), "/x".into(), "a.ts#POST /x".into())];
        let endpoints = vec![("GET".into(), "/x".into(), "GET /x".into())];
        let (matches, unmatched) = match_http_calls(&clients, &endpoints);
        assert!(matches.is_empty(), "method 不同不误连");
        assert_eq!(unmatched.len(), 1);
    }
}
