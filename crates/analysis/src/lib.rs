use std::collections::HashMap;
use std::path::Path;

use anyhow::{Result, anyhow};
use repo_intelligence_graph::GraphStore;
use repo_intelligence_model::{
    ChangeRequest, Edge, EdgeKind, Entity, EntityKind, EvidenceClass, GraphPatch, ImpactFinding,
    ImpactReport, SearchQuery,
};
use repo_intelligence_source::discover;

#[derive(Clone, Debug, Default)]
pub struct ScanSummary {
    pub files_indexed: usize,
    pub entities_indexed: usize,
    pub edges_indexed: usize,
}

#[derive(Default)]
pub struct WorkspaceIndexer;

impl WorkspaceIndexer {
    pub fn scan(&self, root: &Path, store: &mut dyn GraphStore) -> Result<ScanSummary> {
        let files = discover(root)?;
        let mut summary = ScanSummary {
            files_indexed: files.len(),
            ..Default::default()
        };
        for file in &files {
            let patch = repo_intelligence_semantics::extract(file)?;
            summary.entities_indexed += patch.add_entities.len();
            summary.edges_indexed += patch.add_edges.len();
            store.apply_patch(patch)?;
        }
        let resolution = resolve_cross_stack(store.all_entities()?);
        summary.edges_indexed += resolution.add_edges.len();
        store.apply_patch(resolution)?;
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
    for related in fields.values() {
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
        let matches = self
            .store
            .search(SearchQuery::new(source_name).with_limit(500))?;
        let mut report = ImpactReport::default();
        for matched in matches
            .into_iter()
            .filter(|matched| matched.entity.name == source_name)
        {
            let entity = matched.entity;
            let plane = plane_for(entity.kind).to_owned();
            report.findings.push(ImpactFinding {
                path: vec![entity.id.clone()],
                evidence: entity.evidence.clone(),
                entity,
                plane,
                severity: "review_required".into(),
            });
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
