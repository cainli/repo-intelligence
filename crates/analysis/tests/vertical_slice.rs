use std::fs;

use repo_intelligence_analysis::{ImpactAnalyzer, ScanPhase, WorkspaceIndexer};
use repo_intelligence_graph::{GraphStore, SqliteGraphStore};
use repo_intelligence_model::{
    ChangeRequest, EdgeKind, EntityKind, EvidenceClass, SearchQuery, TraverseQuery,
};

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
fn scan_reports_stage_and_file_progress() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("Dto.java"),
        "class Dto { private String name; }",
    )
    .unwrap();
    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    let mut progress = Vec::new();

    WorkspaceIndexer
        .scan_with_progress(dir.path(), &mut store, |event| progress.push(event))
        .unwrap();

    assert_eq!(progress.first().unwrap().phase, ScanPhase::Discovering);
    assert!(progress.iter().any(|event| {
        event.phase == ScanPhase::Parsing && event.processed == 1 && event.total == 1
    }));
    assert!(
        progress
            .iter()
            .any(|event| event.phase == ScanPhase::Resolving)
    );
    assert!(
        progress
            .iter()
            .any(|event| event.phase == ScanPhase::Persisting)
    );
    assert_eq!(progress.last().unwrap().phase, ScanPhase::Completed);
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
            edge_kinds: Vec::new(),
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

#[test]
fn recognizes_custom_rpc_endpoint_annotations() {
    // Non-Spring-MVC services (RMB @RmbMap, Dubbo, ...) must still produce
    // http_endpoint entities so the API view and find_endpoint work for them.
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("RmbService.java"),
        r#"
        @RmbMap("queryOrder")
        class OrderService {}
        "#,
    )
    .unwrap();
    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();
    let endpoint = store
        .search(SearchQuery::new("queryOrder").with_limit(10))
        .unwrap();
    assert!(
        endpoint
            .iter()
            .any(|matched| matched.entity.kind == EntityKind::HttpEndpoint),
        "@RmbMap should produce an http_endpoint entity"
    );
}

#[test]
fn analyze_depth_defaults_shallow_for_destructive_operations() {
    use repo_intelligence_model::{ChangeOperation, Edge, Entity, EntityId, GraphPatch};
    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    let a = Entity::new(
        EntityId::stable("repo", "A.java", EntityKind::Field, "target", ""),
        EntityKind::Field,
        "target",
        "A.target",
    );
    let b = Entity::new(
        EntityId::stable("repo", "B.java", EntityKind::Field, "mid", ""),
        EntityKind::Field,
        "mid",
        "B.mid",
    );
    let c = Entity::new(
        EntityId::stable("repo", "C.java", EntityKind::Field, "far", ""),
        EntityKind::Field,
        "far",
        "C.far",
    );
    let edges = vec![
        Edge::new(a.id.clone(), b.id.clone(), EdgeKind::DependsOn),
        Edge::new(b.id.clone(), c.id.clone(), EdgeKind::DependsOn),
    ];
    store
        .apply_patch(GraphPatch::add(vec![a, b, c], edges))
        .unwrap();

    let shallow = ImpactAnalyzer::new(&store)
        .analyze(&ChangeRequest {
            target_kind: "field".into(),
            operation: ChangeOperation::Remove,
            from: Some("target".into()),
            to: None,
            limit: None,
            offset: None,
            depth: None,
        })
        .unwrap();
    // remove defaults to depth 1 → only the direct dependent (target → mid).
    assert_eq!(shallow.findings.len(), 1);
    assert_eq!(shallow.findings[0].path.len(), 2);

    let deep = ImpactAnalyzer::new(&store)
        .analyze(&ChangeRequest {
            target_kind: "field".into(),
            operation: ChangeOperation::Remove,
            from: Some("target".into()),
            to: None,
            limit: None,
            offset: None,
            depth: Some(2),
        })
        .unwrap();
    // Explicit depth 2 reaches target → mid → far.
    assert_eq!(deep.findings[0].path.len(), 3);
}

