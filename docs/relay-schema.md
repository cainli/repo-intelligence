# Agent 接力 Schema

定义"分析单个目标后，交给下一个 agent 接力"的结构化产出格式。设计目标：每条边自带锚点、语义可判、字段可直接回喂工具，零视觉噪音。

`schema_version: 1`

## 为什么需要这个格式

ASCII 框线图、对齐空格、emoji 对 agent 是纯噪音；同一节点的锚点散落在表格/行号/finding 三处会拼不回完整记录；缺边语义则无法判断"这是入口边还是 DB 读"。本格式用四条规则根治：

1. **锚点是对象，不是字符串** —— `file`+`line`+可选 `kind`，永不省略文件名。
2. **每条边自带 `edge_type`+`layer`** —— 机制与目标层正交，可分别过滤。
3. **单一事实源** —— 出站边只维护一个列表，不维护派生分组视图。
4. **finding 结构化** —— `action`+`links` 让下一个 agent 可机械执行，不必回翻叙述。

## 顶层结构

```yaml
meta:
  schema_version: "1"
  target_qn: cn.webank.cnc.mes.service.process.S27204
  index_commit: <git sha>          # 索引基于哪个 commit，过期即不可信
  generated_by: repo-intelligence@0.1.10

target:
  qn: cn.webank.cnc.mes.service.process.S27204   # 必填，唯一契约键
  short: S27204                                   # 可选，仅展示
  file: mes/mes-service-dcn/.../S27204.java       # 必填
  bean: "mes.S27204"                              # 可选
  interface: IBizProcess<S27204Req, S27204Resp>   # 可选
  business: "权益拉取-推荐券查询"                   # 可选

edges:
  inbound:  [...]   # 进入 target 的边
  outbound: [...]   # target 发出的边（单一事实源，不另维护分组视图）

related:            # 旁支，不污染主结构；各项均可缺省
  homonyms: [...]
  trade_log: {...}
  aliases: [...]
```

## 锚点（anchor）

所有"定位到代码某处"的字段统一用 anchor 对象，禁止 `"文件:行 语义"` 这种混合字符串。

```yaml
# 单点
anchor: {file: S27204.java, line: 133, kind: call, note: "可选人类说明"}
# 多行范围用数组
anchor: {file: S27204.java, line: [810, 829], kind: call}
# 同一处多个点用列表
anchors:
  - {file: BaseRmbService.java, line: 128, kind: call}
  - {file: BaseRmbService.java, line: 138, kind: call}
```

`kind` 受控词表：`call` `def` `field` `anno`（注解/annotation）`comment` `param` `sig`（签名）`import` `decl`。不在表内的用 `custom:<name>` 并补 `note`。

字段规则：`file`、`line` 必填；`line` 为 `int` 或 `[start, end]`；`kind`、`note` 可选。

## 边（edges）

inbound（谁指向 target）与 outbound（target 指向谁）各一个列表。每条边的对端统一用 `peer`（inbound.peer = 调用方/上游，outbound.peer = 被调方/下游）——数组归属已编码方向，`peer` 就是"另一端"。

```yaml
edges:
  inbound:
    - peer: {qn: cn.webank...BaseRmbService, short: BaseRmbService}
      edge_type: framework_dispatch      # 语义层：工具给不出，需 agent 定
      edge_kind: depends_on              # 结构层：工具原生 EdgeKind（snake_case）
      layer: domain
      anchor: {file: mes/mes-facility/.../BaseRmbService.java, line: [128,138]}
      note: "getBean(mes.+serviceCode) → bizProcess; AOP biz-interceptor.xml:28"
  outbound:
    - peer: {qn: cn.webank...CallCPS, short: CallCPS}
      edge_type: http_out                # 语义层（此处工具可机械映射）
      edge_kind: matches_endpoint        # 结构层：工具原生 EdgeKind
      layer: remote
      anchor: {file: S27204.java, line: 133}
      finding: {...}     # 可选：仅当这条边本身是 finding 时挂
```

字段规则：

- 每条边必填 `peer.qn` + `edge_type` + `edge_kind` + `anchor`（file+line）。
- `edge_kind` 是工具原生 `EdgeKind`（snake_case，见下表），结构层，机器直填。
- `edge_type` 是语义层标签：机器能机械映射的就填映射值；映射不了或属纯语义判断的填 `custom:needs-review`，由 agent 补。
- `qn` 是回喂工具的契约键；`short` 仅展示。
- `layer` 是对端所属架构层，与 `edge_type` 正交。
- **不维护 `outbound_groups`**：需要按层聚合时，由消费方对 `edges.outbound` 做 `group by layer`。

### edge_type（语义层）受控词表（半开放）

`edge_type` 是给人/agent 的语义标签，与工具原生 `edge_kind` 分开。半开放：核心闭集 + `custom:<name>` 兜底（须补 `note`）。

| edge_type | 含义 | 工具能否自动映射 |
|---|---|---|
| `call` | 同文件方法调用 | ✓ `calls` |
| `http_out` | 远程/跨栈出站 | ✓ `matches_endpoint`/`sends_http_request` |
| `db_read` / `db_write` | 经 Mapper 的 DB 读/写 | ✓ `reads_table`/`executes_sql`/`reads_column`/`writes_*` |
| `inject` | DI 注入（@Autowired/@Resource） | ✓ `depends_on`→SpringBean |
| `field_propagate` | 跨层同名字段传播 | ✓ `mapped_from` |
| `delegate` | 同语言委托（S27501→S27204） | ✗ 需 agent（语义） |
| `framework_dispatch` | 框架/调度入站（rmb、Spring MVC） | ✗ 需 agent（无 controller→endpoint 边） |
| `inject_dead` | 死注入（无调用路径） | ✗ 需 agent（运行时使用分析） |
| `domain_delegate` / `repo_call` / `infra_util` | 领域委托/仓储/工具 | △ 可由 `layer` 近似，精确分类需 agent |
| `custom:<name>` | 兜底 | 须补 `note` |

