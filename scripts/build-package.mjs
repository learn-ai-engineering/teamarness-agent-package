#!/usr/bin/env node
// Builds the Package Directory for this machine's native target.
//
//   npm run build                      # the native target
//   npm run build -- --target <triple> # the same, refusing any other target
//
// The target is checked before Cargo runs or dist/ is touched.

import { spawnSync } from "node:child_process";
import { dirname } from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import { assemblePackage } from "./assemble-package.mjs";
import { nativeTarget, selectTarget } from "./targets.mjs";

const projectRoot = dirname(dirname(fileURLToPath(import.meta.url)));

function requestedTarget(argv) {
  if (argv.length === 0) return undefined;
  if (argv.length === 2 && argv[0] === "--target") return argv[1];
  throw new Error(`usage: npm run build [-- --target <triple>]; got ${JSON.stringify(argv)}`);
}

function run(command, args) {
  const result = spawnSync(command, args, { cwd: projectRoot, stdio: "inherit", shell: false });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    throw new Error(`${command} ${args.join(" ")} failed with status ${result.status ?? "unknown"}`);
  }
}

try {
  const native = nativeTarget();
  const target = selectTarget({ requested: requestedTarget(process.argv.slice(2)), native });
  process.stdout.write(`Building dev.teamarness.agent for ${target}\n`);
  run("cargo", ["build", "--release", "--locked"]);
  run("npm", ["--workspace", "@teamarness/agent-package-ui", "run", "build"]);
  await assemblePackage({ projectRoot, target, native });
  run("node", ["scripts/validate-dist.mjs", "--target", target]);
} catch (error) {
  process.stderr.write(`${error.message}\n`);
  process.exitCode = error.code?.startsWith("target.") ? 2 : 1;
}
