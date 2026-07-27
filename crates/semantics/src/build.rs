//! 模块依赖图:build.gradle(.kts) / libs.versions.toml / package.json → Package + DependsOn。
//! 收益定位:模块级依赖影响(模块 A 依赖 SDK X),非方法级调用链(后者需 Method 提取)。
//! 注意:实体是 path-scoped(EntityId 含 path),同名依赖跨文件不自动合并——
//! v1 接受;跨文件共享依赖合并需 resolve 层按 ecosystem+name 处理,留后续。

use std::sync::LazyLock;

use anyhow::Result;
use regex::Regex;
use repo_intelligence_model::{Edge, EdgeKind, Entity, EntityId, EntityKind, EvidenceClass};
use repo_intelligence_source::{FileKind, SourceFile};
use serde_json::json;

use crate::registry::{ExtractContext, SemanticExtractor};
use crate::add_contained;

// implementation/api/... ("group:artifact:version") 或 (libs.xxx 别名)。
// 组1=引号坐标(group:artifact,版本剥离以利合并),组2=libs.xxx 别名。
static GRADLE_DEP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?:implementation|api|compileOnly|runtimeOnly|testImplementation|developmentOnly)\s*\(\s*(?:"([^":]+:[^":]+)(?::[^"]*)?"|(libs\.[A-Za-z0-9_.]+))"#,
    )
    .unwrap()
});
static TOML_LIB: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?ms)^\[libraries\.([^\]]+)\][\s\S]*?module\s*=\s*"([^"]+)""#).unwrap()
});

pub struct GradleExtractor;

impl SemanticExtractor for GradleExtractor {
    fn supports(&self, kind: FileKind) -> bool {
        kind == FileKind::Gradle
    }

    fn extract(
        &self,
        _ctx: &ExtractContext,
        file: &SourceFile,
        path: &str,
        entities: &mut Vec<Entity>,
        edges: &mut Vec<Edge>,
    ) -> Result<()> {
        extract_gradle(file, path, entities, edges);
        Ok(())
    }
}

pub struct VersionCatalogExtractor;

impl SemanticExtractor for VersionCatalogExtractor {
    fn supports(&self, kind: FileKind) -> bool {
        kind == FileKind::Toml
    }

    fn extract(
        &self,
        _ctx: &ExtractContext,
        file: &SourceFile,
        path: &str,
        entities: &mut Vec<Entity>,
        edges: &mut Vec<Edge>,
    ) -> Result<()> {
        extract_version_catalog(file, path, entities, edges);
        Ok(())
    }
}

pub struct PackageJsonExtractor;

impl SemanticExtractor for PackageJsonExtractor {
    fn supports(&self, kind: FileKind) -> bool {
        kind == FileKind::Json
    }

    fn extract(
        &self,
        _ctx: &ExtractContext,
        file: &SourceFile,
        path: &str,
        entities: &mut Vec<Entity>,
        edges: &mut Vec<Edge>,
    ) -> Result<()> {
        extract_package_json(file, path, entities, edges);
        Ok(())
    }
}

/// build.gradle(.kts) → 模块 Gradle Package + 依赖 Package(--DependsOn)。
fn extract_gradle(file: &SourceFile, path: &str, entities: &mut Vec<Entity>, edges: &mut Vec<Edge>) {
    // 模块名 = build 脚本所在目录名
    let module_name = file
        .relative_path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("gradle-module")
        .to_string();
    let module = Entity::new(
        EntityId::stable("workspace", path, EntityKind::Package, &module_name, "gradle"),
        EntityKind::Package,
        &module_name,
        &module_name,
    )
    .with_metadata(json!({"ecosystem": "gradle"}))
    .with_evidence(path, 1, 1, EvidenceClass::Fact, 1.0, "Gradle module (build script)");
    let module_id = module.id.clone();
    add_contained(file, path, module, 1, entities, edges);

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
        .with_evidence(path, 1, 1, EvidenceClass::Fact, 1.0, "Gradle version catalog alias");
        entities.push(entity);
    }
}

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
    .with_evidence(path, 1, 1, EvidenceClass::Fact, 1.0, "npm package (package.json)");
    let module_id = module.id.clone();
    add_contained(file, path, module, 1, entities, edges);
    for section in ["dependencies", "devDependencies", "peerDependencies"] {
        let Some(obj) = value.get(section).and_then(|v| v.as_object()) else {
            continue;
        };
        for dep_name in obj.keys() {
            let dep = Entity::new(
                EntityId::stable("workspace", path, EntityKind::Package, dep_name.as_str(), "npm"),
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
