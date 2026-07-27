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

#[test]
fn impact_of_frontend_field_reaches_endpoint_via_file_bridge() {
    // 前端字段是叶节点,单向 traverse 到所在 file 即停。file-桥接应让它触及
    // 同 file 的 http_client_call → MatchesEndpoint → 后端 endpoint,且因 MatchesEndpoint
    // 多为 Inferred,把 finding.confidence 拉到 < 1.0(分级匹配首次在字段分析生效)。
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
        r#"<template>{{ order.customerName }}</template>
        <script setup>
        const order = await request.get("/api/orders/1")
        </script>"#,
    )
    .unwrap();
    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();
    let endpoint = store
        .search(SearchQuery::new("orders").with_limit(20))
        .unwrap()
        .into_iter()
        .find(|matched| matched.entity.kind == EntityKind::HttpEndpoint)
        .expect("backend endpoint")
        .entity;
    let report = ImpactAnalyzer::new(&store)
        .analyze(&ChangeRequest::rename_field(
            "customerName",
            "customerFullName",
        ))
        .unwrap();
    let finding = report
        .findings
        .iter()
        .find(|found| found.entity.kind == EntityKind::FrontendField)
        .expect("frontend field finding");
    assert!(
        finding.path.contains(&endpoint.id),
        "frontend field impact should reach the backend endpoint via the file bridge"
    );
    assert!(
        finding.confidence < 1.0,
        "confidence should drop via the Inferred matches_endpoint edge, got {}",
        finding.confidence
    );
}

#[test]
fn mybatis_plus_links_field_to_column_for_rename_impact() {
    // MyBatis Plus 主力是注解实体 + BaseMapper + QueryWrapper,XML mapper 很少。
    // 持久层贯通必须覆盖:@TableName/@TableField/@TableId 注解、驼峰推断、
    // exist=false 跳过、BaseMapper、QueryWrapper 列引用,且 rename 字段影响要触达 Column。
    use repo_intelligence_model::EntityId;
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("Order.java"),
        r#"
        @TableName("t_order")
        public class Order {
          @TableId("id")
          private Long id;
          @TableField("customer_name")
          private String customerName;
          private Integer orderStatus;
          @TableField(exist = false)
          private String transientField;
        }
        "#,
    )
    .unwrap();
    fs::write(
        dir.path().join("OrderMapper.java"),
        "interface OrderMapper extends BaseMapper<Order> {}",
    )
    .unwrap();

    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();

    // 显式 @TableField → customer_name Column(Fact)
    let customer_cols = store
        .search(SearchQuery::new("customer_name").with_limit(20))
        .unwrap();
    assert!(
        customer_cols
            .iter()
            .any(|m| m.entity.kind == EntityKind::Column),
        "@TableField 应产出 customer_name Column"
    );
    // 无注解字段 orderStatus 驼峰推断 → order_status Column(Inferred)
    assert!(
        store
            .search(SearchQuery::new("order_status").with_limit(20))
            .unwrap()
            .iter()
            .any(|m| m.entity.kind == EntityKind::Column),
        "无注解字段 orderStatus 应驼峰推断为 order_status Column"
    );
    // exist=false 字段不产出 Column
    assert!(
        !store
            .search(SearchQuery::new("transient_field").with_limit(20))
            .unwrap()
            .iter()
            .any(|m| m.entity.kind == EntityKind::Column),
        "@TableField(exist=false) 字段不应产出 Column"
    );
    // @TableName → Table;BaseMapper → Mapper
    assert!(
        store
            .search(SearchQuery::new("t_order").with_limit(10))
            .unwrap()
            .iter()
            .any(|m| m.entity.kind == EntityKind::Table)
    );
    assert!(
        store
            .search(SearchQuery::new("OrderMapper").with_limit(10))
            .unwrap()
            .iter()
            .any(|m| m.entity.kind == EntityKind::Mapper)
    );

    // rename customerName 的影响必须触达 Order.java 的 customer_name Column(贯通链)
    let column_id = EntityId::stable(
        "workspace",
        "Order.java",
        EntityKind::Column,
        "customer_name",
        "",
    );
    let report = ImpactAnalyzer::new(&store)
        .analyze(&ChangeRequest::rename_field(
            "customerName",
            "customerFullName",
        ))
        .unwrap();
    assert!(
        report
            .findings
            .iter()
            .any(|finding| finding.path.contains(&column_id)),
        "rename customerName 应在某 finding 的 path 里触达 customer_name Column"
    );
}

#[test]
fn dependency_graph_links_module_to_declared_dependencies() {
    // package.json → npm 模块 Package + 依赖;build.gradle.kts → gradle 模块 + 依赖。
    // 模块级依赖影响:以依赖为源反向遍历应触达声明它的模块。
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("package.json"),
        r#"{
          "name": "mos-websr",
          "dependencies": { "vue": "^3.5.4", "axios": "^1.12.0" },
          "devDependencies": { "vite": "^5.3.5" }
        }"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("build.gradle.kts"),
        r#"
        dependencies {
          implementation("com.foo:ccl-loan:1.0")
          implementation(libs.baz.qux)
        }
        "#,
    )
    .unwrap();

    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();

    let module = store
        .search(SearchQuery::new("mos-websr").with_limit(10))
        .unwrap()
        .into_iter()
        .find(|m| m.entity.kind == EntityKind::Package && m.entity.name == "mos-websr")
        .expect("npm module Package")
        .entity;
    let vue = store
        .search(SearchQuery::new("vue").with_limit(10))
        .unwrap()
        .into_iter()
        .find(|m| m.entity.kind == EntityKind::Package && m.entity.name == "vue")
        .expect("vue dependency Package")
        .entity;
    // 模块 --DependsOn--> vue:从 vue 反向遍历应到模块
    let vue_dependents = store
        .traverse(TraverseQuery {
            start: vue.id.clone(),
            outbound: false,
            max_depth: 1,
            edge_kinds: vec![EdgeKind::DependsOn],
        })
        .unwrap();
    assert!(
        vue_dependents.entities.iter().any(|e| e.id == module.id),
        "mos-websr 应 DependsOn vue"
    );
    // gradle 依赖:坐标版本剥离 + libs.xxx 别名引用
    assert!(
        store
            .search(SearchQuery::new("com.foo:ccl-loan").with_limit(10))
            .unwrap()
            .iter()
            .any(|m| m.entity.kind == EntityKind::Package)
    );
    assert!(
        store
            .search(SearchQuery::new("libs.baz.qux").with_limit(10))
            .unwrap()
            .iter()
            .any(|m| m.entity.kind == EntityKind::Package)
    );
    // 模块级依赖影响:以 vue 为源的影响分析应 inbound 触达 mos-websr 模块
    let report = ImpactAnalyzer::new(&store)
        .analyze(&ChangeRequest::rename_field("vue", "vue-next"))
        .unwrap();
    assert!(
        report
            .findings
            .iter()
            .any(|finding| finding.path.contains(&module.id)),
        "以 vue 为源的影响分析应 inbound 触达依赖它的 mos-websr 模块"
    );
}
