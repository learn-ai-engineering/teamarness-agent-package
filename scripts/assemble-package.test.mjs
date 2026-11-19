import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdir, mkdtemp, readFile, readdir, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { assemblePackage } from "./assemble-package.mjs";

const identity = "dev.teamarness.fixture";
const zeroDigest = "0".repeat(64);
const MAC = "aarch64-apple-darwin";
const LINUX = "x86_64-unknown-linux-gnu";
const backendPath = (target) => `backend/teamarness-generated-package-backend-${target}`;
const placeholder = (target) => ({ target, path: backendPath(target), size: 0, sha256: zeroDigest });

async function fixtureRoot({ backends = [placeholder(MAC), placeholder(LINUX)] } = {}) {
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
    backends,
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
const withoutCargoTarget = () => {
  const { CARGO_TARGET_DIR: _ignored, ...env } = process.env;
  return env;
};

/// A previous build's output, so a refusal can be shown to leave it byte-identical.
async function previousOutput(root) {
  const output = join(root, "dist", identity);
  await mkdir(output, { recursive: true });
  await writeFile(join(output, "teamarness.package.json"), "previous manifest\n");
  return output;
}

async function snapshot(directory) {
  const entries = {};
  for (const name of (await readdir(directory, { recursive: true })).sort()) {
    const path = join(directory, name);
    if ((await stat(path)).isFile()) entries[name] = await readFile(path, "utf8");
  }
  return entries;
}

for (const target of [MAC, LINUX]) {
  test(`assembles exact final Package bytes, declarations, paths, and modes for ${target}`, async () => {
    const root = await fixtureRoot();
    try {
      const backend = Buffer.from(`#!/bin/sh\necho ${target}\n`);
      await writeFile(join(root, "target/release/teamarness-agent-package"), backend);

      const sourceUi = await readFile(join(root, "ui/dist/index.js"));
      const sourceContract = await readFile(join(root, "ui-contract.json"));
      const output = await assemblePackage({ projectRoot: root, env: withoutCargoTarget(), target, native: target });
      assert.equal(output, join(root, "dist", identity));

      const manifest = JSON.parse(await readFile(join(output, "teamarness.package.json")));
      const ui = await readFile(join(output, manifest.ui.entrypoint.path));
      const contract = await readFile(join(output, manifest.ui.contract.path));
      assert.equal(manifest.identity, identity);
      assert.equal(manifest.backends.length, 1, "only the built target is declared");
      assert.equal(manifest.backends[0].target, target);
      assert.equal(manifest.backends[0].path, backendPath(target));
      assert.deepEqual(await readdir(join(output, "backend")), [`teamarness-generated-package-backend-${target}`]);
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
}

test("reads the Backend from a custom CARGO_TARGET_DIR", async () => {
  const root = await fixtureRoot();
  const targetDirectory = await mkdtemp(join(tmpdir(), "teamarness-assembler-target-"));
  try {
    const backend = Buffer.from("#!/bin/sh\necho custom target\n");
    await mkdir(join(targetDirectory, "release"), { recursive: true });
    await writeFile(join(targetDirectory, "release/teamarness-agent-package"), backend);
    await writeFile(join(root, "target/release/teamarness-agent-package"), "stale default target");

    const output = await assemblePackage({
      projectRoot: root,
      env: { ...process.env, CARGO_TARGET_DIR: targetDirectory },
      target: MAC,
      native: MAC,
    });

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
    await assert.rejects(assemblePackage({ projectRoot: root, env: withoutCargoTarget(), target: MAC, native: MAC }), /ENOENT/);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

const refusals = [
  { name: "a requested target that is not native", options: { target: MAC, native: LINUX }, code: "target.not_native" },
  { name: "an unsupported native target", options: { native: "x86_64-apple-darwin" }, code: "target.unsupported" },
  {
    name: "a source manifest without the selected target",
    fixture: { backends: [placeholder(MAC)] },
    options: { target: LINUX, native: LINUX },
    code: "target.not_declared",
  },
  {
    name: "a source manifest that declares the selected target twice",
    fixture: { backends: [placeholder(LINUX), { ...placeholder(LINUX), path: "backend/other" }] },
    options: { target: LINUX, native: LINUX },
    code: "target.not_declared",
  },
];

for (const { name, fixture, options, code } of refusals) {
  test(`refuses ${name} and leaves the previous output unchanged`, async () => {
    const root = await fixtureRoot(fixture);
    try {
      await writeFile(join(root, "target/release/teamarness-agent-package"), "#!/bin/sh\nexit 0\n");
      const output = await previousOutput(root);
      const before = await snapshot(output);
      await assert.rejects(assemblePackage({ projectRoot: root, env: withoutCargoTarget(), ...options }), { code });
      assert.deepEqual(await snapshot(output), before);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });
}
