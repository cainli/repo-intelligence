use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

use anyhow::{Context, Result};
use repo_intelligence_model::{Edge, Entity, EntityId, GraphPatch, SearchQuery, TraverseQuery};
use rusqlite::{Connection, OptionalExtension, params};

#[derive(Clone, Debug)]
pub struct EntityMatch {
    pub entity: Entity,
    pub score: f32,
}

#[derive(Clone, Debug, Default)]
pub struct Traversal {
    pub entities: Vec<Entity>,
    pub edges: Vec<Edge>,
}

pub trait GraphStore {
    fn apply_patch(&mut self, patch: GraphPatch) -> Result<()>;
    fn replace_snapshot(&mut self, patch: GraphPatch) -> Result<()>;
    fn search(&self, query: SearchQuery) -> Result<Vec<EntityMatch>>;
    /// 精确名匹配(大小写敏感,走 entity_name 索引)。analyze 要 name == source_name,
    /// 不是子串——用这个替代 search 的 LIKE %text% 省内存。
    fn search_exact_name(&self, name: &str, limit: usize) -> Result<Vec<Entity>>;
    fn traverse(&self, query: TraverseQuery) -> Result<Traversal>;
    fn get_entity(&self, id: &EntityId) -> Result<Option<Entity>>;
    fn all_entities(&self) -> Result<Vec<Entity>>;

    /// 增量更新:读取文件 content_hash 快照(path → hash)。空库返回空 map(首次全量)。
    fn get_file_state(&self) -> Result<HashMap<String, String>>;
    /// 增量更新:整表替换文件状态快照(indexed_at 记 epoch 秒)。
    fn set_file_state(&mut self, states: &[(String, String)]) -> Result<()>;
    /// 增量更新:全量重算跨文件推断边(事务内 DELETE resolved=1 再重插 resolved=1)。
    fn replace_resolved_edges(&mut self, edges: Vec<Edge>) -> Result<()>;
    /// 增量更新:删除一个文件贡献的全部实体与边(File 实体 + Contains 子树 + 牵连边)。
    fn delete_file_subtree(&mut self, file_id: &EntityId) -> Result<()>;
    /// 增量更新:读取全部事实提取边(resolved=0),供跨文件 resolve 全量重算作输入。
    fn extract_edges(&self) -> Result<Vec<Edge>>;
}

pub struct SqliteGraphStore {
    connection: Connection,
}

