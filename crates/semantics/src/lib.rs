use std::sync::LazyLock;

use anyhow::Result;
use regex::Regex;
use repo_intelligence_model::{
    Edge, EdgeKind, Entity, EntityId, EntityKind, EvidenceClass, GraphPatch,
};
use repo_intelligence_parsing::{Extractor, JavaParser};
use repo_intelligence_source::{FileKind, SourceFile};
use serde_json::json;
use std::collections::HashMap;
use tree_sitter::Node;

static JAVA_CLASS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(class|interface)\s+([A-Za-z_]\w*)").unwrap());
static JAVA_FIELD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)\b(?:private|protected|public)\s+[\w<>,.?]+\s+([A-Za-z_]\w*)\s*;").unwrap()
});
static REQUEST_MAPPING: LazyLock<Regex> = LazyLock::new(|| {
    // 类级 base 路径。放宽以覆盖三种写法:`@RequestMapping("/x")`、
    // `@RequestMapping(value = "/x", ...)`、`@RequestMapping({"/a","/b"})`(取首个)。
    // 不要求闭合 `)`,以容忍 `value="/x", method=POST` 这类带额外参数的形式。
    Regex::new(r#"@RequestMapping\(\s*(?:value\s*=\s*)?"([^"]+)""#).unwrap()
});
static METHOD_MAPPING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"@(Get|Post|Put|Delete|Patch)Mapping\(\s*"([^"]*)"\s*\)"#).unwrap()
});
static XML_STATEMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<(select|insert|update|delete)\b[^>]*\bid="([^"]+)"[^>]*>(.*?)</(?:select|insert|update|delete)>"#).unwrap()
});
static SQL_ALIAS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b([A-Za-z_]\w*)\s+AS\s+([A-Za-z_]\w*)").unwrap());
static SQL_FROM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:FROM|JOIN|UPDATE|INTO)\s+([A-Za-z_][\w.]*)").unwrap());
static VUE_BINDING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[A-Za-z_]\w*\.([A-Za-z_]\w*)").unwrap());
static HTTP_CALL: LazyLock<Regex> = LazyLock::new(|| {
    // 放宽调用者前缀:除裸 get/post 和 axios.xxx 外,也认封装 client
    // (request.get / http.get / this.$http.post)。代价:非 HTTP 的同名调用
    // (如 map.get("key"))会被误捕为孤立 http_client_call;这类节点匹配不到
    // 端点就不产生边,下游无影响,作为可接受的召回换精度权衡。
    Regex::new(
        r#"(?i)\b(?:[\w$]*(?:\.[\w$]+|\[[^\]]+\])*\.)?(get|post|put|delete|patch)\(\s*["'`]([^"'`]+)["'`]"#,
    )
    .unwrap()
});

/// Framework-specific endpoint annotations recognized in addition to Spring
/// MVC. The first string literal in an annotation's argument list (if any) is
/// taken as the endpoint identifier. Add entries here to teach the indexer
/// about in-house RPC frameworks (RMB `@RmbMap`, Dubbo, ...) so the API view
/// and `find_endpoint` work on non-Spring-MVC services.
const CUSTOM_ENDPOINT_ANNOTATIONS: &[&str] = &["RmbMap", "DubboService", "RpcMapping"];

