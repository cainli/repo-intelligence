use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ignore::WalkBuilder;
use repo_intelligence_model::EntityId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    Java,
    JavaScript,
    TypeScript,
    Vue,
    Xml,
    Yaml,
    Properties,
    Gradle,
    Sql,
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
            Some("yml") | Some("yaml") => Self::Yaml,
            Some("properties") => Self::Properties,
            Some("sql") => Self::Sql,
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

pub fn discover(root: &Path) -> Result<Vec<SourceFile>> {
    let mut files = Vec::new();
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .require_git(false)
        .filter_entry(|entry| {
            !matches!(
                entry.file_name().to_str(),
                Some("node_modules" | "target" | "build" | "dist" | ".gradle" | ".git")
            )
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
        if metadata.len() > 2 * 1024 * 1024 {
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
