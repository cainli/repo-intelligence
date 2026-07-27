//! semantics 提取器的集成测试(本 crate 首批测试)。
//! 直接构造 SourceFile 调 extract,断言产出的实体/边/evidence。
use std::path::PathBuf;

use repo_intelligence_model::{EdgeKind, EntityId, EntityKind, EvidenceClass};
use repo_intelligence_semantics::extract;
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
