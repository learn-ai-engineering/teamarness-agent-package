#!/usr/bin/env node
// Fails when a vendored SDK file has been edited.
//
// The vendored trees are a copy of one Teamarness checkout, recorded in `teamarness-authoring.json`
// by digest. Editing one is how a project silently stops matching the SDK it says it uses, so the
// project's own tests check it.

import { readFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import { VENDOR_TREES, vendorTreeDigest } from "./vendor-digest.mjs";

const projectRoot = dirname(dirname(fileURLToPath(import.meta.url)));

const provenance = JSON.parse(await readFile(join(projectRoot, "teamarness-authoring.json"), "utf8"));
const recorded = provenance.vendorDigests ?? {};

let matched = true;
for (const tree of VENDOR_TREES) {
  const actual = await vendorTreeDigest(join(projectRoot, "vendor/teamarness", tree));
  if (recorded[tree] !== actual) {
    process.stderr.write(`vendored ${tree} differs from teamarness-authoring.json\n`);
    matched = false;
  }
}

if (matched) {
  process.stdout.write("vendored SDK trees match provenance\n");
} else {
  process.stderr.write("A vendored SDK file was edited. Restore it, or generate a new project.\n");
  process.exitCode = 1;
}
