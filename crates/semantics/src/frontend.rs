//! 前端提取:Vue 单文件组件页、`a.b` 属性引用(FrontendField)、HTTP 调用。
//! 噪声词(JS 内建方法)从 `SemanticsConfig` 读,默认 builtin ~70 个。

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use anyhow::Result;
use regex::Regex;
use repo_intelligence_config::SemanticsConfig;
use repo_intelligence_model::{Edge, EdgeKind, Entity, EntityId, EntityKind, EvidenceClass};
use repo_intelligence_source::{FileKind, SourceFile};
use serde_json::json;

use crate::registry::{ExtractContext, SemanticExtractor};
use crate::{add_contained, line_of, normalize_path};

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
// 常量 URL 定义:const/let/var NAME = '/x' → 建 name→url 映射(P0-1c)。仅取"像 URL"
// 的值(以 / 或 http 开头),避免把 const name="hello" 当 URL。
static CONST_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:const|let|var)\s+([A-Za-z_$]\w*)\s*=\s*["'`]([^"'`]+)["'`]"#).unwrap()
});
// 变量形式的 HTTP 调用:get(VAR) —— VAR 须命中 CONST_URL 映射才认,过滤 map.get(key)
// 这类同名非 HTTP 调用。
static HTTP_CALL_VAR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)\b(?:[\w$]*(?:\.[\w$]+|\[[^\]]+\])*\.)?(get|post|put|delete|patch)\(\s*([A-Za-z_$]\w*)\s*\)"#,
    )
    .unwrap()
});
// 对象参数形式:request({ url: '/x', method: 'get' })(plus-ui/vue-element-admin 主流封装)。
// 只负责找 url:,谓词部分(动词缺省 GET)在提取循环里对该调用点的局部窗口二次匹配,
// 避免"method 出现在 url 前"或中间隔字段时一条正则抓不全。
static OBJECT_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\b[\w$][\w$.]*\(\s*\{[^{}]{0,400}?url\s*:\s*["'`]([^"'`]+)["'`]"#).unwrap()
});
static METHOD_IN_WINDOW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\bmethod\s*:\s*["'`](get|post|put|delete|patch)["'`]"#).unwrap()
});

