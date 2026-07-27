use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use anyhow::{Result, anyhow};
use repo_intelligence_graph::GraphStore;
use repo_intelligence_model::{
    ChangeOperation, ChangeRequest, Edge, EdgeKind, Entity, EntityKind, EvidenceClass, GraphPatch,
    ImpactFinding, ImpactReport, SearchQuery, TraverseQuery,
};
use repo_intelligence_source::discover;

/// Default page size for `analyze_change` when the caller omits `limit`. Keeps a
/// single high-fan-out change (e.g. removing a widely-referenced field) from
/// producing millions of characters of JSON the client cannot consume.
const DEFAULT_IMPACT_LIMIT: usize = 100;
/// Hard ceiling on `limit` to bound worst-case result volume.
const MAX_IMPACT_LIMIT: usize = 500;
/// Upper bound on the internal search fan-out used to populate a page.
const MAX_SEARCH_LIMIT: usize = 1_000;

#[derive(Clone, Debug, Default)]
pub struct ScanSummary {
    pub files_indexed: usize,
    pub entities_indexed: usize,
    pub edges_indexed: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanPhase {
    Discovering,
    Parsing,
    Resolving,
    Persisting,
    Completed,
}

impl std::fmt::Display for ScanPhase {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Discovering => "discovering",
            Self::Parsing => "parsing",
            Self::Resolving => "resolving",
            Self::Persisting => "persisting",
            Self::Completed => "completed",
        };
        formatter.write_str(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanProgress {
    pub phase: ScanPhase,
    pub processed: usize,
    pub total: usize,
    pub current_path: Option<String>,
    pub elapsed_ms: u128,
}

#[derive(Default)]
pub struct WorkspaceIndexer;

impl WorkspaceIndexer {
    pub fn scan(&self, root: &Path, store: &mut dyn GraphStore) -> Result<ScanSummary> {
        self.scan_with_progress(root, store, |_| {})
    }

    pub fn scan_with_progress<F>(
        &self,
        root: &Path,
        store: &mut dyn GraphStore,
        mut report: F,
    ) -> Result<ScanSummary>
    where
        F: FnMut(ScanProgress),
    {
        let started = Instant::now();
        report(ScanProgress {
            phase: ScanPhase::Discovering,
            processed: 0,
            total: 0,
            current_path: None,
            elapsed_ms: 0,
        });
        let files = discover(root)?;
        report(ScanProgress {
            phase: ScanPhase::Discovering,
            processed: files.len(),
            total: files.len(),
            current_path: None,
            elapsed_ms: started.elapsed().as_millis(),
        });
        let mut summary = ScanSummary {
            files_indexed: files.len(),
            ..Default::default()
        };
        let mut combined = GraphPatch::default();
        for (index, file) in files.iter().enumerate() {
            report(ScanProgress {
                phase: ScanPhase::Parsing,
                processed: index,
                total: files.len(),
                current_path: Some(file.relative_path.to_string_lossy().to_string()),
                elapsed_ms: started.elapsed().as_millis(),
            });
            let patch = repo_intelligence_semantics::extract(file)?;
            summary.entities_indexed += patch.add_entities.len();
            summary.edges_indexed += patch.add_edges.len();
            combined.add_entities.extend(patch.add_entities);
            combined.add_edges.extend(patch.add_edges);
        }
        report(ScanProgress {
            phase: ScanPhase::Parsing,
            processed: files.len(),
            total: files.len(),
            current_path: None,
            elapsed_ms: started.elapsed().as_millis(),
        });
        report(ScanProgress {
            phase: ScanPhase::Resolving,
            processed: 0,
            total: combined.add_entities.len(),
            current_path: None,
            elapsed_ms: started.elapsed().as_millis(),
        });
        let resolution = resolve_cross_stack(combined.add_entities.clone());
        summary.edges_indexed += resolution.add_edges.len();
        combined.add_edges.extend(resolution.add_edges);
        report(ScanProgress {
            phase: ScanPhase::Persisting,
            processed: 0,
            total: combined.add_entities.len() + combined.add_edges.len(),
            current_path: None,
            elapsed_ms: started.elapsed().as_millis(),
        });
        store.replace_snapshot(combined)?;
        report(ScanProgress {
            phase: ScanPhase::Completed,
            processed: summary.files_indexed,
            total: summary.files_indexed,
            current_path: None,
            elapsed_ms: started.elapsed().as_millis(),
        });
        Ok(summary)
    }
}

fn resolve_cross_stack(entities: Vec<Entity>) -> GraphPatch {
    let mut edges = Vec::new();
    let mut fields: HashMap<String, Vec<&Entity>> = HashMap::new();
    let mut endpoints = Vec::new();
    let mut calls = Vec::new();
    for entity in &entities {
        match entity.kind {
            EntityKind::Field
            | EntityKind::FrontendField
            | EntityKind::SqlField
            | EntityKind::ApiField => {
                fields.entry(entity.name.clone()).or_default().push(entity);
            }
            EntityKind::HttpEndpoint => endpoints.push(entity),
            EntityKind::HttpClientCall => calls.push(entity),
            _ => {}
        }
    }
    for related in fields.values_mut() {
        related.sort_by_key(|entity| semantic_rank(entity.kind));
        for pair in related.windows(2) {
            let evidence = pair[0]
                .evidence
                .first()
                .or_else(|| pair[1].evidence.first());
            let mut edge = Edge::new(pair[0].id.clone(), pair[1].id.clone(), EdgeKind::MappedFrom);
            if let Some(evidence) = evidence {
                edge = edge.with_evidence(
                    &evidence.file,
                    evidence.start_line,
                    evidence.end_line,
                    EvidenceClass::Resolved,
                    0.9,
                    "same field name across semantic layers",
                );
            }
            edges.push(edge);
        }
    }
    for call in calls {
        let call_method = call.metadata.get("method").and_then(|value| value.as_str());
        let call_path = call.metadata.get("path").and_then(|value| value.as_str());
        for endpoint in &endpoints {
            let endpoint_method = endpoint
                .metadata
                .get("method")
                .and_then(|value| value.as_str());
            let endpoint_path = endpoint
                .metadata
                .get("path")
                .and_then(|value| value.as_str());
            if call_method == endpoint_method && call_path == endpoint_path {
                let evidence = call.evidence.first();
                let mut edge = Edge::new(
                    call.id.clone(),
                    endpoint.id.clone(),
                    EdgeKind::MatchesEndpoint,
                );
                if let Some(evidence) = evidence {
                    edge = edge.with_evidence(
                        &evidence.file,
                        evidence.start_line,
                        evidence.end_line,
                        EvidenceClass::Resolved,
                        0.95,
                        "normalized HTTP method and path match",
                    );
                }
                edges.push(edge);
            }
        }
    }
    GraphPatch::add(Vec::new(), edges)
}

fn semantic_rank(kind: EntityKind) -> u8 {
    match kind {
        EntityKind::FrontendField => 0,
        EntityKind::ApiField => 1,
        EntityKind::Field => 2,
        EntityKind::SqlField => 3,
        _ => 4,
    }
}

pub struct ImpactAnalyzer<'a> {
    store: &'a dyn GraphStore,
}

impl<'a> ImpactAnalyzer<'a> {
    pub fn new(store: &'a dyn GraphStore) -> Self {
        Self { store }
    }

