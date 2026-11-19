#!/usr/bin/env node
// Applies Package Admission's rules to the assembled Package Directory, and smoke-launches it.
//
// This is the same verdict the Package Host reaches when it admits the Package, so a build that
// passes here is one Teamarness will accept. It validates against this machine's native target,
// or against `--target <triple>` when that is the native one. The validator's own default target
// is never relied on.

import { spawnSync } from "node:child_process";
import { readFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import { exitCodeFor, failureReason, nativeTarget, parseTargetArgs, selectTarget } from "./targets.mjs";

const projectRoot = dirname(dirname(fileURLToPath(import.meta.url)));

try {
  const manifest = JSON.parse(await readFile(join(projectRoot, "teamarness.package.json"), "utf8"));
  const target = selectTarget({
    requested: parseTargetArgs(process.argv.slice(2)),
    native: nativeTarget({ cwd: projectRoot }),
  });
  const result = spawnSync(
    "cargo",
    [
      "run",
      "--quiet",
      "--locked",
      "-p",
      "teamarness-package-contract",
      "--bin",
      "teamarness-package-validate",
      "--",
      "directory",
      join("dist", manifest.identity),
      "--target",
      target,
      "--smoke",
    ],
    { cwd: projectRoot, stdio: "inherit" },
  );
  // A validator that could not start or was killed has no status: a failure, not a target refusal.
  if (result.error || result.status === null) throw new Error(`validator failed: ${failureReason(result)}`);
  process.exitCode = result.status;
} catch (error) {
  process.stderr.write(`${error.message}\n`);
  process.exitCode = exitCodeFor(error);
}
