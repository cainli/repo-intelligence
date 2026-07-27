# Repo Intelligence

本地运行的跨技术栈代码知识图谱与字段变更影响分析引擎。核心使用 Rust、bundled
SQLite、FTS5 和 Tree-sitter，通过 npm CLI 或原生二进制运行，不依赖图数据库。

当前 Rust 引擎 `0.1.4`，已经支持：

- 遵循 Git ignore 规则扫描 Java、Vue/TypeScript、MyBatis XML 等文件。
- 使用 Tree-sitter 验证 Java 语法，并提取 Java 字段与 Spring Mapping。
- 提取 Vue 属性引用、Axios/fetch 风格调用、MyBatis Statement、SQL alias 和表引用。
- 将实体、关系、证据和置信度持久化到 SQLite，提供搜索和有限深度遍历。
- 对字段重命名生成基于证据关系的前端、代码和数据候选影响平面。
- 通过 CLI JSON 协议和 stdio MCP 提供查询入口。

## 扫描范围与自动忽略

扫描器会读取项目的 `.gitignore` 和 `.ignore`，并固定排除
`.git/`、`node_modules/`、`target/`、`build/`、`dist/`、`.gradle`、`.claude/`。
不支持的文件扩展名和超过 2 MiB 的单个文件也会跳过。项目可以在 `.ignore`
中增加生成代码、日志、缓存或其他不需要分析的目录。

扫描日志和进度写入 stderr，JSON 结果仍单独写入 stdout。日志包含文件发现、
解析、跨栈关系解析、SQLite 持久化和完成阶段；解析阶段每 100 个文件报告一次，
并显示当前文件。

目前的跨层字段关系是基于名称和框架约定的 `resolved` 候选关系，不等同于完整 Java
类型检查，调用方需要结合证据确认。扫描采用内存构建、单事务替换快照，以保证失败时保留
旧索引；文件级增量更新尚未实现。MySQL 在线元数据采集、Git/Submodule 快照差分、完整
Gradle 语义和向量召回仍是后续阶段。

## 构建与运行

```bash
cargo build --release

target/release/repo-intelligence init .
target/release/repo-intelligence scan . --format json
target/release/repo-intelligence search customerName --format json
```

影响请求：

```json
{
  "target_kind": "field",
  "operation": "rename",
  "from": "customerName",
  "to": "customerFullName"
}
```

```bash
target/release/repo-intelligence impact \
  --request change.json \
  --format json
```

MCP 配置使用（`--database` 必须指向可写路径，避免使用只读的系统目录）：

```bash
repo-intelligence --database ~/.repo-intelligence/workspace.sqlite mcp
```

stdout 仅输出 JSON-RPC，诊断信息输出到 stderr。`get_index_status` 与
`show_system_view` 返回聚合计数与分布，而非全量实体，因此在大索引下也不会
撑爆协议通道。

## npm 开发入口

```bash
cargo build
node packages/npm/bin/cli.js doctor --format json
```

公共 npm 预览包为 `@cainli/repo-intelligence`，当前首先提供 macOS arm64
原生平台包。规划中的支持矩阵为 macOS arm64/x64、Linux glibc arm64/x64 和
Windows x64；未发布平台包的平台不能运行。

```bash
npm install -g @cainli/repo-intelligence
repo-intelligence doctor --format json
```

## 验证

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
npm test --prefix packages/npm
```

## 变更记录

见 [CHANGELOG.md](CHANGELOG.md)。
