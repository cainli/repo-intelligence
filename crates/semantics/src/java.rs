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

// 类型声明锚(class|interface|enum)。enum 产实体(实现策略接口的 enum 在 mes 类项目中
// 常见);record 上下文关键字误命中风险高且不吃 implements 的场景罕见,本轮不收。
// implements/superclass 不再用正则抽取——见 scan_type_headers(类型头平衡扫描)。
static JAVA_CLASS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(class|interface|enum)\s+([A-Za-z_]\w*)").unwrap());
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
    // 括号可选(2026-09-04):裸 @GetMapping(无参,路径全靠类级 base)曾因要求括号
    // 连匹配都不进,ruoyi ~23 个方法丢失 endpoint(SysProfileController 的
    // GET/PUT /system/user/profile)。\b 防 @GetMappingXxx 误配。
    Regex::new(r#"@(?:(Get|Post|Put|Delete|Patch)Mapping|RequestMapping)\b\s*(?:\(([^)]*)\))?"#)
        .unwrap()
});
static STRING_LITERAL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""([^"]*)""#).unwrap());
// 注解参数里的 path 提取:value/path 属性优先(任意属性位置,兼容数组 value={"/a","/b"}
// 取首个元素),否则首个裸字符串字面量。
static ANN_VALUE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?:value|path)\s*=\s*\{?\s*"([^"]*)""#).unwrap());

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

/// 类级 base 与方法级 path 的拼接(Spring URL 语义)。返回 None = 无路径可表达,
/// 调用方跳过该注解不产 endpoint(与历史"完全无路径不产 endpoint"语义一致)。
///
/// Spring 语义要点(实现前先想清这三条):
/// 1. **恒拼接**:方法路径不管有没有前导 `/`,都是**追加**到 base 之后,不是"绝对路径
///    覆盖"。`base="/user"` + `"list"` 与 `base="/user"` + `"/list"` 结果相同——
///    直接字符串相接会把 `/user` + `"list"` 拼成 `/userlist`,必须以 `/` 分隔。
/// 2. **空方法路径回退 base**:裸 `@GetMapping` / `@GetMapping("")` / `@GetMapping(method=…)`
///    的路径就是类级 base 本身;此时 base 为空才是真正的"无路径"(→ None)。
/// 3. 斜杠归一(`/` 重复、首尾 `/`)不用管——调用方随后过 `normalize_path`。
///    但拼接时插入的**分隔 `/` 要在这里补**,归一不会凭空造分隔符。
///
/// method_path 的 None(注解无参数组)与 Some("")(注解有参但 path 为空串)在此等价。
fn join_mapping_path(base: &str, method_path: Option<&str>) -> Option<String> {
    // 前导/尾随 `/` 都剥掉:Spring 里方法路径恒为追加,`"/x"` 与 `"x"` 等价;
    // `Some("/")` 视为空(方法路径就是 base 本身)。
    let method_path = method_path.unwrap_or("").trim_matches('/');
    if method_path.is_empty() {
        // 裸注解/空方法路径 → 回退 base;base 也空 = 无路径可表达(不产 endpoint)。
        (!base.is_empty()).then(|| base.to_string())
    } else {
        // 恒以 / 分隔拼接(分隔符只能在这里补,normalize_path 不造分隔符)。
        Some(format!("{base}/{method_path}"))
    }
}
// 通用注解简单名:@Foo(…) / @Foo → group1=Foo。用于白名单注解索引(P1-1)。
static AT_ANNOTATION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"@([A-Za-z_]\w*)").unwrap());
// @Test 方法定位(P1-4)。
static AT_TEST: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"@Test\b").unwrap());

