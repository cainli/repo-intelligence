export function platformPackage(platform, arch) {
  const packages = {
    "darwin-arm64": "@repo-intelligence/darwin-arm64",
    "darwin-x64": "@repo-intelligence/darwin-x64",
    "linux-arm64": "@repo-intelligence/linux-arm64-gnu",
    "linux-x64": "@repo-intelligence/linux-x64-gnu",
    "win32-x64": "@repo-intelligence/win32-x64",
  };
  const name = packages[`${platform}-${arch}`];
  if (!name) {
    throw new Error(`Unsupported platform: ${platform}-${arch}`);
  }
  return name;
}

export function executableName(platform) {
  return platform === "win32" ? "repo-intelligence.exe" : "repo-intelligence";
}

