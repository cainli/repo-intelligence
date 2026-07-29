//! 配置层：把原本写死在 source/semantics/analysis 的"数据型常量"集中到此，
//! 支持 `.repo-intelligence.toml` 覆盖。builtin 值与历史行为完全一致；
//! 零配置（无文件）等价于 `IndexerConfig::default()`。
//!
//! 合并语义（A 方案）：
//! - 标量字段（`max_file_bytes`、各 `limit`）：直接替换 builtin。
//! - 短列表（`custom_endpoint_annotations`，builtin 3 个）：替换——用户填全集即自控。
//! - 长列表（`excluded_dirs`、`frontend_noise`）：只暴露 `_extra` 追加字段，builtin 不动。
//!
//! 每个字段都标了 `#[serde(default = ...)]`，故配置文件只需声明与默认不同的部分，
//! 缺失字段回填 builtin——这是"零配置 = 旧行为"契约的落点。

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// 配置文件固定文件名，放在 workspace 根目录（跟着项目走，非全局）。
pub const CONFIG_FILENAME: &str = ".repo-intelligence.toml";

// ---- builtin 默认值（与历史硬编码一致，单一来源） ----

/// 历史硬编码的目录排除清单（原 `source::EXCLUDED_DIRS`）。
pub const DEFAULT_EXCLUDED_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "build",
    "dist",
    ".gradle",
    ".git",
    ".claude",
    ".repo-intelligence",
];

/// 自研 RPC 框架端点注解（原 `semantics::CUSTOM_ENDPOINT_ANNOTATIONS`）。
pub const DEFAULT_CUSTOM_ENDPOINT_ANNOTATIONS: &[&str] =
    &["RmbMap", "DubboService", "RpcMapping"];

/// 自研 RPC 框架的入口标记接口：implements 这些接口的类视为业务入口（mes/mos 的 RMB
/// 入口普遍用 `@MosApi + implements ApiHandler`、`implements IBizProcess` 这套自定义框架，
/// 纯注解识别覆盖不到）。
pub const DEFAULT_CUSTOM_ENDPOINT_INTERFACES: &[&str] = &["ApiHandler", "IBizProcess"];

/// 前端属性访问噪声词（原 `semantics::FRONTEND_NOISE`）。
pub const DEFAULT_FRONTEND_NOISE: &[&str] = &[
    "length", "size", "toString", "valueOf", "prototype", "constructor",
    "call", "apply", "bind", "push", "pop", "shift", "unshift",
    "split", "join", "slice", "splice", "concat", "reverse", "sort",
    "map", "filter", "forEach", "find", "findIndex", "some", "every", "reduce", "reduceRight",
    "includes", "indexOf", "lastIndexOf", "flat", "flatMap", "fill", "copyWithin",
    "floor", "ceil", "round", "random", "abs", "max", "min", "pow", "sqrt", "log", "exp", "sign",
    "keys", "values", "entries", "assign", "freeze", "from", "isArray", "create", "getPrototypeOf",
    "trim", "trimStart", "trimEnd", "replace", "replaceAll", "match", "matchAll", "search",
    "toLowerCase", "toUpperCase", "charAt", "charCodeAt", "padStart", "padEnd", "startsWith", "endsWith",
    "then", "catch", "finally", "resolve", "reject", "all", "race", "allSettled",
    "log", "error", "warn", "info", "debug", "time", "timeEnd",
    "createElement", "appendChild", "querySelector", "querySelectorAll", "getElementById",
    "addEventListener", "removeEventListener", "preventDefault", "stopPropagation",
];

const DEFAULT_MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const DEFAULT_IMPACT_LIMIT: usize = 100;
const DEFAULT_MAX_IMPACT_LIMIT: usize = 500;
const DEFAULT_MAX_SEARCH_LIMIT: usize = 1000;

