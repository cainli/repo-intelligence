//! Java 提取:类型声明、字段、Spring MVC 端点、自研 RPC 注解端点、
//! Spring Bean 依赖注入(AST)、MyBatis Plus 持久层(注解实体 + BaseMapper + Wrapper)。

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use anyhow::Result;
use regex::Regex;
use repo_intelligence_config::SemanticsConfig;
use repo_intelligence_model::{Edge, EdgeKind, Entity, EntityId, EntityKind, EvidenceClass};
use repo_intelligence_parsing::{Extractor, JavaParser};
use repo_intelligence_source::{FileKind, SourceFile};
use serde_json::json;
use tree_sitter::Node;

use crate::registry::{ExtractContext, SemanticExtractor};
use crate::{add_contained, line_of, normalize_path};

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
static STRING_LITERAL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""([^"]*)""#).unwrap());
// class Name(可选泛型/extends)implements Iface1<Gen>, Iface2 { ... —— 组1=类名,
// 组2=接口列表(含泛型,到 class body 的 {)。跨行靠 [^{] 匹配换行(否定字符类含 \n)。
// 用 regex 而非 tree-sitter:implements 子句节点结构随 grammar 版本不稳,正则最可靠。
static JAVA_IMPLEMENTS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\bclass\s+([A-Za-z_]\w*)[^{]*?\bimplements\s+([^<{]+(?:<[^>]*>)?(?:\s*,\s*[^<{]+(?:<[^>]*>)?)*)",
    )
    .unwrap()
});

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
    // 兼容 MyBatis Plus BaseMapper<T> 与 ruoyi 等自研增强 BaseMapperPlus<T, V>
    // (取首个泛型为实体类型 T;第二个 V 是 VO,忽略)。不锚定闭合 >,以容忍双泛型。
    Regex::new(
        r"\binterface\s+([A-Za-z_]\w*)\s*(?:extends|,)\s*BaseMapper(?:Plus)?\s*<\s*([A-Za-z_]\w*)",
    )
    .unwrap()
});
// QueryWrapper 链式方法的字符串首参(列名)。仅覆盖字符串形式;Lambda 方法引用
// (Entity::getName)需方法→字段映射,留后续。
static MP_WRAPPER_COLUMN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\.(eq|ne|gt|ge|lt|le|like|notLike|in|notIn|between|orderBy(?:Asc|Desc)?|groupBy|having|select)\s*\(\s*"([A-Za-z_]\w*)""#).unwrap()
});
// QueryWrapper Lambda 方法引用:.eq(Entity::getXxx, …)。组2=Entity 类型,组3=getter。
// 是 MP_WRAPPER_COLUMN(字符串列名)的 Lambda 补充;getter→字段名→驼峰列名。
static MP_WRAPPER_LAMBDA: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"\.(eq|ne|gt|ge|lt|le|like|notLike|in|notIn|between|orderBy(?:Asc|Desc)?|groupBy|having|select)\s*\(\s*([A-Za-z_]\w*)::([A-Za-z_]\w*)"#,
    )
    .unwrap()
});

pub struct JavaExtractor;

impl SemanticExtractor for JavaExtractor {
    fn supports(&self, kind: FileKind) -> bool {
        kind == FileKind::Java
    }

    fn extract(
        &self,
        ctx: &ExtractContext,
        file: &SourceFile,
        path: &str,
        entities: &mut Vec<Entity>,
        edges: &mut Vec<Edge>,
    ) -> Result<()> {
        extract_java(file, path, entities, edges, ctx.config)
    }
}

