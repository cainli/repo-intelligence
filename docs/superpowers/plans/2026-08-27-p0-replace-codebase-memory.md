# P0 替代 codebase-memory 四件套(v0.1.36)实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 补齐 RI 替代 codebase-memory 的四个 P0 缺口——①token 瘦身(edge_view 全量 evidence + 47.9KB tools/list)②MCP 只读 SQL 直通查询 ③前端对象参数形式 HTTP 调用识别 ④真仓库上失灵的 tests 边改用 import 推断。

**Architecture:** 全部改动落在既有管线模式内:mcp 层纯响应整形与工具注册;graph 层加一个只读查询方法;semantics 层沿用"LazyLock 正则提 metadata"惯例新增对象形态 HTTP 调用与测试类 imports;analysis 层沿用"metadata → 跨文件建边 + A+ 歧义防护"惯例补 tests 边的第二推断路径。不引入新依赖、不改索引格式版本(EntityId 方案未动,无需 v2 强制重索引)。

**Tech Stack:** Rust(workspace),rusqlite,regex,serde_json,tree-sitter-java(已存在)。基准项目:`验证项目/ruoyi-vue-plus`(CLAUDE.md 强制)。

**关键背景事实(2026-08-27 实测,实现者可直接信任):**

| 事实 | 来源 |
|---|---|
| `tools/list` 总字节 **47936**,17 工具,trace_* 四件占 ~22KB | stdio probe 实测 |
| `edge_view`(crates/mcp/src/lib.rs:1549)每条边全量携带 `evidence[]`;默认 limit=50 时显著放大响应 | lib.rs:1567 |
| fixture 中 `request({ url: '/api/users/list', method: 'get' })` 未产生 http_client_call —— HTTP_CALL 正则只认 `verb('/url')` 位置参数形态 | /tmp/vue-mini 实测 |
| ruoyi 有 4 个测试类实体(ParamUnitTest 等)+ 11 个 @Test 方法实体(test_case),但 tests 边=0:命名约定要求存在同名去 Test 的类(不存在);测试类也无 depends_on 边 | 直连库实测 |
| java.rs 已有 `AT_TEST` 正则(60 行附近)提取 TestCase 实体、`metadata.superclass` 写入先例(1037-1052 行) | crates/semantics/src/java.rs |
| `SqliteGraphStore` 持有单个 `connection: Connection`,无 Mutex 包装 | crates/graph/src/lib.rs:57 |

---

## File Structure(改动全景)

```
crates/mcp/src/lib.rs            # T1: edge_view verbose 门控 + evidence_compact 助手 + trace 工具 verbose 参数
                                 # T2: 全部 17 条工具描述重写(短文案)+ query_sql 新 ToolSpec + dispatch 分支
crates/graph/src/lib.rs          # T2: SqliteGraphStore::read_only_query()
crates/model/src/lib.rs          # T2: QueryResult 结构体(columns/rows/truncated)
crates/semantics/src/frontend.rs # T3: HTTP_CALL_OBJECT 正则 + extract 循环
crates/semantics/src/java.rs     # T4: JAVA_IMPORT 正则,test 类 metadata.imports
crates/analysis/src/lib.rs       # T4: tests 边 import 推断分支(~1370 行命名约定块之后)
scripts/measure_mcp_tokens.sh    # T2: stdio probe 测量脚本(新文件)
验证项目/ruoyi-vue-plus/.repo-intelligence/workspace.sqlite   # 各任务后的强制真项目指标
/tmp/vue-mini                    # T3 的端到端跨栈验证(fixture 已存在于本机)
```

---

## Task 1: 响应 token 瘦身(evidence 门控)

**Files:**
- Modify: `crates/mcp/src/lib.rs`(edge_view ~1549 行、trace_graph 装配段、edge_schema ~118-124 行、finding_schema 及其装配点)

**设计决策:** 紧凑视图丢掉整段 `evidence[]` 数组,换成一个定长小对象 `{count, first:{file,line}}`。保留第一条的 file:line 锚点是因为消费方(LLM)至少需要一个"这可信吗、来自哪"的最小依据;完整数组只在 `verbose=true` 时出现。

- [ ] **Step 1.1: 写失败测试 —— edge_view 紧凑模式不含全量 evidence**

在 `crates/mcp/src/lib.rs` 的 `#[cfg(test)] mod tests` 内(现有 `edge_view_marks_tentative_by_classification_and_confidence` 测试旁,~2127 行后追加):

```rust
#[test]
fn edge_view_compact_mode_omits_full_evidence_array() {
    let mut edge = Edge::new(
        EntityId("a".into()),
        EntityId("b".into()),
        EdgeKind::Calls,
    );
    for i in 0..3 {
        edge = edge.with_evidence(&format!("f{i}.java"), i + 1, i + 1, EvidenceClass::Fact, 1.0, "long reason text");
    }
    let compact = edge_view(&edge, false);
    // 紧凑模式:evidence_count 表示总量,不再序列化完整数组
    assert_eq!(compact["evidence_count"], 3);
    assert!(compact["evidence"].is_null(), "compact view must not carry full evidence[]");
    let first = &compact["evidence_first"];
    assert_eq!(first["file"], "f0.java");
    assert_eq!(first["start_line"], 1);

    let verbose = edge_view(&edge, true);
    assert_eq!(verbose["evidence"].as_array().unwrap().len(), 3);
}
```

注意:现测试 `edge_view_marks_tentative_by_classification_and_confidence` 里 6 处 `edge_view(&x)` 改为 `edge_view(&x, true)`(行为断言不变,走 verbose 路径保持原语义被覆盖)。

- [ ] **Step 1.2: 跑测试确认失败**

