# 变更记录

本项目所有重要变更记录于此文件。

格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，并遵循
[语义化版本](https://semver.org/lang/zh-CN/)。

## [Unreleased]

## [0.1.15] - 2026-07-28

### Fixed

- **scan 遇 minified JS（如 `echarts5.min.js`）卡死**：`.min.js`/`.min.css` 等压缩产物现归
  `Unknown`，discover 跳过（不提取、不读）。压缩代码无语义价值，且巨大单行会让前端提取器
  （frontend.rs）的正则/tree-sitter 灾难性卡死（parsing 阶段停在第 ~13400 个文件）。
  回归测试覆盖 `.min.js`/普通 `.js`（不误伤 `admin.js`）。

## [0.1.14] - 2026-07-28

### Added

- **scan 诊断日志频率可调（`RI_LOG_EVERY`）**：CLI `scan` 的 Parsing 阶段进度日志默认每 100
  个文件打一行；设 `RI_LOG_EVERY=1` 让每个文件都打，scan 卡死时最后一行的 `file=` 即为元凶
  文件。诊断 scan 卡死的标配工具（不改变默认行为）。

## [0.1.13] - 2026-07-28

### Added

- **文件名 glob 排除（`excluded_patterns`）**：`.repo-intelligence.toml` 的 `[discovery]` 段
  新增 `excluded_patterns`（basename glob，匹配即排除文件），与 `excluded_dirs_extra`（目录名）
  分工。治大配置文件（package-lock.json 等）内容驻留导致的 scan 卡死——不必改代码即可排除。
  语法为 glob crate（`*`/`?`），不支持 `!` 取反；要保留某文件就别列它。新增 `glob` workspace 依赖。

## [0.1.12] - 2026-07-28

### Fixed

- **scan 遇 yaml/properties/sql 卡死**：`from_path` 曾把这些扩展名分类为支持的 `FileKind`，
  但 semantics 无对应提取器——内容被 `read_to_string` 全文读进内存并驻留 `Vec<SourceFile>`
  整个 scan，零产出。大项目（几千配置文件）→ OOM/卡死。现归 `Unknown`，discover 跳过（不读内容）。
  删 `FileKind::Yaml/Properties/Sql` dead 变体 + 回归测试。

## [0.1.11] - 2026-07-28

### Added

- **agent 接力结构化产出（relay-schema）**：MCP `build_relay_doc` 端点与 CLI `relay <qn>` 共用
  `build_relay`，按 qn 聚合入/出边，自动填结构层（qn / anchor file:line / edge_kind / edge_type
  机械映射），语义层（bean/interface/business/inject_dead/跨文件调用链）留 `custom:needs-review`
  给 agent。新增 `docs/relay-schema.md`；CLI 加 `--format yaml`（serde_yaml）。
- **增量扫描(P0)**：scan 改为 diff 驱动——对比持久化的 `file_state`(path→hash)快照,
  只对 changed/added 文件重提、对 deleted/changed 文件删旧子树;跨文件推断边
  (`resolved=1`)每次全量重算。改 1 个文件不再"DELETE 整表 + 重插全仓实体 + FTS 全重建",
  agent 写循环可用。
  - graph:edge 表加 `resolved` 列(0=事实提取边 / 1=跨文件推断边,旧库幂等迁移)+ `file_state`
    表;`GraphStore` trait 加 `get/set_file_state`、`replace_resolved_edges`、`delete_file_subtree`
    (按 `Contains` 边反推子实体级联删除)、`extract_edges`。
  - `ScanSummary` 加 `files_added/changed/deleted/unchanged`;CLI `scan` 与 MCP `scan_workspace`
    均输出。
- **关系型输出可信度(P1)**：让 agent 不误信推断边。前提:`evidence`(file:line:confidence)
  本已在 trace/analyze 返回里,P1 补的是"置信度显眼化 + 独立验证"。
  - trace 返回的 edge 加顶层 `confidence` + `tentative`(Fact 不标;Inferred/无证据/Resolved<0.8
    标 tentative),不必翻 `evidence[]` 即可区分可信/存疑。
  - `trace_callers`/`trace_callees` 加 `min_confidence` 过滤(默认 0 全返回——标不滤,不悄悄藏边)。
  - 新工具 `verify_edge(source, target)`：读 source 实体的源文件 grep target,命中=独立佐证,
    未命中=诚实告知"跨文件推断(matches_endpoint/mapped_from 按名解析),非字面引用,视为未验证"。
  - `Evidence` 加 `snippet`(对应源码行,extract 时从文件内容填),agent 免 `Read` 即可初判证据。

### Changed

- 移除进程内 `EXTRACT_CACHE`(0.1.10 的"假增量":只省 parse 且不持久化,CLI 新进程命中率 0),
  由持久化的 `file_state` 快照取代。

### Known Limitations

- 跨文件推断边(`resolve_cross_stack`)每次 scan 仍全量重算,读为 O(实体数)(写已增量);
  `MatchesEndpoint`/`MappedFrom` 边的 `evidence.snippet` 留 None(scan 层产生时无文件内容可读)。

## [0.1.10] - 2026-07-28

### Added

- **search 类工具响应瘦身 + 分页**（`search_entities`/`find_endpoint`/`analyze_requirement`）：默认返回
  compact 视图 `{id, kind, name, qualified_name, evidence_count}`，新增 `verbose`（true 时返回完整
  entity，含 `evidence[].reason` 与 `metadata`）与 `limit`/`offset` 分页 + `has_more`。此前默认
  `limit=100` × 完整 Entity 在宽匹配（如企业编号命中数十实体）下可撑到上万 token；`has_more` 用
  peek `limit+1` 推断，不查 `COUNT(*)`。
- **增量扫描缓存**（analysis）：extract 按 `path → (content_hash, GraphPatch)` 缓存，未变文件复用上次
  patch、跳过 parse/regex；`resolve` 与 `replace_snapshot` 仍全量，跨文件边一致。代价是内存（缓存全部 patch）。
- **跨文件 Mapper→Table 绑定**（analysis）：`resolve_cross_stack` 新增一轮 ——
  `Mapper.metadata.entity_type` 命中同名 `Class` → 其 `DependsOn` 的 `Table`，补足 MyBatis Plus
  实体与 mapper 分文件场景（同文件绑定由 `extract_mybatis_plus` 产）。
- **LambdaQueryWrapper 列提取**（补 0.1.9 限制）：方法引用形式（`.eq(Entity::getName)`）现提取为
  `Column` + `reads_column`，不再仅 `QueryWrapper` 字符串首参。
- **Method/Calls 与 Spring 构造器注入**（semantics）：同文件方法声明与方法间 `calls` 边提取；单构造器
  自动注入、`@Autowired` 构造器 → `DependsOn`，多构造器无 `@Autowired` 不注入。
- **配置层**（config crate）：`EXCLUDED_DIRS`/`FRONTEND_NOISE`/`CUSTOM_ENDPOINT_ANNOTATIONS` 及限制值等
  硬编码常量解耦到 `crates/config`，支持 `.repo-intelligence.toml` 覆盖（标量 + 短列表替换，长列表用
  `_extra` 追加）。CLI `scan` 与 MCP `scan_workspace` 自动 `IndexerConfig::load(workspace)` 发现配置。

### Changed

- **semantics 提取器 trait 化**（内部重构，行为不变）：原 `lib.rs`（1227 行）拆为
  `lib`/`registry`/`java`/`xml`/`frontend`/`build`，引入 `SemanticExtractor` trait（`supports`+`extract`）
  + `Registry::default_java_stack`；新增语言/框架只需 `impl trait + register`，不动核心分发。

### Known Limitations

- `find_endpoint` 带 kind 过滤，而 SQL `LIMIT/OFFSET` 作用在过滤前候选上，其 `has_more` 是"候选窗口
  近似"（可能提前停止翻页，但不丢数据）；`search_entities`/`analyze_requirement`（无过滤）分页精确。
- 增量扫描缓存以内存换 CPU（缓存全部 patch）；超大工作区首次扫描后常驻内存上升。

## [0.1.9] - 2026-07-27

### Added

- **MyBatis Plus 持久层提取**：补齐主力 ORM（MyBatis Plus 3.5.7）的字段级贯通。
  此前持久层贯通只解析传统 MyBatis XML mapper，而 MP 主力是注解实体 + BaseMapper +
  QueryWrapper 链式 API，XML 无语句，导致字段级变更影响分析在主力持久层断裂。
  `extract_java` 末尾新增 `extract_mybatis_plus`（regex 优先，不引入新 parser）：
  - `@TableName("t")` → `Table` 实体 + `Class --depends_on→ Table`。
  - `@TableField("c")`/`@TableId("c")` 注解字段 → `Column` 实体 + 显式
    `Field --mapped_from→ Column`（Fact 1.0）；`@TableName` 类内无注解字段按驼峰转下划线
    推断列名（Inferred 0.7）；`@TableField(exist = false)` 跳过。
  - `interface XxxMapper extends BaseMapper<T>` → `Mapper` 实体；同文件内 T 是
    `@TableName` 类 → `Mapper --depends_on→ Table`。
  - `QueryWrapper` 链式列字面量（`.eq("col", …)` 等） → `Column` 实体 +
    `File --reads_column→ Column`（Inferred 0.6，召回优先）。
  - 同 (文件,列名) 的实体列与 wrapper 列引用合并为单一 Column 节点（EntityId 确定性）。
- **模块依赖图**：`FileKind` 新增 `Toml`/`Json`（此前 `.toml`/`.json` 落 Unknown 被丢弃）；
  `extract_package_json`/`extract_gradle`/`extract_version_catalog` 解析
  package.json / build.gradle(.kts) / libs.versions.toml，产出模块 `Package` +
  `DependsOn` 边，支撑「改 SDK X 影响 mes 哪些模块」的模块级依赖影响分析。
- **前端字段降噪**：`VUE_BINDING` 新增 `is_likely_field` 守卫，过滤 JS/TS 内建方法
  （length/map/forEach…）与全大写常量误报，降低 `FrontendField` 同名匹配噪声。

### Changed

- `resolve_cross_stack` 字段贯通桶纳入 `EntityKind::Column`，`semantic_rank(Column)=4`
  （物理列居最深数据层，SqlField=3 之后）；跨文件/跨技术同名 Column 自动互链。
- 字段重命名影响分析现可经显式 `Field→Column` mapped_from 边覆盖到 `Column` 实体，
  主力持久层断链修复；`ImpactAnalyzer`/`plane_for`/`plane_rank` 无需改动
  （Column 已在 data 平面 rank=2，分析器 kind-agnostic）。
- clippy 漂移修复：Spring AST 区块（v0.1.8）的嵌套 `if let` 按新版 clippy 的
  `collapsible_if` 合并为 let-chains（edition 2024）。
- 新增测试：`mybatis_plus_links_field_to_column_for_rename_impact`、
  `dependency_graph_links_module_to_declared_dependencies`（analysis 端到端）；
  `mybatis_plus_explicit_is_fact_inferred_is_low_confidence`、
  `camel_to_snake_via_inferred_column_names`、
  `query_wrapper_column_reference_produces_inferred_reads_column`、
  `frontend_noise_properties_are_not_extracted_as_fields`（semantics crate 首批集成测试）。

### Known Limitations

- `LambdaQueryWrapper` 的方法引用形式（`.eq(Entity::getName)`）不提取（需方法→字段映射）；
  仅覆盖 `QueryWrapper` 字符串首参形式。
- Mapper→Table 仅同文件解析；跨文件（实体与 mapper 分文件）留待后续在
  `resolve_cross_stack` 加一轮泛型→实体类名→Table 同名匹配。
- 驼峰转下划线对连续大写（`URLPath` → `u_r_l_path`）与 MP 默认略异；这类字段实践中
  多有显式 `@TableField`，影响有限。
- 模块依赖图实体是 path-scoped，同名依赖跨文件不自动合并；Gradle 版本目录别名的
  连字符转点规则（`my-lib` → `libs.my.lib`）未实现，catalog 别名与 build 脚本引用暂不连通。

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
