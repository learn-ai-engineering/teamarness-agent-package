import assert from "node:assert/strict";
import { chmod, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { nativeTarget, parseHostTarget, parseTargetArgs, selectTarget, supportedTargets } from "./targets.mjs";

const MAC = "aarch64-apple-darwin";
const LINUX = "x86_64-unknown-linux-gnu";

test("supports exactly the two native Backend targets", () => {
  assert.deepEqual([...supportedTargets], [MAC, LINUX]);
  assert.ok(Object.isFrozen(supportedTargets));
});

test("reads the host triple from rustc -vV", () => {
  const output = [
    "rustc 1.100.0 (0ef1bc386 2026-11-09)",
    "binary: rustc",
    "commit-hash: 0ef1bc386",
    `host: ${LINUX}`,
    "release: 1.100.0",
    "",
  ].join("\n");
  assert.equal(parseHostTarget(output), LINUX);
  assert.equal(parseHostTarget(`host: ${MAC}\r\n`), MAC);
});

test("refuses rustc output without exactly one host line", () => {
  assert.throws(() => parseHostTarget("rustc 1.100.0\nrelease: 1.100.0\n"), /target\.native_unknown/);
  assert.throws(() => parseHostTarget(`host: ${MAC}\nhost: ${LINUX}\n`), /target\.native_unknown/);
  assert.throws(() => parseHostTarget("host: \n"), /target\.native_unknown/);
});

test("selects the native target when none is requested", () => {
  assert.equal(selectTarget({ native: MAC }), MAC);
  assert.equal(selectTarget({ native: LINUX }), LINUX);
});

test("accepts a requested target only when it is the native one", () => {
  assert.equal(selectTarget({ requested: LINUX, native: LINUX }), LINUX);
  assert.throws(() => selectTarget({ requested: MAC, native: LINUX }), (error) => {
    assert.equal(error.code, "target.not_native");
    assert.match(error.message, new RegExp(`${MAC}.*${LINUX}`));
    return true;
  });
  assert.throws(() => selectTarget({ requested: LINUX, native: MAC }), { code: "target.not_native" });
});

test("refuses unsupported, malformed, and empty targets", () => {
  for (const native of ["x86_64-apple-darwin", "aarch64-unknown-linux-gnu", "x86_64-unknown-linux-musl"]) {
    assert.throws(() => selectTarget({ native }), { code: "target.unsupported" }, native);
  }
  for (const requested of ["", " ", "x86_64", `${LINUX} `, "X86_64-UNKNOWN-LINUX-GNU", "../x86_64-unknown-linux-gnu"]) {
    assert.throws(() => selectTarget({ requested, native: LINUX }), { code: "target.unsupported" }, JSON.stringify(requested));
  }
});

test("accepts no arguments or exactly --target <triple>", () => {
  assert.equal(parseTargetArgs([]), undefined);
  assert.equal(parseTargetArgs(["--target", LINUX]), LINUX);
});

test("refuses every other argument shape as a usage error", () => {
  for (const argv of [
    ["--target"],
    [`--target=${LINUX}`],
    ["--target", LINUX, "--target", MAC],
    ["--target", LINUX, "extra"],
    [LINUX],
    ["--release"],
    ["--target", ""],
  ]) {
    assert.throws(() => parseTargetArgs(argv), { code: "target.usage" }, JSON.stringify(argv));
  }
});

test("the source manifest declares one placeholder Backend per supported target", async () => {
  const manifest = JSON.parse(await readFile(new URL("../teamarness.package.json", import.meta.url), "utf8"));
  assert.deepEqual(manifest.backends.map((artifact) => artifact.target), [...supportedTargets]);
  for (const artifact of manifest.backends) {
    assert.ok(artifact.path.endsWith(`-${artifact.target}`), artifact.path);
    assert.equal(artifact.size, 0, artifact.target);
    assert.equal(artifact.sha256, "0".repeat(64), artifact.target);
  }
});

test("an unsupported requested target names this machine's target", () => {
  assert.throws(() => selectTarget({ requested: "x86_64", native: LINUX }), (error) => {
    assert.equal(error.code, "target.unsupported");
    assert.match(error.message, new RegExp(`this machine builds ${LINUX}$`));
    return true;
  });
  assert.throws(() => selectTarget({ native: "x86_64-apple-darwin" }), (error) => {
    assert.doesNotMatch(error.message, /this machine builds/);
    return true;
  });
});

test("reads the host triple from $RUSTC run in the given directory", async () => {
  const root = await mkdtemp(join(tmpdir(), "teamarness-native-test-"));
  try {
    const stub = join(root, "fake-rustc");
    // The stub answers with the triple stored in its working directory, so the result proves both
    // that $RUSTC was used and that it ran in `cwd`.
    await writeFile(stub, "#!/bin/sh\nIFS= read -r host < host.txt\nprintf 'rustc 1.0.0\\nhost: %s\\n' \"$host\"\n");
    await chmod(stub, 0o755);
    for (const triple of [MAC, LINUX]) {
      const directory = join(root, triple);
      await mkdir(directory);
      await writeFile(join(directory, "host.txt"), `${triple}\n`);
      assert.equal(nativeTarget({ env: { ...process.env, RUSTC: stub }, cwd: directory }), triple);
    }
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("reports why rustc could not run", () => {
  assert.throws(() => nativeTarget({ env: { ...process.env, RUSTC: "/nonexistent/rustc" } }), (error) => {
    assert.equal(error.code, "target.native_unknown");
    assert.match(error.message, /\/nonexistent\/rustc -vV: .*ENOENT/);
    return true;
  });
});