// TS 符号声明(P0,vue/ts 栈 TS 符号层)。行锚定:前端代码经 prettier/eslint 格式化,
// 声明起于行首缩进。覆盖两种函数形态(plus-ui 的 api 层全是箭头形态):
//   export function foo(a: A): B {          / function foo(a) {
//   export const foo = (a: A): B =>         / const foo = async (a) => {
// 非导出函数也提(exported=false):SFC script setup 的页面内函数、utils 内部函数
// 是文件内调用图的真实节点。
static TS_FUNCTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?m)^[ \t]*(?:export\s+)?(?:default\s+)?(?:async\s+)?(?:function\s+([A-Za-z_$][\w$]*)\s*\(|const\s+([A-Za-z_$][\w$]*)\s*(?::[^=\n]+)?=[^=\n]*=>)",
    )
    .unwrap()
});
// 返回类型:箭头形态 `): RET =>`,声明形态 `): RET {`。缺失(推断返回)为 None。
static TS_RETURN_ARROW: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\)\s*:\s*([^=\n]+?)\s*=>").unwrap());
static TS_RETURN_DECL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\)\s*:\s*([^\{\n]+?)\s*\{").unwrap());
static TS_INTERFACE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^[ \t]*(?:export\s+)?(?:declare\s+)?interface\s+([A-Za-z_$][\w$]*)").unwrap()
});
static TS_ENUM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^[ \t]*(?:export\s+)?(?:declare\s+)?(?:const\s+)?enum\s+([A-Za-z_$][\w$]*)")
        .unwrap()
});
static TS_TYPE_ALIAS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^[ \t]*(?:export\s+)?(?:declare\s+)?type\s+([A-Za-z_$][\w$]*)\s*=").unwrap()
});
// interface 成员:体内行首 `name:`(含 readonly/可选 ?);enum 成员:体内行首 `name,`/`name =`。
// 逐行近似——方法签名 `foo(a: A): B;` 的 foo 也命中(同为成员,语义正确)。
static IFACE_MEMBER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^[ \t]*(?:readonly[ \t]+)?([A-Za-z_$][\w$]*)\??[ \t]*:").unwrap()
});
static ENUM_MEMBER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^[ \t]*([A-Za-z_$][\w$]*)[ \t]*[,=]").unwrap());
// TS import 别名表:route 的 `component: Layout` 标识符靠它换回 import spec;.vue 结尾的
// spec 同时是组件引用(component_ref 边的原料)。
static ALIAS_IMPORT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^[ \t]*import\s+([A-Za-z_$][\w$]*)\s+from\s+["'`]([^"'`]+)["'`]"#).unwrap()
});
// vue-router 路由项(P1):path + 窗口内 component(两种形态)/name。
// 仅对含 RouteRecordRaw / vue-router 的文件启用(守卫);path 不锚行首——紧凑单行
// 风格(`{ path: '/user', component: ... }`)同样命中。
static ROUTE_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"\bpath\s*:\s*["']([^"']*)["']"#).unwrap());
static ROUTE_COMPONENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"component\s*:\s*(?:\(\)\s*=>\s*import\(\s*["'`]([^"'`]+)["'`]|([A-Za-z_$][\w$]*))"#,
    )
    .unwrap()
});
static ROUTE_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?m)^[ \t]*name\s*:\s*["']([A-Za-z_$][\w$]*)["']"#).unwrap());
/// route 实体的 component 窗口上限(path 行到下一个 path 行之间;再兜底 800B)。
const ROUTE_WINDOW_CAP: usize = 800;
/// interface/enum members 清单上限:防御巨型类型定义撑爆实体 json。
const MEMBERS_CAP: usize = 64;
// 调用点扫描:文件内调用边与 vue_page call_sites 共用。成员调用 `obj.method(` 会把
// method 抓为候选名——文件内边只认"名字 ∈ 本文件函数集合"自然过滤;call_sites 进
// metadata 后由 resolve 层对全图 function 索引过滤,均无误连,只多存几条记录。
static CALL_SITE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b([A-Za-z_$][\w$]*)\s*\(").unwrap());
// 语句关键字:出现在调用名位置必非函数调用。
const CALL_KEYWORDS: &[&str] = &[
    "if",
    "for",
    "while",
    "switch",
    "catch",
    "return",
    "new",
    "function",
    "typeof",
    "await",
    "async",
    "constructor",
    "super",
    "import",
    "export",
    "throw",
    "try",
    "else",
    "do",
    "delete",
    "void",
    "yield",
    "case",
];
/// vue_page metadata.call_sites 上限:防御极端巨型 SFC 撑爆实体 json。
const CALL_SITES_CAP: usize = 400;

pub struct FrontendExtractor;

impl SemanticExtractor for FrontendExtractor {
    fn supports(&self, kind: FileKind) -> bool {
        matches!(
            kind,
            FileKind::Vue | FileKind::JavaScript | FileKind::TypeScript
        )
    }

    fn extract(
        &self,
        ctx: &ExtractContext,
        file: &SourceFile,
        path: &str,
        entities: &mut Vec<Entity>,
        edges: &mut Vec<Edge>,
    ) -> Result<()> {
        extract_frontend(file, path, entities, edges, ctx.config);
        Ok(())
    }
}

/// 下取整到最近的 UTF-8 字符边界(Rust stable 无 floor_char_boundary)。
/// 正则给出的偏移本应是边界,此处对 +200 的窗口截断点兜底,防多字节字符劈半 panic。
fn floor_boundary(content: &str, mut idx: usize) -> usize {
    while idx > 0 && !content.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// 判断前端属性访问 `a.b` 的 b 是否「像业务字段」而非工具方法/常量。
fn is_likely_field(name: &str, noise: &HashSet<String>) -> bool {
    if noise.contains(name) {
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
    config: &SemanticsConfig,
) {
    let noise: HashSet<String> = config.effective_frontend_noise().into_iter().collect();
    // TS 符号先提:文件内调用边的 target 集合与 frontend_field 去重名单都来自这里。
    let (fns, ts_names) = extract_ts_symbols(file, path, entities);
    // import 别名表:name → spec。route 的 `component: Layout` 标识符换 spec 用;
    // .vue 结尾的 spec 同时是组件引用清单的原料。
    let imports: HashMap<&str, &str> = ALIAS_IMPORT
        .captures_iter(&file.content)
        .filter_map(|c| Some((c.get(1)?.as_str(), c.get(2)?.as_str())))
        .collect();
    if file.kind == FileKind::Vue {
        let name = file
            .relative_path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or(path);
        // call_sites:页面内的函数调用点清单(name+line),resolve 层对全图 ts function
        // 索引做唯一名匹配建跨文件边(vue_page -[calls]-> api function)。噪音词过滤。
        let mut call_sites = Vec::new();
        for capture in CALL_SITE.captures_iter(&file.content) {
            let name = capture.get(1).unwrap().as_str();
            if CALL_KEYWORDS.contains(&name) || noise.contains(name) {
                continue;
            }
            let line = line_of(&file.content, capture.get(1).unwrap().start());
            call_sites.push(json!({"name": name, "line": line}));
            if call_sites.len() >= CALL_SITES_CAP {
                break;
            }
        }
        // component_refs:本页 import 的 .vue 组件清单(spec 原样),resolve 层归一
        // 路径后建 component_ref 边。import 语句是事实,比同名弱近似(mapped_from)可靠。
        let component_refs: Vec<serde_json::Value> = ALIAS_IMPORT
            .captures_iter(&file.content)
            .filter_map(|c| {
                let name = c.get(1)?.as_str();
                let spec = c.get(2)?.as_str();
                let line = line_of(&file.content, c.get(1)?.start());
                spec.ends_with(".vue")
                    .then(|| json!({"name": name, "spec": spec, "line": line}))
            })
            .take(CALL_SITES_CAP)
            .collect();
        let page = Entity::new(
            EntityId::stable("workspace", path, EntityKind::VuePage, name, ""),
            EntityKind::VuePage,
            name,
            path,
        )
        .with_metadata(json!({"call_sites": call_sites, "component_refs": component_refs}))
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
    // vue-router 路由项(守卫在函数内:仅 RouteRecordRaw / vue-router 文件)。
    extract_routes(file, path, &imports, entities);
    if !fns.is_empty() {
        extract_in_file_calls(file, path, &fns, &noise, edges);
    }
    for capture in VUE_BINDING.captures_iter(&file.content) {
        let name = capture.get(1).unwrap();
        if !is_likely_field(name.as_str(), &noise) {
            continue;
        }
        // 去重:同文件已有同名 TS 符号声明(function/interface/enum/type_alias)时,
        // 属性引用不再另建 frontend_field——否则 .vue 内 `settings.theme` 与
        // `const theme = ...` 各产一条同名实体(plus-ui 实测 20 例双份)。
        if ts_names.contains(name.as_str()) {
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
    // 同文件内常量 URL 引用(P0-1c):先建 name→url 映射,再认 get(VAR) 形式的调用,
    // 召回封装在常量里的 URL(字面量正则捕获不到变量)。
    let const_urls: HashMap<String, String> = CONST_URL
        .captures_iter(&file.content)
        .filter_map(|capture| {
            let name = capture.get(1)?.as_str().to_string();
            let value = capture.get(2)?.as_str();
            let looks_url = value.starts_with('/') || value.to_lowercase().starts_with("http");
            looks_url.then(|| (name, normalize_path(value)))
        })
        .collect();
    for capture in HTTP_CALL_VAR.captures_iter(&file.content) {
        let matched = capture.get(0).unwrap();
        let method = capture[1].to_uppercase();
        let var = capture.get(2).unwrap().as_str();
        let Some(url) = const_urls.get(var) else {
            continue;
        };
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
            EvidenceClass::Inferred,
            0.7,
            "frontend HTTP call via constant URL",
        );
        add_contained(file, path, call, line, entities, edges);
    }
    // 对象参数形式(P0①):url 为字面量(Fact 0.9),method 在调用点窗口内找,缺省 GET。
    for capture in OBJECT_URL.captures_iter(&file.content) {
        let matched = capture.get(0).unwrap();
        let window_end =
            floor_boundary(&file.content, (matched.end() + 200).min(file.content.len()));
        let window = &file.content[floor_boundary(&file.content, matched.end())..window_end];
        let verb = METHOD_IN_WINDOW
            .captures(window)
            .map(|m| m[1].to_uppercase())
            .unwrap_or_else(|| "GET".into());
        let url = normalize_path(&capture[1]);
        let name = format!("{verb} {url}");
        let line = line_of(&file.content, matched.start());
        let call = Entity::new(
            EntityId::stable("workspace", path, EntityKind::HttpClientCall, &name, ""),
            EntityKind::HttpClientCall,
            &name,
            format!("{path}#{name}"),
        )
        .with_metadata(json!({"method": verb, "path": url}))
        .with_evidence(
            path,
            line,
            line,
            EvidenceClass::Fact,
            0.9,
            "frontend HTTP call (object-literal URL)",
        );
        add_contained(file, path, call, line, entities, edges);
    }
}

/// TS 符号提取:function(新 kind)/interface(复用)/enum/type_alias 实体。
/// 返回 `(函数声明 (name, decl_line, id), 全部符号名集合)`——前者供文件内调用归属,
/// 后者供 frontend_field 去重(同文件同名声明存在时属性引用不再建弱实体)。
/// 签名/成员 metadata 尽力而为:prettier 保证声明首行含完整签名,跨行签名丢参数/
/// 返回类型但不丢实体。interface/enum 另锚 `body_end_line`(配对 `}` 行)与成员清单。
fn extract_ts_symbols(
    file: &SourceFile,
    path: &str,
    entities: &mut Vec<Entity>,
) -> (Vec<(String, u32, EntityId)>, HashSet<String>) {
    let mut fns = Vec::new();
    let mut all_names = HashSet::new();
    for capture in TS_FUNCTION.captures_iter(&file.content) {
        let matched = capture.get(0).unwrap();
        let name = capture.get(1).or_else(|| capture.get(2)).unwrap().as_str();
        let line = line_of(&file.content, matched.start());
        let exported = matched.as_str().trim_start().starts_with("export");
        // 行内签名细节(args/return_type);首个括号即参数列表(格式化保证的常见情形)。
        let line_text = line_text(&file.content, matched.start());
        let args = LINE_ARGS.captures(line_text).map(|c| c[1].to_string());
        let ret = TS_RETURN_ARROW
            .captures(line_text)
            .or_else(|| TS_RETURN_DECL.captures(line_text))
            .map(|c| c[1].trim().to_string());
        let mut metadata = json!({"exported": exported});
        if let Some(args) = args {
            metadata["signature"] = json!(args);
        }
        if let Some(ret) = ret {
            metadata["return_type"] = json!(ret);
        }
        // 符号体末行(配对 `}` 所在行;单行函数 = 声明行)。对标 cb 的符号行范围。
        if let Some((_, body_close)) = brace_pair(&file.content, matched.start()) {
            metadata["body_end_line"] = json!(line_of(&file.content, body_close));
        }
        let entity = Entity::new(
            EntityId::stable("workspace", path, EntityKind::Function, name, ""),
            EntityKind::Function,
            name,
            format!("{path}#{name}"),
        )
        .with_metadata(metadata)
        .with_evidence(
            path,
            line,
            line,
            EvidenceClass::Fact,
            1.0,
            "TS function declaration",
        );
        fns.push((name.to_string(), line, entity.id.clone()));
        all_names.insert(name.to_string());
        entities.push(entity);
    }
    // interface/enum:带体符号——补 body_end_line + members;type_alias 无体,单行。
    let mut typed = |regex: &Regex, kind, reason: &str, members_of: fn(&str) -> Vec<String>| {
        for capture in regex.captures_iter(&file.content) {
            let matched = capture.get(0).unwrap();
            let name = capture.get(1).unwrap().as_str();
            let line = line_of(&file.content, matched.start());
            let exported = matched.as_str().trim_start().starts_with("export");
            let mut metadata = json!({"exported": exported});
            if let Some((body_open, body_close)) = brace_pair(&file.content, matched.start()) {
                metadata["body_end_line"] = json!(line_of(&file.content, body_close));
                let body = &file.content[body_open..=body_close];
                let members = members_of(body);
                if !members.is_empty() {
                    metadata["members"] = json!(members);
                }
            }
            entities.push(
                Entity::new(
                    EntityId::stable("workspace", path, kind, name, ""),
                    kind,
                    name,
                    format!("{path}#{name}"),
                )
                .with_metadata(metadata)
                .with_evidence(path, line, line, EvidenceClass::Fact, 1.0, reason),
            );
            all_names.insert(name.to_string());
        }
    };
    typed(
        &TS_INTERFACE,
        EntityKind::Interface,
        "TS interface declaration",
        |body| line_members(body, &IFACE_MEMBER),
    );
    typed(&TS_ENUM, EntityKind::Enum, "TS enum declaration", |body| {
        line_members(body, &ENUM_MEMBER)
    });
    typed(
        &TS_TYPE_ALIAS,
        EntityKind::TypeAlias,
        "TS type alias declaration",
        |_| Vec::new(),
    );
    (fns, all_names)
}

/// 体内成员名清单:逐行正则,去重保序,上限 `MEMBERS_CAP`。
fn line_members(body: &str, regex: &Regex) -> Vec<String> {
    let mut members = Vec::new();
    for capture in regex.captures_iter(body) {
        let name = capture.get(1).unwrap().as_str();
        if members.iter().any(|m: &String| m == name) {
            continue;
        }
        members.push(name.to_string());
        if members.len() >= MEMBERS_CAP {
            break;
        }
    }
    members
}

/// 从 `from` 偏移起找第一个 `{` 并返回其配对 `}` 的偏移。跳过字符串字面量
/// ('...'/"..."/`...`,含 `\` 转义;`${}` 内层再嵌反引号的极端形态不识别)与
/// `//`、`/* */` 注释。interface/enum/function 的符号体定位共用。
fn brace_pair(content: &str, from: usize) -> Option<(usize, usize)> {
    let bytes = content.as_bytes();
    let mut i = from;
    let open = loop {
        match bytes.get(i) {
            Some(b'{') => break i,
            Some(b'\'' | b'"' | b'`') => i = skip_string(bytes, i),
            Some(b'/') if bytes.get(i + 1) == Some(&b'/') => i = skip_line_comment(bytes, i),
            Some(b'/') if bytes.get(i + 1) == Some(&b'*') => i = skip_block_comment(bytes, i),
            Some(_) => i += 1,
            None => return None,
        }
    };
    let mut depth = 0usize;
    let mut i = open;
    while let Some(byte) = bytes.get(i) {
        match byte {
            b'\'' | b'"' | b'`' => {
                i = skip_string(bytes, i);
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                i = skip_line_comment(bytes, i);
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i = skip_block_comment(bytes, i);
                continue;
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((open, i));
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// 字符串起点 → 结束引号之后的下标(转义 `\\` 直接跳两字节)。
fn skip_string(bytes: &[u8], start: usize) -> usize {
    let quote = bytes[start];
    let mut i = start + 1;
    while let Some(byte) = bytes.get(i) {
        match byte {
            b'\\' => i += 2,
            b if *b == quote => return i + 1,
            _ => i += 1,
        }
    }
    i
}

fn skip_line_comment(bytes: &[u8], start: usize) -> usize {
    bytes[start..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map(|offset| start + offset)
        .unwrap_or(bytes.len())
}

fn skip_block_comment(bytes: &[u8], start: usize) -> usize {
    bytes[start..]
        .windows(2)
        .position(|window| window == b"*/")
        .map(|offset| start + offset + 2)
        .unwrap_or(bytes.len())
}

/// vue-router 路由项提取(P1)。守卫:文件须含 `RouteRecordRaw` 或 `vue-router`,
/// 防止普通对象字面量的 `path:` 误报。每条 `path:` 在「本对象窗口」(到下一个 `path:`
/// 之前)内找 component(字面量 import() spec 或标识符——后者经 imports 别名表换 spec)
/// 与 name;嵌套 children 的相对 path 按括号深度栈与父 path 拼接(vue-router 语义:
/// 子 path 不以 `/` 开头则拼接父 path)。
fn extract_routes(
    file: &SourceFile,
    path: &str,
    imports: &HashMap<&str, &str>,
    entities: &mut Vec<Entity>,
) {
    // 路由配置只在 .ts/.js;.vue 里的 `router.push({ path: '/x' })` 是导航跳转
    // 不是路由定义,且文件名守卫兜不住(import 了 RouteRecordRaw 类型的 SFC)。
    if file.kind == FileKind::Vue {
        return;
    }
    let is_router_file =
        file.content.contains("RouteRecordRaw") || file.content.contains("vue-router");
    if !is_router_file {
        return;
    }
    // 每个 path: 捕获处的花括号深度:一次线性扫描(跳字符串/注释),同步推进捕获游标。
    let paths: Vec<_> = ROUTE_PATH
        .captures_iter(&file.content)
        .map(|c| {
            (
                c.get(0).unwrap().start(),
                c.get(0).unwrap().end(),
                c[1].to_string(),
            )
        })
        .collect();
    let mut depths = Vec::with_capacity(paths.len());
    let mut cursor = 0usize;
    let mut depth = 0usize;
    let bytes = file.content.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        while cursor < paths.len() && paths[cursor].0 <= i {
            depths.push(depth);
            cursor += 1;
        }
        match bytes[i] {
            b'\'' | b'"' | b'`' => i = skip_string(bytes, i),
            b'/' if bytes.get(i + 1) == Some(&b'/') => i = skip_line_comment(bytes, i),
            b'/' if bytes.get(i + 1) == Some(&b'*') => i = skip_block_comment(bytes, i),
            b'{' => {
                depth += 1;
                i += 1;
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            _ => i += 1,
        }
    }
    while depths.len() < paths.len() {
        depths.push(depth);
    }
    // 相对 path 拼接:栈里存 (深度, 完整 path);弹掉所有 ≥ 自身深度的,剩下的栈顶即父。
    let mut stack: Vec<(usize, String)> = Vec::new();
    for (index, (start, end, raw)) in paths.iter().enumerate() {
        let own_depth = depths[index];
        let keep = stack.iter().take_while(|(d, _)| *d < own_depth).count();
        stack.truncate(keep);
        let full = if raw.starts_with('/') {
            raw.clone()
        } else if raw.is_empty() {
            stack
                .last()
                .map(|(_, parent)| parent.clone())
                .unwrap_or_else(|| "/".to_string())
        } else {
            match stack.last() {
                Some((_, parent)) if parent != "/" => format!("{parent}/{raw}"),
                Some((_, parent)) => format!("{parent}{raw}"),
                None => format!("/{raw}"),
            }
        };
        // 窗口:本 path 行到下一个 path: 之间(兜底 800B),只看本对象的属性行。
        let cap_end = floor_boundary(&file.content, end + ROUTE_WINDOW_CAP);
        let window_end = paths
            .get(index + 1)
            .map(|(next_start, _, _)| (*next_start).min(cap_end))
            .unwrap_or(cap_end);
        let window = &file.content[floor_boundary(&file.content, *end)..window_end];
        let component_spec = ROUTE_COMPONENT.captures(window).and_then(|c| {
            if let Some(spec) = c.get(1) {
                Some(spec.as_str().to_string())
            } else {
                // 标识符形态(component: Layout):别名表换 import spec。
                imports
                    .get(c.get(2)?.as_str())
                    .map(|spec| (*spec).to_string())
            }
        });
        let route_name = ROUTE_NAME.captures(window).map(|c| c[1].to_string());
        let line = line_of(&file.content, *start);
        let mut metadata = json!({"path": full});
        if let Some(spec) = component_spec {
            metadata["component_spec"] = json!(spec);
        }
        if let Some(route_name) = route_name {
            metadata["name"] = json!(route_name);
        }
        entities.push(
            Entity::new(
                EntityId::stable("workspace", path, EntityKind::Route, &full, ""),
                EntityKind::Route,
                &full,
                format!("{path}#{full}"),
            )
            .with_metadata(metadata)
            .with_evidence(
                path,
                line,
                line,
                EvidenceClass::Fact,
                0.9,
                "vue-router route entry",
            ),
        );
        stack.push((own_depth, full));
    }
}

/// 单行文本(声明签名 metadata 的提取窗口)。
fn line_text(content: &str, offset: usize) -> &str {
    let line_start = content[..offset].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line_end = content[offset..]
        .find('\n')
        .map(|i| offset + i)
        .unwrap_or(content.len());
    &content[line_start..line_end]
}

/// 参数列表:行内第一个括号对的内容(prettier 保证签名在首行的常见情形)。
static LINE_ARGS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\(([^)]*)\)").unwrap());

/// 文件内调用边:调用点名字 ∈ 本文件函数集合、非定义行 → Calls 边,归属到
/// 词法包围函数(声明行 < 调用行的最近者;嵌套函数内部调用归最内层声明——
/// 正则无作用域感知,近似,Inferred 0.6 标注启发式本质)。
fn extract_in_file_calls(
    file: &SourceFile,
    path: &str,
    fns: &[(String, u32, EntityId)],
    noise: &HashSet<String>,
    edges: &mut Vec<Edge>,
) {
    let by_name: HashMap<&str, &EntityId> = fns
        .iter()
        .map(|(name, _, id)| (name.as_str(), id))
        .collect();
    let decl_lines: HashSet<u32> = fns.iter().map(|(_, line, _)| *line).collect();
    for capture in CALL_SITE.captures_iter(&file.content) {
        let name = capture.get(1).unwrap();
        if CALL_KEYWORDS.contains(&name.as_str()) || noise.contains(name.as_str()) {
            continue;
        }
        let Some(target) = by_name.get(name.as_str()) else {
            continue;
        };
        let line = line_of(&file.content, name.start());
        if decl_lines.contains(&line) {
            continue;
        }
        // 词法包围:声明行在调用行之前、最近的那一个。
        let Some(source) = fns
            .iter()
            .filter(|(_, decl, _)| *decl < line)
            .max_by_key(|(_, decl, _)| *decl)
        else {
            continue;
        };
        let edge = Edge::new(source.2.clone(), (*target).clone(), EdgeKind::Calls).with_evidence(
            path,
            line,
            line,
            EvidenceClass::Inferred,
            0.6,
            "call-site by name in file (lexical enclosing function)",
        );
        edges.push(edge);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn js_file(name: &str, body: &str) -> SourceFile {
        SourceFile {
            id: EntityId::stable("workspace", name, EntityKind::File, name, ""),
            relative_path: PathBuf::from(name),
            kind: FileKind::JavaScript,
            content_hash: "test".into(),
            content: body.to_string(),
        }
    }

    fn extract_calls(file: &SourceFile) -> Vec<Entity> {
        let config = SemanticsConfig::default();
        let mut entities = Vec::new();
        let mut edges = Vec::new();
        extract_frontend(file, "src/api/user.js", &mut entities, &mut edges, &config);
        entities
            .into_iter()
            .filter(|e| e.kind == EntityKind::HttpClientCall)
            .collect()
    }

    #[test]
    fn object_form_http_call_is_extracted() {
        // plus-ui/vue-element-admin 标准封装形态:对象参数携带 url + method。
        let src = r#"
import request from '@/utils/request'
export function fetchUsers(params) {
  return request({ url: '/api/users/list', method: 'get', params })
}
"#;
        let file = js_file("src/api/user.js", src);
        let calls = extract_calls(&file);
        assert_eq!(calls.len(), 1, "{:#?}", calls);
        assert_eq!(calls[0].name, "GET /api/users/list");
        let ev = calls[0].evidence.first().unwrap();
        assert_eq!(ev.classification, EvidenceClass::Fact);
        assert!((ev.confidence - 0.9).abs() < f32::EPSILON);
    }

    #[test]
    fn object_form_defaults_to_get_when_method_absent() {
        // method 缺省时按 GET 处理(GET 是 axios 默认方法)。
        let src = r#"request({ url: "/a/b" });"#;
        let file = js_file("src/api/user.js", src);
        let calls = extract_calls(&file);
        assert_eq!(calls.len(), 1, "{:#?}", calls);
        assert_eq!(calls[0].name, "GET /a/b");
    }

    fn extract_all(file: &SourceFile) -> (Vec<Entity>, Vec<Edge>) {
        let config = SemanticsConfig::default();
        let mut entities = Vec::new();
        let mut edges = Vec::new();
        extract_frontend(file, "src/x.ts", &mut entities, &mut edges, &config);
        (entities, edges)
    }

    #[test]
    fn ts_arrow_function_with_signature_is_extracted() {
        // plus-ui api 层真实形态:export const + 箭头函数 + 返回类型。
        let src = "export const listUser = (query: UserQuery): AxiosPromise<PageResult<UserVO>> => {\n  return request({ url: '/system/user/list' });\n};\n";
        let file = js_file("src/api/user.ts", src);
        let (entities, _) = extract_all(&file);
        let fns: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Function)
            .collect();
        assert_eq!(fns.len(), 1, "{:#?}", fns);
        assert_eq!(fns[0].name, "listUser");
        assert_eq!(fns[0].qualified_name, "src/x.ts#listUser");
        assert_eq!(
            fns[0].metadata.get("return_type").and_then(|v| v.as_str()),
            Some("AxiosPromise<PageResult<UserVO>>")
        );
        assert_eq!(
            fns[0].metadata.get("exported").and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn ts_function_declaration_and_in_file_call_edge() {
        // export function 形态 + 文件内调用:调用点归属词法包围函数,非定义行。
        let src = "export function parseTime(time: any, pattern?: string) {\n  return format(time);\n}\nfunction format(t: any) {\n  return t;\n}\n";
        let file = js_file("src/utils/ruoyi.ts", src);
        let (entities, edges) = extract_all(&file);
        let names: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Function)
            .map(|e| e.name.as_str())
            .collect();
        assert!(
            names.contains(&"parseTime") && names.contains(&"format"),
            "{names:?}"
        );
        let calls: Vec<_> = edges.iter().filter(|e| e.kind == EdgeKind::Calls).collect();
        assert_eq!(calls.len(), 1, "{calls:#?}");
        let by_id = |id: &EntityId| entities.iter().find(|e| &e.id == id).unwrap();
        assert_eq!(by_id(&calls[0].source).name, "parseTime");
        assert_eq!(by_id(&calls[0].target).name, "format");
        let ev = calls[0].evidence.first().unwrap();
        assert_eq!(ev.classification, EvidenceClass::Inferred);
    }

    #[test]
    fn ts_interface_enum_type_alias_are_extracted() {
        let src = "export interface UserVO {\n  id: number;\n}\nexport const enum Status {\n  On = 1,\n}\nexport type UserId = string | number;\n";
        let file = js_file("src/api/types.ts", src);
        let (entities, _) = extract_all(&file);
        let kinds: Vec<(EntityKind, &str)> = entities
            .iter()
            .filter(|e| {
                matches!(
                    e.kind,
                    EntityKind::Interface | EntityKind::Enum | EntityKind::TypeAlias
                )
            })
            .map(|e| (e.kind, e.name.as_str()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (EntityKind::Interface, "UserVO"),
                (EntityKind::Enum, "Status"),
                (EntityKind::TypeAlias, "UserId")
            ]
        );
    }

    #[test]
    fn vue_page_carries_call_sites_metadata() {
        // SFC 内调用点进 vue_page metadata(供 resolve 层跨文件匹配)。
        let config = SemanticsConfig::default();
        let mut entities = Vec::new();
        let mut edges = Vec::new();
        let mut file = js_file(
            "src/views/user/index.vue",
            "<template>\n<div />\n</template>\n<script setup lang=\"ts\">\nimport { listUser } from '@/api/system/user';\nconst res = await listUser(queryParams);\nconsole.log(res);\n</script>\n",
        );
        file.kind = FileKind::Vue;
        extract_frontend(
            &file,
            "src/views/user/index.vue",
            &mut entities,
            &mut edges,
            &config,
        );
        let page = entities
            .iter()
            .find(|e| e.kind == EntityKind::VuePage)
            .unwrap();
        let sites = page.metadata.get("call_sites").unwrap().as_array().unwrap();
        let names: Vec<&str> = sites
            .iter()
            .map(|s| s.get("name").unwrap().as_str().unwrap())
            .collect();
        assert!(names.contains(&"listUser"), "{names:?}");
        assert!(
            !names.contains(&"log"),
            "noise 方法不应进 call_sites: {names:?}"
        );
    }

    #[test]
    fn ts_interface_carries_members_and_body_end_line() {
        // 符号体粒度(P1):interface 锚完整 body(`}` 行)+ 成员清单,对标 cb。
        let src =
            "export interface UserQuery extends PageQuery {\n  id: number;\n  name?: string;\n}\n";
        let file = js_file("src/api/types.ts", src);
        let (entities, _) = extract_all(&file);
        let iface = entities
            .iter()
            .find(|e| e.kind == EntityKind::Interface)
            .unwrap();
        assert_eq!(
            iface.metadata.get("body_end_line").and_then(|v| v.as_u64()),
            Some(4),
            "{:#?}",
            iface.metadata
        );
        let members: Vec<&str> = iface.metadata["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(members, vec!["id", "name"]);
    }

    #[test]
    fn ts_enum_and_function_body_end_line() {
        // enum 成员 + function 体末行;体内字符串/注释里的花括号不打乱配对。
        let src = "export enum Status {\n  // brace { in comment\n  On = 'x{',\n  Off,\n}\nfunction f() {\n  return Status.On;\n}\n";
        let file = js_file("src/api/types.ts", src);
        let (entities, _) = extract_all(&file);
        let en = entities
            .iter()
            .find(|e| e.kind == EntityKind::Enum)
            .unwrap();
        assert_eq!(
            en.metadata.get("body_end_line").and_then(|v| v.as_u64()),
            Some(5)
        );
        let members: Vec<&str> = en.metadata["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(members, vec!["On", "Off"]);
        let f = entities
            .iter()
            .find(|e| e.kind == EntityKind::Function && e.name == "f")
            .unwrap();
        assert_eq!(
            f.metadata.get("body_end_line").and_then(|v| v.as_u64()),
            Some(8)
        );
    }

    #[test]
    fn vue_router_routes_with_nesting_and_alias_component() {
        // constantRoutes:嵌套 children 相对 path 拼接;component 两种形态(import()
        // 字面量 / 标识符经别名表)都换回 spec。
        let src = "import Layout from '@/layout/index.vue';\nexport const constantRoutes: RouteRecordRaw[] = [\n  {\n    path: '/user',\n    component: Layout,\n    children: [\n      {\n        path: 'profile',\n        component: () => import('@/views/system/user/profile/index.vue'),\n        name: 'UserProfile',\n      },\n    ],\n  },\n  {\n    path: '/login',\n    component: () => import('@/views/login.vue'),\n  },\n];\n";
        let file = js_file("src/router/index.ts", src);
        let (entities, _) = extract_all(&file);
        let routes: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Route)
            .collect();
        let by_path = |p: &str| {
            routes
                .iter()
                .find(|r| r.name == p)
                .unwrap_or_else(|| panic!("route {p} missing: {routes:#?}"))
        };
        let user = by_path("/user");
        assert_eq!(
            user.metadata.get("component_spec").and_then(|v| v.as_str()),
            Some("@/layout/index.vue"),
            "标识符 component 经别名表换 spec"
        );
        let profile = by_path("/user/profile");
        assert_eq!(
            profile
                .metadata
                .get("component_spec")
                .and_then(|v| v.as_str()),
            Some("@/views/system/user/profile/index.vue")
        );
        assert_eq!(
            profile.metadata.get("name").and_then(|v| v.as_str()),
            Some("UserProfile")
        );
        by_path("/login");
    }

    #[test]
    fn plain_object_path_is_not_a_route() {
        // 守卫:无 RouteRecordRaw / vue-router 的文件不提 route(普通对象 path: 误报)。
        let src = "export const menu = {\n  path: '/fake',\n};\n";
        let file = js_file("src/x.ts", src);
        let (entities, _) = extract_all(&file);
        assert!(!entities.iter().any(|e| e.kind == EntityKind::Route));
    }

    #[test]
    fn vue_sfc_never_yields_routes() {
        // 守卫 2:.vue 里的 router.push({ path: '/x' }) 是导航跳转,不是路由定义。
        let config = SemanticsConfig::default();
        let mut entities = Vec::new();
        let mut edges = Vec::new();
        let mut file = js_file(
            "src/views/user/authRole.vue",
            "<script setup>\nimport type { RouteRecordRaw } from 'vue-router';\nrouter.push({ path: '/system/user' });\n</script>\n",
        );
        file.kind = FileKind::Vue;
        extract_frontend(
            &file,
            "src/views/user/authRole.vue",
            &mut entities,
            &mut edges,
            &config,
        );
        assert!(!entities.iter().any(|e| e.kind == EntityKind::Route));
    }

    #[test]
    fn vue_page_carries_component_refs() {
        // .vue import 的组件清单进 metadata(供 resolve 层建 component_ref 边)。
        let config = SemanticsConfig::default();
        let mut entities = Vec::new();
        let mut edges = Vec::new();
        let mut file = js_file(
            "src/views/user/index.vue",
            "<script setup>\nimport UserForm from './components/UserForm.vue';\nimport { listUser } from '@/api/system/user';\n</script>\n",
        );
        file.kind = FileKind::Vue;
        extract_frontend(
            &file,
            "src/views/user/index.vue",
            &mut entities,
            &mut edges,
            &config,
        );
        let page = entities
            .iter()
            .find(|e| e.kind == EntityKind::VuePage)
            .unwrap();
        let refs = page
            .metadata
            .get("component_refs")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(refs.len(), 1, "{refs:?}");
        assert_eq!(refs[0]["spec"], "./components/UserForm.vue");
        assert_eq!(refs[0]["name"], "UserForm");
    }

    #[test]
    fn frontend_field_skips_ts_symbol_names() {
        // 去重(P2):.vue 内 `const theme = ...` 是 function;`settings.theme` 的属性
        // 引用不再另建同名 frontend_field。
        let config = SemanticsConfig::default();
        let mut entities = Vec::new();
        let mut edges = Vec::new();
        let mut file = js_file(
            "src/layout/index.vue",
            "<script setup>\nconst theme = computed(() => settings.theme);\nconsole.log(settings.theme);\n</script>\n",
        );
        file.kind = FileKind::Vue;
        extract_frontend(
            &file,
            "src/layout/index.vue",
            &mut entities,
            &mut edges,
            &config,
        );
        assert!(
            entities
                .iter()
                .any(|e| e.kind == EntityKind::Function && e.name == "theme"),
            "声明仍应产出 function 实体"
        );
        assert!(
            !entities
                .iter()
                .any(|e| e.kind == EntityKind::FrontendField && e.name == "theme"),
            "同名属性引用应被去重: {entities:?}"
        );
    }
}
