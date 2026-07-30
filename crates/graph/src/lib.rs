use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

use anyhow::{Context, Result};
use repo_intelligence_model::{Edge, Entity, EntityId, GraphPatch, SearchQuery, TraverseQuery};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

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

    /// 向量层(v0.1.26):写入 entity embedding(批量 upsert)。Vec<f32> 为模型维度向量,
    /// text_hash 记录生成时的文本摘要(增量:文本变才重新 embed)。默认 no-op。
    fn set_embeddings(&mut self, _embeddings: &[(EntityId, Vec<f32>, String)]) -> Result<()> {
        Ok(())
    }
    /// 向量层:读取已索引实体 → text_hash(增量判断:谁已有 embedding + 文本摘要)。
    fn get_embedding_state(&self) -> Result<HashMap<EntityId, String>> {
        Ok(HashMap::new())
    }
    /// 向量层:读取全部 entity_id → embedding(语义检索查询用,应用层算余弦)。
    fn get_all_embeddings(&self) -> Result<Vec<(EntityId, Vec<f32>)>> {
        Ok(Vec::new())
    }
}

pub struct SqliteGraphStore {
    connection: Connection,
    /// 是否维护 `entity_fts`(trigram 全文索引)并启用 MATCH 查询。
    /// false 时表 schema 仍建(兼容旧库),但不写不读,`search` 走 LIKE 兜底。
    fts_enabled: bool,
    /// entity_fts 当前是否有数据(open 时探测)。决定 search 走 MATCH 还是 LIKE 兜底,
    /// 避免空 FTS 库(未 scan / fts 关闭 / 旧库未迁移)的查询因 MATCH 返回空而失效。
    fts_populated: bool,
}

