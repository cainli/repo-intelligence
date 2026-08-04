# repo-intelligence 开发约定

## 验证基准项目:ruoyi-vue-plus(强制)

**所有能力改动(提取器 / 分析 / 图存储)的测试与实验,必须在真实项目 `验证项目/ruoyi-vue-plus` 上验证完成。** 单元测试(`cargo test`)只覆盖内联小 fixture,不足以证明真实 Java 仓库上的效果——不能用"单元测试全绿"替代真实项目验证。

- **位置**:`验证项目/ruoyi-vue-plus`(gitee 镜像 `https://gitee.com/dromara/RuoYi-Vue-Plus`,已 gitignore,不进 RI 仓库)
- **技术栈**:Spring Boot 3 + MyBatis Plus + Sa-Token + Redis + Lombok(国内企业级 Java 全栈代表)

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

## 构建 / 测试

- `cargo test` —— 全 workspace 单元/集成测试(**不替代**上面的真实项目验证)。
- 版本号:根 `Cargo.toml` `[workspace.package] version`(当前 0.1.32),所有 crate `version.workspace = true`。
- 提交风格:`release vX.Y.Z: ...`(见 git log)。

## 项目结构

- `crates/` — config / model / source / parsing / semantics / graph / analysis / protocol / mcp / cli
- 提取器扩展点:`crates/semantics/src/lib.rs` 的 `SemanticExtractor` trait + `Registry::default_java_stack()`
- 跨文件边在 `crates/analysis/src/lib.rs::resolve_cross_stack` 建(extract 层只建同文件边,因 EntityId path-scoped)
- 行号冗余列在 `entity`/`edge` 表顶层(裸 SQL 友好);工具层走 trait API 读 json,不依赖该列
- 历史优化记录:`~/.claude/projects/.../memory/`(各版本 memory,如 `repo-intelligence-v023-calls-annotations`)