Run: `cargo test -p repo-intelligence-mcp edge_view -- --nocapture`
Expected: 编译失败 —— `edge_view` 尚无第二参数。

- [ ] **Step 1.3: 实现 —— 新签名 + evidence_compact 助手**

替换 `fn edge_view`(lib.rs:1549)整体为:

```rust
/// 边的协议视图。verbose=false(默认)为紧凑档:不带完整 `evidence[]`
/// (reason 长文本是 trace 大响应的主因),仅给 `evidence_count` +
/// 第一条证据的 file:line 锚点;verbose=true 才展开全部证据。
fn edge_view(edge: &Edge, verbose: bool) -> Value {
    let evidence = edge.evidence.first();
    let confidence = evidence.map(|item| item.confidence).unwrap_or(1.0);
    let tentative = match evidence.map(|item| item.classification) {
        Some(EvidenceClass::Fact) => false,
        Some(EvidenceClass::Resolved) => confidence < 0.8,
        Some(_) | None => true,
    };
    let mut view = json!({
        "source": edge.source.0,
        "target": edge.target.0,
        "kind": edge.kind.as_str(),
        "edge_kind": edge.kind.as_str(),
        "confidence": confidence,
        "tentative": tentative,
        "evidence_count": edge.evidence.len(),
    });
    match verbose {
        true => view["evidence"] =
            serde_json::to_value(&edge.evidence).unwrap_or_default(),
        false => {
            if let Some(first) = edge.evidence.first() {
                view["evidence_first"] = json!({
                    "file": first.file,
                    "line": first.start_line,
                });
            }
        }
    }
    view
}
```

同文件新增助手(供 findings 复用,DRY):

```rust
/// finding 上的证据紧凑视图:count + 首条锚点。与 edge_view 同策略。
fn compact_evidence(evidence: &[repo_intelligence_graph::Evidence]) -> Value {
    match evidence.first() {
        None => json!({"count": 0}),
        Some(first) => json!({
            "count": evidence.len(),
            "first": {"file": first.file, "line": first.start_line},
        }),
    }
}
```

> 实现者注意:`Evidence` 的真实引用路径以文件顶部 use 为准(lib.rs 顶部已 `use` 了 Edge/EvidenceClass 同族类型);若 `repo_intelligence_graph::Evidence` 不存在,直接用当前代码里 evidence 参数的类型别名,fingerprint 以编译器为准。`Evidence` 类型若实际定义在 model crate,则改为该路径——先 `rg -n 'pub struct Evidence' crates/` 确认。

跑编译器找出全部调用点并穿参:

Run: `cargo build -p repo-intelligence-mcp 2>&1 | grep -E '^error|-->' | head -20`
Expected: 报出 trace_graph 等处调用。逐个把布尔值传下去:trace 类工具内部一律 `false`,后续 Step 1.5 再接 verbose 输入。

- [ ] **Step 1.4: 更新 outputSchema —— edge_schema/findings 的 evidence 字段改可选并新增 evidence_count**

`edge_schema()`(lib.rs ~118-124)中 `"evidence"` 从 `required` 列表移除,并加入:

```rust
"evidence_count": {"type": "integer", "minimum": 0},
```

同样处理 finding_schema()(~81、~99 行两处):required 移除 `evidence`,加 `evidence_count`。然后在 findings 装配点(定位方式:Run `rg -n '"evidence"' crates/mcp/src/lib.rs` 找非 schema 函数体内的赋值点)把 `.map(|f| json!({... "evidence": serde_json::to_value(&f.evidence)...}))` 形态改为塞入 `compact_evidence(&f.evidence)`。

- [ ] **Step 1.5: trace_* 四工具透传 verbose**

在四个 trace 工具的 `input_schema`(`trace_input`,`"min_confidence"` 属性之后)追加:

```rust
"verbose": {"type": "boolean", "default": false, "description": "Include full evidence[] per edge."}
```

解析侧在每个 trace 分支(`trace_graph` 调用处 ~1580 起):`let verbose = arguments["verbose"].as_bool().unwrap_or(false);`,把其中边装配循环里对 `edge_view(&edge)` 的调用改为 `edge_view(&edge, verbose)`。

- [ ] **Step 1.6: 全量测试回归**

Run: `cargo test --workspace`
Expected: 全绿(含改造过的旧 edge_view 断言)。

- [ ] **Step 1.7: 实测响应字节下降(真库)**

Run:
```bash
printf '%s\n%s\n%s\n' \
 '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"probe","version":"0"}}}' \
 '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
 '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"trace_callees","arguments":{"name":"SysUserServiceImpl","depth":2}}}' \
 | ./target/debug/repo-intelligence --database 验证项目/ruoyi-vue-plus/.repo-intelligence/workspace.sqlite mcp 2>/dev/null | tail -1 | wc -c
```
Expected: 显著小于改动前(改动前该调用 ≈ 数百 KB 量级;记录具体数字写入 Task 6 CHANGELOG)。若报"找不到 SysUserServiceImpl",先用 `search_entities` 确认真的名字再换。

- [ ] **Step 1.8: Commit**

```bash
git add crates/mcp/src/lib.rs
git commit -m "feat(mcp): 边/finding 证据默认紧凑视图,evidence[] 仅 verbose=true 展开"
```

---

## Task 2: query_sql 只读直通工具 + tools/list 文案瘦身

**Files:**
- Modify: `crates/model/src/lib.rs`(QueryResult)
- Modify: `crates/graph/src/lib.rs`(read_only_query)
- Modify: `crates/mcp/src/lib.rs`(ToolSpec 注册、dispatch 分支、17 条描述全文替换)
- Create: `scripts/measure_mcp_tokens.sh`

