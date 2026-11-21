import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { chmod, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { TargetError, failureReason, nativeTarget, selectTarget } from "./targets.mjs";

const root = dirname(dirname(fileURLToPath(import.meta.url)));

/**
 * Assembles the Package Directory for one native target. `scripts/build-package.mjs` and
 * `scripts/assemble-package.test.mjs` import it; it has no command line of its own.
 *
 * The source manifest declares one placeholder Backend per supported target. The assembled
 * manifest declares only the selected target's Backend, so it never claims an artifact it does
 * not ship. The Backend is read from Cargo's `--target <triple>` output, never from an untargeted
 * build that may be stale. Every refusal happens before the previous output is removed.
 *
 * It also copies `headless/agent-setup` to `dist/agent-setup` (0755), beside the Package
 * Directory and outside it, so one copy of `dist/` carries both. Admission reads only
 * `dist/<identity>/`, so the script is not part of the Package's declared bytes.
 */
export async function assemblePackage({ projectRoot = root, env = process.env, target, native } = {}) {
  const selected = selectTarget({ requested: target, native: native ?? nativeTarget({ env, cwd: projectRoot }) });
  const manifestFile = join(projectRoot, "teamarness.package.json");
  const manifest = JSON.parse(await readFile(manifestFile, "utf8"));
  const declared = manifest.backends.filter((artifact) => artifact.target === selected);
  if (declared.length === 0) {
    throw new TargetError("target.not_declared", `teamarness.package.json declares no Backend for ${selected}`);
  }
  if (declared.length > 1) {
    throw new TargetError(
      "target.declared_twice",
      `teamarness.package.json declares ${declared.length} Backends for ${selected}; it must declare exactly one`,
    );
  }
  manifest.backends = declared;
  const backendSource = join(cargoTargetDirectory(projectRoot, env), selected, "release/teamarness-agent-package");
  const uiSource = join(projectRoot, "ui/dist/index.js");
  const contractSource = join(projectRoot, "ui-contract.json");
  const destination = join(projectRoot, "dist", manifest.identity);
  const backendPath = manifest.backends[0].path;
  const backend = await readFile(backendSource);
  const ui = await readFile(uiSource);
  const contract = await readFile(contractSource);
  const setup = await readFile(join(projectRoot, "headless/agent-setup"));
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
  const setupDestination = join(projectRoot, "dist", "agent-setup");
  await rm(setupDestination, { force: true });
  await writeFile(setupDestination, setup, { mode: 0o755 });
  await chmod(setupDestination, 0o755);
  return destination;
}

/** Asks Cargo, so `CARGO_TARGET_DIR` and `build.target-dir` are honoured. */
function cargoTargetDirectory(projectRoot, env) {
  const result = spawnSync("cargo", ["metadata", "--format-version", "1", "--no-deps"], {
    cwd: projectRoot,
    env,
    encoding: "utf8",
    maxBuffer: 16 * 1024 * 1024,
    shell: false,
  });
  if (result.error || result.status !== 0) throw new Error(`cargo metadata failed: ${failureReason(result)}`);
  return JSON.parse(result.stdout).target_directory;
}

function digest(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}
