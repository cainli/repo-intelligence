#!/usr/bin/env node
import { spawn } from "node:child_process";
import { createRequire } from "node:module";
import { existsSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { executableName, platformPackage } from "../lib/platform.js";

const require = createRequire(import.meta.url);
const packageName = platformPackage(process.platform, process.arch);
const executable = executableName(process.platform);
let binary;

try {
  const manifest = require.resolve(`${packageName}/package.json`);
  binary = resolve(dirname(manifest), "bin", executable);
} catch {
  const repositoryRoot = resolve(dirname(fileURLToPath(import.meta.url)), "../../..");
  const candidates = [
    resolve(repositoryRoot, "target", "release", executable),
    resolve(repositoryRoot, "target", "debug", executable),
  ];
  binary = candidates.find(existsSync);
}

if (!binary) {
  // 主包 optionalDependencies 钉死的期望版本——报错带版本号,让用户能区分
  // 「没装上」(本地/镜像问题)与「没发布」(registry 查无此版本),后者也是
  // 私源镜像同步延迟的典型表现(第三轮反馈 P0-B)。
  let expectedVersion = "unknown";
  try {
    const manifest = require("../package.json");
    expectedVersion = manifest.optionalDependencies?.[packageName] ?? "unknown";
  } catch {
    // 读不到 manifest 时退回 unknown,不阻塞报错
  }
  console.error(
    `Native package ${packageName}@${expectedVersion} is unavailable for ${process.platform}-${process.arch}.

Common causes (platform binaries ship as optionalDependencies):
  - installed with --no-optional / --omit=optional
  - registry mirror lag: the version exists on npmjs.com but your configured
    registry may not have synced it yet. Check:
      npm config get registry
      npm view ${packageName}@${expectedVersion} version
    (fails only on your mirror -> trigger a mirror sync or switch registry)
  - the platform package was not published for this release

Fix:
  npm install -g @cainli/repo-intelligence --force`,
  );
  process.exit(1);
}

const child = spawn(binary, process.argv.slice(2), { stdio: "inherit" });
for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => child.kill(signal));
}
child.on("error", (error) => {
  console.error(error.message);
  process.exit(1);
});
child.on("exit", (code, signal) => {
  if (signal) process.kill(process.pid, signal);
  else process.exit(code ?? 1);
});
