#!/usr/bin/env node
// Fails when a bundled Provider contract no longer matches its recorded provenance.
//
// `backend/catalog/*.ports.json` are verbatim copies of Provider `ports.json` files, recorded in
// `backend/catalog/provenance.json` by digest. The Backend compiles them in, so an edited copy
// would silently describe a contract no Provider offers. Refresh them with
// `node scripts/refresh-catalog.mjs --from <teamarness-checkout>` instead of editing them.

import { createHash } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const projectRoot = dirname(dirname(fileURLToPath(import.meta.url)));
const catalogDirectory = join(projectRoot, "backend/catalog");

const provenance = JSON.parse(await readFile(join(catalogDirectory, "provenance.json"), "utf8"));
const recorded = provenance.files ?? {};
const present = (await readdir(catalogDirectory)).filter((name) => name.endsWith(".ports.json"));

let matched = Object.keys(recorded).length > 0;
for (const name of new Set([...Object.keys(recorded), ...present])) {
  if (!present.includes(name)) {
    process.stderr.write(`catalog ${name} is recorded but missing\n`);
    matched = false;
    continue;
  }
  const bytes = await readFile(join(catalogDirectory, name));
  const actual = `sha256:${createHash("sha256").update(bytes).digest("hex")}`;
  if (recorded[name] !== actual) {
    process.stderr.write(`catalog ${name} differs from provenance.json\n`);
    matched = false;
  }
}

if (matched) {
  process.stdout.write(`bundled catalog matches provenance (${provenance.teamarnessRevision})\n`);
} else {
  process.stderr.write("A bundled catalog file was edited. Refresh it with scripts/refresh-catalog.mjs.\n");
  process.exitCode = 1;
}
