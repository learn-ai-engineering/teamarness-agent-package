#!/usr/bin/env node
// Controlled migration to the approved canonical snapshot, not an in-place SDK updater.
import { execFileSync } from "node:child_process";
import { cp, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const revision = "4e0249b7e354f87586971aa4d8951613151906eb";
const source = resolve(process.argv[2] ?? "");
const git = (...args) => execFileSync("git", ["-C", source, ...args], { encoding: "utf8" }).trim();
if (git("rev-parse", "HEAD") !== revision || git("status", "--porcelain") !== "") {
  throw new Error(`SDK refresh requires clean canonical Host ${revision}`);
}
const root = dirname(dirname(fileURLToPath(import.meta.url)));
const temporary = await mkdtemp(join(tmpdir(), "agent-sdk-refresh-"));
try {
  const { createPackageProject } = await import(pathToFileURL(join(source, "scripts/create-package.mjs")));
  const generated = join(temporary, "generated");
  await createPackageProject({ output: generated, identity: "dev.teamarness.agent", displayName: "Agent" });
  for (const path of ["vendor/teamarness", "scripts/vendor-digest.mjs"]) {
    await rm(join(root, path), { recursive: true, force: true });
    await cp(join(generated, path), join(root, path), { recursive: true });
  }
  const provenance = JSON.parse(await readFile(join(generated, "teamarness-authoring.json"), "utf8"));
  await writeFile(join(root, "teamarness-authoring.json"), `${JSON.stringify(provenance, null, 2)}\n`);
} finally {
  await rm(temporary, { recursive: true, force: true });
}