fn extract_java(
    file: &SourceFile,
    path: &str,
    entities: &mut Vec<Entity>,
    edges: &mut Vec<Edge>,
    config: &SemanticsConfig,
) -> Result<()> {
    let parsed = JavaParser.parse(file)?;
    let syntax_confidence = if parsed.has_syntax_errors { 0.8 } else { 1.0 };
    // 先跑 AST 遍历:产出 Bean DI 边 + 收集 Spring 信号(事务/定时),后者作为
    // metadata 挂到对应 class 实体,故必须在 class 实体创建前完成。
    let mut signals: HashMap<String, SpringSignals> = HashMap::new();
    let mut methods: HashMap<String, EntityId> = HashMap::new();
    let mut invocations: Vec<(String, String, u32)> = Vec::new();
    // 每个 method 的 (name 节点 offset, id),供 endpoint 注解 offset 配对到所在 method。
    let mut method_spans: Vec<(usize, EntityId)> = Vec::new();
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
        visit_methods(
            tree.root_node(),
            file.content.as_bytes(),
            file,
            path,
            entities,
            edges,
            &mut methods,
            &mut invocations,
            &mut method_spans,
            None,
        );
    }
    // 同文件 method 调用 → Calls 边(低保真:按方法名匹配,跨类同名混淆、跨文件留后续)。
    for (caller, callee, line) in &invocations {
        if let (Some(caller_id), Some(callee_id)) = (methods.get(caller), methods.get(callee))
            && caller_id != callee_id
        {
            edges.push(
                Edge::new(caller_id.clone(), callee_id.clone(), EdgeKind::Calls).with_evidence(
                    path,
                    *line,
                    *line,
                    EvidenceClass::Inferred,
                    0.7,
                    "same-file method call",
                ),
            );
        }
    }
    // 把调用意图存入 method 实体 metadata.invokes,供 resolve_cross_stack 跨文件解析
    // (Controller→Service 这类跨文件调用 = 注入依赖类型 + 方法名匹配)。按 caller short
    // name 分组回填;文件内方法名通常唯一,重名时后者覆盖前者。
    let mut invokes_by_caller: HashMap<&str, Vec<serde_json::Value>> = HashMap::new();
    for (caller, callee, line) in &invocations {
        invokes_by_caller
            .entry(caller.as_str())
            .or_default()
            .push(json!({"name": callee, "line": line}));
    }
    for entity in entities.iter_mut() {
        if entity.kind != EntityKind::Method {
            continue;
        }
        let Some(calls) = invokes_by_caller.get(entity.name.as_str()) else {
            continue;
        };
        let mut meta = match entity.metadata.clone() {
            serde_json::Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        meta.insert("invokes".into(), serde_json::Value::Array(calls.clone()));
        entity.metadata = serde_json::Value::Object(meta);
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
    // method_spans 按 offset 排序,供 endpoint 注解 offset 配对到所在 method。
    method_spans.sort_by_key(|(offset, _)| *offset);
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
        .with_evidence(path, line, line, EvidenceClass::Fact, 1.0, "Spring mapping annotation");
        let endpoint_id = entity.id.clone();
        add_contained(file, path, entity, line, entities, edges);
        // method→endpoint(Exposes):mapping 注解贴在方法声明前,配对"注解 offset 之后
        // 最近的 method",让 find_endpoint/relay 能从 URL 追到处理方法。
        let ann_offset = matched.start();
        if let Some((_, method_id)) = method_spans
            .iter()
            .filter(|(offset, _)| *offset > ann_offset)
            .min_by_key(|(offset, _)| *offset)
        {
            edges.push(
                Edge::new(method_id.clone(), endpoint_id, EdgeKind::Exposes).with_evidence(
                    path,
                    line,
                    line,
                    EvidenceClass::Fact,
                    1.0,
                    "controller method exposes HTTP endpoint",
                ),
            );
        }
    }
    extract_custom_endpoints(file, path, entities, edges, config);
    extract_mybatis_plus(file, path, entities, edges);
    extract_implements(file, entities);
    extract_interface_endpoints(file, path, entities, edges, config);
    Ok(())
}