fn default_max_file_bytes() -> u64 {
    DEFAULT_MAX_FILE_BYTES
}
fn default_impact_limit() -> usize {
    DEFAULT_IMPACT_LIMIT
}
fn default_max_impact_limit() -> usize {
    DEFAULT_MAX_IMPACT_LIMIT
}
fn default_max_search_limit() -> usize {
    DEFAULT_MAX_SEARCH_LIMIT
}
fn default_custom_endpoint_annotations() -> Vec<String> {
    DEFAULT_CUSTOM_ENDPOINT_ANNOTATIONS
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}
fn default_custom_endpoint_interfaces() -> Vec<String> {
    DEFAULT_CUSTOM_ENDPOINT_INTERFACES
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexerConfig {
    #[serde(default)]
    pub discovery: DiscoveryConfig,
    #[serde(default)]
    pub semantics: SemanticsConfig,
    #[serde(default)]
    pub analysis: AnalysisConfig,
}

impl IndexerConfig {
    /// 从 workspace 根目录加载配置。无文件则返回 builtin default。
    /// 文件存在但解析失败时返回错误（fail-fast，避免静默用错配置）。
    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join(CONFIG_FILENAME);
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(&path)
            .with_context(|| format!("read config {}", path.display()))?;
        let config: IndexerConfig = toml::from_str(&text)
            .with_context(|| format!("parse config {}", path.display()))?;
        Ok(config)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscoveryConfig {
    /// 追加到 builtin 排除目录（builtin 不动）。长列表用追加语义。
    #[serde(default)]
    pub excluded_dirs_extra: Vec<String>,
    /// 文件名 glob 模式（匹配 basename 即排除文件），如 `["package-lock.json", "*.log"]`。
    /// 与 `excluded_dirs_extra`（目录名）分工：这里排具体文件，那里排整目录。语法为
    /// glob crate（`*`/`?`，`*` 不跨 `/`；basename 无 `/` 故无碍）。不支持 `!` 取反——
    /// 要保留某文件（如 package.json）就别列它。
    #[serde(default)]
    pub excluded_patterns: Vec<String>,
    /// 单文件大小上限，超过则跳过。替换 builtin（2 MiB）。
    #[serde(default = "default_max_file_bytes")]
    pub max_file_bytes: u64,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            excluded_dirs_extra: Vec::new(),
            excluded_patterns: Vec::new(),
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
        }
    }
}

