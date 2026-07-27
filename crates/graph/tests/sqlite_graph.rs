use repo_intelligence_graph::{GraphStore, SqliteGraphStore};
use repo_intelligence_model::{
    Edge, EdgeKind, Entity, EntityId, EntityKind, EvidenceClass, GraphPatch, SearchQuery,
    TraverseQuery,
};

fn entity(kind: EntityKind, name: &str) -> Entity {
    Entity::new(
        EntityId::stable("repo", name, kind, name, ""),
        kind,
        name,
        name,
    )
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
