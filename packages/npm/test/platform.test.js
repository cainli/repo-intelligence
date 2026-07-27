import test from "node:test";
import assert from "node:assert/strict";

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
