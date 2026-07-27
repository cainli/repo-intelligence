use repo_intelligence_graph::{GraphStore, SqliteGraphStore};
use repo_intelligence_model::{
    Edge, EdgeKind, Entity, EntityId, EntityKind, EvidenceClass, GraphPatch, SearchQuery,
    TraverseQuery,
};
use std::time::{Duration, Instant};

fn entity(kind: EntityKind, name: &str) -> Entity {
    Entity::new(
        EntityId::stable("repo", name, kind, name, ""),
        kind,
        name,
        name,
    )
}

#[test]
fn replaces_a_large_snapshot_without_per_entity_fts_deletes() {
    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    let old_entities = (0..5_000)
        .map(|index| entity(EntityKind::Field, &format!("old_field_{index}")))
        .collect();
    store
        .replace_snapshot(GraphPatch::add(old_entities, vec![]))
        .unwrap();

    let replacement = entity(EntityKind::Field, "replacement");
    let started = Instant::now();
    store
        .replace_snapshot(GraphPatch::add(vec![replacement.clone()], vec![]))
        .unwrap();

    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(
        store
            .search(SearchQuery::new("old_field_"))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .search(SearchQuery::new("replacement"))
            .unwrap()
            .first()
            .unwrap()
            .entity
            .id,
        replacement.id
    );
}

#[test]
fn applies_patch_searches_and_traverses() {
    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    let page = entity(EntityKind::VuePage, "OrderPage");
    let field = entity(EntityKind::FrontendField, "customerName");
    let edge = Edge::new(page.id.clone(), field.id.clone(), EdgeKind::Contains).with_evidence(
        "OrderPage.vue",
        3,
        3,
        EvidenceClass::Fact,
        1.0,
        "template binding",
    );
    store
        .apply_patch(GraphPatch::add(
            vec![page.clone(), field.clone()],
            vec![edge],
        ))
        .unwrap();

    let matches = store
        .search(SearchQuery::new("customerName").with_limit(10))
        .unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].entity.id, field.id);

    let traversal = store
        .traverse(TraverseQuery::outbound(page.id).with_depth(2))
        .unwrap();
    assert_eq!(traversal.entities.len(), 2);
    assert_eq!(traversal.edges.len(), 1);
}

#[test]
fn traverse_filters_by_edge_kind() {
    let mut store = SqliteGraphStore::open_in_memory().unwrap();
    let alpha = entity(EntityKind::Method, "Alpha");
    let beta = entity(EntityKind::Method, "Beta");
    let gamma = entity(EntityKind::Method, "Gamma");
    // Reached only via a Contains edge — must drop out when filtering to Calls.
    let nested = entity(EntityKind::Class, "Nested");
    let edges = vec![
        Edge::new(alpha.id.clone(), beta.id.clone(), EdgeKind::Calls),
        Edge::new(beta.id.clone(), gamma.id.clone(), EdgeKind::Calls),
        Edge::new(alpha.id.clone(), nested.id.clone(), EdgeKind::Contains),
    ];
    store
        .apply_patch(GraphPatch::add(
            vec![alpha.clone(), beta, gamma, nested],
            edges,
        ))
        .unwrap();

    // No filter: every edge kind is walked, so Nested is reached via Contains.
    let all = store
        .traverse(TraverseQuery::outbound(alpha.id.clone()).with_depth(2))
        .unwrap();
    assert_eq!(all.entities.len(), 4);
    assert_eq!(all.edges.len(), 3);

    // Filtered to Calls: Nested (reached only via Contains) drops out, and the
    // depth budget is spent on the call chain alone. `entities` comes back in
    // HashMap-iteration order, so sort before asserting membership.
    let calls_only = store
        .traverse(
            TraverseQuery::outbound(alpha.id.clone())
                .with_depth(2)
                .with_kinds(vec![EdgeKind::Calls]),
        )
        .unwrap();
    let mut names: Vec<_> = calls_only
        .entities
        .iter()
        .map(|entity| entity.name.as_str())
        .collect();
    names.sort_unstable();
    assert_eq!(names, vec!["Alpha", "Beta", "Gamma"]);
    assert_eq!(calls_only.edges.len(), 2);
    assert!(calls_only
        .edges
        .iter()
        .all(|edge| edge.kind == EdgeKind::Calls));
}
