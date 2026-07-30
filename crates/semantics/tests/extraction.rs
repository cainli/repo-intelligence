//! semantics 提取器的集成测试(本 crate 首批测试)。
//! 直接构造 SourceFile 调 extract,断言产出的实体/边/evidence。
use std::path::PathBuf;

use repo_intelligence_model::{EdgeKind, EntityId, EntityKind, EvidenceClass};
use repo_intelligence_semantics::{extract, extract_with_config, SemanticsConfig};
use repo_intelligence_source::{FileKind, SourceFile};

fn java_file(name: &str, body: &str) -> SourceFile {
    SourceFile {
        id: EntityId::stable("workspace", name, EntityKind::File, name, ""),
        relative_path: PathBuf::from(name),
        kind: FileKind::Java,
        content_hash: "test".into(),
        content: body.to_string(),
    }
}

#[test]
fn mybatis_plus_explicit_is_fact_inferred_is_low_confidence() {
    // 显式 @TableField → Fact 1.0;无注解字段驼峰推断 → Inferred 0.7。
    let sf = java_file(
        "User.java",
        r#"
        @TableName("t_user")
        public class User {
          @TableField("user_name")
          private String userName;
          private Integer orderStatus;
        }
        "#,
    );
    let patch = extract(&sf).unwrap();

    let explicit = patch
        .add_entities
        .iter()
        .find(|e| e.kind == EntityKind::Column && e.name == "user_name")
        .expect("显式 user_name Column");
    let ev = explicit.evidence.first().unwrap();
    assert_eq!(ev.classification, EvidenceClass::Fact);
    assert!((ev.confidence - 1.0).abs() < 1e-6);

    let inferred = patch
        .add_entities
        .iter()
        .find(|e| e.kind == EntityKind::Column && e.name == "order_status")
        .expect("推断 order_status Column");
    let ev = inferred.evidence.first().unwrap();
    assert_eq!(ev.classification, EvidenceClass::Inferred);
    assert!((ev.confidence - 0.7).abs() < 1e-6);

    // Field --MappedFrom--> Column(显式配对边)
    let field_id = EntityId::stable("workspace", "User.java", EntityKind::Field, "userName", "");
    assert!(patch.add_edges.iter().any(|e| {
        e.kind == EdgeKind::MappedFrom && e.source == field_id && e.target == explicit.id
    }));
}

#[test]
fn camel_to_snake_via_inferred_column_names() {
    // 驼峰转下划线推断:userId→user_name? 不,userId→user_id;id→id。
    let sf = java_file(
        "E.java",
        r#"
        @TableName("t")
        class E {
          private String userId;
          private Long id;
        }
        "#,
    );
    let patch = extract(&sf).unwrap();
    let cols: Vec<&str> = patch
        .add_entities
        .iter()
        .filter(|e| e.kind == EntityKind::Column)
        .map(|e| e.name.as_str())
        .collect();
    assert!(cols.contains(&"user_id"), "userId → user_id, got {cols:?}");
    assert!(cols.contains(&"id"), "id → id, got {cols:?}");
}

#[test]
fn query_wrapper_column_reference_produces_inferred_reads_column() {
    // QueryWrapper .eq("col", …) → Column + File--ReadsColumn(Inferred 0.6)。
    let sf = java_file(
        "Q.java",
        r#"
        class Q {
          void run() {
            new QueryWrapper<Q>().eq("status", 1).like("name", "x");
          }
        }
        "#,
    );
    let patch = extract(&sf).unwrap();
    assert!(
        patch
            .add_entities
            .iter()
            .any(|e| e.kind == EntityKind::Column && e.name == "status"),
        "应产出 status Column"
    );
    let reads = patch
        .add_edges
        .iter()
        .filter(|e| e.kind == EdgeKind::ReadsColumn)
        .collect::<Vec<_>>();
    assert!(!reads.is_empty(), "应产出 ReadsColumn 边");
    // ReadsColumn 边置信度 0.6(Inferred)
    let ev = reads[0].evidence.first().expect("edge evidence");
    assert_eq!(ev.classification, EvidenceClass::Inferred);
    assert!((ev.confidence - 0.6).abs() < 1e-6);
}

#[test]
fn frontend_noise_properties_are_not_extracted_as_fields() {
    // VUE_BINDING 把任何 a.b 当 FrontendField;is_likely_field 守卫应滤掉噪声
    // (length/max 等),保留业务字段(customerName)。
    let sf = SourceFile {
        id: EntityId::stable("workspace", "page.vue", EntityKind::File, "page.vue", ""),
        relative_path: PathBuf::from("page.vue"),
        kind: FileKind::Vue,
        content_hash: "t".into(),
        content:
            r#"<template>{{ order.customerName }} {{ items.length }} {{ Math.max(x) }}</template>"#
                .to_string(),
    };
    let patch = extract(&sf).unwrap();
    assert!(
        patch
            .add_entities
            .iter()
            .any(|e| { e.kind == EntityKind::FrontendField && e.name == "customerName" }),
        "业务字段 customerName 应保留"
    );
    assert!(
        !patch
            .add_entities
            .iter()
            .any(|e| e.kind == EntityKind::FrontendField && e.name == "length"),
        "噪声 length 应被过滤"
    );
    assert!(
        !patch
            .add_entities
            .iter()
            .any(|e| e.kind == EntityKind::FrontendField && e.name == "max"),
        "噪声 max 应被过滤"
    );
}

