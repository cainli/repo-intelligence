use std::collections::{HashMap, HashSet};
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
use serde_json::json;

/// 索引格式版本:EntityId 方案变更(方法 arity 判别符、字段所属类判别符等)必须强制
/// 全量重提,否则增量扫描会让旧 id 实体与新方案边(id 不匹配)并存,边悬空。做法:
/// file_state 的值带版本前缀,版本变更后首次扫描新旧哈希不等 → 全量重提,之后稳定回增量。
const INDEX_FORMAT: u32 = 2;

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
    /// A+ 策略下因裸名歧义被跳过的跨文件解析数(已记入请求方实体的
    /// metadata.ambiguous_resolution,消费方可据此消歧或复核)。
    pub ambiguous_skipped: usize,
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
        // file_state 值带 INDEX_FORMAT 前缀:EntityId 方案变更后旧快照哈希不带前缀 →
        // 不等 → 全量重提;之后新旧都带前缀,回到正常增量。
        let new_state: HashMap<String, String> = files
            .iter()
            .map(|file| {
                (
                    file.relative_path.to_string_lossy().to_string(),
                    format!("f{INDEX_FORMAT}:{}", file.content_hash),
                )
            })
            .collect();
        let mut to_reindex: Vec<&SourceFile> = Vec::new(); // changed ∪ added
        let mut to_delete: Vec<String> = Vec::new(); // deleted ∪ changed 的 path(删旧子树)
        for file in &files {
            let path = file.relative_path.to_string_lossy().to_string();
            // 比较 file_state 时用带版本前缀的值(与 new_state 同公式),保证增量语义正确。
            let current = format!("f{INDEX_FORMAT}:{}", file.content_hash);
            match old_state.get(&path) {
                Some(hash) if *hash == current => summary.files_unchanged += 1,
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
        // 快照本次变更实体(id + 向量化文本),供 resolve 后增量生成 embedding。
        // 仅在 embedding 开启时构建——否则 ann_name_by_id/anns_by_owner 的 O(entities+edges)
        // 迭代 + HashMap 分配是纯浪费(apply_patch 后 combined 被 move,故必须在此先构建)。
        // 去重 by id:add_entities 可能含重复 id(同实体多 patch),去重避免重复推理。
        let embed_inputs: Vec<(EntityId, String)> = if config.index.embedding {
            // 注解画像:给被注解实体拼上注解名(经 Annotated 边回查,注解不在 metadata)。
            let ann_name_by_id: HashMap<&EntityId, &str> = combined
                .add_entities
                .iter()
                .filter(|e| e.kind == EntityKind::Annotation)
                .map(|e| (&e.id, e.name.as_str()))
                .collect();
            let mut anns_by_owner: HashMap<&EntityId, Vec<&str>> = HashMap::new();
            for edge in &combined.add_edges {
                if edge.kind == EdgeKind::Annotated
                    && let Some(name) = ann_name_by_id.get(&edge.target)
                {
                    anns_by_owner.entry(&edge.source).or_default().push(*name);
                }
            }
            let mut embed_seen = std::collections::HashSet::new();
            combined
                .add_entities
                .iter()
                .filter(|e| embed_seen.insert(e.id.0.clone()))
                .map(|e| {
                    let mut text = format!("{} {} {}", e.kind.as_str(), e.qualified_name, e.name);
                    if let Some(anns) = anns_by_owner.get(&e.id)
                        && !anns.is_empty()
                    {
                        // 带 @ 前缀,贴近 Java 源码与用户查询习惯。
                        text.push(' ');
                        text.push_str(
                            &anns
                                .iter()
                                .map(|a| format!("@{a}"))
                                .collect::<Vec<_>>()
                                .join(" "),
                        );
                    }
                    (e.id.clone(), text)
                })
                .collect()
        } else {
            Vec::new()
        };
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
        let n_resolved = resolution.patch.add_edges.len();
        summary.edges_indexed += n_resolved;
        // 克隆跨文件结构边(resolved=1)供 transitive_loop_depth 传播 + 架构聚类用;
        // replace_resolved_edges 会 move 原始 Vec,后续分析需独立持有副本。
        let resolved_structural: Vec<Edge> = resolution
            .patch
            .add_edges
            .iter()
            .filter(|e| {
                matches!(
                    e.kind,
                    EdgeKind::Calls
                        | EdgeKind::Injects
                        | EdgeKind::Declares
                        | EdgeKind::SuperclassOf
                        | EdgeKind::Implements
                )
            })
            .cloned()
            .collect();
        let t = std::time::Instant::now();
        store.replace_resolved_edges(resolution.patch.add_edges)?;
        // A+ 歧义:被跳过的跨文件解析记录到请求方实体的 metadata.ambiguous_resolution
        // (而非强行建一条会误导的边)。消费方既看得到"哪些连接是歧义的、候选有哪些",
        // 又能用候选文件 + import 自行消歧;图本身保持"出现即可信"。
        summary.ambiguous_skipped = resolution.ambiguities.len();
        let amb_by_holder: HashMap<&EntityId, Vec<serde_json::Value>> = {
            let mut by_holder: HashMap<&EntityId, Vec<serde_json::Value>> = HashMap::new();
            for note in &resolution.ambiguities {
                by_holder.entry(&note.holder).or_default().push(json!({
                    "kind": note.kind,
                    "name": note.name,
                    "candidates": note.candidates,
                }));
            }
            by_holder
        };
        eprintln!(
            "[ri-diag] resolve_cross_stack: {} entities → {} edges in {:.2}s; replace_resolved in {:.2}s",
            all_entities.len(),
            n_resolved,
            t_resolve.as_secs_f64(),
            t.elapsed().as_secs_f64()
        );

        // transitive_loop_depth 沿 CALLS 传播(对标 codebase-memory):把单函数 loop_depth
        // 升级为调用链最坏嵌套度,跨函数发现 O(n²) 热点。resolved=0(同文件)+ resolved=1
        // (跨文件/桥接)的 calls 边都参与。复用 clone-as-map metadata 回填模式。
        let t = Instant::now();
        let all_calls: Vec<&Edge> = extract_edges
            .iter()
            .chain(resolved_structural.iter())
            .filter(|e| e.kind == EdgeKind::Calls)
            .collect();
        let tld = compute_transitive_loop_depth(&all_entities, &all_calls);
        eprintln!(
            "[ri-diag] transitive_loop_depth: {} methods, {} calls, in {:.2}s",
            tld.len(),
            all_calls.len(),
            t.elapsed().as_secs_f64()
        );

        // 架构聚类(label propagation):在代码结构依赖图(calls/injects/declares/
        // superclass_of/implements)上识别"事实模块"(对标 codebase-memory Leiden)。
        // 同文件 + 跨文件结构边都参与。cluster_id 写回 metadata 供 get_clusters 消费。
        let t = Instant::now();
        let structural: Vec<&Edge> = extract_edges
            .iter()
            .chain(resolved_structural.iter())
            .filter(|e| {
                matches!(
                    e.kind,
                    EdgeKind::Calls
                        | EdgeKind::Injects
                        | EdgeKind::Declares
                        | EdgeKind::SuperclassOf
                        | EdgeKind::Implements
                )
            })
            .collect();
        let clusters = compute_clusters(&structural);
        let n_clusters_distinct = clusters
            .values()
            .collect::<std::collections::HashSet<_>>()
            .len();
        // 元数据统一合并回填:transitive_loop_depth / cluster_id / ambiguous_resolution
        // 三路信号一次写入。此前各阶段各自从 resolve 前的快照整行覆盖实体,互相冲键
        // (如方法既有 tld 又入 cluster 时 tld 被后写阶段抹掉、ambiguous_resolution 被
        // cluster 阶段整体覆盖),改为单遍合并彻底消除该类丢失。
        let t_meta = std::time::Instant::now();
        let mut n_tld = 0;
        let mut n_cluster = 0;
        let mut updated: Vec<Entity> = Vec::new();
        for entity in &all_entities {
            let is_method_with_tld =
                entity.kind == EntityKind::Method && tld.contains_key(&entity.id);
            if !is_method_with_tld
                && !clusters.contains_key(&entity.id)
                && !amb_by_holder.contains_key(&entity.id)
            {
                continue;
            }
            let mut meta = match entity.metadata.clone() {
                serde_json::Value::Object(map) => map,
                _ => serde_json::Map::new(),
            };
            if is_method_with_tld {
                meta.insert("transitive_loop_depth".into(), json!(tld[&entity.id]));
                n_tld += 1;
            }
            if let Some(&cid) = clusters.get(&entity.id) {
                meta.insert("cluster_id".into(), json!(cid));
                n_cluster += 1;
            }
            if let Some(entries) = amb_by_holder.get(&entity.id) {
                meta.insert(
                    "ambiguous_resolution".into(),
                    serde_json::Value::Array(entries.clone()),
                );
            }
            let mut entity = entity.clone();
            entity.metadata = serde_json::Value::Object(meta);
            updated.push(entity);
        }
        let n_meta_rows = updated.len();
        if !updated.is_empty() {
            store.apply_patch(GraphPatch::add(updated, Vec::new()))?;
        }
        eprintln!(
            "[ri-diag] metadata merge-back: {n_meta_rows} rows (tld {n_tld}, cluster {n_cluster}, ambiguous {}) in {:.2}s",
            resolution.ambiguities.len(),
            t_meta.elapsed().as_secs_f64()
        );
        eprintln!(
            "[ri-diag] clusters: {n_cluster} entities → {n_clusters_distinct} clusters, in {:.2}s",
            t.elapsed().as_secs_f64()
        );

        // 异常流解析(对标 codebase-memory THROWS/HANDLES):method.metadata.exception_flow
        // 的 type name → class/interface 实体,建 throws/handles 边。同名唯一命中建边,
        // 歧义(多个同名类)跳过——同 calls A+ 策略,保证"出现即可信"。
        let t = Instant::now();
        let mut class_by_name: HashMap<&str, Vec<&EntityId>> = HashMap::new();
        for e in &all_entities {
            if matches!(e.kind, EntityKind::Class | EntityKind::Interface) {
                class_by_name
                    .entry(e.name.as_str())
                    .or_default()
                    .push(&e.id);
            }
        }
        let mut exc_edges: Vec<Edge> = Vec::new();
        for entity in &all_entities {
            if entity.kind != EntityKind::Method {
                continue;
            }
            let Some(flows) = entity
                .metadata
                .get("exception_flow")
                .and_then(|v| v.as_array())
            else {
                continue;
            };
            let Some(file) = entity.evidence.first().map(|ev| ev.file.as_str()) else {
                continue;
            };
            for flow in flows {
                let Some(type_name) = flow.get("type").and_then(|v| v.as_str()) else {
                    continue;
                };
                let Some(candidates) = class_by_name.get(type_name) else {
                    continue;
                };
                let [cid] = candidates.as_slice() else {
                    continue;
                }; // 歧义跳过
                let kind = if flow.get("flow").and_then(|v| v.as_str()) == Some("throws") {
                    EdgeKind::Throws
                } else {
                    EdgeKind::Handles
                };
                let line = flow.get("line").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                exc_edges.push(
                    Edge::new(entity.id.clone(), (*cid).clone(), kind).with_evidence(
                        file,
                        line,
                        line,
                        EvidenceClass::Inferred,
                        0.7,
                        "exception flow resolve",
                    ),
                );
            }
        }
        let n_exc = exc_edges.len();
        if !exc_edges.is_empty() {
            store.apply_patch(GraphPatch::add(Vec::new(), exc_edges))?;
        }
        eprintln!(
            "[ri-diag] exception_flow: {} throws/handles edges, in {:.2}s",
            n_exc,
            t.elapsed().as_secs_f64()
        );

        // 向量层:对本次变更实体生成 embedding(增量——仅 text_hash 变化才重新生成)。
        if config.index.embedding && !embed_inputs.is_empty() {
            let t = Instant::now();
            let state = store.get_embedding_state()?;
            // 筛 text_hash 变化(或新实体)。
            let to_embed: Vec<(EntityId, String)> = embed_inputs
                .into_iter()
                .filter(|(id, text)| {
                    let h = blake3::hash(text.as_bytes()).to_hex().to_string();
                    state.get(id).is_none_or(|old| *old != h)
                })
                .collect();
            let n = to_embed.len();
            if n > 0 {
                // 降级:模型加载/推理/存储任一失败 → warning 跳过,绝不阻塞 scan。
                // (模型缺失/损坏/OOM 不应让整个 scan 崩;FTS 等其他产物仍保留。)
                match repo_intelligence_embedding::Embedder::new().and_then(|mut embedder| {
                    let texts: Vec<String> = to_embed.iter().map(|(_, t)| t.clone()).collect();
                    embedder.embed(texts)
                }) {
                    Ok(vecs) => {
                        let rows: Vec<(EntityId, Vec<f32>, String)> = to_embed
                            .iter()
                            .zip(vecs)
                            .map(|((id, text), vec)| {
                                (
                                    id.clone(),
                                    vec,
                                    blake3::hash(text.as_bytes()).to_hex().to_string(),
                                )
                            })
                            .collect();
                        if let Err(e) = store.set_embeddings(&rows) {
                            eprintln!("[ri-diag] embedding 存储失败(不阻塞 scan): {e}");
                        }
                    }
                    Err(e) => {
                        eprintln!("[ri-diag] embedding 跳过(模型加载/推理失败,不阻塞 scan): {e}")
                    }
                }
            }
            eprintln!(
                "[ri-diag] embedding: {n} entities in {:.2}s",
                t.elapsed().as_secs_f64()
            );
        }

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

/// 沿 CALLS 边传播 transitive_loop_depth(调用链最坏循环嵌套度),对标 codebase-memory。
/// `tld(node) = own_loop_depth + max(tld(callee))`;固定点迭代到收敛,上限 32 轮防止
/// 互递归环无限累加(环上节点过估,反映递归风险,对热点发现可接受)。
fn compute_transitive_loop_depth<'a>(
    entities: &'a [Entity],
    calls_edges: &[&Edge],
) -> HashMap<&'a EntityId, u32> {
    let own: HashMap<&EntityId, u32> = entities
        .iter()
        .map(|e| (&e.id, own_loop_depth(e)))
        .collect();
    // 邻接:caller -> [callee],只保留两端都在 own 里的边(防悬空)。
    let mut out: HashMap<&EntityId, Vec<&EntityId>> = HashMap::new();
    for &e in calls_edges {
        if e.kind == EdgeKind::Calls && own.contains_key(&e.source) && own.contains_key(&e.target) {
            out.entry(&e.source).or_default().push(&e.target);
        }
    }
    let mut tld: HashMap<&EntityId, u32> = own.clone();
    for _ in 0..32 {
        let mut changed = false;
        let next: Vec<(&EntityId, u32)> = tld
            .keys()
            .map(|&node| {
                let o = own[node];
                let max_callee = out
                    .get(node)
                    .map(|cs| {
                        cs.iter()
                            .filter_map(|c| tld.get(c).copied())
                            .max()
                            .unwrap_or(0)
                    })
                    .unwrap_or(0);
                let prev = *tld.get(node).unwrap_or(&0);
                (node, o.saturating_add(max_callee).max(prev))
            })
            .collect();
        for (k, v) in &next {
            if tld.get(k).copied().unwrap_or(0) != *v {
                changed = true;
                break;
            }
        }
        for (k, v) in next {
            tld.insert(k, v);
        }
        if !changed {
            break;
        }
    }
    tld
}

