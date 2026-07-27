use std::sync::LazyLock;

use anyhow::Result;
use regex::Regex;
use repo_intelligence_model::{
    Edge, EdgeKind, Entity, EntityId, EntityKind, EvidenceClass, GraphPatch,
};
use repo_intelligence_parsing::{Extractor, JavaParser};
use repo_intelligence_source::{FileKind, SourceFile};
use serde_json::json;
use std::collections::{HashMap, HashSet};
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

/// 前端属性访问中常见的非字段噪声(JS/TS 内建方法、工具函数、全大写常量)。
/// VUE_BINDING 把任何 `a.b` 当 FrontendField,命中此集合的 b 不产出以降噪声。
/// 这些名字与业务字段同名的概率极低,不影响贯通召回。
const FRONTEND_NOISE: &[&str] = &[
    "length",
    "size",
    "toString",
    "valueOf",
    "prototype",
    "constructor",
    "call",
    "apply",
    "bind",
    "push",
    "pop",
    "shift",
    "unshift",
    "split",
    "join",
    "slice",
    "splice",
    "concat",
    "reverse",
    "sort",
    "map",
    "filter",
    "forEach",
    "find",
    "findIndex",
    "some",
    "every",
    "reduce",
    "reduceRight",
    "includes",
    "indexOf",
    "lastIndexOf",
    "flat",
    "flatMap",
    "fill",
    "copyWithin",
    "floor",
    "ceil",
    "round",
    "random",
    "abs",
    "max",
    "min",
    "pow",
    "sqrt",
    "log",
    "exp",
    "sign",
    "keys",
    "values",
    "entries",
    "assign",
    "freeze",
    "from",
    "isArray",
    "create",
    "getPrototypeOf",
    "trim",
    "trimStart",
    "trimEnd",
    "replace",
    "replaceAll",
    "match",
    "matchAll",
    "search",
    "toLowerCase",
    "toUpperCase",
    "charAt",
    "charCodeAt",
    "padStart",
    "padEnd",
    "startsWith",
    "endsWith",
    "then",
    "catch",
    "finally",
    "resolve",
    "reject",
    "all",
    "race",
    "allSettled",
    "log",
    "error",
    "warn",
    "info",
    "debug",
    "time",
    "timeEnd",
    "createElement",
    "appendChild",
    "querySelector",
    "querySelectorAll",
    "getElementById",
    "addEventListener",
    "removeEventListener",
    "preventDefault",
    "stopPropagation",
];

