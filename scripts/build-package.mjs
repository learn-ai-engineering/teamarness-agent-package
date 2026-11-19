#!/usr/bin/env node
// Builds the Package Directory for this machine's native target.
//
//   npm run build                      # the native target
//   npm run build -- --target <triple> # the same, refusing any other target
//
// The target is checked before Cargo runs or dist/ is touched. Cargo builds with an explicit
// `--target`, so a `CARGO_BUILD_TARGET` or `build.target` setting cannot leave a stale binary
// where the assembler reads.

import { spawnSync } from "node:child_process";
import { dirname } from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import { assemblePackage } from "./assemble-package.mjs";
import { exitCodeFor, nativeTarget, parseTargetArgs, selectTarget } from "./targets.mjs";

const projectRoot = dirname(dirname(fileURLToPath(import.meta.url)));

function run(command, args) {
  const result = spawnSync(command, args, { cwd: projectRoot, stdio: "inherit", shell: false });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    throw new Error(`${command} ${args.join(" ")} failed with status ${result.status ?? "unknown"}`);
  }
}

try {
  const requested = parseTargetArgs(process.argv.slice(2));
  const native = nativeTarget();
  const target = selectTarget({ requested, native });
  process.stdout.write(`Building dev.teamarness.agent for ${target}\n`);
  run("cargo", ["build", "--release", "--locked", "--target", target]);
  run("npm", ["--workspace", "@teamarness/agent-package-ui", "run", "build"]);
  await assemblePackage({ projectRoot, target, native });
  run("node", ["scripts/validate-dist.mjs", "--target", target]);
} catch (error) {
  process.stderr.write(`${error.message}\n`);
  process.exitCode = exitCodeFor(error);
}
