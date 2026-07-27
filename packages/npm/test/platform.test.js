import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync, statSync } from "node:fs";
import { fileURLToPath } from "node:url";

import { platformPackage } from "../lib/platform.js";

test("maps supported platforms to public packages owned by the publisher", () => {
  assert.equal(
    platformPackage("darwin", "arm64"),
    "@cainli/repo-intelligence-darwin-arm64",
  );
  assert.equal(
    platformPackage("win32", "x64"),
    "@cainli/repo-intelligence-win32-x64",
  );
  assert.equal(
    platformPackage("linux", "x64"),
    "@cainli/repo-intelligence-linux-x64-gnu",
  );
});

test("rejects unsupported platforms clearly", () => {
  assert.throws(() => platformPackage("aix", "ppc64"), /Unsupported platform/);
});

test("publishes an executable CLI entrypoint", () => {
  const cli = fileURLToPath(new URL("../bin/cli.js", import.meta.url));
  const manifest = JSON.parse(
    readFileSync(fileURLToPath(new URL("../package.json", import.meta.url)), "utf8"),
  );
  assert.equal(manifest.bin["repo-intelligence"], "bin/cli.js");
  if (process.platform !== "win32") {
    assert.notEqual(statSync(cli).mode & 0o111, 0);
  }
});
