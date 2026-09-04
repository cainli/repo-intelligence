# repo-intelligence 开发约定

## 验证基准项目:ruoyi-vue-plus(强制)

**所有能力改动(提取器 / 分析 / 图存储)的测试与实验,必须在真实项目 `验证项目/ruoyi-vue-plus` 上验证完成。** 单元测试(`cargo test`)只覆盖内联小 fixture,不足以证明真实 Java 仓库上的效果——不能用"单元测试全绿"替代真实项目验证。

- **位置**:`验证项目/ruoyi-vue-plus`(gitee 镜像 `https://gitee.com/dromara/RuoYi-Vue-Plus`,已 gitignore,不进 RI 仓库)
- **技术栈**:Spring Boot 3 + MyBatis Plus + Sa-Token + Redis + Lombok(国内企业级 Java 全栈代表)
- **配套前端样本**:`验证项目/plus-ui`(gitee `https://gitee.com/JavaLionLi/plus-ui`,Vue 3.5 + TS + Element Plus,102 个 .vue / 235 文件)——vue/ts 能力改动在此验证;与 ruoyi 配对可测前后端 http 链路。注意 ruoyi 6.X 主仓库**不含前端目录**。查询坑:http_endpoint 的 name 带 method 前缀(`GET /system/user/list`),前缀匹配要 `LIKE '%/system/user%'` 不是 `'/system/user%'`
- **对比评测报告**:`docs/eval/ri-vs-cb-20260828.md`(Java 深栈)、`docs/eval/ri-vs-cb-20260902.md`(vue/ts 四栈替代可行性;其 §4 P0 TS 符号层已于 v0.1.38 实施)、`docs/eval/ri-vs-cb-20260903.md`(v0.1.38 工作区实测确认四栈替代闭环 + §5 当日 gap 全修)、`docs/eval/ri-vs-cb-20260904.md`(v0.1.39 端到端链路实测闭环 + §7 两项 RI P2 当日修复:裸 `@GetMapping` 类级前缀拼接——端点 243→284、http-join 95→134;impact CLI 紧凑档——27.8KB→571B。复杂度维度 RI 反超 cb:cx>1 方法 916 vs 12,cb full 模式重索引后仍 12)
- **vue/ts 前端链路指标(v0.1.39 gap 全修)**:`route` 12 实体(constantRoutes 静态部分;.vue 内 `router.push({path})` 是导航跳转,守卫排除)、`renders` 11(静态路由全连通)、`component_ref` 45、`.vue` 双份提取(function+frontend_field 同名)= 0、interface/enum 带 `members` + `body_end_line`(UserQuery 16-24 与 cb 对齐)、`http-join` CLI 193 调用→95 精确匹配。验收 SQL:
  ```bash
  DB=验证项目/plus-ui/.repo-intelligence/workspace.sqlite
  sqlite3 "$DB" "SELECT COUNT(*) FROM edge WHERE kind IN ('renders','component_ref');"  # 11 / 45
  sqlite3 "$DB" "SELECT COUNT(*) FROM entity f1 WHERE f1.kind='function' AND EXISTS (SELECT 1 FROM entity f2 WHERE f2.kind='frontend_field' AND f2.name=f1.name AND f2.qualified_name=f1.qualified_name);"  # 0
  sqlite3 "$DB" "SELECT json_extract(json,'$.metadata.members') FROM entity WHERE kind='interface' AND name='UserQuery';"
  ./target/release/repo-intelligence http-join 验证项目/plus-ui/.repo-intelligence/workspace.sqlite \
    验证项目/ruoyi-vue-plus/.repo-intelligence/workspace.sqlite --format json
  ```
  注意:**/system/user 等业务路由是后端菜单动态下发**(`dynamicRoutes=[]`),静态索引只含 constantRoutes——查"页面属于哪个路由"对动态菜单项目天然为空,不是缺陷。