impl DiscoveryConfig {
    /// 生效的排除目录 = builtin ∪ extra（保持 builtin 在前，去重）。
    pub fn effective_excluded_dirs(&self) -> Vec<String> {
        let mut out: Vec<String> = DEFAULT_EXCLUDED_DIRS
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        for dir in &self.excluded_dirs_extra {
            if !out.iter().any(|existing| existing == dir) {
                out.push(dir.clone());
            }
        }
        out
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SemanticsConfig {
    /// 前端噪声词追加（builtin ~70 个不动）。长列表追加语义。
    #[serde(default)]
    pub frontend_noise_extra: Vec<String>,
    /// 自研 RPC 端点注解。短列表替换语义：默认注入 builtin 3 个，
    /// 用户填写则完全替换（自控全集）。
    #[serde(default = "default_custom_endpoint_annotations")]
    pub custom_endpoint_annotations: Vec<String>,
    /// 自研 RPC 入口标记接口（implements 这些接口的类 = 业务入口）。短列表替换语义：
    /// 默认注入 builtin 2 个（ApiHandler/IBizProcess），用户填写则完全替换。
    #[serde(default = "default_custom_endpoint_interfaces")]
    pub custom_endpoint_interfaces: Vec<String>,
}

impl Default for SemanticsConfig {
    fn default() -> Self {
        Self {
            frontend_noise_extra: Vec::new(),
            custom_endpoint_annotations: default_custom_endpoint_annotations(),
            custom_endpoint_interfaces: default_custom_endpoint_interfaces(),
        }
    }
}

impl SemanticsConfig {
    /// 生效噪声词 = builtin ∪ extra（去重）。
    pub fn effective_frontend_noise(&self) -> Vec<String> {
        let mut out: Vec<String> = DEFAULT_FRONTEND_NOISE
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        for word in &self.frontend_noise_extra {
            if !out.iter().any(|existing| existing == word) {
                out.push(word.clone());
            }
        }
        out
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AnalysisConfig {
    #[serde(default = "default_impact_limit")]
    pub default_impact_limit: usize,
    #[serde(default = "default_max_impact_limit")]
    pub max_impact_limit: usize,
    #[serde(default = "default_max_search_limit")]
    pub max_search_limit: usize,
}

impl Default for AnalysisConfig {
    fn default() -> Self {
        Self {
            default_impact_limit: DEFAULT_IMPACT_LIMIT,
            max_impact_limit: DEFAULT_MAX_IMPACT_LIMIT,
            max_search_limit: DEFAULT_MAX_SEARCH_LIMIT,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn default_matches_historic_builtin_values() {
        // builtin 必须与历史硬编码一致——这是"零配置=旧行为"的契约。
        let cfg = IndexerConfig::default();
        assert_eq!(cfg.discovery.max_file_bytes, 2 * 1024 * 1024);
        assert_eq!(cfg.analysis.default_impact_limit, 100);
        assert_eq!(cfg.analysis.max_impact_limit, 500);
        assert_eq!(cfg.analysis.max_search_limit, 1000);
        assert_eq!(
            cfg.semantics.custom_endpoint_annotations,
            vec!["RmbMap", "DubboService", "RpcMapping"]
        );
        assert_eq!(
            cfg.discovery.effective_excluded_dirs(),
            DEFAULT_EXCLUDED_DIRS
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn partial_config_fills_missing_fields_with_builtin() {
        // 只覆盖一个字段，其余回填 builtin（字段级 default 合并）。
        let toml = r#"
[discovery]
excluded_dirs_extra = ["gen", "generated-sources"]
"#;
        let cfg: IndexerConfig = toml::from_str(toml).unwrap();
        assert_eq!(
            cfg.discovery.excluded_dirs_extra,
            vec!["gen", "generated-sources"]
        );
        assert_eq!(cfg.discovery.max_file_bytes, 2 * 1024 * 1024); // 回填
        assert_eq!(cfg.analysis.default_impact_limit, 100); // 回填
        let dirs = cfg.discovery.effective_excluded_dirs();
        assert_eq!(dirs.len(), DEFAULT_EXCLUDED_DIRS.len() + 2);
        assert!(dirs.contains(&"gen".to_string()));
        assert!(dirs.contains(&"node_modules".to_string()));
    }

    #[test]
    fn custom_endpoint_annotations_replace_semantics() {
        // 短列表替换：用户写全集，builtin 不再混入。
        let toml = r#"
[semantics]
custom_endpoint_annotations = ["HessianMapping"]
"#;
        let cfg: IndexerConfig = toml::from_str(toml).unwrap();
        assert_eq!(
            cfg.semantics.custom_endpoint_annotations,
            vec!["HessianMapping"]
        );
    }

    #[test]
    fn frontend_noise_extra_appends_to_builtin() {
        let toml = r#"
[semantics]
frontend_noise_extra = ["bizUtil", "formatDate"]
"#;
        let cfg: IndexerConfig = toml::from_str(toml).unwrap();
        let noise = cfg.semantics.effective_frontend_noise();
        assert!(noise.contains(&"map".to_string())); // builtin 保留
        assert!(noise.contains(&"bizUtil".to_string())); // 追加
        assert_eq!(noise.len(), DEFAULT_FRONTEND_NOISE.len() + 2);
    }

    #[test]
    fn load_returns_default_when_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = IndexerConfig::load(dir.path()).unwrap();
        assert_eq!(cfg.analysis.default_impact_limit, 100);
    }

    #[test]
    fn load_reads_workspace_config() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = fs::File::create(dir.path().join(CONFIG_FILENAME)).unwrap();
        f.write_all(b"[analysis]\ndefault_impact_limit = 250\n").unwrap();
        drop(f);
        let cfg = IndexerConfig::load(dir.path()).unwrap();
        assert_eq!(cfg.analysis.default_impact_limit, 250);
        assert_eq!(cfg.analysis.max_impact_limit, 500); // 未覆盖，回填
    }

    #[test]
    fn load_fails_fast_on_malformed_config() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = fs::File::create(dir.path().join(CONFIG_FILENAME)).unwrap();
        f.write_all(b"this is not = valid = toml [[").unwrap();
        drop(f);
        assert!(IndexerConfig::load(dir.path()).is_err());
    }

    #[test]
    fn excluded_patterns_default_empty_and_configurable() {
        let cfg = IndexerConfig::default();
        assert!(cfg.discovery.excluded_patterns.is_empty());
        let toml = r#"
[discovery]
excluded_patterns = ["package-lock.json", "*.log"]
"#;
        let cfg: IndexerConfig = toml::from_str(toml).unwrap();
        assert_eq!(
            cfg.discovery.excluded_patterns,
            vec!["package-lock.json".to_string(), "*.log".to_string()]
        );
        // 未覆盖字段回填 builtin
        assert_eq!(cfg.discovery.max_file_bytes, 2 * 1024 * 1024);
        assert!(cfg.discovery.excluded_dirs_extra.is_empty());
    }
}
