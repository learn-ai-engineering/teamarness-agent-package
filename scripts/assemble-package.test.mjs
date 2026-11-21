import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { chmod, mkdir, mkdtemp, readFile, readdir, rm, stat, writeFile } from "node:fs/promises";
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
// Cargo writes a `--target <triple>` build under `<target-dir>/<triple>/release/`.
const builtBackend = (root, target) => join(root, "target", target, "release/teamarness-agent-package");

async function fixtureRoot({ backends = [placeholder(MAC), placeholder(LINUX)] } = {}) {
  const root = await mkdtemp(join(tmpdir(), "teamarness-assembler-test-"));
  for (const directory of ["target/release", `target/${MAC}/release`, `target/${LINUX}/release`]) {
    await mkdir(join(root, directory), { recursive: true });
  }
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
  await mkdir(join(root, "headless"), { recursive: true });
  await writeFile(join(root, "headless/agent-setup"), "#!/usr/bin/env bash\necho setup\n", { mode: 0o644 });
  return root;
}

const digest = (bytes) => createHash("sha256").update(bytes).digest("hex");
const withoutCargoTarget = () => {
  const { CARGO_TARGET_DIR: _ignored, ...env } = process.env;
  return env;
};

/** A previous build's output, shaped like a real Package Directory, so a refusal can be shown to leave it byte-identical. */
async function previousOutput(root) {
  const output = join(root, "dist", identity);
  await mkdir(join(output, "backend"), { recursive: true, mode: 0o700 });
  await mkdir(join(output, "ui"), { recursive: true, mode: 0o700 });
  const files = [
    [backendPath(MAC), Buffer.from([0x7f, 0x45, 0x4c, 0x46, 0x00, 0xff, 0xfe, 0x0a]), 0o700],
    ["ui/index.js", Buffer.from("export const mount = () => () => {};\n"), 0o600],
    ["teamarness.package.json", Buffer.from("{\"previous\":true}\n"), 0o600],
  ];
  for (const [path, bytes, mode] of files) {
    await writeFile(join(output, path), bytes, { mode });
    await chmod(join(output, path), mode);
  }
  return output;
}

/** Every entry under `directory`: its kind, mode, and, for a file, its exact bytes. */
async function snapshot(directory) {
  const entries = {};
  for (const name of (await readdir(directory, { recursive: true })).sort()) {
    const path = join(directory, name);
    const info = await stat(path);
    entries[name] = {
      kind: info.isFile() ? "file" : info.isDirectory() ? "directory" : "other",
      mode: info.mode & 0o777,
      bytes: info.isFile() ? (await readFile(path)).toString("base64") : null,
    };
  }
  return entries;
}

for (const target of [MAC, LINUX]) {
  test(`assembles exact final Package bytes, declarations, paths, and modes for ${target}`, async () => {
    const root = await fixtureRoot();
    try {
      const backend = Buffer.from(`#!/bin/sh\necho ${target}\n`);
      await writeFile(builtBackend(root, target), backend);
      // A build without `--target` would have landed here; it must never be packaged.
      await writeFile(join(root, "target/release/teamarness-agent-package"), "stale untargeted build");

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
      // The setup script ships beside the Package Directory, never inside it.
      const setup = join(root, "dist", "agent-setup");
      assert.deepEqual(await readFile(setup), await readFile(join(root, "headless/agent-setup")));
      assert.equal((await stat(setup)).mode & 0o777, 0o755);
      assert.deepEqual((await readdir(output)).sort(), ["backend", "teamarness.package.json", "ui"]);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });
}

test("reads the targeted Backend from a custom CARGO_TARGET_DIR", async () => {
  const root = await fixtureRoot();
  const targetDirectory = await mkdtemp(join(tmpdir(), "teamarness-assembler-target-"));
  try {
    const backend = Buffer.from("#!/bin/sh\necho custom target\n");
    await mkdir(join(targetDirectory, MAC, "release"), { recursive: true });
    await mkdir(join(targetDirectory, "release"), { recursive: true });
    await writeFile(join(targetDirectory, MAC, "release/teamarness-agent-package"), backend);
    await writeFile(join(targetDirectory, "release/teamarness-agent-package"), "stale untargeted build");
    await writeFile(builtBackend(root, MAC), "stale default target directory");

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

test("refuses when the targeted Backend is missing, even beside an untargeted build, and leaves the previous output unchanged", async () => {
  const root = await fixtureRoot();
  try {
    await writeFile(join(root, "target/release/teamarness-agent-package"), "stale untargeted build");
    const output = await previousOutput(root);
    const before = await snapshot(output);
    await assert.rejects(assemblePackage({ projectRoot: root, env: withoutCargoTarget(), target: MAC, native: MAC }), /ENOENT/);
    assert.deepEqual(await snapshot(output), before);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("reports why cargo metadata could not run, and leaves the previous output unchanged", async () => {
  const root = await fixtureRoot();
  try {
    await writeFile(builtBackend(root, MAC), "#!/bin/sh\nexit 0\n");
    const output = await previousOutput(root);
    const before = await snapshot(output);
    const env = { ...withoutCargoTarget(), PATH: "/nonexistent" };
    await assert.rejects(assemblePackage({ projectRoot: root, env, target: MAC, native: MAC }), /^Error: cargo metadata failed: .*ENOENT/);
    assert.deepEqual(await snapshot(output), before);
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
    code: "target.declared_twice",
  },
];

for (const { name, fixture, options, code } of refusals) {
  test(`refuses ${name} and leaves the previous output unchanged`, async () => {
    const root = await fixtureRoot(fixture);
    try {
      for (const target of [MAC, LINUX]) await writeFile(builtBackend(root, target), "#!/bin/sh\nexit 0\n");
      const output = await previousOutput(root);
      const before = await snapshot(output);
      await assert.rejects(assemblePackage({ projectRoot: root, env: withoutCargoTarget(), ...options }), { code });
      assert.deepEqual(await snapshot(output), before);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });
}
