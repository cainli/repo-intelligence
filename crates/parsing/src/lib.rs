use anyhow::{Result, anyhow};
use repo_intelligence_source::{FileKind, SourceFile};
use tree_sitter::Parser;

pub struct ParseOutput<'a> {
    pub file: &'a SourceFile,
    pub has_syntax_errors: bool,
    /// Java 文件的完整语法树,供语义层用 tree-sitter Query 做结构化提取
    /// (字段类型 + 注解配对、构造参数、@Bean 方法等)。非 Java 文件为 None。
    /// 注:tree_sitter::Tree 未实现 Clone/Debug,故本结构不再 derive 它们;
    /// 语义层只消费一次,无需克隆。
    pub tree: Option<tree_sitter::Tree>,
}

pub trait Extractor {
    fn supports(&self, file: &SourceFile) -> bool;
    fn parse<'a>(&self, file: &'a SourceFile) -> Result<ParseOutput<'a>>;
}

#[derive(Default)]
pub struct JavaParser;

impl Extractor for JavaParser {
    fn supports(&self, file: &SourceFile) -> bool {
        file.kind == FileKind::Java
    }

    fn parse<'a>(&self, file: &'a SourceFile) -> Result<ParseOutput<'a>> {
        if !self.supports(file) {
            return Err(anyhow!("unsupported file kind"));
        }
        let mut parser = Parser::new();
        parser.set_language(&tree_sitter_java::LANGUAGE.into())?;
        let tree = parser
            .parse(&file.content, None)
            .ok_or_else(|| anyhow!("tree-sitter returned no syntax tree"))?;
        let has_syntax_errors = tree.root_node().has_error();
        Ok(ParseOutput {
            file,
            has_syntax_errors,
            tree: Some(tree),
        })
    }
}