#[test]
fn default_whitelist_covers_jpa_and_conditions_annotations() {
    // 默认白名单已覆盖 JPA 标识(@Entity)与 Spring Boot 装配条件
    // (@ConditionalOnProperty)——两者都应结构化为 Annotation + Annotated 边。
    let sf = java_file(
        "A.java",
        r#"
        @Entity
        public class A {
            @ConditionalOnProperty(name = "x")
            public void m() {}
        }
        "#,
    );
    let patch = extract(&sf).unwrap();
    let names: Vec<&str> = patch
        .add_entities
        .iter()
        .filter(|e| e.kind == EntityKind::Annotation)
        .map(|e| e.name.as_str())
        .collect();
    assert!(names.contains(&"Entity"), "@Entity 应结构化, got {names:?}");
    assert!(
        names.contains(&"ConditionalOnProperty"),
        "@ConditionalOnProperty 应结构化, got {names:?}"
    );
    assert!(
        patch.add_edges.iter().any(|e| e.kind == EdgeKind::Annotated),
        "应产出 Annotated 边"
    );
}

#[test]
fn blacklist_suppresses_annotation_even_when_whitelisted() {
    // 白名单同时含 Override,但黑名单也含 → blacklist 兜底必须挡住;
    // Transactional 仅在白名单 → 正常产出。证明 annotation_blacklist 已生效。
    let mut cfg = SemanticsConfig::default();
    cfg.annotation_whitelist = vec!["Transactional".into(), "Override".into()];
    cfg.annotation_blacklist = vec!["Override".into()];
    let sf = java_file(
        "B.java",
        r#"
        public class B {
            @Override
            public String toString() { return "b"; }
            @Transactional
            public void save() {}
        }
        "#,
    );
    let patch = extract_with_config(&sf, &cfg).unwrap();
    let names: Vec<&str> = patch
        .add_entities
        .iter()
        .filter(|e| e.kind == EntityKind::Annotation)
        .map(|e| e.name.as_str())
        .collect();
    assert!(
        !names.contains(&"Override"),
        "blacklist 应挡住 @Override, got {names:?}"
    );
    assert!(
        names.contains(&"Transactional"),
        "@Transactional 应产出, got {names:?}"
    );
}

#[test]
fn method_body_end_line_covers_full_body() {
    // 多行方法:metadata.body_end_line 应大于声明行(到闭合 })。
    // evidence.end_line 仍是声明行(不动 end_line 语义,避免破坏下游跨文件边行号)。
    let sf = java_file(
        "S.java",
        r#"
        class S {
            void run() {
                int a = 1;
                int b = 2;
                System.out.println(a + b);
            }
        }
        "#,
    );
    let patch = extract(&sf).unwrap();
    let m = patch
        .add_entities
        .iter()
        .find(|e| e.kind == EntityKind::Method && e.name == "run")
        .expect("method run");
    let ev = m.evidence.first().unwrap();
    assert_eq!(ev.start_line, ev.end_line, "evidence 仍是声明行");
    let body_end = m
        .metadata
        .get("body_end_line")
        .and_then(|v| v.as_u64())
        .expect("metadata.body_end_line 存在");
    assert!(
        body_end > ev.start_line as u64,
        "body_end_line {body_end} 应大于声明行 {}", ev.start_line
    );
}

#[test]
fn extract_extends_and_abstract_metadata() {
    // extends → metadata.superclass;abstract class → metadata.abstract=true。
    let sf = java_file(
        "Hier.java",
        r#"
        abstract class AbstractBase {
          abstract void doWork();
        }
        class Concrete extends AbstractBase {
          void doWork() {}
        }
        "#,
    );
    let patch = extract(&sf).unwrap();
    let base = patch
        .add_entities
        .iter()
        .find(|e| e.kind == EntityKind::Class && e.name == "AbstractBase")
        .expect("AbstractBase");
    let concrete = patch
        .add_entities
        .iter()
        .find(|e| e.kind == EntityKind::Class && e.name == "Concrete")
        .expect("Concrete");
    assert_eq!(
        base.metadata.get("abstract").and_then(|v| v.as_bool()),
        Some(true),
        "AbstractBase 应 metadata.abstract=true"
    );
    assert_eq!(
        concrete.metadata.get("superclass").and_then(|v| v.as_str()),
        Some("AbstractBase"),
        "Concrete 应 metadata.superclass=AbstractBase"
    );
    assert!(
        concrete.metadata.get("abstract").and_then(|v| v.as_bool()).is_none(),
        "Concrete 非 abstract,不应有 abstract metadata"
    );
}