**安全模型:** 语句级三重防线 —— ①前缀必须 `select`/`with`;②整条语句禁止出现 `;`(防堆叠语句;子查询不需要分号);③行数封顶 + 单元格截断。sqlite 连接本身仍是普通读写连接,但防线①②保证永远不产生写语义(`WITH` 不带 INSERT/UPDATE 子句时只能读)。

### Part A: QueryResult + read_only_query

- [ ] **Step 2A.1: 写失败测试(graph 层)**

在 `crates/graph/tests/`(已有集成测试文件任选,或 vertical_slice 所在目录)新增:

```rust
#[test]
fn read_only_query_rejects_non_select_and_returns_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.sqlite");
    let store = SqliteGraphStore::open(&db).unwrap();
    // 拒绝写语句
    assert!(store.read_only_query("DELETE FROM entity", 10).is_err());
    assert!(store.read_only_query("select 1; drop table entity", 10).is_err());
    assert!(store.read_only_query("ATTACH DATABASE '/tmp/x' AS x", 10).is_err());
    // 正常读取
    let r = store.read_only_query("SELECT 1 AS one, 'a' AS txt", 10).unwrap();
    assert_eq!(r.columns, vec!["one".to_string(), "txt".to_string()]);
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], serde_json::json!(1));
    assert_eq!(r.truncated, false);
}
```

> 若 graph crate 测试目录无 tempfile dev-dependency,先看 `crates/graph/Cargo.toml [dev-dependencies]`——已有同类用法就照抄;没有则在 dev-dependencies 加 `tempfile = "3"`。

- [ ] **Step 2A.2: 跑测试确认失败**

Run: `cargo test -p repo-intelligence-graph read_only_query`
Expected: FAIL —— 方法不存在。

- [ ] **Step 2A.3: 实现 model::QueryResult**

`crates/model/src/lib.rs` 追加(放文件尾部公共结构区):

```rust
/// query_sql 的返回:列名 + 已序列化为 JSON 的行 + 是否截断。
#[derive(Debug, Clone)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<serde_json::Value>>,
    pub truncated: bool,
}
```

- [ ] **Step 2A.4: 实现 SqliteGraphStore::read_only_query**

`crates/graph/src/lib.rs` 的 `impl SqliteGraphStore` 内追加:

```rust
/// 只读 SQL 直通(query_sql 工具的后端)。防线:
/// ① trim 后必须以 select/with 开头(大小写不敏感);
/// ② 整条语句不允许出现分号(拒绝堆叠语句);
/// ③ ATXACH/PRAGMA/写入关键词黑名单双保险;
/// ④ 行数超 max_rows 截断并把 truncated=true,单元格文本超 400 字符截断。
/// query_only PRAGMA 兜底:仅本次调用期间打开,物理阻断任何写语义。
pub fn read_only_query(&self, sql: &str, max_rows: usize) -> Result<crate::QueryResult> {
    use crate::{model 之外可见性按实际 }; // ← 实现者按下方说明修正 use
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let lower = trimmed.to_ascii_lowercase();
    anyhow::ensure!(
        lower.starts_with("select") || lower.starts_with("with"),
        "only SELECT/WITH statements are allowed"
    );
    anyhow::ensure!(!trimmed.contains(';'), "multiple statements are not allowed");
    for banned in ["insert ", "update ", "delete ", "drop ", "alter ", "attach ",
                   "pragma ", "create ", "vacuum ", "reindex "] {
        anyhow::ensure!(
            !lower.contains(banned),
            "keyword {banned:?} is not allowed in read-only queries"
        );
    }
    self.connection.execute_batch("PRAGMA query_only = ON")?;
    let result = self.run_select(trimmed, max_rows);
    self.connection.execute_batch("PRAGMA query_only = OFF")?;
    result
}

fn run_select(&self, sql: &str, max_rows: usize) -> Result<crate::QueryResult> {
    use rusqlite::types::ValueRef;
    let mut stmt = self.connection.prepare(sql)?;
    let columns: Vec<String> =
        stmt.column_names().iter().map(|c| c.to_string()).collect();
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
                        s = format!("{}…({} chars)", &s[..200], s.chars().count());
                    }
                    serde_json::json!(s)
                }
                ValueRef::Blob(b) => serde_json::json!(format!("<blob {} bytes>", b.len())),
            });
        }
        rows.push(out);
    }
    Ok(crate::QueryResult { columns, rows, truncated })
}
```

> 说明:①函数开头那句伪 use 是占位提醒,**删除它**;QueryResult 通过 `crate::QueryResult` 引用(graph crate 需要 `pub use repo_intelligence_model::QueryResult;` 或直接写全路径 `repo_intelligence_model::QueryResult`——照 crates/graph 现有对 model 类型的引用惯例来,`rg -n 'use repo_intelligence_model' crates/graph/src/lib.rs` 参考首部)。②rusqlite Text 切片处需字符边界安全:先 `let s_lossy: String` 再取前 200 chars 的实现如上即可满足。

- [ ] **Step 2A.5: 跑测试确认通过**

Run: `cargo test -p repo-intelligence-graph read_only_query`
Expected: PASS

### Part B: MCP 工具注册 + dispatch

- [ ] **Step 2B.1: 写失败测试(mcp 层路由)**

```rust
#[test]
fn query_sql_routes_through_dispatch_with_cap() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("w.sqlite");
    {
        let store = SqliteGraphStore::open(&db_path).unwrap();
        let _ = store; // open 即建表
    }
    let req = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "query_sql",
                   "arguments": {"sql": "SELECT COUNT(*) AS n FROM entity"}}
    });
    let resp = call_tool_from_request(&db_path, &req);
    let rows = resp["result"]["structuredContent"]["rows"].as_array().unwrap();
    assert_eq!(rows[0]["n"], 0_i64_or_as_published());
}
```

