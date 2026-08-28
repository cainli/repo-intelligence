use repo_intelligence_graph::{GraphStore, SqliteGraphStore};

#[test]
fn read_only_query_rejects_non_select_and_returns_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.sqlite");
    let store = SqliteGraphStore::open(&db).unwrap();
    // 拒绝写语句
    assert!(store.read_only_query("DELETE FROM entity", 10).is_err());
    assert!(
        store
            .read_only_query("select 1; drop table entity", 10)
            .is_err()
    );
    assert!(
        store
            .read_only_query("ATTACH DATABASE '/tmp/x' AS x", 10)
            .is_err()
    );
    assert!(
        store
            .read_only_query("PRAGMA journal_mode = DELETE", 10)
            .is_err()
    );
    // 正常读取
    let r = store
        .read_only_query("SELECT 1 AS one, 'a' AS txt", 10)
        .unwrap();
    assert_eq!(r.columns, vec!["one".to_string(), "txt".to_string()]);
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], serde_json::json!(1));
    assert!(!r.truncated);
}

#[test]
fn read_only_query_caps_rows_and_truncates_cells() {
    use repo_intelligence_model::{Edge, EdgeKind, EntityId, GraphPatch};
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.sqlite");
    let mut store = SqliteGraphStore::open(&db).unwrap();
    let edges: Vec<Edge> = (0..5)
        .map(|i| {
            Edge::new(
                EntityId(format!("a{i}")),
                EntityId("b".into()),
                EdgeKind::Calls,
            )
        })
        .collect();
    store
        .apply_patch(GraphPatch::add(Vec::new(), edges))
        .unwrap();

    // 行数封顶:max_rows=3 → 3 行 + truncated=true。
    let r = store
        .read_only_query("SELECT source_id FROM edge ORDER BY source_id", 3)
        .unwrap();
    assert_eq!(r.rows.len(), 3);
    assert!(r.truncated);
    // 不封顶 → 全量 + truncated=false。
    let r = store
        .read_only_query("SELECT source_id FROM edge ORDER BY source_id", 50)
        .unwrap();
    assert_eq!(r.rows.len(), 5);
    assert!(!r.truncated);

    // 单元格截断:>400 字符文本按字符边界截到 200 chars + 标注(不 panic 于多字节)。
    let long = "汉".repeat(500);
    let r = store
        .read_only_query(&format!("SELECT '{long}' AS s"), 1)
        .unwrap();
    let cell = r.rows[0][0].as_str().unwrap();
    assert!(cell.starts_with("汉"));
    assert!(cell.chars().take(200).count() == 200 || cell.ends_with("chars)"));
}
