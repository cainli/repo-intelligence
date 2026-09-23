#!/usr/bin/env bash
# 发布后校验(P0-B,第三轮反馈):npm publish 五包后必须跑,当场暴露「半截发版」。
#   1. 五包(主包 + 4 平台包)的当前版本必须已存在于 registry;
#   2. 干净目录 npm install 冒烟 + --version 执行(optionalDependencies 缺失在
#      安装阶段被 npm 静默容忍,延迟到运行时才炸——冒烟把它提前到发布当下)。
# 用法:./scripts/release_verify.sh [--registry <url>]   # 默认官方 registry
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="$(node -p "require('${ROOT}/packages/npm/package.json').version")"
MAIN="@cainli/repo-intelligence"
REGISTRY_ARGS=()
if [[ "${1:-}" == "--registry" && -n "${2:-}" ]]; then
  REGISTRY_ARGS=(--registry "$2")
fi

echo "== release verify: ${MAIN}@${VERSION} =="

fail=0
# 主包 + 平台包逐一校验 registry 存在性
for pkg in "${MAIN}" \
  "${MAIN}-darwin-arm64" \
  "${MAIN}-win32-x64" \
  "${MAIN}-linux-x64-gnu" \
  "${MAIN}-linux-arm64-gnu"; do
  if npm view "${pkg}@${VERSION}" version ${REGISTRY_ARGS[@]+"${REGISTRY_ARGS[@]}"} >/dev/null 2>&1; then
    echo "  ok    ${pkg}@${VERSION}"
  else
    echo "  MISS  ${pkg}@${VERSION}  <-- not on registry (half release)"
    fail=1
  fi
done

# 干净目录安装冒烟:平台二进制缺失会当场炸
if [[ $fail -eq 0 ]]; then
  TMP="$(mktemp -d)"
  trap 'rm -rf "${TMP}"' EXIT
  echo "== smoke install into ${TMP} =="
  if npm install --prefix "${TMP}" ${REGISTRY_ARGS[@]+"${REGISTRY_ARGS[@]}"} "${MAIN}@${VERSION}" >/dev/null 2>&1 \
    && "${TMP}/node_modules/.bin/repo-intelligence" --version >/dev/null 2>&1; then
    echo "  ok    npm install + --version"
  else
    echo "  FAIL  npm install smoke (platform binary missing or broken)"
    echo "       rerun manually to inspect:"
    echo "         npm install --prefix ${TMP} ${REGISTRY_ARGS[@]+"${REGISTRY_ARGS[@]}"} ${MAIN}@${VERSION}"
    echo "         ${TMP}/node_modules/.bin/repo-intelligence --version"
    fail=1
  fi
fi

if [[ $fail -ne 0 ]]; then
  echo "RESULT: FAIL"
  exit 1
fi
echo "RESULT: PASS"
