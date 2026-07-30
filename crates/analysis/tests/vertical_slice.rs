use std::fs;

use repo_intelligence_analysis::{ImpactAnalyzer, IndexerConfig, ScanPhase, WorkspaceIndexer};
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
    assert!(progress
        .iter()
        .any(|event| event.phase == ScanPhase::Parsing && event.total == 1));
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
    // Injects 方向:owner(OrderService) → bean(OrderRepo)。从 bean 反向遍历应到 owner。
    let owners = store
        .traverse(TraverseQuery {
            start: bean.id,
            outbound: false,
            max_depth: 1,
            edge_kinds: vec![EdgeKind::Injects],
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

#[test]
fn scan_with_config_honors_excluded_dirs_extra() {
    // excluded_dirs_extra 是追加语义:builtin 不含 "generated",配置后该目录被排除。
    // 这条端到端覆盖 analysis → discover 的 config 传递链。
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("Keep.java"), "class Keep {}").unwrap();
    fs::create_dir_all(dir.path().join("generated")).unwrap();
    fs::write(dir.path().join("generated/Gen.java"), "class Gen {}").unwrap();

    let mut cfg = IndexerConfig::default();
    cfg.discovery.excluded_dirs_extra.push("generated".into());

    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    WorkspaceIndexer
        .scan_with_config(dir.path(), &mut store, &cfg, |_| {})
        .unwrap();

    assert!(
        store
            .search(SearchQuery::new("Gen").with_limit(10))
            .unwrap()
            .is_empty(),
        "generated/ 在 excluded_dirs_extra,Gen 不应入库"
    );
    assert!(
        !store
            .search(SearchQuery::new("Keep").with_limit(10))
            .unwrap()
            .is_empty(),
        "Keep.java 不在排除目录,应入库"
    );
}

#[test]
fn scan_with_config_honors_custom_endpoint_replacement() {
    // custom_endpoint_annotations 是替换语义:配置只含 MyRpc 时,
    // builtin 的 @RmbMap 不再被识别,而 @MyRpc 会。
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("Svc.java"),
        r#"
        @RmbMap("builtinEndpoint")
        @MyRpc("customEndpoint")
        class Svc {}
        "#,
    )
    .unwrap();

    let mut cfg = IndexerConfig::default();
    cfg.semantics.custom_endpoint_annotations = vec!["MyRpc".into()];

    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    WorkspaceIndexer
        .scan_with_config(dir.path(), &mut store, &cfg, |_| {})
        .unwrap();

    let entities = store
        .search(SearchQuery::new("Endpoint").with_limit(20))
        .unwrap();
    let names: Vec<&str> = entities.iter().map(|m| m.entity.name.as_str()).collect();
    assert!(
        names.contains(&"customEndpoint"),
        "配置的 @MyRpc 应被识别为 endpoint"
    );
    assert!(
        !names.contains(&"builtinEndpoint"),
        "builtin @RmbMap 被替换语义移除,不应再被识别"
    );
}

#[test]
fn spring_autowired_constructor_injection_produces_injects() {
    // @Autowired 构造器:OrderService(OrderRepo) → Injects OrderRepo bean。
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("OrderService.java"),
        r#"
        class OrderService {
          private final OrderRepo repo;
          @Autowired
          public OrderService(OrderRepo repo) { this.repo = repo; }
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
        .find(|m| m.entity.kind == EntityKind::SpringBean)
        .expect("@Autowired 构造器应产出 OrderRepo SpringBean")
        .entity;
    let owners = store
        .traverse(TraverseQuery {
            start: bean.id,
            outbound: false,
            max_depth: 1,
            edge_kinds: vec![EdgeKind::Injects],
        })
        .unwrap();
    assert!(
        owners.entities.iter().any(|e| e.name == "OrderService"),
        "OrderService 应经构造器注入 Injects OrderRepo"
    );
}

#[test]
fn spring_single_constructor_injects_without_annotation() {
    // 无 @Autowired 的唯一构造器:Spring 默认自动注入。
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("OrderService.java"),
        r#"
        class OrderService {
          public OrderService(OrderRepo repo) {}
        }
        "#,
    )
    .unwrap();
    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();
    assert!(
        store
            .search(SearchQuery::new("OrderRepo").with_limit(10))
            .unwrap()
            .iter()
            .any(|m| m.entity.kind == EntityKind::SpringBean),
        "单构造器(无 @Autowired)应自动注入"
    );
}

