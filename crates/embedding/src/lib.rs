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
use std::path::PathBuf;

/// 本地 ONNX 文本 embedder。包装 fastembed 单例模型,scan 时批量生成 embedding。
pub struct Embedder {
    model: TextEmbedding,
}

impl Embedder {
    /// 从打包的本地模型文件加载 ONNX(无网络)。
    pub fn new() -> Result<Self> {
        let dir = model_dir();
        let read = |name: &str| {
            std::fs::read(dir.join(name)).with_context(|| format!("读模型文件 {name}"))
        };
        let tokenizer_files = TokenizerFiles {
            tokenizer_file: read("tokenizer.json")?,
            config_file: read("config.json")?,
            special_tokens_map_file: read("special_tokens_map.json")?,
            tokenizer_config_file: read("tokenizer_config.json")?,
        };
        let mut model = UserDefinedEmbeddingModel::new(read("model.onnx")?, tokenizer_files);
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

/// 打包模型目录(编译时 CARGO_MANIFEST_DIR = crates/embedding)。
fn model_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("models/all-MiniLM-L6-v2")
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