- **vue/ts 基线指标(v0.1.38 TS 符号层落地)**:function 1202 / interface 143 / type_alias 43 / enum 5,跨文件 vue_page→function calls 490 + 文件内 calls 518,ambiguous_skipped 314(前端惯用名跨页面同名,A+ 拒边 + `metadata.ambiguous_resolution` 候选清单,67 页面命中)。验收 SQL:
  ```bash
  DB=验证项目/plus-ui/.repo-intelligence/workspace.sqlite
  sqlite3 "$DB" "SELECT COUNT(*) FROM edge e JOIN entity s ON s.id=e.source_id JOIN entity t ON t.id=e.target_id WHERE e.kind='calls' AND s.kind='vue_page' AND t.kind='function';"
  sqlite3 "$DB" "SELECT s.qualified_name FROM edge e JOIN entity s ON s.id=e.source_id JOIN entity t ON t.id=e.target_id WHERE e.kind='calls' AND t.name='listUser';"  # UserSelect + user/index 两个 SFC
  ```
  **提取器正则变更后 plus-ui 也要 rm 库全量重提**(与 HTTP_CALL 同坑)。ruoyi 回归:790 文件全 unchanged、实体/边零变化即通过。

### 验证流程(每次能力增强后必须执行)

```bash
# 1. 重新索引(库单独放验证项目下,不污染 RI 自身的 .repo-intelligence/)
mkdir -p 验证项目/ruoyi-vue-plus/.repo-intelligence
cargo run -p repo-intelligence -- scan 验证项目/ruoyi-vue-plus \
  --database 验证项目/ruoyi-vue-plus/.repo-intelligence/workspace.sqlite

# 2. 直连库查关键指标,对比改动前后
DB=验证项目/ruoyi-vue-plus/.repo-intelligence/workspace.sqlite
sqlite3 "$DB" "SELECT json_extract(json,'\$.evidence[0].confidence'), COUNT(*) FROM edge WHERE kind='calls' GROUP BY 1;"
```

### 必须对比的指标

- **calls 置信分布**:0.7 = 静态/字段精确解析(v0.1.23 Step A/B 新增),0.5 = 裸名注入匹配。改动前只有裸名匹配。**注意 confidence 嵌在 `edge.json.evidence[].confidence`**(不是 `json.confidence`):
  ```bash
  sqlite3 "$DB" "SELECT json_extract(json,'\$.evidence[0].confidence'), COUNT(*) FROM edge WHERE kind='calls' GROUP BY 1;"
  ```
  v0.1.32+ 起 A+ 歧义防护会让跨包同名类的 calls 解析被跳过并计入 `ambiguous_skipped`(scan 摘要)与实体 `metadata.ambiguous_resolution`;calls 边数可能略降但更纯。