/// 实体的自身循环嵌套深度(从 metadata.loop_depth 读;非方法或缺失 = 0)。
fn own_loop_depth(e: &Entity) -> u32 {
    if e.kind != EntityKind::Method {
        return 0;
    }
    e.metadata
        .get("loop_depth")
        .and_then(|v| v.as_u64())
        .map(|v| v.min(u32::MAX as u64) as u32)
        .unwrap_or(0)
}

/// 在代码结构依赖图(calls/injects/declares/superclass_of/implements)上跑 label
/// propagation 社区发现,识别跨文件夹的"事实模块"(对标 codebase-memory 的 Leiden,
/// 用更简单的 LPA 近似)。确定性同步迭代:每轮基于上一轮标签计数,tie-break 取最小
/// label;固定 20 轮兜底震荡。返回规整化后的连续 cluster 序号(0..K)。
fn compute_clusters(edges: &[&Edge]) -> HashMap<EntityId, u64> {
    use EdgeKind::*;
    let selected = [Calls, Injects, Declares, SuperclassOf, Implements];
    let mut nodes: HashSet<EntityId> = HashSet::new();
    let mut adj: HashMap<EntityId, Vec<EntityId>> = HashMap::new();
    for &e in edges {
        if selected.contains(&e.kind) {
            nodes.insert(e.source.clone());
            nodes.insert(e.target.clone());
            adj.entry(e.source.clone())
                .or_default()
                .push(e.target.clone());
            adj.entry(e.target.clone())
                .or_default()
                .push(e.source.clone());
        }
    }
    // 确定序:按 EntityId 字符串排序,初始 label = 序号。
    let mut order: Vec<EntityId> = nodes.iter().cloned().collect();
    order.sort_by(|a, b| a.0.cmp(&b.0));
    let mut label: HashMap<EntityId, u64> = HashMap::new();
    for (i, id) in order.iter().enumerate() {
        label.insert(id.clone(), i as u64);
    }
    for _ in 0..20 {
        let prev = label.clone();
        for node in &order {
            let mut counts: HashMap<u64, usize> = HashMap::new();
            *counts.entry(prev[node]).or_default() += 1; // 含自己
            if let Some(ns) = adj.get(node) {
                for n in ns {
                    *counts.entry(prev[n]).or_default() += 1;
                }
            }
            // 最高频;tie-break:count 降序,同 count 取 label 升序(min,确定)。
            if let Some((best, _)) = counts
                .into_iter()
                .max_by(|(l1, c1), (l2, c2)| c1.cmp(c2).then(l2.cmp(l1)))
            {
                label.insert(node.clone(), best);
            }
        }
        if label == prev {
            break;
        }
    }
    // 规整化:原始 label → 连续 cluster 序号(0..K),便于 get_clusters 输出。
    let mut ids: Vec<u64> = label.values().copied().collect();
    ids.sort_unstable();
    ids.dedup();
    let remap: HashMap<u64, u64> = ids
        .into_iter()
        .enumerate()
        .map(|(i, v)| (v, i as u64))
        .collect();
    label.into_iter().map(|(k, v)| (k, remap[&v])).collect()
}

