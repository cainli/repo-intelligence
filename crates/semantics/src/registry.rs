//! 语义提取器注册表。`SemanticExtractor` 是"语义提取"(产出实体/边),
//! 区别于 `parsing::Extractor`(语法解析 → tree-sitter Tree)。新增语言/框架
//! 提取器:实现 trait + `Registry::register`,无需改核心分发。
//!
//! 默认注册集覆盖 indexer 当前瞄准的 Java 全栈(Spring/MyBatis Plus/MyBatis XML/
//! Vue·前端 HTTP/Gradle·TOML·npm 依赖图)。一文件由首个 `supports` 的提取器处理。

use anyhow::Result;
use repo_intelligence_config::SemanticsConfig;
use repo_intelligence_model::{Edge, Entity};
use repo_intelligence_source::{FileKind, SourceFile};

/// 提取期上下文:携带语义配置(自研 RPC 注解、前端噪声词)。
pub struct ExtractContext<'a> {
    pub config: &'a SemanticsConfig,
}

/// 语义提取器:把一个源文件的框架语义抽成实体/边。命名 `SemanticExtractor`
/// 而非 `Extractor`,避免与 `repo_intelligence_parsing::Extractor`(语法解析)混淆。
pub trait SemanticExtractor: Send + Sync {
    fn supports(&self, kind: FileKind) -> bool;
    fn extract(
        &self,
        ctx: &ExtractContext,
        file: &SourceFile,
        path: &str,
        entities: &mut Vec<Entity>,
        edges: &mut Vec<Edge>,
    ) -> Result<()>;
}

/// 提取器集合。`default_java_stack()` 是内置注册集;`register()` 是扩展点。
pub struct Registry {
    extractors: Vec<Box<dyn SemanticExtractor>>,
}

impl Registry {
    /// 内置注册集(Java 全栈)。顺序即优先级:首个 `supports` 的提取器处理该文件。
    pub fn default_java_stack() -> Self {
        Self {
            extractors: vec![
                Box::new(crate::java::JavaExtractor),
                Box::new(crate::xml::XmlExtractor),
                Box::new(crate::frontend::FrontendExtractor),
                Box::new(crate::build::GradleExtractor),
                Box::new(crate::build::VersionCatalogExtractor),
                Box::new(crate::build::PackageJsonExtractor),
            ],
        }
    }

    /// 注册额外提取器(扩展点:加新语言/框架)。
    pub fn register(&mut self, extractor: Box<dyn SemanticExtractor>) {
        self.extractors.push(extractor);
    }

    /// 用首个支持该文件类型的提取器提取;无匹配则 no-op(与历史 `_ => {}` 一致)。
    pub fn extract(
        &self,
        ctx: &ExtractContext,
        file: &SourceFile,
        path: &str,
        entities: &mut Vec<Entity>,
        edges: &mut Vec<Edge>,
    ) -> Result<()> {
        for extractor in &self.extractors {
            if extractor.supports(file.kind) {
                return extractor.extract(ctx, file, path, entities, edges);
            }
        }
        Ok(())
    }
}
