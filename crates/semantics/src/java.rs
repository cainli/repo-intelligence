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
    // 类级 base 路径。只捕获注解括号内的参数列表,path 提取交给 annotation_path
    // (兼容 value 不在首位、裸字符串、数组 {"/a","/b"} 三类写法)。
    Regex::new(r#"@RequestMapping\s*\(([^)]*)\)"#).unwrap()
});
static METHOD_MAPPING: LazyLock<Regex> = LazyLock::new(|| {
    // 方法级 HTTP 映射注解,两类写法:
    //  (a) @(Get|Post|Put|Delete|Patch)Mapping("/x"…) → group1 = 动词;
    //  (b) @RequestMapping("/x"…)                     → group1 缺失(method 通配)。
    // group2 = 括号内参数列表,path 由 annotation_path 解析(支持 value 在任意属性位)。
    // 类级 @RequestMapping 虽也被该正则命中,但由配对阶段的 class_offset 检查排除
    // (见 extract_java),仅作 base。
    Regex::new(r#"@(?:(Get|Post|Put|Delete|Patch)Mapping|RequestMapping)\s*\(([^)]*)\)"#)
        .unwrap()
});
static STRING_LITERAL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""([^"]*)""#).unwrap());
// 注解参数里的 path 提取:value/path 属性优先(任意属性位置,兼容数组 value={"/a","/b"}
// 取首个元素),否则首个裸字符串字面量。
static ANN_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:value|path)\s*=\s*\{?\s*"([^"]*)""#).unwrap()
});

/// 从注解参数列表提取 path 字符串。修复 `@RequestMapping(method = POST, value = "/x")`
/// 这类 value 不在首位的写法被静默丢弃的历史问题(旧正则要求 value/裸串紧跟左括号)。
fn annotation_path(args: &str) -> Option<String> {
    if let Some(capture) = ANN_VALUE.captures(args) {
        return Some(capture[1].to_string());
    }
    STRING_LITERAL
        .captures(args)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}
// 通用注解简单名:@Foo(…) / @Foo → group1=Foo。用于白名单注解索引(P1-1)。
static AT_ANNOTATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"@([A-Za-z_]\w*)").unwrap());
// @Test 方法定位(P1-4)。
static AT_TEST: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"@Test\b").unwrap());
// AOP advice 注解 + 其 pointcut 字面量(P1-2)。group2 = 参数列表,pointcut 经
// annotation_path 提取(兼容 value 不在首位)。
static ADVICE_ANN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"@(Around|Before|After|AfterReturning|AfterThrowing)\s*\(([^)]*)\)"#).unwrap()
});
// execution(返回类型 包.类.方法(..)) → 全限定方法签名(组1)。简单版,不处理通配 */||/within。
static EXECUTION_SIG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"execution\s*\(\s*\S+\s+([\w.$]+)\s*\(").unwrap()
});
// class Name(可选泛型/extends)implements Iface1<Gen>, Iface2 { ... —— 组1=类名,
// 组2=接口列表(含泛型,到 class body 的 {)。跨行靠 [^{] 匹配换行(否定字符类含 \n)。
// 用 regex 而非 tree-sitter:implements 子句节点结构随 grammar 版本不稳,正则最可靠。
static JAVA_IMPLEMENTS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\bclass\s+([A-Za-z_]\w*)[^{]*?\bimplements\s+([^<{]+(?:<[^>]*>)?(?:\s*,\s*[^<{]+(?:<[^>]*>)?)*)",
    )
    .unwrap()
});
// class Sub extends Super —— 组1=子类,组2=超类简单名(去泛型)。只匹配 class(非 interface),
// 故不与 BaseMapper 的 interface extends 冲突。跨文件继承边由 resolve_cross_stack 按 superclass 名解析。
static JAVA_EXTENDS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bclass\s+([A-Za-z_]\w*)[^{]*?\bextends\s+([A-Za-z_]\w*)").unwrap()
});
// abstract class Foo —— abstract 修饰符(不论有无 extends)。存 metadata.abstract 供 trace 标注。
static JAVA_ABSTRACT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\babstract\s+class\s+([A-Za-z_]\w*)").unwrap()
});

// ---- MyBatis Plus 持久层(MP 3.5.7 主力 ORM:注解实体 + BaseMapper + Wrapper) ----
// 注解-声明关联用 offset 配对(见 extract_mybatis_plus),不走 AST,避免 grammar 改动。
// 括号内参数列表捕获,path 交 annotation_path(兼容 @TableName(schema="x", value="t")
// 这类 value 不在首位的写法)。
static MP_TABLE_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"@TableName\s*\(([^)]*)\)"#).unwrap());
static MP_TABLE_FIELD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"@TableField\s*\(([^)]*)\)"#).unwrap());
static MP_TABLE_ID_VAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"@TableId\s*\(([^)]*)\)"#).unwrap());
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

/// 保偏移的双档掩码源码(见 `mask_java`):两档都保留字节偏移与换行,
/// 掩码后的内容可直接跑原正则,offset 配对/line_of 全部不受影响。
struct MaskedSource {
    /// 仅掩码注释,保留字符串字面量 —— 给需要读字符串实参的正则
    /// (mapping 路径/@TableName 列名/wrapper 列名/pointcut)。
    code: String,
    /// 注释 + 字符串/字符字面量全掩码 —— 给结构性正则
    /// (class/field/annotation/extends/implements),根除 Javadoc 里的
    /// "@Transactional"、注释里的 "class Foo" 产幻影实体。
    bare: String,
}

