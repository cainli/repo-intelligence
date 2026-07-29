use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EntityId(pub String);

impl EntityId {
    pub fn stable(
        source_id: &str,
        relative_path: &str,
        kind: EntityKind,
        qualified_name: &str,
        discriminator: &str,
    ) -> Self {
        let identity = format!(
            "{source_id}\0{relative_path}\0{}\0{qualified_name}\0{discriminator}",
            kind.as_str()
        );
        Self(blake3::hash(identity.as_bytes()).to_hex().to_string())
    }
}

impl fmt::Display for EntityId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    Workspace,
    Repository,
    Submodule,
    File,
    Package,
    Class,
    Interface,
    Method,
    Field,
    VuePage,
    VueComponent,
    FrontendField,
    HttpClientCall,
    HttpEndpoint,
    ApiField,
    SpringBean,
    Mapper,
    MapperMethod,
    XmlStatement,
    ResultMap,
    SqlField,
    Datasource,
    Database,
    Table,
    Column,
    TestCase,
    ConfigFile,
}

impl EntityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::Repository => "repository",
            Self::Submodule => "submodule",
            Self::File => "file",
            Self::Package => "package",
            Self::Class => "class",
            Self::Interface => "interface",
            Self::Method => "method",
            Self::Field => "field",
            Self::VuePage => "vue_page",
            Self::VueComponent => "vue_component",
            Self::FrontendField => "frontend_field",
            Self::HttpClientCall => "http_client_call",
            Self::HttpEndpoint => "http_endpoint",
            Self::ApiField => "api_field",
            Self::SpringBean => "spring_bean",
            Self::Mapper => "mapper",
            Self::MapperMethod => "mapper_method",
            Self::XmlStatement => "xml_statement",
            Self::ResultMap => "result_map",
            Self::SqlField => "sql_field",
            Self::Datasource => "datasource",
            Self::Database => "database",
            Self::Table => "table",
            Self::Column => "column",
            Self::TestCase => "test_case",
            Self::ConfigFile => "config_file",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceClass {
    Fact,
    Resolved,
    Inferred,
    RuntimeUnknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    pub file: String,
    pub start_line: u32,
    pub end_line: u32,
    pub classification: EvidenceClass,
    pub confidence: f32,
    pub reason: String,
    /// 对应行的源码片段(scan 时从文件内容填充;跨文件推断边的 evidence file 非当前
    /// 文件,留 None)。让 agent 不必 Read 文件即可快速判断该证据是否成立。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

impl Evidence {
    pub fn new(
        file: impl Into<String>,
        start_line: u32,
        end_line: u32,
        classification: EvidenceClass,
        confidence: f32,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            file: file.into(),
            start_line,
            end_line,
            classification,
            confidence: confidence.clamp(0.0, 1.0),
            reason: reason.into(),
            snippet: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entity {
    pub id: EntityId,
    pub kind: EntityKind,
    pub name: String,
    pub qualified_name: String,
    pub metadata: serde_json::Value,
    pub evidence: Vec<Evidence>,
}

impl Entity {
    pub fn new(
        id: EntityId,
        kind: EntityKind,
        name: impl Into<String>,
        qualified_name: impl Into<String>,
    ) -> Self {
        Self {
            id,
            kind,
            name: name.into(),
            qualified_name: qualified_name.into(),
            metadata: serde_json::Value::Null,
            evidence: Vec::new(),
        }
    }

    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = metadata;
        self
    }

    pub fn with_evidence(
        mut self,
        file: impl Into<String>,
        start_line: u32,
        end_line: u32,
        classification: EvidenceClass,
        confidence: f32,
        reason: impl Into<String>,
    ) -> Self {
        self.evidence.push(Evidence::new(
            file,
            start_line,
            end_line,
            classification,
            confidence,
            reason,
        ));
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    Contains,
    Declares,
    Calls,
    Exposes,
    SendsHttpRequest,
    MatchesEndpoint,
    HasResponseField,
    SerializedFrom,
    MappedFrom,
    BindsToStatement,
    ExecutesSql,
    ReadsTable,
    WritesTable,
    ReadsColumn,
    WritesColumn,
    DependsOn,
    Injects,
    SubmoduleOf,
}

impl EdgeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Contains => "contains",
            Self::Declares => "declares",
            Self::Calls => "calls",
            Self::Exposes => "exposes",
            Self::SendsHttpRequest => "sends_http_request",
            Self::MatchesEndpoint => "matches_endpoint",
            Self::HasResponseField => "has_response_field",
            Self::SerializedFrom => "serialized_from",
            Self::MappedFrom => "mapped_from",
            Self::BindsToStatement => "binds_to_statement",
            Self::ExecutesSql => "executes_sql",
            Self::ReadsTable => "reads_table",
            Self::WritesTable => "writes_table",
            Self::ReadsColumn => "reads_column",
            Self::WritesColumn => "writes_column",
            Self::DependsOn => "depends_on",
            Self::Injects => "injects",
            Self::SubmoduleOf => "submodule_of",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub source: EntityId,
    pub target: EntityId,
    pub kind: EdgeKind,
    pub evidence: Vec<Evidence>,
}

impl Edge {
    pub fn new(source: EntityId, target: EntityId, kind: EdgeKind) -> Self {
        Self {
            source,
            target,
            kind,
            evidence: Vec::new(),
        }
    }

    pub fn with_evidence(
        mut self,
        file: impl Into<String>,
        start_line: u32,
        end_line: u32,
        classification: EvidenceClass,
        confidence: f32,
        reason: impl Into<String>,
    ) -> Self {
        self.evidence.push(Evidence::new(
            file,
            start_line,
            end_line,
            classification,
            confidence,
            reason,
        ));
        self
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GraphPatch {
    pub add_entities: Vec<Entity>,
    pub add_edges: Vec<Edge>,
    pub remove_entities: Vec<EntityId>,
}

impl GraphPatch {
    pub fn add(add_entities: Vec<Entity>, add_edges: Vec<Edge>) -> Self {
        Self {
            add_entities,
            add_edges,
            remove_entities: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchQuery {
    pub text: String,
    pub limit: usize,
    /// Number of matches to skip before the first returned row. Combined with
    /// `limit` this gives keyset-free pagination; the MCP search tools expose it
    /// as `offset` so a wide substring match (e.g. an enterprise ID that names
    /// dozens of entities) can be paged instead of returned in one huge batch.
    #[serde(default)]
    pub offset: usize,
}

impl SearchQuery {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            limit: 50,
            offset: 0,
        }
    }

    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    pub fn with_offset(mut self, offset: usize) -> Self {
        self.offset = offset;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraverseQuery {
    pub start: EntityId,
    pub outbound: bool,
    pub max_depth: usize,
    /// Restricts the BFS to these edge kinds. Empty = walk every edge kind
    /// (the historical behavior `analyze_change` relies on). Non-empty = only
    /// traverse along the listed kinds, so a callers/callees query can stay on
    /// the `Calls` edge instead of being polluted by `Contains`/`DependsOn`.
    #[serde(default)]
    pub edge_kinds: Vec<EdgeKind>,
}

impl TraverseQuery {
    pub fn outbound(start: EntityId) -> Self {
        Self {
            start,
            outbound: true,
            max_depth: 1,
            edge_kinds: Vec::new(),
        }
    }

    pub fn with_depth(mut self, max_depth: usize) -> Self {
        self.max_depth = max_depth;
        self
    }

    /// Restrict the traversal to the given edge kinds (empty walks all kinds).
    pub fn with_kinds(mut self, edge_kinds: Vec<EdgeKind>) -> Self {
        self.edge_kinds = edge_kinds;
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeOperation {
    Add,
    Remove,
    Rename,
    ChangeType,
    ChangeNullable,
    ChangeFormat,
    ChangeSemantics,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeRequest {
    pub target_kind: String,
    pub operation: ChangeOperation,
    pub from: Option<String>,
    pub to: Option<String>,
    /// Maximum number of impact findings to return (server caps this).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Number of findings to skip — the result window is `[offset, offset+limit)`.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Graph traversal depth around each finding. `None` selects an
    /// operation-appropriate default (destructive ops stay shallow).
    #[serde(default)]
    pub depth: Option<usize>,
}

impl ChangeRequest {
    pub fn rename_field(from: impl Into<String>, to: impl Into<String>) -> Self {
        Self {
            target_kind: "field".into(),
            operation: ChangeOperation::Rename,
            from: Some(from.into()),
            to: Some(to.into()),
            limit: None,
            offset: None,
            depth: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ImpactFinding {
    pub entity: Entity,
    pub plane: String,
    pub severity: String,
    /// 该 finding 可达性的最低证据置信度。1.0 = 仅经 Fact/Resolved 边可达;
    /// <1.0 = path 上触及 Inferred 边(如分级匹配产生的低置信 matches_endpoint)。
    /// 客户端据此区分"精确命中"与"推断命中"。分页当前不按此排序(那需要
    /// 遍历全部候选),后续可做两阶段排序让高置信优先占用分页额度。
    pub confidence: f32,
    pub path: Vec<EntityId>,
    pub evidence: Vec<Evidence>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ImpactReport {
    pub findings: Vec<ImpactFinding>,
    pub open_questions: Vec<String>,
    /// Total candidate findings before pagination (the full fan-out count).
    pub total: usize,
    /// Page size in effect for this result.
    pub limit: usize,
    /// Offset in effect for this result.
    pub offset: usize,
    /// True when `offset + limit < total` — more findings are available.
    pub has_more: bool,
}
