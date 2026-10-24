// The digest of one vendored SDK tree, computed the same way in this checkout and in a generated
// project, so a project can tell whether its copy still matches what it was generated from.
//
// Node builtins only: this file is copied into generated projects as `scripts/vendor-digest.mjs`.

import { createHash } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import { join } from "node:path";

/** Every vendored tree a generated project carries, in the order a report lists them. */
export const VENDOR_TREES = Object.freeze([
  "package-contract",
  "package-protocol",
  "package-sdk",
  "package-ui-sdk",
]);

const SKIPPED_DIRECTORIES = new Set(["node_modules", "target"]);

async function relativeFiles(directory, prefix = "") {
  const entries = await readdir(directory, { withFileTypes: true });
  const files = [];
  for (const entry of entries) {
    const relative = prefix === "" ? entry.name : `${prefix}/${entry.name}`;
    if (entry.isDirectory()) {
      if (SKIPPED_DIRECTORIES.has(entry.name)) continue;
      files.push(...await relativeFiles(join(directory, entry.name), relative));
    } else if (entry.isFile()) {
      files.push(relative);
    }
  }
  return files;
}

/**
 * The SHA-256 of every regular file in `directory`, by path and content.
 *
 * @param {string} directory a vendored package directory
 * @returns {Promise<string>} `sha256:<hex>`
 */
export async function vendorTreeDigest(directory) {
  const files = (await relativeFiles(directory)).sort();
  const tree = createHash("sha256");
  for (const relative of files) {
    const digest = createHash("sha256").update(await readFile(join(directory, relative))).digest("hex");
    tree.update(`${relative}\0${digest}\n`);
  }
  return `sha256:${tree.digest("hex")}`;
}