#[test]
fn spring_multi_constructor_not_injected_without_autowired() {
    // 多构造器且无 @Autowired:不注入,避免过捕。
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("OrderService.java"),
        r#"
        class OrderService {
          public OrderService(OrderRepo repo) {}
          public OrderService(OrderRepo repo, int x) {}
        }
        "#,
    )
    .unwrap();
    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();
    assert!(
        !store
            .search(SearchQuery::new("OrderRepo").with_limit(10))
            .unwrap()
            .iter()
            .any(|m| m.entity.kind == EntityKind::SpringBean),
        "多构造器无 @Autowired 不应注入"
    );
}

#[test]
fn cross_file_basemapper_binds_to_entity_table() {
    // Order.java:@TableName 类;OrderMapper.java:BaseMapper<Order>(不同文件)。
    // resolve 应跨文件连 OrderMapper --DependsOn--> t_order。
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("Order.java"),
        r#"
        @TableName("t_order")
        public class Order {
          @TableField("customer_name")
          private String customerName;
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
    let mapper = store
        .search(SearchQuery::new("OrderMapper").with_limit(10))
        .unwrap()
        .into_iter()
        .find(|m| m.entity.kind == EntityKind::Mapper)
        .expect("OrderMapper")
        .entity;
    let outbound = store
        .traverse(
            TraverseQuery::outbound(mapper.id)
                .with_depth(1)
                .with_kinds(vec![EdgeKind::DependsOn]),
        )
        .unwrap();
    assert!(
        outbound
            .entities
            .iter()
            .any(|e| e.name == "t_order" && e.kind == EntityKind::Table),
        "跨文件 OrderMapper 应 DependsOn t_order"
    );
}

#[test]
fn mybatis_plus_lambda_wrapper_extracts_column() {
    // wrapper.eq(Order::getCustomerName, …):Lambda 方法引用 → customer_name Column。
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("Order.java"),
        r#"
        @TableName("t_order")
        public class Order {
          private String customerName;
          public String getCustomerName() { return customerName; }
        }
        "#,
    )
    .unwrap();
    fs::write(
        dir.path().join("OrderRepo.java"),
        r#"
        class OrderRepo {
          public Object find() {
            return wrapper.eq(Order::getCustomerName, "x").list();
          }
        }
        "#,
    )
    .unwrap();
    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();
    assert!(
        store
            .search(SearchQuery::new("customer_name").with_limit(20))
            .unwrap()
            .iter()
            .any(|m| m.entity.kind == EntityKind::Column),
        "Lambda Order::getCustomerName 应产出 customer_name Column"
    );
}

#[test]
fn incremental_scan_reuses_unchanged_extracts() {
    // 未变文件跳过:第二次 scan files_extracted=0,且图中实体数不变(file_state 增量)。
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("A.java"), "class A { private String name; }").unwrap();
    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    let first = WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();
    assert_eq!(first.files_extracted, first.files_indexed, "首次全量 extract");
    let (entities_after_first, _) = store.counts().unwrap();
    let second = WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();
    assert_eq!(second.files_extracted, 0, "未变文件跳过,extract=0");
    assert_eq!(second.files_unchanged, 1, "unchanged 计数=1");
    let (entities_after_second, _) = store.counts().unwrap();
    assert_eq!(
        entities_after_second, entities_after_first,
        "未变文件,图中实体数不变"
    );
}