> 这里 `call_tool_from_request` 是示意名:现有测试已有通过 `call_tool(request, database, base)` 打桩的模式(`rg -n 'call_tool\(' crates/mcp/src/lib.rs` 参考 tests 区两个既有用例的真实签名后照抄其构造方式);`rows[0]["n"]` 的取值形态对齐 structuredContent 序列化约定(见 Step 2B.3 输出形状)。

- [ ] **Step 2B.2: 跑测试确认失败**

Run: `cargo test -p repo-intelligence-mcp query_sql`
Expected: FAIL —— 无此工具。

- [ ] **Step 2B.3: 注册 ToolSpec(schema 必须遵守新基准:单工具 ≤ 1200 字节)**

在 `tool_specs()` 追加:

```rust
ToolSpec {
    name: "query_sql",
    description: "Read-only SQL over the index SQLite (entity/edge tables). Full SELECT/WITH freedom incl. joins, aggregates, CTEs. Writes and multiple statements rejected.",
    input_schema: json!({
        "type": "object", "additionalProperties": false,
        "properties": {
            "sql": {"type": "string", "description": "Single SELECT/WITH statement. No semicolons."},
            "max_rows": {"type": "integer", "minimum": 1, "maximum": 500, "default": 200}
        },
        "required": ["sql"]
    }),
    output_schema: json!({
        "type": "object", "additionalProperties": false,
        "properties": {
            "columns": {"type": "array", "items": {"type": "string"}},
            "rows": {"type": "array", "items": {"type": "array"}},
            "row_count": {"type": "integer"},
            "truncated": {"type": "boolean"}
        },
        "required": ["columns", "rows", "row_count", "truncated"]
    }),
}
```

dispatch 匹配臂(call_tool 的 match 处):

```rust
"query_sql" => {
    let sql = arguments["sql"].as_str().context("missing sql")?;
    let max_rows = arguments["max_rows"]
        .as_u64().unwrap_or(200)
        .clamp(1, 500) as usize;
    let r = store.read_only_query(sql, max_rows)?;
    Ok(json!({
        "columns": r.columns,
        "rows": r.rows,
        "row_count": r.rows.len(),
        "truncated": r.truncated
    }))
}
```

> `store` 与 `arguments` 变量名对齐各分支现场所用(`rg -n '=> \{$' crates/mcp/src/lib.rs` 看 trace 分支怎么取 arguments)。

- [ ] **Step 2B.4: 跑测试确认通过 + 手工验收真库**

```bash
cargo test -p repo-intelligence-mcp query_sql
printf '%s\n%s\n%s\n' \
 '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"probe","version":"0"}}}' \
 '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
 '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"query_sql","arguments":{"sql":"SELECT kind, COUNT(*) n FROM entity GROUP BY kind ORDER BY n DESC LIMIT 3"}}}' \
 | ./target/debug/repo-intelligence --database /tmp/vue-mini/.repo-intelligence/workspace.sqlite mcp 2>/dev/null | tail -1 | python3 -m json.tool | head -30
```
Expected: 返回 table/vue_page/method 等前 3 类计数。

- [ ] **Step 2B.5: Commit**

```bash
git add crates/model/src/lib.rs crates/graph/src/lib.rs crates/mcp/src/lib.rs
git commit -m "feat(mcp): query_sql 只读直通工具——SQL 自由度对标 cb Cypher,行数/单元格双重封顶"
```

### Part C: tools/list 文案瘦身(47936 B → 目标 ≤ 24576 B)

- [ ] **Step 2C.1: 先立测量基线脚本**

创建 `scripts/measure_mcp_tokens.sh`(可执行):

```bash
#!/usr/bin/env bash
# 测量 tools/list 字节数。用途:tools/list 会话级注入,是 RI 对比 cb 的固定 token 税。
# 用法: scripts/measure_mcp_tokens.sh [binary] [database]
BIN="${1:-./target/debug/repo-intelligence}"
DB="${2:-/tmp/vue-mini/.repo-intelligence/workspace.sqlite}"
printf '%s\n%s\n%s\n' \
 '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"probe","version":"0"}}}' \
 '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
 '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
 | "$BIN" --database "$DB" mcp 2>/dev/null | tail -1 | python3 -c "
import json,sys
d=json.load(sys.stdin); tools=d['result']['tools']
print(f'total_bytes={len(json.dumps(d))} tools={len(tools)}')
for t in sorted(tools,key=lambda x:-len(json.dumps(x))):
    print(f\"  {t['name']:22s} {len(json.dumps(t)):>6}\")
"
```

```bash
chmod +x scripts/measure_mcp_tokens.sh && ./scripts/measure_mcp_tokens.sh
```
Expected: `total_bytes≈47936+query_sql增量` 记录为基线。

- [ ] **Step 2C.2: 加字节上限守卫测试(防回涨)**

```rust
#[test]
fn tools_list_stays_under_token_budget() {
    let total: usize = tool_specs()
        .iter()
        .map(|spec| {
            serde_json::to_string(&json!({
                "name": spec.name,
                "description": spec.description,
                "inputSchema": spec.input_schema,
                "outputSchema": spec.output_schema,
            }))
            .unwrap()
            .len()
        })
        .sum();
    assert!(
        total <= 24_576,
        "tools/list payload regressed: {total} bytes (budget 24576)"
    );
}
```

Run: `cargo test -p repo-intelligence-mcp tools_list_stays_under_token_budget`
Expected: FAIL(当前约 48KB)。

- [ ] **Step 2C.3: 替换全部工具 description(定稿文案,逐条粘贴)**