static CUSTOM_ENDPOINT: LazyLock<Regex> = LazyLock::new(|| {
    let alternation = CUSTOM_ENDPOINT_ANNOTATIONS.join("|");
    Regex::new(&format!(r#"@({alternation})\b\s*(?:\(([^)]*)\))?"#)).unwrap()
});

static STRING_LITERAL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""([^"]*)""#).unwrap());

pub fn extract(file: &SourceFile) -> Result<GraphPatch> {
    let path = file.relative_path.to_string_lossy().to_string();
    let file_entity = Entity::new(
        file.id.clone(),
        EntityKind::File,
        file.relative_path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(&path),
        &path,
    )
    .with_metadata(json!({"content_hash": file.content_hash}))
    .with_evidence(
        &path,
        1,
        1,
        EvidenceClass::Fact,
        1.0,
        "discovered source file",
    );
    let mut entities = vec![file_entity];
    let mut edges = Vec::new();
    match file.kind {
        FileKind::Java => extract_java(file, &path, &mut entities, &mut edges)?,
        FileKind::Xml => extract_xml(file, &path, &mut entities, &mut edges),
        FileKind::Vue | FileKind::JavaScript | FileKind::TypeScript => {
            extract_frontend(file, &path, &mut entities, &mut edges)
        }
        _ => {}
    }
    Ok(GraphPatch::add(entities, edges))
}

fn line_of(content: &str, offset: usize) -> u32 {
    content[..offset.min(content.len())]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count() as u32
        + 1
}

fn add_contained(
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

fn extract_java(
    file: &SourceFile,
    path: &str,
    entities: &mut Vec<Entity>,
    edges: &mut Vec<Edge>,
) -> Result<()> {
    let parsed = JavaParser.parse(file)?;
    let syntax_confidence = if parsed.has_syntax_errors { 0.8 } else { 1.0 };
    // 先跑 AST 遍历:产出 Bean DI 边 + 收集 Spring 信号(事务/定时),后者作为
    // metadata 挂到对应 class 实体,故必须在 class 实体创建前完成。
    let mut signals: HashMap<String, SpringSignals> = HashMap::new();
    if let Some(tree) = parsed.tree.as_ref() {
        visit_spring(
            tree.root_node(),
            file.content.as_bytes(),
            file,
            path,
            entities,
            edges,
            &mut signals,
        );
    }
    for capture in JAVA_CLASS.captures_iter(&file.content) {
        let name = capture.get(2).unwrap();
        let kind = if &capture[1] == "interface" {
            EntityKind::Interface
        } else {
            EntityKind::Class
        };
        let line = line_of(&file.content, name.start());
        let mut entity = Entity::new(
            EntityId::stable("workspace", path, kind, name.as_str(), ""),
            kind,
            name.as_str(),
            name.as_str(),
        );
        if let Some(sig) = signals.get(name.as_str()) {
            let mut meta = serde_json::Map::new();
            if sig.transactional {
                meta.insert("transactional".into(), json!(true));
            }
            if sig.scheduled {
                meta.insert("scheduled".into(), json!(true));
            }
            if !meta.is_empty() {
                entity = entity.with_metadata(serde_json::Value::Object(meta));
            }
        }
        let entity = entity.with_evidence(
            path,
            line,
            line,
            EvidenceClass::Fact,
            syntax_confidence,
            "Java type declaration",
        );
        add_contained(file, path, entity, line, entities, edges);
    }
    for capture in JAVA_FIELD.captures_iter(&file.content) {
        let name = capture.get(1).unwrap();
        let line = line_of(&file.content, name.start());
        let entity = Entity::new(
            EntityId::stable("workspace", path, EntityKind::Field, name.as_str(), ""),
            EntityKind::Field,
            name.as_str(),
            format!("{path}#{}", name.as_str()),
        )
        .with_evidence(
            path,
            line,
            line,
            EvidenceClass::Fact,
            syntax_confidence,
            "Java field declaration",
        );
        add_contained(file, path, entity, line, entities, edges);
    }
    let base = REQUEST_MAPPING
        .captures(&file.content)
        .map(|capture| capture[1].to_string())
        .unwrap_or_default();
    for capture in METHOD_MAPPING.captures_iter(&file.content) {
        let matched = capture.get(0).unwrap();
        let method = capture[1].to_uppercase();
        let endpoint_path = normalize_path(&format!("{base}{}", &capture[2]));
        let name = format!("{method} {endpoint_path}");
        let line = line_of(&file.content, matched.start());
        let entity = Entity::new(
            EntityId::stable("workspace", path, EntityKind::HttpEndpoint, &name, ""),
            EntityKind::HttpEndpoint,
            &name,
            &name,
        )
        .with_metadata(json!({"method": method, "path": endpoint_path}))
        .with_evidence(
            path,
            line,
            line,
            EvidenceClass::Fact,
            1.0,
            "Spring mapping annotation",
        );
        add_contained(file, path, entity, line, entities, edges);
    }
    extract_custom_endpoints(file, path, entities, edges);
    Ok(())
}

fn extract_xml(file: &SourceFile, path: &str, entities: &mut Vec<Entity>, edges: &mut Vec<Edge>) {
    for capture in XML_STATEMENT.captures_iter(&file.content) {
        let operation = capture[1].to_lowercase();
        let statement_id = capture.get(2).unwrap();
        let sql = capture.get(3).unwrap();
        let line = line_of(&file.content, statement_id.start());
        let statement = Entity::new(
            EntityId::stable(
                "workspace",
                path,
                EntityKind::XmlStatement,
                statement_id.as_str(),
                "",
            ),
            EntityKind::XmlStatement,
            statement_id.as_str(),
            format!("{path}#{}", statement_id.as_str()),
        )
        .with_metadata(json!({"operation": operation}))
        .with_evidence(
            path,
            line,
            line,
            EvidenceClass::Fact,
            1.0,
            "MyBatis statement",
        );
        let statement_id_value = statement.id.clone();
        add_contained(file, path, statement, line, entities, edges);
        for alias in SQL_ALIAS.captures_iter(sql.as_str()) {
            let name = alias.get(2).unwrap();
            let field_line = line_of(&file.content, sql.start() + name.start());
            let field = Entity::new(
                EntityId::stable(
                    "workspace",
                    path,
                    EntityKind::SqlField,
                    name.as_str(),
                    &alias[1],
                ),
                EntityKind::SqlField,
                name.as_str(),
                format!("{path}#{}:{}", statement_id.as_str(), name.as_str()),
            )
            .with_metadata(json!({"source_column": &alias[1]}))
            .with_evidence(
                path,
                field_line,
                field_line,
                EvidenceClass::Fact,
                1.0,
                "SQL column alias",
            );
            edges.push(
                Edge::new(
                    statement_id_value.clone(),
                    field.id.clone(),
                    EdgeKind::ReadsColumn,
                )
                .with_evidence(
                    path,
                    field_line,
                    field_line,
                    EvidenceClass::Fact,
                    1.0,
                    "selected SQL field",
                ),
            );
            entities.push(field);
        }
        for (table_index, table_match) in SQL_FROM.captures_iter(sql.as_str()).enumerate() {
            let name = table_match.get(1).unwrap();
            let table = Entity::new(
                EntityId::stable("workspace", path, EntityKind::Table, name.as_str(), ""),
                EntityKind::Table,
                name.as_str(),
                name.as_str(),
            )
            .with_evidence(
                path,
                line,
                line,
                EvidenceClass::Fact,
                1.0,
                "SQL table reference",
            );
            let writes_target = operation != "select" && table_index == 0;
            let edge_kind = if writes_target {
                EdgeKind::WritesTable
            } else {
                EdgeKind::ReadsTable
            };
            edges.push(
                Edge::new(statement_id_value.clone(), table.id.clone(), edge_kind).with_evidence(
                    path,
                    line,
                    line,
                    EvidenceClass::Fact,
                    1.0,
                    if writes_target {
                        "SQL mutation target"
                    } else {
                        "SQL read table reference"
                    },
                ),
            );
            entities.push(table);
        }
    }
}

fn extract_frontend(
    file: &SourceFile,
    path: &str,
    entities: &mut Vec<Entity>,
    edges: &mut Vec<Edge>,
) {
    if file.kind == FileKind::Vue {
        let name = file
            .relative_path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or(path);
        let page = Entity::new(
            EntityId::stable("workspace", path, EntityKind::VuePage, name, ""),
            EntityKind::VuePage,
            name,
            path,
        )
        .with_evidence(
            path,
            1,
            1,
            EvidenceClass::Fact,
            1.0,
            "Vue single-file component",
        );
        add_contained(file, path, page, 1, entities, edges);
    }
    for capture in VUE_BINDING.captures_iter(&file.content) {
        let name = capture.get(1).unwrap();
        let line = line_of(&file.content, name.start());
        let field = Entity::new(
            EntityId::stable(
                "workspace",
                path,
                EntityKind::FrontendField,
                name.as_str(),
                "",
            ),
            EntityKind::FrontendField,
            name.as_str(),
            format!("{path}#{}", name.as_str()),
        )
        .with_evidence(
            path,
            line,
            line,
            EvidenceClass::Fact,
            1.0,
            "frontend property reference",
        );
        add_contained(file, path, field, line, entities, edges);
    }
    for capture in HTTP_CALL.captures_iter(&file.content) {
        let matched = capture.get(0).unwrap();
        let method = capture[1].to_uppercase();
        let url = normalize_path(&capture[2]);
        let name = format!("{method} {url}");
        let line = line_of(&file.content, matched.start());
        let call = Entity::new(
            EntityId::stable("workspace", path, EntityKind::HttpClientCall, &name, ""),
            EntityKind::HttpClientCall,
            &name,
            format!("{path}#{name}"),
        )
        .with_metadata(json!({"method": method, "path": url}))
        .with_evidence(
            path,
            line,
            line,
            EvidenceClass::Fact,
            1.0,
            "frontend HTTP call",
        );
        add_contained(file, path, call, line, entities, edges);
    }
}

fn extract_custom_endpoints(
    file: &SourceFile,
    path: &str,
    entities: &mut Vec<Entity>,
    edges: &mut Vec<Edge>,
) {
    for capture in CUSTOM_ENDPOINT.captures_iter(&file.content) {
        let token = capture.get(0).unwrap();
        let annotation = capture.get(1).unwrap();
        let args = capture.get(2).map(|m| m.as_str()).unwrap_or("");
        let value = STRING_LITERAL
            .captures(args)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str())
            .unwrap_or("");
        let line = line_of(&file.content, token.start());
        // Preserve the raw identifier (an RMB business code is not a URL path);
        // normalizing it would distort the value users actually search for.
        let endpoint_path = value.to_string();
        let (name, discriminator) = if endpoint_path.is_empty() {
            // No path argument: identify by file + line so each service entry
            // stays distinct even when the annotation carries no value.
            (annotation.as_str().to_string(), format!("{line}"))
        } else {
            (endpoint_path.clone(), String::new())
        };
        let entity = Entity::new(
            EntityId::stable(
                "workspace",
                path,
                EntityKind::HttpEndpoint,
                &name,
                &discriminator,
            ),
            EntityKind::HttpEndpoint,
            &name,
            &name,
        )
        .with_metadata(json!({"path": endpoint_path, "framework": "custom"}))
        .with_evidence(
            path,
            line,
            line,
            EvidenceClass::Fact,
            1.0,
            "custom RPC framework mapping",
        );
        add_contained(file, path, entity, line, entities, edges);
    }
}

// ---- Spring Bean 依赖注入(基于 tree-sitter AST) ----
// 正则无法把 "@Autowired 字段类型" 与所在类型可靠关联(多参数/泛型/跨行),
// 故走 AST。当前覆盖:字段注入(@Autowired/@Resource)→DependsOn、
// @Bean 工厂方法返回类型→Exposes。构造器参数注入留作后续
// (需判断单构造器或 @Autowired 构造器以免过捕)。

/// 一个类型上观测到的 Spring 运行时信号(B3:事务/定时),作为 metadata 挂到 class 实体。
/// 注:完整行为图(定时任务→方法调用链、事务边界跨方法传播)需先补 Method 级提取
/// (EntityKind::Method 已定义但未产出),当前只做"该类涉及事务/定时"的标记。
#[derive(Default)]
struct SpringSignals {
    transactional: bool,
    scheduled: bool,
}

#[allow(clippy::too_many_arguments)]
fn visit_spring(
    node: Node<'_>,
    source: &[u8],
    file: &SourceFile,
    path: &str,
    entities: &mut Vec<Entity>,
    edges: &mut Vec<Edge>,
    signals: &mut HashMap<String, SpringSignals>,
) {
    match node.kind() {
        "class_declaration" | "interface_declaration" | "record_declaration" | "enum_declaration" => {
            if has_annotation(source, node, &["Transactional"]) {
                if let Some(name_node) = node.child_by_field_name("name") {
                    signals
                        .entry(node_text(source, name_node))
                        .or_default()
                        .transactional = true;
                }
            }
        }
        "method_declaration" => {
            if has_annotation(source, node, &["Scheduled"]) {
                if let Some((owner, _)) = enclosing_type(source, node) {
                    signals.entry(owner).or_default().scheduled = true;
                }
            }
            if has_annotation(source, node, &["Bean"]) {
                if let Some(type_node) = node.child_by_field_name("type") {
                    let type_name = node_text(source, type_node);
                    if !type_name.is_empty() {
                        if let Some((owner_name, owner_kind)) = enclosing_type(source, node) {
                            link_bean(
                                file, path, &type_name, &owner_name, owner_kind,
                                EdgeKind::Exposes, node, entities, edges,
                            );
                        }
                    }
                }
            }
        }
        "field_declaration" => {
            if let Some(type_name) = injected_field_type(source, node) {
                if let Some((owner_name, owner_kind)) = enclosing_type(source, node) {
                    link_bean(
                        file, path, &type_name, &owner_name, owner_kind, EdgeKind::DependsOn,
                        node, entities, edges,
                    );
                }
            }
        }
        _ => {}
    }
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i) {
            visit_spring(child, source, file, path, entities, edges, signals);
        }
    }
}

fn node_text(source: &[u8], node: Node<'_>) -> String {
    std::str::from_utf8(&source[node.start_byte()..node.end_byte()])
        .unwrap_or("")
        .to_string()
}

/// 向上爬到最近的类型声明(class/interface/enum/record),返回其名字与对应
/// EntityKind(须与 extract_java 的实体生成对齐,否则边会悬空)。
fn enclosing_type(source: &[u8], mut node: Node<'_>) -> Option<(String, EntityKind)> {
    while let Some(parent) = node.parent() {
        let kind = match parent.kind() {
            "class_declaration" | "record_declaration" | "enum_declaration" => {
                Some(EntityKind::Class)
            }
            "interface_declaration" => Some(EntityKind::Interface),
            _ => None,
        };
        if let Some(kind) = kind {
            return parent
                .child_by_field_name("name")
                .map(|name| (node_text(source, name), kind));
        }
        node = parent;
    }
    None
}

/// 节点是否带指定注解。tree-sitter-java 把注解放在 `modifiers` 容器节点下
/// (declaration → modifiers → modifier → annotation),层级比直觉多。这里定位
/// `modifiers` 子节点并递归收集其中所有注解名,对层级变化鲁棒。
fn has_annotation(source: &[u8], node: Node<'_>, names: &[&str]) -> bool {
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i) {
            if child.kind() == "modifiers" {
                let found = collect_annotation_names(source, child);
                if found.iter().any(|name| names.contains(&name.as_str())) {
                    return true;
                }
            }
        }
    }
    false
}