#[test]
fn incremental_scan_reextracts_changed_file() {
    // 变化文件重 extract:content_hash 变 → 缓存失效 → files_extracted=1 + 新字段入库。
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("A.java");
    fs::write(&a, "class A { private String name; }").unwrap();
    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();
    fs::write(&a, "class A { private String name; private String added; }").unwrap();
    let second = WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();
    assert_eq!(second.files_extracted, 1, "变化的文件重 extract");
    assert!(
        store
            .search(SearchQuery::new("added").with_limit(10))
            .unwrap()
            .iter()
            .any(|m| m.entity.kind == EntityKind::Field),
        "新字段入库"
    );
}

#[test]
fn incremental_scan_removes_deleted_file_entities() {
    // 删除文件:第二次 scan files_deleted=1,该文件实体从图中消失,其余不变。
    let dir = tempfile::tempdir().unwrap();
    let b = dir.path().join("B.java");
    fs::write(dir.path().join("A.java"), "class A { private String name; }").unwrap();
    fs::write(&b, "class B { private String title; }").unwrap();
    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();
    assert!(
        store
            .search(SearchQuery::new("title").with_limit(10))
            .unwrap()
            .iter()
            .any(|m| m.entity.kind == EntityKind::Field),
        "首次扫描 B 的 title 字段入库"
    );
    let (entities_before, _) = store.counts().unwrap();

    fs::remove_file(&b).unwrap();
    let second = WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();
    assert_eq!(second.files_deleted, 1, "B 被删除");
    assert!(
        !store
            .search(SearchQuery::new("title").with_limit(10))
            .unwrap()
            .iter()
            .any(|m| m.entity.kind == EntityKind::Field),
        "B 删除后 title 字段消失"
    );
    assert!(
        store
            .search(SearchQuery::new("name").with_limit(10))
            .unwrap()
            .iter()
            .any(|m| m.entity.kind == EntityKind::Field),
        "A 的 name 字段仍在"
    );
    let (entities_after, _) = store.counts().unwrap();
    assert!(entities_after < entities_before, "删除后实体数减少");
}

#[test]
fn extract_fills_snippet_from_source_line() {
    // scan 后实体的 evidence 应带 snippet(对应源码行),让 agent 不必 Read 即可判断证据。
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("A.java"),
        "class A {\n  private String name;\n}\n",
    )
    .unwrap();
    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();
    let entity = store
        .search(SearchQuery::new("name").with_limit(10))
        .unwrap()
        .into_iter()
        .find(|m| m.entity.kind == EntityKind::Field)
        .expect("name field indexed");
    let snippet = entity
        .entity
        .evidence
        .first()
        .and_then(|e| e.snippet.as_deref())
        .expect("evidence should carry a snippet");
    assert!(
        snippet.contains("private String name"),
        "snippet should be the source line, got: {snippet}"
    );
}

#[test]
fn method_declarations_and_same_file_calls_extracted() {
    // methodA→methodB→methodC:产出 Method 实体 + 同文件 Calls 边。
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("Svc.java"),
        r#"
        class Svc {
          public void methodA() { methodB(); }
          public void methodB() { methodC(); }
          public void methodC() {}
        }
        "#,
    )
    .unwrap();
    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();
    let a = store
        .search(SearchQuery::new("methodA").with_limit(10))
        .unwrap()
        .into_iter()
        .find(|m| m.entity.kind == EntityKind::Method)
        .expect("Method methodA")
        .entity;
    let chain = store
        .traverse(
            TraverseQuery::outbound(a.id)
                .with_depth(2)
                .with_kinds(vec![EdgeKind::Calls]),
        )
        .unwrap();
    let names: Vec<&str> = chain.entities.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"methodB"), "methodA 调用 methodB");
    assert!(names.contains(&"methodC"), "调用链达 methodC");
}