impl SqliteGraphStore {
    /// 默认开启 FTS(对标 codebase-memory 的全文检索能力)。
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_fts(path, true)
    }

    /// 显式控制 FTS 开关。CLI/MCP 先 `IndexerConfig::load` 再据此 open。
    pub fn open_with_fts(path: impl AsRef<Path>, fts_enabled: bool) -> Result<Self> {
        let connection = Connection::open(path)?;
        let mut store = Self {
            connection,
            fts_enabled,
            fts_populated: false,
        };
        store.initialize()?;
        store.refresh_fts_populated();
        Ok(store)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::open_in_memory_with_fts(true)
    }

    pub fn open_in_memory_with_fts(fts_enabled: bool) -> Result<Self> {
        let connection = Connection::open_in_memory()?;
        let mut store = Self {
            connection,
            fts_enabled,
            fts_populated: false,
        };
        store.initialize()?;
        store.refresh_fts_populated();
        Ok(store)
    }

    pub fn fts_enabled(&self) -> bool {
        self.fts_enabled
    }

    /// 探测 entity_fts 是否有数据,缓存到 fts_populated。open 与 scan 后调用。
    fn refresh_fts_populated(&mut self) {
        self.fts_populated = self
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM entity_fts LIMIT 1)",
                [],
                |row| row.get(0),
            )
            .unwrap_or(false);
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
            PRAGMA synchronous = NORMAL;
            PRAGMA cache_size = -65536;
            PRAGMA temp_store = MEMORY;
            PRAGMA foreign_keys = ON;
            CREATE TABLE IF NOT EXISTS entity (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                name TEXT NOT NULL,
                qualified_name TEXT NOT NULL,
                json TEXT NOT NULL,
                start_line INTEGER NOT NULL DEFAULT 0,
                end_line INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS edge (
                source_id TEXT NOT NULL,
                target_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                json TEXT NOT NULL,
                resolved INTEGER NOT NULL DEFAULT 0,
                start_line INTEGER NOT NULL DEFAULT 0,
                end_line INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY(source_id, target_id, kind)
            );
            CREATE INDEX IF NOT EXISTS edge_source ON edge(source_id, kind);
            CREATE INDEX IF NOT EXISTS edge_target ON edge(target_id, kind);
            CREATE INDEX IF NOT EXISTS entity_name ON entity(name);
            -- entity_fts(trigram)由 ensure_fts_trigram 统一建/迁移:execute_batch 的
            -- IF NOT EXISTS 救不了 tokenizer 升级,旧库(unicode61)需 DROP+CREATE。
            CREATE TABLE IF NOT EXISTS file_state (
                path TEXT PRIMARY KEY,
                hash TEXT NOT NULL,
                indexed_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS entity_embedding (
                entity_id TEXT PRIMARY KEY,
                embedding BLOB NOT NULL,
                dim INTEGER NOT NULL,
                text_hash TEXT NOT NULL
            );
            ",
        )?;
        // 旧库迁移:为已存在的 edge 表补 `resolved` 列(新库已在 CREATE 中带)。
        self.ensure_edge_resolved_column()?;
        // 旧库迁移:为 entity/edge 表补 `start_line`/`end_line` 顶层冗余列
        // (裸 SQL 直连查询友好,免 json_extract;新库已在 CREATE 中带)。
        self.ensure_line_columns()?;
        // entity_fts tokenizer 迁移:旧库 unicode61 → trigram(对 camelCase 标识符召回
        // 追平 LIKE);不存在则按 trigram 建;开启 FTS 时旧库数据一次性全量回填。
        self.ensure_fts_trigram()?;
        Ok(())
    }

    /// 幂等建/迁移 entity_fts 到 trigram tokenizer。
    ///
    /// 历史:早期 entity_fts 用 unicode61,对 camelCase 标识符(getUserId 等)召回近全空;
    /// v0.1.19 又因逐实体 INSERT 慢(2054s)整体删除了写入。v0.1.26 恢复写入并改用 trigram
    /// (索引化子串匹配,≥3 字符召回追平 LIKE、大小写不敏感),写入改为批量 INSERT...SELECT
    /// 绕开逐行往返。本函数:
    /// - 表不存在 → 按 trigram 建(新库);
    /// - 已是 trigram → 跳过;
    /// - 旧 unicode61 → DROP+CREATE,并在 `fts_enabled` 时从 entity 全量回填(批量,一次性)。
    fn ensure_fts_trigram(&self) -> Result<()> {
        let existing_sql: Option<String> = self
            .connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='entity_fts'",
                [],
                |row| row.get(0),
            )
            .ok();
        let needs_rebuild = match &existing_sql {
            Some(sql) => !sql.to_lowercase().contains("trigram"),
            None => true, // 表不存在(新库)
        };
        if needs_rebuild {
            if existing_sql.is_some() {
                self.connection.execute("DROP TABLE entity_fts", [])?;
            }
            self.connection.execute(
                "CREATE VIRTUAL TABLE entity_fts USING fts5(\
                 entity_id UNINDEXED, name, qualified_name, tokenize='trigram')",
                [],
            )?;
            // 旧库迁移且开启 FTS:把已有 entity 一次性批量回填(单语句,远快于逐行)。
            // 新库此处 entity 为空,回填为 no-op。
            if self.fts_enabled {
                let t = std::time::Instant::now();
                let n = self.connection.execute(
                    "INSERT INTO entity_fts(entity_id, name, qualified_name) \
                     SELECT id, name, qualified_name FROM entity",
                    [],
                )?;
                eprintln!(
                    "[ri-diag] fts migrate backfill: {n} entities in {:.2}s",
                    t.elapsed().as_secs_f64()
                );
            }
        }
        Ok(())
    }

    /// 幂等迁移:若 entity/edge 表缺少 `start_line` 列则补 start_line + end_line。
    /// 行号冗余列从 json.evidence 派生,仅利好裸 SQL 直连查询;工具层走 trait API 读
    /// json,不依赖本列(故读反序列化路径 row_entity/row_edge 无需改动)。
    fn ensure_line_columns(&self) -> Result<()> {
        for table in ["entity", "edge"] {
            let missing = {
                let mut statement =
                    self.connection.prepare(&format!("PRAGMA table_info({table})"))?;
                let columns: Vec<String> = statement
                    .query_map([], |row| row.get::<_, String>(1))?
                    .filter_map(|result| result.ok())
                    .collect();
                !columns.iter().any(|name| name == "start_line")
            };
            if missing {
                self.connection.execute(
                    &format!("ALTER TABLE {table} ADD COLUMN start_line INTEGER NOT NULL DEFAULT 0"),
                    [],
                )?;
                self.connection.execute(
                    &format!("ALTER TABLE {table} ADD COLUMN end_line INTEGER NOT NULL DEFAULT 0"),
                    [],
                )?;
            }
        }
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
        fts_enabled: bool,
    ) -> Result<()> {
        if replacing_snapshot {
            transaction.execute_batch(
                "
                DELETE FROM edge;
                DELETE FROM entity;
                ",
            )?;
        } else {
            for id in patch.remove_entities {
                transaction.execute(
                    "DELETE FROM edge WHERE source_id = ?1 OR target_id = ?1",
                    [&id.0],
                )?;
                transaction.execute("DELETE FROM entity WHERE id = ?1", [&id.0])?;
            }
        }

        {
            let mut upsert_entity = transaction.prepare_cached(
                "INSERT INTO entity(id, kind, name, qualified_name, json, start_line, end_line)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(id) DO UPDATE SET
                   kind=excluded.kind, name=excluded.name,
                   qualified_name=excluded.qualified_name, json=excluded.json,
                   start_line=excluded.start_line, end_line=excluded.end_line",
            )?;
            // v0.1.26:恢复 entity_fts 写入,但改 trigram tokenizer + 批量 INSERT...SELECT。
            // 历史:早期逐实体 DELETE+INSERT 到 unicode61 FTS,mes/mos 22 万实体花 2054s
            // (apply_patch 99.7%),v0.1.19 整体删除写入降回 ~5s。现批量后绕开逐行往返,
            // trigram 对 camelCase 召回追平 LIKE(unicode61 近全空)。详见 ensure_fts_trigram。
            let n_ent = patch.add_entities.len();
            // 先快照本次新增/更新实体 id(循环将 move add_entities),供 FTS 批量回填。
            let add_entity_ids: Vec<String> =
                patch.add_entities.iter().map(|e| e.id.0.clone()).collect();
            let t = std::time::Instant::now();
            for entity in patch.add_entities {
                let json = serde_json::to_string(&entity)?;
                let (start_line, end_line) = entity
                    .evidence
                    .first()
                    .map(|e| (e.start_line, e.end_line))
                    .unwrap_or((0, 0));
                upsert_entity.execute(params![
                    entity.id.0,
                    entity.kind.as_str(),
                    entity.name,
                    entity.qualified_name,
                    json,
                    start_line,
                    end_line
                ])?;
            }
            eprintln!(
                "[ri-diag] write_patch entities: {n_ent} in {:.2}s",
                t.elapsed().as_secs_f64()
            );

            // FTS 批量回填(仅 fts_enabled)。replacing_snapshot 先清空;增量先删旧避免重复行。
            // INSERT...SELECT 必须在 entity upsert 之后(entity 表需已有这些行)。
            if fts_enabled && !add_entity_ids.is_empty() {
                let t_fts = std::time::Instant::now();
                if replacing_snapshot {
                    transaction.execute("DELETE FROM entity_fts", [])?;
                } else {
                    Self::fts_bulk_in(
                        transaction,
                        "DELETE FROM entity_fts WHERE entity_id IN",
                        &add_entity_ids,
                    )?;
                }
                Self::fts_bulk_in(
                    transaction,
                    "INSERT INTO entity_fts(entity_id, name, qualified_name) \
                     SELECT id, name, qualified_name FROM entity WHERE id IN",
                    &add_entity_ids,
                )?;
                eprintln!(
                    "[ri-diag] write_patch fts: {} entities in {:.2}s",
                    add_entity_ids.len(),
                    t_fts.elapsed().as_secs_f64()
                );
            }
        }

        {
            let mut upsert_edge = transaction.prepare_cached(
                "INSERT INTO edge(source_id, target_id, kind, json, resolved, start_line, end_line)
                 VALUES(?1, ?2, ?3, ?4, 0, ?5, ?6)
                 ON CONFLICT(source_id, target_id, kind) DO UPDATE SET
                   json=excluded.json, resolved=0,
                   start_line=excluded.start_line, end_line=excluded.end_line",
            )?;
            let n_edg = patch.add_edges.len();
            let t = std::time::Instant::now();
            for edge in patch.add_edges {
                let json = serde_json::to_string(&edge)?;
                let (start_line, end_line) = edge
                    .evidence
                    .first()
                    .map(|e| (e.start_line, e.end_line))
                    .unwrap_or((0, 0));
                upsert_edge.execute(params![
                    edge.source.0,
                    edge.target.0,
                    edge.kind.as_str(),
                    json,
                    start_line,
                    end_line
                ])?;
            }
            eprintln!(
                "[ri-diag] write_patch edges: {n_edg} in {:.2}s",
                t.elapsed().as_secs_f64()
            );
        }
        Ok(())
    }

    /// 对 `ids` 分批执行 `"<prefix> (?, ?, ...)"`,绕开 SQLite 单语句 999 绑定上限
    /// (SQLITE_MAX_VARIABLE)。FTS 写入/删除复用,把 N 次逐行往返压成 N/900 批。
    fn fts_bulk_in(
        transaction: &rusqlite::Transaction<'_>,
        prefix: &str,
        ids: &[String],
    ) -> Result<()> {
        for chunk in ids.chunks(900) {
            let placeholders: String = (0..chunk.len())
                .map(|_| "?")
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!("{prefix} ({placeholders})");
            transaction.execute(&sql, params_from_iter(chunk.iter()))?;
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
                "INSERT INTO edge(source_id, target_id, kind, json, resolved, start_line, end_line)
                 VALUES(?1, ?2, ?3, ?4, 1, ?5, ?6)
                 ON CONFLICT(source_id, target_id, kind) DO UPDATE SET
                   json=excluded.json, resolved=1,
                   start_line=excluded.start_line, end_line=excluded.end_line",
            )?;
            for edge in edges {
                let json = serde_json::to_string(&edge)?;
                let (start_line, end_line) = edge
                    .evidence
                    .first()
                    .map(|e| (e.start_line, e.end_line))
                    .unwrap_or((0, 0));
                upsert.execute(params![
                    edge.source.0,
                    edge.target.0,
                    edge.kind.as_str(),
                    json,
                    start_line,
                    end_line
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
        // 删除顺序:先批量清 FTS(fts_enabled 时,单语句),再逐实体清牵连边 + 实体本身。
        // 单文件子树通常几十实体;edge/entity 用 prepare_cached 逐条(避开 999 绑定上限)。
        if self.fts_enabled && !ids.is_empty() {
            Self::fts_bulk_in(&transaction, "DELETE FROM entity_fts WHERE entity_id IN", &ids)?;
        }
        // 清理向量层(无开关依赖:有数据则删,no-op 否则)。fts_bulk_in 是通用批量删除 helper。
        if !ids.is_empty() {
            Self::fts_bulk_in(
                &transaction,
                "DELETE FROM entity_embedding WHERE entity_id IN",
                &ids,
            )?;
        }
        {
            let mut delete_edges = transaction
                .prepare_cached("DELETE FROM edge WHERE source_id = ?1 OR target_id = ?1")?;
            let mut delete_entity =
                transaction.prepare_cached("DELETE FROM entity WHERE id = ?1")?;
            for id in &ids {
                delete_edges.execute([id])?;
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

    fn set_embeddings(&mut self, embeddings: &[(EntityId, Vec<f32>, String)]) -> Result<()> {
        let transaction = self.connection.transaction()?;
        {
            let mut stmt = transaction.prepare_cached(
                "INSERT INTO entity_embedding(entity_id, embedding, dim, text_hash)
                 VALUES(?1, ?2, ?3, ?4)
                 ON CONFLICT(entity_id) DO UPDATE SET
                   embedding=excluded.embedding, dim=excluded.dim, text_hash=excluded.text_hash",
            )?;
            for (id, vec, hash) in embeddings {
                let dim = vec.len() as i64;
                // f32 数组 → native-endian 字节流(同机读写一致)。
                let bytes: Vec<u8> = vec.iter().flat_map(|f| f.to_ne_bytes()).collect();
                stmt.execute(params![&id.0, bytes, dim, hash])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    fn get_embedding_state(&self) -> Result<HashMap<EntityId, String>> {
        let mut stmt = self
            .connection
            .prepare("SELECT entity_id, text_hash FROM entity_embedding")?;
        let rows =
            stmt.query_map([], |row| {
                Ok((EntityId(row.get::<_, String>(0)?), row.get::<_, String>(1)?))
            })?;
        let mut map = HashMap::new();
        for row in rows {
            let (id, hash) = row?;
            map.insert(id, hash);
        }
        Ok(map)
    }

    fn get_all_embeddings(&self) -> Result<Vec<(EntityId, Vec<f32>)>> {
        let mut stmt = self
            .connection
            .prepare("SELECT entity_id, embedding, dim FROM entity_embedding")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, blob, dim) = row?;
            let vec: Vec<f32> = blob
                .chunks_exact(4)
                .map(|c| f32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            debug_assert_eq!(vec.len(), dim as usize);
            out.push((EntityId(id), vec));
        }
        Ok(out)
    }

    fn apply_patch(&mut self, patch: GraphPatch) -> Result<()> {
        let transaction = self.connection.transaction()?;
        Self::write_patch(&transaction, patch, false, self.fts_enabled)?;
        transaction.commit()?;
        // scan 填充了 FTS,刷新缓存让同 store 后续 search 走 MATCH。
        self.refresh_fts_populated();
        Ok(())
    }

    fn replace_snapshot(&mut self, patch: GraphPatch) -> Result<()> {
        let transaction = self.connection.transaction()?;
        Self::write_patch(&transaction, patch, true, self.fts_enabled)?;
        transaction.commit()?;
        self.refresh_fts_populated();
        Ok(())
    }

    fn search(&self, query: SearchQuery) -> Result<Vec<EntityMatch>> {
        // 拆词 OR:多词查询(如 "user login")任一词命中即召回,与历史行为一致。
        let raw_words: Vec<String> = {
            let split: Vec<&str> = query.text.split_whitespace().collect();
            if split.len() <= 1 {
                vec![query.text.clone()]
            } else {
                split.into_iter().map(String::from).collect()
            }
        };
        // 决策:fts_enabled 且所有词 ≥3 字符 → trigram MATCH(索引化,告别 leading-wildcard
        // 全表扫描);否则 LIKE 兜底。trigram 对 <3 字符查询无法产生 3-gram,实测召回失败
        // (如查 "eq" 找不到 eqUser),故含 <3 字符词时必须回退 LIKE。
        let use_fts = self.fts_enabled
            && self.fts_populated
            && raw_words.iter().all(|w| w.chars().count() >= 3);
        let (clauses, terms): (Vec<String>, Vec<String>) = if use_fts {
            let clauses = raw_words
                .iter()
                .enumerate()
                .map(|(i, _)| {
                    let idx = i + 1;
                    format!(
                        "e.id IN (SELECT entity_id FROM entity_fts WHERE entity_fts MATCH ?{idx})"
                    )
                })
                .collect();
            // 短语包裹('"word"')避免 FTS5 特殊字符(* : " 等)被当操作符解析;
            // trigram 短语仍做 3-gram 子串匹配,召回与子串 LIKE 等价。
            let terms = raw_words.iter().map(|w| format!("\"{w}\"")).collect();
            (clauses, terms)
        } else {
            let terms: Vec<String> = raw_words.iter().map(|w| format!("%{w}%")).collect();
            let clauses = terms
                .iter()
                .enumerate()
                .map(|(i, _)| {
                    let idx = i + 1;
                    format!(
                        "(lower(e.name) LIKE lower(?{idx}) OR lower(e.qualified_name) LIKE lower(?{idx}))"
                    )
                })
                .collect();
            (clauses, terms)
        };
        let where_clause = clauses.join(" OR ");
        let name_idx = terms.len() + 1;
        let limit_idx = terms.len() + 2;
        let offset_idx = terms.len() + 3;
        let sql = format!(
            "SELECT e.json FROM entity e WHERE {where_clause} \
             ORDER BY CASE WHEN lower(e.name) = lower(?{name_idx}) THEN 0 ELSE 1 END, e.name \
             LIMIT ?{limit_idx} OFFSET ?{offset_idx}"
        );
        let mut statement = self.connection.prepare(&sql)?;
        let limit = query.limit as i64;
        let offset = query.offset as i64;
        let mut params_vec: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(terms.len() + 3);
        for w in &terms {
            params_vec.push(w);
        }
        params_vec.push(&query.text);
        params_vec.push(&limit);
        params_vec.push(&offset);
        let rows = statement.query_map(params_vec.as_slice(), Self::row_entity)?;
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
    fn line_columns_populated_for_bare_sql() {
        // 行号冗余列写入 entity/edge 顶层,裸 SQL 直连可查(免 json_extract)。
        // 工具层走 trait API 读 json 不依赖本列;此测试钉住"直连查询体验"这一收益。
        use repo_intelligence_model::{Entity, EntityKind, EvidenceClass};
        let mut store = SqliteGraphStore::open_in_memory().unwrap();
        let entity = Entity::new(id("C"), EntityKind::Class, "C", "C")
            .with_evidence("F.java", 42, 48, EvidenceClass::Fact, 1.0, "decl");
        store
            .apply_patch(GraphPatch::add(vec![entity], Vec::new()))
            .unwrap();
        let (start, end): (i64, i64) = store
            .connection
            .query_row("SELECT start_line, end_line FROM entity WHERE id = 'C'", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(start, 42, "start_line 应写入顶层列");
        assert_eq!(end, 48, "end_line 应写入顶层列");
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