/// 递归收集一个节点(通常是 `modifiers` 容器)子树内所有注解的简单名。
fn collect_annotation_names(source: &[u8], node: Node<'_>) -> Vec<String> {
    let mut names = Vec::new();
    collect_annotation_names_rec(source, node, &mut names);
    names
}

fn collect_annotation_names_rec(source: &[u8], node: Node<'_>, names: &mut Vec<String>) {
    if matches!(node.kind(), "marker_annotation" | "annotation") {
        if let Some(name_node) = node.child_by_field_name("name") {
            names.push(node_text(source, name_node));
        }
        return;
    }
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i) {
            collect_annotation_names_rec(source, child, names);
        }
    }
}

/// @Autowired / @Resource 字段的类型名(若有)。
fn injected_field_type(source: &[u8], node: Node<'_>) -> Option<String> {
    if !has_annotation(source, node, &["Autowired", "Resource"]) {
        return None;
    }
    node.child_by_field_name("type").map(|t| node_text(source, t))
}

#[allow(clippy::too_many_arguments)]
fn link_bean(
    file: &SourceFile,
    path: &str,
    type_name: &str,
    owner_name: &str,
    owner_kind: EntityKind,
    relation: EdgeKind,
    node: Node<'_>,
    entities: &mut Vec<Entity>,
    edges: &mut Vec<Edge>,
) {
    let line = line_of(&file.content, node.start_byte());
    let bean = Entity::new(
        EntityId::stable("workspace", path, EntityKind::SpringBean, type_name, ""),
        EntityKind::SpringBean,
        type_name,
        type_name,
    )
    .with_evidence(path, line, line, EvidenceClass::Fact, 1.0, "Spring bean (DI target)");
    let owner_id = EntityId::stable("workspace", path, owner_kind, owner_name, "");
    let reason = match relation {
        EdgeKind::DependsOn => "field injection (@Autowired/@Resource)",
        EdgeKind::Exposes => "@Bean factory method",
        _ => "Spring bean relation",
    };
    edges.push(
        Edge::new(owner_id, bean.id.clone(), relation)
            .with_evidence(path, line, line, EvidenceClass::Fact, 1.0, reason),
    );
    edges.push(
        Edge::new(file.id.clone(), bean.id.clone(), EdgeKind::Contains)
            .with_evidence(path, line, line, EvidenceClass::Fact, 1.0, "declared in file"),
    );
    entities.push(bean);
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
