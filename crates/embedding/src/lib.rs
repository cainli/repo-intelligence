//! 向量层(v0.1.26):本地 ONNX(fastembed-rs)为 entity 文本生成 embedding,
//! 对标 codebase-memory 的 `node_vectors`。这是"语义"层(FTS 是"全文"层)。
//!
//! 模型 AllMiniLML6V2(384 维,量化 ~22MB)随 crate 打包分发(crates/embedding/models/),
//! 用 `UserDefinedEmbeddingModel` 本地加载——绕开 fastembed 5.x 经 hf-hub 下载的
//! Content-Range bug,用户零联网。模型来源 Xenova/all-MiniLM-L6-v2(onnx 量化版)。

use anyhow::{Context, Result};
use fastembed::{
    InitOptionsUserDefined, Pooling, TextEmbedding, TokenizerFiles, UserDefinedEmbeddingModel,
};

/// 本地 ONNX 文本 embedder。包装 fastembed 单例模型,scan 时批量生成 embedding。
pub struct Embedder {
    model: TextEmbedding,
}

impl Embedder {
    /// 从 binary 内嵌的模型字节加载 ONNX(自包含——模型 `include_bytes!` 编译进 binary,
    /// 运行时零文件依赖,故发布的 npm binary 无需额外分发模型文件;修复 v0.1.26 发布包
    /// 运行时读 CARGO_MANIFEST_DIR 找不到 tokenizer.json 的致命 bug)。
    pub fn new() -> Result<Self> {
        let tokenizer_files = TokenizerFiles {
            tokenizer_file: include_bytes!("../models/all-MiniLM-L6-v2/tokenizer.json").to_vec(),
            config_file: include_bytes!("../models/all-MiniLM-L6-v2/config.json").to_vec(),
            special_tokens_map_file: include_bytes!("../models/all-MiniLM-L6-v2/special_tokens_map.json").to_vec(),
            tokenizer_config_file: include_bytes!("../models/all-MiniLM-L6-v2/tokenizer_config.json").to_vec(),
        };
        let mut model = UserDefinedEmbeddingModel::new(
            include_bytes!("../models/all-MiniLM-L6-v2/model.onnx").to_vec(),
            tokenizer_files,
        );
        // AllMiniLML6V2 用 mean pooling(对短文本/entity 名效果好)。
        model.pooling = Some(Pooling::Mean);
        let model = TextEmbedding::try_new_from_user_defined(
            model,
            InitOptionsUserDefined::default(),
        )
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

    /// 模型维度(AllMiniLML6V2 = 384)。建表/存储用。
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
    let score = if na == 0.0 || nb == 0.0 { 0.0 } else { dot / (na * nb) };
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