static CUSTOM_ENDPOINT: LazyLock<Regex> = LazyLock::new(|| {
    let alternation = CUSTOM_ENDPOINT_ANNOTATIONS.join("|");
    Regex::new(&format!(r#"@({alternation})\b\s*(?:\(([^)]*)\))?"#)).unwrap()
});

static STRING_LITERAL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""([^"]*)""#).unwrap());

// ---- MyBatis Plus 持久层(MP 3.5.7 主力 ORM:注解实体 + BaseMapper + Wrapper) ----
// 注解-声明关联用 offset 配对(见 extract_mybatis_plus),不走 AST,避免 grammar 改动。
static MP_TABLE_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"@TableName\s*\(\s*(?:value\s*=\s*)?"([^"]+)""#).unwrap());
static MP_TABLE_FIELD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"@TableField\s*\(\s*(?:value\s*=\s*)?"([^"]+)""#).unwrap());
static MP_TABLE_ID_VAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"@TableId\s*\(\s*(?:value\s*=\s*)?"([^"]+)""#).unwrap());
// @TableField(exist = false):非表字段,推断时跳过以降误报。
static MP_NON_EXISTENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"@TableField\s*\([^)]*exist\s*=\s*false").unwrap());
static MP_MAPPER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\binterface\s+([A-Za-z_]\w*)\s*(?:extends|,)\s*BaseMapper\s*<\s*([A-Za-z_]\w*)\s*>",
    )
    .unwrap()
});
// QueryWrapper 链式方法的字符串首参(列名)。仅覆盖字符串形式;Lambda 方法引用
// (Entity::getName)需方法→字段映射,留后续。
static MP_WRAPPER_COLUMN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\.(eq|ne|gt|ge|lt|le|like|notLike|in|notIn|between|orderBy(?:Asc|Desc)?|groupBy|having|select)\s*\(\s*"([A-Za-z_]\w*)""#).unwrap()
});

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
        FileKind::Gradle => extract_gradle(file, &path, &mut entities, &mut edges),
        FileKind::Toml => extract_version_catalog(file, &path, &mut entities, &mut edges),
        FileKind::Json => extract_package_json(file, &path, &mut entities, &mut edges),
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
    extract_mybatis_plus(file, path, entities, edges);
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

/// 判断前端属性访问 `a.b` 的 b 是否「像业务字段」而非工具方法/常量。
fn is_likely_field(name: &str) -> bool {
    if FRONTEND_NOISE.contains(&name) {
        return false;
    }
    // 全大写(无小写字母,≥2 字符):视为常量/缩写(URL/MAX_VALUE),排除
    if name.len() >= 2 && !name.chars().any(|c| c.is_ascii_lowercase()) {
        return false;
    }
    true
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
        if !is_likely_field(name.as_str()) {
            continue;
        }
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

/// camelCase → snake_case(MyBatis Plus 默认 mapUnderscoreToCamelCase 的逆推断)。
/// 位置 0 的大写不插下划线。仅用于 @TableName 类内无显式 @TableField 字段的列名
/// 推断,配 EvidenceClass::Inferred(显式注解走 Fact 1.0)。连续大写(URLPath)会转成
/// u_r_l_path,与 MP 默认略异——这类字段实践中多有显式注解,影响有限。
fn camel_to_snake(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (i, ch) in name.chars().enumerate() {
        if ch.is_ascii_uppercase() && i != 0 {
            out.push('_');
        }
        out.push(ch.to_ascii_lowercase());
    }
    out
}

/// MyBatis Plus 持久层提取。MP 主力是注解实体 + BaseMapper + QueryWrapper 链式 API,
/// XML mapper 很少,故持久层贯通不能只靠 extract_xml。
///
/// 注解-声明关联用 offset 配对(注解 offset → 之后最近的 class/field offset),而非
/// tree-sitter AST——避免 grammar 改动,精度足够(MP 实体多为顶层类、字段注解紧贴声明)。
/// 产出:
///   @TableName → Table + Class--DependsOn→Table
///   @TableField/@TableId → Column + Field--MappedFrom→Column(Fact 1.0);@TableName 类
///     内无注解字段驼峰推断 → Column(Inferred 0.7);@TableField(exist=false) 跳过
///   BaseMapper<T> → Mapper;同文件内 T 是 @TableName 类 → Mapper--DependsOn→Table
///   QueryWrapper .eq("col",…) → Column + File--ReadsColumn→Column(Inferred 0.6)
/// 同 (文件,列名) 的实体列与 wrapper 列引用因 EntityId 确定性相同而合并为单节点。
#[allow(clippy::too_many_lines)]
fn extract_mybatis_plus(
    file: &SourceFile,
    path: &str,
    entities: &mut Vec<Entity>,
    edges: &mut Vec<Edge>,
) {
    let content = &file.content;

    // 收集 class / field 的 (名字 offset, 名字),复用 extract_java 同源 regex。
    let classes: Vec<(usize, String)> = JAVA_CLASS
        .captures_iter(content)
        .map(|c| {
            let name = c.get(2).unwrap();
            (name.start(), name.as_str().to_string())
        })
        .collect();
    let fields: Vec<(usize, String)> = JAVA_FIELD
        .captures_iter(content)
        .map(|c| {
            let name = c.get(1).unwrap();
            (name.start(), name.as_str().to_string())
        })
        .collect();

    // (1) @TableName → 类 → Table + (后续)类内字段 → Column。
    // table_by_class: class_name -> table_name(本文件内 @TableName 标注的实体类)
    let mut table_by_class: HashMap<String, String> = HashMap::new();
    for table_cap in MP_TABLE_NAME.captures_iter(content) {
        let table_name = table_cap.get(1).unwrap();
        let ann_end = table_cap.get(0).unwrap().end();
        // 注解之后最近的 class 声明 = 注解所属类
        let class_name = match classes
            .iter()
            .filter(|(start, _)| *start >= ann_end)
            .min_by_key(|(start, _)| *start)
        {
            Some((_, name)) => name.clone(),
            None => continue,
        };
        table_by_class.insert(class_name.clone(), table_name.as_str().to_string());
        let line = line_of(content, table_name.start());
        let table = Entity::new(
            EntityId::stable(
                "workspace",
                path,
                EntityKind::Table,
                table_name.as_str(),
                "",
            ),
            EntityKind::Table,
            table_name.as_str(),
            table_name.as_str(),
        )
        .with_evidence(
            path,
            line,
            line,
            EvidenceClass::Fact,
            1.0,
            "MyBatis Plus @TableName",
        );
        let class_id = EntityId::stable("workspace", path, EntityKind::Class, &class_name, "");
        edges.push(
            Edge::new(class_id, table.id.clone(), EdgeKind::DependsOn).with_evidence(
                path,
                line,
                line,
                EvidenceClass::Fact,
                1.0,
                "entity class maps to table via @TableName",
            ),
        );
        entities.push(table);
    }

    // 显式 @TableField/@TableId → 字段偏移 -> (列名, 注解行)
    let mut explicit_col: HashMap<usize, (String, u32)> = HashMap::new();
    for ann in MP_TABLE_FIELD
        .captures_iter(content)
        .chain(MP_TABLE_ID_VAL.captures_iter(content))
    {
        let col = ann.get(1).unwrap();
        let ann_start = ann.get(0).unwrap().start();
        if let Some((foff, _)) = fields
            .iter()
            .filter(|(foff, _)| *foff > ann_start)
            .min_by_key(|(foff, _)| *foff)
        {
            explicit_col
                .entry(*foff)
                .or_insert((col.as_str().to_string(), line_of(content, col.start())));
        }
    }

    // exist=false → 被标注的字段偏移集合(跳过)
    let non_existent: HashSet<usize> = MP_NON_EXISTENT
        .captures_iter(content)
        .filter_map(|ann| {
            let ann_start = ann.get(0).unwrap().start();
            fields
                .iter()
                .filter(|(foff, _)| *foff > ann_start)
                .min_by_key(|(foff, _)| *foff)
                .map(|(foff, _)| *foff)
        })
        .collect();

    for (foff, fname) in &fields {
        // 字段所属类 = offset ≤ 字段的最大 class
        let class_name = match classes.iter().rev().find(|(cstart, _)| *cstart <= *foff) {
            Some((_, name)) => name.as_str(),
            None => continue,
        };
        // 仅 @TableName 类的字段才映射列(其它类的字段不是 MP 实体字段)
        if !table_by_class.contains_key(class_name) {
            continue;
        }
        if non_existent.contains(foff) {
            continue;
        }
        let (col_name, classification, confidence, reason) = match explicit_col.get(foff) {
            Some((col, _)) => (
                col.clone(),
                EvidenceClass::Fact,
                1.0,
                "@TableField/@TableId maps field to column",
            ),
            None => (
                camel_to_snake(fname),
                EvidenceClass::Inferred,
                0.7,
                "inferred column name (camelCase→snake_case, no explicit @TableField)",
            ),
        };
        let line = explicit_col
            .get(foff)
            .map(|(_, line)| *line)
            .unwrap_or_else(|| line_of(content, *foff));
        let col = Entity::new(
            EntityId::stable("workspace", path, EntityKind::Column, &col_name, ""),
            EntityKind::Column,
            &col_name,
            format!("{path}#{col_name}"),
        )
        .with_metadata(json!({"mapped_field": fname, "source": "mybatis_plus"}))
        .with_evidence(path, line, line, classification, confidence, reason);
        let field_id = EntityId::stable("workspace", path, EntityKind::Field, fname, "");
        edges.push(
            Edge::new(field_id, col.id.clone(), EdgeKind::MappedFrom).with_evidence(
                path,
                line,
                line,
                classification,
                confidence,
                "field mapped to physical column",
            ),
        );
        entities.push(col);
    }

    // (2) Mapper 接口:interface XxxMapper extends BaseMapper<Entity>
    for mapper_cap in MP_MAPPER.captures_iter(content) {
        let mname = mapper_cap.get(1).unwrap();
        let entity_type = mapper_cap.get(2).unwrap().as_str().to_string();
        let line = line_of(content, mname.start());
        let mapper = Entity::new(
            EntityId::stable("workspace", path, EntityKind::Mapper, mname.as_str(), ""),
            EntityKind::Mapper,
            mname.as_str(),
            mname.as_str(),
        )
        .with_evidence(
            path,
            line,
            line,
            EvidenceClass::Fact,
            1.0,
            "MyBatis Plus mapper interface (BaseMapper<T>)",
        );
        let mapper_id = mapper.id.clone();
        add_contained(file, path, mapper, line, entities, edges);
        // 同文件内实体类型是 @TableName 类 → Mapper 绑定其表(跨文件解析留后续)
        if let Some(table_name) = table_by_class.get(&entity_type) {
            let table_id = EntityId::stable("workspace", path, EntityKind::Table, table_name, "");
            edges.push(
                Edge::new(mapper_id, table_id, EdgeKind::DependsOn).with_evidence(
                    path,
                    line,
                    line,
                    EvidenceClass::Fact,
                    1.0,
                    "BaseMapper<EntityType> binds mapper to entity table (same file)",
                ),
            );
        }
    }

    // (3) Wrapper 链式列引用:.eq("col", …) 等
    for wrapper_cap in MP_WRAPPER_COLUMN.captures_iter(content) {
        let col = wrapper_cap.get(2).unwrap();
        let col_name = col.as_str();
        let line = line_of(content, col.start());
        let column = Entity::new(
            EntityId::stable("workspace", path, EntityKind::Column, col_name, ""),
            EntityKind::Column,
            col_name,
            format!("{path}#{col_name}"),
        )
        .with_metadata(json!({"source": "query_wrapper"}))
        .with_evidence(
            path,
            line,
            line,
            EvidenceClass::Inferred,
            0.6,
            "QueryWrapper first-arg column literal",
        );
        edges.push(
            Edge::new(file.id.clone(), column.id.clone(), EdgeKind::ReadsColumn).with_evidence(
                path,
                line,
                line,
                EvidenceClass::Inferred,
                0.6,
                "QueryWrapper column reference",
            ),
        );
        entities.push(column);
    }
}

// ---- 模块依赖图(build.gradle.kts / libs.versions.toml / package.json → Package + DependsOn) ----
// 收益定位:模块级依赖影响(模块 A 依赖 SDK X),非方法级调用链(后者需 Method 提取)。
// 注意:实体是 path-scoped(EntityId 含 path),同名依赖跨文件不自动合并——
// v1 接受;跨文件共享依赖合并需 resolve 层按 ecosystem+name 处理,留后续。

/// package.json → 本模块 npm Package + 依赖 Package(--DependsOn)。
fn extract_package_json(
    file: &SourceFile,
    path: &str,
    entities: &mut Vec<Entity>,
    edges: &mut Vec<Edge>,
) {
    // 仅处理 package.json(其它 .json 如 tsconfig 不提取依赖)
    let is_pkg = file
        .relative_path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n == "package.json");
    if !is_pkg {
        return;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&file.content) else {
        return;
    };
    let module_name = value
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| {
            file.relative_path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("npm-module")
                .to_string()
        });
    let module = Entity::new(
        EntityId::stable("workspace", path, EntityKind::Package, &module_name, "npm"),
        EntityKind::Package,
        &module_name,
        &module_name,
    )
    .with_metadata(json!({"ecosystem": "npm"}))
    .with_evidence(
        path,
        1,
        1,
        EvidenceClass::Fact,
        1.0,
        "npm package (package.json)",
    );
    let module_id = module.id.clone();
    add_contained(file, path, module, 1, entities, edges);
    for section in ["dependencies", "devDependencies", "peerDependencies"] {
        let Some(obj) = value.get(section).and_then(|v| v.as_object()) else {
            continue;
        };
        for dep_name in obj.keys() {
            let dep = Entity::new(
                EntityId::stable(
                    "workspace",
                    path,
                    EntityKind::Package,
                    dep_name.as_str(),
                    "npm",
                ),
                EntityKind::Package,
                dep_name.as_str(),
                dep_name.as_str(),
            )
            .with_metadata(json!({"ecosystem": "npm"}))
            .with_evidence(path, 1, 1, EvidenceClass::Fact, 1.0, "npm dependency");
            edges.push(
                Edge::new(module_id.clone(), dep.id.clone(), EdgeKind::DependsOn).with_evidence(
                    path,
                    1,
                    1,
                    EvidenceClass::Fact,
                    1.0,
                    "npm dependency declared in package.json",
                ),
            );
            entities.push(dep);
        }
    }
}

/// build.gradle(.kts) → 模块 Gradle Package + 依赖 Package(--DependsOn)。
fn extract_gradle(
    file: &SourceFile,
    path: &str,
    entities: &mut Vec<Entity>,
    edges: &mut Vec<Edge>,
) {
    // 模块名 = build 脚本所在目录名
    let module_name = file
        .relative_path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("gradle-module")
        .to_string();
    let module = Entity::new(
        EntityId::stable(
            "workspace",
            path,
            EntityKind::Package,
            &module_name,
            "gradle",
        ),
        EntityKind::Package,
        &module_name,
        &module_name,
    )
    .with_metadata(json!({"ecosystem": "gradle"}))
    .with_evidence(
        path,
        1,
        1,
        EvidenceClass::Fact,
        1.0,
        "Gradle module (build script)",
    );
    let module_id = module.id.clone();
    add_contained(file, path, module, 1, entities, edges);

    // implementation/api/... ("group:artifact:version") 或 (libs.xxx 别名)。
    // 组1=引号坐标(group:artifact,版本剥离以利合并),组2=libs.xxx 别名。
    static GRADLE_DEP: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?:implementation|api|compileOnly|runtimeOnly|testImplementation|developmentOnly)\s*\(\s*(?:"([^":]+:[^":]+)(?::[^"]*)?"|(libs\.[A-Za-z0-9_.]+))"#,
        )
        .unwrap()
    });
    for cap in GRADLE_DEP.captures_iter(&file.content) {
        let dep_name = if let Some(m) = cap.get(1) {
            m.as_str().to_string()
        } else if let Some(m) = cap.get(2) {
            m.as_str().to_string()
        } else {
            continue;
        };
        let dep = Entity::new(
            EntityId::stable("workspace", path, EntityKind::Package, &dep_name, "gradle"),
            EntityKind::Package,
            &dep_name,
            &dep_name,
        )
        .with_metadata(json!({"ecosystem": "gradle", "coordinate": dep_name}))
        .with_evidence(path, 1, 1, EvidenceClass::Fact, 1.0, "Gradle dependency");
        edges.push(
            Edge::new(module_id.clone(), dep.id.clone(), EdgeKind::DependsOn).with_evidence(
                path,
                1,
                1,
                EvidenceClass::Fact,
                1.0,
                "Gradle dependency declared in build script",
            ),
        );
        entities.push(dep);
    }
}

