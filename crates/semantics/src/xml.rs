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
// G1:WHERE 子句体 → group1(WHERE 到 GROUP BY/ORDER BY/HAVING/LIMIT/结尾)。
static SQL_WHERE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)\bWHERE\b(.*?)(?:\bGROUP\s+BY\b|\bORDER\s+BY\b|\bHAVING\b|\bLIMIT\b|$|;)").unwrap()
});
// G1:JOIN ON 子句体 → group1(ON 到 WHERE/下一 JOIN/其他子句关键字)。
static SQL_JOIN_ON: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)\bON\b\s+(.*?)(?:\bWHERE\b|\bGROUP\s+BY\b|\bORDER\s+BY\b|\bHAVING\b|\bLIMIT\b|\bJOIN\b|$|;)").unwrap()
});
// G1:列名候选(标识符,可带别名前缀 u.col)。函数名/关键字由 classify 过滤;
// MyBatis 占位符 #{...}/${...} 内的标识符(jdbcType/VARCHAR 等参数属性)由区间屏蔽排除。
static SQL_COLUMN_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b([A-Za-z_]\w*(?:\.[A-Za-z_]\w*)?)").unwrap()
});
// G1:MyBatis 参数占位符区间。落在其中的标识符(参数名、jdbcType、SQL 类型字面量)
// 不是列——必须用区间屏蔽,逐字符推断拦不住 `#{name,jdbcType=VARCHAR}` 的深层属性。
static SQL_PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)#\{[^}]*\}|\$\{[^}]*\}").unwrap()
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
        // G1:WHERE/JOIN 的列引用 → SqlField + ReadsColumn(条件/连接列进图谱)。
        // 历史只抓 SELECT alias + 写入列,WHERE/JOIN 列缺失 → 改这些列的影响面分析漏
        // (如改 user_id,`WHERE user_id=?` 的语句追不到)。先抽 WHERE/JOIN 子句体,
        // 再经 extract_where_join_columns 过滤噪声(占位符区间屏蔽 + classify)取列名。
        {
            let mut seen_cols: std::collections::HashSet<String> = std::collections::HashSet::new();
            for regex in [&*SQL_WHERE, &*SQL_JOIN_ON] {
                for cap in regex.captures_iter(sql.as_str()) {
                    let body_match = cap.get(1).unwrap();
                    let body = body_match.as_str();
                    let body_start = body_match.start();
                    for (col_name, tok_start) in extract_where_join_columns(body) {
                        if !seen_cols.insert(col_name.clone()) {
                            continue;
                        }
                        let field_line = line_of(&file.content, sql.start() + body_start + tok_start);
                        let field = Entity::new(
                            EntityId::stable(
                                "workspace",
                                path,
                                EntityKind::SqlField,
                                &col_name,
                                "where_join",
                            ),
                            EntityKind::SqlField,
                            &col_name,
                            format!("{path}#{}:{}", statement_id.as_str(), col_name),
                        )
                        .with_metadata(json!({"source_column": &col_name, "origin": "where_join"}))
                        .with_evidence(path, field_line, field_line, EvidenceClass::Fact, 1.0, "SQL where/join column");
                        edges.push(
                            Edge::new(statement_id_value.clone(), field.id.clone(), EdgeKind::ReadsColumn)
                                .with_evidence(path, field_line, field_line, EvidenceClass::Fact, 1.0, "where/join reads column"),
                        );
                        entities.push(field);
                    }
                }
            }
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

/// 从 WHERE/JOIN 子句体提取列引用(计划 G1)。返回 (列名, token 在 body 内起始偏移)。
///
/// 三层过滤:① MyBatis 占位符 `#{...}`/`${...}` 区间屏蔽(拦参数名 + jdbcType + SQL 类型,
///   逐字符推断拦不住深层 `#{name,jdbcType=VARCHAR}`);② classify_where_column 过滤
///   SQL 关键字/函数名;③ `alias.col` 规范化取 col。不做去重——调用方按列名去重
///   (WHERE 与 JOIN 可能引用同列,EntityId 稳定故同列只建一条)。
fn extract_where_join_columns(body: &str) -> Vec<(String, usize)> {
    let placeholder_spans: Vec<(usize, usize)> = SQL_PLACEHOLDER
        .find_iter(body)
        .map(|m| (m.start(), m.end()))
        .collect();
    let mut out = Vec::new();
    for tok in SQL_COLUMN_TOKEN.captures_iter(body) {
        let m = tok.get(1).unwrap();
        let start = m.start();
        if placeholder_spans.iter().any(|(a, b)| start >= *a && start < *b) {
            continue;
        }
        let raw = m.as_str();
        if let Some(col) = classify_where_column(raw, body, start) {
            out.push((col, start));
        }
    }
    out
}

/// 判断 WHERE/JOIN 子句里的标识符 token 是否为列引用,返回规范化列名。
///
/// 过滤噪声(计划 G1 难点):SQL 关键字、函数名(后跟 `(`)、MyBatis 参数占位符
/// (`#{xxx}`/`${xxx}` 内的标识符)。`alias.col` 取 `col`——影响面分析按列名匹配,
/// 表别名前缀无意义且跨 statement 不稳定。
fn classify_where_column(token: &str, body: &str, match_start: usize) -> Option<String> {
    const KEYWORDS: &[&str] = &[
        "and", "or", "not", "null", "is", "in", "like", "between", "exists",
        "as", "on", "where", "select", "from", "join", "inner", "left", "right",
        "full", "outer", "group", "order", "by", "having", "limit", "union",
        "case", "when", "then", "else", "end", "distinct", "all", "any",
        "true", "false", "asc", "desc",
    ];
    let lower = token.to_ascii_lowercase();
    if KEYWORDS.contains(&lower.as_str()) {
        return None;
    }
    // MyBatis 参数占位符内的标识符:token 前紧邻 #{ 或 ${ → 不是列。
    let before = body[..match_start].trim_end();
    if before.ends_with("#{") || before.ends_with("${") {
        return None;
    }
    // 函数名:token 后紧跟 ( → DATE(x)/COUNT(*),不是列。
    let after = body.get(match_start + token.len()..).unwrap_or("");
    if after.trim_start().starts_with('(') {
        return None;
    }
    // alias.col → col(rsplit 取最后一段;无前缀时即自身)。
    let col = token.rsplit('.').next()?;
    if col.is_empty() {
        return None;
    }
    Some(col.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_keeps_plain_column() {
        assert_eq!(classify_where_column("user_id", "where user_id = 1", 6), Some("user_id".into()));
    }
    #[test]
    fn classify_drops_sql_keyword() {
        assert_eq!(classify_where_column("and", "a and b", 2), None);
    }
    #[test]
    fn classify_strips_alias_prefix() {
        assert_eq!(classify_where_column("u.user_id", "where u.user_id = 1", 6), Some("user_id".into()));
    }
    #[test]
    fn classify_drops_function_call() {
        assert_eq!(classify_where_column("date", "where date(x) = 1", 6), None);
    }
    #[test]
    fn classify_drops_mybatis_param() {
        // #{userid}:token 前是 #{ → 参数占位符,非列。
        assert_eq!(classify_where_column("userid", "where id = #{userid}", 13), None);
    }
    #[test]
    fn classify_keeps_join_on_column() {
        // ON a.id = b.aid:a.id 取 id。
        assert_eq!(classify_where_column("a.id", "a.id = b.aid", 0), Some("id".into()));
    }
    #[test]
    fn extract_filters_mybatis_placeholder_attrs() {
        // `#{dataName,jdbcType=VARCHAR}` 占位符区间内的 dataName/jdbcType/VARCHAR 全屏蔽,
        // 只剩 = 左的 data_name 真列(逐字符推断会漏抓 jdbcType/VARCHAR)。
        let body = "data_name = #{dataName,jdbcType=VARCHAR}";
        let cols: Vec<String> = extract_where_join_columns(body)
            .into_iter()
            .map(|(c, _)| c)
            .collect();
        assert_eq!(cols, vec!["data_name".to_string()]);
    }
}