impl SqliteGraphStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let connection = Connection::open(path)?;
        let store = Self { connection };
        store.initialize()?;
        Ok(store)
    }

    pub fn open_in_memory() -> Result<Self> {
        let connection = Connection::open_in_memory()?;
        let store = Self { connection };
        store.initialize()?;
        Ok(store)
    }

    pub fn counts(&self) -> Result<(u64, u64)> {
        let entities = self
            .connection
            .query_row("SELECT count(*) FROM entity", [], |row| row.get(0))?;
        let edges = self
            .connection
            .query_row("SELECT count(*) FROM edge", [], |row| row.get(0))?;
        Ok((entities, edges))
    }

    /// Counts entities grouped by `EntityKind`, aggregated in SQLite rather than
    /// loaded into memory. Used to render a bounded "system view" for very large
    /// graphs where `all_entities()` would overflow the MCP stdout channel.
    pub fn counts_by_kind(&self) -> Result<HashMap<String, u64>> {
        let mut statement = self
            .connection
            .prepare("SELECT kind, count(*) FROM entity GROUP BY kind")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
        })?;
        let mut counts = HashMap::new();
        for row in rows {
            let (kind, count) = row?;
            counts.insert(kind, count);
        }
        Ok(counts)
    }

    fn initialize(&self) -> Result<()> {
        self.connection.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA foreign_keys = ON;
            CREATE TABLE IF NOT EXISTS entity (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                name TEXT NOT NULL,
                qualified_name TEXT NOT NULL,
                json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS edge (
                source_id TEXT NOT NULL,
                target_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                json TEXT NOT NULL,
                resolved INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY(source_id, target_id, kind)
            );
            CREATE INDEX IF NOT EXISTS edge_source ON edge(source_id, kind);
            CREATE INDEX IF NOT EXISTS edge_target ON edge(target_id, kind);
            CREATE INDEX IF NOT EXISTS entity_name ON entity(name);
            CREATE VIRTUAL TABLE IF NOT EXISTS entity_fts USING fts5(
                entity_id UNINDEXED,
                name,
                qualified_name,
                tokenize = 'unicode61'
            );
            CREATE TABLE IF NOT EXISTS file_state (
                path TEXT PRIMARY KEY,
                hash TEXT NOT NULL,
                indexed_at INTEGER NOT NULL
            );
            ",
        )?;
        // 旧库迁移:为已存在的 edge 表补 `resolved` 列(新库已在 CREATE 中带)。
        self.ensure_edge_resolved_column()?;
        Ok(())
    }

    /// 幂等迁移:若 `edge` 表缺少 `resolved` 列则补上。增量更新用它区分事实提取边
    /// (resolved=0)与跨文件推断边(resolved=1),后者每次 scan 全量重算。
    fn ensure_edge_resolved_column(&self) -> Result<()> {
        let missing: bool = {
            let mut statement = self.connection.prepare("PRAGMA table_info(edge)")?;
            let columns: Vec<String> = statement
                .query_map([], |row| row.get::<_, String>(1))?
                .filter_map(|result| result.ok())
                .collect();
            !columns.iter().any(|name| name == "resolved")
        };
        if missing {
            self.connection.execute(
                "ALTER TABLE edge ADD COLUMN resolved INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
        Ok(())
    }

    fn row_entity(row: &rusqlite::Row<'_>) -> rusqlite::Result<Entity> {
        let json: String = row.get(0)?;
        serde_json::from_str(&json).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                json.len(),
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
    }

    fn row_edge(row: &rusqlite::Row<'_>) -> rusqlite::Result<Edge> {
        let json: String = row.get(0)?;
        serde_json::from_str(&json).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                json.len(),
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
    }

    fn adjacent_edges(&self, id: &EntityId, outbound: bool) -> Result<Vec<Edge>> {
        let sql = if outbound {
            "SELECT json FROM edge WHERE source_id = ?1"
        } else {
            "SELECT json FROM edge WHERE target_id = ?1"
        };
        let mut statement = self.connection.prepare(sql)?;
        let rows = statement.query_map([&id.0], |row| {
            let json: String = row.get(0)?;
            serde_json::from_str(&json).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    json.len(),
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    fn write_patch(
        transaction: &rusqlite::Transaction<'_>,
        patch: GraphPatch,
        replacing_snapshot: bool,
    ) -> Result<()> {
        if replacing_snapshot {
            transaction.execute_batch(
                "
                DELETE FROM edge;
                DELETE FROM entity_fts;
                DELETE FROM entity;
                ",
            )?;
        } else {
            for id in patch.remove_entities {
                transaction.execute(
                    "DELETE FROM edge WHERE source_id = ?1 OR target_id = ?1",
                    [&id.0],
                )?;
                transaction.execute("DELETE FROM entity_fts WHERE entity_id = ?1", [&id.0])?;
                transaction.execute("DELETE FROM entity WHERE id = ?1", [&id.0])?;
            }
        }

        {
            let mut upsert_entity = transaction.prepare_cached(
                "INSERT INTO entity(id, kind, name, qualified_name, json)
                 VALUES(?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(id) DO UPDATE SET
                   kind=excluded.kind, name=excluded.name,
                   qualified_name=excluded.qualified_name, json=excluded.json",
            )?;
            let mut delete_fts = if replacing_snapshot {
                None
            } else {
                Some(transaction.prepare_cached("DELETE FROM entity_fts WHERE entity_id = ?1")?)
            };
            let mut insert_fts = transaction.prepare_cached(
                "INSERT INTO entity_fts(entity_id, name, qualified_name) VALUES(?1, ?2, ?3)",
            )?;
            for entity in patch.add_entities {
                let json = serde_json::to_string(&entity)?;
                upsert_entity.execute(params![
                    entity.id.0,
                    entity.kind.as_str(),
                    entity.name,
                    entity.qualified_name,
                    json
                ])?;
                if let Some(statement) = delete_fts.as_mut() {
                    statement.execute([&entity.id.0])?;
                }
                insert_fts.execute(params![entity.id.0, entity.name, entity.qualified_name])?;
            }
        }

        {
            let mut upsert_edge = transaction.prepare_cached(
                "INSERT INTO edge(source_id, target_id, kind, json, resolved)
                 VALUES(?1, ?2, ?3, ?4, 0)
                 ON CONFLICT(source_id, target_id, kind) DO UPDATE SET
                   json=excluded.json, resolved=0",
            )?;
            for edge in patch.add_edges {
                let json = serde_json::to_string(&edge)?;
                upsert_edge.execute(params![
                    edge.source.0,
                    edge.target.0,
                    edge.kind.as_str(),
                    json
                ])?;
            }
        }
        Ok(())
    }
}

impl GraphStore for SqliteGraphStore {
    fn get_file_state(&self) -> Result<HashMap<String, String>> {
        let mut statement = self
            .connection
            .prepare("SELECT path, hash FROM file_state")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut states = HashMap::new();
        for row in rows {
            let (path, hash) = row?;
            states.insert(path, hash);
        }
        Ok(states)
    }

    fn set_file_state(&mut self, states: &[(String, String)]) -> Result<()> {
        let transaction = self.connection.transaction()?;
        transaction.execute("DELETE FROM file_state", [])?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or(0);
        {
            let mut insert = transaction.prepare_cached(
                "INSERT INTO file_state(path, hash, indexed_at) VALUES(?1, ?2, ?3)",
            )?;
            for (path, hash) in states {
                insert.execute(params![path, hash, now])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    fn replace_resolved_edges(&mut self, edges: Vec<Edge>) -> Result<()> {
        let transaction = self.connection.transaction()?;
        transaction.execute("DELETE FROM edge WHERE resolved = 1", [])?;
        {
            let mut upsert = transaction.prepare_cached(
                "INSERT INTO edge(source_id, target_id, kind, json, resolved)
                 VALUES(?1, ?2, ?3, ?4, 1)
                 ON CONFLICT(source_id, target_id, kind) DO UPDATE SET
                   json=excluded.json, resolved=1",
            )?;
            for edge in edges {
                let json = serde_json::to_string(&edge)?;
                upsert.execute(params![
                    edge.source.0,
                    edge.target.0,
                    edge.kind.as_str(),
                    json
                ])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// 删除一个文件贡献的全部实体与边:File 实体 + 经 `Contains` 边挂载的所有子实体,
    /// 以及这些实体作为 source/target 参与的任何边(含指向它们的跨文件 resolved 边)。
    ///
    /// 增量 scan 中先按变更文件逐个调用本方法删旧,再 `apply_patch` 提取边,最后
    /// `replace_resolved_edges` 全量重算跨文件边——指向已删实体的悬空边随实体删除,
    /// 随后重算不再生成,故无残留。
    fn delete_file_subtree(&mut self, file_id: &EntityId) -> Result<()> {
        let transaction = self.connection.transaction()?;
        // 待删实体 = file_id 自身 + 经 Contains 边挂载的全部子实体(类/字段/方法…)。
        let mut ids: Vec<String> = vec![file_id.0.clone()];
        {
            let mut select_children = transaction.prepare(
                "SELECT target_id FROM edge WHERE source_id = ?1 AND kind = 'contains'",
            )?;
            let rows = select_children.query_map([&file_id.0], |row| row.get::<_, String>(0))?;
            for row in rows {
                ids.push(row?);
            }
        }
        // 逐实体删除:先清牵连边(source/target 命中,含指向该实体的跨文件 resolved 边),
        // 再清 fts,最后删实体本身。单文件子树通常几十实体,逐条 prepare_cached 足够快,
        // 且避开 SQLite 单语句 999 绑定参数上限。
        {
            let mut delete_edges = transaction
                .prepare_cached("DELETE FROM edge WHERE source_id = ?1 OR target_id = ?1")?;
            let mut delete_fts =
                transaction.prepare_cached("DELETE FROM entity_fts WHERE entity_id = ?1")?;
            let mut delete_entity =
                transaction.prepare_cached("DELETE FROM entity WHERE id = ?1")?;
            for id in &ids {
                delete_edges.execute([id])?;
                delete_fts.execute([id])?;
                delete_entity.execute([id])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    fn extract_edges(&self) -> Result<Vec<Edge>> {
        let mut statement = self
            .connection
            .prepare("SELECT json FROM edge WHERE resolved = 0")?;
        let rows = statement.query_map([], Self::row_edge)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    fn apply_patch(&mut self, patch: GraphPatch) -> Result<()> {
        let transaction = self.connection.transaction()?;
        Self::write_patch(&transaction, patch, false)?;
        transaction.commit()?;
        Ok(())
    }

    fn replace_snapshot(&mut self, patch: GraphPatch) -> Result<()> {
        let transaction = self.connection.transaction()?;
        Self::write_patch(&transaction, patch, true)?;
        transaction.commit()?;
        Ok(())
    }

    fn search(&self, query: SearchQuery) -> Result<Vec<EntityMatch>> {
        let mut statement = self.connection.prepare(
            "SELECT e.json
             FROM entity e
             WHERE lower(e.name) LIKE lower(?1)
                OR lower(e.qualified_name) LIKE lower(?1)
             ORDER BY CASE WHEN lower(e.name) = lower(?2) THEN 0 ELSE 1 END, e.name
             LIMIT ?3 OFFSET ?4",
        )?;
        let pattern = format!("%{}%", query.text);
        let rows = statement.query_map(
            params![pattern, query.text, query.limit as i64, query.offset as i64],
            Self::row_entity,
        )?;
        rows.map(|result| result.map(|entity| EntityMatch { entity, score: 1.0 }))
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    fn search_exact_name(&self, name: &str, limit: usize) -> Result<Vec<Entity>> {
        let mut statement = self
            .connection
            .prepare("SELECT json FROM entity WHERE name = ?1 LIMIT ?2")?;
        let rows = statement.query_map(params![name, limit as i64], Self::row_entity)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    fn traverse(&self, query: TraverseQuery) -> Result<Traversal> {
        let mut entities = HashMap::new();
        let mut edges = Vec::new();
        let mut edge_keys = HashSet::new();
        let mut visited = HashSet::new();
        let mut queue = VecDeque::from([(query.start.clone(), 0usize)]);
        while let Some((current, depth)) = queue.pop_front() {
            if !visited.insert(current.clone()) {
                continue;
            }
            if let Some(entity) = self.get_entity(&current)? {
                entities.insert(current.clone(), entity);
            }
            if depth >= query.max_depth {
                continue;
            }
            for edge in self.adjacent_edges(&current, query.outbound)? {
                // When the caller pins `edge_kinds`, stay on those edges only —
                // otherwise a callers/callees walk would ride `Contains`/`DependsOn`
                // edges, burning depth budget and dragging in unrelated nodes.
                if !query.edge_kinds.is_empty() && !query.edge_kinds.contains(&edge.kind) {
                    continue;
                }
                let next = if query.outbound {
                    edge.target.clone()
                } else {
                    edge.source.clone()
                };
                let key = (edge.source.clone(), edge.target.clone(), edge.kind);
                if edge_keys.insert(key) {
                    edges.push(edge);
                }
                queue.push_back((next, depth + 1));
            }
        }
        Ok(Traversal {
            entities: entities.into_values().collect(),
            edges,
        })
    }

    fn get_entity(&self, id: &EntityId) -> Result<Option<Entity>> {
        self.connection
            .query_row(
                "SELECT json FROM entity WHERE id = ?1",
                [&id.0],
                Self::row_entity,
            )
            .optional()
            .context("read entity")
    }

    fn all_entities(&self) -> Result<Vec<Entity>> {
        let mut statement = self.connection.prepare("SELECT json FROM entity")?;
        let rows = statement.query_map([], Self::row_entity)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use repo_intelligence_model::EdgeKind;

    fn id(value: &str) -> EntityId {
        EntityId(value.to_string())
    }

    #[test]
    fn file_state_roundtrips_and_replaces() {
        let mut store = SqliteGraphStore::open_in_memory().unwrap();
        assert!(store.get_file_state().unwrap().is_empty());
        store
            .set_file_state(&[
                ("a.java".to_string(), "h1".to_string()),
                ("b.java".to_string(), "h2".to_string()),
            ])
            .unwrap();
        let state = store.get_file_state().unwrap();
        assert_eq!(state.len(), 2);
        assert_eq!(state.get("a.java").unwrap(), "h1");

        // 整表替换:b 被移除,c 加入
        store
            .set_file_state(&[("c.java".to_string(), "h3".to_string())])
            .unwrap();
        let state = store.get_file_state().unwrap();
        assert_eq!(state.len(), 1);
        assert!(!state.contains_key("a.java"));
        assert_eq!(state.get("c.java").unwrap(), "h3");
    }

    #[test]
    fn replace_resolved_edges_preserves_extract_edges() {
        let mut store = SqliteGraphStore::open_in_memory().unwrap();
        // 一条事实提取边(resolved=0),apply_patch 写入。
        let fact = Edge::new(id("a"), id("b"), EdgeKind::Contains);
        store
            .apply_patch(GraphPatch::add(Vec::new(), vec![fact]))
            .unwrap();
        // 第一批推断边(resolved=1)
        store
            .replace_resolved_edges(vec![Edge::new(id("c"), id("d"), EdgeKind::MappedFrom)])
            .unwrap();
        // 模拟下次 scan 重算:换成另一批推断边,旧的推断边必须被替换而非累加。
        store
            .replace_resolved_edges(vec![Edge::new(id("e"), id("f"), EdgeKind::MappedFrom)])
            .unwrap();

        let (_, edge_count) = store.counts().unwrap();
        // 提取边 1 条(不变)+ 推断边 1 条(替换不累加)= 2
        assert_eq!(edge_count, 2);

        // 提取边仍在(resolved=0 没被碰)
        let traversal = store
            .traverse(TraverseQuery {
                start: id("a"),
                outbound: true,
                max_depth: 1,
                edge_kinds: Vec::new(),
            })
            .unwrap();
        assert!(traversal.edges.iter().any(|edge| edge.target == id("b")));
    }

    #[test]
    fn delete_file_subtree_removes_subtree_and_dangling_edges() {
        use repo_intelligence_model::{Entity, EntityKind};
        let mut store = SqliteGraphStore::open_in_memory().unwrap();
        // file F 包含 class C;无关实体 G 独立挂在 Y 下。
        let f = Entity::new(id("file:F"), EntityKind::File, "F.java", "F.java");
        let c = Entity::new(id("C"), EntityKind::Class, "C", "C");
        let g = Entity::new(id("G"), EntityKind::Class, "G", "G");
        store
            .apply_patch(GraphPatch::add(
                vec![f, c, g],
                vec![
                    Edge::new(id("file:F"), id("C"), EdgeKind::Contains),
                    Edge::new(id("Y"), id("G"), EdgeKind::Contains),
                ],
            ))
            .unwrap();
        // 一条跨文件 resolved 边指向 C —— 删 C 时应随实体一并清除(无悬空残留)。
        store
            .replace_resolved_edges(vec![Edge::new(id("X"), id("C"), EdgeKind::MappedFrom)])
            .unwrap();

        store.delete_file_subtree(&id("file:F")).unwrap();

        assert!(store.get_entity(&id("file:F")).unwrap().is_none());
        assert!(store.get_entity(&id("C")).unwrap().is_none());
        assert!(store.get_entity(&id("G")).unwrap().is_some());
        // 只剩 Y→G 的 contains;F→C contains 与 X→C resolved 都随 C 删除。
        let (_, edge_count) = store.counts().unwrap();
        assert_eq!(edge_count, 1);
    }
}
