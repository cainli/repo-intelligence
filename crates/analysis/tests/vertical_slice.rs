use std::fs;

use repo_intelligence_analysis::{ImpactAnalyzer, WorkspaceIndexer};
use repo_intelligence_graph::{GraphStore, SqliteGraphStore};
use repo_intelligence_model::{ChangeRequest, EdgeKind, EntityKind, SearchQuery, TraverseQuery};

#[test]
fn indexes_cross_stack_field_chain_and_reports_rename_impact() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("OrderController.java"),
        r#"
        @RestController
        @RequestMapping("/orders")
        class OrderController {
          @GetMapping("/{id}")
          OrderDto getOrder() { return service.getOrder(); }
        }
        class OrderDto { private String customerName; }
        "#,
    )
    .unwrap();
    fs::write(
        dir.path().join("OrderMapper.xml"),
        r#"<mapper namespace="demo.OrderMapper">
          <select id="getOrder" resultType="OrderDto">
            SELECT customer_name AS customerName FROM orders
          </select>
        </mapper>"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("OrderPage.vue"),
        r#"<template><span>{{ order.customerName }}</span></template>
        <script setup lang="ts">
        const order = await axios.get("/orders/1")
        </script>"#,
    )
    .unwrap();

    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    let summary = WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();
    assert!(summary.files_indexed >= 3);

    let fields = store
        .search(SearchQuery::new("customerName").with_limit(20))
        .unwrap();
    assert!(
        fields
            .iter()
            .any(|m| m.entity.kind == EntityKind::FrontendField)
    );
    assert!(fields.iter().any(|m| m.entity.kind == EntityKind::Field));
    assert!(fields.iter().any(|m| m.entity.kind == EntityKind::SqlField));

    let report = ImpactAnalyzer::new(&store)
        .analyze(&ChangeRequest::rename_field(
            "customerName",
            "customerFullName",
        ))
        .unwrap();
    assert!(report.findings.len() >= 3);
    assert!(
        report
            .findings
            .iter()
            .all(|finding| !finding.path.is_empty())
    );
    assert!(
        report
            .findings
            .iter()
            .all(|finding| !finding.evidence.is_empty())
    );
}

#[test]
fn classifies_mutating_sql_targets_as_writes() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("OrderMapper.xml"),
        r#"<mapper namespace="OrderMapper">
        <update id="rename">UPDATE orders SET customer_name = 'x'</update>
        </mapper>"#,
    )
    .unwrap();
    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();
    let table = store
        .search(SearchQuery::new("orders"))
        .unwrap()
        .into_iter()
        .find(|matched| matched.entity.kind == EntityKind::Table)
        .unwrap()
        .entity;
    let incoming = store
        .traverse(TraverseQuery {
            start: table.id,
            outbound: false,
            max_depth: 1,
        })
        .unwrap();
    assert!(
        incoming
            .edges
            .iter()
            .any(|edge| edge.kind == EdgeKind::WritesTable)
    );
    assert!(
        !incoming
            .edges
            .iter()
            .any(|edge| edge.kind == EdgeKind::ReadsTable)
    );
}

#[test]
fn rescanning_removes_entities_for_deleted_files() {
    let dir = tempfile::tempdir().unwrap();
    let java = dir.path().join("Temporary.java");
    fs::write(&java, "class Temporary { private String obsoleteField; }").unwrap();
    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();
    assert_eq!(
        store
            .search(SearchQuery::new("obsoleteField").with_limit(10))
            .unwrap()
            .len(),
        1
    );

    fs::remove_file(java).unwrap();
    WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();
    assert!(
        store
            .search(SearchQuery::new("obsoleteField").with_limit(10))
            .unwrap()
            .is_empty()
    );
}