规则:每条 description ≤ 260 字符;inputSchema/outputSchema 里 >100 字符的 property description 一律缩到 ≤60 字符(语义不许丢:"default/enum/含义务"保留,教学段落删除)。以下为新 description 定稿:

| 工具 | 新 description(原样粘贴) |
|---|---|
| `scan_workspace` | `Index a workspace. Returns counts by kind + excluded dirs.` |
| `search_entities` | `Substring search across indexed entities. Semantic/concept matching: use semantic_search instead.` |
| `find_endpoint` | `Find HTTP endpoints/client-calls/api-fields by path or name. Spring MVC mappings recognized.` |
| `semantic_search` | `Meaning-based search (bundled local ONNX embeddings, cosine top-k). Needs embedding enabled + prior scan.` |
| `find_hotspots` | `Rank methods by complexity metrics. Default transitive_loop_depth catches cross-function O(n^2) drivers.` |
| `get_clusters` | `Architecture communities (label propagation over calls/injects/declares/superclass_of/implements). Omit cluster_id to list clusters; else list members.` |
| `analyze_change` | `Structured change impact (field rename/type/nullability...). Paginated findings with bounded traversal.` |
| `analyze_requirement` | `Requirement-keyword to candidate entities (substring). Not semantic search.` |
| `trace_callers` | `Inbound BFS (who reaches X). Default kinds: calls/injects/declares/superclass_of. Cross-file links are inferred; ground with verify_edge.` |
| `trace_callees` | `Outbound BFS (what X reaches). Default kinds: calls/injects/declares/superclass_of. Abstract base auto-drills into subclasses via superclass_of.` |
| `trace_table_access` | `One-shot: readers/writers of a table + upstream chain. BFS along reads/writes_table + calls + injects.` |
| `trace_full_path` | `Generic end-to-end BFS with chosen edge_kinds/direction, optional to_kind filter. Frontend→HTTP→backend→DB reach in one direction per call.` |
| `verify_edge` | `Ground an inferred edge in source: greps the source entity's file for the target name. verified=false means cross-file inference only.` |
| `show_system_view` | `Bounded counts grouped by kind for repositories/api/data views. Never returns full entities.` |
| `get_index_status` | `Local index status: database path + entity/edge counts.` |
| `list_repositories` | `Multi-repo mode: manifest + per-repo counts under --base.` |
| `build_relay_doc` | `Relay-doc skeleton (relay-schema v1) around a qn: inbound/outbound edges with anchors. Structure filled; semantic fields marked custom:needs-review.` |
| `query_sql` | `(Part B 已是新基准,无需再改)` |

property description 重点削剪点(>100 字符的):
- `search_input` 中 `verbose` 的长解释 → `Compact views by default; verbose=true expands metadata+evidence.`
- `min_confidence` 的使用教学句 → `Drop edges below this confidence. Default 0 keeps all.`
- `edge_kinds` 的默认集枚举句 → `Edge kinds to follow (see trace defaults).`
- `semantic_search.limit`、`find_hotspots.*`、`get_clusters.*`、`analyze_change.target_kind/from/to/limit/offset/depth`、`verify_edge.source/target/workspace`、`show_system_view.view`、`build_relay_doc.qn/depth/verbose` —— 同规则处理。
- `scan_workspace` 的 `workspace` 描述已是短句不动。

- [ ] **Step 2C.4: 跑守卫测试 + 探针复测**

```bash
cargo test -p repo-intelligence-mcp tools_list_stays_under_token_budget
./scripts/measure_mcp_tokens.sh
```
Expected: 守卫 PASS;total_bytes ≤ 24576。若仍超预算,优先再砍 outputSchema 的 property description(outputSchema 通常客户端不渲染,是最安全的削减面)。

- [ ] **Step 2C.5: Commit**

```bash
git add crates/mcp/src/lib.rs scripts/measure_mcp_tokens.sh
git commit -m "perf(mcp): tools/list 47.9KB→≤24KB——会话级 token 税减半,加字节预算守卫测试"
```

---

## Task 3: 前端对象参数形式 HTTP 调用识别(P0①)

**Files:**
- Modify: `crates/semantics/src/frontend.rs`(新增正则 + 提取分支)

**背景:** plus-ui / vue-element-admin 系项目的标准形态是封装层对象参数:
`request({ url: '/system/user/list', method: 'get', params })`。现有 HTTP_CALL 正则只认 `verb('/url')` 位置参数形态,vue-mini fixture 已证实漏检。

- [ ] **Step 3.1: 写失败测试**

frontend.rs 的测试模块内(若无则按该 crate 其他 extractor 测试文件的既有风格新建 `#[cfg(test)] mod tests` 于文件尾;参考 `rg -n '#\[cfg\(test\)\]' crates/semantics/src/*.rs` 中 neighbors 的样例,但断言代码如下为准):

```rust
use repo_intelligence_config::SemanticsConfig;

#[test]
fn object_form_http_call_is_extracted() {
    let src = r#"
import request from '@/utils/request'
export function fetchUsers(params) {
  return request({ url: '/api/users/list', method: 'get', params })
}
"#;
    let config = SemanticsConfig::default();
    let mut entities = Vec::new();
    let mut edges = Vec::new();
    let file = SourceFile::vue(src); // 无此构造器就用该测试模块现有的 SourceFile 组装方式
    extract_frontend(&file, "src/api/user.js", &mut entities, &mut edges, &config);
    let calls: Vec<_> = entities.iter().filter(|e| e.kind == EntityKind::HttpClientCall).collect();
    assert_eq!(calls.len(), 1, "{:#?}", calls);
    assert_eq!(calls[0].name, "GET /api/users/list");
}

#[test]
fn object_form_defaults_to_get_when_method_absent() {
    let src = r#"request({ url: "/a/b" });"#;
    // ... 同上组装,断言 name == "GET /a/b"
}
```

