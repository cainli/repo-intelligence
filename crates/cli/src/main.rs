use std::fs;
use std::io;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use repo_intelligence_config::IndexerConfig;
use repo_intelligence_analysis::{ImpactAnalyzer, ScanPhase, ScanProgress, WorkspaceIndexer};
use repo_intelligence_graph::{GraphStore, SqliteGraphStore};
use repo_intelligence_mcp::build_relay;
use repo_intelligence_model::{ChangeRequest, EntityId, SearchQuery};
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
        #[arg(long)]
        request: PathBuf,
        #[arg(long, value_enum, default_value = "text")]
        format: OutputFormat,
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
    if let Some(parent) = cli.database.parent() {
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
                    "edges_indexed": summary.edges_indexed
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
        Command::Impact { request, format } => {
            let change: ChangeRequest = serde_json::from_slice(
                &fs::read(&request).with_context(|| format!("read {}", request.display()))?,
            )?;
            let store = SqliteGraphStore::open(&cli.database)?;
            let report = ImpactAnalyzer::new(&store).analyze(&change)?;
            emit(format, report)
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
        Command::Relay {
            qn,
            depth,
            format,
        } => {
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
        anyhow::bail!(
            "无 embedding:未 scan 或配置 [index] embedding=false。scan 后再查。"
        );
    }
    let mut embedder = repo_intelligence_embedding::Embedder::new()
        .context("加载 embedding 模型失败")?;
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