    pub fn analyze(&self, change: &ChangeRequest) -> Result<ImpactReport> {
        let source_name = change
            .from
            .as_deref()
            .ok_or_else(|| anyhow!("change request is missing the source field"))?;
        let limit = change
            .limit
            .unwrap_or(DEFAULT_IMPACT_LIMIT)
            .clamp(1, MAX_IMPACT_LIMIT);
        let offset = change.offset.unwrap_or(0);
        let depth = change
            .depth
            .unwrap_or_else(|| default_depth(change.operation));
        // Pull the full candidate set (capped) so `total` reflects the real
        // fan-out and pagination stays accurate — the page window is applied in
        // memory after ranking. Using `offset+limit` as the SQL LIMIT instead
        // would silently cap `total` at the window size and break `has_more`.
        // The cap bounds a runaway query; reaching it means `total` is a lower
        // bound (surfaced as an open question below).
        let matches = self
            .store
            .search(SearchQuery::new(source_name).with_limit(MAX_SEARCH_LIMIT))?;
        // Only exact-name matches are real impact targets; substring hits are
        // coincidence (e.g. a table named `customer_name_id`).
        let mut candidates: Vec<Entity> = matches
            .into_iter()
            .filter(|matched| matched.entity.name == source_name)
            .map(|matched| matched.entity)
            .collect();
        let total = candidates.len();
        // Rank by impact surface: user-visible planes (frontend/api/data)
        // first, so a truncated page still shows the changes a human cares most
        // about. Destructive remove/change ops get shallow depth by default.
        candidates.sort_by(|a, b| {
            plane_rank(b.kind)
                .cmp(&plane_rank(a.kind))
                .then_with(|| a.qualified_name.cmp(&b.qualified_name))
        });
        let has_more = offset.saturating_add(limit) < total;
        let mut report = ImpactReport {
            total,
            limit,
            offset,
            has_more,
            ..Default::default()
        };
        for entity in candidates.into_iter().skip(offset).take(limit) {
            let plane = plane_for(entity.kind).to_owned();
            let mut path = vec![entity.id.clone()];
            let mut evidence = entity.evidence.clone();
            for traversal in [
                self.store
                    .traverse(TraverseQuery::outbound(entity.id.clone()).with_depth(depth))?,
                self.store.traverse(TraverseQuery {
                    start: entity.id.clone(),
                    outbound: false,
                    max_depth: depth,
                })?,
            ] {
                for edge in traversal.edges {
                    evidence.extend(edge.evidence);
                }
                for related in traversal.entities {
                    if !path.contains(&related.id) {
                        path.push(related.id);
                    }
                }
            }
            report.findings.push(ImpactFinding {
                path,
                evidence,
                entity,
                plane,
                severity: "review_required".into(),
            });
        }
        if total >= MAX_SEARCH_LIMIT {
            report.open_questions.push(format!(
                "Impact search reached the {MAX_SEARCH_LIMIT}-candidate cap; `total` is a lower bound and additional matches may exist beyond it."
            ));
        }
        if report.findings.is_empty() {
            report.open_questions.push(format!(
                "No exact field named `{source_name}` was found; confirm the target or scan scope."
            ));
        }
        if report
            .findings
            .iter()
            .any(|finding| finding.entity.kind == EntityKind::FrontendField)
            && !report
                .findings
                .iter()
                .any(|finding| finding.entity.kind == EntityKind::Field)
        {
            report.open_questions.push(
                "Frontend field has no resolved Java field; the API may be external or dynamic."
                    .into(),
            );
        }
        Ok(report)
    }
}

fn plane_for(kind: EntityKind) -> &'static str {
    match kind {
        EntityKind::VuePage | EntityKind::VueComponent | EntityKind::FrontendField => "frontend",
        EntityKind::HttpEndpoint | EntityKind::ApiField | EntityKind::HttpClientCall => "api",
        EntityKind::SqlField
        | EntityKind::XmlStatement
        | EntityKind::Table
        | EntityKind::Column => "data",
        EntityKind::TestCase => "test",
        EntityKind::ConfigFile => "delivery",
        _ => "code",
    }
}

/// Default traversal depth per operation. Destructive operations (remove,
/// type/nullable/format/semantics change) only need direct dependents — going
/// deeper explodes the result for a question that is really "what breaks right
/// here?". Add/rename want a wider net but are still capped.
fn default_depth(operation: ChangeOperation) -> usize {
    match operation {
        ChangeOperation::Remove
        | ChangeOperation::ChangeType
        | ChangeOperation::ChangeNullable
        | ChangeOperation::ChangeFormat
        | ChangeOperation::ChangeSemantics => 1,
        ChangeOperation::Add | ChangeOperation::Rename => 2,
    }
}

/// Impact-surface priority for ranking truncated pages. Higher = more
/// user-visible, so a capped page surfaces the planes a human reviews first.
fn plane_rank(kind: EntityKind) -> u8 {
    match kind {
        EntityKind::FrontendField | EntityKind::VuePage | EntityKind::VueComponent => 4,
        EntityKind::HttpEndpoint | EntityKind::ApiField | EntityKind::HttpClientCall => 3,
        EntityKind::SqlField
        | EntityKind::XmlStatement
        | EntityKind::Table
        | EntityKind::Column => 2,
        EntityKind::Field => 1,
        _ => 0,
    }
}