> SourceFile 组装以同文件/同 crate 已有测试的做法为准(`rg -n 'SourceFile::' crates/semantics/src --glob '*test*'`);FileKind 选 JavaScript 即可(supports 覆盖 Vue/JS/TS 三类,统一一个入口 `extract_frontend`)。

- [ ] **Step 3.2: 跑测试确认失败**

Run: `cargo test -p repo-intelligence-semantics object_form`
Expected: FAIL —— 0 个 http_client_call。

- [ ] **Step 3.3: 实现对象形态识别**

frontend.rs 常量区(HTTP_CALL_VAR 之后)追加:

```rust
// 对象参数形式:request({ url: '/x', method: 'get' })(plus-ui/vue-element-admin 主流封装)。
// 只负责找 url:,谓词部分(动词缺省 GET)在提取循环里对该调用点的局部窗口二次匹配,
// 避免"method 出现在 url 前"或中间隔字段时一条正则抓不全。
static OBJECT_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\b[\w$][\w$.]*\(\s*\{[^{}]{0,400}?url\s*:\s*["'`]([^"'`]+)["'`]"#).unwrap()
});
static METHOD_IN_WINDOW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\bmethod\s*:\s*["'`](get|post|put|delete|patch)["'`]"#).unwrap()
});
```

`extract_frontend` 的 HTTP_CALL_VAR 循环之后追加(建边逻辑与常量 URL 版一致,Fact 置信 0.9——URL 是源码字面量,但经过封装层不是语法意义上的直接 axios 调用):

```rust
// 对象参数形式(P0①):url 为字面量(Fact 0.9),method 在调用点窗口内找,缺省 GET。
for capture in OBJECT_URL.captures_iter(&file.content) {
    let matched = capture.get(0).unwrap();
    let window_end = (matched.end() + 200).min(file.content.len());
    // 字符边界安全:回退到最近的可切分位置
    while !file.content.is_char_boundary(window_end.min(file.content.len())) {
        unreachable!(); // 见下方实现说明,实际用 floor_char_boundary 逻辑
    }
    let window = &file.content[matched.end()..window_end];
    let verb = METHOD_IN_WINDOW
        .captures(window)
        .map(|m| m[1].to_uppercase())
        .unwrap_or_else(|| "GET".into());
    let url = normalize_path(&capture[1]);
    let name = format!("{verb} {url}");
    let line = line_of(&file.content, matched.start());
    let call = Entity::new(
        EntityId::stable("workspace", path, EntityKind::HttpClientCall, &name, ""),
        EntityKind::HttpClientCall,
        &name,
        format!("{path}#{name}"),
    )
    .with_metadata(json!({"method": verb, "path": url}))
    .with_evidence(
        path, line, line, EvidenceClass::Fact, 0.9,
        "frontend HTTP call (object-literal URL)",
    );
    add_contained(file, path, call, line, entities, edges);
}
```

> **char_boundary 的规范写法**(上面 unreachable! 占位处替换为):Rust stable 没有 `floor_char_boundary`,用循环实现下取整边界,放进 extract_frontend 外的小工具函数,避免重复:
>
> ```rust
> fn floor_boundary(content: &str, mut idx: usize) -> usize {
>     while idx < content.len() && !content.is_char_boundary(idx) {
>         idx -= 1;
>     }
>     idx
> }
> ```
>
> 窗口计算改为:`let end = floor_boundary(&file.content, (matched.end() + 200).min(file.content.len())); let window = &file.content[floor_boundary(&file.content, matched.end())..end];`

- [ ] **Step 3.4: 跑测试确认通过**

Run: `cargo test -p repo-intelligence-semantics`
Expected: 全绿(含新两用例)。

- [ ] **Step 3.5: fixture 端到端验证(前端第一跳打通)**

```bash
rm -rf /tmp/vue-mini/.repo-intelligence && cargo run -q -p repo-intelligence -- scan /tmp/vue-mini --database /tmp/vue-mini/.repo-intelligence/workspace.sqlite >/dev/null 2>&1
DB=/tmp/vue-mini/.repo-intelligence/workspace.sqlite
sqlite3 "$DB" "SELECT name FROM entity WHERE kind='http_client_call';"
sqlite3 "$DB" "SELECT s.kind, s.name, t.kind, t.name FROM edge e JOIN entity s ON s.id=e.source_id JOIN entity t ON t.id=e.target_id WHERE e.kind='matches_endpoint';"
```
Expected: 存在 `GET /api/users/list` 实体 + `http_client_call → http_endpoint` 的 matches_endpoint 边。此时链路 http_client_call→endpoint→list→listUsers→queryUsers→xml_statement→sys_user 单向 trace 完整(残余缺口:VuePage↔api.js 的 import 关联,列为后续 P2,不在本计划)。

- [ ] **Step 3.6: ruoyi 回归(能力不该伤到纯后端库)**

按 CLAUDE.md 流程强制全量重扫(见 Task 6 Step 6.2 统一口令),对比 calls/calls-confidence 分布不变(2973@0.7 + 28@0.5 基线)。

- [ ] **Step 3.7: Commit**

```bash
git add crates/semantics/src/frontend.rs
git commit -m "feat(semantics): 前端对象参数 HTTP 调用识别 request({url,method})——plus-ui 标准形态"
```

---

## Task 4: tests 边修复(命名约定 + import 推断)(P0②)

**Files:**
- Modify: `crates/semantics/src/java.rs`(测试类 metadata.imports)
- Modify: `crates/analysis/src/lib.rs`(~1370 行 Tests 建区块)

**背景:** ruoyi 的 4 个测试类(DemoUnitTest 等)不遵循 "XxxTest→Xxx 被测类同名" 约定,且类间无 depends_on 边可借。新信号:测试类的 import 全限定名与项目唯一类名的末段匹配(A+:多命中拒边记 ambiguity,与全家桶策略一致)。

- [ ] **Step 4.1: 写失败测试(analysis 层)**

`crates/analysis/tests/vertical_slice.rs`(既有 Tests 用例所在的集成测试文件,`rg -n 'EdgeKind::Tests' crates/analysis/tests/vertical_slice.rs` 找到后在其邻近追加):

```rust
#[test]
fn tests_edge_inferred_from_test_class_imports() {
    // 测试类 DemoUnitTest import 项目内的 biz.service.DemoService;无同名约定命中。
    let patch = GraphPatch::add(
        vec![
            class_entity("org/demo/DemoUnitTest.java", "com.demo.DemoUnitTest", r#"{"imports":["com.demo.biz.DemoService","org.junit.jupiter.api.Test"]}"#),
            class_entity("org/demo/biz/service/DemoService.java", "com.demo.biz.DemoService", "{}"),
        ],
        vec![],
    );
    let resolution = resolve_cross_stack(&apply(patch)).unwrap();
    let tests: Vec<_> = resolution.patch.edges.iter().filter(|e| e.kind == EdgeKind::Tests).collect();
    assert_eq!(tests.len(), 1);
    // Inferred 0.6,且方向是 测试类 → 被测类
    let ev = tests[0].evidence.first().unwrap();
    assert_eq!(ev.classification, EvidenceClass::Inferred);
    assert!((ev.confidence - 0.6).abs() < f64::EPSILON);
}

#[test]
fn tests_edge_import_ambiguous_across_packages_is_skipped() {
    // 两个包各有 DemoService → A+ 拒边 + ambiguity note(kind=test_import)
    // 组装两个同名类 + 一个 import 它们的测试类;断言 edges 无 Tests 且 ambiguities 非空
}
```

> helper `class_entity(path,qn,metadata_json)` 的现成形态看该测试文件既有的 entity 构造辅助(没有就写一个小 helper:Entity::new(...kind Class...).with_metadata(serde_json::from_str(metadata_json).unwrap()));第二个测试的实现照第一个套壳即可,完整给出而非"类似"——执行者须把它写成真实代码。

- [ ] **Step 4.2: 跑测试确认失败**

Run: `cargo test -p repo-intelligence-analysis tests_edge_inferred_from_test_class_imports`
Expected: FAIL —— tests 边数为 0。

- [ ] **Step 4.3: java.rs 提取测试类 imports**

java.rs 常量区(AT_TEST 之后):

```rust
static JAVA_IMPORT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^\s*import\s+(?:static\s+)?([\w.]+(?:\.\*)?)\s*;"#).unwrap()
});
```

在 visit/class 装配处(superclass 元数据写入的同一段落,class meta 的 `HashMap<String, Value>` 构建,1037-1052 行模式)给**每个 class** 记录文件级 imports 至 metadata(不做只有测试类才记的特判,让 analysis 层决定怎么用,信号完整性更好):

```rust
let imports: Vec<Value> = JAVA_IMPORT
    .captures_iter(content)
    .filter_map(|c| c.get(1))
    .filter(|m| !m.as_str().ends_with('*'))
    .map(|m| serde_json::Value::String(m.as_str().to_string()))
    .collect();
