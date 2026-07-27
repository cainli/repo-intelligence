//! MyBatis XML mapper 提取:statement、SQL alias(SqlField)、表引用(读/写)。

use std::sync::LazyLock;

use anyhow::Result;
use regex::Regex;
use repo_intelligence_model::{Edge, EdgeKind, Entity, EntityId, EntityKind, EvidenceClass};
use repo_intelligence_source::{FileKind, SourceFile};
use serde_json::json;

use crate::registry::{ExtractContext, SemanticExtractor};
use crate::{add_contained, line_of};

static XML_STATEMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<(select|insert|update|delete)\b[^>]*\bid="([^"]+)"[^>]*>(.*?)</(?:select|insert|update|delete)>"#).unwrap()
});
static SQL_ALIAS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b([A-Za-z_]\w*)\s+AS\s+([A-Za-z_]\w*)").unwrap());
static SQL_FROM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:FROM|JOIN|UPDATE|INTO)\s+([A-Za-z_][\w.]*)").unwrap());

pub struct XmlExtractor;

impl SemanticExtractor for XmlExtractor {
    fn supports(&self, kind: FileKind) -> bool {
        kind == FileKind::Xml
    }

    fn extract(
        &self,
        _ctx: &ExtractContext,
        file: &SourceFile,
        path: &str,
        entities: &mut Vec<Entity>,
        edges: &mut Vec<Edge>,
    ) -> Result<()> {
        extract_xml(file, path, entities, edges);
        Ok(())
    }
}

fn extract_xml(file: &SourceFile, path: &str, entities: &mut Vec<Entity>, edges: &mut Vec<Edge>) {
    for capture in XML_STATEMENT.captures_iter(&file.content) {
        let operation = capture[1].to_lowercase();
        let statement_id = capture.get(2).unwrap();
        let sql = capture.get(3).unwrap();
        let line = line_of(&file.content, statement_id.start());
        let statement = Entity::new(
            EntityId::stable(
                "workspace",
                path,
                EntityKind::XmlStatement,
                statement_id.as_str(),
                "",
            ),
            EntityKind::XmlStatement,
            statement_id.as_str(),
            format!("{path}#{}", statement_id.as_str()),
        )
        .with_metadata(json!({"operation": operation}))
        .with_evidence(path, line, line, EvidenceClass::Fact, 1.0, "MyBatis statement");
        let statement_id_value = statement.id.clone();
        add_contained(file, path, statement, line, entities, edges);
        for alias in SQL_ALIAS.captures_iter(sql.as_str()) {
            let name = alias.get(2).unwrap();
            let field_line = line_of(&file.content, sql.start() + name.start());
            let field = Entity::new(
                EntityId::stable(
                    "workspace",
                    path,
                    EntityKind::SqlField,
                    name.as_str(),
                    &alias[1],
                ),
                EntityKind::SqlField,
                name.as_str(),
                format!("{path}#{}:{}", statement_id.as_str(), name.as_str()),
            )
            .with_metadata(json!({"source_column": &alias[1]}))
            .with_evidence(path, field_line, field_line, EvidenceClass::Fact, 1.0, "SQL column alias");
            edges.push(
                Edge::new(
                    statement_id_value.clone(),
                    field.id.clone(),
                    EdgeKind::ReadsColumn,
                )
                .with_evidence(path, field_line, field_line, EvidenceClass::Fact, 1.0, "selected SQL field"),
            );
            entities.push(field);
        }
        for (table_index, table_match) in SQL_FROM.captures_iter(sql.as_str()).enumerate() {
            let name = table_match.get(1).unwrap();
            let table = Entity::new(
                EntityId::stable("workspace", path, EntityKind::Table, name.as_str(), ""),
                EntityKind::Table,
                name.as_str(),
                name.as_str(),
            )
            .with_evidence(path, line, line, EvidenceClass::Fact, 1.0, "SQL table reference");
            let writes_target = operation != "select" && table_index == 0;
            let edge_kind = if writes_target {
                EdgeKind::WritesTable
            } else {
                EdgeKind::ReadsTable
            };
            edges.push(
                Edge::new(statement_id_value.clone(), table.id.clone(), edge_kind).with_evidence(
                    path,
                    line,
                    line,
                    EvidenceClass::Fact,
                    1.0,
                    if writes_target {
                        "SQL mutation target"
                    } else {
                        "SQL read table reference"
                    },
                ),
            );
            entities.push(table);
        }
    }
}