fn extract_custom_endpoints(
    file: &SourceFile,
    path: &str,
    entities: &mut Vec<Entity>,
    edges: &mut Vec<Edge>,
    config: &SemanticsConfig,
) {
    let annotations: &[String] = &config.custom_endpoint_annotations;
    if annotations.is_empty() {
        return;
    }
    // 按配置的自研注解现场构建正则(替代原写死 static)。escape 以容忍
    // 配置值含正则元字符。
    let alternation = annotations
        .iter()
        .map(|annotation| regex::escape(annotation))
        .collect::<Vec<_>>()
        .join("|");
    let custom_re = Regex::new(&format!(r#"@({alternation})\b\s*(?:\(([^)]*)\))?"#)).unwrap();
    for capture in custom_re.captures_iter(&file.content) {
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
            EntityId::stable("workspace", path, EntityKind::HttpEndpoint, &name, &discriminator),
            EntityKind::HttpEndpoint,
            &name,
            &name,
        )
        .with_metadata(json!({"path": endpoint_path, "framework": "custom"}))
        .with_evidence(path, line, line, EvidenceClass::Fact, 1.0, "custom RPC framework mapping");
        add_contained(file, path, entity, line, entities, edges);
    }
}

/// camelCase → snake_case(MyBatis Plus 默认 mapUnderscoreToCamelCase 的逆推断)。
/// 仅用于 @TableName 类内无显式 @TableField 字段的列名推断,配 EvidenceClass::Inferred
/// (显式注解走 Fact 1.0)。下划线插在两类边界:
///   - 小写→大写(camelCase):userId → user_id
///   - 大写缩写词结束(后接小写):URLPath → url_path、XMLParser → xml_parser
///     (缩写词内部连续大写不逐字拆分,与 MP 默认一致)
fn camel_to_snake(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::with_capacity(name.len() + 4);
    for (i, &ch) in chars.iter().enumerate() {
        if ch.is_ascii_uppercase() && i > 0 {
            let prev = chars[i - 1];
            // 小写→大写:camelCase 边界(userId 的 I)
            // 大写→大写 且 下一个是小写:缩写词到此结束,当前是新词首(URLPath 的 P)
            if prev.is_ascii_lowercase()
                || (prev.is_ascii_uppercase()
                    && i + 1 < chars.len()
                    && chars[i + 1].is_ascii_lowercase())
            {
                out.push('_');
            }
        }
        out.push(ch.to_ascii_lowercase());
    }
    out
}

/// getter 方法名 → 字段名:getCustomerName→customerName,isActive→active。
/// 去掉 get/is 前缀后首字母小写;无前缀则原样首字母小写。
fn method_to_field(method: &str) -> String {
    let after = method
        .strip_prefix("get")
        .or_else(|| method.strip_prefix("is"))
        .unwrap_or(method);
    let mut chars = after.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_ascii_lowercase(), chars.as_str()),
        None => String::new(),
    }
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
            EntityId::stable("workspace", path, EntityKind::Table, table_name.as_str(), ""),
            EntityKind::Table,
            table_name.as_str(),
            table_name.as_str(),
        )
        .with_evidence(path, line, line, EvidenceClass::Fact, 1.0, "MyBatis Plus @TableName");
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
        .with_metadata(json!({"entity_type": entity_type}))
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

    // (4) Lambda 方法引用:wrapper.eq(Entity::getXxx, …) → 列(Inferred)
    //     getXxx→字段名→驼峰列名。跨文件 Entity 的显式 @TableField 未查(精度换覆盖);
    //     同 (文件,列名) 的 Column 因 EntityId 确定性合并。
    for lambda_cap in MP_WRAPPER_LAMBDA.captures_iter(content) {
        let method = lambda_cap.get(3).unwrap();
        let field_name = method_to_field(method.as_str());
        if field_name.is_empty() {
            continue;
        }
        let col_name = camel_to_snake(&field_name);
        let line = line_of(content, method.start());
        let column = Entity::new(
            EntityId::stable("workspace", path, EntityKind::Column, &col_name, ""),
            EntityKind::Column,
            &col_name,
            format!("{path}#{col_name}"),
        )
        .with_metadata(json!({"source": "lambda_wrapper"}))
        .with_evidence(
            path,
            line,
            line,
            EvidenceClass::Inferred,
            0.6,
            "QueryWrapper Lambda method reference",
        );
        edges.push(
            Edge::new(file.id.clone(), column.id.clone(), EdgeKind::ReadsColumn).with_evidence(
                path,
                line,
                line,
                EvidenceClass::Inferred,
                0.6,
                "QueryWrapper Lambda column reference",
            ),
        );
        entities.push(column);
    }
}

