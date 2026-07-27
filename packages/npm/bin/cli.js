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
  console.error(
    `Native package ${packageName} is unavailable. Reinstall @repo-intelligence/cli for ${process.platform}-${process.arch}.`,
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
