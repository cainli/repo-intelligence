#!/usr/bin/env bash
# 下载 paraphrase-multilingual-MiniLM-L12-v2(onnx 量化版,~118MB)到打包目录。
# 绕开 fastembed 5.x 经 hf-hub 下载的 Content-Range bug:用 curl 直连 hf-mirror 镜像。
# v0.1.37 起模型由 all-MiniLM-L6-v2 换为多语版(与 src/lib.rs 的 include_bytes! 路径一致)。
# 用途:新 clone/CI 拉取模型(crates/embedding/models/ 默认 gitignore,不进仓库)。
set -euo pipefail
DIR="$(cd "$(dirname "$0")/.." && pwd)/models/paraphrase-multilingual-MiniLM-L12-v2"
mkdir -p "$DIR"
BASE="https://hf-mirror.com/Xenova/paraphrase-multilingual-MiniLM-L12-v2/resolve/main"
fetch() { # src dst
  local code; code=$(curl -sL -w "%{http_code}" -o "$DIR/$2" "$BASE/$1")
  echo "$2 ← $1  HTTP $code  $(du -h "$DIR/$2" | cut -f1)"
  [ "$code" = "200" ] || { echo "FAIL: $2"; exit 1; }
}
fetch "onnx/model_quantized.onnx"   "model.onnx"
fetch "tokenizer.json"              "tokenizer.json"
fetch "tokenizer_config.json"       "tokenizer_config.json"
fetch "config.json"                 "config.json"
fetch "special_tokens_map.json"     "special_tokens_map.json"
echo "OK → $DIR"
