#!/usr/bin/env bash
# 发布后校验(P0-A′,第四轮反馈:校验从「建议」升为 CI 强制卡点,release.yml 的
# verify-release job 调用本脚本):npm publish 五包后运行,当场暴露「半截发版」。
#   1. 五包(主包 + 4 平台包)的当前版本必须已存在于 registry;
#   2. 干净目录 npm install 冒烟 + --version 执行(optionalDependencies 缺失在
#      安装阶段被 npm 静默容忍,延迟到运行时才炸——冒烟把它提前到发布当下)。
# 沉降重试:npm registry CDN 传播窗口实测 5-10 分钟且各包独立交错沉降(0.1.48/49
# 两次实测:win32 晚主包 6-10 分钟)——publish 后立即跑会误报 MISS。环境变量:
#   RETRIES      总尝试次数(默认 1 = 不重试,手动场景);CI 设 10
#   INTERVAL_SEC 重试间隔秒(默认 60);CI 设 90(10 次 × 90s ≈ 15 分钟上限)
# 用法:./scripts/release_verify.sh [--registry <url>]   # 默认官方 registry
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="$(node -p "require('${ROOT}/packages/npm/package.json').version")"
MAIN="@cainli/repo-intelligence"
REGISTRY_ARGS=()
if [[ "${1:-}" == "--registry" && -n "${2:-}" ]]; then
  REGISTRY_ARGS=(--registry "$2")
fi

verify_once() {
  local fail=0
  # 主包 + 平台包逐一校验 registry 存在性
  for pkg in "${MAIN}" \
    "${MAIN}-darwin-arm64" \
    "${MAIN}-win32-x64" \
    "${MAIN}-linux-x64-gnu" \
    "${MAIN}-linux-arm64-gnu"; do
    if npm view "${pkg}@${VERSION}" version ${REGISTRY_ARGS[@]+"${REGISTRY_ARGS[@]}"} >/dev/null 2>&1; then
      echo "  ok    ${pkg}@${VERSION}"
    else
      echo "  MISS  ${pkg}@${VERSION}  <-- not on registry (or not yet propagated)"
      fail=1
    fi
  done

  # 干净目录安装冒烟:平台二进制缺失会当场炸
  if [[ $fail -eq 0 ]]; then
    local tmp
    tmp="$(mktemp -d)"
    echo "== smoke install into ${tmp} =="
    if npm install --prefix "${tmp}" ${REGISTRY_ARGS[@]+"${REGISTRY_ARGS[@]}"} "${MAIN}@${VERSION}" >/dev/null 2>&1 \
      && "${tmp}/node_modules/.bin/repo-intelligence" --version >/dev/null 2>&1; then
      echo "  ok    npm install + --version"
    else
      echo "  FAIL  npm install smoke (platform binary missing or broken)"
      echo "       rerun manually to inspect:"
      echo "         npm install --prefix ${tmp} ${REGISTRY_ARGS[@]+"${REGISTRY_ARGS[@]}"} ${MAIN}@${VERSION}"
      echo "         ${tmp}/node_modules/.bin/repo-intelligence --version"
      fail=1
    fi
    rm -rf "${tmp}"
  fi
  return $fail
}

RETRIES="${RETRIES:-1}"
INTERVAL_SEC="${INTERVAL_SEC:-60}"
echo "== release verify: ${MAIN}@${VERSION} (retries=${RETRIES}, interval=${INTERVAL_SEC}s) =="
attempt=1
while true; do
  echo "--- attempt ${attempt}/${RETRIES} ---"
  if verify_once; then
    echo "RESULT: PASS"
    exit 0
  fi
  if [[ ${attempt} -ge ${RETRIES} ]]; then
    echo "RESULT: FAIL after ${attempt} attempt(s)"
    exit 1
  fi
  echo "(registry propagation window — retrying in ${INTERVAL_SEC}s)"
  sleep "${INTERVAL_SEC}"
  attempt=$((attempt + 1))
done
