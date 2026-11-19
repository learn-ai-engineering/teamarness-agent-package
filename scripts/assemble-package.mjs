import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { chmod, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { TargetError, nativeTarget, selectTarget } from "./targets.mjs";

const scriptPath = fileURLToPath(import.meta.url);
const root = dirname(dirname(scriptPath));

/// Assembles the Package Directory for one native target.
///
/// The source manifest declares one placeholder Backend per supported target. The assembled
/// manifest declares only the selected target's Backend, so it never claims an artifact it does
/// not ship. Every refusal happens before the previous output is removed.
export async function assemblePackage({ projectRoot = root, env = process.env, target, native } = {}) {
  const selected = selectTarget({ requested: target, native: native ?? nativeTarget({ env }) });
  const manifestFile = join(projectRoot, "teamarness.package.json");
  const manifest = JSON.parse(await readFile(manifestFile, "utf8"));
  const declared = manifest.backends.filter((artifact) => artifact.target === selected);
  if (declared.length !== 1) {
    throw new TargetError(
      "target.not_declared",
      `teamarness.package.json must declare exactly one Backend for ${selected}, found ${declared.length}`,
    );
  }
  manifest.backends = declared;
  const backendSource = join(cargoTargetDirectory(projectRoot, env), "release/teamarness-agent-package");
  const uiSource = join(projectRoot, "ui/dist/index.js");
  const contractSource = join(projectRoot, "ui-contract.json");
  const destination = join(projectRoot, "dist", manifest.identity);
  const backendPath = manifest.backends[0].path;
  const backend = await readFile(backendSource);
  const ui = await readFile(uiSource);
  const contract = await readFile(contractSource);
  manifest.backends[0].size = backend.length;
  manifest.backends[0].sha256 = digest(backend);
  manifest.ui.entrypoint.size = ui.length;
  manifest.ui.entrypoint.sha256 = digest(ui);
  manifest.ui.contract.sha256 = digest(contract);

  await rm(destination, { recursive: true, force: true });
  await mkdir(join(destination, "backend"), { recursive: true, mode: 0o700 });
  await mkdir(join(destination, "ui"), { recursive: true, mode: 0o700 });
  await writeFile(join(destination, backendPath), backend, { mode: 0o700 });
  await chmod(join(destination, backendPath), 0o700);
  await writeFile(join(destination, manifest.ui.entrypoint.path), ui, { mode: 0o600 });
  await writeFile(join(destination, manifest.ui.contract.path), contract, { mode: 0o600 });
  await writeFile(join(destination, "teamarness.package.json"), `${JSON.stringify(manifest, null, 2)}\n`, { mode: 0o600 });
  return destination;
}

// Ask Cargo so CARGO_TARGET_DIR and build.target-dir are honoured.
function cargoTargetDirectory(projectRoot, env) {
  const result = spawnSync("cargo", ["metadata", "--format-version", "1", "--no-deps"], {
    cwd: projectRoot,
    env,
    encoding: "utf8",
    maxBuffer: 16 * 1024 * 1024,
    shell: false,
  });
  if (result.status !== 0) throw new Error(`cargo metadata failed with status ${result.status ?? "unknown"}`);
  return JSON.parse(result.stdout).target_directory;
}

function digest(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

if (process.argv[1] === scriptPath) {
  const flag = process.argv.indexOf("--target");
  await assemblePackage({ target: flag === -1 ? undefined : process.argv[flag + 1] });
}
