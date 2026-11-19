import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { chmod, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { delimiter, dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const script = join(dirname(fileURLToPath(import.meta.url)), "build-package.mjs");

/**
 * Runs `build-package.mjs` with stub `rustc` and `cargo` first on PATH. The stub `rustc` reports
 * `host`; the stub `cargo` and `npm` record that they were called. A refusal must happen before
 * either runs.
 */
async function build(host, argv) {
  const stubs = await mkdtemp(join(tmpdir(), "teamarness-build-test-"));
  const calls = join(stubs, "calls.log");
  try {
    await writeFile(join(stubs, "rustc"), `#!/bin/sh\nprintf 'rustc 1.0.0\\nhost: ${host}\\nrelease: 1.0.0\\n'\n`);
    for (const tool of ["cargo", "npm"]) {
      await writeFile(join(stubs, tool), `#!/bin/sh\necho ${tool} "$@" >> "${calls}"\nexit 97\n`);
    }
    for (const tool of ["rustc", "cargo", "npm"]) await chmod(join(stubs, tool), 0o755);
    const { RUSTC: _rustc, ...env } = process.env;
    const result = spawnSync(process.execPath, [script, ...argv], {
      env: { ...env, PATH: `${stubs}${delimiter}${process.env.PATH}` },
      encoding: "utf8",
    });
    const called = await readFile(calls, "utf8").catch(() => "");
    return { status: result.status, stderr: result.stderr, called };
  } finally {
    await rm(stubs, { recursive: true, force: true });
  }
}

for (const { name, host, argv, code } of [
  { name: "a requested target that is not native", host: "x86_64-unknown-linux-gnu", argv: ["--target", "aarch64-apple-darwin"], code: "target.not_native" },
  { name: "an unsupported native target", host: "x86_64-apple-darwin", argv: [], code: "target.unsupported" },
  { name: "a malformed argument list", host: "aarch64-apple-darwin", argv: ["--target"], code: "target.usage" },
  { name: "an unknown argument", host: "aarch64-apple-darwin", argv: ["--release"], code: "target.usage" },
]) {
  test(`refuses ${name} with exit 2 before Cargo or npm runs`, async () => {
    const { status, stderr, called } = await build(host, argv);
    assert.equal(status, 2, stderr);
    assert.match(stderr, new RegExp(`^${code.replace(".", "\\.")}: `));
    assert.equal(called, "", "no build tool ran");
  });
}

test("runs Cargo for exactly the selected native target", async () => {
  const { status, called } = await build("x86_64-unknown-linux-gnu", ["--target", "x86_64-unknown-linux-gnu"]);
  assert.equal(status, 1, "the stub cargo fails, which ends the build");
  assert.equal(called, "cargo build --release --locked --target x86_64-unknown-linux-gnu\n");
});