/// 单遍扫描产出两档掩码(状态机:行注释/块注释/字符串/字符/text block)。
/// 被掩码字节替换为空格,`\n` 原样保留(保证 line_of 与 offset 配对正确)。
/// tree-sitter 路径仍用原文;掩码只服务正则提取。
fn mask_java(content: &str) -> MaskedSource {
    let bytes = content.as_bytes();
    let n = bytes.len();
    let mut code = bytes.to_vec();
    let mut bare = bytes.to_vec();
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        Code,
        LineComment,
        BlockComment,
        Str,
        Char,
        TextBlock,
    }
    let mut state = State::Code;
    let mut i = 0;
    while i < n {
        let b = bytes[i];
        match state {
            State::Code => {
                if b == b'/' && i + 1 < n && bytes[i + 1] == b'/' {
                    state = State::LineComment;
                    code[i] = b' ';
                    bare[i] = b' ';
                    code[i + 1] = b' ';
                    bare[i + 1] = b' ';
                    i += 2;
                } else if b == b'/' && i + 1 < n && bytes[i + 1] == b'*' {
                    state = State::BlockComment;
                    code[i] = b' ';
                    bare[i] = b' ';
                    code[i + 1] = b' ';
                    bare[i + 1] = b' ';
                    i += 2;
                } else if b == b'"' && i + 2 < n && bytes[i + 1] == b'"' && bytes[i + 2] == b'"' {
                    state = State::TextBlock;
                    bare[i] = b' ';
                    bare[i + 1] = b' ';
                    bare[i + 2] = b' ';
                    i += 3;
                } else if b == b'"' {
                    state = State::Str;
                    bare[i] = b' ';
                    i += 1;
                } else if b == b'\'' {
                    state = State::Char;
                    bare[i] = b' ';
                    i += 1;
                } else {
                    i += 1;
                }
            }
            State::LineComment => {
                if b == b'\n' {
                    state = State::Code;
                    i += 1;
                } else {
                    code[i] = b' ';
                    bare[i] = b' ';
                    i += 1;
                }
            }
            State::BlockComment => {
                if b == b'*' && i + 1 < n && bytes[i + 1] == b'/' {
                    code[i] = b' ';
                    bare[i] = b' ';
                    code[i + 1] = b' ';
                    bare[i + 1] = b' ';
                    i += 2;
                    state = State::Code;
                } else if b == b'\n' {
                    i += 1;
                } else {
                    code[i] = b' ';
                    bare[i] = b' ';
                    i += 1;
                }
            }
            State::Str => {
                if b == b'\\' && i + 1 < n {
                    bare[i] = b' ';
                    bare[i + 1] = b' ';
                    i += 2;
                } else if b == b'"' || b == b'\n' {
                    // `\n`:容忍未闭合字符串(畸形输入),回到代码态。
                    if b == b'"' {
                        bare[i] = b' ';
                    }
                    state = State::Code;
                    i += 1;
                } else {
                    bare[i] = b' ';
                    i += 1;
                }
            }
            State::Char => {
                if b == b'\\' && i + 1 < n {
                    bare[i] = b' ';
                    bare[i + 1] = b' ';
                    i += 2;
                } else if b == b'\'' || b == b'\n' {
                    if b == b'\'' {
                        bare[i] = b' ';
                    }
                    state = State::Code;
                    i += 1;
                } else {
                    bare[i] = b' ';
                    i += 1;
                }
            }
            State::TextBlock => {
                if b == b'"' && i + 2 < n && bytes[i + 1] == b'"' && bytes[i + 2] == b'"' {
                    bare[i] = b' ';
                    bare[i + 1] = b' ';
                    bare[i + 2] = b' ';
                    i += 3;
                    state = State::Code;
                } else if b == b'\n' {
                    i += 1;
                } else {
                    bare[i] = b' ';
                    i += 1;
                }
            }
        }
    }
    MaskedSource {
        // 掩码只写空格,原文其余字节不变,结果必然是合法 UTF-8。
        code: String::from_utf8(code).unwrap_or_else(|_| content.to_string()),
        bare: String::from_utf8(bare).unwrap_or_else(|_| content.to_string()),
    }
}

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
    // 正则提取统一跑在掩码源码上(保偏移):结构正则用 bare(注释+字符串全掩),
    // 需读字符串实参的正则用 code(仅掩注释)。tree-sitter 路径不受影响。
    let masked = mask_java(&file.content);
    // 先跑 AST 遍历:产出 Bean DI 边 + 收集 Spring 信号(事务/定时),后者作为
    // metadata 挂到对应 class 实体,故必须在 class 实体创建前完成。
    let mut signals: HashMap<String, SpringSignals> = HashMap::new();
    let mut injected_fields: HashMap<String, Vec<(String, String)>> = HashMap::new();
    // 同名方法(重载/同文件跨类同名)收集为 Vec:A+ 策略——调用解析遇歧义跳过不猜。
    let mut methods: HashMap<String, Vec<EntityId>> = HashMap::new();
    let mut invocations: Vec<Invocation> = Vec::new();
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
            &mut injected_fields,
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
    // 同文件 method 调用 → Calls 边。A+ 策略:caller/callee 名字对应多个方法
    // (重载或同文件跨类同名)即歧义 → 跳过不猜连;唯一命中才建边。
    for inv in &invocations {
        let (Some(caller_ids), Some(callee_ids)) =
            (methods.get(&inv.caller), methods.get(&inv.callee))
        else {
            continue;
        };
        let [caller_id] = caller_ids.as_slice() else {
            continue;
        };
        let [callee_id] = callee_ids.as_slice() else {
            continue;
        };
        if caller_id == callee_id {
            continue;
        }
        edges.push(
            Edge::new(caller_id.clone(), callee_id.clone(), EdgeKind::Calls).with_evidence(
                path,
                inv.line,
                inv.line,
                EvidenceClass::Inferred,
                0.7,
                "same-file method call",
            ),
        );
    }
    // 把调用意图存入 method 实体 metadata.invokes,供 resolve_cross_stack 跨文件解析
    // (Controller→Service 这类跨文件调用 = 注入依赖类型 + 方法名匹配)。按 caller short
    // name 分组回填;重载同名方法共享 invokes(跨文件解析本就按名匹配,歧义由
    // resolve 层的 A+ 防护兜底)。
    let mut invokes_by_caller: HashMap<&str, Vec<serde_json::Value>> = HashMap::new();
    for inv in &invocations {
        invokes_by_caller.entry(inv.caller.as_str()).or_default().push(json!({
            "name": inv.callee,
            "line": inv.line,
            "receiver_kind": inv.receiver_kind,
            "receiver": inv.receiver,
        }));
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
    // 类型声明统一采集一次(掩码后源码,注释里的 "class Foo" 不再产幻影实体),
    // 供实体创建、class_offsets、字段所属类判定三处复用。
    let class_hits: Vec<(usize, String, EntityKind)> = JAVA_CLASS
        .captures_iter(&masked.bare)
        .map(|capture| {
            let name = capture.get(2).unwrap();
            let kind = if &capture[1] == "interface" {
                EntityKind::Interface
            } else {
                EntityKind::Class
            };
            (name.start(), name.as_str().to_string(), kind)
        })
        .collect();
    // 字段所属类判定用:(class name offset, name) 序列。
    let classes: Vec<(usize, String)> = class_hits
        .iter()
        .map(|(offset, name, _)| (*offset, name.clone()))
        .collect();
    for (offset, name, kind) in &class_hits {
        let line = line_of(&file.content, *offset);
        let mut entity = Entity::new(
            EntityId::stable("workspace", path, *kind, name.as_str(), ""),
            *kind,
            name.as_str(),
            name.as_str(),
        );
        let mut meta = serde_json::Map::new();
        if let Some(sig) = signals.get(name.as_str()) {
            if sig.transactional {
                meta.insert("transactional".into(), json!(true));
            }
            if sig.scheduled {
                meta.insert("scheduled".into(), json!(true));
            }
        }
        if let Some(fields) = injected_fields.get(name.as_str())
            && !fields.is_empty()
        {
            let arr: Vec<serde_json::Value> = fields
                .iter()
                .map(|(f, t)| json!({ "name": f, "type": t }))
                .collect();
            meta.insert("injected_fields".into(), serde_json::Value::Array(arr));
        }
        if !meta.is_empty() {
            entity = entity.with_metadata(serde_json::Value::Object(meta));
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
    for capture in JAVA_FIELD.captures_iter(&masked.bare) {
        let name = capture.get(1).unwrap();
        let line = line_of(&file.content, name.start());
        // EntityId 含所属类判别符:同文件跨类同名字段(外部类+内部类常见)不再坍缩。
        let qualified = field_qualified_name(name.as_str(), &classes, name.start());
        let entity = Entity::new(
            field_entity_id(path, name.as_str(), &classes, name.start()),
            EntityKind::Field,
            name.as_str(),
            format!("{path}#{qualified}"),
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
    // class/interface 与 method 的 name offset,供 base 与 endpoint 注解的级别判定。
    method_spans.sort_by_key(|(offset, _)| *offset);
    let class_offsets: Vec<usize> = class_hits.iter().map(|(offset, _, _)| *offset).collect();
    // 类级 base:取其后最近声明是 class/interface(且中间无更近的 method)的
    // @RequestMapping 的 path —— 排除方法级 @RequestMapping 被误当 base(否则
    // base=path 再拼方法 path 会翻倍成 /foo/foo)。
    let base = REQUEST_MAPPING
        .captures_iter(&masked.code)
        .find_map(|capture| {
            let ann_end = capture.get(0)?.end();
            let nearest_class = class_offsets.iter().filter(|off| **off >= ann_end).min().copied();
            let nearest_method = method_spans
                .iter()
                .map(|(off, _)| *off)
                .filter(|off| *off >= ann_end)
                .min();
            match (nearest_class, nearest_method) {
                (Some(coff), Some(moff)) if moff < coff => None, // 方法更近 → 方法级,跳过
                (Some(_), _) => annotation_path(capture.get(1)?.as_str()),
                _ => None,
            }
        })
        .unwrap_or_default();
    for capture in METHOD_MAPPING.captures_iter(&masked.code) {
        let matched = capture.get(0).unwrap();
        let ann_offset = matched.start();
        // 配对 ann 之后最近的 method;若 ann 与该 method 之间隔着 class/interface 声明,
        // 说明这是类级 @RequestMapping(应仅作 base,不产方法 endpoint)→ 跳过。
        let pair = method_spans
            .iter()
            .filter(|(offset, _)| *offset > ann_offset)
            .min_by_key(|(offset, _)| *offset);
        let is_class_level = match pair {
            Some((method_offset, _)) => class_offsets
                .iter()
                .any(|&coff| coff > ann_offset && coff < *method_offset),
            None => true,
        };
        if is_class_level {
            continue;
        }
        // group1 缺失 = @RequestMapping(无动词)→ method 通配,metadata 不含 method,
        // 让 analysis 端 endpoint_method=None → 与任意前端 call_method 低置信匹配。
        let method = capture.get(1).map(|m| m.as_str().to_uppercase());
        // group2 = 注解参数列表;path 经 annotation_path 解析(value 可在任意属性位)。
        // 括号内无字符串字面量(如纯 method=POST 无路径)→ 不产 endpoint。
        let Some(path_arg) = capture.get(2).and_then(|m| annotation_path(m.as_str())) else {
            continue;
        };
        let endpoint_path = normalize_path(&format!("{base}{path_arg}"));
        let name = match &method {
            Some(verb) => format!("{verb} {endpoint_path}"),
            None => format!("ANY {endpoint_path}"),
        };
        let line = line_of(&file.content, matched.start());
        let mut meta = serde_json::Map::new();
        if let Some(verb) = &method {
            meta.insert("method".into(), json!(verb));
        }
        meta.insert("path".into(), json!(endpoint_path));
        let entity = Entity::new(
            EntityId::stable("workspace", path, EntityKind::HttpEndpoint, &name, ""),
            EntityKind::HttpEndpoint,
            &name,
            &name,
        )
        .with_metadata(serde_json::Value::Object(meta))
        .with_evidence(path, line, line, EvidenceClass::Fact, 1.0, "Spring mapping annotation");
        let endpoint_id = entity.id.clone();
        add_contained(file, path, entity, line, entities, edges);
        // method→endpoint(Exposes):mapping 注解贴在方法声明前,配对"注解 offset 之后
        // 最近的 method",让 find_endpoint/relay 能从 URL 追到处理方法。
        if let Some((_, method_id)) = pair {
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
    extract_custom_endpoints(file, path, &masked, entities, edges, config);
    extract_mybatis_plus(file, path, &masked, entities, edges);
    extract_implements(&masked, entities);
    extract_extends(&masked, entities);
    extract_interface_endpoints(file, path, entities, edges, config);
    extract_annotations(file, path, &masked, &method_spans, entities, edges, config);
    extract_tests(file, path, &masked, &method_spans, entities, edges);
    extract_jobs(file, path, &masked, &method_spans, entities, edges, config);
    extract_aspects(&masked, &method_spans, entities, edges);
    Ok(())
}

/// 字段 qualified_name:有所属类时前缀类名(`Outer.name`),消除同文件跨类同名字段歧义。
/// classes 为 (class name offset, name) 序列;所属类 = offset ≤ 字段 offset 的最近 class。
fn field_qualified_name(name: &str, classes: &[(usize, String)], offset: usize) -> String {
    match classes.iter().rev().find(|(start, _)| *start <= offset) {
        Some((_, class)) => format!("{class}.{name}"),
        None => name.to_string(),
    }
}

/// 字段 EntityId(与 field_qualified_name 同源,保证实体与引用它的边 id 一致)。
fn field_entity_id(path: &str, name: &str, classes: &[(usize, String)], offset: usize) -> EntityId {
    let qualified = field_qualified_name(name, classes, offset);
    EntityId::stable("workspace", path, EntityKind::Field, &qualified, "")
}

fn extract_custom_endpoints(
    file: &SourceFile,
    path: &str,
    masked: &MaskedSource,
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
    for capture in custom_re.captures_iter(&masked.code) {
        let token = capture.get(0).unwrap();
        let annotation = capture.get(1).unwrap();
        let args = capture.get(2).map(|m| m.as_str()).unwrap_or("");
        // value= 优先于首个裸字符串(容忍 @MosApi(group="x", value="CODE") 写法)。
        let value = annotation_path(args).unwrap_or_default();
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
    masked: &MaskedSource,
    entities: &mut Vec<Entity>,
    edges: &mut Vec<Edge>,
) {
    let content = &file.content;

    // 收集 class / field 的 (名字 offset, 名字),复用 extract_java 同源 regex
    // (bare 掩码:注释里的声明样式文本不产幻影,offset 与 code 档一致)。
    let classes: Vec<(usize, String)> = JAVA_CLASS
        .captures_iter(&masked.bare)
        .map(|c| {
            let name = c.get(2).unwrap();
            (name.start(), name.as_str().to_string())
        })
        .collect();
    let fields: Vec<(usize, String)> = JAVA_FIELD
        .captures_iter(&masked.bare)
        .map(|c| {
            let name = c.get(1).unwrap();
            (name.start(), name.as_str().to_string())
        })
        .collect();

    // (1) @TableName → 类 → Table + (后续)类内字段 → Column。
    // table_by_class: class_name -> table_name(本文件内 @TableName 标注的实体类)
    let mut table_by_class: HashMap<String, String> = HashMap::new();
    for table_cap in MP_TABLE_NAME.captures_iter(&masked.code) {
        // group1 = 注解参数列表;表名经 annotation_path 提取(value 可在任意属性位)。
        let Some(table_name_m) = table_cap.get(1).and_then(|m| annotation_path(m.as_str())) else {
            continue;
        };
        let table_name_str = table_name_m;
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
        table_by_class.insert(class_name.clone(), table_name_str.clone());
        let line = line_of(content, table_cap.get(0).unwrap().start());
        let table = Entity::new(
            EntityId::stable("workspace", path, EntityKind::Table, &table_name_str, ""),
            EntityKind::Table,
            &table_name_str,
            &table_name_str,
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
        .captures_iter(&masked.code)
        .chain(MP_TABLE_ID_VAL.captures_iter(&masked.code))
    {
        let ann_match = ann.get(0).unwrap();
        let Some(col) = ann.get(1).and_then(|m| annotation_path(m.as_str())) else {
            continue;
        };
        let ann_start = ann_match.start();
        if let Some((foff, _)) = fields
            .iter()
            .filter(|(foff, _)| *foff > ann_start)
            .min_by_key(|(foff, _)| *foff)
        {
            explicit_col
                .entry(*foff)
                .or_insert((col, line_of(content, ann_start)));
        }
    }

    // exist=false → 被标注的字段偏移集合(跳过)
    let non_existent: HashSet<usize> = MP_NON_EXISTENT
        .captures_iter(&masked.code)
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
        // 与 extract_java 的 Field 实体同公式(含所属类判别符),保证边能命中实体。
        let field_id = field_entity_id(path, fname, &classes, *foff);
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

    // (2) Mapper 接口:interface XxxMapper extends BaseMapper<Entity>(bare 掩码,
    // 注释里的 BaseMapper 字样不产幻影 Mapper)
    for mapper_cap in MP_MAPPER.captures_iter(&masked.bare) {
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

    // (3) Wrapper 链式列引用:.eq("col", …) 等(code 掩码:注释里的 .eq 不命中,
    // 字符串实参保留)
    for wrapper_cap in MP_WRAPPER_COLUMN.captures_iter(&masked.code) {
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
    for lambda_cap in MP_WRAPPER_LAMBDA.captures_iter(&masked.code) {
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
fn extract_implements(masked: &MaskedSource, entities: &mut [Entity]) {
    let caps: Vec<(String, Vec<serde_json::Value>)> = JAVA_IMPLEMENTS
        .captures_iter(&masked.bare)
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

/// extends 关系 + abstract 标记提取:存 class.metadata.superclass(超类简单名,单继承)与
/// class.metadata.abstract(true)。跨文件继承边(SuperclassOf)由 resolve_cross_stack 按
/// superclass 名解析,与 implements 同模式(Extract 层 EntityId path-scoped,建不了跨文件边)。
fn extract_extends(masked: &MaskedSource, entities: &mut [Entity]) {
    // superclass:class Sub extends Super(单继承,去泛型)。bare 掩码:Javadoc 里的
    // "class A extends B" 字样不再污染 metadata.superclass。
    for cap in JAVA_EXTENDS.captures_iter(&masked.bare) {
        let class_name = cap[1].to_string();
        let superclass = cap[2].to_string();
        for entity in entities.iter_mut() {
            if entity.kind == EntityKind::Class && entity.name == class_name {
                let mut meta = match entity.metadata.clone() {
                    serde_json::Value::Object(map) => map,
                    _ => serde_json::Map::new(),
                };
                meta.insert("superclass".into(), serde_json::Value::String(superclass.clone()));
                entity.metadata = serde_json::Value::Object(meta);
            }
        }
    }
    // abstract:abstract class Foo(不论有无 extends)。
    for cap in JAVA_ABSTRACT.captures_iter(&masked.bare) {
        let class_name = cap[1].to_string();
        for entity in entities.iter_mut() {
            if entity.kind == EntityKind::Class && entity.name == class_name {
                let mut meta = match entity.metadata.clone() {
                    serde_json::Value::Object(map) => map,
                    _ => serde_json::Map::new(),
                };
                meta.insert("abstract".into(), serde_json::Value::Bool(true));
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
        let class_id = EntityId::stable("workspace", path, EntityKind::Class, &class_name, "");
        let endpoint_id = EntityId::stable(
            "workspace",
            path,
            EntityKind::HttpEndpoint,
            &class_name,
            &format!("iface:{iface}"),
        );
        let entity = Entity::new(endpoint_id.clone(), EntityKind::HttpEndpoint, &class_name, &class_name)
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
        // 关联入口方法:该类(Declares 边)里名为 handle/bizProcess 等约定入口方法的,建
        // method→endpoint Exposes 边,让 relay/find_endpoint 能从 RMB 入口追到处理逻辑。
        // 入口方法名是约定(mes/mos 自研框架无注解标入口),后续可配置化扩展。
        const ENTRY_METHODS: &[&str] =
            &["handle", "handleRequest", "bizProcess", "process", "apiProcess"];
        let entry_edges: Vec<Edge> = edges
            .iter()
            .filter(|edge| edge.kind == EdgeKind::Declares && edge.source == class_id)
            .filter_map(|edge| {
                let is_entry = entities
                    .iter()
                    .any(|entity| entity.id == edge.target
                        && ENTRY_METHODS.contains(&entity.name.as_str()));
                is_entry.then(|| {
                    Edge::new(edge.target.clone(), endpoint_id.clone(), EdgeKind::Exposes).with_evidence(
                        path,
                        line,
                        line,
                        EvidenceClass::Inferred,
                        0.8,
                        "entry method of custom RPC handler (implements framework interface)",
                    )
                })
            })
            .collect();
        edges.extend(entry_edges);
    }
}

/// 通用注解索引(P1-1):白名单注解(@Transactional 等业务/框架注解)→ Annotation 实体 +
/// owner-[Annotated]->annotation 边(Fact)。owner 按 offset 配对到 ann 之后最近的
/// class/interface/method/field 声明。默认白名单(非全扫)避免 @Override 等噪音爆炸。
fn extract_annotations(
    file: &SourceFile,
    path: &str,
    masked: &MaskedSource,
    method_spans: &[(usize, EntityId)],
    entities: &mut Vec<Entity>,
    edges: &mut Vec<Edge>,
    config: &SemanticsConfig,
) {
    let whitelist: HashSet<&str> = config.annotation_whitelist.iter().map(|s| s.as_str()).collect();
    if whitelist.is_empty() {
        return;
    }
    // 黑名单兜底：即便白名单（含用户自填全集替换）误命中 @Override 等噪音也跳过。
    let blacklist: HashSet<&str> = config.annotation_blacklist.iter().map(|s| s.as_str()).collect();
    let content = &file.content;
    // owner 候选:(声明 name offset, EntityId)。class/interface、method、field 合并取最近。
    // 全部跑在 bare 掩码上,且 Field id 与 extract_java 同公式(含所属类判别符)。
    let mut owners: Vec<(usize, EntityId)> = Vec::new();
    let mut classes: Vec<(usize, String)> = Vec::new();
    for capture in JAVA_CLASS.captures_iter(&masked.bare) {
        let name = capture.get(2).unwrap();
        let kind = if &capture[1] == "interface" {
            EntityKind::Interface
        } else {
            EntityKind::Class
        };
        owners.push((
            name.start(),
            EntityId::stable("workspace", path, kind, name.as_str(), ""),
        ));
        classes.push((name.start(), name.as_str().to_string()));
    }
    for (offset, id) in method_spans {
        owners.push((*offset, id.clone()));
    }
    for capture in JAVA_FIELD.captures_iter(&masked.bare) {
        let name = capture.get(1).unwrap();
        owners.push((
            name.start(),
            field_entity_id(path, name.as_str(), &classes, name.start()),
        ));
    }
    owners.sort_by_key(|(offset, _)| *offset);
    // AT_ANNOTATION 跑在 bare 掩码上:Javadoc/字符串里提及的 @Transactional 等不再
    // 产幻影 Annotation 实体(历史污染 annotation 覆盖指标的主要来源)。
    for capture in AT_ANNOTATION.captures_iter(&masked.bare) {
        let ann_name = capture.get(1).unwrap();
        if !whitelist.contains(ann_name.as_str()) || blacklist.contains(ann_name.as_str()) {
            continue;
        }
        let ann_offset = capture.get(0).unwrap().start();
        let Some((_, owner_id)) = owners
            .iter()
            .filter(|(offset, _)| *offset > ann_offset)
            .min_by_key(|(offset, _)| *offset)
        else {
            continue;
        };
        let line = line_of(content, ann_offset);
        let annotation = Entity::new(
            EntityId::stable(
                "workspace",
                path,
                EntityKind::Annotation,
                ann_name.as_str(),
                &format!("{line}"),
            ),
            EntityKind::Annotation,
            ann_name.as_str(),
            format!("{path}#{}@{line}", ann_name.as_str()),
        )
        .with_evidence(path, line, line, EvidenceClass::Fact, 1.0, "annotation usage");
        let ann_id = annotation.id.clone();
        entities.push(annotation);
        edges.push(
            Edge::new(owner_id.clone(), ann_id, EdgeKind::Annotated).with_evidence(
                path,
                line,
                line,
                EvidenceClass::Fact,
                1.0,
                "entity annotated with @…",
            ),
        );
    }
}

/// 测试用例(P1-4):@Test 方法 → TestCase 实体(Fact)。Tests 边(test_class→被测类)
/// 由 resolve_cross_stack 按命名约定解析(见 analysis)。
fn extract_tests(
    file: &SourceFile,
    path: &str,
    masked: &MaskedSource,
    method_spans: &[(usize, EntityId)],
    entities: &mut Vec<Entity>,
    _edges: &mut Vec<Edge>,
) {
    let content = &file.content;
    // 先借 entities 收集 name 映射、提取 (method_name, line) 后释放借用,再 push TestCase
    // (push 需要 mutable borrow,与不可变 name_by_id 冲突)。
    let hits: Vec<(String, u32)> = {
        let name_by_id: HashMap<&EntityId, &str> = entities
            .iter()
            .filter(|entity| entity.kind == EntityKind::Method)
            .filter_map(|entity| Some((&entity.id, entity.name.as_str())))
            .collect();
        AT_TEST
            .captures_iter(&masked.bare)
            .filter_map(|capture| {
                let ann_offset = capture.get(0)?.start();
                let (_, method_id) = method_spans
                    .iter()
                    .filter(|(offset, _)| *offset > ann_offset)
                    .min_by_key(|(offset, _)| *offset)?;
                let line = line_of(content, ann_offset);
                let method_name = name_by_id.get(method_id).copied().unwrap_or("test").to_string();
                Some((method_name, line))
            })
            .collect()
    };
    for (method_name, line) in hits {
        let test_case = Entity::new(
            EntityId::stable(
                "workspace",
                path,
                EntityKind::TestCase,
                &method_name,
                &format!("test:{line}"),
            ),
            EntityKind::TestCase,
            &method_name,
            format!("{path}#test:{method_name}:{line}"),
        )
        .with_metadata(json!({"tested_method": method_name}))
        .with_evidence(path, line, line, EvidenceClass::Fact, 1.0, "JUnit @Test method");
        entities.push(test_case);
    }
}

/// 调度入口(P1-6):@Scheduled/@XxlJob/@JobHandler 方法 → Job 实体 + Job-[Schedules]->handler。
/// Job 作为端到端链路的定时起点(batch→调用链→表)。
fn extract_jobs(
    file: &SourceFile,
    path: &str,
    masked: &MaskedSource,
    method_spans: &[(usize, EntityId)],
    entities: &mut Vec<Entity>,
    edges: &mut Vec<Edge>,
    config: &SemanticsConfig,
) {
    let sched = &config.scheduler_annotations;
    if sched.is_empty() {
        return;
    }
    let alternation = sched
        .iter()
        .map(|annotation| regex::escape(annotation))
        .collect::<Vec<_>>()
        .join("|");
    let re = Regex::new(&format!(r"@({alternation})\b")).unwrap();
    let content = &file.content;
    let scan_source = &masked.bare;
    // 先借 entities 收集 name 映射,提取 (method_name, method_id, line, trigger) 后释放,
    // 再 push Job + Schedules 边(mutable borrow 冲突)。
    let hits: Vec<(String, EntityId, u32, String)> = {
        let name_by_id: HashMap<&EntityId, &str> = entities
            .iter()
            .filter(|entity| entity.kind == EntityKind::Method)
            .filter_map(|entity| Some((&entity.id, entity.name.as_str())))
            .collect();
        re.captures_iter(scan_source)
            .filter_map(|capture| {
                let trigger = capture.get(1)?.as_str().to_string();
                let ann_offset = capture.get(0)?.start();
                let (_, method_id) = method_spans
                    .iter()
                    .filter(|(offset, _)| *offset > ann_offset)
                    .min_by_key(|(offset, _)| *offset)?;
                let line = line_of(content, ann_offset);
                let method_name = name_by_id.get(method_id).copied().unwrap_or("job").to_string();
                Some((method_name, method_id.clone(), line, trigger))
            })
            .collect()
    };
    for (method_name, method_id, line, trigger) in hits {
        let job = Entity::new(
            EntityId::stable(
                "workspace",
                path,
                EntityKind::Job,
                &method_name,
                &format!("sched:{line}"),
            ),
            EntityKind::Job,
            &method_name,
            format!("{path}#job:{method_name}:{line}"),
        )
        .with_metadata(json!({"trigger": trigger}))
        .with_evidence(path, line, line, EvidenceClass::Fact, 1.0, "scheduled job entry");
        let job_id = job.id.clone();
        entities.push(job);
        edges.push(
            Edge::new(job_id, method_id, EdgeKind::Schedules).with_evidence(
                path,
                line,
                line,
                EvidenceClass::Fact,
                1.0,
                "job schedules handler method",
            ),
        );
    }
}

/// AOP 切面(P1-2):@Around/@Before/@After 的 execution(pointcut) → 解析出全限定方法签名,
/// 存到 advice method 的 metadata.pointcut,由 resolve_cross_stack 全局匹配目标方法建
/// Intercepts 边(Inferred)。仅处理 `execution(返回类型 包.类.方法(..))` 完全签名形式,
/// 通配 *、组合 ||、within/bean 切点不处理(分阶段)。
fn extract_aspects(
    masked: &MaskedSource,
    method_spans: &[(usize, EntityId)],
    entities: &mut Vec<Entity>,
    _edges: &mut Vec<Edge>,
) {
    for capture in ADVICE_ANN.captures_iter(&masked.code) {
        let ann_offset = capture.get(0).unwrap().start();
        // group2 = 注解参数列表;pointcut 字面量经 annotation_path 提取(value 任意位)。
        let Some(pointcut_expr) = capture.get(2).and_then(|m| annotation_path(m.as_str())) else {
            continue;
        };
        let Some(signature) = EXECUTION_SIG
            .captures(&pointcut_expr)
            .and_then(|inner| inner.get(1))
            .map(|m| m.as_str().to_string())
        else {
            continue;
        };
        let Some((_, method_id)) = method_spans
            .iter()
            .filter(|(offset, _)| *offset > ann_offset)
            .min_by_key(|(offset, _)| *offset)
        else {
            continue;
        };
        for entity in entities.iter_mut() {
            if entity.id == *method_id {
                let mut meta = match entity.metadata.clone() {
                    serde_json::Value::Object(map) => map,
                    _ => serde_json::Map::new(),
                };
                meta.insert("pointcut".into(), json!(signature));
                meta.insert("aspect_advice".into(), json!(true));
                entity.metadata = serde_json::Value::Object(meta);
                break;
            }
        }
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
    // owner class/interface 名 → [(字段名, 注入类型名)]。供 analysis 把
    // `this.service.foo()` 的 receiver=service 精确解析到注入类型(Step B 字段消歧)。
    injected_fields: &mut HashMap<String, Vec<(String, String)>>,
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
                                if let Some(field_name) = field_declarator_name(source, member) {
                                    injected_fields
                                        .entry(name.clone())
                                        .or_default()
                                        .push((field_name, type_name.clone()));
                                }
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
                    for (param_name, param_type) in constructor_param_name_types(source, node) {
                        if !param_type.is_empty() {
                            injected_fields
                                .entry(owner_name.clone())
                                .or_default()
                                .push((param_name, param_type.clone()));
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
                if let Some(field_name) = field_declarator_name(source, node) {
                    injected_fields
                        .entry(owner_name.clone())
                        .or_default()
                        .push((field_name, type_name.clone()));
                }
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
            visit_spring(child, source, file, path, entities, edges, signals, injected_fields);
        }
    }
}

/// 方法级提取:method_declaration → Method 实体;method_invocation → 调用信号。
/// 一次方法调用(caller 在方法体内调用 callee),含接收者分类以便跨文件解析。
struct Invocation {
    caller: String,
    callee: String,
    line: u32,
    /// bare(裸名)/this/field(this.x)/name(x 或 XxxUtil)/qualified(com.x.Y)/
    /// chain(a().b())/new(new X().b())/super。bare 与 name 走 analysis 名匹配,
    /// field 走注入字段精确解析(后续 Step),其余跳过(控噪)。
    receiver_kind: &'static str,
    receiver: Option<String>,
}

/// 解析 method_invocation 的接收者(object 字段)分类。tree-sitter Java:object 可缺省
/// (裸名)或为 this/super/identifier/field_access/method_invocation/object_creation_expression。
/// 仅 field(this.x)/name(x) 能可靠用于跨文件解析;链式/new/FQCN 成本高且噪音大,跳过。
fn classify_receiver(source: &[u8], inv_node: Node<'_>) -> (&'static str, Option<String>) {
    let Some(obj) = inv_node.child_by_field_name("object") else {
        return ("bare", None);
    };
    match obj.kind() {
        "this" => ("this", None),
        "super" => ("super", None),
        "identifier" => ("name", Some(node_text(source, obj))),
        "field_access" => {
            let inner = obj.child_by_field_name("object");
            let field = obj.child_by_field_name("field");
            match (inner, field) {
                (Some(i), Some(f)) if i.kind() == "this" && f.kind() == "identifier" => {
                    ("field", Some(node_text(source, f)))
                }
                _ => ("qualified", Some(node_text(source, obj))),
            }
        }
        "method_invocation" => ("chain", None),
        "object_creation_expression" => ("new", None),
        _ => ("bare", None),
    }
}

/// caller/callee 按方法名同文件匹配(低保真:跨类同名混淆、跨文件调用留后续)。
#[allow(clippy::too_many_arguments)]
fn visit_methods(
    node: Node<'_>,
    source: &[u8],
    file: &SourceFile,
    path: &str,
    entities: &mut Vec<Entity>,
    edges: &mut Vec<Edge>,
    methods: &mut HashMap<String, Vec<EntityId>>,
    invocations: &mut Vec<Invocation>,
    method_spans: &mut Vec<(usize, EntityId)>,
    current_method: Option<&str>,
) {
    if node.kind() == "method_declaration"
        && let Some(name_node) = node.child_by_field_name("name")
    {
        let name = node_text(source, name_node);
        let line = line_of(&file.content, name_node.start_byte());
        // 方法体结束行（闭合 `}` 所在行）；abstract/interface 无方法体时 == 声明行。
        // 走 metadata 而非改 end_line：后者下游（analysis 跨文件边）当声明行号用。
        let body_end_line = line_of(&file.content, node.end_byte());
        // 参数个数入 EntityId 判别符:同名重载不再哈希碰撞(旧方案 discriminator=""
        // 让两个重载坍缩为同一 id,store upsert 静默覆盖先声明的那个)。
        // 局限:同名同参个数、仅类型不同的重载仍碰撞(罕见,接受)。
        let arity = node
            .child_by_field_name("parameters")
            .map(|params| {
                (0..params.named_child_count())
                    .filter_map(|i| params.named_child(i))
                    .filter(|p| matches!(p.kind(), "formal_parameter" | "spread_parameter"))
                    .count()
            })
            .unwrap_or(0);
        let entity = Entity::new(
            EntityId::stable("workspace", path, EntityKind::Method, &name, &format!("arity:{arity}")),
            EntityKind::Method,
            &name,
            format!("{path}#{name}"),
        )
        .with_evidence(path, line, line, EvidenceClass::Fact, 1.0, "Java method declaration")
        .with_metadata(json!({ "body_end_line": body_end_line, "arity": arity }));
        let id = entity.id.clone();
        add_contained(file, path, entity, line, entities, edges);
        methods.entry(name.clone()).or_default().push(id.clone());
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
            let (receiver_kind, receiver) = classify_receiver(source, node);
            invocations.push(Invocation {
                caller: caller.to_string(),
                callee,
                line: line_of(&file.content, node.start_byte()),
                receiver_kind,
                receiver,
            });
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

/// 构造器参数的 (名, 类型) 列表(formal_parameter.name + .type)。供 Step B 采注入字段名。
fn constructor_param_name_types(source: &[u8], node: Node<'_>) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i)
            && child.kind() == "formal_parameters"
        {
            for j in 0..child.named_child_count() {
                if let Some(param) = child.named_child(j)
                    && param.kind() == "formal_parameter"
                    && let Some(type_node) = param.child_by_field_name("type")
                    && let Some(name_node) = param.child_by_field_name("name")
                {
                    pairs.push((node_text(source, name_node), node_text(source, type_node)));
                }
            }
        }
    }
    pairs
}

/// field_declaration 的声明名(variable_declarator.name)。供 Step B 采注入字段名。
fn field_declarator_name(source: &[u8], field_node: Node<'_>) -> Option<String> {
    for i in 0..field_node.named_child_count() {
        if let Some(child) = field_node.named_child(i)
            && child.kind() == "variable_declarator"
            && let Some(name_node) = child.child_by_field_name("name")
        {
            return Some(node_text(source, name_node));
        }
    }
    None
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
