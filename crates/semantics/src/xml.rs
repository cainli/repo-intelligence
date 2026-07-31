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
// insert 的列列表:INSERT INTO t (col1, col2, …) → group1 = 列清单(P1-3 写入列)。
static SQL_INSERT_COLS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)INSERT\s+INTO\s+[\w.]+\s*\(([^)]+)\)").unwrap()
});
// update 的 SET 段:SET col1=…, col2=… → group1 = SET 子句(再按 word= 取列名)。
static SQL_UPDATE_SET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bSET\s+(.*?)(?:\bWHERE\b|;|$)").unwrap()
});
// <mapper namespace="com.x.UserDao">:MyBatis 接口绑定的权威键(=接口 Java 全限定名)。
// 配对 method↔statement 走 namespace + statement_id,见 analysis::resolve_cross_stack。
// 原生 MyBatis(无 @TableName/BaseMapper)的 method→table 链全靠这条 namespace 接上。
static XML_NAMESPACE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)<mapper\b[^>]*\bnamespace\s*=\s*"([^"]+)""#).unwrap()
});

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
    // 每个 mapper.xml 一个 namespace(=接口 Java 全限定名)。存入其下每个 statement 的
    // metadata,供 analysis 按 namespace+id 把 Mapper 接口方法绑定到 statement。
    let namespace = XML_NAMESPACE
        .captures(&file.content)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string());
    for capture in XML_STATEMENT.captures_iter(&file.content) {
        let operation = capture[1].to_lowercase();
        let statement_id = capture.get(2).unwrap();
        let sql = capture.get(3).unwrap();
        let line = line_of(&file.content, statement_id.start());
        let metadata = match &namespace {
            Some(ns) => json!({"operation": operation, "namespace": ns, "sql": sql.as_str()}),
            None => json!({"operation": operation, "sql": sql.as_str()}),
        };
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
        .with_metadata(metadata)
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
        // insert/update 的写入列 → Column + statement-[WritesColumn]->column(P1-3)。
        // 历史只提取 select 的读列(ReadsColumn),写入列缺失,导致"哪个列被写"不可查。
        if operation == "insert" || operation == "update" {
            let write_cols: Vec<String> = if operation == "insert" {
                SQL_INSERT_COLS
                    .captures(sql.as_str())
                    .and_then(|capture| capture.get(1))
                    .map(|list| list.as_str())
                    .map(|list| {
                        list.split(',')
                            .map(|token| {
                                token
                                    .trim()
                                    .trim_matches('`')
                                    .trim_matches('"')
                                    .to_string()
                            })
                            .filter(|token| {
                                !token.is_empty()
                                    && token
                                        .chars()
                                        .next()
                                        .is_some_and(|ch| ch.is_ascii_alphabetic())
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                SQL_UPDATE_SET
                    .captures(sql.as_str())
                    .and_then(|capture| capture.get(1))
                    .map(|set_clause| set_clause.as_str())
                    .map(|set_clause| {
                        let assign_re =
                            Regex::new(r"([A-Za-z_]\w*)\s*=").unwrap();
                        assign_re
                            .captures_iter(set_clause)
                            .filter_map(|capture| {
                                capture.get(1).map(|m| m.as_str().to_string())
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            };
            for col_name in write_cols {
                let column = Entity::new(
                    EntityId::stable("workspace", path, EntityKind::Column, &col_name, ""),
                    EntityKind::Column,
                    &col_name,
                    format!("{path}#{col_name}"),
                )
                .with_metadata(json!({"source": "xml_write"}))
                .with_evidence(path, line, line, EvidenceClass::Fact, 1.0, "SQL write column");
                edges.push(
                    Edge::new(statement_id_value.clone(), column.id.clone(), EdgeKind::WritesColumn)
                        .with_evidence(
                            path,
                            line,
                            line,
                            EvidenceClass::Fact,
                            1.0,
                            "insert/update writes column",
                        ),
                );
                entities.push(column);
            }
        }
    }
}
