//! 语义提取层入口。共享 helper + 框架提取器注册表分发。
//! 各框架提取逻辑在子模块:`java` / `xml` / `frontend` / `build`。
//! 新增语言/框架提取器:实现 `SemanticExtractor` + `Registry::register`。

mod build;
mod frontend;
mod java;
mod registry;
mod xml;

pub use registry::{ExtractContext, Registry, SemanticExtractor};

use anyhow::Result;
use regex::Regex;
use repo_intelligence_config::SemanticsConfig;
use repo_intelligence_model::{Edge, EdgeKind, Entity, EntityKind, EvidenceClass, GraphPatch};
use repo_intelligence_source::SourceFile;
use serde_json::json;

pub fn extract(file: &SourceFile) -> Result<GraphPatch> {
    extract_with_config(file, &SemanticsConfig::default())
}

/// 按 `SemanticsConfig` 提取语义:用 `Registry::default_java_stack()` 分发到
/// 首个支持该文件类型的提取器。自研 RPC 注解与前端噪声词从配置读取。
pub fn extract_with_config(
    file: &SourceFile,
    config: &SemanticsConfig,
) -> Result<GraphPatch> {
    let path = file.relative_path.to_string_lossy().to_string();
    let file_entity = base_file_entity(file, &path);
    let mut entities = vec![file_entity];
    let mut edges = Vec::new();
    let ctx = ExtractContext { config };
    Registry::default_java_stack().extract(&ctx, file, &path, &mut entities, &mut edges)?;
    Ok(GraphPatch::add(entities, edges))
}

/// File 实体(每个源文件的基础实体,所有提取器产出的实体由 Contains 边挂到它)。
pub(crate) fn base_file_entity(file: &SourceFile, path: &str) -> Entity {
    Entity::new(
        file.id.clone(),
        EntityKind::File,
        file.relative_path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(path),
        path,
    )
    .with_metadata(json!({"content_hash": file.content_hash}))
    .with_evidence(path, 1, 1, EvidenceClass::Fact, 1.0, "discovered source file")
}

pub(crate) fn line_of(content: &str, offset: usize) -> u32 {
    content[..offset.min(content.len())]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count() as u32
        + 1
}

pub(crate) fn add_contained(
    file: &SourceFile,
    path: &str,
    entity: Entity,
    line: u32,
    entities: &mut Vec<Entity>,
    edges: &mut Vec<Edge>,
) {
    edges.push(
        Edge::new(file.id.clone(), entity.id.clone(), EdgeKind::Contains).with_evidence(
            path,
            line,
            line,
            EvidenceClass::Fact,
            1.0,
            "declared in file",
        ),
    );
    entities.push(entity);
}

pub fn normalize_path(path: &str) -> String {
    let parameter = Regex::new(r"\{[^}]+\}|\$\{[^}]+\}|\b\d+\b").unwrap();
    let normalized = parameter.replace_all(path, "{}");
    let mut value = format!("/{}", normalized.trim_matches('/'));
    while value.contains("//") {
        value = value.replace("//", "/");
    }
    value
}
