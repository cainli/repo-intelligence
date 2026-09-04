//! 向量层(v0.1.26):本地 ONNX(fastembed-rs)为 entity 文本生成 embedding,
//! 对标 codebase-memory 的 `node_vectors`。这是"语义"层(FTS 是"全文"层)。
//!
//! 模型随 crate 打包分发(crates/embedding/models/),用 `UserDefinedEmbeddingModel`
//! 本地加载——绕开 fastembed 5.x 经 hf-hub 下载的 Content-Range bug,用户零联网。
//! v0.1.37 起 Xenova/paraphrase-multilingual-MiniLM-L12-v2(多语量化版,384 维):
//! 英文 AllMiniLM 对中文 query 几乎逐字拆分、检索失效(评测 C8),多语模型同为
//! 384 维,schema 与 text_hash 缓存机制不变。

use anyhow::{Context, Result};
use fastembed::{
    InitOptionsUserDefined, Pooling, TextEmbedding, TokenizerFiles, UserDefinedEmbeddingModel,
};

/// 本地 ONNX 文本 embedder。包装 fastembed 单例模型,scan 时批量生成 embedding。
pub struct Embedder {
    model: TextEmbedding,
}

impl Embedder {
    /// 模型指纹:scan 的 text_hash 以此为前缀,换模型后旧向量自动失效重算。
    pub const MODEL_ID: &str = "paraphrase-multilingual-MiniLM-L12-v2-quantized";
}

impl Embedder {
    /// 从 binary 内嵌的模型字节加载 ONNX(自包含——模型 `include_bytes!` 编译进 binary,
    /// 运行时零文件依赖,故发布的 npm binary 无需额外分发模型文件;修复 v0.1.26 发布包
    /// 运行时读 CARGO_MANIFEST_DIR 找不到 tokenizer.json 的致命 bug)。
    pub fn new() -> Result<Self> {
        let tokenizer_files = TokenizerFiles {
            tokenizer_file: include_bytes!(
                "../models/paraphrase-multilingual-MiniLM-L12-v2/tokenizer.json"
            )
            .to_vec(),
            config_file: include_bytes!(
                "../models/paraphrase-multilingual-MiniLM-L12-v2/config.json"
            )
            .to_vec(),
            special_tokens_map_file: include_bytes!(
                "../models/paraphrase-multilingual-MiniLM-L12-v2/special_tokens_map.json"
            )
            .to_vec(),
            tokenizer_config_file: include_bytes!(
                "../models/paraphrase-multilingual-MiniLM-L12-v2/tokenizer_config.json"
            )
            .to_vec(),
        };
        let mut model = UserDefinedEmbeddingModel::new(
            include_bytes!("../models/paraphrase-multilingual-MiniLM-L12-v2/model.onnx").to_vec(),
            tokenizer_files,
        );
        // AllMiniLML6V2 用 mean pooling(对短文本/entity 名效果好)。
        model.pooling = Some(Pooling::Mean);
        let model =
            TextEmbedding::try_new_from_user_defined(model, InitOptionsUserDefined::default())
                .context("加载本地 ONNX 模型失败")?;
        Ok(Self { model })
    }

    /// 批量生成 embedding。返回顺序与输入一致,每条 384 维 f32。
    /// fastembed 5.x 的 embed 需 &mut(内部 ONNX session 状态),故本方法 &mut self。
    pub fn embed(&mut self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        let embeddings = self
            .model
            .embed(texts, None)
            .context("fastembed 推理失败")?;
        Ok(embeddings)
    }

    /// 模型维度(MiniLM-L6/L12 均为 384,换模型 schema 不变)。建表/存储用。
    pub const DIM: usize = 384;
}

/// 余弦相似度(语义检索打分核)。向量运算的天然归属在此 crate,
/// mcp 的 semantic_search 与 cli 的 semantic-search 子命令共用同一实现,避免复制漂移。
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    // 维度不等(跨版本 embedding / 损坏行):zip 会静默截断算出无意义有限值,
    // 显式判 0 避免错误排序。含 NaN/Inf 的损坏向量算出的结果也归 0,
    // 让调用方不必各自处理 NaN(json! 序列化 NaN 会 panic / serde_json Err)。
    if a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    let score = if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    };
    if score.is_finite() { score } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore] // 加载 ONNX 重,手动跑估算耗时。
    fn spike_embed_batch_and_time() {
        let mut embedder = Embedder::new().unwrap();
        let texts: Vec<String> = (0..100)
            .map(|i| format!("UserService method handleRequest {i}"))
            .collect();
        let start = std::time::Instant::now();
        let out = embedder.embed(texts).unwrap();
        let elapsed = start.elapsed();
        assert_eq!(out.len(), 100);
        assert_eq!(out[0].len(), Embedder::DIM);
        eprintln!(
            "[spike] embed 100 texts in {:.2}s ({:.2}ms/text → 6854 ≈ {:.1}s)",
            elapsed.as_secs_f64(),
            elapsed.as_secs_f64() * 10.0,
            elapsed.as_secs_f64() * 68.54
        );
    }
}

#[cfg(test)]
mod multilingual_smoke {
    use super::*;

    #[test]
    fn chinese_query_matches_chinese_doc() {
        let mut e = Embedder::new().unwrap();
        let corpus = vec![
            "method login 用户登录".to_string(),
            "class AuthController POST /auth/login".to_string(),
            "method insertUser 新增保存用户信息".to_string(),
            "table sys_login_info 登录日志".to_string(),
            "method exportExcel 导出 Excel".to_string(),
        ];
        let doc_vecs = e.embed(corpus.clone()).unwrap();
        // 中文 query 命中含中文释义的实体文本(英文模型做不到的场景,v0.1.37 的目标);
        // 英文 query 不得回归(对齐旧英文模型的基线能力)。
        for (q, expect, max_rank) in [
            ("用户登录认证", 0, 1usize),
            ("登录日志记录", 3, 2usize),
            // 英文 query 的最佳答案同样是 login 方法(跨语料 top1)。
            ("user login authentication", 0, 1usize),
        ] {
            let qv = e.embed(vec![q.to_string()]).unwrap().pop().unwrap();
            let mut ranked: Vec<(usize, f32)> = doc_vecs
                .iter()
                .enumerate()
                .map(|(i, v)| (i, cosine(&qv, v)))
                .collect();
            ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
            let rank = ranked.iter().position(|(i, _)| *i == expect).unwrap();
            assert!(
                rank < max_rank,
                "query {q:?} 应让 {} 进 top{max_rank}, got {ranked:?}",
                corpus[expect]
            );
            eprintln!(
                "smoke: {q:?} → {:?} ({:.3})",
                corpus[ranked[0].0], ranked[0].1
            );
        }
    }
}