#[test]
fn matches_endpoint_via_suffix_aligns_baseurl_prefix() {
    // 前端调用带 baseURL 前缀(/api),后端类级 @RequestMapping 用 value= 形式。
    // 精确全等失败,但后缀段对齐应产生 Inferred 低置信 matches_endpoint 边。
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("OrderController.java"),
        r#"
        @RestController
        @RequestMapping(value = "/orders")
        class OrderController {
          @GetMapping("/{id}")
          OrderDto getOrder() { return null; }
        }
        "#,
    )
    .unwrap();
    fs::write(
        dir.path().join("OrderPage.vue"),
        r#"<script setup>
        const r = await request.get("/api/orders/1")
        </script>"#,
    )
    .unwrap();
    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();
    let call = store
        .search(SearchQuery::new("api/orders").with_limit(10))
        .unwrap()
        .into_iter()
        .find(|matched| matched.entity.kind == EntityKind::HttpClientCall)
        .expect("http_client_call for /api/orders")
        .entity;
    let traversal = store
        .traverse(TraverseQuery::outbound(call.id).with_depth(1))
        .unwrap();
    let edge = traversal
        .edges
        .iter()
        .find(|edge| edge.kind == EdgeKind::MatchesEndpoint)
        .expect("matches_endpoint edge via suffix alignment");
    let evidence = edge.evidence.first().expect("edge evidence");
    assert_eq!(evidence.classification, EvidenceClass::Inferred);
    assert!((evidence.confidence - 0.6).abs() < 1e-6);
}

#[test]
fn extracts_spring_bean_dependency_injection() {
    // @Autowired 字段注入:OrderService 依赖 OrderRepo(bean)。
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("OrderService.java"),
        r#"
        class OrderService {
          @Autowired
          private OrderRepo repo;
        }
        "#,
    )
    .unwrap();
    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();
    let bean = store
        .search(SearchQuery::new("OrderRepo").with_limit(10))
        .unwrap()
        .into_iter()
        .find(|matched| matched.entity.kind == EntityKind::SpringBean)
        .expect("SpringBean for OrderRepo")
        .entity;
    // DependsOn 方向:owner(OrderService) → bean(OrderRepo)。从 bean 反向遍历应到 owner。
    let owners = store
        .traverse(TraverseQuery {
            start: bean.id,
            outbound: false,
            max_depth: 1,
            edge_kinds: vec![EdgeKind::DependsOn],
        })
        .unwrap();
    assert!(
        owners
            .entities
            .iter()
            .any(|entity| entity.name == "OrderService"),
        "OrderService should depend on (inject) OrderRepo"
    );
}

#[test]
fn marks_transactional_and_scheduled_classes() {
    // @Transactional(类级)与 @Scheduled(方法级)应作为 metadata 挂到所在 class。
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("FooService.java"),
        r#"
        @Transactional
        class FooService {
          @Scheduled(cron = "0 0 * * *")
          public void cleanup() {}
        }
        "#,
    )
    .unwrap();
    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();
    let class = store
        .search(SearchQuery::new("FooService").with_limit(10))
        .unwrap()
        .into_iter()
        .find(|matched| matched.entity.kind == EntityKind::Class)
        .expect("FooService class")
        .entity;
    assert_eq!(class.metadata["transactional"].as_bool(), Some(true));
    assert_eq!(class.metadata["scheduled"].as_bool(), Some(true));
}

#[test]
fn impact_finding_carries_confidence_field() {
    // finding 携带 confidence。字段级分析路径上的边都是 Fact/Resolved,故 confidence=1.0。
    // (Inferred 边在 call↔endpoint 之间,字段路径暂不触及——已知结构限制。)
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("Dto.java"),
        "class Dto { private String name; }",
    )
    .unwrap();
    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();
    let report = ImpactAnalyzer::new(&store)
        .analyze(&ChangeRequest::rename_field("name", "renamed"))
        .unwrap();
    assert!(
        report
            .findings
            .iter()
            .all(|finding| finding.confidence >= 1.0 - 1e-6),
        "pure-field path should be fully confident"
    );
}
