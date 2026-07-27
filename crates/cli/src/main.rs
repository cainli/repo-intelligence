use std::fs;
use std::io;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use repo_intelligence_analysis::{ImpactAnalyzer, ScanPhase, ScanProgress, WorkspaceIndexer};
use repo_intelligence_graph::{GraphStore, SqliteGraphStore};
use repo_intelligence_model::{ChangeRequest, SearchQuery};
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
    Mcp,
}

#[derive(Clone, Copy, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("repo-intelligence: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    if let Some(parent) = cli.database.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create data directory {}", parent.display()))?;
    }
    match cli.command {
        Command::Mcp => repo_intelligence_mcp::serve(
            io::stdin().lock(),
            io::stdout().lock(),
            Some(&cli.database),
        ),
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
            let mut store = SqliteGraphStore::open(&cli.database)?;
            let summary =
                WorkspaceIndexer.scan_with_progress(&workspace, &mut store, log_scan_progress)?;
            emit(
                format,
                serde_json::json!({
                    "files_indexed": summary.files_indexed,
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
            let entities = store.all_entities()?;
            emit(
                format,
                serde_json::json!({
                    "database": cli.database,
                    "entities": entities.len()
                }),
            )
        }
        Command::Overview { view, format } => {
            let store = SqliteGraphStore::open(&cli.database)?;
            let entities = store.all_entities()?;
            emit(
                format,
                serde_json::json!({"view": view, "entities": entities}),
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
    }
}

fn log_scan_progress(progress: ScanProgress) {
    if progress.phase == ScanPhase::Parsing
        && progress.current_path.is_some()
        && progress.processed != 0
        && !progress.processed.is_multiple_of(100)
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
    }
    Ok(())
}