/// implements 关系提取:把 class 实现的接口名列表存入 class 实体 metadata.implements。
/// EntityId 是 path-scoped(每文件独立命名空间),class 在本文件、interface 实体在定义
/// 文件,extract 层建不了跨文件边(id 不匹配)。故只传递接口名,由 resolve_cross_stack
/// 按全局 interface 实体名解析建边(与跨文件 Mapper→Table 同模式)。
fn extract_implements(file: &SourceFile, entities: &mut [Entity]) {
    let content = &file.content;
    let caps: Vec<(String, Vec<serde_json::Value>)> = JAVA_IMPLEMENTS
        .captures_iter(content)
        .filter_map(|cap| {
            let class_name = cap[1].to_string();
            let list = cap.get(2).map(|m| m.as_str()).unwrap_or("");
            let ifaces: Vec<serde_json::Value> = list
                .split(',')
                .filter_map(|raw| {
                    let iface = raw.split('<').next().unwrap_or("").trim();
                    // 合法 Java 标识符(首字母、后续字母数字下划线),过滤泛型残余如 "V>"
                    let valid = !iface.is_empty()
                        && iface.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
                        && iface.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
                    if valid {
                        Some(serde_json::Value::String(iface.to_string()))
                    } else {
                        None
                    }
                })
                .collect();
            if ifaces.is_empty() {
                None
            } else {
                Some((class_name, ifaces))
            }
        })
        .collect();
    for (class_name, ifaces) in caps {
        for entity in entities.iter_mut() {
            if entity.kind == EntityKind::Class && entity.name == class_name {
                let mut meta = match entity.metadata.clone() {
                    serde_json::Value::Object(map) => map,
                    _ => serde_json::Map::new(),
                };
                meta.insert("implements".into(), serde_json::Value::Array(ifaces.clone()));
                entity.metadata = serde_json::Value::Object(meta);
            }
        }
    }
}

