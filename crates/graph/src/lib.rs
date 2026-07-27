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
    fn traverse(&self, query: TraverseQuery) -> Result<Traversal>;
    fn get_entity(&self, id: &EntityId) -> Result<Option<Entity>>;
    fn all_entities(&self) -> Result<Vec<Entity>>;
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
                PRIMARY KEY(source_id, target_id, kind)
            );
            CREATE INDEX IF NOT EXISTS edge_source ON edge(source_id, kind);
            CREATE INDEX IF NOT EXISTS edge_target ON edge(target_id, kind);
            CREATE VIRTUAL TABLE IF NOT EXISTS entity_fts USING fts5(
                entity_id UNINDEXED,
                name,
                qualified_name,
                tokenize = 'unicode61'
            );
            ",
        )?;
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
                "INSERT INTO edge(source_id, target_id, kind, json)
                 VALUES(?1, ?2, ?3, ?4)
                 ON CONFLICT(source_id, target_id, kind) DO UPDATE SET json=excluded.json",
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
             LIMIT ?3",
        )?;
        let pattern = format!("%{}%", query.text);
        let rows = statement
            .query_map(params![pattern, query.text, query.limit as i64], |row| {
                Self::row_entity(row)
            })?;
        rows.map(|result| result.map(|entity| EntityMatch { entity, score: 1.0 }))
            .collect::<rusqlite::Result<Vec<_>>>()
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