#[test]
fn implements_custom_interface_becomes_endpoint() {
    // 回归 P1-4:implements ApiHandler(自研 RMB 入口模式)→ HttpEndpoint,name=类名(交易码)。
    // mes/mos 的 RMB 入口用 `@MosApi + implements ApiHandler`,纯注解识别覆盖不到。
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("S27204.java"),
        r#"
        class S27204 implements ApiHandler<S27204Req, Object> {
          public Object handle(S27204Req req) { return null; }
        }
        "#,
    )
    .unwrap();
    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();
    let endpoint = store
        .search(SearchQuery::new("S27204").with_limit(20))
        .unwrap()
        .into_iter()
        .find(|m| m.entity.kind == EntityKind::HttpEndpoint)
        .expect("implements ApiHandler 应产出 HttpEndpoint");
    assert_eq!(endpoint.entity.name, "S27204");

    // P1-4 补全:入口方法 handle() 经 Exposes 连到 endpoint,让 relay 从入口追到处理逻辑。
    let handle = store
        .search(SearchQuery::new("handle").with_limit(20))
        .unwrap()
        .into_iter()
        .find(|m| m.entity.kind == EntityKind::Method && m.entity.name == "handle")
        .expect("handle() method")
        .entity;
    let exposed = store
        .traverse(
            TraverseQuery::outbound(handle.id)
                .with_depth(1)
                .with_kinds(vec![EdgeKind::Exposes]),
        )
        .unwrap();
    assert!(
        exposed
            .entities
            .iter()
            .any(|e| e.kind == EntityKind::HttpEndpoint && e.name == "S27204"),
        "handle() 应经 Exposes 连到 S27204 endpoint: {exposed:?}"
    );
}

#[test]
fn method_level_request_mapping_wildcard_match() {
    // P0-1a:方法级 @RequestMapping("/foo")(无动词)→ 产 endpoint(ANY /foo,method 通配);
    // 前端 POST /foo 经 method 通配 + path 精确匹配产生 matches_endpoint 边(Inferred 低置信)。
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("Ctl.java"),
        r#"
        @RestController
        class Ctl {
          @RequestMapping("/foo")
          String handle() { return ""; }
        }
        "#,
    )
    .unwrap();
    fs::write(dir.path().join("p.vue"), r#"<script>axios.post("/foo")</script>"#).unwrap();
    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();
    let call = store
        .search(SearchQuery::new("POST /foo").with_limit(10))
        .unwrap()
        .into_iter()
        .find(|m| m.entity.kind == EntityKind::HttpClientCall)
        .expect("http_client_call POST /foo")
        .entity;
    let t = store
        .traverse(
            TraverseQuery::outbound(call.id)
                .with_depth(1)
                .with_kinds(vec![EdgeKind::MatchesEndpoint]),
        )
        .unwrap();
    let edge = t
        .edges
        .iter()
        .find(|e| e.kind == EdgeKind::MatchesEndpoint)
        .expect("matches_endpoint via method wildcard");
    let ev = edge.evidence.first().expect("evidence");
    assert_eq!(ev.classification, EvidenceClass::Inferred);
    assert!(
        ev.confidence < 0.6,
        "通配匹配应低置信, got {}",
        ev.confidence
    );
}

#[test]
fn segment_suffix_align_rejects_long_tail() {
    // P0-1b:段数差 >3 的长尾不连(/foo 不应误连 /a/b/c/d/foo)。
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("Ctl.java"),
        r#"
        @RestController
        class Ctl {
          @GetMapping("/a/b/c/d/foo")
          String handle() { return ""; }
        }
        "#,
    )
    .unwrap();
    fs::write(dir.path().join("p.vue"), r#"<script>axios.get("/foo")</script>"#).unwrap();
    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();
    let call = store
        .search(SearchQuery::new("GET /foo").with_limit(10))
        .unwrap()
        .into_iter()
        .find(|m| m.entity.kind == EntityKind::HttpClientCall)
        .expect("http_client_call")
        .entity;
    let t = store
        .traverse(
            TraverseQuery::outbound(call.id)
                .with_depth(1)
                .with_kinds(vec![EdgeKind::MatchesEndpoint]),
        )
        .unwrap();
    assert!(t.edges.is_empty(), "段数差>3 的长尾不应连, got {:?}", t.edges);
}

