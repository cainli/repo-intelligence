use repo_intelligence_model::{
    ChangeOperation, ChangeRequest, Entity, EntityId, EntityKind, EvidenceClass,
};

#[test]
fn entity_ids_are_stable_and_sensitive_to_semantic_identity() {
    let first = EntityId::stable(
        "repo",
        "src/Order.java",
        EntityKind::Field,
        "Order.name",
        "",
    );
    let second = EntityId::stable(
        "repo",
        "src/Order.java",
        EntityKind::Field,
        "Order.name",
        "",
    );
    let other = EntityId::stable("repo", "src/Order.java", EntityKind::Field, "Order.id", "");
    assert_eq!(first, second);
    assert_ne!(first, other);
}

#[test]
fn change_request_models_a_field_rename() {
    let change = ChangeRequest::rename_field("customerName", "customerFullName");
    assert_eq!(change.operation, ChangeOperation::Rename);
    assert_eq!(change.from.as_deref(), Some("customerName"));
    assert_eq!(change.to.as_deref(), Some("customerFullName"));
}

#[test]
fn entity_can_carry_direct_evidence() {
    let entity = Entity::new(
        EntityId::stable("repo", "A.java", EntityKind::Field, "A.value", ""),
        EntityKind::Field,
        "value",
        "A.value",
    )
    .with_evidence(
        "A.java",
        4,
        4,
        EvidenceClass::Fact,
        1.0,
        "field declaration",
    );
    assert_eq!(entity.evidence.len(), 1);
}