if !imports.is_empty() {
    meta.insert("imports".into(), serde_json::Value::Array(imports));
}
```

> 变量名 `content`/`meta` 以该函数现场为准;注释掩码沿用文件既有做法(v0.1.32 注释掩码机制的同一管线——import 提取也应跑在掩码后内容上,若 visit_methods 系列已持有 masked 内容就直接用之)。

同文件测试:Java 提取既有测试文件内加一例(class 带 2 个 import → metadata.imports 数组断言),失败→实现→通过。

- [ ] **Step 4.4: analysis 层建边**

`crates/analysis/src/lib.rs` 命名约定 Tests 区块(~1354-1383)之后追加:

```rust
// Tests 边第二推断(import):测试类 metadata.imports 的末段若在项目内唯一命中一个
// class(排除自身),Inferred 0.6 建立 测试类→被测类。多命中 = 跨包同名,A+ 拒边记
// kind=test_import 歧义注记,消费方可结合 import 全限定名自行消歧。
for entity in entities.iter().filter(|e| e.kind == EntityKind::Class) {
    let Some(Value::Array(imports)) = entity.metadata.get("imports") else {
        continue;
    };
    let candidates: Vec<&str> = imports
        .iter()
        .filter_map(Value::as_str)
        .map(|fq| fq.rsplit('.').next().unwrap_or(fq))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    for simple_name in candidates {
        if simple_name == entity.name.strip_suffix("Test").unwrap_or(entity.name) {
            continue; // 命名约定路径已覆盖
        }
        let Some(hits) = classes_by_name_all.get(simple_name) else { continue };
        if hits.len() > 1 {
            ambiguities.push(AmbiguityNote {
                holder: entity.id.clone(),
                kind: "test_import",
                name: simple_name.to_string(),
                candidates: candidate_files(hits),
            });
            continue;
        }
        if hits[0].id == entity.id {
            continue;
        }
        let mut edge = Edge::new(entity.id.clone(), hits[0].id.clone(), EdgeKind::Tests);
        if let Some(ev) = entity.evidence.first() {
            edge = edge.with_evidence(
                &ev.file, ev.start_line, ev.end_line,
                EvidenceClass::Inferred, 0.6,
                "test class imports target",
            );
        }
        edges.push(edge);
    }
}
```

> 类型细节:该函数体内 entities 是 `&[Entity]`(以编译器提示为准);`classes_by_name_all`/`ambiguities`/`candidate_files` 均为本区块上方已存在的绑定/助手,直接复用;`Value` 若未 use 需加 `serde_json::Value`。

- [ ] **Step 4.5: 跑测试确认通过 + 全量回归**

```bash
cargo test -p repo-intelligence-analysis
cargo test --workspace
```
Expected: 全绿。

- [ ] **Step 4.6: ruoyi 真项目验证(CLAUDE.md 强制)**

```bash
cd /Users/cainli/dev/workspace/repo-intelligence/验证项目/ruoyi-vue-plus/.repo-intelligence && cp workspace.sqlite workspace.pre-v0136.sqlite && rm workspace.sqlite* && cd -
cargo run -q -p repo-intelligence -- scan 验证项目/ruoyi-vue-plus --database 验证项目/ruoyi-vue-plus/.repo-intelligence/workspace.sqlite 2>&1 | tail -3
DB=验证项目/ruoyi-vue-plus/.repo-intelligence/workspace.sqlite
sqlite3 "$DB" "SELECT COUNT(*) FROM edge WHERE kind='tests';"
sqlite3 "$DB" "SELECT s.name, t.name FROM edge e JOIN entity s ON s.id=e.source_id JOIN entity t ON t.id=e.target_id WHERE e.kind='tests';"
```
Expected: tests > 0(基线 0);列出 测试类→被测类 成对名单人工抽查合理(DemoUnitTest→其 import 的业务类等)。同时记录 `ambiguous_skipped` 是否新增 test_import 相关条目(scan stderr 里）。**如果依然为 0**,做归因诊断(查 ParamUnitTest.java 的 import 与全库类名末段的交集),把结论写进 CHANGELOG 的 Known-limitations——不接受静默零产出收尾。

- [ ] **Step 4.7: Commit**

```bash
git add crates/semantics/src/java.rs crates/analysis/src/lib.rs crates/analysis/tests/vertical_slice.rs
git commit -m "feat(analysis): tests 边补 import 推断路径(0.6/Inferred + A+ 歧义防护)——修真仓库 0 边"
```

---

## Task 5: 收尾发布(release v0.1.36)

**Files:**
- Modify: `Cargo.toml`(根 workspace.package version)
- Modify: `CHANGELOG.md`
- Modify: `CLAUDE.md`(验证指标段补两条)

- [ ] **Step 5.1: 全量重扫 ruoyi + 指标快照进 CLAUDE.md**

```bash
cd 验证项目/ruoyi-vue-plus/.repo-intelligence && cp workspace.sqlite workspace.v0136-baseline.sqlite && cd ../../..
cargo run -q -p repo-intelligence -- scan 验证项目/ruoyi-vue-plus --database 验证项目/ruoyi-vue-plus/.repo-intelligence/workspace.sqlite 2>&1 | tail -3
```
对照记录(CLAUDE.md「必须对比的指标」逐项跑一遍,数值填进 CHANGELOG):calls 分布、tests 边数、http_client_call/annotation 计数、tools/list 字节、fixture 端到端 matches_endpoint。

CLAUDE.md 指标段追加两小节(插在「异常流边(v0.1.35)」之后,格式照抄现有条目):

```markdown
- **tests 边(v0.1.36)**:`SELECT COUNT(*) FROM edge WHERE kind='tests';` —— 双路径(命名约定 0.7 / import 推断 0.6),import 多命中跳过计入歧义注记 kind=test_import。
- **MCP token 预算**:tools/list 由 `scripts/measure_mcp_tokens.sh` 度量,v0.1.36 起预算 ≤24576 字节(守卫测试 tools_list_stays_under_token_budget 防回涨)。
```

- [ ] **Step 5.2: CHANGELOG + 版本号**

根 `Cargo.toml` `[workspace.package] version = "0.1.35"` → `"0.1.36"`。CHANGELOG.md 顶部(Unreleased 下)新增 `## [0.1.36] - 2026-08-27`,Added/Fixed/Changed/Performance 分类装四件事,含前后数字(tools/list 字节、trace_callees 真库响应字节、ruoyi tests 边 0→N、fixture 链路打通证据)。