/// implements 约定接口(ApiHandler/IBizProcess 等)的类视为自研 RPC 入口,补一个 HttpEndpoint
/// 实体(name=类名,即交易码/业务码),让 find_endpoint 能命中——mes/mos 的 RMB 入口普遍用
/// `@MosApi + implements ApiHandler` 这套自定义框架,纯注解识别覆盖不到。metadata.implements
/// 已由 extract_implements 填(接口名去泛型),故须在其后调用。
fn extract_interface_endpoints(
    file: &SourceFile,
    path: &str,
    entities: &mut Vec<Entity>,
    edges: &mut Vec<Edge>,
    config: &SemanticsConfig,
) {
    let interfaces: &[String] = &config.custom_endpoint_interfaces;
    if interfaces.is_empty() {
        return;
    }
    // 先收集命中类,避免遍历自身(建实体时会 push 进 entities)。
    let hits: Vec<(String, u32, String)> = entities
        .iter()
        .filter(|entity| entity.kind == EntityKind::Class)
        .filter_map(|entity| {
            let impls = entity
                .metadata
                .get("implements")
                .and_then(|value| value.as_array())?;
            let iface = impls.iter().find_map(|item| {
                let name = item.as_str()?;
                interfaces
                    .iter()
                    .find(|wanted| *wanted == name)
                    .map(|_| name.to_string())
            })?;
            let line = entity
                .evidence
                .first()
                .map(|evidence| evidence.start_line)
                .unwrap_or(0);
            Some((entity.name.clone(), line, iface))
        })
        .collect();
    for (class_name, line, iface) in hits {
        let entity = Entity::new(
            EntityId::stable(
                "workspace",
                path,
                EntityKind::HttpEndpoint,
                &class_name,
                &format!("iface:{iface}"),
            ),
            EntityKind::HttpEndpoint,
            &class_name,
            &class_name,
        )
        .with_metadata(json!({"path": class_name, "framework": format!("implements {iface}")}))
        .with_evidence(
            path,
            line,
            line,
            EvidenceClass::Inferred,
            0.8,
            "custom RPC entry (implements framework interface)",
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
    /// 该类型直接子构造器数量(决定单构造器是否自动注入)。interface 恒为 0。
    constructor_count: usize,
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
        "class_declaration" | "interface_declaration" | "record_declaration"
        | "enum_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(source, name_node);
                let sig = signals.entry(name.clone()).or_default();
                if has_annotation(source, node, &["Transactional"]) {
                    sig.transactional = true;
                }
                // 数构造器(class_body/enum_body 等容器下的成员,非 class 直接子);
                // interface 无构造器→0。决定单构造器是否自动注入。
                let mut ctors = 0;
                for i in 0..node.named_child_count() {
                    if let Some(body) = node.named_child(i) {
                        for j in 0..body.named_child_count() {
                            if let Some(member) = body.named_child(j)
                                && member.kind() == "constructor_declaration"
                            {
                                ctors += 1;
                            }
                        }
                    }
                }
                sig.constructor_count = ctors;
                // Lombok 构造器注入:@RequiredArgsConstructor(final 字段)/
                // @AllArgsConstructor(全部字段)生成构造器,但 AST 里无显式
                // constructor_declaration 节点,故按注解把字段类型当作注入参数,
                // 补 DependsOn(owner→bean)。这是 Controller→Service 跨文件链的前提。
                let owner_kind = match node.kind() {
                    "interface_declaration" => EntityKind::Interface,
                    _ => EntityKind::Class,
                };
                let all_args = has_annotation(source, node, &["AllArgsConstructor"]);
                if all_args || has_annotation(source, node, &["RequiredArgsConstructor"]) {
                    for bi in 0..node.named_child_count() {
                        let Some(body) = node.named_child(bi) else { continue };
                        for ci in 0..body.named_child_count() {
                            let Some(member) = body.named_child(ci) else { continue };
                            if member.kind() != "field_declaration" {
                                continue;
                            }
                            if !all_args && !field_is_final(source, member) {
                                continue;
                            }
                            let Some(type_node) = member.child_by_field_name("type") else {
                                continue;
                            };
                            let type_name = node_text(source, type_node);
                            if !type_name.is_empty() {
                                link_bean(
                                    file,
                                    path,
                                    &type_name,
                                    &name,
                                    owner_kind,
                                    EdgeKind::Injects,
                                    member,
                                    entities,
                                    edges,
                                );
                            }
                        }
                    }
                }
            }
        }
        "constructor_declaration" => {
            // 构造器注入:@Autowired 标注,或所在 class 恰好 1 个构造器(Spring 默认)。
            if let Some((owner_name, owner_kind)) = enclosing_type(source, node) {
                let single = signals
                    .get(&owner_name)
                    .is_some_and(|sig| sig.constructor_count == 1);
                if has_annotation(source, node, &["Autowired"]) || single {
                    for param_type in constructor_param_types(source, node) {
                        if !param_type.is_empty() {
                            link_bean(
                                file,
                                path,
                                &param_type,
                                &owner_name,
                                owner_kind,
                                EdgeKind::Injects,
                                node,
                                entities,
                                edges,
                            );
                        }
                    }
                }
            }
        }
        "method_declaration" => {
            if has_annotation(source, node, &["Scheduled"])
                && let Some((owner, _)) = enclosing_type(source, node)
            {
                signals.entry(owner).or_default().scheduled = true;
            }
            if has_annotation(source, node, &["Bean"])
                && let Some(type_node) = node.child_by_field_name("type")
            {
                let type_name = node_text(source, type_node);
                if !type_name.is_empty()
                    && let Some((owner_name, owner_kind)) = enclosing_type(source, node)
                {
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
                && let Some((owner_name, owner_kind)) = enclosing_type(source, node)
            {
                link_bean(
                    file,
                    path,
                    &type_name,
                    &owner_name,
                    owner_kind,
                    EdgeKind::Injects,
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

/// 方法级提取:method_declaration → Method 实体;method_invocation → 调用信号。
/// caller/callee 按方法名同文件匹配(低保真:跨类同名混淆、跨文件调用留后续)。
#[allow(clippy::too_many_arguments)]
fn visit_methods(
    node: Node<'_>,
    source: &[u8],
    file: &SourceFile,
    path: &str,
    entities: &mut Vec<Entity>,
    edges: &mut Vec<Edge>,
    methods: &mut HashMap<String, EntityId>,
    invocations: &mut Vec<(String, String, u32)>,
    method_spans: &mut Vec<(usize, EntityId)>,
    current_method: Option<&str>,
) {
    if node.kind() == "method_declaration"
        && let Some(name_node) = node.child_by_field_name("name")
    {
        let name = node_text(source, name_node);
        let line = line_of(&file.content, name_node.start_byte());
        let entity = Entity::new(
            EntityId::stable("workspace", path, EntityKind::Method, &name, ""),
            EntityKind::Method,
            &name,
            format!("{path}#{name}"),
        )
        .with_evidence(path, line, line, EvidenceClass::Fact, 1.0, "Java method declaration");
        let id = entity.id.clone();
        add_contained(file, path, entity, line, entities, edges);
        methods.insert(name.clone(), id.clone());
        // Class→Method 层级边(Declares):让 relay/impact 从类型到达其方法。
        // owner id 与 extract_java 的 class/interface entity 对齐(stable path+kind+name)。
        if let Some((owner_name, owner_kind)) = enclosing_type(source, node) {
            let owner_id = EntityId::stable("workspace", path, owner_kind, &owner_name, "");
            edges.push(
                Edge::new(owner_id, id.clone(), EdgeKind::Declares).with_evidence(
                    path,
                    line,
                    line,
                    EvidenceClass::Fact,
                    1.0,
                    "method declared by type",
                ),
            );
        }
        method_spans.push((name_node.start_byte(), id));
        for i in 0..node.named_child_count() {
            if let Some(child) = node.named_child(i) {
                visit_methods(
                    child,
                    source,
                    file,
                    path,
                    entities,
                    edges,
                    methods,
                    invocations,
                    method_spans,
                    Some(&name),
                );
            }
        }
        return;
    }
    if node.kind() == "method_invocation"
        && let (Some(caller), Some(name_node)) =
            (current_method, node.child_by_field_name("name"))
    {
        let callee = node_text(source, name_node);
        if !callee.is_empty() {
            invocations.push((
                caller.to_string(),
                callee,
                line_of(&file.content, node.start_byte()),
            ));
        }
    }
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i) {
            visit_methods(
                child,
                source,
                file,
                path,
                entities,
                edges,
                methods,
                invocations,
                method_spans,
                current_method,
            );
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
            && child.kind() == "modifiers"
        {
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

/// 字段是否带 `final` 修饰符(Lombok @RequiredArgsConstructor 只注入 final 字段)。
/// 取 field_declaration 的 modifiers 子节点文本,按空白切分匹配 `final` 关键字;
/// 注解参数里即便出现 final 字样也不会作为独立 token,故不会误判。
fn field_is_final(source: &[u8], field_node: Node<'_>) -> bool {
    for i in 0..field_node.named_child_count() {
        if let Some(child) = field_node.named_child(i)
            && child.kind() == "modifiers"
        {
            return node_text(source, child)
                .split_whitespace()
                .any(|token| token == "final");
        }
    }
    false
}

/// 构造器的参数类型列表(formal_parameters → formal_parameter.type)。
fn constructor_param_types(source: &[u8], node: Node<'_>) -> Vec<String> {
    let mut types = Vec::new();
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i)
            && child.kind() == "formal_parameters"
        {
            for j in 0..child.named_child_count() {
                if let Some(param) = child.named_child(j)
                    && param.kind() == "formal_parameter"
                    && let Some(type_node) = param.child_by_field_name("type")
                {
                    types.push(node_text(source, type_node));
                }
            }
        }
    }
    types
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
        EdgeKind::Injects => "constructor/field injection (@Autowired/@Resource/Lombok)",
        EdgeKind::Exposes => "@Bean factory method",
        _ => "Spring bean relation",
    };
    edges.push(
        Edge::new(owner_id, bean.id.clone(), relation)
            .with_evidence(path, line, line, EvidenceClass::Fact, 1.0, reason),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camel_to_snake_handles_camel_case_and_acronyms() {
        // 普通 camelCase
        assert_eq!(camel_to_snake("userId"), "user_id");
        assert_eq!(camel_to_snake("customerName"), "customer_name");
        // 连续大写(缩写词)——历史 bug:URLPath 曾被错转成 u_r_l_path
        assert_eq!(camel_to_snake("URLPath"), "url_path");
        assert_eq!(camel_to_snake("XMLParser"), "xml_parser");
        assert_eq!(camel_to_snake("HTTPSConnection"), "https_connection");
        // 全大写缩写词(无后续小写):整体小写,无下划线
        assert_eq!(camel_to_snake("URL"), "url");
        assert_eq!(camel_to_snake("id"), "id");
        // 单字符大写首
        assert_eq!(camel_to_snake("Id"), "id");
        // 小写后接全大写结尾
        assert_eq!(camel_to_snake("userURL"), "user_url");
    }
}