/// libs.versions.toml → 别名 Package(libs.xxx,metadata.coordinate=group:artifact)。
/// v1 限制:Gradle 版本目录的连字符转点别名规则(my-lib→libs.my.lib)未实现,
/// 故 catalog 别名与 build 脚本里的 libs.xxx 引用暂不连通;本提取主要用于
/// 记录别名→坐标映射。完整 TOML 解析需 toml crate(未引入),regex 足够。
fn extract_version_catalog(
    file: &SourceFile,
    path: &str,
    entities: &mut Vec<Entity>,
    edges: &mut Vec<Edge>,
) {
    let _ = edges;
    let is_catalog = file
        .relative_path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n == "libs.versions.toml");
    if !is_catalog {
        return;
    }
    static TOML_LIB: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?ms)^\[libraries\.([^\]]+)\][\s\S]*?module\s*=\s*"([^"]+)""#).unwrap()
    });
    for cap in TOML_LIB.captures_iter(&file.content) {
        let alias = cap.get(1).unwrap().as_str();
        let coord = cap.get(2).unwrap().as_str();
        let alias_key = format!("libs.{alias}");
        let entity = Entity::new(
            EntityId::stable(
                "workspace",
                path,
                EntityKind::Package,
                &alias_key,
                "gradle-catalog",
            ),
            EntityKind::Package,
            &alias_key,
            &alias_key,
        )
        .with_metadata(json!({"ecosystem": "gradle", "coordinate": coord, "alias": alias}))
        .with_evidence(
            path,
            1,
            1,
            EvidenceClass::Fact,
            1.0,
            "Gradle version catalog alias",
        );
        entities.push(entity);
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
        "class_declaration"
        | "interface_declaration"
        | "record_declaration"
        | "enum_declaration" => {
            if has_annotation(source, node, &["Transactional"])
                && let Some(name_node) = node.child_by_field_name("name") {
                    signals
                        .entry(node_text(source, name_node))
                        .or_default()
                        .transactional = true;
                }
        }
        "method_declaration" => {
            if has_annotation(source, node, &["Scheduled"])
                && let Some((owner, _)) = enclosing_type(source, node) {
                    signals.entry(owner).or_default().scheduled = true;
                }
            if has_annotation(source, node, &["Bean"])
                && let Some(type_node) = node.child_by_field_name("type") {
                    let type_name = node_text(source, type_node);
                    if !type_name.is_empty()
                        && let Some((owner_name, owner_kind)) = enclosing_type(source, node) {
                            link_bean(
                                file,
                                path,
                                &type_name,
                                &owner_name,
                                owner_kind,
                                EdgeKind::Exposes,
                                node,
                                entities,
                                edges,
                            );
                        }
                }
        }
        "field_declaration" => {
            if let Some(type_name) = injected_field_type(source, node)
                && let Some((owner_name, owner_kind)) = enclosing_type(source, node) {
                    link_bean(
                        file,
                        path,
                        &type_name,
                        &owner_name,
                        owner_kind,
                        EdgeKind::DependsOn,
                        node,
                        entities,
                        edges,
                    );
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
        if let Some(child) = node.named_child(i)
            && child.kind() == "modifiers" {
                let found = collect_annotation_names(source, child);
                if found.iter().any(|name| names.contains(&name.as_str())) {
                    return true;
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
    node.child_by_field_name("type")
        .map(|t| node_text(source, t))
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
    .with_evidence(
        path,
        line,
        line,
        EvidenceClass::Fact,
        1.0,
        "Spring bean (DI target)",
    );
    let owner_id = EntityId::stable("workspace", path, owner_kind, owner_name, "");
    let reason = match relation {
        EdgeKind::DependsOn => "field injection (@Autowired/@Resource)",
        EdgeKind::Exposes => "@Bean factory method",
        _ => "Spring bean relation",
    };
    edges.push(
        Edge::new(owner_id, bean.id.clone(), relation).with_evidence(
            path,
            line,
            line,
            EvidenceClass::Fact,
            1.0,
            reason,
        ),
    );
    edges.push(
        Edge::new(file.id.clone(), bean.id.clone(), EdgeKind::Contains).with_evidence(
            path,
            line,
            line,
            EvidenceClass::Fact,
            1.0,
            "declared in file",
        ),
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
