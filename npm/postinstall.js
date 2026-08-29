const fs = require("fs");
const path = require("path");

const binaryName = process.platform === "win32" ? "vorpal.exe" : "vorpal";
const alternativeName = process.platform === "win32" ? "vp.exe" : "vp";

function detectPackageName() {
  const { platform, arch } = process;
  switch (platform) {
    case "darwin":
      if (arch === "arm64") return "@hyper-light/vorpal-cli-darwin-arm64";
      if (arch === "x64") return "@hyper-light/vorpal-cli-darwin-x64";
      break;
    case "linux": {
      const { MUSL, familySync } = require("detect-libc");
      const libc = familySync() === MUSL ? "musl" : "gnu";
      if (arch === "arm64") return `@hyper-light/vorpal-cli-linux-arm64-${libc}`;
      if (arch === "x64") return `@hyper-light/vorpal-cli-linux-x64-${libc}`;
      break;
    }
    case "win32":
      if (arch === "arm64") return "@hyper-light/vorpal-cli-win32-arm64-msvc";
      if (arch === "ia32") return "@hyper-light/vorpal-cli-win32-ia32-msvc";
      if (arch === "x64") return "@hyper-light/vorpal-cli-win32-x64-msvc";
      break;
  }
  return null;
}

function resolveBinaryDir() {
  const pkgName = detectPackageName();
  if (pkgName) {
    try {
      const dir = path.dirname(
        require.resolve(`${pkgName}/package.json`, { paths: [__dirname] }),
      );
      if (fs.existsSync(path.join(dir, binaryName))) return dir;
    } catch (_) {
      // fall through to local dev paths
    }
  }
  for (const profile of ["release", "debug"]) {
    const dir = path.join(__dirname, "..", "target", profile);
    if (fs.existsSync(path.join(dir, binaryName))) return dir;
  }
  return null;
}

function resolveBinaryPath() {
  const dir = resolveBinaryDir();
  return dir ? path.join(dir, binaryName) : null;
}

function main() {
  const sourceDir = resolveBinaryDir();
  if (!sourceDir) {
    console.error("Failed to locate @hyper-light/vorpal-cli native binary.");
    process.exit(1);
  }

  const src = path.join(sourceDir, binaryName);
  const destBin = path.join(__dirname, binaryName);
  const destAlt = path.join(__dirname, alternativeName);

  try {
    fs.linkSync(src, destBin);
    fs.linkSync(src, destAlt);
  } catch (_) {
    try {
      fs.copyFileSync(src, destBin);
      fs.copyFileSync(src, destAlt);
    } catch (err) {
      console.error("Failed to move @hyper-light/vorpal-cli binary into place.");
      process.exit(1);
    }
  }

  // Keep the extensionless JS shims on Windows: npm-generated global wrappers
  // call those bin targets through node, so removing them breaks global installs.
}

module.exports = {
  binaryName,
  alternativeName,
  detectPackageName,
  resolveBinaryDir,
  resolveBinaryPath,
};

if (require.main === module) {
  main();
}
