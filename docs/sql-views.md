# 直连 SQL 指南:v_edge / v_entity 视图

> 面向直接打开 `workspace.sqlite` 写 SQL 的使用方。trace/query 类 API 回包的边/实体
> 字段与底表列名**不同**,按 API 字段查 `edge`/`entity` 表会报 `no such column`——
> 请改查本页的兼容视图(v0.1.47 起,每次开库 `CREATE VIEW IF NOT EXISTS`,旧库开一次
> 即得;`query_sql` 报错时的近邻提示也会指路到这里)。

## 快速验证

```bash
sqlite3 your.db "SELECT name FROM sqlite_master WHERE type='view';"
# 应输出:v_edge  v_entity
```

## v_edge(API 字段词汇表)

```sql
SELECT source, target, kind, confidence FROM v_edge WHERE kind='calls' LIMIT 5;
SELECT source, target FROM v_edge
 WHERE kind='implements_method' AND target LIKE '%IFooService.java#bar%';
```

| v_edge 列 | 来源 | 说明 |
|---|---|---|
| `source` / `target` | JOIN entity 的 qualified_name | **API 同名**;qn 格式 `{相对路径}#{名称}`(v0.1.48 起全 kind 路径化,Windows 分隔符归一 `/`) |
| `kind` | edge.kind | snake_case,与 API `edge_kind` 一致(calls / injects / implements / implements_method / superclass_of / reads_table …) |
| `confidence` | `json.evidence[0].confidence` | 0-1 |
| `tentative` | 由 evidence 分类派生 | Fact 且 <0.8 为 false 等 |
| `source_id` / `target_id` | 底表列 | EntityId(blake3 哈希) |
| `resolved` / `start_line` / `end_line` | 底表列 | resolved=1 表示跨文件消解边 |

## v_entity

```sql
SELECT kind, name, qualified_name FROM v_entity WHERE kind='class' LIMIT 5;
```

含 `id` / `kind` / `name` / `qualified_name` / `anchor`(evidence[0] 的 file:line)及
`metadata` JSON。

## 底表列名对照(为什么直接查 edge 表会报错)

| 你想要的 | API 字段(v_edge) | edge 底表列 |
|---|---|---|
| 起点/终点 | `source` / `target` | `source_id` / `target_id`(是 id 不是 qn) |
| 置信度 | `confidence` | 无顶层列,在 `json` 的 `evidence[0].confidence` |
| 试探性 | `tentative` | 无(派生) |

```sql
-- 不用视图时的等价写法(啰嗦版,供理解;日常请用 v_edge):
SELECT json_extract(json,'$.evidence[0].confidence') FROM edge WHERE kind='calls';
```

## 常见坑

- **http_endpoint 的 name 带 method 前缀**(`GET /system/user/list`):前缀匹配要
  `LIKE '%/system/user%'` 而非 `'/system/user%'`。
- 动态菜单路由(constantRoutes 之外)的项目,静态索引只含静态路由——查「页面属于
  哪个路由」为空是预期,不是缺陷。
- 视图是 `CREATE VIEW IF NOT EXISTS`,RI 工具每次开库都会补建;若你的库从没被
  0.1.47+ 的二进制打开过,先跑一次任意 CLI 命令(如 `repo-intelligence search`)即可。