✓ = `build_relay_doc`/`relay` 自动填；✗ = 工具检测不到，骨架里标 `custom:needs-review`，由 agent 补。

### EdgeKind → edge_type 机械映射（工具自动产出）

本项目 `EdgeKind` 枚举（17 个，`crates/model/src/lib.rs`）与 edge_type 的映射：

| EdgeKind（snake_case） | edge_type |
|---|---|
| `calls` | `call` |
| `matches_endpoint` / `sends_http_request` | `http_out` |
| `reads_table` / `executes_sql` / `reads_column` / `binds_to_statement` | `db_read` |
| `writes_table` / `writes_column` | `db_write` |
| `depends_on`（→ spring_bean） | `inject` |
| `depends_on`（→ table/column/database） | `db_read` |
| `mapped_from` | `field_propagate` |
| `contains`/`declares`/`exposes`/`submodule_of`/`has_response_field`/`serialized_from` | 不作为接力边（结构边，过滤掉） |
| 其他 | `custom:needs-review` |

### layer 取值

`remote` `domain` `db_mapper` `infra`。非穷举。

## finding

```yaml
finding:
  id: "1"
  action: delete          # delete | modify | add | verify
  target: {file: S27204.java, line: [810,829], kind: call}   # 可选：finding 的主作用点
  reason: "消除 CPS23221 空跑"
  risk: medium            # low | medium | high，可选
  links:                  # 联动点，统一 anchor；删/改时一处不漏
    - {file: S27204.java, line: 808, kind: comment, note: "CPS23221 注释"}
    - {file: S27204.java, line: [810,829], kind: call, note: "FutureTask queryEquity"}
    - {file: S27204.java, line: 876, kind: param}
    - {file: S27204.java, line: 1345, kind: sig}
    - {file: S27204.java, line: 184, kind: field, note: "threadPool"}
```

字段规则：`id`、`action`、`reason` 必填；`target`、`risk`、`links` 可选。`links` 里每个元素必须是完整 anchor 对象，不得写 `:876 传参` 这种省文件写法。

## related（旁支）

```yaml
related:
  homonyms:                 # 同名不同物，防误连
    - qn: cn.webank.cnc.mos...S27204
      file: mos/mes-mosweb/.../web/rmb/api/S27204.java
      api: "@MosApi('S27204')"
      tenant: DSDL
      note: "数据监控，与 mes 权益无关，transCode 同名巧合"
  trade_log:
    qn: cn.webank...TRD27204
    file: mes/.../tradeLog/TRD27204.java
    eventType: "27204"
  aliases: []               # 显式空数组表示"已查，无"
```

## 工具自动产出（`build_relay_doc` / `relay` CLI）

MCP 端点 `build_relay_doc` 与 CLI `relay <qn>` 产出本 schema 的**结构层骨架**：

- **自动填**：`target.qn/short/kind/anchor`、每条边的 `peer`/`edge_kind`/`edge_type`（机械映射）/`layer`/`anchor`、`related.homonyms`。
- **留 `custom:needs-review`（agent 补）**：`target.bean`（实例 id）、`target.interface`、`target.business`、纯语义 `edge_type`（`framework_dispatch`/`inject_dead`/`delegate`/...）、`finding`、`related.trade_log`。
- 顶层 `hint` 字段标注已知限制。

### 已知限制（影响 inbound 完整性）

- **跨文件 Java `calls` 边未提取**：`Calls` 边只在同文件内输出。被跨文件调度的目标（Spring bean 入口、被其他类调用的方法）其 **inbound 会缺失**——这正是 agent 要补的部分。`trace_callers` 复核时同样受此限。
- **anchor 仅单行**：提取器目前 `end_line == start_line`；多行范围留待提取器补全。
- **无语法 `kind`**：anchor 只有 `file`+`line`，不带 `call`/`def`/`field` 等 `kind`（工具的 `classification` 是证据来源，非语法类型）；`kind` 由 agent 按需补。
- **MapperMethod / HttpEndpoint owner**：`MapperMethod` 声明但未提取；`HttpEndpoint` 无显式 owner-class 边（仅隐式共享文件）。

## 工具回喂契约

| 字段 | 回喂工具 |
|---|---|
| `target.qn` / `to.qn` / `from.qn` | `trace_callers` / `trace_callees` / `get_code_snippet` |
| `*.short` | `search_entities(name_pattern=...)` |
| `anchor.file` + `line` | `Read`（精确行） |
| `edge_type` | 仅供判断，无直接工具（语义层） |

## 约定与校验

- **必填缺一不可**：`meta.schema_version`、`meta.target_qn`、`target.qn`、`target.file`；每条边 `edge_type` + 端点 `qn` + `anchor` 的 `file`+`line`；每个 finding 的 `id`+`action`+`reason`。
- **缺省语义**：key 不存在 = 该信息未采集；`[]` = 已查且为空；不用 `null` 占位。
- **字符串引号**：含中文、空格、冒号、特殊字符的值用双引号；纯标识符可不加。
- **生成方 checklist**：锚点全是对象、edge_type 在表内或 `custom:` 带注、outbound 无重复分组视图、finding.links 无省文件写法、related 已查（空也写 `[]`）。
- **消费方 checklist**：先读 `meta.index_commit` 判新鲜度；按 `qn` 回喂工具；按 `edge_type`/`layer` 过滤；`inject_dead`/`custom:*` 不可直接当工具结论，需人工或 note 确认。