#[test]
fn test_case_and_tests_edge() {
    // P1-4:@Test → TestCase 实体;FooTest → Foo Tests 边(命名约定)。
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("Foo.java"), "class Foo { void bar() {} }").unwrap();
    fs::write(
        dir.path().join("FooTest.java"),
        r#"
        class FooTest {
          @Test
          void testBar() {}
        }
        "#,
    )
    .unwrap();
    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();
    store
        .search(SearchQuery::new("testBar").with_limit(20))
        .unwrap()
        .iter()
        .find(|m| m.entity.kind == EntityKind::TestCase)
        .expect("TestCase from @Test");
    let test_class = store
        .search(SearchQuery::new("FooTest").with_limit(20))
        .unwrap()
        .into_iter()
        .find(|m| m.entity.kind == EntityKind::Class && m.entity.name == "FooTest")
        .expect("FooTest class")
        .entity;
    let t = store
        .traverse(
            TraverseQuery::outbound(test_class.id)
                .with_depth(1)
                .with_kinds(vec![EdgeKind::Tests]),
        )
        .unwrap();
    assert!(
        t.entities
            .iter()
            .any(|e| e.kind == EntityKind::Class && e.name == "Foo"),
        "FooTest 应经 Tests 连到 Foo: {:?}",
        t.entities
    );
}

#[test]
fn scheduled_job_with_schedules_edge() {
    // P1-6:@Scheduled 方法 → Job 实体 + Job-[Schedules]->handler。
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("Batch.java"),
        r#"
        class Batch {
          @Scheduled(cron = "0 * * * * *")
          public void runJob() {}
        }
        "#,
    )
    .unwrap();
    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();
    let job = store
        .search(SearchQuery::new("runJob").with_limit(20))
        .unwrap()
        .into_iter()
        .find(|m| m.entity.kind == EntityKind::Job)
        .expect("Job from @Scheduled")
        .entity;
    let t = store
        .traverse(
            TraverseQuery::outbound(job.id)
                .with_depth(1)
                .with_kinds(vec![EdgeKind::Schedules]),
        )
        .unwrap();
    assert!(
        t.entities
            .iter()
            .any(|e| e.kind == EntityKind::Method && e.name == "runJob"),
        "Job 应经 Schedules 连到 handler: {:?}",
        t.entities
    );
}

#[test]
fn transactional_annotation_entity_and_edge() {
    // P1-1:@Transactional(白名单)→ Annotation 实体 + class-[Annotated]->annotation。
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("Svc.java"),
        r#"
        @Transactional
        class Svc { void op() {} }
        "#,
    )
    .unwrap();
    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();
    let svc = store
        .search(SearchQuery::new("Svc").with_limit(20))
        .unwrap()
        .into_iter()
        .find(|m| m.entity.kind == EntityKind::Class && m.entity.name == "Svc")
        .expect("Svc class")
        .entity;
    let t = store
        .traverse(
            TraverseQuery::outbound(svc.id)
                .with_depth(1)
                .with_kinds(vec![EdgeKind::Annotated]),
        )
        .unwrap();
    assert!(
        t.entities
            .iter()
            .any(|e| e.kind == EntityKind::Annotation && e.name == "Transactional"),
        "Svc 应经 Annotated 连到 Transactional: {:?}",
        t.entities
    );
}

#[test]
fn xml_insert_produces_writes_column_edge() {
    // P1-3:insert 列 → writes_column 边(statement-[WritesColumn]->column)。
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("M.xml"),
        r#"<mapper namespace="M">
          <insert id="addRow">
            INSERT INTO t (col_a, col_b) VALUES (#{a}, #{b})
          </insert>
        </mapper>"#,
    )
    .unwrap();
    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();
    let stmt = store
        .search(SearchQuery::new("addRow").with_limit(20))
        .unwrap()
        .into_iter()
        .find(|m| m.entity.kind == EntityKind::XmlStatement)
        .expect("XmlStatement addRow")
        .entity;
    let t = store
        .traverse(
            TraverseQuery::outbound(stmt.id)
                .with_depth(1)
                .with_kinds(vec![EdgeKind::WritesColumn]),
        )
        .unwrap();
    assert!(
        t.entities
            .iter()
            .any(|e| e.kind == EntityKind::Column && e.name == "col_a"),
        "insert 应经 WritesColumn 连到 col_a: {:?}",
        t.entities
    );
}

