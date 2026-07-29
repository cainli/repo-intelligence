use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use anyhow::{Result, anyhow};
pub use repo_intelligence_config::{AnalysisConfig, IndexerConfig};
use repo_intelligence_graph::GraphStore;
use repo_intelligence_model::{
    ChangeOperation, ChangeRequest, Edge, EdgeKind, Entity, EntityId, EntityKind, EvidenceClass,
    GraphPatch, ImpactFinding, ImpactReport, TraverseQuery,
};
use repo_intelligence_source::{SourceFile, discover_with_config};

#[derive(Clone, Debug, Default)]
pub struct ScanSummary {
    pub files_indexed: usize,
    /// 实际 extract 的文件数(changed + added);增量下远小于 files_indexed。
    pub files_extracted: usize,
    pub entities_indexed: usize,
    pub edges_indexed: usize,
    /// 增量统计(相对上次 file_state 快照);首次全量时全部计入 added。
    pub files_added: usize,
    pub files_changed: usize,
    pub files_deleted: usize,
    pub files_unchanged: usize,
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
        report: F,
    ) -> Result<ScanSummary>
    where
        F: FnMut(ScanProgress),
    {
        self.scan_with_config(root, store, &IndexerConfig::default(), report)
    }

    /// 按 `IndexerConfig` 扫描并索引(增量):发现用 `config.discovery`,语义提取用
    /// `config.semantics`。对比持久化的 `file_state` 快照,只对 changed/added 文件重提、
    /// 对 deleted/changed 文件删旧子树;跨文件 resolve 边(resolved=1)每次全量重算。
    pub fn scan_with_config<F>(
        &self,
        root: &Path,
        store: &mut dyn GraphStore,
        config: &IndexerConfig,
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
        let files = discover_with_config(root, &config.discovery)?;
        let file_count = files.len();
        report(ScanProgress {
            phase: ScanPhase::Discovering,
            processed: file_count,
            total: file_count,
            current_path: None,
            elapsed_ms: started.elapsed().as_millis(),
        });
        let mut summary = ScanSummary {
            files_indexed: file_count,
            ..Default::default()
        };

        // diff:旧 file_state(path → hash) vs 本次发现的文件。
        let old_state = store.get_file_state()?;
        let new_state: HashMap<String, String> = files
            .iter()
            .map(|file| {
                (
                    file.relative_path.to_string_lossy().to_string(),
                    file.content_hash.clone(),
                )
            })
            .collect();
        let mut to_reindex: Vec<&SourceFile> = Vec::new(); // changed ∪ added
        let mut to_delete: Vec<String> = Vec::new(); // deleted ∪ changed 的 path(删旧子树)
        for file in &files {
            let path = file.relative_path.to_string_lossy().to_string();
            match old_state.get(&path) {
                Some(hash) if *hash == file.content_hash => summary.files_unchanged += 1,
                Some(_) => {
                    summary.files_changed += 1;
                    to_delete.push(path);
                    to_reindex.push(file);
                }
                None => {
                    summary.files_added += 1;
                    to_reindex.push(file);
                }
            }
        }
        for path in old_state.keys() {
            if !new_state.contains_key(path) {
                summary.files_deleted += 1;
                to_delete.push(path.clone());
            }
        }

        // 删除阶段:deleted + changed 的旧子树。file_id 与 source::discover 的稳定公式一致。
        report(ScanProgress {
            phase: ScanPhase::Persisting,
            processed: 0,
            total: to_delete.len(),
            current_path: None,
            elapsed_ms: started.elapsed().as_millis(),
        });
        for path in &to_delete {
            let file_id = EntityId::stable("workspace", path, EntityKind::File, path, "");
            store.delete_file_subtree(&file_id)?;
        }

        // 提取 + 写入阶段:changed ∪ added。
        let mut combined = GraphPatch::default();
        for (index, file) in to_reindex.iter().enumerate() {
            report(ScanProgress {
                phase: ScanPhase::Parsing,
                processed: index,
                total: to_reindex.len(),
                current_path: Some(file.relative_path.to_string_lossy().to_string()),
                elapsed_ms: started.elapsed().as_millis(),
            });
            // extract 容错:单个文件提取 panic(畸形 AST / 索引越界 / tree-sitter 异常)
            // 不应让整个 scan 崩溃。catch_unwind 捕获后跳过该文件并记 stderr,其余文件
            // 继续——对应 mes/mos 在 parsing 末尾整体崩溃的场景(坏文件被跳过即可完成)。
            let patch = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                repo_intelligence_semantics::extract_with_config(file, &config.semantics)
            })) {
                Ok(Ok(patch)) => patch,
                Ok(Err(err)) => return Err(err),
                Err(payload) => {
                    let msg = payload
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
                        .unwrap_or_else(|| "<non-string panic payload>".to_string());
                    eprintln!(
                        "[ri-diag] extract panicked, skipping {}: {}",
                        file.relative_path.to_string_lossy(),
                        msg
                    );
                    continue;
                }
            };
            summary.files_extracted += 1;
            summary.entities_indexed += patch.add_entities.len();
            summary.edges_indexed += patch.add_edges.len();
            combined.add_entities.extend(patch.add_entities);
            combined.add_edges.extend(patch.add_edges);
        }
        if !combined.add_entities.is_empty() || !combined.add_edges.is_empty() {
            let n_ent = combined.add_entities.len();
            let n_edg = combined.add_edges.len();
            let t = std::time::Instant::now();
            store.apply_patch(combined)?;
            eprintln!(
                "[ri-diag] apply_patch: {n_ent} entities + {n_edg} edges in {:.2}s",
                t.elapsed().as_secs_f64()
            );
        }

        // 跨文件 resolve 全量重算:输入 = 当前全图实体 + 全部事实提取边(resolved=0)。
        report(ScanProgress {
            phase: ScanPhase::Resolving,
            processed: 0,
            total: 0,
            current_path: None,
            elapsed_ms: started.elapsed().as_millis(),
        });
        let all_entities = store.all_entities()?;
        let extract_edges = store.extract_edges()?;
        let t = std::time::Instant::now();
        let resolution = resolve_cross_stack(&all_entities, &extract_edges);
        let t_resolve = t.elapsed();
        let n_resolved = resolution.add_edges.len();
        summary.edges_indexed += n_resolved;
        let t = std::time::Instant::now();
        store.replace_resolved_edges(resolution.add_edges)?;
        eprintln!(
            "[ri-diag] resolve_cross_stack: {} entities → {} edges in {:.2}s; replace_resolved in {:.2}s",
            all_entities.len(),
            n_resolved,
            t_resolve.as_secs_f64(),
            t.elapsed().as_secs_f64()
        );

        // 写新 file_state 快照。
        report(ScanProgress {
            phase: ScanPhase::Persisting,
            processed: to_delete.len() + to_reindex.len(),
            total: file_count,
            current_path: None,
            elapsed_ms: started.elapsed().as_millis(),
        });
        let new_state_vec: Vec<(String, String)> = new_state.into_iter().collect();
        store.set_file_state(&new_state_vec)?;

        report(ScanProgress {
            phase: ScanPhase::Completed,
            processed: file_count,
            total: file_count,
            current_path: None,
            elapsed_ms: started.elapsed().as_millis(),
        });
        Ok(summary)
    }
}

