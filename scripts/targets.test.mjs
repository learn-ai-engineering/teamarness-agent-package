import assert from "node:assert/strict";
import test from "node:test";

import { supportedTargets, parseHostTarget, selectTarget } from "./targets.mjs";

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