#[test]
fn frontend_constant_url_call() {
    // P0-1c:const URL='/x' 后 get(URL) → http_client_call(PATH /x)。
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("p.vue"),
        r#"<script>
        const FETCH_URL = "/api/x";
        axios.get(FETCH_URL);
        </script>"#,
    )
    .unwrap();
    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();
    let call = store
        .search(SearchQuery::new("GET /api/x").with_limit(20))
        .unwrap()
        .into_iter()
        .find(|m| m.entity.kind == EntityKind::HttpClientCall)
        .expect("http_client_call from constant URL")
        .entity;
    assert!(call.name.contains("/api/x"), "name={}", call.name);
}

#[test]
fn cross_file_static_call_on_named_type() {
    // Step A:JsonUtil.stringify(x) —— receiver 为类型名 JsonUtil → 跨文件 Calls 连到
    // JsonUtil.stringify(0.7)。注入型匹配覆盖不到静态工具调用,这是 classify_receiver 新增能力。
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("OrderService.java"),
        r#"
        class OrderService {
          void m(Object x) { String s = JsonUtil.stringify(x); }
        }
        "#,
    )
    .unwrap();
    fs::write(
        dir.path().join("JsonUtil.java"),
        r#"
        class JsonUtil {
          static String stringify(Object o) { return String.valueOf(o); }
        }
        "#,
    )
    .unwrap();
    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();
    let m = store
        .search(SearchQuery::new("m").with_limit(100))
        .unwrap()
        .into_iter()
        .find(|x| x.entity.kind == EntityKind::Method && x.entity.name == "m")
        .expect("method m");
    let chain = store
        .traverse(
            TraverseQuery::outbound(m.entity.id.clone())
                .with_kinds(vec![EdgeKind::Calls]),
        )
        .unwrap();
    let names: Vec<&str> = chain.entities.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"stringify"),
        "静态调用 JsonUtil.stringify 应跨文件连上, got {names:?}"
    );
}

#[test]
fn chain_and_new_receivers_do_not_create_cross_file_calls() {
    // Step A 安全降级:new Builder().build()(object_creation_expression)、
    // getCache().put()(method_invocation 链式)接收者无法可靠解析 → 不建跨文件边。
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("Svc.java"),
        r#"
        class Svc {
          void m() {
            new Builder().build();
            getCache().put();
          }
        }
        "#,
    )
    .unwrap();
    fs::write(
        dir.path().join("Builder.java"),
        r#"class Builder { void build() {} }"#,
    )
    .unwrap();
    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();
    let m = store
        .search(SearchQuery::new("m").with_limit(100))
        .unwrap()
        .into_iter()
        .find(|x| x.entity.kind == EntityKind::Method && x.entity.name == "m")
        .expect("method m");
    let chain = store
        .traverse(
            TraverseQuery::outbound(m.entity.id.clone())
                .with_kinds(vec![EdgeKind::Calls]),
        )
        .unwrap();
    let names: Vec<&str> = chain.entities.iter().map(|e| e.name.as_str()).collect();
    assert!(
        !names.contains(&"build"),
        "new Builder().build() 不应跨文件连上, got {names:?}"
    );
    assert!(
        !names.contains(&"put"),
        "getCache().put() 不应跨文件连上, got {names:?}"
    );
}