fn resolve_cross_stack(entities: &[Entity], input_edges: &[Edge]) -> Resolution {
    let mut edges = Vec::new();
    let mut ambiguities: Vec<AmbiguityNote> = Vec::new();
    let mut fields: HashMap<String, Vec<&Entity>> = HashMap::new();
    let mut endpoints = Vec::new();
    let mut calls = Vec::new();
    let mut mappers: Vec<&Entity> = Vec::new();
    for entity in entities {
        match entity.kind {
            EntityKind::Field
            | EntityKind::FrontendField
            | EntityKind::ApiField
            | EntityKind::Column => {
                fields.entry(entity.name.clone()).or_default().push(entity);
            }
            EntityKind::SqlField => {
                // 排除 G1 的 where_join 列:它们是 statement-局部的"读了哪个列"(regex 推断),
                // 非跨层字段映射语义;其裸列名(id/name/status)若参与 name 分组会与 Java
                // Field/Column 跨层碰撞,污染 MappedFrom → impact/relay。SELECT-alias 仍参与。
                let is_where_join = entity
                    .metadata
                    .get("origin")
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| s == "where_join");
                if !is_where_join {
                    fields.entry(entity.name.clone()).or_default().push(entity);
                }
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
    // 类/接口按裸名全量分组 + 计数:A+ 歧义防护的依据(裸名跨包同名 = 跨文件解析的歧义源)。
    // 一次遍历建表,后续各处复用 len()/候选文件。
    let mut classes_by_name_all: HashMap<&str, Vec<&Entity>> = HashMap::new();
    let mut ifaces_by_name_all: HashMap<&str, Vec<&Entity>> = HashMap::new();
    for entity in entities {
        match entity.kind {
            EntityKind::Class => {
                classes_by_name_all
                    .entry(entity.name.as_str())
                    .or_default()
                    .push(entity);
            }
            EntityKind::Interface => {
                ifaces_by_name_all
                    .entry(entity.name.as_str())
                    .or_default()
                    .push(entity);
            }
            _ => {}
        }
    }
    // 候选文件清单(去重排序),供 A+ 歧义 note:消费方读候选文件 + import 自行消歧。
    let candidate_files = |list: &[&Entity]| -> Vec<String> {
        let mut out: Vec<String> = list
            .iter()
            .filter_map(|e| e.evidence.first().map(|ev| ev.file.clone()))
            .collect();
        out.sort();
        out.dedup();
        out
    };
    let mut class_to_table: HashMap<&str, &EntityId> = HashMap::new();
    for edge in input_edges {
        if edge.kind == EdgeKind::DependsOn
            && let (Some(src), Some(tgt)) = (
                entity_by_id.get(&edge.source),
                entity_by_id.get(&edge.target),
            )
            && src.kind == EntityKind::Class
            && tgt.kind == EntityKind::Table
        {
            // A+:同名实体类跨包 → 不入表(后面 mapper 侧记录歧义),宁可不连也不乱连。
            if classes_by_name_all
                .get(src.name.as_str())
                .is_some_and(|v| v.len() > 1)
            {
                continue;
            }
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
        } else if let Some(cands) = classes_by_name_all.get(entity_type).filter(|v| v.len() > 1) {
            // entity_type 是跨包同名类 → 无法确定绑哪张表,记歧义不连。
            ambiguities.push(AmbiguityNote {
                holder: mapper.id.clone(),
                kind: "mapper_table",
                name: entity_type.to_string(),
                candidates: candidate_files(cands),
            });
        }
    }

    // 跨文件 MyBatis 绑定:Mapper 接口方法 ↔ XmlStatement。原生 MyBatis 不走
    // @TableName/BaseMapper,Dao 方法→表的链路断在 method↔xml_statement。按 MyBatis 官方
    // 绑定规则(namespace + id)配对:namespace(=接口 Java FQN)后缀匹配 interface 的 path
    // (不依赖 Maven source-root 约定),再要求方法名 == statement_id,建 method→BindsToStatement。
    // namespace 权威、id 精确 → Resolved 0.9。
    let xml_by_ns_suffix: HashMap<String, Vec<&Entity>> = entities
        .iter()
        .filter(|e| e.kind == EntityKind::XmlStatement)
        .filter_map(|xs| {
            xs.metadata
                .get("namespace")
                .and_then(|v| v.as_str())
                .map(|ns| (format!("{}.java", ns.replace('.', "/")), xs))
        })
        .fold(HashMap::new(), |mut acc, (suffix, xs)| {
            acc.entry(suffix).or_default().push(xs);
            acc
        });
    // interface path → 其 namespace 命中的 XmlStatement 列表。
    let mut iface_to_xmls: HashMap<&EntityId, Vec<&Entity>> = HashMap::new();
    for edge in input_edges {
        if edge.kind != EdgeKind::Declares {
            continue;
        }
        let (Some(iface), Some(_)) = (
            entity_by_id.get(&edge.source),
            entity_by_id.get(&edge.target),
        ) else {
            continue;
        };
        if iface.kind != EntityKind::Interface || iface_to_xmls.contains_key(&iface.id) {
            continue;
        }
        let Some(ev) = iface.evidence.first() else {
            continue;
        };
        // Windows 路径分隔符归一:ev.file 在 Windows 上是反斜杠,namespace 后缀是
        // '/' 形式,不归一则 binds_to_statement 在 Windows 全空(CI 实测)。
        let file_norm = ev.file.replace('\\', "/");
        let matched: Vec<&Entity> = xml_by_ns_suffix
            .iter()
            .filter(|(suffix, _)| file_norm.ends_with(suffix.as_str()))
            .flat_map(|(_, xs)| xs.iter().copied())
            .collect();
        iface_to_xmls.insert(&iface.id, matched);
    }
    // declares(interface→method):method 名命中某 statement_id → 建绑定边。
    for edge in input_edges {
        if edge.kind != EdgeKind::Declares {
            continue;
        }
        let (Some(iface), Some(method)) = (
            entity_by_id.get(&edge.source),
            entity_by_id.get(&edge.target),
        ) else {
            continue;
        };
        if iface.kind != EntityKind::Interface || method.kind != EntityKind::Method {
            continue;
        }
        let Some(xmls) = iface_to_xmls.get(&iface.id) else {
            continue;
        };
        for xs in xmls {
            if xs.name != method.name {
                continue;
            }
            let mut b = Edge::new(method.id.clone(), xs.id.clone(), EdgeKind::BindsToStatement);
            if let Some(mev) = method.evidence.first() {
                b = b.with_evidence(
                    &mev.file,
                    mev.start_line,
                    mev.end_line,
                    EvidenceClass::Resolved,
                    0.9,
                    "MyBatis mapper method binds to statement (namespace + id)",
                );
            }
            edges.push(b);
        }
    }

    // 跨文件 method 调用:method.metadata.invokes 的 callee 名 + 所属类型的注入依赖
    // (DependsOn→SpringBean type)→ 在注入 type 声明的 method 里按名匹配,补
    // Controller→Service→Mapper 跨文件调用链(同文件 calls 由 extract_java 产)。
    // 低保真:方法名 + 注入类型匹配;同名歧义(多个注入 type 都有该方法)则跳过。
    // 复用上面的 entity_by_id(同源 entities slice,此前另建一份 entity_by_id_cf 是重复)。
    let mut type_methods: HashMap<&str, HashMap<&str, &EntityId>> = HashMap::new();
    let mut method_owner: HashMap<&EntityId, &str> = HashMap::new();
    for edge in input_edges {
        if edge.kind == EdgeKind::Declares
            && let (Some(owner), Some(m)) = (
                entity_by_id.get(&edge.source),
                entity_by_id.get(&edge.target),
            )
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
            && let (Some(owner), Some(bean)) = (
                entity_by_id.get(&edge.source),
                entity_by_id.get(&edge.target),
            )
            && matches!(owner.kind, EntityKind::Class | EntityKind::Interface)
            && bean.kind == EntityKind::SpringBean
        {
            owner_injected
                .entry(owner.name.as_str())
                .or_default()
                .push(bean.name.as_str());
        }
    }
    // 类型名出现次数:静态调用(receiver=类型名)解析时,同名跨包歧义则跳过避免乱连。
    let mut type_name_owners: HashMap<&str, usize> = HashMap::new();
    for e in entities {
        if matches!(e.kind, EntityKind::Class | EntityKind::Interface) {
            *type_name_owners.entry(e.name.as_str()).or_insert(0) += 1;
        }
    }
    // owner → {字段名 → 注入类型名}:Step B 把 `this.service.foo()` 的 receiver=service
    // 精确解析到注入类型,消除"多个注入 type 都有同名方法"的歧义(字段名直接锁定单一 type)。
    let mut owner_fields: HashMap<&str, HashMap<&str, &str>> = HashMap::new();
    for e in entities {
        if matches!(e.kind, EntityKind::Class | EntityKind::Interface)
            && let Some(arr) = e.metadata.get("injected_fields").and_then(|v| v.as_array())
        {
            let map = owner_fields.entry(e.name.as_str()).or_default();
            for f in arr {
                if let (Some(fname), Some(ftype)) = (
                    f.get("name").and_then(|v| v.as_str()),
                    f.get("type").and_then(|v| v.as_str()),
                ) {
                    map.insert(fname, ftype);
                }
            }
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
        // owner 可能无注入依赖(如纯静态工具调用 JsonUtil.stringify);injected 缺省为空,
        // 静态调用路径(receiver=类型名)不依赖它,不应被此处 continue 卡掉。
        let injected: &[&str] = owner_injected.get(owner).map(Vec::as_slice).unwrap_or(&[]);
        // A+:owner 类型名跨包歧义 → 其 type_methods/owner_injected/owner_fields 是被污染的
        // 聚合(两个同名类的条目互相覆盖/合并)。字段/注入两条路径在 owner 歧义时跳过;
        // 静态调用路径(priority 1)自带歧义防护,不受影响。记一条 note(按 holder+name 去重)。
        let owner_ambiguous = type_name_owners.get(owner).copied().unwrap_or(0) > 1;
        if owner_ambiguous {
            let mut cands: Vec<&Entity> = Vec::new();
            if let Some(v) = classes_by_name_all.get(owner) {
                cands.extend(v.iter().copied());
            }
            if let Some(v) = ifaces_by_name_all.get(owner) {
                cands.extend(v.iter().copied());
            }
            ambiguities.push(AmbiguityNote {
                holder: entity.id.clone(),
                kind: "call_resolution",
                name: owner.to_string(),
                candidates: candidate_files(&cands),
            });
        }
        for invoke in invokes {
            let Some(callee_name) = invoke.get("name").and_then(|v| v.as_str()) else {
                continue;
            };
            let line = invoke.get("line").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            // receiver_kind/receiver 由提取层 classify_receiver 产出(旧库缺省为 bare,向后兼容)。
            let receiver_kind = invoke
                .get("receiver_kind")
                .and_then(|v| v.as_str())
                .unwrap_or("bare");
            let receiver = invoke.get("receiver").and_then(|v| v.as_str());

            let mut resolved: Option<&EntityId> = None;
            let mut confidence: f32 = 0.0;
            let mut reason: &str = "";
            // 优先级 0:注入字段调用。receiver=字段名 → 用字段名查 owner 的注入类型,
            // 再定位该 type 的方法,精确解析 0.7(消除多 type 同名方法歧义)。
            // 覆盖两种写法:field=this.service.foo(显式 this)与 name=service.foo
            // (Java 主流省略 this——identifier receiver 先当注入字段试,再当静态类型)。
            let mut via_field = false;
            // owner 不歧义 + 注入字段类型本身也不歧义,才走精确字段路径。
            if let Some(rv) = receiver
                && matches!(receiver_kind, "field" | "name")
                && !owner_ambiguous
                && let Some(field_map) = owner_fields.get(owner)
                && let Some(&target_type) = field_map.get(rv)
                && type_name_owners.get(target_type).copied().unwrap_or(0) <= 1
                && let Some(ms) = type_methods.get(target_type)
                && let Some(&callee_id) = ms.get(callee_name)
                && callee_id != &entity.id
            {
                resolved = Some(callee_id);
                confidence = 0.7;
                reason = "cross-file call via injected field receiver";
                via_field = true;
            }
            // 优先级 1:静态调用(receiver 是全局类型名且非歧义,如 JsonUtil.foo)→ 精确 0.7。
            if !via_field
                && receiver_kind == "name"
                && let Some(rv) = receiver
                && type_name_owners.get(rv).copied().unwrap_or(0) <= 1
                && let Some(ms) = type_methods.get(rv)
                && let Some(&callee_id) = ms.get(callee_name)
                && callee_id != &entity.id
            {
                resolved = Some(callee_id);
                confidence = 0.7;
                reason = "cross-file static call on named type";
            }
            // 优先级 2:裸名 / 未精确解析的 name → 注入依赖类型名匹配(低保真),唯一命中 0.5。
            // A+:owner 歧义时其 injected 列表被污染 → 跳过;注入类型自身歧义的不参与 hits。
            if resolved.is_none() && !owner_ambiguous {
                let mut hits: Vec<&EntityId> = Vec::new();
                for type_name in injected {
                    if type_name_owners.get(*type_name).copied().unwrap_or(0) > 1 {
                        continue;
                    }
                    if let Some(ms) = type_methods.get(*type_name)
                        && let Some(callee_id) = ms.get(callee_name).copied()
                    {
                        hits.push(callee_id);
                    }
                }
                // 无 receiver 信息时多命中=歧义跳过避免连错;唯一命中且非自调用才建边。
                if hits.len() == 1 && hits[0] != &entity.id {
                    resolved = Some(hits[0]);
                    confidence = 0.5;
                    reason = "cross-file call via injected dependency (matched by name)";
                }
            }
            if let Some(callee_id) = resolved
                && let Some(ev) = entity.evidence.first()
            {
                edges.push(
                    Edge::new(entity.id.clone(), callee_id.clone(), EdgeKind::Calls).with_evidence(
                        &ev.file,
                        line,
                        line,
                        EvidenceClass::Inferred,
                        confidence,
                        reason,
                    ),
                );
            }
        }
    }

    // implements:跨文件 class.metadata.implements → 全局 interface 实体(EntityId 是
    // path-scoped,extract 层建不了跨文件边,故 metadata 传递 + 这里按 interface 名解析)。
    // 同时建 class→interface depends_on 边 + 反向索引 interface→impl,供下面的桥接。
    // A+:interface 裸名跨包同名(两个 Handler 接口)→ 只保留无歧义的;
    // 命中歧义名的 class 记 note 不连边。原先 HashMap::collect 在重名上后写覆盖,
    // 且 implements 边以 1.0/Fact 输出 —— 这是最严重的语义反转(最不确定拿了最高置信)。
    let interfaces_by_name: HashMap<&str, &EntityId> = ifaces_by_name_all
        .iter()
        .filter(|(_, v)| v.len() == 1)
        .map(|(name, v)| (*name, &v[0].id))
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
            let Some(iface_name) = iface_val.as_str() else {
                continue;
            };
            if let Some(cands) = ifaces_by_name_all.get(iface_name).filter(|v| v.len() > 1) {
                ambiguities.push(AmbiguityNote {
                    holder: entity.id.clone(),
                    kind: "implements",
                    name: iface_name.to_string(),
                    candidates: candidate_files(cands),
                });
                continue;
            }
            let Some(&iface_id) = interfaces_by_name.get(iface_name) else {
                continue;
            };
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
            // 反向 Implements 边(interface→class):让"接口有哪些实现"可查(P1-5 MapStruct
            // Impl 等编译时生成代码补全后,接口到实现的显式语义)。
            let mut impl_edge =
                Edge::new(iface_id.clone(), entity.id.clone(), EdgeKind::Implements);
            if let Some(ev) = entity.evidence.first() {
                impl_edge = impl_edge.with_evidence(
                    &ev.file,
                    ev.start_line,
                    ev.end_line,
                    EvidenceClass::Fact,
                    1.0,
                    "interface implemented by class",
                );
            }
            edges.push(impl_edge);
            interface_to_impls
                .entry(iface_name)
                .or_default()
                .push(entity.name.as_str());
        }
    }

    // 类继承:跨文件 class.metadata.superclass → 全局超类实体,建 SuperclassOf(超类→子类,
    // outbound)。让 trace 从超类(含 abstract 抽象基类)下钻到具体子类——业务逻辑常在子类,
    // abstract 类自身方法不直接调 Dao,需经此边追到子类的表依赖。与 implements 同模式。
    let classes_by_name: HashMap<&str, &EntityId> = classes_by_name_all
        .iter()
        .filter(|(_, v)| v.len() == 1)
        .map(|(name, v)| (*name, &v[0].id))
        .collect();
    for entity in entities {
        if entity.kind != EntityKind::Class {
            continue;
        }
        let Some(superclass) = entity.metadata.get("superclass").and_then(|v| v.as_str()) else {
            continue;
        };
        // A+:同名超类跨包歧义 → 记 note 不连(此前静默跳过,缺席不可见)。
        if classes_by_name_all
            .get(superclass)
            .is_some_and(|v| v.len() > 1)
        {
            ambiguities.push(AmbiguityNote {
                holder: entity.id.clone(),
                kind: "superclass",
                name: superclass.to_string(),
                candidates: candidate_files(
                    classes_by_name_all
                        .get(superclass)
                        .map(Vec::as_slice)
                        .unwrap_or(&[]),
                ),
            });
            continue;
        }
        let Some(&super_id) = classes_by_name.get(superclass) else {
            continue;
        };
        let mut edge = Edge::new(super_id.clone(), entity.id.clone(), EdgeKind::SuperclassOf);
        if let Some(ev) = entity.evidence.first() {
            edge = edge.with_evidence(
                &ev.file,
                ev.start_line,
                ev.end_line,
                EvidenceClass::Fact,
                1.0,
                "class extends superclass",
            );
        }
        edges.push(edge);
    }
    for edge in input_edges {
        if edge.kind == EdgeKind::Declares
            && let (Some(iface), Some(m)) = (
                entity_by_id.get(&edge.source),
                entity_by_id.get(&edge.target),
            )
            && iface.kind == EntityKind::Interface
            && m.kind == EntityKind::Method
            && let Some(impls) = interface_to_impls.get(iface.name.as_str())
        {
            let mut hits: Vec<&EntityId> = Vec::new();
            for impl_name in impls {
                // A+:实现类裸名跨包同名 → type_methods[impl_name] 被污染,不参与命中。
                if classes_by_name_all
                    .get(*impl_name)
                    .is_some_and(|v| v.len() > 1)
                {
                    continue;
                }
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
    // Mapper method → Table:复用上面的 class_to_table(同源 DependsOn class→table 边,
    // 含相同 A+ 歧义过滤);不再单独构建第二份(此前与 class_to_table 重复且各自 last-writer-wins)。
    let class_to_table_m2t = &class_to_table;
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
        let (Some(owner), Some(method)) = (
            entity_by_id.get(&edge.source),
            entity_by_id.get(&edge.target),
        ) else {
            continue;
        };
        if owner.kind != EntityKind::Interface || method.kind != EntityKind::Method {
            continue;
        }
        let Some(ev) = owner.evidence.first() else {
            continue;
        };
        let Some(mapper) = mapper_by_key.get(&(owner.name.as_str(), ev.file.as_str())) else {
            continue;
        };
        let Some(entity_type) = mapper.metadata.get("entity_type").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(table_id) = class_to_table_m2t.get(entity_type) else {
            // A+:entity_type 是跨包同名类 → 记 note(mapper 侧;与上面 mapper 循环的 note 去重)。
            if let Some(cands) = classes_by_name_all.get(entity_type).filter(|v| v.len() > 1) {
                ambiguities.push(AmbiguityNote {
                    holder: mapper.id.clone(),
                    kind: "mapper_table",
                    name: entity_type.to_string(),
                    candidates: candidate_files(cands),
                });
            }
            continue;
        };
        let mut m2t = Edge::new(
            edge.target.clone(),
            (*table_id).clone(),
            EdgeKind::ReadsTable,
        );
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

    // HTTP call×endpoint 配对按 method 分桶:endpoint 无显式动词(@RequestMapping 通配)
    // 进 wildcard(与任意 call_method 配),其余按动词进桶。call 只与同动词桶 + wildcard
    // 配对,砍掉 O(calls × 全部 endpoints) 的全配对——前端密集仓库(vue/ts axios 调用 +
    // 端点都很多)时是 scan 的可见常数因子。
    let mut endpoints_by_method: HashMap<&str, Vec<&Entity>> = HashMap::new();
    let wildcard_endpoints: Vec<&Entity> = endpoints
        .iter()
        .copied()
        .filter(|endpoint| {
            endpoint
                .metadata
                .get("method")
                .and_then(|value| value.as_str())
                .map(|m| m.is_empty())
                .unwrap_or(true)
        })
        .collect();
    for endpoint in endpoints.iter().copied() {
        if let Some(m) = endpoint
            .metadata
            .get("method")
            .and_then(|value| value.as_str())
            && !m.is_empty()
        {
            endpoints_by_method.entry(m).or_default().push(endpoint);
        }
    }
    for call in calls {
        let call_method = call.metadata.get("method").and_then(|value| value.as_str());
        let call_path = call.metadata.get("path").and_then(|value| value.as_str());
        // 候选 = wildcard 通配端点 + 同动词端点。
        let mut candidates: Vec<&Entity> = wildcard_endpoints.clone();
        if let Some(m) = call_method
            && let Some(bucket) = endpoints_by_method.get(m)
        {
            candidates.extend_from_slice(bucket);
        }
        for endpoint in candidates {
            let endpoint_method = endpoint
                .metadata
                .get("method")
                .and_then(|value| value.as_str());
            // endpoint 无显式 HTTP 动词(@RequestMapping 无 method)→ 视为通配,与任意
            // 前端 call_method 匹配(下游 match_kind 降置信);否则要求 method 相等。
            let endpoint_unspecified = endpoint_method.map(|m| m.is_empty()).unwrap_or(true);
            if !endpoint_unspecified && call_method != endpoint_method {
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
                // endpoint 动词通配(@RequestMapping)时降置信并标 Inferred/tentative。
                let (classification, confidence, reason) = if endpoint_unspecified {
                    (
                        EvidenceClass::Inferred,
                        confidence * 0.6,
                        "path matches; endpoint verb unspecified (wildcard)",
                    )
                } else {
                    (classification, confidence, reason)
                };
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

    // Tests 边(P1-4):命名约定 XxxTest → Xxx(被测类)。仅对以 Test 结尾的 class 建,
    // 让"这个类被哪些测试覆盖"可查(Inferred,命名约定)。
    // A+:被测类名跨包同名 → 命中靠命名约定的 Tests 边会连到任意一个,记 note 不连。
    let class_by_name: HashMap<&str, &EntityId> = classes_by_name_all
        .iter()
        .filter(|(_, v)| v.len() == 1)
        .map(|(name, v)| (*name, &v[0].id))
        .collect();
    for entity in entities {
        if entity.kind != EntityKind::Class {
            continue;
        }
        let Some(stripped) = entity.name.strip_suffix("Test").filter(|s| !s.is_empty()) else {
            continue;
        };
        if classes_by_name_all
            .get(stripped)
            .is_some_and(|v| v.len() > 1)
        {
            ambiguities.push(AmbiguityNote {
                holder: entity.id.clone(),
                kind: "test_convention",
                name: stripped.to_string(),
                candidates: candidate_files(
                    classes_by_name_all
                        .get(stripped)
                        .map(Vec::as_slice)
                        .unwrap_or(&[]),
                ),
            });
            continue;
        }
        if let Some(&tested_id) = class_by_name.get(stripped) {
            let mut edge = Edge::new(entity.id.clone(), tested_id.clone(), EdgeKind::Tests);
            if let Some(ev) = entity.evidence.first() {
                edge = edge.with_evidence(
                    &ev.file,
                    ev.start_line,
                    ev.end_line,
                    EvidenceClass::Inferred,
                    0.7,
                    "test class covers target (naming convention)",
                );
            }
            edges.push(edge);
        }
    }

    // Tests 边第二推断(P0②):测试类 metadata.imports 的简单名末段若在项目内唯一命中一个
    // class(排除自身、且不与命名约定路径重复),Inferred 0.6 建立 测试类→被测类。
    // 简单名多命中 = 跨包同名 → A+ 拒边,记 kind=test_import 歧义注记,消费方可结合
    // import 全限定名自行消歧(与 calls/implements/superclass 的 A+ 策略同款)。
    // 仅对测试文件里的 class 生效(判定:@Test 产出的 TestCase 与类同文件)——
    // metadata.imports 是全类记录的文件级信号,不筛会把普通业务类的 import 图误当覆盖关系。
    let test_case_files: HashSet<&str> = entities
        .iter()
        .filter(|e| e.kind == EntityKind::TestCase)
        .filter_map(|e| e.evidence.first().map(|ev| ev.file.as_str()))
        .collect();
    for entity in entities {
        if entity.kind != EntityKind::Class {
            continue;
        }
        if !entity
            .evidence
            .first()
            .is_some_and(|ev| test_case_files.contains(ev.file.as_str()))
        {
            continue;
        }
        let Some(imports) = entity.metadata.get("imports").and_then(|v| v.as_array()) else {
            continue;
        };
        let mut candidates: Vec<&str> = imports
            .iter()
            .filter_map(|value| value.as_str())
            .map(|fq| fq.rsplit('.').next().unwrap_or(fq))
            .collect();
        candidates.sort_unstable();
        candidates.dedup();
        for simple_name in candidates {
            if entity.name.strip_suffix("Test") == Some(simple_name) {
                continue; // 命名约定路径已覆盖(XxxTest → Xxx)
            }
            let Some(hits) = classes_by_name_all.get(simple_name) else {
                continue;
            };
            if hits.len() > 1 {
                ambiguities.push(AmbiguityNote {
                    holder: entity.id.clone(),
                    kind: "test_import",
                    name: simple_name.to_string(),
                    candidates: candidate_files(hits),
                });
                continue;
            }
            if hits[0].id == entity.id {
                continue;
            }
            let mut edge = Edge::new(entity.id.clone(), hits[0].id.clone(), EdgeKind::Tests);
            if let Some(ev) = entity.evidence.first() {
                edge = edge.with_evidence(
                    &ev.file,
                    ev.start_line,
                    ev.end_line,
                    EvidenceClass::Inferred,
                    0.6,
                    "test class imports target",
                );
            }
            edges.push(edge);
        }
    }

    // Intercepts 边(P1-2):aspect advice method.metadata.pointcut(全限定方法签名)→
    // 目标方法。取签名末段作方法名全局匹配,唯一命中才建边(Inferred,pointcut 解析有限)。
    let method_by_name: HashMap<&str, Vec<&EntityId>> = {
        let mut map: HashMap<&str, Vec<&EntityId>> = HashMap::new();
        for entity in entities {
            if entity.kind == EntityKind::Method {
                map.entry(entity.name.as_str())
                    .or_default()
                    .push(&entity.id);
            }
        }
        map
    };
    for entity in entities {
        if entity.kind != EntityKind::Method {
            continue;
        }
        let Some(pointcut) = entity.metadata.get("pointcut").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(target_name) = pointcut.rsplit('.').next() else {
            continue;
        };
        if let Some(targets) = method_by_name.get(target_name)
            && targets.len() == 1
            && targets[0] != &entity.id
        {
            let mut edge = Edge::new(entity.id.clone(), targets[0].clone(), EdgeKind::Intercepts);
            if let Some(ev) = entity.evidence.first() {
                edge = edge.with_evidence(
                    &ev.file,
                    ev.start_line,
                    ev.end_line,
                    EvidenceClass::Inferred,
                    0.5,
                    "AOP advice intercepts target (pointcut signature match)",
                );
            }
            edges.push(edge);
        }
    }

    // A+ note 去重:同一实体同一 (kind,name) 可能被多个 invoke / 多个 method 触发,
    // 合并成一条,避免 metadata.ambiguous_resolution 噪声膨胀。
    let mut seen: HashSet<(String, &'static str, String)> = HashSet::new();
    ambiguities.retain(|note| seen.insert((note.holder.0.clone(), note.kind, note.name.clone())));
    Resolution {
        patch: GraphPatch::add(Vec::new(), edges),
        ambiguities,
    }
}

/// 一条因裸名歧义被跳过的跨文件解析(A+ 策略)。`holder` = 请求方实体(边本该从它出发),
/// 写回其 metadata.ambiguous_resolution;`candidates` = 同名候选所在的文件,供消费方读
/// import 自行消歧。`kind` 标解析类别(implements/mapper_table/superclass/call_resolution/test_convention)。
#[derive(Clone, Debug)]
pub struct AmbiguityNote {
    pub holder: EntityId,
    pub kind: &'static str,
    pub name: String,
    pub candidates: Vec<String>,
}

/// resolve_cross_stack 的产物:跨文件边 patch + 因歧义被跳过的解析清单。
pub struct Resolution {
    pub patch: GraphPatch,
    pub ambiguities: Vec<AmbiguityNote>,
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
    if endpoint_segments.is_empty() {
        return false;
    }
    let diff = call_segments.len().saturating_sub(endpoint_segments.len());
    // 段数差 0 = 精确全等(归精确分支);>3 = 长尾误连(如 /a 误连 /x/y/z/w/a),不连。
    if diff == 0 || diff > 3 {
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
            // path_set 与 path 同步:HashSet 做 O(1) 成员判定,避免大图上
            // path.contains 的 O(n²)(宽图 depth=2 可达数百实体)。
            let mut path_set: HashSet<EntityId> = HashSet::new();
            path_set.insert(entity.id.clone());
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
                    if path_set.insert(related.id.clone()) {
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
                && let Some(file_id) = containing_file
            {
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
                    if path_set.insert(related.id.clone()) {
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
