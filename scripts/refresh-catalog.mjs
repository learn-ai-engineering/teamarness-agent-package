#!/usr/bin/env node
// Refreshes the bundled Provider contract snapshot in `backend/catalog/` from a Teamarness checkout.
//
// Usage: node scripts/refresh-catalog.mjs --from <teamarness-checkout>
//
// Each Provider's `ports.json` is copied byte for byte, and `provenance.json` records the
// checkout's revision, whether it had local changes, and the SHA-256 of every copied file.
// `npm run check:catalog` recomputes those digests.

import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { readFile, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

/** The Providers whose contracts the Agent bundles, by catalog name. */
const PROVIDERS = Object.freeze(["conversations", "documents", "exhibits"]);

const projectRoot = dirname(dirname(fileURLToPath(import.meta.url)));
const catalogDirectory = join(projectRoot, "backend/catalog");

const fromIndex = process.argv.indexOf("--from");
const from = fromIndex === -1 ? undefined : process.argv[fromIndex + 1];
if (from === undefined || from === "") {
  process.stderr.write("usage: node scripts/refresh-catalog.mjs --from <teamarness-checkout>\n");
  process.exit(2);
}
const checkout = resolve(from);

function git(...args) {
  return execFileSync("git", ["-C", checkout, ...args], { encoding: "utf8" });
}

const files = {};
for (const provider of PROVIDERS) {
  const bytes = await readFile(join(checkout, "packages", `${provider}-package`, "ports.json"));
  const name = `${provider}.ports.json`;
  await writeFile(join(catalogDirectory, name), bytes);
  files[name] = `sha256:${createHash("sha256").update(bytes).digest("hex")}`;
}

const provenance = {
  teamarnessRevision: git("rev-parse", "HEAD").trim(),
  teamarnessRevisionDirty: git("status", "--porcelain").trim() !== "",
  files,
};
await writeFile(join(catalogDirectory, "provenance.json"), `${JSON.stringify(provenance, null, 2)}\n`);
process.stdout.write(`catalog refreshed from ${provenance.teamarnessRevision}\n`);