#[test]
fn injected_field_call_reaches_injected_type_method() {
    // Step B 基本场景:OrderController 构造器注入 service(OrderService),this.service.getOrder()
    // → OrderService.getOrder(字段名 service 经 injected_fields 精确解析到注入类型)。
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("OrderController.java"),
        r#"
        class OrderController {
          private final OrderService service;
          OrderController(OrderService service) { this.service = service; }
          Object get() { return this.service.getOrder(); }
        }
        "#,
    )
    .unwrap();
    fs::write(
        dir.path().join("OrderService.java"),
        r#"class OrderService { Object getOrder() { return null; } }"#,
    )
    .unwrap();
    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();
    let get = store
        .search(SearchQuery::new("get").with_limit(100))
        .unwrap()
        .into_iter()
        .find(|x| x.entity.kind == EntityKind::Method && x.entity.name == "get")
        .expect("method get");
    let chain = store
        .traverse(
            TraverseQuery::outbound(get.entity.id.clone()).with_kinds(vec![EdgeKind::Calls]),
        )
        .unwrap();
    let names: Vec<&str> = chain.entities.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"getOrder"),
        "this.service.getOrder 应跨文件连上, got {names:?}"
    );
}

#[test]
fn injected_field_call_disambiguates_same_named_methods() {
    // Step B 歧义消歧:Ctrl 注入 a(ServiceA)、b(ServiceB),两者都有 save()。
    // this.a.save() → ServiceA.save,this.b.save() → ServiceB.save。旧逻辑无字段名,
    // 多 type 同名方法 hits.len()==2 被一票否决全丢;字段名精确锁定后两条边都建上。
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("Ctrl.java"),
        r#"
        class Ctrl {
          private final ServiceA a;
          private final ServiceB b;
          Ctrl(ServiceA a, ServiceB b) { this.a = a; this.b = b; }
          void go() { this.a.save(); this.b.save(); }
        }
        "#,
    )
    .unwrap();
    fs::write(
        dir.path().join("ServiceA.java"),
        r#"class ServiceA { void save() {} }"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("ServiceB.java"),
        r#"class ServiceB { void save() {} }"#,
    )
    .unwrap();
    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();
    let go = store
        .search(SearchQuery::new("go").with_limit(100))
        .unwrap()
        .into_iter()
        .find(|x| x.entity.kind == EntityKind::Method && x.entity.name == "go")
        .expect("method go");
    let chain = store
        .traverse(
            TraverseQuery::outbound(go.entity.id.clone())
                .with_depth(2)
                .with_kinds(vec![EdgeKind::Calls]),
        )
        .unwrap();
    let save_count = chain.entities.iter().filter(|e| e.name == "save").count();
    assert!(
        save_count >= 2,
        "注入字段应分别连上 ServiceA.save 与 ServiceB.save, got {save_count} 个 save"
    );
}

#[test]
fn injected_field_call_without_explicit_this_resolves_via_field() {
    // Java 主流写法:省略 this 的 service.foo()(receiver=identifier → name kind)。
    // name 优先当注入字段试,精确解析到注入类型 0.7(覆盖绝大多数注入调用,不止显式 this)。
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("OrderController.java"),
        r#"
        class OrderController {
          private final OrderService service;
          OrderController(OrderService service) { this.service = service; }
          Object get() { return service.getOrder(); }
        }
        "#,
    )
    .unwrap();
    fs::write(
        dir.path().join("OrderService.java"),
        r#"class OrderService { Object getOrder() { return null; } }"#,
    )
    .unwrap();
    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();
    let get = store
        .search(SearchQuery::new("get").with_limit(100))
        .unwrap()
        .into_iter()
        .find(|x| x.entity.kind == EntityKind::Method && x.entity.name == "get")
        .expect("method get");
    let chain = store
        .traverse(
            TraverseQuery::outbound(get.entity.id.clone()).with_kinds(vec![EdgeKind::Calls]),
        )
        .unwrap();
    let names: Vec<&str> = chain.entities.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"getOrder"),
        "service.getOrder()(省略 this)应经字段解析连上, got {names:?}"
    );
}

