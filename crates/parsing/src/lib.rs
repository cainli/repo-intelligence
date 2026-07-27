use anyhow::{Result, anyhow};
use repo_intelligence_source::{FileKind, SourceFile};
use tree_sitter::Parser;

#[derive(Clone, Debug)]
pub struct ParseOutput<'a> {
    pub file: &'a SourceFile,
    pub has_syntax_errors: bool,
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
        Ok(ParseOutput {
            file,
            has_syntax_errors: tree.root_node().has_error(),
        })
    }
}
