# 变更记录

本项目所有重要变更记录于此文件。

格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，并遵循
[语义化版本](https://semver.org/lang/zh-CN/)。

## [Unreleased]

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