- [ ] **Step 5.3: 发布前全套门禁**

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
npm test --prefix packages/npm
```
Expected: 全绿。

- [ ] **Step 5.4: Release commit(仓库惯例)**

```bash
git add -A
git commit -m "release v0.1.36: cb 替代四件套——token 瘦身/query_sql/对象形态 HTTP/tests 边修复"
```

(打 tag 触发 release CI 属发版动作,由用户决定,不在本计划自动执行。)

---

## Self-Review 结论

1. **Spec 覆盖**:用户指定的四件(①对象形态 HTTP ②tests 边 ③query_sql ④token)各有专属 Task(3/4/2/1),外加发布任务收口 —— 无缺口。
2. **Placeholder 扫描**:三处标注了"以现场为准"的适配点(SourceFile 构造器、Evidence 路径、helper 命名),均已附带定位命令(`rg` 锚点)与替代判定标准,不属于 TBD。
3. **类型一致性**:edge_view 新签名 `(edge,&bool)` 在测试/实现/schema 三处一致;QueryResult 字段 columns/rows/truncated 在 graph 实现、mcp 分支、output_schema 三处一致;AmbiguityNote.kind 取值 "test_import" 在 analysis 代码与 CLAUDE.md 指标描述一致;confidence 取值约定 0.7/0.6/0.5 与 v0.1.23 起的既有刻度兼容。
