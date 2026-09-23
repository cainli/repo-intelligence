//! 语义提取层入口。共享 helper + 框架提取器注册表分发。
//! 各框架提取逻辑在子模块:`java` / `xml` / `frontend` / `build`。
//! 新增语言/框架提取器:实现 `SemanticExtractor` + `Registry::register`。

mod build;
mod frontend;
mod java;
mod registry;
mod xml;

pub use registry::{ExtractContext, Registry, SemanticExtractor};
pub use repo_intelligence_config::SemanticsConfig;

use anyhow::Result;
use regex::Regex;
use repo_intelligence_model::{
    Edge, EdgeKind, Entity, EntityKind, Evidence, EvidenceClass, GraphPatch,
};
use repo_intelligence_source::SourceFile;
use serde_json::json;

pub fn extract(file: &SourceFile) -> Result<GraphPatch> {
    extract_with_config(file, &SemanticsConfig::default())
}

/// qualified_name 展示字段统一口径:`{归一 path}#{name}`。Windows 反斜杠归一为 `/`,
/// 跨平台查询稳定(v0.1.45 page_by_path 同病同修;此前仅 method/frontend 路径化,
/// class/bean/endpoint/table/mapper 裸名在同名类仓里无法区分归属——第三轮反馈 P1-C)。
/// 注意:EntityId::stable 的 qn 槽维持裸名(身份判别输入,与展示字段解耦);
/// 改展示格式不动 id,但须升 INDEX_FORMAT 强制全量重提刷新存量行。
pub(crate) fn path_qualified(path: &str, name: &str) -> String {
    format!("{}#{}", path.replace('\\', "/"), name)
}

/// 按 `SemanticsConfig` 提取语义:用 `Registry::default_java_stack()` 分发到
/// 首个支持该文件类型的提取器。自研 RPC 注解与前端噪声词从配置读取。
pub fn extract_with_config(file: &SourceFile, config: &SemanticsConfig) -> Result<GraphPatch> {
    let path = file.relative_path.to_string_lossy().to_string();
    let file_entity = base_file_entity(file, &path);
    let mut entities = vec![file_entity];
    let mut edges = Vec::new();
    let ctx = ExtractContext { config };
    Registry::default_java_stack().extract(&ctx, file, &path, &mut entities, &mut edges)?;
    // 填 snippet:单文件证据(file==path)从 file.content 取对应行;跨文件 resolve 边
    // 的 evidence file 非当前文件,留 None(它们在 scan 层产生,无 content 可读)。
    fill_snippets(&mut entities, &mut edges, &path, &file.content);
    Ok(GraphPatch::add(entities, edges))
}

/// 给本文件产出的证据填充 `snippet`:取 start_line 对应的源码行(trim 缩进)。
/// 只填 file==path 的证据,跨文件证据(file 指向别处)不动。
fn fill_snippets(entities: &mut [Entity], edges: &mut [Edge], path: &str, content: &str) {
    let lines: Vec<&str> = content.lines().collect();
    let fill = |evidence: &mut Evidence| {
        if evidence.snippet.is_none()
            && evidence.file == path
            && let Some(line) = lines.get(evidence.start_line.saturating_sub(1) as usize)
        {
            evidence.snippet = Some(line.trim().to_string());
        }
    };
    for entity in entities.iter_mut() {
        for evidence in entity.evidence.iter_mut() {
            fill(evidence);
        }
    }
    for edge in edges.iter_mut() {
        for evidence in edge.evidence.iter_mut() {
            fill(evidence);
        }
    }
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
    .with_evidence(
        path,
        1,
        1,
        EvidenceClass::Fact,
        1.0,
        "discovered source file",
    )
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
