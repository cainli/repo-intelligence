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
        Self::new_with_threads(0)
    }

    /// 同 `new()`,但可限 ort intra-op 线程数(0 = 不限,吃满所有核)。
    /// 目标机器不能被推理打满时由 scan 配置传入小值(如 2)。
    pub fn new_with_threads(intra_threads: usize) -> Result<Self> {
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
        // intra_threads=0 传默认(None)= 用满所有核;>0 时封顶,限 CPU 占用换吞吐。
        let options = InitOptionsUserDefined::default();
        let options = if intra_threads > 0 {
            options.with_intra_threads(intra_threads)
        } else {
            options
        };
        let model = TextEmbedding::try_new_from_user_defined(model, options)
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

/// 标识符分词(第五轮反馈 F2):camelCase/PascalCase/SNAKE/kebab/路径复合串拆成
/// 空格分隔词,让「概念 query」与「标识符向量」进入同一语义空间。
///
/// 背景:embedding 文本此前原样拼 qn/name——`checkLoanTerms` 经 WordPiece 切成碎片,
/// 与整句 query「check loan terms validation」的余弦低而平(ruoyi 实测英文概念 query
/// top 全是 0.45-0.47 的噪声,核心方法零上榜)。分词后实体侧与 query 侧同函数处理,
/// 幂等(已分词文本再跑不变,词内无 case/分隔符边界)。
///
/// 规则:①按非字母数字字符(`/ _ - . : # $ @` 与空白等)切段;②段内拆 case 边界
/// (小写→大写;大写 run 后跟小写时,run 末位大写归下一段,`XMLHttpRequest` →
/// `XML Http Request`);③**不拆字母-数字边界**(`base64`/`S27204`/`utf8` 保持原样,
/// 哈希串/交易码拆了反而破坏与 query 的一致性);④中文等无 case 文字原样透传。
pub fn word_split(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    let mut current_word = String::new();
    fn flush(out: &mut String, word: &mut String) {
        if !word.is_empty() {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(word);
            word.clear();
        }
    }
    for ch in text.chars() {
        if !ch.is_alphanumeric() {
            flush(&mut out, &mut current_word);
            continue;
        }
        // case 边界判定只对 ASCII 大小写有意义;非 ASCII(中文等)与数字只追加不触发。
        let prev = current_word.chars().last();
        let prev_lower = prev.is_some_and(|c| c.is_ascii_lowercase());
        let prev_upper = prev.is_some_and(|c| c.is_ascii_uppercase());
        if prev_lower && ch.is_ascii_uppercase() {
            // 小写→大写:checkLoan → check | Loan
            flush(&mut out, &mut current_word);
            current_word.push(ch);
        } else if prev_upper && ch.is_ascii_lowercase() && current_word.len() > 1 {
            // 大写 run(≥2)后跟小写:run 末位大写归新词。XMLHttp → XML | Http;
            // run 长度 1 时是普通 PascalCase 词首(Loan),不拆。
            let last = current_word.pop();
            flush(&mut out, &mut current_word);
            current_word.push(last.unwrap_or(ch));
            current_word.push(ch);
        } else {
            current_word.push(ch);
        }
    }
    flush(&mut out, &mut current_word);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_split_camel_snake_kebab_and_paths() {
        // camel / Pascal / 缩写 run
        assert_eq!(word_split("checkLoanTerms"), "check Loan Terms");
        assert_eq!(word_split("XMLHttpRequest"), "XML Http Request");
        assert_eq!(word_split("getUserVO"), "get User VO");
        assert_eq!(word_split("Loan"), "Loan");
        // 分隔符族:snake / kebab / 路径 / qn 的 # 与 . 与 $
        assert_eq!(word_split("user_name"), "user name");
        assert_eq!(
            word_split("ruoyi-modules/ruoyi-system"),
            "ruoyi modules ruoyi system"
        );
        assert_eq!(
            word_split(
                "ruoyi-system/src/main/java/org/dromara/SysUserServiceImpl.java#selectUserById"
            ),
            "ruoyi system src main java org dromara Sys User Service Impl java select User By Id"
        );
        // 字母-数字边界不拆(哈希串/交易码/base64 与 query 保持同形)
        assert_eq!(word_split("S27204"), "S27204");
        assert_eq!(word_split("base64"), "base64");
        assert_eq!(word_split("md5Hash"), "md5 Hash");
        // 中文透传;幂等(已分词文本再跑不变)
        assert_eq!(word_split("用户登录认证"), "用户登录认证");
        assert_eq!(word_split("check Loan Terms"), "check Loan Terms");
        assert_eq!(word_split("XML Http Request"), "XML Http Request");
        // 空与纯分隔符
        assert_eq!(word_split(""), "");
        assert_eq!(word_split("---"), "");
    }

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
