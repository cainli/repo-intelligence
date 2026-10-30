use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

use anyhow::{Context, Result};
use repo_intelligence_model::{
    ClusterInfo, Edge, Entity, EntityId, EntityKind, EvidenceClass, GraphPatch, QueryResult,
    SearchQuery, TraverseQuery,
};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

#[derive(Clone, Debug)]
pub struct EntityMatch {
    pub entity: Entity,
    pub score: f32,
}

/// 编辑距离(lenvenshtein,64 字符截断足够做 schema 近邻提示)。
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().take(64).collect();
    let b: Vec<char> = b.chars().take(64).collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            cur[j] = (prev[j] + 1)
                .min(cur[j - 1] + 1)
                .min(prev[j - 1] + usize::from(a[i - 1] != b[j - 1]));
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
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
    /// 子串搜索的命中分面:同 `search` 的召回口径,按 kind 分组计数(过滤前)。
    /// 消费端(mcp search_entities)用它组装 total_count + kind facet,让调用方
    /// 判断「值不值得翻页/要不要加 kind 过滤」——此前只有 has_more,宽词命中
    /// 数千实体时无从决策(第五轮反馈 F4)。默认无实现(空 = 无分面能力)。
    fn search_facets(&self, _query: &SearchQuery) -> Result<Vec<(String, usize)>> {
        Ok(Vec::new())
    }
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
    /// 增量更新:批量删除多个文件的子树(语义 = 逐个调 delete_file_subtree)。
    /// 大仓全量重提(数万文件)时单文件逐删的每文件事务 + FTS 全表扫是二次方,
    /// 批量路径一次 TEMP 表集合删除;返回删除的实体行数。默认实现退化为逐文件。
    fn delete_file_subtrees_batch(&mut self, file_ids: &[EntityId]) -> Result<usize> {
        for file_id in file_ids {
            self.delete_file_subtree(file_id)?;
        }
        Ok(file_ids.len())
    }
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
    /// 向量层:读取全部 entity_id → embedding + text_hash(语义检索查询用,应用层
    /// 算余弦;text_hash 供调用方过滤旧向量空间)。
    fn get_all_embeddings(&self) -> Result<Vec<(EntityId, Vec<f32>, String)>> {
        Ok(Vec::new())
    }

    /// 聚类汇总(v0.1.37):全量替换 cluster_info 表(scan 每轮重算覆盖)。默认 no-op。
    fn set_cluster_info(&mut self, _infos: &[ClusterInfo]) -> Result<()> {
        Ok(())
    }
    /// 聚类汇总:读取全部簇标注(按 size 降序)。旧库无表/未重扫时返回空。
    fn get_cluster_info(&self) -> Result<Vec<ClusterInfo>> {
        Ok(Vec::new())
    }
    /// 索引健康度元数据(第五轮反馈 F5):scan 收尾把摘要落库(finished_at/duration/
    /// files/entities/edges/embedded…),get_index_status/doctor 读出——此前只有裸
    /// entity/edge 计数,「该不该重建」「这次重建质量如何」无从判断。默认 no-op。
    fn set_scan_meta(&mut self, _meta: &[(String, String)]) -> Result<()> {
        Ok(())
    }
    /// 读取全部 scan 元数据;未 scan 的库返回空 map。
    fn get_scan_meta(&self) -> Result<HashMap<String, String>> {
        Ok(HashMap::new())
    }
    /// 索引健康度三计数:(实体数, 有向量的实体数, entity_fts 行数)。embedding 覆盖
    /// 与 fts 同步是「重建质量」的两个最常见盲区(向量中断残留、fts 漂移)。
    fn health_counts(&self) -> Result<(usize, usize, usize)> {
        Ok((0, 0, 0))
    }
    /// 边健康度分布:kind × resolved × evidence classification → 计数。doctor 用来
    /// 一眼判断索引健康(如「calls 全部低分类」= resolve 层未跑全)。
    fn edge_health(&self) -> Result<Vec<(String, i64, String, usize)>> {
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
        store.reconcile_fts()?;
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
        store.reconcile_fts()?;
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

    /// open 后(fts_enabled 时)校验 entity_fts 与 entity 行数,不一致则全量重建。
    /// 修两类漂移的正确性 bug(此前搜索会返回错误结果):
    /// (1) scan 跑过 fts_enabled=false 期间增删了实体但未动 fts —— 重开 fts=true 后
    ///     新实体对 MATCH 不可见(FTS 漏索引);
    /// (2) trigram 迁移的 DROP+CREATE+回填非原子,中途崩溃留下空 fts 表(0 vs N)。
    /// 正常增量扫描两边行数一致 → 直接返回,无重建开销。
    /// 局限:增删恰好等量(±N)时行数仍相等,极少数陈旧行残留——但 JOIN `e.id IN (fts)`
    /// 会滤掉已删实体(无误报),且下次 fts_enabled 扫描的 changed-file 增量会覆盖。
    fn reconcile_fts(&self) -> Result<()> {
        if !self.fts_enabled {
            return Ok(());
        }
        let entity_count: i64 =
            self.connection
                .query_row("SELECT COUNT(*) FROM entity", [], |row| row.get(0))?;
        let fts_count: i64 = self
            .connection
            .query_row("SELECT COUNT(*) FROM entity_fts", [], |row| row.get(0))
            .unwrap_or(0);
        if entity_count == fts_count {
            return Ok(());
        }
        let t = std::time::Instant::now();
        self.connection.execute("DELETE FROM entity_fts", [])?;
        let n = self.connection.execute(
            // rowid 对齐(见 ensure_fts_rowid_aligned):fts 删除走 rowid 索引的前提。
            "INSERT INTO entity_fts(rowid, entity_id, name, qualified_name) \
             SELECT rowid, id, name, qualified_name FROM entity",
            [],
        )?;
        eprintln!(
            "[ri-diag] fts reconcile: entity={entity_count} fts={fts_count} mismatch → rebuilt {n} rows in {:.2}s",
            t.elapsed().as_secs_f64()
        );
        Ok(())
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

    /// 只读 SQL 直通(query_sql 工具的后端)。语句级防线:
    /// ① trim 后必须以 select/with 开头(大小写不敏感);
    /// ② 整条语句不允许出现分号(拒绝堆叠语句;子查询不需要分号);
    /// ③ 写入关键词黑名单双保险(insert/update/delete/drop/alter/attach/pragma/create/…);
    /// ④ 行数超 max_rows 截断并把 truncated=true,单元格文本超 400 字符截断。
    /// query_only PRAGMA 兜底:仅本次调用期间打开,物理阻断任何写语义。
    pub fn read_only_query(&self, sql: &str, max_rows: usize) -> Result<QueryResult> {
        let trimmed = sql.trim().trim_end_matches(';').trim();
        let lower = trimmed.to_ascii_lowercase();
        anyhow::ensure!(
            lower.starts_with("select") || lower.starts_with("with"),
            "only SELECT/WITH statements are allowed"
        );
        anyhow::ensure!(
            !trimmed.contains(';'),
            "multiple statements are not allowed"
        );
        for banned in [
            "insert ", "update ", "delete ", "drop ", "alter ", "attach ", "pragma ", "create ",
            "vacuum ", "reindex ",
        ] {
            anyhow::ensure!(
                !lower.contains(banned),
                "keyword {banned:?} is not allowed in read-only queries"
            );
        }
        // SqliteGraphStore 持单 Connection 且无锁:PRAGMA query_only ON → 查询 → OFF
        // 必须同一次调用内完成,query_only 只在本方法执行窗口内生效。
        self.connection.execute_batch("PRAGMA query_only = ON")?;
        let result = self.run_select(trimmed, max_rows);
        self.connection.execute_batch("PRAGMA query_only = OFF")?;
        result
    }

    fn run_select(&self, sql: &str, max_rows: usize) -> Result<QueryResult> {
        use rusqlite::types::ValueRef;
        let mut stmt = self
            .connection
            .prepare(sql)
            .map_err(|e| self.hint_schema_error(e))?;
        let columns: Vec<String> = stmt.column_names().iter().map(|c| c.to_string()).collect();
        let mut rows = Vec::new();
        let mut truncated = false;
        let mut rows_iter = stmt.query([])?;
        while let Some(row) = rows_iter.next()? {
            if rows.len() >= max_rows {
                truncated = true;
                break;
            }
            let mut out = Vec::with_capacity(columns.len());
            for idx in 0..columns.len() {
                out.push(match row.get_ref(idx)? {
                    ValueRef::Null => serde_json::Value::Null,
                    ValueRef::Integer(v) => serde_json::json!(v),
                    ValueRef::Real(v) => serde_json::json!(v),
                    ValueRef::Text(t) => {
                        let mut s = String::from_utf8_lossy(t).to_string();
                        if s.chars().count() > 400 {
                            // 字符边界安全截断:先按 chars 取前 200(直接 &s[..200] 字节切片
                            // 会在多字节字符中间 panic —— json/evidence 里中文注释很常见)。
                            let head: String = s.chars().take(200).collect();
                            s = format!("{}…({} chars)", head, s.chars().count());
                        }
                        serde_json::json!(s)
                    }
                    ValueRef::Blob(b) => serde_json::json!(format!("<blob {} bytes>", b.len())),
                });
            }
            rows.push(out);
        }
        Ok(QueryResult {
            columns,
            rows,
            truncated,
        })
    }

    /// schema 可发现性(mes-activity 反馈 P2-6):表名单数(entity/edge)、判别列叫
    /// kind(非 edge_kind),首猜必错且 SQLite 的 "no such table: edges" 裸透传无任何
    /// 提示。prepare 失败且消息含 no such table/column 时,读真实 schema 做最近邻提示
    /// 重写错误文案。零 tools/list 成本(不占 description),列名错误自动同覆盖。
    fn hint_schema_error(&self, err: rusqlite::Error) -> anyhow::Error {
        let msg = err.to_string();
        let lower = msg.to_ascii_lowercase();
        if !lower.contains("no such table") && !lower.contains("no such column") {
            return err.into();
        }
        // 被拒名字:列错误消息带 SQL 上下文后缀("no such column: edge_kind in SELECT …
        // at offset 7"),取 ':' 后第一个空白前 token;表错误("no such table: edges")
        // 同样兼容。
        let rejected = msg
            .split(':')
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .map(|s| s.trim_matches('\'').to_string())
            .unwrap_or_default();
        // 真实 schema:表+视图清单(FTS shadow 表过滤)+ 各表列名(视图也参与
        // 近邻提示——v_edge/v_entity 是 API 字段词汇表的官方入口,清单里看不到
        // 等于没做)。
        let mut tables: Vec<String> = Vec::new();
        if let Ok(mut stmt) = self.connection.prepare(
            "SELECT name FROM sqlite_master WHERE type IN ('table','view') \
             AND name NOT LIKE 'sqlite_%' AND name NOT LIKE 'entity_fts_%'",
        ) && let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0))
        {
            for t in rows.flatten() {
                tables.push(t);
            }
        }
        if tables.is_empty() {
            return err.into();
        }
        tables.sort();
        tables.dedup(); // 防御:个别环境下 sqlite_master 重名行会让提示清单翻倍
        let mut columns: Vec<(String, String)> = Vec::new(); // (table, column)
        for t in &tables {
            if let Ok(mut stmt) = self.connection.prepare(&format!("PRAGMA table_info({t})"))
                && let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(1))
            {
                for c in rows.flatten() {
                    columns.push((t.clone(), c));
                }
            }
        }
        if lower.contains("no such table") {
            let nearest = tables
                .iter()
                .min_by_key(|t| edit_distance(t, &rejected))
                .map(|t| format!("Did you mean `{t}`? "))
                .unwrap_or_default();
            anyhow::anyhow!(
                "{msg}. {nearest}Tables: {}. Names are singular; `kind` is the discriminator column.",
                tables.join(", ")
            )
        } else {
            // 始终给最近列建议(阈值会漏 edge_kind→kind 这种"前缀+下划线变体",距离 5);
            // 平局列出全部归属表(kind 在 entity/edge 都有,不能瞎指一个)。
            let best = columns
                .iter()
                .map(|(t, c)| (edit_distance(c, &rejected), t.clone(), c.clone()))
                .min_by_key(|(d, _, _)| *d);
            let hint = match best {
                Some((d, _, c)) => {
                    let mut in_tables: Vec<&str> = columns
                        .iter()
                        .filter(|(_, col)| edit_distance(col, &rejected) == d)
                        .map(|(t, _)| t.as_str())
                        .collect();
                    in_tables.sort_unstable();
                    in_tables.dedup();
                    format!("Did you mean `{c}` (in table {})? ", in_tables.join(" or "))
                }
                None => String::new(),
            };
            let by_table: Vec<String> = tables
                .iter()
                .map(|t| {
                    let cols: Vec<String> = columns
                        .iter()
                        .filter(|(tb, _)| tb == t)
                        .map(|(_, c)| c.clone())
                        .collect();
                    format!("{t}({})", cols.join(", "))
                })
                .collect();
            anyhow::anyhow!("{msg}. {hint}Columns: {}.", by_table.join("; "))
        }
    }

    fn initialize(&mut self) -> Result<()> {
        // {resolved_below_threshold}:v_edge.tentative 计算列的 Resolved 置信阈值,
        // 与 mcp edge_view 共用 model 层常量(单源,防两套口径漂移)。
        let resolved_below_threshold = EvidenceClass::RESOLVED_TENTATIVE_BELOW;
        self.connection.execute_batch(&format!(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA cache_size = -65536;
            -- MCP 查询与 scan 写并发时,SQLite 默认立即抛 SQLITE_BUSY;WAL 下短暂等待
            -- 即可自愈,而不是把瞬时锁竞争以「database is locked」裸错误炸给用户。
            PRAGMA busy_timeout = 5000;
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
            CREATE TABLE IF NOT EXISTS cluster_info (
                cluster_id INTEGER PRIMARY KEY,
                label TEXT,
                cohesion REAL NOT NULL DEFAULT 0,
                top_nodes TEXT NOT NULL DEFAULT '[]',
                size INTEGER NOT NULL DEFAULT 0
            );
            -- scan 收尾摘要(第五轮反馈 F5):get_index_status/doctor 的健康度数据源。
            CREATE TABLE IF NOT EXISTS scan_meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            -- API 同名字段兼容视图(大仓反馈 P1-2:trace/query 回包是 source/target/
            -- confidence/tentative,裸 SQL 直连却是 source_id/target_id/json,两套词汇
            -- 表只能靠踩坑习得)。v_edge 直接暴露 API 字段名。
            -- v2(第五轮反馈 F1):tentative 不再提取 `$.tentative`(edge JSON 无顶层
            -- 字段,恒 NULL 的死列曾让直查者误读「injects 100% tentative」)——改按
            -- evidence[0].classification 复算,与 mcp edge_view 同一判定;阈值常量
            -- 由 Rust 侧 format 注入(model 单源)。视图无数据,每次 DROP+CREATE
            -- 保证旧库也拿到最新定义(IF NOT EXISTS 救不了定义变更)。
            DROP VIEW IF EXISTS v_edge;
            CREATE VIEW v_edge AS
            SELECT e.source_id, e.target_id, e.kind, e.resolved,
                   e.start_line, e.end_line,
                   s.qualified_name AS source, t.qualified_name AS target,
                   json_extract(e.json, '$.evidence[0].confidence') AS confidence,
                   json_extract(e.json, '$.evidence[0].classification') AS evidence_class,
                   CASE
                     WHEN json_extract(e.json, '$.evidence[0].classification') = 'fact' THEN 0
                     WHEN json_extract(e.json, '$.evidence[0].classification') = 'resolved'
                          AND json_extract(e.json, '$.evidence[0].confidence') >= {resolved_below_threshold} THEN 0
                     ELSE 1
                   END AS tentative
            FROM edge e
            JOIN entity s ON s.id = e.source_id
            JOIN entity t ON t.id = e.target_id;
            CREATE VIEW IF NOT EXISTS v_entity AS
            SELECT id, kind, name, qualified_name, start_line, end_line,
                   json_array_length(json, '$.evidence') AS evidence_count,
                   json_extract(json, '$.metadata') AS metadata
            FROM entity;
            ",
        ))?;
        // 旧库迁移:为已存在的 edge 表补 `resolved` 列(新库已在 CREATE 中带)。
        self.ensure_edge_resolved_column()?;
        // 旧库迁移:为 entity/edge 表补 `start_line`/`end_line` 顶层冗余列
        // (裸 SQL 直连查询友好,免 json_extract;新库已在 CREATE 中带)。
        self.ensure_line_columns()?;
        // entity_fts tokenizer 迁移:旧库 unicode61 → trigram(对 camelCase 标识符召回
        // 追平 LIKE);不存在则按 trigram 建;开启 FTS 时旧库数据一次性全量回填。
        self.ensure_fts_trigram()?;
        // entity_fts rowid 对齐迁移:FTS 删除走 rowid 索引的前提(见函数注释)。
        self.ensure_fts_rowid_aligned()?;
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
                    // rowid 对齐(见 ensure_fts_rowid_aligned):fts 删除走 rowid 索引的前提。
                    "INSERT INTO entity_fts(rowid, entity_id, name, qualified_name) \
                     SELECT rowid, id, name, qualified_name FROM entity",
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

    /// 存量库一次性迁移:把 entity_fts 的 rowid 对齐到 entity 的隐式 rowid
    /// (INSERT 显式带 rowid),使 `DELETE FROM entity_fts WHERE rowid IN (...)`
    /// 走 FTS5 的 rowid 索引。此前 entity_id(UNINDEXED)的 IN 删除是虚拟表
    /// 全表 SCAN——45.9 万行库上单次 60ms,逐文件子树删除 3 万次 = 小时级
    /// (mes-activity 全量重提"卡死"根因);rowid 路径单次 0.3ms(200 倍)。
    /// 旧库的 fts rowid 是 FTS5 自动分配的顺序号,与 entity.rowid 无对应,
    /// 必须全量重建一次对齐(≈22s,与 reconcile 同级,之后不再发生)。
    /// `PRAGMA user_version` 空闲,用 1 标记"已对齐";新库回填 0 行 no-op。
    /// 不变量:此后所有写 entity_fts 的路径都显式带 rowid=entity.rowid
    /// (reconcile_fts / ensure_fts_trigram 回填 / write_patch)。
    fn ensure_fts_rowid_aligned(&mut self) -> Result<()> {
        let version: i64 = self
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version >= 1 {
            return Ok(());
        }
        if self.fts_enabled {
            let t = std::time::Instant::now();
            let transaction = self.connection.transaction()?;
            transaction.execute("DELETE FROM entity_fts", [])?;
            let n = transaction.execute(
                "INSERT INTO entity_fts(rowid, entity_id, name, qualified_name) \
                 SELECT rowid, id, name, qualified_name FROM entity",
                [],
            )?;
            transaction.execute_batch("PRAGMA user_version = 1")?;
            transaction.commit()?;
            eprintln!(
                "[ri-diag] fts rowid-align migrate: {n} rows in {:.2}s",
                t.elapsed().as_secs_f64()
            );
        } else {
            // fts 关闭时不写 fts(空表天然对齐),只推进版本标记。
            self.connection.execute_batch("PRAGMA user_version = 1")?;
        }
        Ok(())
    }

    /// 幂等迁移:若 entity/edge 表缺少 `start_line` 列则补 start_line + end_line。
    /// 行号冗余列从 json.evidence 派生,仅利好裸 SQL 直连查询;工具层走 trait API 读
    /// json,不依赖本列(故读反序列化路径 row_entity/row_edge 无需改动)。
    fn ensure_line_columns(&self) -> Result<()> {
        for table in ["entity", "edge"] {
            let missing = {
                let mut statement = self
                    .connection
                    .prepare(&format!("PRAGMA table_info({table})"))?;
                let columns: Vec<String> = statement
                    .query_map([], |row| row.get::<_, String>(1))?
                    .filter_map(|result| result.ok())
                    .collect();
                !columns.iter().any(|name| name == "start_line")
            };
            if missing {
                self.connection.execute(
                    &format!(
                        "ALTER TABLE {table} ADD COLUMN start_line INTEGER NOT NULL DEFAULT 0"
                    ),
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
            // 必须去重:add_entities 常含重复 id(同实体多 patch,entity UPSERT 幂等),
            // 而 fts 的 rowid 有唯一约束——重复 id 跨 900-块再 INSERT 同 rowid 即
            // PRIMARY KEY 冲突;旧 entity_id 路径虽不炸但会插重复 fts 行(漂移源)。
            let mut add_entity_ids: Vec<String> =
                patch.add_entities.iter().map(|e| e.id.0.clone()).collect();
            add_entity_ids.sort_unstable();
            add_entity_ids.dedup();
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
            // 删除走 rowid 路径(经 entity 反查 rowid,见 fts_delete_by_entity_ids):
            // entity_id 是 UNINDEXED 列,IN 删除是 FTS 虚拟表全表 SCAN。
            if fts_enabled && !add_entity_ids.is_empty() {
                let t_fts = std::time::Instant::now();
                if replacing_snapshot {
                    transaction.execute("DELETE FROM entity_fts", [])?;
                } else {
                    Self::fts_delete_by_entity_ids(transaction, &add_entity_ids)?;
                }
                Self::fts_bulk_in(
                    transaction,
                    "INSERT INTO entity_fts(rowid, entity_id, name, qualified_name) \
                     SELECT rowid, id, name, qualified_name FROM entity WHERE id IN",
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
    ///
    /// 仅用于 INSERT 路径;删除必须走 [`Self::fts_delete_by_entity_ids`](rowid 索引)。
    fn fts_bulk_in(
        transaction: &rusqlite::Transaction<'_>,
        prefix: &str,
        ids: &[String],
    ) -> Result<()> {
        for chunk in ids.chunks(900) {
            let placeholders: String = (0..chunk.len()).map(|_| "?").collect::<Vec<_>>().join(", ");
            let sql = format!("{prefix} ({placeholders})");
            transaction.execute(&sql, params_from_iter(chunk.iter()))?;
        }
        Ok(())
    }

    /// 按 entity id 批量删 entity_fts 行,走 rowid 索引而非 UNINDEXED entity_id
    /// 的全表 SCAN(45.9 万行库:60ms → 0.3ms/次)。依赖 ensure_fts_rowid_aligned
    /// 建立(及各写入路径维持)的 fts.rowid == entity.rowid 对齐;实体行必须
    /// 仍存在(rowid 经 entity 反查),调用点均在 entity 删除之前或同 id upsert
    /// 场景(update 分支 rowid 不变),成立。
    fn fts_delete_by_entity_ids(
        transaction: &rusqlite::Transaction<'_>,
        ids: &[String],
    ) -> Result<()> {
        for chunk in ids.chunks(900) {
            let placeholders: String = (0..chunk.len()).map(|_| "?").collect::<Vec<_>>().join(", ");
            let sql = format!(
                "DELETE FROM entity_fts WHERE rowid IN \
                 (SELECT rowid FROM entity WHERE id IN ({placeholders}))"
            );
            transaction.execute(&sql, params_from_iter(chunk.iter()))?;
        }
        Ok(())
    }

    /// search/search_facets 共享:拆词 + FTS/LIKE 谓词构造(fts 决策 + 转义规则)。
    /// 返回 (where_clause, 绑定参数 terms)。
    fn search_predicates(&self, query: &SearchQuery) -> (String, Vec<String>) {
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
            // 词内 " 需转义为 ""(FTS5 短语内字面引号),否则 `foo"bar` 产出
            // `"foo"bar"` 是未闭合引号 → MATCH 报错,整个搜索失败。
            let terms = raw_words
                .iter()
                .map(|w| format!("\"{}\"", w.replace('"', "\"\"")))
                .collect();
            (clauses, terms)
        } else {
            // LIKE 通配符 % _ 未转义会让查询过宽(如查 "100%" 匹配含 "100" 的任何串)。
            // ESCAPE '\' 后 \\% / \\_ 当字面量。
            let terms: Vec<String> = raw_words
                .iter()
                .map(|w| {
                    format!(
                        "%{}%",
                        w.replace('\\', "\\\\")
                            .replace('%', "\\%")
                            .replace('_', "\\_")
                    )
                })
                .collect();
            let clauses = terms
                .iter()
                .enumerate()
                .map(|(i, _)| {
                    let idx = i + 1;
                    format!(
                        "(lower(e.name) LIKE lower(?{idx}) ESCAPE '\\' OR lower(e.qualified_name) LIKE lower(?{idx}) ESCAPE '\\')"
                    )
                })
                .collect();
            (clauses, terms)
        };
        let where_clause = clauses.join(" OR ");
        (where_clause, terms)
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
    /// 增量 scan 中先按变更文件批量调 `delete_file_subtrees_batch` 删旧,再
    /// `apply_patch` 提取边,最后 `replace_resolved_edges` 全量重算跨文件边——指向
    /// 已删实体的悬空边随实体删除,随后重算不再生成,故无残留。
    fn delete_file_subtree(&mut self, file_id: &EntityId) -> Result<()> {
        self.delete_file_subtrees_batch(std::slice::from_ref(file_id))
            .map(|_| ())
    }

    /// 批量删除多个文件的子树,语义 = 逐个 delete_file_subtree,但单事务集合化:
    /// 待删 id 收进 TEMP 表(无 999 绑定上限),FTS/edge/entity 各一条集合语句。
    /// 历史坑:曾逐文件单事务 + FTS 按 entity_id(UNINDEXED)IN 删除,FTS 虚拟表
    /// 全表 SCAN——mes-activity 45.9 万实体 × 3 万文件全量重提实测小时级且进度条
    /// 停在 0(循环内无上报);批量化 + FTS rowid 路径后为秒~分钟级。块间由调用方
    /// 分块提交(analysis 层 2000 文件/块),块内原子——中断回滚整块,无半删状态。
    /// 向量层不随子树删除(P1-C′ 断点续扫),孤儿向量由 get_all_embeddings 读时
    /// JOIN entity 过滤;rm 库全清。
    fn delete_file_subtrees_batch(&mut self, file_ids: &[EntityId]) -> Result<usize> {
        if file_ids.is_empty() {
            return Ok(0);
        }
        let transaction = self.connection.transaction()?;
        transaction.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS del_ids(id TEXT PRIMARY KEY);
             DELETE FROM del_ids;",
        )?;
        for chunk in file_ids.chunks(900) {
            let values: String = (0..chunk.len())
                .map(|_| "(?)")
                .collect::<Vec<_>>()
                .join(",");
            transaction.execute(
                &format!("INSERT OR IGNORE INTO del_ids(id) VALUES {values}"),
                params_from_iter(chunk.iter().map(|id| id.0.as_str())),
            )?;
        }
        // 一层 Contains 展开(File→类/字段/方法,与单文件版一致):
        // 语句开始时 del_ids 已含 file ids,SELECT 读到的即它们。
        transaction.execute(
            "INSERT OR IGNORE INTO del_ids(id) \
             SELECT target_id FROM edge \
             WHERE kind = 'contains' AND source_id IN (SELECT id FROM del_ids)",
            [],
        )?;
        // 删除顺序:先 FTS(rowid 路径,实体行仍在),再边,再实体。
        if self.fts_enabled {
            transaction.execute(
                "DELETE FROM entity_fts WHERE rowid IN \
                 (SELECT rowid FROM entity WHERE id IN (SELECT id FROM del_ids))",
                [],
            )?;
        }
        transaction.execute(
            "DELETE FROM edge WHERE source_id IN (SELECT id FROM del_ids)",
            [],
        )?;
        transaction.execute(
            "DELETE FROM edge WHERE target_id IN (SELECT id FROM del_ids)",
            [],
        )?;
        let deleted = transaction.execute(
            "DELETE FROM entity WHERE id IN (SELECT id FROM del_ids)",
            [],
        )?;
        transaction.commit()?;
        Ok(deleted)
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
        let rows = stmt.query_map([], |row| {
            Ok((EntityId(row.get::<_, String>(0)?), row.get::<_, String>(1)?))
        })?;
        let mut map = HashMap::new();
        for row in rows {
            let (id, hash) = row?;
            map.insert(id, hash);
        }
        Ok(map)
    }

    fn get_all_embeddings(&self) -> Result<Vec<(EntityId, Vec<f32>, String)>> {
        // JOIN entity 过滤孤儿向量:delete_file_subtree 不再级联删向量(P1-C′ 断点续扫),
        // 真删除文件的向量行残留于此,读时排除,不进语义检索候选。text_hash 随行返回,
        // 供调用方(mcp/cli)过滤旧向量空间——旧空间向量与新版 query 余弦错标,宁可缺席。
        let mut stmt = self.connection.prepare(
            "SELECT e.entity_id, e.embedding, e.dim, e.text_hash FROM entity_embedding e \
             JOIN entity ON entity.id = e.entity_id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, blob, dim, text_hash) = row?;
            let vec: Vec<f32> = blob
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| f32::from_ne_bytes(*c))
                .collect();
            // 维度真校验(旧为 debug_assert,release 下静默放行):blob 字节数/4 必须等于
            // dim 列。损坏/截断的 blob(磁盘满部分写、未来 schema 变更)会产出错维向量,
            // 让余弦打分算错甚至越界 panic。跳过该行并告警,semantic_search 优雅降级。
            if vec.len() != dim as usize {
                eprintln!(
                    "[ri-diag] embedding dim mismatch for {}: blob={} values, dim={}; skipping",
                    id,
                    vec.len(),
                    dim
                );
                continue;
            }
            out.push((EntityId(id), vec, text_hash));
        }
        Ok(out)
    }

    fn set_cluster_info(&mut self, infos: &[ClusterInfo]) -> Result<()> {
        let transaction = self.connection.transaction()?;
        transaction.execute("DELETE FROM cluster_info", [])?;
        for c in infos {
            transaction.execute(
                "INSERT INTO cluster_info(cluster_id, label, cohesion, top_nodes, size)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    c.cluster_id as i64,
                    c.label,
                    c.cohesion,
                    serde_json::to_string(&c.top_nodes)?,
                    c.size as i64,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    fn get_cluster_info(&self) -> Result<Vec<ClusterInfo>> {
        let mut stmt = self.connection.prepare(
            "SELECT cluster_id, label, cohesion, top_nodes, size
             FROM cluster_info ORDER BY size DESC, cluster_id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            let top_nodes_raw: String = row.get(3)?;
            Ok(ClusterInfo {
                cluster_id: row.get::<_, i64>(0)? as u64,
                label: row.get(1)?,
                cohesion: row.get(2)?,
                top_nodes: serde_json::from_str(&top_nodes_raw).unwrap_or_default(),
                size: row.get::<_, i64>(4)? as u32,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    fn set_scan_meta(&mut self, meta: &[(String, String)]) -> Result<()> {
        // UPSERT:重复 scan 幂等覆盖同键。
        let transaction = self.connection.transaction()?;
        let mut stmt = transaction.prepare(
            "INSERT INTO scan_meta(key, value) VALUES (?1, ?2)
                      ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        )?;
        for (key, value) in meta {
            stmt.execute(params![key, value])?;
        }
        drop(stmt);
        transaction.commit()?;
        Ok(())
    }

    fn get_scan_meta(&self) -> Result<HashMap<String, String>> {
        let mut stmt = self
            .connection
            .prepare("SELECT key, value FROM scan_meta")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<rusqlite::Result<HashMap<_, _>>>()
            .map_err(Into::into)
    }

    fn health_counts(&self) -> Result<(usize, usize, usize)> {
        // 有向量实体按 entity JOIN 计数而非 entity_embedding 行数:孤儿向量(实体已删,
        // P1-C′ 断点续扫的有意保留)不算覆盖。fts 行数同理对照 entity 行数。
        self.connection
            .query_row(
                "SELECT (SELECT COUNT(*) FROM entity),
                    (SELECT COUNT(*) FROM entity_embedding ee JOIN entity e ON e.id = ee.entity_id),
                    (SELECT COUNT(*) FROM entity_fts)",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)? as usize,
                        row.get::<_, i64>(1)? as usize,
                        row.get::<_, i64>(2)? as usize,
                    ))
                },
            )
            .map_err(Into::into)
    }

    fn edge_health(&self) -> Result<Vec<(String, i64, String, usize)>> {
        let mut stmt = self.connection.prepare(
            "SELECT kind, resolved, COALESCE(json_extract(json, '$.evidence[0].classification'), 'none'), COUNT(*)
             FROM edge GROUP BY kind, resolved, 3 ORDER BY kind, resolved",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, usize>(3)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
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
        let (where_clause, terms) = self.search_predicates(&query);
        let name_idx = terms.len() + 1;
        let limit_idx = terms.len() + 2;
        let offset_idx = terms.len() + 3;
        // kind 权重第一关键字、精确同名第二(第五轮反馈 F4):此前精确同名优先 + CASE
        // 只列 6 个 Java kind,搜 "LoanTerm" 时一堆同名 field 霸占页首,前端仓
        // (function/vue_page…)全落 ELSE 档退化为字母序。权重表来自 model 的
        // search_rank(全 kind 覆盖),数据成员族默认沉底——精确找字段用 kind 过滤。
        let kind_case = EntityKind::SEARCH_RANK_SQL
            .iter()
            .map(|(kind, rank)| format!("WHEN '{kind}' THEN {rank}"))
            .collect::<Vec<_>>()
            .join(" ");
        let sql = format!(
            "SELECT e.json FROM entity e WHERE {where_clause} \
             ORDER BY CASE e.kind {kind_case} ELSE 99 END, \
             CASE WHEN lower(e.name) = lower(?{name_idx}) THEN 0 ELSE 1 END, \
             e.name \
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

    fn search_facets(&self, query: &SearchQuery) -> Result<Vec<(String, usize)>> {
        // 与 search 完全同口径的召回(共享谓词),仅把 SELECT json…LIMIT 换成按 kind
        // 分组计数——返回的是「过滤前」分面:kind/复杂度过滤是调用方的展示决策,
        // 分面回答的是「这个子串一共命中了什么」,帮调用方决定翻页还是收窄。
        let (where_clause, terms) = self.search_predicates(query);
        let sql = format!(
            "SELECT e.kind, COUNT(*) FROM entity e WHERE {where_clause} GROUP BY e.kind ORDER BY 2 DESC"
        );
        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(terms.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, usize>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
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
    use repo_intelligence_model::{EdgeKind, EntityKind, EvidenceClass};

    fn id(value: &str) -> EntityId {
        EntityId(value.to_string())
    }

    /// P2-6:query_sql 表名/列名首猜必错时,错误文案带 did-you-mean 近邻提示。
    #[test]
    fn query_errors_hint_nearest_schema_name() {
        let store = SqliteGraphStore::open_in_memory().unwrap();
        // 表名复数误猜(edges)→ 建议 edge,并附完整表清单
        let err = store
            .read_only_query("SELECT COUNT(*) FROM edges", 100)
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("Did you mean `edge`?"), "{msg}");
        assert!(msg.contains("Tables: cluster_info, edge, entity"), "{msg}");
        assert!(msg.contains("v_edge, v_entity"), "{msg}");
        assert!(msg.contains("`kind` is the discriminator"), "{msg}");
        // 列名误猜(edge_kind)→ 建议 kind;平局列出全部归属表(kind 在
        // edge/entity 都有,entity_embedding 的 entity_id 同距,不能瞎指一个)
        let err = store
            .read_only_query("SELECT edge_kind FROM edge LIMIT 1", 100)
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Did you mean `kind` (in table edge or entity"),
            "{msg}"
        );
        // 正常查询不受影响
        assert!(
            store
                .read_only_query("SELECT COUNT(*) FROM entity", 10)
                .is_ok()
        );
    }

    /// P1-2(大仓反馈):v_edge/v_entity 视图暴露 trace/query API 同名字段,
    /// 裸 SQL 不必再学 source_id/json 一套词汇表。
    #[test]
    fn views_expose_api_field_vocabulary() {
        let mut store = SqliteGraphStore::open_in_memory().unwrap();
        let cls = Entity::new(id("host"), EntityKind::Class, "Host", "src/Host.java")
            .with_evidence("src/Host.java", 1, 9, EvidenceClass::Fact, 1.0, "class");
        let m = Entity::new(id("run"), EntityKind::Method, "run", "src/Host.java#run")
            .with_evidence("src/Host.java", 2, 8, EvidenceClass::Fact, 1.0, "method");
        store
            .apply_patch(GraphPatch::add(vec![cls, m], Vec::new()))
            .unwrap();
        let call = Edge::new(id("run"), id("run2"), EdgeKind::Calls).with_evidence(
            "src/Host.java",
            5,
            5,
            EvidenceClass::Inferred,
            0.7,
            "cross-file call via injected field receiver (mirrored duplicate)",
        );
        // run2 实体不存在(v_edge JOIN entity 会过滤悬空边)——补一个最小实体
        let m2 = Entity::new(id("run2"), EntityKind::Method, "bar", "svc/Foo.java#bar")
            .with_evidence("svc/Foo.java", 3, 3, EvidenceClass::Fact, 1.0, "method");
        store
            .apply_patch(GraphPatch::add(vec![m2], vec![call]))
            .unwrap();
        let qr = store
            .read_only_query(
                "SELECT source, target, confidence, kind FROM v_edge WHERE kind='calls'",
                10,
            )
            .unwrap();
        let col = |name: &str| qr.columns.iter().position(|c| c == name).unwrap();
        assert_eq!(
            qr.rows[0][col("source")].as_str().unwrap(),
            "src/Host.java#run"
        );
        assert_eq!(
            qr.rows[0][col("target")].as_str().unwrap(),
            "svc/Foo.java#bar"
        );
        assert!((qr.rows[0][col("confidence")].as_f64().unwrap() - 0.7).abs() < 1e-6);
        let qr = store
            .read_only_query(
                "SELECT name, evidence_count FROM v_entity WHERE id='run'",
                10,
            )
            .unwrap();
        let col = |name: &str| qr.columns.iter().position(|c| c == name).unwrap();
        assert_eq!(qr.rows[0][col("evidence_count")].as_i64().unwrap(), 1);
    }

    #[test]
    fn v_edge_tentative_is_computed_not_null() {
        // 第五轮反馈 F1:v_edge.tentative 曾提取不存在的 `$.tentative` 恒 NULL,直查者
        // 把 NULL 当「全部 tentative」误读(injects 明明是 fact)。v0.1.53 起按
        // classification 复算,与 mcp edge_view 同一判定(阈值常量同源)。
        let mut store = SqliteGraphStore::open_in_memory().unwrap();
        let mk = |name: &str| {
            Entity::new(id(name), EntityKind::Method, name, format!("a/{name}")).with_evidence(
                "a.java",
                1,
                1,
                EvidenceClass::Fact,
                1.0,
                "m",
            )
        };
        let entities = vec![mk("m1"), mk("m2"), mk("m3"), mk("m4")];
        let edge = |target: &str, class: EvidenceClass, confidence: f32| {
            Edge::new(id("m1"), id(target), EdgeKind::Calls)
                .with_evidence("a.java", 1, 1, class, confidence, "t")
        };
        let edges = vec![
            edge("m2", EvidenceClass::Fact, 1.0),
            edge("m3", EvidenceClass::Resolved, 0.7), // calls 精确解析档 → 不 tentative
            edge("m4", EvidenceClass::Inferred, 0.5), // 裸名档 → tentative
        ];
        store.apply_patch(GraphPatch::add(entities, edges)).unwrap();
        let qr = store
            .read_only_query(
                "SELECT target, evidence_class, tentative FROM v_edge WHERE kind='calls' ORDER BY target",
                10,
            )
            .unwrap();
        let col = |name: &str| qr.columns.iter().position(|c| c == name).unwrap();
        let row = |i: usize| {
            (
                qr.rows[i][col("target")].as_str().unwrap().to_string(),
                qr.rows[i][col("evidence_class")]
                    .as_str()
                    .unwrap()
                    .to_string(),
                qr.rows[i][col("tentative")].as_i64().unwrap(),
            )
        };
        assert_eq!(row(0), ("a/m2".into(), "fact".into(), 0));
        assert_eq!(row(1), ("a/m3".into(), "resolved".into(), 0));
        assert_eq!(row(2), ("a/m4".into(), "inferred".into(), 1));
    }

    #[test]
    fn search_ranks_structure_kinds_above_fields_and_facets_count_hits() {
        // 第五轮反馈 F4:kind 权重第一关键字——精确同名 field 不再霸占页首,前端
        // kind(function)有明确权重不再落 ELSE 档;facet 给 total + per-kind 计数。
        let mut store = SqliteGraphStore::open_in_memory().unwrap();
        let field = Entity::new(
            id("f"),
            EntityKind::Field,
            "LoanTerm",
            "a/Dto.java#LoanTerm",
        )
        .with_evidence("a/Dto.java", 1, 1, EvidenceClass::Fact, 1.0, "f");
        let class = Entity::new(id("c"), EntityKind::Class, "LoanTermConfig", "b/Cfg.java")
            .with_evidence("b/Cfg.java", 1, 1, EvidenceClass::Fact, 1.0, "c");
        let func = Entity::new(id("fn"), EntityKind::Function, "useLoanTerm", "c/u.ts")
            .with_evidence("c/u.ts", 1, 1, EvidenceClass::Fact, 1.0, "fn");
        store
            .apply_patch(GraphPatch::add(vec![field, class, func], Vec::new()))
            .unwrap();
        let hits = store
            .search(SearchQuery::new("LoanTerm").with_limit(10))
            .unwrap();
        let kinds: Vec<&str> = hits.iter().map(|m| m.entity.kind.as_str()).collect();
        // class(1) < function(4) < field(12):结构实体在前,精确同名 field 沉底
        assert_eq!(kinds, vec!["class", "function", "field"], "排序: {kinds:?}");
        // facet:过滤前口径的 per-kind 计数,总和 = 命中总数
        let facets = store.search_facets(&SearchQuery::new("LoanTerm")).unwrap();
        let total: usize = facets.iter().map(|(_, n)| n).sum();
        assert_eq!(total, 3);
        assert!(facets.contains(&("class".to_string(), 1)));
        assert!(facets.contains(&("field".to_string(), 1)));
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
    fn scan_meta_roundtrips_and_health_counts_track_coverage() {
        // 第五轮反馈 F5:scan 摘要落库 + 健康度三计数。空库 meta 为空;写入后 UPSERT
        // 幂等;embedding 覆盖按 entity JOIN 计数(孤儿向量不算),fts 行数对照实体数。
        let mut store = SqliteGraphStore::open_in_memory().unwrap();
        assert!(store.get_scan_meta().unwrap().is_empty());
        store
            .set_scan_meta(&[
                ("finished_at".to_string(), "1770000000".to_string()),
                ("files_indexed".to_string(), "790".to_string()),
            ])
            .unwrap();
        // 同键覆盖
        store
            .set_scan_meta(&[("files_indexed".to_string(), "791".to_string())])
            .unwrap();
        let meta = store.get_scan_meta().unwrap();
        assert_eq!(meta.get("files_indexed").unwrap(), "791");
        assert_eq!(meta.len(), 2);

        // 健康计数:2 实体、1 向量(悬空向量指向不存在实体,不计)、fts 应与实体同步
        let a = Entity::new(id("a"), EntityKind::Class, "A", "a/A").with_evidence(
            "a.java",
            1,
            1,
            EvidenceClass::Fact,
            1.0,
            "c",
        );
        let b = Entity::new(id("b"), EntityKind::Class, "B", "a/B").with_evidence(
            "a.java",
            1,
            1,
            EvidenceClass::Fact,
            1.0,
            "c",
        );
        store
            .apply_patch(GraphPatch::add(vec![a, b], Vec::new()))
            .unwrap();
        store
            .set_embeddings(&[
                (id("a"), vec![0.1, 0.2], "model:h1".to_string()),
                (id("ghost"), vec![0.3, 0.4], "model:h2".to_string()),
            ])
            .unwrap();
        let (entities, embedded, fts_rows) = store.health_counts().unwrap();
        assert_eq!(entities, 2);
        assert_eq!(embedded, 1, "孤儿向量不计覆盖");
        assert_eq!(fts_rows, entities, "apply_patch 后 fts 与实体同步");

        // 边健康分布:kind × resolved × classification
        let call = Edge::new(id("a"), id("b"), EdgeKind::Calls).with_evidence(
            "a.java",
            2,
            2,
            EvidenceClass::Resolved,
            0.7,
            "t",
        );
        store
            .apply_patch(GraphPatch::add(Vec::new(), vec![call]))
            .unwrap();
        let health = store.edge_health().unwrap();
        assert!(
            health
                .iter()
                .any(|(kind, resolved, class, count)| kind == "calls"
                    && *resolved == 0
                    && class == "resolved"
                    && *count == 1),
            "edge_health: {health:?}"
        );
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
        let entity = Entity::new(id("C"), EntityKind::Class, "C", "C").with_evidence(
            "F.java",
            42,
            48,
            EvidenceClass::Fact,
            1.0,
            "decl",
        );
        store
            .apply_patch(GraphPatch::add(vec![entity], Vec::new()))
            .unwrap();
        let (start, end): (i64, i64) = store
            .connection
            .query_row(
                "SELECT start_line, end_line FROM entity WHERE id = 'C'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
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

    /// P1-C′(第四轮反馈):向量不随子树删除——中断/升级重跑时 text_hash 筛选靠存量
    /// 向量命中跳过,级联删除会让「分批落库 + 中断保留」设计失效(全量重算)。
    #[test]
    fn delete_file_subtree_keeps_embeddings_for_resume() {
        use repo_intelligence_model::{Entity, EntityKind};
        let mut store = SqliteGraphStore::open_in_memory().unwrap();
        let f = Entity::new(id("file:F"), EntityKind::File, "F.java", "F.java");
        let c = Entity::new(id("C"), EntityKind::Class, "C", "C");
        store
            .apply_patch(GraphPatch::add(
                vec![f, c],
                vec![Edge::new(id("file:F"), id("C"), EdgeKind::Contains)],
            ))
            .unwrap();
        store
            .set_embeddings(&[(id("C"), vec![0.1, 0.2], "model:hash1".to_string())])
            .unwrap();

        store.delete_file_subtree(&id("file:F")).unwrap();

        let state = store.get_embedding_state().unwrap();
        assert!(
            state.contains_key(&id("C")),
            "向量应随 EntityId 保留(断点续扫前提): {state:?}"
        );
    }

    /// 孤儿向量(实体已删)不进语义检索候选——get_all_embeddings 读时 JOIN entity。
    #[test]
    fn get_all_embeddings_filters_orphan_rows() {
        use repo_intelligence_model::{Entity, EntityKind};
        let mut store = SqliteGraphStore::open_in_memory().unwrap();
        let f = Entity::new(id("file:F"), EntityKind::File, "F.java", "F.java");
        let c = Entity::new(id("C"), EntityKind::Class, "C", "C");
        let g = Entity::new(id("G"), EntityKind::Class, "G", "G");
        store
            .apply_patch(GraphPatch::add(
                vec![f, c.clone(), g],
                vec![
                    Edge::new(id("file:F"), id("C"), EdgeKind::Contains),
                    Edge::new(id("Y"), id("G"), EdgeKind::Contains),
                ],
            ))
            .unwrap();
        store
            .set_embeddings(&[
                (id("C"), vec![0.1, 0.2], "h1".to_string()),
                (id("G"), vec![0.3, 0.4], "h2".to_string()),
            ])
            .unwrap();
        assert_eq!(store.get_all_embeddings().unwrap().len(), 2);

        store.delete_file_subtree(&id("file:F")).unwrap();

        let all = store.get_all_embeddings().unwrap();
        assert_eq!(all.len(), 1, "孤儿 C 的向量不进候选: {all:?}");
        assert!(all.iter().any(|(eid, _, _)| *eid == id("G")));
    }

    /// 批量删除 = 逐个删除的等价性守卫(实体/边/FTS 三层全等)。
    #[test]
    fn delete_file_subtrees_batch_matches_per_file_deletion() {
        use repo_intelligence_model::{Entity, EntityKind};
        let build = || {
            let f1 = Entity::new(id("file:F1"), EntityKind::File, "F1.java", "F1.java");
            let f2 = Entity::new(id("file:F2"), EntityKind::File, "F2.java", "F2.java");
            let f3 = Entity::new(id("file:F3"), EntityKind::File, "F3.java", "F3.java");
            let c1 = Entity::new(id("C1"), EntityKind::Class, "AlphaService", "C1");
            let c2 = Entity::new(id("C2"), EntityKind::Class, "BetaMapper", "C2");
            let c3 = Entity::new(id("C3"), EntityKind::Class, "GammaUtil", "C3");
            let entities = vec![f1, f2, f3, c1.clone(), c2.clone(), c3.clone()];
            let edges = vec![
                Edge::new(id("file:F1"), id("C1"), EdgeKind::Contains),
                Edge::new(id("file:F2"), id("C2"), EdgeKind::Contains),
                Edge::new(id("file:F3"), id("C3"), EdgeKind::Contains),
                // 跨文件 resolved 边:C3 calls C2 —— 删 F2 或 F3 时应随端点清除。
                Edge::new(id("C3"), id("C2"), EdgeKind::Calls),
            ];
            (entities, edges)
        };
        let mut per_file = SqliteGraphStore::open_in_memory().unwrap();
        let mut batch = SqliteGraphStore::open_in_memory().unwrap();
        let (entities, edges) = build();
        per_file
            .apply_patch(GraphPatch::add(entities, edges))
            .unwrap();
        let (entities, edges) = build();
        batch.apply_patch(GraphPatch::add(entities, edges)).unwrap();

        let targets = [id("file:F1"), id("file:F3")];
        for file_id in &targets {
            per_file.delete_file_subtree(file_id).unwrap();
        }
        let deleted = batch.delete_file_subtrees_batch(&targets).unwrap();

        assert_eq!(deleted, 4, "两个文件 + 各自子类");
        assert_eq!(per_file.counts().unwrap(), batch.counts().unwrap());
        let fts_of = |store: &SqliteGraphStore| -> Vec<String> {
            let mut stmt = store
                .connection
                .prepare("SELECT entity_id FROM entity_fts ORDER BY entity_id")
                .unwrap();

            stmt.query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(fts_of(&per_file), fts_of(&batch));
        // F2 的 BetaMapper 仍在,两边召回一致。
        for store in [&per_file, &batch] {
            let hits = store.search(SearchQuery::new("BetaMapper")).unwrap();
            assert_eq!(hits.len(), 1);
        }
    }

    /// 批量删除后 FTS 与 entity 行数一致 + rowid 对齐不变量不被破坏
    /// (对齐是 rowid 删除路径的正确性前提,断言写死防回归)。
    #[test]
    fn batch_deletion_keeps_fts_in_sync_and_rowid_aligned() {
        use repo_intelligence_model::{Entity, EntityKind};
        let aligned_count = |store: &SqliteGraphStore| -> (i64, i64, i64) {
            store
                .connection
                .query_row(
                    "SELECT
                       (SELECT COUNT(*) FROM entity),
                       (SELECT COUNT(*) FROM entity_fts),
                       (SELECT COUNT(*) FROM entity_fts f
                        JOIN entity e ON e.rowid = f.rowid AND e.id = f.entity_id)",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap()
        };
        let mut store = SqliteGraphStore::open_in_memory().unwrap();
        let f1 = Entity::new(id("file:F1"), EntityKind::File, "F1.java", "F1.java");
        let f2 = Entity::new(id("file:F2"), EntityKind::File, "F2.java", "F2.java");
        let c1 = Entity::new(id("C1"), EntityKind::Class, "AlphaService", "C1");
        let c2 = Entity::new(id("C2"), EntityKind::Class, "BetaMapper", "C2");
        store
            .apply_patch(GraphPatch::add(
                vec![f1, f2, c1, c2],
                vec![
                    Edge::new(id("file:F1"), id("C1"), EdgeKind::Contains),
                    Edge::new(id("file:F2"), id("C2"), EdgeKind::Contains),
                ],
            ))
            .unwrap();
        let (entities, fts, aligned) = aligned_count(&store);
        assert_eq!((entities, fts, aligned), (4, 4, 4), "写路径维持 rowid 对齐");

        store
            .delete_file_subtrees_batch(&[id("file:F1"), id("file:F2")])
            .unwrap();

        let (entities, fts, aligned) = aligned_count(&store);
        assert_eq!(entities, 0, "全部实体随文件删除");
        assert_eq!(fts, 0, "FTS 行同步删除,无孤儿(免 reconcile 重建)");
        assert_eq!(aligned, 0);
    }

    /// patch 内重复实体 id(同实体多 patch 合并)不得让 FTS rowid 插入炸 PK
    /// 冲突,也不得留重复 fts 行(旧行为的漂移源)。
    #[test]
    fn write_patch_tolerates_duplicate_entity_ids_in_fts() {
        use repo_intelligence_model::{Entity, EntityKind};
        let mut store = SqliteGraphStore::open_in_memory().unwrap();
        let a1 = Entity::new(id("C1"), EntityKind::Class, "AlphaService", "C1");
        let a2 = Entity::new(id("C1"), EntityKind::Class, "AlphaService", "C1");
        let b = Entity::new(id("C2"), EntityKind::Class, "BetaMapper", "C2");
        // C1 重复出现 900+ 次,保证跨块(fts_bulk_in 按 900 分块)重复插入同 rowid。
        let mut entities = vec![a1, b];
        for _ in 0..950 {
            entities.push(a2.clone());
        }
        store
            .apply_patch(GraphPatch::add(entities, Vec::new()))
            .unwrap();

        let (entity_count, fts_count): (i64, i64) = store
            .connection
            .query_row(
                "SELECT (SELECT COUNT(*) FROM entity), (SELECT COUNT(*) FROM entity_fts)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (entity_count, fts_count),
            (2, 2),
            "重复 id 不炸且 fts 无重复行"
        );
    }

    /// 旧库存量 fts 行 rowid 与 entity.rowid 脱钩(FTS5 自动分配),open 后由
    /// ensure_fts_rowid_aligned 一次性重建对齐,并对后续 rowid 删除路径生效。
    #[test]
    fn legacy_fts_rowids_are_realigned_on_migrate() {
        use repo_intelligence_model::{Entity, EntityKind};
        let mut store = SqliteGraphStore::open_in_memory().unwrap();
        let f = Entity::new(id("file:F"), EntityKind::File, "F.java", "F.java");
        let c = Entity::new(id("C"), EntityKind::Class, "AlphaService", "C");
        let g = Entity::new(id("G"), EntityKind::Class, "BetaMapper", "G");
        store
            .apply_patch(GraphPatch::add(
                vec![f, c, g],
                vec![
                    Edge::new(id("file:F"), id("C"), EdgeKind::Contains),
                    Edge::new(id("file:F"), id("G"), EdgeKind::Contains),
                ],
            ))
            .unwrap();
        // 手工降级成旧形态:fts 不带 rowid 重插(ORDER BY id 制造与 entity 插入序
        // 的错位)+ user_version 清零,模拟 0.1.51 之前的存量库。
        store
            .connection
            .execute_batch(
                "PRAGMA user_version = 0;
                 DELETE FROM entity_fts;
                 INSERT INTO entity_fts(entity_id, name, qualified_name)
                 SELECT id, name, qualified_name FROM entity ORDER BY id;",
            )
            .unwrap();
        let misaligned: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM entity_fts f \
                 JOIN entity e ON e.rowid = f.rowid AND e.id = f.entity_id",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(misaligned < 3, "错位构造失败,测试前提不成立: {misaligned}");

        store.ensure_fts_rowid_aligned().unwrap();

        let aligned: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM entity_fts f \
                 JOIN entity e ON e.rowid = f.rowid AND e.id = f.entity_id",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(aligned, 3, "迁移后全部行 rowid 对齐");
        let version: i64 = store
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 1, "版本标记推进,迁移只发生一次");
        // 对齐后 rowid 删除路径删的是正确的行:删子树后 fts 无 C 残留。
        store.delete_file_subtree(&id("file:F")).unwrap();
        let residual: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM entity_fts WHERE entity_id = 'C'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(residual, 0, "被删实体的 fts 行必须清掉(错位则会残留他行)");
    }

    /// 大仓规模基准(手动跑:`cargo test --release -p repo-intelligence-graph
    /// -- --ignored large_scale`):mes-activity 量级 3 万文件 × 15 实体 = 45 万实体,
    /// 全量重提的删除阶段。修复前(逐文件 × FTS 全表扫)外推约小时级;修复后
    /// 应在分钟内。不断言硬时限(防 CI flaky),打印耗时供人工判读。
    #[test]
    #[ignore]
    fn large_scale_delete_bench() {
        use repo_intelligence_model::{Entity, EntityKind};
        use std::time::Instant;
        let mut store = SqliteGraphStore::open_in_memory().unwrap();
        const FILES: usize = 30_000;
        const CHILDREN: usize = 15;
        let t_build = Instant::now();
        for batch in 0..(FILES / 1_000) {
            let mut entities = Vec::with_capacity(1_000 * (CHILDREN + 1));
            let mut edges = Vec::with_capacity(1_000 * CHILDREN);
            for f in (batch * 1_000)..((batch + 1) * 1_000) {
                let file_id = id(&format!("file:F{f}"));
                entities.push(Entity::new(
                    file_id.clone(),
                    EntityKind::File,
                    format!("F{f}.java"),
                    format!("F{f}.java"),
                ));
                for c in 0..CHILDREN {
                    let child = id(&format!("C{f}_{c}"));
                    entities.push(Entity::new(
                        child.clone(),
                        EntityKind::Class,
                        format!("Class{f}_{c}"),
                        format!("pkg{}.Class{f}_{c}", f % 500),
                    ));
                    edges.push(Edge::new(file_id.clone(), child, EdgeKind::Contains));
                }
            }
            store.apply_patch(GraphPatch::add(entities, edges)).unwrap();
        }
        let fts_of = |store: &SqliteGraphStore| -> i64 {
            store
                .connection
                .query_row("SELECT COUNT(*) FROM entity_fts", [], |row| row.get(0))
                .unwrap()
        };
        let (entity_count, _) = store.counts().unwrap();
        eprintln!(
            "[bench] built {entity_count} entities / {} fts rows in {:.1}s",
            fts_of(&store),
            t_build.elapsed().as_secs_f32()
        );
        assert_eq!(entity_count, (FILES * (CHILDREN + 1)) as u64);

        let file_ids: Vec<_> = (0..FILES).map(|f| id(&format!("file:F{f}"))).collect();
        let t_del = Instant::now();
        let mut deleted = 0usize;
        for chunk in file_ids.chunks(2_000) {
            deleted += store.delete_file_subtrees_batch(chunk).unwrap();
        }
        eprintln!(
            "[bench] deleted {deleted} entities ({} files) in {:.1}s",
            FILES,
            t_del.elapsed().as_secs_f32()
        );
        let (entity_count, _) = store.counts().unwrap();
        assert_eq!(entity_count, 0);
        assert_eq!(fts_of(&store), 0, "fts 无孤儿残留");
    }
}
