//! 前端提取:Vue 单文件组件页、`a.b` 属性引用(FrontendField)、HTTP 调用。
//! 噪声词(JS 内建方法)从 `SemanticsConfig` 读,默认 builtin ~70 个。

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use anyhow::Result;
use regex::Regex;
use repo_intelligence_config::SemanticsConfig;
use repo_intelligence_model::{Edge, Entity, EntityId, EntityKind, EvidenceClass};
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

pub struct FrontendExtractor;

impl SemanticExtractor for FrontendExtractor {
    fn supports(&self, kind: FileKind) -> bool {
        matches!(kind, FileKind::Vue | FileKind::JavaScript | FileKind::TypeScript)
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
        if !is_likely_field(name.as_str(), &noise) {
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
}