#[test]
fn native_mybatis_binds_method_to_statement_to_table() {
    // 原生 MyBatis(无 @TableName/BaseMapper):Dao 接口方法经 namespace + id 绑定到
    // Mapper.xml 的 statement,statement→ReadsTable/WritesTable→table。补 BindsToStatement
    // 边前,method↔statement 断链,trace 到 table 必 0 命中(MES/MOS 场景)。
    // namespace 后缀匹配 interface 的物理 path,故需搭出 Maven 包目录结构。
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("src/main/java/com/example/dao");
    fs::create_dir_all(&pkg).unwrap();
    fs::write(
        pkg.join("UserDao.java"),
        r#"package com.example.dao;
        public interface UserDao {
          Object findById(Long id);
          int insert(Object u);
        }"#,
    )
    .unwrap();
    fs::create_dir_all(dir.path().join("src/main/resources/mapper")).unwrap();
    fs::write(
        dir.path().join("src/main/resources/mapper/UserDao.xml"),
        r#"<mapper namespace="com.example.dao.UserDao">
        <select id="findById" resultType="User">
          SELECT id, name FROM t_user WHERE id = #{id}
        </select>
        <insert id="insert">
          INSERT INTO t_user (id, name) VALUES (#{id}, #{name})
        </insert>
        </mapper>"#,
    )
    .unwrap();
    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();

    // findById method 经 BindsToStatement 直达 statement,再 ReadsTable 到 t_user。
    let find_method = store
        .search(SearchQuery::new("findById").with_limit(20))
        .unwrap()
        .into_iter()
        .find(|m| m.entity.kind == EntityKind::Method)
        .expect("UserDao.findById method")
        .entity;
    let chain = store
        .traverse(
            TraverseQuery::outbound(find_method.id)
                .with_depth(3)
                .with_kinds(vec![EdgeKind::BindsToStatement, EdgeKind::ReadsTable]),
        )
        .unwrap();
    assert!(
        chain
            .entities
            .iter()
            .any(|e| e.kind == EntityKind::XmlStatement && e.name == "findById"),
        "findById method 应 BindsToStatement 到 statement findById, got {:?}",
        chain.entities
    );
    assert!(
        chain
            .entities
            .iter()
            .any(|e| e.kind == EntityKind::Table && e.name == "t_user"),
        "findById → statement → ReadsTable → t_user 应连通, got {:?}",
        chain.entities
    );

    // insert method 走 WritesTable 抵达 t_user(写侧对照)。
    let insert_method = store
        .search(SearchQuery::new("insert").with_limit(20))
        .unwrap()
        .into_iter()
        .find(|m| m.entity.kind == EntityKind::Method)
        .expect("UserDao.insert method")
        .entity;
    let write_chain = store
        .traverse(
            TraverseQuery::outbound(insert_method.id)
                .with_depth(3)
                .with_kinds(vec![EdgeKind::BindsToStatement, EdgeKind::WritesTable]),
        )
        .unwrap();
    assert!(
        write_chain
            .entities
            .iter()
            .any(|e| e.name == "t_user"),
        "insert → statement → WritesTable → t_user 应连通, got {:?}",
        write_chain.entities
    );
}

#[test]
fn superclass_of_edge_links_superclass_to_subclass() {
    // class Concrete extends AbstractBase → SuperclassOf(AbstractBase→Concrete,outbound)。
    // trace 从 abstract 基类出发应下钻到具体子类(业务逻辑常在子类)。
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("Hier.java"),
        r#"
        abstract class AbstractBase {
          void shared() {}
        }
        class Concrete extends AbstractBase {
          void doWork() {}
        }
        "#,
    )
    .unwrap();
    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    WorkspaceIndexer.scan(dir.path(), &mut store).unwrap();
    let base = store
        .search(SearchQuery::new("AbstractBase").with_limit(20))
        .unwrap()
        .into_iter()
        .find(|m| m.entity.kind == EntityKind::Class)
        .expect("AbstractBase")
        .entity;
    let chain = store
        .traverse(
            TraverseQuery::outbound(base.id.clone())
                .with_kinds(vec![EdgeKind::SuperclassOf]),
        )
        .unwrap();
    let names: Vec<&str> = chain.entities.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"Concrete"),
        "AbstractBase 经 SuperclassOf 应到 Concrete, got {names:?}"
    );
}