// import 全限定名(含 static,剔通配符由提取侧做):写入每个 class 的 metadata.imports,
// analysis 层借其简单名末段做 Tests 边的第二推断路径(P0②)。跑在 bare 掩码上——
// Javadoc / 示例代码里的 "import x.y;" 字样不会误收。
static JAVA_IMPORT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*import\s+(?:static\s+)?([\w.]+(?:\.\*)?)\s*;").unwrap());
// AOP advice 注解 + 其参数列表(P1-2 增强)。参数列表支持两层嵌套括号——
// `@AfterReturning(pointcut = "@annotation(x)", returning = "j")` 一层,而
// `@Around("execution(* a.add*(..))")` 是两层(外层注解 > execution > (..));
// 旧 [^)]* 连一层都截不完整(实际 bug:字面量切在 repeatSubmit 后)。
static ADVICE_ANN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"@(Around|Before|After|AfterReturning|AfterThrowing)\s*\(((?:[^()]|\((?:[^()]|\([^()]*\))*\))*)\)"#,
    )
    .unwrap()
});
// @annotation(X) pointcut:X = 注解 FQN/短名,或 advice 方法参数名(如 @annotation(controllerLog))。
static PC_ANNOTATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"@annotation\s*\(\s*([\w.$]+)\s*\)").unwrap());
// execution(RET FQCN.method(..)) → group1=返回类型 group2=FQCN.method(支持通配 * 与包递归 ..)。
static EXECUTION_PAT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"execution\s*\(\s*([\w.$*]+)\s+([\w.$*]+)\s*\(").unwrap());
// 同文件 @Pointcut 方法声明:注解参数(支持嵌套括号)+ 其后 void 方法名。组2=方法名。
static POINTCUT_DEF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"@Pointcut\s*\(((?:[^()]|\([^()]*\))*)\)\s*[^;{}]*?\bvoid\s+(\w+)\s*\("#).unwrap()
});
// pointcut 整体是一个 @Pointcut 方法引用:"pc()" / "dataScopePoint()"。
static PC_REF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\w+)\s*\(\s*\)$").unwrap());
// static final String 常量(反射字面量一级传播)。bare 掩码上收集。
static CONST_STRING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"final\s+String\s+([A-Za-z_]\w*)\s*=\s*"([^"]*)""#).unwrap());
// Class.forName(字面量 | 常量标识符)。code 掩码上。
static FOR_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"Class\s*\.\s*forName\s*\(\s*(?:"([^"]+)"|([A-Za-z_]\w*))\s*\)"#).unwrap());
/// 文件路径 → 类 FQN:src/main/java 或 src/test/java 标记后段目录→包,文件名须与类名
/// 一致(一个文件多类时非 public 类不可采信)。非标布局返回 None,调用端不写 fqn——
/// 跨文件消解(execution/反射)宁缺毋滥。Windows 反斜杠先归一。
fn java_fqn_for_class(path: &str, class_name: &str) -> Option<String> {
    let norm = path.replace('\\', "/");
    let marker = ["src/main/java/", "src/test/java/"]
        .iter()
        .find_map(|m| norm.find(m).map(|i| i + m.len()))?;
    let stem = norm[marker..].strip_suffix(".java")?;
    let (pkg, file_class) = stem.rsplit_once('/')?;
    (file_class == class_name).then(|| pkg.replace('/', "."))
}

// abstract class Foo —— abstract 修饰符(不论有无 extends)。存 metadata.abstract 供 trace 标注。
// 组1 start = 类名 token start,与 class_hits offset 同源(offset 绑定用)。
static JAVA_ABSTRACT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\babstract\s+class\s+([A-Za-z_]\w*)").unwrap());

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
    let mut exception_refs: Vec<ExceptionRef> = Vec::new();
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
        walk_exceptions(
            tree.root_node(),
            file.content.as_bytes(),
            None,
            &mut exception_refs,
            &file.content,
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
        invokes_by_caller
            .entry(inv.caller.as_str())
            .or_default()
            .push(json!({
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
    // 异常流引用回填到 method.metadata.exception_flow(analysis 后处理按 type name
    // resolve 到 class 实体,建 throws/handles 边)。按 method 分组,重载同名共享。
    if !exception_refs.is_empty() {
        let mut exc_by_method: HashMap<&str, Vec<serde_json::Value>> = HashMap::new();
        for r in &exception_refs {
            exc_by_method
                .entry(r.method.as_str())
                .or_default()
                .push(json!({
                    "type": r.type_name,
                    "line": r.line,
                    "flow": r.kind,
                }));
        }
        for entity in entities.iter_mut() {
            if entity.kind != EntityKind::Method {
                continue;
            }
            let Some(flows) = exc_by_method.get(entity.name.as_str()) else {
                continue;
            };
            let mut meta = match entity.metadata.clone() {
                serde_json::Value::Object(map) => map,
                _ => serde_json::Map::new(),
            };
            meta.insert(
                "exception_flow".into(),
                serde_json::Value::Array(flows.clone()),
            );
            entity.metadata = serde_json::Value::Object(meta);
        }
    }
    // 类型声明统一采集一次(掩码后源码,注释里的 "class Foo" 不再产幻影实体),
    // 供实体创建、class_offsets、字段所属类判定三处复用。
    let class_hits: Vec<(usize, String, EntityKind)> = JAVA_CLASS
        .captures_iter(&masked.bare)
        .map(|capture| {
            let name = capture.get(2).unwrap();
            let kind = match &capture[1] {
                "interface" => EntityKind::Interface,
                "enum" => EntityKind::Enum,
                _ => EntityKind::Class,
            };
            (name.start(), name.as_str().to_string(), kind)
        })
        .collect();
    // 字段所属类判定用:(class name offset, name) 序列。
    let classes: Vec<(usize, String)> = class_hits
        .iter()
        .map(|(offset, name, _)| (*offset, name.clone()))
        .collect();
    // implements/superclass/abstract 在实体创建时按 offset 精确绑定——旧实现按名绑定,
    // 同文件两个同名类(顶层 + 内部类)会互相覆盖/偷取继承关系。
    let type_headers = scan_type_headers(&masked.bare);
    let headers_by_offset: HashMap<usize, &TypeHeader> =
        type_headers.iter().map(|h| (h.decl_offset, h)).collect();
    let abstract_offsets: HashSet<usize> = JAVA_ABSTRACT
        .captures_iter(&masked.bare)
        .filter_map(|c| c.get(1).map(|m| m.start()))
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
        // 类 FQN(标准 Maven/Gradle 布局从路径推导)。跨文件 pointcut/反射解析(analysis
        // 层)靠它把 execution 表达式与 Class.forName 字面量对到类实体;非标布局不写,
        // 消费端宁缺毋滥。
        if let Some(fqn) = java_fqn_for_class(path, name.as_str()) {
            meta.insert("fqn".into(), json!(fqn));
        }
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
        // implements/superclass(类型头扫描器):简单名键保持旧形态(下游
        // extract_interface_endpoints / analysis 名字索引不变),*_full 存全名原文
        // (FQCN 接口/超类时与简单名不同),analysis 消歧优先等值匹配。
        if let Some(h) = headers_by_offset.get(offset) {
            if !h.implements.is_empty() {
                meta.insert(
                    "implements".into(),
                    json!(h.implements.iter().map(|(s, _)| s).collect::<Vec<_>>()),
                );
                meta.insert(
                    "implements_full".into(),
                    json!(h.implements.iter().map(|(_, f)| f).collect::<Vec<_>>()),
                );
            }
            if let Some((simple, full)) = &h.superclass {
                meta.insert("superclass".into(), json!(simple));
                meta.insert("superclass_full".into(), json!(full));
            }
        }
        if abstract_offsets.contains(offset) {
            meta.insert("abstract".into(), json!(true));
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
            let nearest_class = class_offsets
                .iter()
                .filter(|off| **off >= ann_end)
                .min()
                .copied();
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
        // group2 = 注解参数列表(裸注解时整个分组缺失);path 经 annotation_path 解析
        // (value 可在任意属性位)。无方法路径(裸注解 / 纯 method=POST)→ 回退类级 base;
        // base 也为空 → 无路径可表达,不产 endpoint(维持历史语义)。
        let method_path = capture.get(2).and_then(|m| annotation_path(m.as_str()));
        let Some(joined) = join_mapping_path(&base, method_path.as_deref()) else {
            continue;
        };
        let endpoint_path = normalize_path(&joined);
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
        .with_evidence(
            path,
            line,
            line,
            EvidenceClass::Fact,
            1.0,
            "Spring mapping annotation",
        );
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
    extract_imports(&masked, entities);
    extract_interface_endpoints(file, path, entities, edges, config);
    extract_annotations(file, path, &masked, &method_spans, entities, edges, config);
    extract_tests(file, path, &masked, &method_spans, entities, edges);
    extract_jobs(file, path, &masked, &method_spans, entities, edges, config);
    extract_aspects(&masked, &method_spans, entities, edges);
    extract_reflection(&masked, &method_spans, entities);
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

/// 类型头扫描结果。实体绑定用 decl_offset(class_hits 同源同偏移),不用名字——
/// 旧正则按名绑定,同文件两个同名类(顶层 + 内部类)互相覆盖继承关系。
struct TypeHeader {
    /// 类型名 token 的 start byte。
    decl_offset: usize,
    /// (简单名, 全名)。仅 class 的顶层 extends(单继承);interface 的 extends 是
    /// 多继承不采(BaseMapper 场景由 MP_MAPPER 专门处理),enum 语法上无 extends。
    superclass: Option<(String, String)>,
    /// class/enum 的顶层 implements。全名与简单名仅 FQCN 时不同。
    implements: Vec<(String, String)>,
}

/// full 名取末段简单名。
fn simple_name_of(full: &str) -> &str {
    full.rsplit('.').next().unwrap_or(full)
}

/// 类型头平衡扫描:替代旧 IMPLEMENTS/JAVA_EXTENDS 正则。正则版四个实测缺陷(mes-activity
/// 反馈)在此根治:① 嵌套泛型逗号泄漏——`implements Map<String, Handler<X>>` 把类型实参
/// 基名当接口(幻觉误归);② FQCN 接口(`implements com.acme.ISvc`)过不了标识符校验整条
/// 丢弃;③ bounded type parameter 吃掉 extends——`<T extends Comparable<T>>` 的 superclass
/// 变 "Comparable",FQCN 超类截成 "com";④ 声明头 `[^{]*?` 跨类偷取后续类的 implements。
/// 做法:锚与 JAVA_CLASS 同源(decl_offset 对齐 class_hits),从类型名后扫到顶层 `{`,
/// 按 angle-depth 平衡 token 化,只认 depth==0 的 extends/implements。
fn scan_type_headers(bare: &str) -> Vec<TypeHeader> {
    const HEADER_CAP: usize = 2048; // 声明头上限,坏输入防御
    let bytes = bare.as_bytes();
    let mut headers = Vec::new();
    for cap in JAVA_CLASS.captures_iter(bare) {
        let name_m = cap.get(2).unwrap();
        let kind = match &cap[1] {
            "interface" => EntityKind::Interface,
            "enum" => EntityKind::Enum,
            _ => EntityKind::Class,
        };
        // 头部 token 化:(文本, angle-depth)。词 = ASCII 标识符字符(掩码后源码的
        // 结构字符全 ASCII,非 ASCII 字节作分隔符);`<`/`>` 实时升降 depth。
        let mut tokens: Vec<(String, i32)> = Vec::new();
        let mut angle: i32 = 0;
        let mut cur = String::new();
        let mut cur_depth = 0;
        let mut i = name_m.end();
        let limit = (i + HEADER_CAP).min(bytes.len());
        while i < limit {
            let b = bytes[i];
            i += 1;
            if b.is_ascii_alphanumeric() || b == b'_' || b == b'$' {
                if cur.is_empty() {
                    cur_depth = angle;
                }
                cur.push(b as char);
                continue;
            }
            if !cur.is_empty() {
                tokens.push((std::mem::take(&mut cur), cur_depth));
            }
            match b {
                b'<' => {
                    angle += 1;
                    tokens.push(("<".into(), angle));
                }
                b'>' => {
                    angle -= 1;
                    tokens.push((">".into(), angle));
                }
                b'.' | b',' => tokens.push(((b as char).to_string(), angle)),
                b'{' if angle == 0 => break,
                b';' => break,
                _ => {}
            }
            if angle < 0 {
                break;
            }
        }
        if !cur.is_empty() {
            tokens.push((cur, cur_depth));
        }
        // 顶层(depth==0)关键词条目定位。
        let mut extends_idx = None;
        let mut implements_idx = None;
        for (idx, (text, depth)) in tokens.iter().enumerate() {
            if *depth != 0 {
                continue;
            }
            match text.as_str() {
                "extends" if extends_idx.is_none() => extends_idx = Some(idx),
                "implements" if implements_idx.is_none() => implements_idx = Some(idx),
                _ => {}
            }
        }
        let superclass = if kind == EntityKind::Class {
            extends_idx
                .map(|ext| {
                    let to = implements_idx.unwrap_or(tokens.len());
                    parse_type_list(&tokens, ext + 1, to)
                })
                .and_then(|list| list.into_iter().next())
        } else {
            None
        };
        let implements = implements_idx
            .map(|imp| parse_type_list(&tokens, imp + 1, tokens.len()))
            .unwrap_or_default();
        let to_pairs = |fulls: Vec<String>| {
            fulls.into_iter()
                .map(|f| {
                    let simple = simple_name_of(&f).to_string();
                    (simple, f)
                })
                .collect::<Vec<_>>()
        };
        headers.push(TypeHeader {
            decl_offset: name_m.start(),
            superclass: superclass.map(|f| {
                let simple = simple_name_of(&f).to_string();
                (simple, f)
            }),
            implements: to_pairs(implements),
        });
    }
    headers
}

/// 解析继承/实现子句 tokens[from..to]:顶层(depth==0)`.` 连接词组成全名,`<...>`
/// 泛型段按 angle 平衡跳过(类型实参不进结果),顶层 `,` 切分。遇顶层 permits 终止
/// (sealed 的允许子类列表不属于 implements)。
fn parse_type_list(tokens: &[(String, i32)], from: usize, to: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut full = String::new();
    let mut i = from;
    while i < to {
        let (text, depth) = &tokens[i];
        if *depth > 0 {
            if text == "<" {
                let mut inner = 0i32;
                while i < to {
                    match tokens[i].0.as_str() {
                        "<" => inner += 1,
                        ">" => {
                            inner -= 1;
                            if inner == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    i += 1;
                }
            }
            i += 1;
            continue;
        }
        match text.as_str() {
            "." if full.is_empty() => {} // 前导点:坏输入,忽略
            "." => full.push('.'),
            "," => {
                if !full.is_empty() {
                    out.push(std::mem::take(&mut full));
                }
            }
            "permits" => break,
            word => full.push_str(word),
        }
        i += 1;
    }
    if !full.is_empty() {
        out.push(full);
    }
    out
}

/// 文件级 imports → 同文件每个 class 的 metadata.imports(全限定名数组,剔通配符)。
/// 所有 class 统一记录、不做"仅测试类"特判:哪些信号可用由 analysis 层决定,
/// 提取层保持信号完整。static import 末段是成员名而非类名,匹配时天然落空,无害。
fn extract_imports(masked: &MaskedSource, entities: &mut [Entity]) {
    let imports: Vec<String> = JAVA_IMPORT
        .captures_iter(&masked.bare)
        .filter_map(|cap| cap.get(1))
        .map(|m| m.as_str().to_string())
        .filter(|fq| !fq.ends_with('*'))
        .collect();
    if imports.is_empty() {
        return;
    }
    for entity in entities.iter_mut() {
        if entity.kind != EntityKind::Class {
            continue;
        }
        let mut meta = match entity.metadata.clone() {
            serde_json::Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        meta.insert(
            "imports".into(),
            serde_json::Value::Array(
                imports
                    .iter()
                    .map(|fq| serde_json::Value::String(fq.clone()))
                    .collect(),
            ),
        );
        entity.metadata = serde_json::Value::Object(meta);
    }
}

/// implements 约定接口(ApiHandler/IBizProcess 等)的类视为自研 RPC 入口,补一个 HttpEndpoint
/// 实体(name=类名,即交易码/业务码),让 find_endpoint 能命中——mes/mos 的 RMB 入口普遍用
/// `@MosApi + implements ApiHandler` 这套自定义框架,纯注解识别覆盖不到。metadata.implements
/// 已由实体创建阶段的类型头扫描器(scan_type_headers)写入,时序天然满足。
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
        let entity = Entity::new(
            endpoint_id.clone(),
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
        // 关联入口方法:该类(Declares 边)里名为 handle/bizProcess 等约定入口方法的,建
        // method→endpoint Exposes 边,让 relay/find_endpoint 能从 RMB 入口追到处理逻辑。
        // 入口方法名是约定(mes/mos 自研框架无注解标入口),后续可配置化扩展。
        const ENTRY_METHODS: &[&str] = &[
            "handle",
            "handleRequest",
            "bizProcess",
            "process",
            "apiProcess",
        ];
        let entry_edges: Vec<Edge> = edges
            .iter()
            .filter(|edge| edge.kind == EdgeKind::Declares && edge.source == class_id)
            .filter_map(|edge| {
                let is_entry = entities.iter().any(|entity| {
                    entity.id == edge.target && ENTRY_METHODS.contains(&entity.name.as_str())
                });
                is_entry.then(|| {
                    Edge::new(edge.target.clone(), endpoint_id.clone(), EdgeKind::Exposes)
                        .with_evidence(
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
    let whitelist: HashSet<&str> = config
        .annotation_whitelist
        .iter()
        .map(|s| s.as_str())
        .collect();
    if whitelist.is_empty() {
        return;
    }
    // 黑名单兜底：即便白名单（含用户自填全集替换）误命中 @Override 等噪音也跳过。
    let blacklist: HashSet<&str> = config
        .annotation_blacklist
        .iter()
        .map(|s| s.as_str())
        .collect();
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
    // 全量注解短名记录(白名单过滤前):方法 metadata.annotations = 短名数组。业务标记
    // 注解(@Log/@DataScope 等)不进白名单不产实体,但切面 @annotation(controllerLog)
    // 的 pointcut 需要知道哪些方法带它——Intercepts 跨文件消解的数据源。
    let mut anns_by_owner: HashMap<EntityId, Vec<String>> = HashMap::new();
    for capture in AT_ANNOTATION.captures_iter(&masked.bare) {
        let ann_name = capture.get(1).unwrap();
        let ann_offset = capture.get(0).unwrap().start();
        let owner_id = owners
            .iter()
            .filter(|(offset, _)| *offset > ann_offset)
            .min_by_key(|(offset, _)| *offset)
            .map(|(_, id)| id);
        if let Some(owner_id) = owner_id {
            anns_by_owner
                .entry(owner_id.clone())
                .or_default()
                .push(ann_name.as_str().to_string());
        }
        if !whitelist.contains(ann_name.as_str()) || blacklist.contains(ann_name.as_str()) {
            continue;
        }
        let Some((_, owner_id)) = owner_id.map(|id| (0, id.clone())) else {
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
        .with_evidence(
            path,
            line,
            line,
            EvidenceClass::Fact,
            1.0,
            "annotation usage",
        );
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
    // 回填方法 metadata.annotations(取现有 object 再 insert,与 extract_aspects 的
    // pointcut 回填互不覆盖)。只回填 method:类/字段的注解已有白名单实体表达。
    for entity in entities.iter_mut() {
        if entity.kind != EntityKind::Method {
            continue;
        }
        if let Some(list) = anns_by_owner.get(&entity.id) {
            let mut meta = match entity.metadata.clone() {
                serde_json::Value::Object(map) => map,
                _ => serde_json::Map::new(),
            };
            meta.insert("annotations".into(), json!(list));
            entity.metadata = serde_json::Value::Object(meta);
        }
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
            .map(|entity| (&entity.id, entity.name.as_str()))
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
                let method_name = name_by_id
                    .get(method_id)
                    .copied()
                    .unwrap_or("test")
                    .to_string();
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
        .with_evidence(
            path,
            line,
            line,
            EvidenceClass::Fact,
            1.0,
            "JUnit @Test method",
        );
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
            .map(|entity| (&entity.id, entity.name.as_str()))
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
                let method_name = name_by_id
                    .get(method_id)
                    .copied()
                    .unwrap_or("job")
                    .to_string();
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
        .with_evidence(
            path,
            line,
            line,
            EvidenceClass::Fact,
            1.0,
            "scheduled job entry",
        );
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

/// AOP 切面(P1-2 增强):@Around/@Before/@After*/@AfterThrowing 的 pointcut 存入 advice
/// 方法的 metadata,由 resolve_cross_stack 跨文件解析建 Intercepts 边(Inferred 0.5)。
/// 真实项目(ruoyi 实测)主流写法是 `@annotation(参数名)`——旧实现只认"无通配 execution
/// 全限定签名",在 ruoyi 上 0 边。现分三类:
/// - `@annotation(X)`:X 为注解 FQN/短名直接存;X 为 advice 方法参数名时,在注解后方法
///   签名区消解参数类型短名(`ControllerLog controllerLog` → ControllerLog)
/// - `execution(RET FQCN.method(..))`:存 FQCN.method 原文(支持通配 * 与 ..),analysis
///   层编译正则匹配
/// - `"pcName()"`:引用同文件 @Pointcut 方法(先收集声明表再解引用,一级防环)
/// 其他形态(within/bean/组合表达式)存 pointcut_raw 供排查,不建边。
fn extract_aspects(
    masked: &MaskedSource,
    method_spans: &[(usize, EntityId)],
    entities: &mut [Entity],
    _edges: &mut Vec<Edge>,
) {
    // 同文件 @Pointcut 方法声明表:方法名 → 表达式。
    let pc_defs: HashMap<String, String> = POINTCUT_DEF
        .captures_iter(&masked.code)
        .filter_map(|c| {
            let expr = annotation_path(c.get(1).unwrap().as_str())?;
            Some((c.get(2)?.as_str().to_string(), expr))
        })
        .collect();
    for capture in ADVICE_ANN.captures_iter(&masked.code) {
        let ann_offset = capture.get(0).unwrap().start();
        // group2 = 注解参数列表;pointcut 字面量经 annotation_path 提取(value 任意位)。
        let Some(pointcut_expr) = capture.get(2).and_then(|m| annotation_path(m.as_str())) else {
            continue;
        };
        // @Pointcut 方法引用解引用(一级)。
        let pointcut_expr = match PC_REF.captures(&pointcut_expr) {
            Some(ref_m) => pc_defs
                .get(&ref_m[1])
                .cloned()
                .unwrap_or(pointcut_expr),
            None => pointcut_expr,
        };
        let Some((_, method_id)) = method_spans
            .iter()
            .filter(|(offset, _)| *offset > ann_offset)
            .min_by_key(|(offset, _)| *offset)
        else {
            continue;
        };
        // @annotation(X) 的参数名消解:注解后 500 字符(方法签名区)内找 "Type X[,)]",
        // 类型短名即注解类型。FQN(含 .)不经此步直接存。窗口法对签名写在注解后不远处的
        // 常规布局成立;方法体内同形文本的误匹配概率低,且最终产物是 Inferred 0.5 边。
        let resolved_annotation = PC_ANNOTATION
            .captures(&pointcut_expr)
            .map(|m| m[1].to_string())
            .filter(|x| !x.contains('.'))
            .and_then(|param| {
                let end = (ann_offset + 500).min(masked.code.len());
                let window = &masked.code[ann_offset..end];
                Regex::new(&format!(r"([A-Za-z_]\w*)\s+{}\s*[,)]", regex::escape(&param)))
                    .ok()?
                    .captures(window)
                    .map(|c| c[1].to_string())
            });
        let mut meta_patch = serde_json::Map::new();
        meta_patch.insert("aspect_advice".into(), json!(true));
        if let Some(pc_cap) = PC_ANNOTATION.captures(&pointcut_expr) {
            let target = resolved_annotation.unwrap_or_else(|| pc_cap[1].to_string());
            meta_patch.insert("pointcut_annotation".into(), json!(target));
        } else if let Some(m) = EXECUTION_PAT.captures(&pointcut_expr) {
            meta_patch.insert("pointcut_execution".into(), json!(m[2].to_string()));
        } else {
            meta_patch.insert("pointcut_raw".into(), json!(pointcut_expr));
        }
        for entity in entities.iter_mut() {
            if entity.id == *method_id {
                let mut meta = match entity.metadata.clone() {
                    serde_json::Value::Object(map) => map,
                    _ => serde_json::Map::new(),
                };
                meta.extend(meta_patch);
                entity.metadata = serde_json::Value::Object(meta);
                break;
            }
        }
    }
}

/// 反射字面量(P1-3):`Class.forName("FQN")` 提取,存所在方法的 metadata.reflects =
/// [FQN 数组],由 resolve_cross_stack 匹配类实体建 ReflectsTo 边。参数为标识符时查同文件
/// `static final String` 常量表做一级传播(跨文件/多级不传播——推断边宁缺毋滥)。
fn extract_reflection(
    masked: &MaskedSource,
    method_spans: &[(usize, EntityId)],
    entities: &mut [Entity],
) {
    let consts: HashMap<&str, &str> = CONST_STRING
        .captures_iter(&masked.code)
        .map(|c| (c.get(1).unwrap().as_str(), c.get(2).unwrap().as_str()))
        .collect();
    let mut by_method: HashMap<EntityId, Vec<String>> = HashMap::new();
    for capture in FOR_NAME.captures_iter(&masked.code) {
        let fqn = match (capture.get(1), capture.get(2)) {
            (Some(lit), _) => Some(lit.as_str().to_string()),
            (_, Some(ident)) => consts.get(ident.as_str()).map(|s| s.to_string()),
            _ => None,
        };
        let Some(fqn) = fqn else {
            continue;
        };
        // 所在方法:声明 offset <= 调用 offset 的最近一个(调用在方法体内)。
        let Some((_, method_id)) = method_spans
            .iter()
            .filter(|(offset, _)| *offset <= capture.get(0).unwrap().start())
            .max_by_key(|(offset, _)| *offset)
        else {
            continue;
        };
        by_method.entry(method_id.clone()).or_default().push(fqn);
    }
    for (method_id, mut fqns) in by_method {
        // 同方法内去重后回填(同 FQN 多次 forName 只记一条)。
        fqns.sort();
        fqns.dedup();
        for entity in entities.iter_mut() {
            if entity.id == method_id {
                let mut meta = match entity.metadata.clone() {
                    serde_json::Value::Object(map) => map,
                    _ => serde_json::Map::new(),
                };
                meta.insert("reflects".into(), json!(fqns));
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
        "class_declaration"
        | "interface_declaration"
        | "record_declaration"
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
                        let Some(body) = node.named_child(bi) else {
                            continue;
                        };
                        for ci in 0..body.named_child_count() {
                            let Some(member) = body.named_child(ci) else {
                                continue;
                            };
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
            visit_spring(
                child,
                source,
                file,
                path,
                entities,
                edges,
                signals,
                injected_fields,
            );
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
                // 限定名(com.foo.Bar.stat() 的 receiver com.foo.Bar):取末段标识符作类型名
                // 候选,供 P1 静态调用按短名解析(歧义防护在 analysis 层)。
                (_, Some(f)) => ("qualified", Some(node_text(source, f))),
                _ => ("qualified", None),
            }
        }
        "method_invocation" => ("chain", None),
        "object_creation_expression" => ("new", None),
        _ => ("bare", None),
    }
}

/// 单函数复杂度统计(对标 codebase-memory Method 节点的复杂度属性)。
/// 全部从方法体 AST 静态计算,单遍递归,无副作用。
#[derive(Default, Debug, Clone)]
struct ComplexityStats {
    /// 圈复杂度(cyclomatic):决策点数,基线 1。if/for/while/do/switch/catch/ternary/&&/|| 各 +1。
    complexity: u32,
    /// 循环语句总数(for / enhanced-for / while / do)。
    loop_count: u32,
    /// 方法体内最深的循环嵌套层数(自身循环 = 1 层)。
    loop_depth: u32,
    /// 循环内的线性扫描调用数(contains/indexOf/find/containsKey 等,潜在 O(n²))。
    linear_scan_in_loop: u32,
}

impl ComplexityStats {
    /// 从方法体 block 节点统计。无方法体时调用方应改用 `Default`(complexity=0)。
    fn for_body(body: Node<'_>, source: &[u8]) -> Self {
        let mut stats = Self {
            complexity: 1,
            ..Default::default()
        };
        walk_complexity(body, source, &mut stats, 0);
        stats
    }
}

/// 递归统计方法体子树。`loop_nesting` = 当前所处循环嵌套深度(0 = 不在循环内)。
fn walk_complexity(node: Node<'_>, source: &[u8], stats: &mut ComplexityStats, loop_nesting: u32) {
    let kind = node.kind();
    let mut child_loop_nesting = loop_nesting;
    match kind {
        "if_statement"
        | "for_statement"
        | "enhanced_for_statement"
        | "while_statement"
        | "do_statement"
        | "switch_expression"
        | "switch_statement"
        | "catch_clause"
        | "ternary_expression" => stats.complexity += 1,
        // && / ||:tree-sitter-java 的 binary_expression,文本含操作符即一个布尔决策点。
        "binary_expression" => {
            let text = node_text(source, node);
            if text.contains("&&") || text.contains("||") {
                stats.complexity += 1;
            }
        }
        _ => {}
    }
    if matches!(
        kind,
        "for_statement" | "enhanced_for_statement" | "while_statement" | "do_statement"
    ) {
        stats.loop_count += 1;
        child_loop_nesting = loop_nesting + 1;
        if child_loop_nesting > stats.loop_depth {
            stats.loop_depth = child_loop_nesting;
        }
    }
    // 循环内的线性扫描调用(集合/串查找,循环内即潜在 O(n²))。
    if loop_nesting > 0
        && kind == "method_invocation"
        && let Some(name_node) = node.child_by_field_name("name")
    {
        let callee = node_text(source, name_node);
        if is_linear_scan(&callee) {
            stats.linear_scan_in_loop += 1;
        }
    }
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i) {
            walk_complexity(child, source, stats, child_loop_nesting);
        }
    }
}

/// 集合/串线性扫描方法名。循环内出现即潜在 O(n²);equals 不含(常量比较居多)。
fn is_linear_scan(name: &str) -> bool {
    matches!(
        name,
        "contains" | "indexOf" | "lastIndexOf" | "find" | "containsKey" | "containsValue"
    )
}

/// 异常流引用:method throws/throw/catch 的异常类型名(analysis 后处理按名 resolve 到 class 实体)。
#[derive(Clone)]
struct ExceptionRef {
    method: String,
    type_name: String,
    line: u32,
    kind: String, // "throws" / "raise" / "handles"
}

/// 取类型节点的简单名(Exception / BusinessException),去 scoped/generic 后缀。
fn simple_type_name(node: &Node, source: &[u8]) -> Option<String> {
    if !matches!(
        node.kind(),
        "type_identifier" | "scoped_type_identifier" | "generic_type"
    ) {
        return None;
    }
    let text = node_text(source, *node);
    Some(
        text.split('<')
            .next()
            .unwrap_or("")
            .split('.')
            .next_back()
            .unwrap_or("")
            .to_string(),
    )
}

/// 找节点子树第一个类型名(供 catch_clause 定位异常类型)。
fn first_type_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    if let Some(n) = simple_type_name(&node, source) {
        return Some(n);
    }
    for i in 0..node.named_child_count() {
        if let Some(c) = node.named_child(i)
            && let Some(n) = first_type_name(c, source)
        {
            return Some(n);
        }
    }
    None
}

/// 遍历收集异常流引用:method_declaration 的 throws 子句 → throws;
/// 方法体内 `throw new X(...)` 语句 → raise(仅 new 形态,抛变量/返回值不收);
/// catch_clause → handles。current_method 由 method 声明下推。
fn walk_exceptions(
    node: Node<'_>,
    source: &[u8],
    current_method: Option<&str>,
    refs: &mut Vec<ExceptionRef>,
    file_content: &str,
) {
    if matches!(
        node.kind(),
        "method_declaration" | "constructor_declaration"
    ) && let Some(name_node) = node.child_by_field_name("name")
    {
        let mname = node_text(source, name_node);
        for i in 0..node.named_child_count() {
            if let Some(child) = node.named_child(i) {
                // tree-sitter-java 的 throws 子句节点 kind 是 `throws`(非 throws_clause)。
                if child.kind() == "throws" {
                    for j in 0..child.named_child_count() {
                        if let Some(t) = child.named_child(j)
                            && let Some(tn) = simple_type_name(&t, source)
                        {
                            refs.push(ExceptionRef {
                                method: mname.clone(),
                                type_name: tn,
                                line: line_of(file_content, t.start_byte()),
                                kind: "throws".into(),
                            });
                        }
                    }
                } else {
                    walk_exceptions(child, source, Some(&mname), refs, file_content);
                }
            }
        }
        return;
    }
    if node.kind() == "catch_clause"
        && let Some(m) = current_method
        && let Some(tn) = first_type_name(node, source)
    {
        refs.push(ExceptionRef {
            method: m.to_string(),
            type_name: tn,
            line: line_of(file_content, node.start_byte()),
            kind: "handles".into(),
        });
    }
    if node.kind() == "throw_statement"
        && let Some(m) = current_method
        && let Some(expr) = node.named_child(0)
        && expr.kind() == "object_creation_expression"
        && let Some(tn) = first_type_name(expr, source)
    {
        refs.push(ExceptionRef {
            method: m.to_string(),
            type_name: tn,
            line: line_of(file_content, node.start_byte()),
            kind: "raise".into(),
        });
    }
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i) {
            walk_exceptions(child, source, current_method, refs, file_content);
        }
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
        // 单函数复杂度(对标 codebase-memory Method 属性):从方法体 AST 静态计算。
        // abstract/interface/native 无 body → complexity=0,标记"无可分析体",不参与热点排序。
        let complexity = node
            .child_by_field_name("body")
            .map(|body| ComplexityStats::for_body(body, source))
            .unwrap_or_default();
        let entity = Entity::new(
            EntityId::stable(
                "workspace",
                path,
                EntityKind::Method,
                &name,
                &format!("arity:{arity}"),
            ),
            EntityKind::Method,
            &name,
            format!("{path}#{name}"),
        )
        .with_evidence(
            path,
            line,
            line,
            EvidenceClass::Fact,
            1.0,
            "Java method declaration",
        )
        .with_metadata(json!({
            "body_end_line": body_end_line,
            "arity": arity,
            "complexity": complexity.complexity,
            "loop_count": complexity.loop_count,
            "loop_depth": complexity.loop_depth,
            "linear_scan_in_loop": complexity.linear_scan_in_loop,
        }));
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
        && let (Some(caller), Some(name_node)) = (current_method, node.child_by_field_name("name"))
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
        EdgeKind::Injects => "constructor/field injection (@Autowired/@Resource/Lombok)",
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

    /// ruoyi 真实形态 fixture:@annotation(参数名) pointcut + advice 方法参数消解。
    /// LogAspect.java:65 的写法(@Around(value = "@annotation(controllerLog)"))。
    #[test]
    fn aspect_annotation_pointcut_resolves_param_name_to_type() {
        let src = r#"
@Aspect
@Component
public class LogAspect {
    @Around(value = "@annotation(controllerLog)")
    public void doAround(JoinPoint joinPoint, ControllerLog controllerLog) throws Throwable {
    }
}
"#;
        let path = "src/main/java/org/dromara/log/aspect/LogAspect.java";
        let masked = mask_java(src);
        let id = EntityId::stable(
            "workspace",
            path,
            EntityKind::Method,
            "doAround",
            "",
        );
        let mut entities = vec![Entity::new(id.clone(), EntityKind::Method, "doAround", "doAround")];
        let method_spans = vec![(src.find("doAround").unwrap(), id)];
        extract_aspects(&masked, &method_spans, &mut entities, &mut Vec::new());
        let meta = &entities[0].metadata;
        assert_eq!(
            meta.get("pointcut_annotation").and_then(|v| v.as_str()),
            Some("ControllerLog"),
            "参数名 controllerLog 应消解为注解类型 ControllerLog: {meta}"
        );
        assert_eq!(meta.get("aspect_advice"), Some(&json!(true)));
    }

    /// execution 通配 + @Pointcut 方法引用两形态。旧实现(无通配全签名)在这两种
    /// 写法上都是 0 边。
    #[test]
    fn aspect_extracts_execution_wildcard_and_pointcut_ref() {
        let src = r#"
public class DataScopeAspect {
    @Pointcut("@annotation(dataScope)")
    public void dataScopePoint() {}

    @Before("dataScopePoint()")
    public void doBefore(JoinPoint point, DataScope dataScope) {}

    @Around("execution(* com.ruoyi..*Service.add*(..))")
    public Object aroundAll(ProceedingJoinPoint point) { return null; }
}
"#;
        let path = "src/main/java/org/dromara/aspect/DataScopeAspect.java";
        let masked = mask_java(src);
        let mk = |name: &str| {
            Entity::new(
                EntityId::stable("workspace", path, EntityKind::Method, name, ""),
                EntityKind::Method,
                name,
                name,
            )
        };
        let mut entities = vec![mk("dataScopePoint"), mk("doBefore"), mk("aroundAll")];
        let method_spans: Vec<(usize, EntityId)> = ["dataScopePoint", "doBefore", "aroundAll"]
            .iter()
            .map(|n| (src.find(n).unwrap(), {
                EntityId::stable("workspace", path, EntityKind::Method, n, "")
            }))
            .collect();
        extract_aspects(&masked, &method_spans, &mut entities, &mut Vec::new());
        let meta = |name: &str| {
            entities
                .iter()
                .find(|e| e.name == name)
                .unwrap()
                .metadata
                .clone()
        };
        // @Pointcut 引用解引用到 @annotation(参数名) → 消解类型
        assert_eq!(
            meta("doBefore").get("pointcut_annotation").and_then(|v| v.as_str()),
            Some("DataScope"),
            "{:?}",
            meta("doBefore")
        );
        // execution 通配原文保留(匹配在 analysis 层)
        assert_eq!(
            meta("aroundAll").get("pointcut_execution").and_then(|v| v.as_str()),
            Some("com.ruoyi..*Service.add*"),
            "{:?}",
            meta("aroundAll")
        );
    }

    /// 反射字面量 + 同文件常量一级传播;同方法去重。
    #[test]
    fn reflection_collects_literal_and_constant_forname() {
        let src = r#"
public class AnnotationUtils {
    static final String CONST_FOO = "a.b.Constant";

    public Object resolve(String name) throws Exception {
        Class<?> direct = Class.forName("a.b.Direct");
        Class<?> viaConst = Class.forName("a.b.Direct");
        return Class.forName(CONST_FOO);
    }
}
"#;
        let path = "src/main/java/org/dromara/AnnotationUtils.java";
        let masked = mask_java(src);
        let id = EntityId::stable("workspace", path, EntityKind::Method, "resolve", "");
        let mut entities = vec![Entity::new(id.clone(), EntityKind::Method, "resolve", "resolve")];
        let method_spans = vec![(src.find("resolve").unwrap(), id)];
        extract_reflection(&masked, &method_spans, &mut entities);
        let reflects = entities[0]
            .metadata
            .get("reflects")
            .and_then(|v| v.as_array())
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(reflects, vec!["a.b.Constant", "a.b.Direct"], "常量解引用 + 字面量 + 去重");
    }

    // ---- 类型头平衡扫描器(scan_type_headers):mes-activity 反馈的四类正则缺陷 ----

    /// 嵌套泛型逗号泄漏(旧正则 split(',') 切进泛型、split('<') 把类型实参基名当接口):
    /// `implements Map<String, Handler<X>>` 曾产出幻觉接口 ["Map","Handler"]。
    #[test]
    fn scanner_nested_generic_args_not_treated_as_interfaces() {
        let headers = scan_type_headers(
            "class DataFieldConstants implements Map<String, PfHandler<Req>> {\n}",
        );
        assert_eq!(headers.len(), 1);
        assert_eq!(
            headers[0].implements,
            vec![("Map".to_string(), "Map".to_string())],
            "泛型实参基名不得进 implements"
        );
    }

    /// FQCN 接口合法保留(旧正则整条丢弃),simple/full 平行。
    #[test]
    fn scanner_fqcn_interfaces_kept_whole() {
        let headers = scan_type_headers(
            "class Svc implements com.acme.ISvc, LocalIface<Gen> {\n}",
        );
        assert_eq!(
            headers[0].implements,
            vec![
                ("ISvc".to_string(), "com.acme.ISvc".to_string()),
                ("LocalIface".to_string(), "LocalIface".to_string()),
            ]
        );
    }

    /// bounded type parameter 的 extends 不吃掉顶层 extends(旧正则 superclass 变
    /// "Comparable");FQCN 超类不截断(旧正则得 "com")。
    #[test]
    fn scanner_bounded_type_param_and_fqcn_superclass() {
        let headers = scan_type_headers(
            "class Foo<T extends Comparable<T>> extends com.acme.Base<T> implements Handler {\n}",
        );
        assert_eq!(
            headers[0].superclass,
            Some(("Base".to_string(), "com.acme.Base".to_string())),
            "只认顶层 extends,超类全名保留"
        );
        assert_eq!(headers[0].implements.len(), 1);
    }

    /// interface 的 extends 是多继承,不写 superclass;enum 的 implements 有效。
    #[test]
    fn scanner_interface_extends_ignored_enum_implements_kept() {
        let ifaces = scan_type_headers("interface BaseMapper<T> extends Mapper<T> {\n}");
        assert_eq!(ifaces[0].superclass, None, "interface 多继承不采 superclass");
        let enums = scan_type_headers("enum Color implements Named {\n  RED, GREEN;\n}");
        assert_eq!(enums[0].implements, vec![("Named".to_string(), "Named".to_string())]);
    }

    /// sealed permits 子句不属于 implements;同文件两个类不互相偷取(旧正则
    /// `[^{]*?` 跨 brace-free 区让前一个类偷走后一个类的 implements)。
    #[test]
    fn scanner_permits_stops_and_no_cross_class_theft() {
        let headers = scan_type_headers(
            "class Foo implements I1 permits S {\n}\nclass Bar {\n}\nclass Baz implements I2 {\n}",
        );
        assert_eq!(headers.len(), 3);
        assert_eq!(headers[0].implements, vec![("I1".to_string(), "I1".to_string())]);
        assert!(headers[1].implements.is_empty(), "Bar 无 implements,不得偷取 Baz 的");
        assert_eq!(headers[2].implements, vec![("I2".to_string(), "I2".to_string())]);
    }

    /// 全链路:enum 产实体(kind=Enum)、implements/abstract 按 offset 精确绑定到
    /// 同文件的正确类(同名内部类不串)。
    #[test]
    fn extract_binds_implements_by_offset_not_name() {
        let src = r#"
public class Outer {
    class Handler implements IInner {
    }
}
enum Color implements Named {
    RED
}
public abstract class Base implements TopIface {
}
"#;
        let path = "src/main/java/com/a/Outer.java";
        let file = SourceFile {
            id: EntityId::stable("workspace", path, EntityKind::File, "Outer", ""),
            relative_path: std::path::PathBuf::from(path),
            kind: FileKind::Java,
            content_hash: String::new(),
            content: src.into(),
        };
        let config = SemanticsConfig::default();
        let mut entities = Vec::new();
        let mut edges = Vec::new();
        extract_java(&file, path, &mut entities, &mut edges, &config).unwrap();
        let meta = |kind: EntityKind, name: &str| {
            entities
                .iter()
                .find(|e| e.kind == kind && e.name == name)
                .unwrap_or_else(|| panic!("实体缺失 {kind:?} {name}"))
                .metadata
                .clone()
        };
        // enum 产实体 + implements
        let color = meta(EntityKind::Enum, "Color");
        assert_eq!(
            color.get("implements").and_then(|v| v.as_array()).map(|a| a.len()),
            Some(1),
            "enum 实体应产且带 implements: {color}"
        );
        // 同文件内部类 Handler 与 enum/其他类按 offset 绑定,不串
        let handler = meta(EntityKind::Class, "Handler");
        assert_eq!(
            handler
                .get("implements")
                .and_then(|v| v.as_array())
                .and_then(|a| a[0].as_str()),
            Some("IInner")
        );
        let base = meta(EntityKind::Class, "Base");
        assert_eq!(base.get("abstract"), Some(&json!(true)));
        assert_eq!(
            base.get("superclass_full").is_none(),
            true,
            "无 extends 时不得有 superclass_full"
        );
    }
}
