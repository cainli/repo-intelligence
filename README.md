# Repo Intelligence

本地运行的跨技术栈代码知识图谱与字段变更影响分析引擎。核心使用 Rust、bundled
SQLite、FTS5 和 Tree-sitter，通过 npm CLI 或原生二进制运行，不依赖图数据库。

当前 `0.1.0` 是首个可运行垂直切片，已经支持：

- 遵循 Git ignore 规则扫描 Java、Vue/TypeScript、MyBatis XML 等文件。
- 使用 Tree-sitter 验证 Java 语法，并提取 Java 字段与 Spring Mapping。
- 提取 Vue 属性引用、Axios/fetch 风格调用、MyBatis Statement、SQL alias 和表引用。
- 将实体、关系、证据和置信度持久化到 SQLite，提供搜索和有限深度遍历。
- 对字段重命名生成基于证据关系的前端、代码和数据候选影响平面。
- 通过 CLI JSON 协议和 stdio MCP 提供查询入口。

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

MCP 配置使用：

```bash
repo-intelligence --database /absolute/path/workspace.sqlite mcp
```

stdout 仅输出 JSON-RPC，诊断信息输出到 stderr。

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