- **annotation 覆盖**:`SELECT name, COUNT(*) FROM entity WHERE kind='annotation' GROUP BY name ORDER BY 2 DESC` —— 验证 @Transactional/@Cacheable/@Slf4j(项目自定义)等结构化。
- **跨文件链路**:从 Controller outbound traverse Calls,看是否连通到 Service→Mapper(Step B 字段消歧解了同名方法歧义)。
- **从类 trace 到表**:`WITH RECURSIVE reach(...) ... edge_kinds 含 'declares' ... WHERE kind='table'` 从 ServiceImpl 类出发应命中表(默认 trace edge_kinds 已含 declares,类→method→Mapper→表);不含 declares 时为 0(回归对照)。
- **从基类/abstract 类 trace 到表(v0.1.25)**:`SELECT COUNT(*) FROM edge WHERE kind='superclass_of'` 应 > 0;`WITH RECURSIVE ... edge_kinds 含 'superclass_of'` 从基类(如 BaseController)出发应经子类到达 table(此前 0,基类自身不调 Mapper)。abstract 类有 metadata.abstract=true。
- **原生 MyBatis 追表(v0.1.24)**:`SELECT COUNT(*) FROM edge WHERE kind='binds_to_statement'` 应 > 0(原生 MyBatis Dao 接口 + Mapper.xml 的项目);`WITH RECURSIVE reach ... edge_kinds 含 'binds_to_statement','reads_table'` 从 Dao 接口方法出发应抵达 table。mes/mos 等纯原生 MyBatis 项目,这是"trace 能否追到表"的分水岭指标;MyBatis Plus(@TableName)项目走另一条 reads_table 边,本指标可低。
- **body_end_line**:`SELECT json_extract(json,'\$.metadata.body_end_line') FROM entity WHERE kind='method' AND qualified_name LIKE '%#%' LIMIT 5`。
- **节点复杂度属性(v0.1.34)**:`SELECT name, json_extract(json,'\$.metadata.complexity') cx, json_extract(json,'\$.metadata.loop_depth') ld, json_extract(json,'\$.metadata.transitive_loop_depth') tld FROM entity WHERE kind='method' ORDER BY json_extract(json,'\$.metadata.complexity') DESC LIMIT 10;` —— complexity(cyclomatic)/loop_count/loop_depth(单函数)+ transitive_loop_depth(沿 CALLS 固定点传播,跨函数 O(n²) 探测器)。对标 codebase-memory Q4 热点;tld > own loop_depth 的方法是"局部无害但调用链深"的 cb 杀手锏信号。
- **架构聚类(v0.1.34)**:`SELECT json_extract(json,'\$.metadata.cluster_id') cid, COUNT(*) n FROM entity WHERE json_extract(json,'\$.metadata.cluster_id') IS NOT NULL GROUP BY cid ORDER BY n DESC LIMIT 10;` —— label propagation 在 calls/injects/declares/superclass_of/implements 图上跑(对标 cb Leiden),识别跨文件夹的"事实模块"。大 cluster 应对应 module(ruoyi 验证:cluster 2=ruoyi-demo、cluster 4=ruoyi-workflow 全内聚)。
- **异常流边(v0.1.35 → v0.1.37 重写)**:`SELECT kind, COUNT(*) FROM edge WHERE kind IN ('throws','handles') GROUP BY kind;` —— method throws/catch 解析到 class/interface 实体;`SELECT COUNT(*) FROM entity WHERE json_extract(json,'\$.metadata.exception_flow') IS NOT NULL;` 看提取覆盖。v0.1.37 起解析 `throw new X(...)` 语句(kind=raise,Fact 0.9 "throw statement")并修复签名 throws 分支的节点 kind bug(tree-sitter-java 里是 `throws` 而非 `throws_clause`,此前**从未生效**);ruoyi 实测 throws 112 条(对照 cb 96)。**注意**:catch JDK 异常的项目 handles 边仍为 0(异常类型非项目 class),需项目自定义异常链才验证 handles 连通。
- **tests 边(v0.1.36 双路径)**:`SELECT COUNT(*) FROM edge WHERE kind='tests';` + 成对名单(见下方模板)。两条推断路径:命名约定 0.7 / import 推断 0.6(Inferred)。import 推断**仅对"文件内存在 @Test TestCase 实体"的类**做,多命中跨包同名记歧义注记 kind=test_import 并拒边(A+ 同策略)。朴素全类 import 推断会产生 ~1296 条误报,判定必须保留。
  ```bash
  sqlite3 "$DB" "SELECT s.name, t.name FROM edge e JOIN entity s ON s.id=e.source_id JOIN entity t ON t.id=e.target_id WHERE e.kind='tests';"
  ```
