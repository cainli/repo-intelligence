use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ignore::WalkBuilder;
use repo_intelligence_config::DiscoveryConfig;
use repo_intelligence_model::EntityId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    Java,
    JavaScript,
    TypeScript,
    Vue,
    Xml,
    Gradle,
    Toml,
    Json,
    Unknown,
}

impl FileKind {
    pub fn from_path(path: &Path) -> Self {
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        if name.ends_with(".gradle") || name.ends_with(".gradle.kts") {
            return Self::Gradle;
        }
        match path.extension().and_then(|value| value.to_str()) {
            Some("java") => Self::Java,
            Some("js") | Some("jsx") => Self::JavaScript,
            Some("ts") | Some("tsx") => Self::TypeScript,
            Some("vue") => Self::Vue,
            Some("xml") => Self::Xml,
            // 注意:yml/yaml/properties/sql 等无语义提取器的扩展名落到 Unknown,
            // discover 会跳过(见下方 `if kind == Unknown`),不再把全文读进内存。
            // 回归:yaml 曾被分类为 Yaml kind,大项目配置文件内容全驻留 → OOM/卡死。
            Some("toml") => Self::Toml,
            Some("json") => Self::Json,
            _ => Self::Unknown,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SourceFile {
    pub id: EntityId,
    pub relative_path: PathBuf,
    pub kind: FileKind,
    pub content_hash: String,
    pub content: String,
}

/// Directory names always excluded from discovery. Re-exported from the config
/// crate (the builtin default) so the MCP layer can still echo it; a workspace
/// `.repo-intelligence.toml` may append more via `excluded_dirs_extra`, resolved
/// at scan time via `DiscoveryConfig::effective_excluded_dirs`.
pub use repo_intelligence_config::DEFAULT_EXCLUDED_DIRS as EXCLUDED_DIRS;

pub fn discover(root: &Path) -> Result<Vec<SourceFile>> {
    discover_with_config(root, &DiscoveryConfig::default())
}

/// 按 `DiscoveryConfig` 发现源文件：排除 `effective_excluded_dirs()` 列出的目录，
/// 跳过超过 `max_file_bytes` 的文件，忽略不支持的扩展名。
pub fn discover_with_config(root: &Path, config: &DiscoveryConfig) -> Result<Vec<SourceFile>> {
    let excluded: HashSet<String> = config.effective_excluded_dirs().into_iter().collect();
    let max_bytes = config.max_file_bytes;
    let mut files = Vec::new();
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .require_git(false)
        .filter_entry(move |entry| {
            let name = entry.file_name().to_str();
            !name.is_some_and(|value| excluded.contains(value))
        })
        .build();
    for entry in walker {
        let entry = entry?;
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.path();
        let kind = FileKind::from_path(path);
        if kind == FileKind::Unknown {
            continue;
        }
        let metadata = entry.metadata()?;
        if metadata.len() > max_bytes {
            continue;
        }
        let content = fs::read_to_string(path)
            .with_context(|| format!("read source file {}", path.display()))?;
        let relative_path = path.strip_prefix(root).unwrap_or(path).to_path_buf();
        let relative = relative_path.to_string_lossy();
        files.push(SourceFile {
            id: EntityId::stable(
                "workspace",
                &relative,
                repo_intelligence_model::EntityKind::File,
                &relative,
                "",
            ),
            relative_path,
            kind,
            content_hash: blake3::hash(content.as_bytes()).to_hex().to_string(),
            content,
        });
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn unsupported_kinds_classify_as_unknown() {
        // 无提取器的扩展名归 Unknown —— discover 跳过,不再把全文读进内存。
        // 回归:yaml 曾被分类为支持的 kind,大项目配置文件内容全驻留 → OOM/卡死。
        for ext in &["yaml", "yml", "properties", "sql", "log", "md", "txt"] {
            let name = format!("config.{ext}");
            assert_eq!(
                FileKind::from_path(Path::new(&name)),
                FileKind::Unknown,
                ".{ext} 应归 Unknown"
            );
        }
    }

    #[test]
    fn supported_kinds_still_classified() {
        assert_eq!(FileKind::from_path(Path::new("A.java")), FileKind::Java);
        assert_eq!(FileKind::from_path(Path::new("m.xml")), FileKind::Xml);
        assert_eq!(FileKind::from_path(Path::new("b.gradle")), FileKind::Gradle);
        assert_eq!(
            FileKind::from_path(Path::new("b.gradle.kts")),
            FileKind::Gradle
        );
        assert_eq!(FileKind::from_path(Path::new("c.json")), FileKind::Json);
        assert_eq!(FileKind::from_path(Path::new("c.toml")), FileKind::Toml);
        assert_eq!(FileKind::from_path(Path::new("v.vue")), FileKind::Vue);
        assert_eq!(FileKind::from_path(Path::new("a.js")), FileKind::JavaScript);
        assert_eq!(FileKind::from_path(Path::new("a.ts")), FileKind::TypeScript);
    }

    #[test]
    fn discover_skips_unsupported_files() {
        // yaml/properties 归 Unknown → discover 不收集(不读内容、不进 Vec<SourceFile>)。
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("big.yaml"), "key: value\n".repeat(50_000)).unwrap();
        std::fs::write(dir.path().join("app.properties"), "k=v\n").unwrap();
        std::fs::write(dir.path().join("B.java"), "class B {}").unwrap();
        let files = discover(dir.path()).unwrap();
        assert_eq!(files.len(), 1, "只收集 .java,跳过 .yaml/.properties");
        assert_eq!(files[0].kind, FileKind::Java);
        assert!(
            files.iter().all(|f| f.kind != FileKind::Unknown),
            "Unknown 不应进 Vec"
        );
    }
}
