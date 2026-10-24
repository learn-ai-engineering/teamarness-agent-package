import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdir, mkdtemp, readFile, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { assemblePackage } from "./assemble-package.mjs";

const identity = "dev.teamarness.fixture";
const zeroDigest = "0".repeat(64);

async function fixtureRoot() {
  const root = await mkdtemp(join(tmpdir(), "teamarness-assembler-test-"));
  await mkdir(join(root, "target/release"), { recursive: true });
  await mkdir(join(root, "ui/dist"), { recursive: true });
  await mkdir(join(root, "src"), { recursive: true });
  // A minimal Cargo project lets the assembler ask Cargo for the target directory.
  await writeFile(join(root, "Cargo.toml"), '[package]\nname = "assembler-fixture"\nversion = "0.1.0"\nedition = "2024"\n\n[workspace]\n');
  await writeFile(join(root, "src/main.rs"), "fn main() {}\n");
  await writeFile(join(root, "teamarness.package.json"), JSON.stringify({
    schemaVersion: 1,
    identity,
    version: "1.0.0",
    presentation: { displayName: "Fixture" },
    protocol: { major: 1, minMinor: 0, maxMinor: 0 },
    backends: [{
      target: "aarch64-apple-darwin",
      path: "backend/teamarness-generated-package-backend-aarch64-apple-darwin",
      size: 0,
      sha256: zeroDigest,
    }],
    ui: {
      entrypoint: { path: "ui/index.js", size: 0, sha256: zeroDigest },
      assets: [],
      contract: { path: "ui/contract.json", sha256: zeroDigest },
    },
    capabilities: [],
  }));
  await writeFile(join(root, "ui-contract.json"), '{"schemaVersion":1,"$defs":{},"requests":{},"subscriptions":{}}');
  await writeFile(join(root, "ui/dist/index.js"), "export const mount = () => () => {};\n");
  return root;
}

const digest = (bytes) => createHash("sha256").update(bytes).digest("hex");

test("assembles exact final Package bytes, declarations, paths, and modes", async () => {
  const root = await fixtureRoot();
  try {
    const backend = Buffer.from("#!/bin/sh\nexit 0\n");
    await writeFile(join(root, "target/release/teamarness-agent-package"), backend);

    const sourceUi = await readFile(join(root, "ui/dist/index.js"));
    const sourceContract = await readFile(join(root, "ui-contract.json"));
    const { CARGO_TARGET_DIR: _ignored, ...env } = process.env;
    const output = await assemblePackage({ projectRoot: root, env });
    assert.equal(output, join(root, "dist", identity));

    const manifestBytes = await readFile(join(output, "teamarness.package.json"));
    const manifest = JSON.parse(manifestBytes);
    const ui = await readFile(join(output, manifest.ui.entrypoint.path));
    const contract = await readFile(join(output, manifest.ui.contract.path));
    assert.equal(manifest.identity, identity);
    assert.equal(manifest.backends[0].path, "backend/teamarness-generated-package-backend-aarch64-apple-darwin");
    assert.deepEqual(await readFile(join(output, manifest.backends[0].path)), backend);
    assert.deepEqual(ui, sourceUi);
    assert.deepEqual(contract, sourceContract);
    assert.equal(manifest.backends[0].size, backend.length);
    assert.equal(manifest.backends[0].sha256, digest(backend));
    assert.equal(manifest.ui.entrypoint.size, ui.length);
    assert.equal(manifest.ui.entrypoint.sha256, digest(ui));
    assert.equal(manifest.ui.contract.sha256, digest(contract));
    for (const value of [manifest.backends[0].sha256, manifest.ui.entrypoint.sha256, manifest.ui.contract.sha256]) {
      assert.match(value, /^[0-9a-f]{64}$/);
    }
    assert.equal((await stat(join(output, manifest.backends[0].path))).mode & 0o777, 0o700);
    for (const path of [manifest.ui.entrypoint.path, manifest.ui.contract.path, "teamarness.package.json"]) {
      assert.equal((await stat(join(output, path))).mode & 0o777, 0o600, path);
    }
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("reads the Backend from a custom CARGO_TARGET_DIR", async () => {
  const root = await fixtureRoot();
  const targetDirectory = await mkdtemp(join(tmpdir(), "teamarness-assembler-target-"));
  try {
    const backend = Buffer.from("#!/bin/sh\necho custom target\n");
    await mkdir(join(targetDirectory, "release"), { recursive: true });
    await writeFile(join(targetDirectory, "release/teamarness-agent-package"), backend);
    await writeFile(join(root, "target/release/teamarness-agent-package"), "stale default target");

    const output = await assemblePackage({ projectRoot: root, env: { ...process.env, CARGO_TARGET_DIR: targetDirectory } });

    const manifest = JSON.parse(await readFile(join(output, "teamarness.package.json")));
    assert.deepEqual(await readFile(join(output, manifest.backends[0].path)), backend);
    assert.equal(manifest.backends[0].sha256, digest(backend));
  } finally {
    await rm(root, { recursive: true, force: true });
    await rm(targetDirectory, { recursive: true, force: true });
  }
});

test("refuses to assemble when a required Backend input is missing", async () => {
  const root = await fixtureRoot();
  try {
    await assert.rejects(assemblePackage({ projectRoot: root }), /ENOENT/);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