- **MCP token 预算(v0.1.36)**:`./scripts/measure_mcp_tokens.sh` 测量 tools/list 字节,**预算 ≤24576**(守卫测试 `tools_list_stays_under_token_budget` 防回涨;v0.1.36 实测 22837/18 工具)。工具描述是会话级注入的固定 token 税——加新工具时文案守基准(单工具 description 短句,inputSchema/outputSchema property 说明 ≤60 字符),发现超预算先砍 outputSchema 文案。
- **trace/query 默认紧凑视图(v0.1.36)**:trace_* 与 query 边/实体默认紧凑档——边 `{confidence,tentative,evidence_count,evidence_first}`,实体 `{id,kind,name,qualified_name,anchor}` + evidence_count,**完整 metadata/evidence[] 仅 `verbose=true` 展开**。旧口径的大响应字节数与新口径不可直接对比;对比性能指标时必须同版本对测。ruoyi 实测 trace_callees(SysUserServiceImpl depth=2):168269 → 80331 B。
- **对象形态 HTTP(v0.1.36)**:`request({url:'/x',method:'get'})`(plus-ui 标准封装)语义层已识别(Fact 0.9);fixture 口令提示:HTTP_CALL 正则变更后增量扫描不会重提旧文件,需换库强制全量重提才反映:
  ```bash
  rm -rf /tmp/vue-mini/.repo-intelligence && cargo run -q -p repo-intelligence -- scan /tmp/vue-mini --database /tmp/vue-mini/.repo-intelligence/workspace.sqlite
  # 验收:http_client_call 存在 GET /api/users/list + matches_endpoint 边到 http_endpoint
  ```
- **聚类自动标注(v0.1.37)**:`SELECT cluster_id, label, round(cohesion,2), size, top_nodes FROM cluster_info ORDER BY size DESC LIMIT 5;` —— scan 时随聚类一并落库(graph 新表),`get_clusters` 直接读表输出,消费端不再手写 SQL 做"簇→模块"归属。label = 成员路径前缀众数,cohesion = 簇内边/(簇内+跨界边),top_nodes = calls/injects 度数 top-3(Declares 结构边不计度)。ruoyi 实测:cluster 42 自动得 `ruoyi-common-mybatis`(0.78)、cluster 46 得 `ruoyi-common-redis`。
- **多语 embedding(v0.1.37)**:模型换 `paraphrase-multilingual-MiniLM-L12-v2` 量化版(384 维不变,binary ~165MB,+120MB 属预期);中文 query 从完全失败(0.23 误命中)变为可用("用户登录认证" → user_name/password/login 0.59-0.62)。**text_hash 带 MODEL_ID 前缀**——换模型后旧向量自动失效重算,勿去掉前缀。embedding 耗时上收 scan 摘要(`embedded_count`/`embedding_ms`),ruoyi 全量 10.9s→20.2s(多语 12 层推理翻倍,接受;急用可 `[index] embedding=false`)。
- **impact 快捷入口(v0.1.37)**:`impact --entity <NAME>`(默认 change_semantics)免手写 ChangeRequest JSON;`--request` 传非 JSON 时给用法提示。CLI 与 MCP 的 `ImpactAnalyzer` 均已 `with_config` 注入——`.repo-intelligence.toml` 的 `[analysis] default_impact_limit/max_impact_limit` 现在真实生效(此前是死配置)。

## 构建 / 测试

- `cargo test` —— 全 workspace 单元/集成测试(**不替代**上面的真实项目验证)。
- 版本号:根 `Cargo.toml` `[workspace.package] version`(当前 0.1.38),所有 crate `version.workspace = true`。
- 提交风格:`release vX.Y.Z: ...`(见 git log)。

## 项目结构

- `crates/` — config / model / source / parsing / semantics / graph / analysis / protocol / mcp / cli
- 提取器扩展点:`crates/semantics/src/lib.rs` 的 `SemanticExtractor` trait + `Registry::default_java_stack()`
- 跨文件边在 `crates/analysis/src/lib.rs::resolve_cross_stack` 建(extract 层只建同文件边,因 EntityId path-scoped)
- 行号冗余列在 `entity`/`edge` 表顶层(裸 SQL 友好);工具层走 trait API 读 json,不依赖该列
- 历史优化记录:`~/.claude/projects/.../memory/`(各版本 memory,如 `repo-intelligence-v023-calls-annotations`)
