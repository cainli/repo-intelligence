# 变更记录

本项目所有重要变更记录于此文件。

格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，并遵循
[语义化版本](https://semver.org/lang/zh-CN/)。

## [Unreleased]

## [0.1.8] - 2026-07-27

### Added

- **调用链追踪工具 `trace_callers` / `trace_callees`**：补齐"实体搜索≠调用链
  追踪"的缺口（真实项目对比中，调用链还原曾靠 grep 兜底）。按精确实体名解析
  起点，沿 `calls` 边（默认）BFS 上游/下游到 `depth`（默认 2），返回
  `{items, edges, count, start_count}`——`S27501→S27204` 这类链路一步可得。
  `edge_kinds` 可覆盖为 `depends_on`/`reads_table`/`writes_table` 等做其他依赖
  追踪（DB↔SQL 等），不再需要 grep 猜。
- `TraverseQuery` 新增 `edge_kinds` 过滤字段与 `with_kinds()` builder，BFS 可
  限定边类型；空值保持 `analyze_change` 原"走所有边"的行为（向后兼容）。
- **matches_endpoint 分级匹配**：前端调用与后端端点的连接从精确全等放宽为
  分级——精确 `Resolved`(0.95)、路径后缀段对齐(吸收 baseURL/版本前缀)
  `Inferred`(0.6)；同步放宽 `@RequestMapping`(`value=`/多路径)与前端 HTTP 调用
  (封装 client)正则。解决"826 调用 × 537 端点仅匹配 2 条"的跨栈断链。
- **Spring Bean 依赖注入**：`ParseOutput` 返回 tree-sitter AST(原本 parse 后
  丢弃)；`@Autowired`/`@Resource` 字段→`SpringBean`+`DependsOn`，`@Bean` 方法
  →`Exposes`；`@Scheduled`/`@Transactional` 作为 class metadata 标记。
- **ImpactFinding.confidence**：finding 携带可达性置信度(路径边证据的最小值)，
  供客户端区分精确命中与推断命中。
- **MCP 空库可观测性**：`get_index_status` 显示规范化绝对路径；空库时
  `analyze_change`/`search` 加 open_question，避免"零影响"被误读为"安全"。
- **前端字段影响 file-桥接**：`ImpactAnalyzer` 对前端字段 finding 额外从所在 file
  outbound 到同 file 的 `http_client_call`→`MatchesEndpoint`→后端 endpoint，让"改前端
  字段"的 blast radius 触及后端端点（此前字段是叶节点，单向 traverse 到 file 即停）。
  桥接走的 Inferred 边会拉低 finding.confidence，与分级匹配呼应。

### Changed

- trace 起点按精确名匹配（非子串），与 `analyze_change` 对齐，避免
  `trace_callers("S27204")` 把 `S27204Req`/`S27204Resp` 的调用方混入。
- trace 结果按 `qualified_name` / `(source, target, kind)` 排序，保证输出与
  测试确定性（BFS 内部用 HashMap/HashSet，顺序本不定）。
- `trace_callers`/`trace_callees` 起点缺失时返回 `hint`，区别于"工具坏了"。
- 新增回归测试：`traverse_filters_by_edge_kind`、
  `trace_callers_walks_the_inbound_call_chain`、
  `trace_callees_walks_the_outbound_call_chain`、
  `trace_follows_a_non_default_edge_kind`、`trace_respects_depth`、
  `trace_unknown_name_attaches_a_hint`、`trace_tools_declare_typed_schemas`。

## [0.1.7] - 2026-07-27

### Fixed

- **`analyze_change` 结果体积失控**：删除或修改一个被广泛引用的字段曾产出数百万
  字符的影响报告（双向深度 8 遍历 + 500 条硬上限），客户端无法直接消费。现在：
  - 新增 `limit`（默认 100）/`offset` 分页参数，结果按影响平面（前端/API/数据
    优先）排序后截断，并返回 `total`/`limit`/`offset`/`has_more`。
  - `operation` 真正驱动遍历深度：`remove`/`change_*` 默认只取直接依赖方
    （depth 1），`add`/`rename` 默认 depth 2，均可被 `depth` 参数覆盖。此前
    `operation` 被反序列化后即丢弃，所有操作走同一条爆炸路径。
- **`find_endpoint` 名不副实**：此前与 `search_entities` 共用同一段代码，返回任意
  匹配实体（包括 DTO 类），而非端点。现在只返回端点类型（`http_endpoint`/
  `http_client_call`/`api_field`）。
- **非 Spring MVC 项目 API 视图与端点查找失效**：端点识别此前只认
  `@RequestMapping`/`@GetMapping` 等 Spring MVC 注解，对 RMB `@RmbMap`、Dubbo
  `@DubboService` 等自研 RPC 框架项目，`http_endpoint` 实体数为 0，API 视图与
  `find_endpoint` 全空。现在内置可扩展的自定义端点注解识别（默认含 `@RmbMap`/
  `@DubboService`/`@RpcMapping`，在 `CUSTOM_ENDPOINT_ANNOTATIONS` 增项即可接入）。
- **扫描混入自身索引目录**：`.repo-intelligence/` 未排除，可能被重复扫入。
  现已加入固定排除集。

### Added

- **`scan_workspace` 健康报告**：扫描结果新增 `entities_by_kind`（按实体类型分布）
  与 `excluded_dirs`（被排除目录清单），可一眼发现索引污染（如 worktree 副本使
  每个类型计数翻倍）。
- **空结果提示**：`find_endpoint`/`analyze_requirement` 在零命中时返回 `hint`，
  说明匹配范围（实体名子串，非全文/语义检索）或端点识别规则，避免误判"工具坏了"。
  `show_system_view` 的 `api`/`data` 视图为空时同样给出提示。

### Changed

- `analyze_change` 的 `inputSchema` 补全 `limit`/`offset`/`depth`，`outputSchema`
  补全 `total`/`limit`/`offset`/`has_more`。
- `show_system_view` 的 `view` 改为 enum（`repositories`/`api`/`data`）。
- `search_entities`/`find_endpoint`/`analyze_requirement` 的 `outputSchema` 新增可选
  `hint` 字段；描述明确"按实体名/限定名子串匹配，非注解、全文或语义检索"。
- CLI `overview` 命令从返回全量实体改为返回按类型分布的概览（避免大索引撑爆
  stdout），与 MCP `show_system_view` 对齐。
- 固定排除目录列表抽为 `repo_intelligence_source::EXCLUDED_DIRS` 公开常量。
- 新增回归测试：`find_endpoint_returns_only_endpoint_kinds`、
  `analyze_change_paginates_findings_and_reports_total`、
  `scan_workspace_reports_kind_distribution_and_excluded_dirs`、
  `empty_search_attaches_a_hint_instead_of_silent_zero`、
  `recognizes_custom_rpc_endpoint_annotations`、
  `analyze_depth_defaults_shallow_for_destructive_operations`，并扩展 discovery
  测试覆盖 `.repo-intelligence/` 排除。

## [0.1.6] - 2026-07-27

### Added

- **空索引自诊断**：`get_index_status` 新增 `indexed`（布尔）字段，并在索引为空
  （`entity_count == 0`）时附带 `hint`，提示先运行 `scan_workspace`。此前一个从未
  扫描的空库会返回与"健康服务"无异的零计数结果，让人误以为工具可用，直到其它工具
  静默返回空结果。

### Fixed

- **单个工具 panic 不再连累同批调用**：`tools/call` 现在用 `catch_unwind` 隔离每次
  调用，工具内部的 panic 会降级为一条 `isError` 响应（panic 信息同时打到 stderr 便于
  诊断），而不会 unwind 出 `serve`、杀掉整个会话——后者正是"多个并行调用里已应答的
  正常、排在后面的全部报 internal error"这一类故障的结构性成因。

## [0.1.5] - 2026-07-27

### Fixed

- **MCP 工具未声明参数 schema 导致参数黑洞**：`tools/list` 此前给每个工具同一个
  空 `inputSchema`（`{type:object, additionalProperties:true}`，零属性），客户端
  因此不透传参数 → `analyze_change` 报 `missing field target_kind`、`show_system_view`
  的 `view` 失效。现在为每个工具声明真实的 `inputSchema`（含 `query`/`target_kind`/
  `operation`/`view`/`workspace` 等）与对象型 `outputSchema`。
- **MCP 搜索类工具返回裸数组被协议拒绝**：`search_entities`/`find_endpoint`/
  `analyze_requirement` 原本把 `Vec<Entity>` 直接作为 `structuredContent`，而 MCP 要求
  其为 JSON 对象，触发 `expected record, received array`。现在包成 `{items, count}`。

### Changed

- `show_system_view` 的 `view` 参数真正生效：`api`/`data` 视图只返回对应平面的实体
  分布；`repositories` 及未知视图仍返回全量概览（向后兼容）。
- 搜索类工具新增可选 `limit` 参数（默认 100，与原硬编码一致）。
- `analyze_change` 的 `from` 在 `inputSchema` 中标记为必填（运行时本就必填）。
- 新增回归测试：`search_returns_structured_content_as_an_object`、
  `tools_list_declares_typed_input_and_output_schemas`、
  `system_view_filters_to_the_requested_plane`。

## [0.1.4] - 2026-07-27

### Fixed

- **MCP stdout 溢出导致连接断开**：`get_index_status` 与 `show_system_view` 此前通过
  `all_entities()` 把整张图序列化写入 stdout 协议通道，大索引（>16MB）触发客户端
  JSON-RPC 内存保护并强制断连（`Connection closed`）。
  - `get_index_status` 改为返回 `{database, entity_count, edge_count}`。
  - `show_system_view` 改为返回按 `EntityKind` 分组的分布概览。
- **扫描混入 worktree 副本**：文件发现阶段未排除 `.claude/`，导致
  `.claude/worktrees/` 下的副本被索引，产生同类重复命中。现在固定排除 `.claude/`。

### Changed

- 新增 `SqliteGraphStore::counts_by_kind()`，用 SQL `GROUP BY` 在数据库端聚合，
  避免把实体加载进内存。
- CLI `status` 改用 `counts()` 替代 `all_entities()`（此前仅为取数量却全量加载）。
- 统一版本号到 `0.1.4`：平台二进制此前停在 `0.1.1`，而主包已发到 `0.1.3`
  （主包 0.1.2/0.1.3 发布时原生二进制未变）。本次 Rust 改动使二进制真正变更，
  平台包随之升级。

## [0.1.0–0.1.3] - 2026-07

早期 npm 预览版本，主要包含：

- 跨技术栈代码知识图谱垂直切片（Java / Vue / TypeScript / MyBatis XML）。
- 基于 Tree-sitter 的 Java 语法验证与字段、Spring Mapping 提取。
- 实体、关系、证据与置信度持久化到 bundled SQLite + FTS5。
- 字段重命名的影响平面分析（前端、代码、数据）。
- 扫描进度上报与单事务原子替换快照。
- 可执行 npm CLI 与 macOS arm64 原生平台包发布。