fn resolve_cross_stack(entities: &[Entity], input_edges: &[Edge]) -> GraphPatch {
    let mut edges = Vec::new();
    let mut fields: HashMap<String, Vec<&Entity>> = HashMap::new();
    let mut endpoints = Vec::new();
    let mut calls = Vec::new();
    let mut mappers: Vec<&Entity> = Vec::new();
    for entity in entities {
        match entity.kind {
            EntityKind::Field
            | EntityKind::FrontendField
            | EntityKind::SqlField
            | EntityKind::ApiField
            | EntityKind::Column => {
                fields.entry(entity.name.clone()).or_default().push(entity);
            }
            EntityKind::HttpEndpoint => endpoints.push(entity),
            EntityKind::HttpClientCall => calls.push(entity),
            EntityKind::Mapper => mappers.push(entity),
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
    // 跨文件 Mapper→Table:Mapper.metadata.entity_type 命中同名 Class → 其 DependsOn 的 Table
    // (同文件绑定由 extract_mybatis_plus 产;这里补跨文件,store upsert 按 (s,t,kind) 去重)。
    let entity_by_id: HashMap<&EntityId, &Entity> =
        entities.iter().map(|entity| (&entity.id, entity)).collect();
    let mut class_to_table: HashMap<&str, &EntityId> = HashMap::new();
    for edge in input_edges {
        if edge.kind == EdgeKind::DependsOn
            && let (Some(src), Some(tgt)) =
                (entity_by_id.get(&edge.source), entity_by_id.get(&edge.target))
            && src.kind == EntityKind::Class
            && tgt.kind == EntityKind::Table
        {
            class_to_table.insert(src.name.as_str(), &tgt.id);
        }
    }
    for mapper in &mappers {
        let Some(entity_type) = mapper.metadata.get("entity_type").and_then(|v| v.as_str()) else {
            continue;
        };
        if let Some(table_id) = class_to_table.get(entity_type) {
            let mut edge = Edge::new(mapper.id.clone(), (*table_id).clone(), EdgeKind::DependsOn);
            if let Some(ev) = mapper.evidence.first() {
                edge = edge.with_evidence(
                    &ev.file,
                    ev.start_line,
                    ev.end_line,
                    EvidenceClass::Resolved,
                    0.9,
                    "BaseMapper<EntityType> binds mapper to entity table (cross-file)",
                );
            }
            edges.push(edge);
        }
    }

    // 跨文件 method 调用:method.metadata.invokes 的 callee 名 + 所属类型的注入依赖
    // (DependsOn→SpringBean type)→ 在注入 type 声明的 method 里按名匹配,补
    // Controller→Service→Mapper 跨文件调用链(同文件 calls 由 extract_java 产)。
    // 低保真:方法名 + 注入类型匹配;同名歧义(多个注入 type 都有该方法)则跳过。
    let entity_by_id_cf: HashMap<&EntityId, &Entity> =
        entities.iter().map(|entity| (&entity.id, entity)).collect();
    let mut type_methods: HashMap<&str, HashMap<&str, &EntityId>> = HashMap::new();
    let mut method_owner: HashMap<&EntityId, &str> = HashMap::new();
    for edge in input_edges {
        if edge.kind == EdgeKind::Declares
            && let (Some(owner), Some(m)) =
                (entity_by_id_cf.get(&edge.source), entity_by_id_cf.get(&edge.target))
            && matches!(owner.kind, EntityKind::Class | EntityKind::Interface)
            && m.kind == EntityKind::Method
        {
            method_owner.insert(&edge.target, owner.name.as_str());
            type_methods
                .entry(owner.name.as_str())
                .or_default()
                .insert(m.name.as_str(), &edge.target);
        }
    }
    let mut owner_injected: HashMap<&str, Vec<&str>> = HashMap::new();
    for edge in input_edges {
        if edge.kind == EdgeKind::Injects
            && let (Some(owner), Some(bean)) =
                (entity_by_id_cf.get(&edge.source), entity_by_id_cf.get(&edge.target))
            && matches!(owner.kind, EntityKind::Class | EntityKind::Interface)
            && bean.kind == EntityKind::SpringBean
        {
            owner_injected
                .entry(owner.name.as_str())
                .or_default()
                .push(bean.name.as_str());
        }
    }
    for entity in entities {
        if entity.kind != EntityKind::Method {
            continue;
        }
        let Some(invokes) = entity.metadata.get("invokes").and_then(|v| v.as_array()) else {
            continue;
        };
        let Some(&owner) = method_owner.get(&entity.id) else {
            continue;
        };
        let Some(injected) = owner_injected.get(owner) else {
            continue;
        };
        for invoke in invokes {
            let Some(callee_name) = invoke.get("name").and_then(|v| v.as_str()) else {
                continue;
            };
            let line = invoke.get("line").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let mut hits: Vec<&EntityId> = Vec::new();
            for type_name in injected {
                if let Some(ms) = type_methods.get(*type_name)
                    && let Some(callee_id) = ms.get(callee_name).copied()
                {
                    hits.push(callee_id);
                }
            }
            // 仅在唯一命中且非自调用时建边;多命中=歧义跳过,避免连错。
            if hits.len() == 1 && hits[0] != &entity.id {
                let evidence = entity.evidence.first();
                let mut edge = Edge::new(entity.id.clone(), hits[0].clone(), EdgeKind::Calls);
                if let Some(ev) = evidence {
                    edge = edge.with_evidence(
                        &ev.file,
                        line,
                        line,
                        EvidenceClass::Inferred,
                        0.6,
                        "cross-file call via injected dependency (matched by name)",
                    );
                }
                edges.push(edge);
            }
        }
    }

    // implements:跨文件 class.metadata.implements → 全局 interface 实体(EntityId 是
    // path-scoped,extract 层建不了跨文件边,故 metadata 传递 + 这里按 interface 名解析)。
    // 同时建 class→interface depends_on 边 + 反向索引 interface→impl,供下面的桥接。
    let interfaces_by_name: HashMap<&str, &EntityId> = entities
        .iter()
        .filter(|e| e.kind == EntityKind::Interface)
        .map(|e| (e.name.as_str(), &e.id))
        .collect();
    let mut interface_to_impls: HashMap<&str, Vec<&str>> = HashMap::new();
    for entity in entities {
        if entity.kind != EntityKind::Class {
            continue;
        }
        let Some(impls) = entity.metadata.get("implements").and_then(|v| v.as_array()) else {
            continue;
        };
        for iface_val in impls {
            let Some(iface_name) = iface_val.as_str() else { continue };
            let Some(&iface_id) = interfaces_by_name.get(iface_name) else { continue };
            let mut edge = Edge::new(entity.id.clone(), iface_id.clone(), EdgeKind::DependsOn);
            if let Some(ev) = entity.evidence.first() {
                edge = edge.with_evidence(
                    &ev.file,
                    ev.start_line,
                    ev.end_line,
                    EvidenceClass::Fact,
                    1.0,
                    "class implements interface",
                );
            }
            edges.push(edge);
            interface_to_impls
                .entry(iface_name)
                .or_default()
                .push(entity.name.as_str());
        }
    }
    for edge in input_edges {
        if edge.kind == EdgeKind::Declares
            && let (Some(iface), Some(m)) =
                (entity_by_id_cf.get(&edge.source), entity_by_id_cf.get(&edge.target))
            && iface.kind == EntityKind::Interface
            && m.kind == EntityKind::Method
            && let Some(impls) = interface_to_impls.get(iface.name.as_str())
        {
            let mut hits: Vec<&EntityId> = Vec::new();
            for impl_name in impls {
                if let Some(ms) = type_methods.get(*impl_name)
                    && let Some(impl_m_id) = ms.get(m.name.as_str()).copied()
                {
                    hits.push(impl_m_id);
                }
            }
            // 唯一实现且非自环时建桥接边;多实现=歧义跳过。
            if hits.len() == 1 && hits[0] != &edge.target {
                let mut bridge = Edge::new(edge.target.clone(), hits[0].clone(), EdgeKind::Calls);
                if let Some(ev) = m.evidence.first() {
                    bridge = bridge.with_evidence(
                        &ev.file,
                        ev.start_line,
                        ev.end_line,
                        EvidenceClass::Inferred,
                        0.7,
                        "interface dispatch to implementation",
                    );
                }
                edges.push(bridge);
            }
        }
    }

    // Mapper method → Table:method 的 owner interface 与同文件同名 Mapper entity(MP_MAPPER)
    // 是同一物的两面。Mapper entity 的 entity_type 命中的 Class→Table(extract_mybatis_plus
    // 产 @TableName 类→Table,在 extract 边里)即 method 操作的表。建 method→Table ReadsTable,
    // 让调用链从 Mapper method 抵达 data 层。不依赖 resolve 产的跨文件 Mapper→Table 边
    // (那些不在本次 input_edges 里)。
    let class_to_table_m2t: HashMap<&str, &EntityId> = input_edges
        .iter()
        .filter_map(|edge| {
            if edge.kind != EdgeKind::DependsOn {
                return None;
            }
            let (Some(cls), Some(tbl)) =
                (entity_by_id_cf.get(&edge.source), entity_by_id_cf.get(&edge.target))
            else {
                return None;
            };
            if cls.kind == EntityKind::Class && tbl.kind == EntityKind::Table {
                Some((cls.name.as_str(), &edge.target))
            } else {
                None
            }
        })
        .collect();
    let mapper_by_key: HashMap<(&str, &str), &Entity> = entities
        .iter()
        .filter_map(|e| {
            if e.kind != EntityKind::Mapper {
                return None;
            }
            e.evidence
                .first()
                .map(|ev| ((e.name.as_str(), ev.file.as_str()), e))
        })
        .collect();
    for edge in input_edges {
        if edge.kind != EdgeKind::Declares {
            continue;
        }
        let (Some(owner), Some(method)) =
            (entity_by_id_cf.get(&edge.source), entity_by_id_cf.get(&edge.target))
        else {
            continue;
        };
        if owner.kind != EntityKind::Interface || method.kind != EntityKind::Method {
            continue;
        }
        let Some(ev) = owner.evidence.first() else { continue };
        let Some(mapper) = mapper_by_key.get(&(owner.name.as_str(), ev.file.as_str())) else {
            continue;
        };
        let Some(entity_type) = mapper.metadata.get("entity_type").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(table_id) = class_to_table_m2t.get(entity_type) else {
            continue;
        };
        let mut m2t = Edge::new(edge.target.clone(), (*table_id).clone(), EdgeKind::ReadsTable);
        if let Some(mev) = method.evidence.first() {
            m2t = m2t.with_evidence(
                &mev.file,
                mev.start_line,
                mev.end_line,
                EvidenceClass::Inferred,
                0.7,
                "mapper method reads table (BaseMapper<entity_type> → @TableName class → table)",
            );
        }
        edges.push(m2t);
    }

    for call in calls {
        let call_method = call.metadata.get("method").and_then(|value| value.as_str());
        let call_path = call.metadata.get("path").and_then(|value| value.as_str());
        for endpoint in &endpoints {
            let endpoint_method = endpoint
                .metadata
                .get("method")
                .and_then(|value| value.as_str());
            // custom RPC endpoints(@RmbMap 等)无 method,不与前端 HTTP 调用匹配。
            if call_method != endpoint_method {
                continue;
            }
            let endpoint_path = endpoint
                .metadata
                .get("path")
                .and_then(|value| value.as_str());
            // 分级匹配:精确全等 = Resolved 高置信;否则后缀段对齐 = Inferred 低置信
            // (吸收 baseURL/版本前缀)。同一 (call, endpoint) 对最多产生一条边。
            let match_kind = match (call_path, endpoint_path) {
                (Some(cp), Some(ep)) if cp == ep => Some((
                    EvidenceClass::Resolved,
                    0.95,
                    "normalized HTTP method and path match",
                )),
                (Some(cp), Some(ep)) if segment_suffix_align(cp, ep) => Some((
                    EvidenceClass::Inferred,
                    0.6,
                    "path suffix aligns (baseURL/version prefix tolerated)",
                )),
                _ => None,
            };
            if let Some((classification, confidence, reason)) = match_kind {
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
                        classification,
                        confidence,
                        reason,
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
        EntityKind::Column => 4,
        _ => 4,
    }
}

/// 判断 endpoint 路径是否为 call 路径的"连续后缀段"——用于在前端调用带了
/// baseURL/版本前缀(如 `/api/v1/orders/{}`)而后端只声明 `/orders/{}` 时仍能连上。
/// 两侧路径都已被 `semantics::normalize_path` 归一化(参数→`{}`、统一前导 `/`)。
///
/// 规则:按 `/` 切段(去空),endpoint 段序列必须等于 call 段序列的末尾连续段,
/// 且段数差 ≥ 1(差为 0 即精确全等,由上游精确分支处理)。
///
/// 可调边界(召回 vs 精度的旋钮,按真实仓库的误连情况再拧):
/// - 段数差当前无上限。若 `/a` 误连到 `/x/y/z/w/a` 这类长尾,加上限(如差 ≤ 3)。
/// - 参数段 `{}` 当前只与 `{}` 相等,不通配任意段。若要 `/orders/{}` 对上
///   `/orders/123`,需让 `{}` 通配——但这会显著抬高误报,慎用。
/// - 当前不要求 HTTP method 之外的前缀词匹配;若噪声多,可加调用者白名单。
fn segment_suffix_align(call_path: &str, endpoint_path: &str) -> bool {
    let call_segments: Vec<&str> = call_path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    let endpoint_segments: Vec<&str> = endpoint_path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    if endpoint_segments.is_empty() || endpoint_segments.len() >= call_segments.len() {
        // endpoint 为空,或段数 ≥ call(含相等)→ 不算后缀对齐(相等归精确分支)。
        return false;
    }
    let offset = call_segments.len() - endpoint_segments.len();
    call_segments[offset..] == endpoint_segments[..]
}

pub struct ImpactAnalyzer<'a> {
    store: &'a dyn GraphStore,
    analysis: AnalysisConfig,
}

impl<'a> ImpactAnalyzer<'a> {
    pub fn new(store: &'a dyn GraphStore) -> Self {
        Self {
            store,
            analysis: AnalysisConfig::default(),
        }
    }

    /// 用指定 `AnalysisConfig` 构造(分页上限等从配置读)。
    pub fn with_config(store: &'a dyn GraphStore, analysis: AnalysisConfig) -> Self {
        Self { store, analysis }
    }

    pub fn analyze(&self, change: &ChangeRequest) -> Result<ImpactReport> {
        let source_name = change
            .from
            .as_deref()
            .ok_or_else(|| anyhow!("change request is missing the source field"))?;
        let limit = change
            .limit
            .unwrap_or(self.analysis.default_impact_limit)
            .clamp(1, self.analysis.max_impact_limit);
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
        // 精确名匹配(走 entity_name 索引,非子串 LIKE)——只有 name == source_name
        // 的实体才是真影响目标;子串命中(如 customer_name_id)是巧合。
        let mut candidates: Vec<Entity> = self
            .store
            .search_exact_name(source_name, self.analysis.max_search_limit)?;
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
            // finding 的可达性置信度 = path 周围边上证据的最小 confidence。
            // 触及 Inferred 边(分级匹配的低置信 matches_endpoint)会拉低它,
            // 让客户端能区分精确命中与推断命中。起点实体自身的存在证据不参与
            // (那是"它存在"的证据,不是"这样可达"的证据)。
            let mut confidence = 1.0_f32;
            let outbound_trav = self
                .store
                .traverse(TraverseQuery::outbound(entity.id.clone()).with_depth(depth))?;
            let inbound_trav = self.store.traverse(TraverseQuery {
                start: entity.id.clone(),
                outbound: false,
                max_depth: depth,
                edge_kinds: Vec::new(),
            })?;
            // 所在 file:从 inbound 邻居里找 File 实体(file-桥接要用)。
            let containing_file = inbound_trav
                .entities
                .iter()
                .find(|related| related.kind == EntityKind::File)
                .map(|file| file.id.clone());
            for traversal in [outbound_trav, inbound_trav] {
                for edge in traversal.edges {
                    for ev in &edge.evidence {
                        if ev.confidence < confidence {
                            confidence = ev.confidence;
                        }
                    }
                    evidence.extend(edge.evidence);
                }
                for related in traversal.entities {
                    if !path.contains(&related.id) {
                        path.push(related.id);
                    }
                }
            }
            // 前端字段 file-桥接:字段是叶节点,单向 traverse 到所在 file 即停,
            // 触及不到同 file 的 http_client_call。这里额外从 file outbound 到
            // call(Contains)→ endpoint(MatchesEndpoint),让"改前端字段"的 blast
            // radius 能到后端端点。MatchesEndpoint 多为 Inferred,会拉低 confidence,
            // 与分级匹配呼应。启发式:同页面所有 endpoint 都纳入,靠 confidence 区分。
            if entity.kind == EntityKind::FrontendField
                && let Some(file_id) = containing_file {
                    let bridge = self.store.traverse(TraverseQuery {
                        start: file_id,
                        outbound: true,
                        max_depth: 2,
                        edge_kinds: vec![EdgeKind::Contains, EdgeKind::MatchesEndpoint],
                    })?;
                    for edge in bridge.edges {
                        for ev in &edge.evidence {
                            if ev.confidence < confidence {
                                confidence = ev.confidence;
                            }
                        }
                        evidence.extend(edge.evidence);
                    }
                    for related in bridge.entities {
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
                confidence,
            });
        }
        if total >= self.analysis.max_search_limit {
            report.open_questions.push(format!(
                "Impact search reached the {}-candidate cap; `total` is a lower bound and additional matches may exist beyond it.",
                self.analysis.max_search_limit
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
