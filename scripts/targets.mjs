// The Backend targets this Package builds, and which one this machine can build.
//
// Builds are native only: the selected target is always the Rust host triple. Asking for another
// target refuses rather than producing an artifact whose declared target is false.

import { spawnSync } from "node:child_process";

export const supportedTargets = Object.freeze(["aarch64-apple-darwin", "x86_64-unknown-linux-gnu"]);

/** A refusal with a stable `target.*` code; scripts exit 2 for it. */
export class TargetError extends Error {
  constructor(code, message) {
    super(`${code}: ${message}`);
    this.name = "TargetError";
    this.code = code;
  }
}

/** The requested target from a script's arguments: none, or exactly `--target <triple>`. */
export function parseTargetArgs(argv) {
  if (argv.length === 0) return undefined;
  if (argv.length === 2 && argv[0] === "--target" && argv[1] !== "") return argv[1];
  throw new TargetError("target.usage", `expected no arguments or --target <triple>; got ${JSON.stringify(argv)}`);
}

/** The `host:` triple from `rustc -vV` output. */
export function parseHostTarget(output) {
  const hosts = String(output)
    .split(/\r?\n/)
    .filter((line) => line.startsWith("host:"))
    .map((line) => line.slice("host:".length).trim());
  if (hosts.length !== 1 || hosts[0] === "") {
    throw new TargetError("target.native_unknown", "rustc -vV did not report exactly one host target");
  }
  return hosts[0];
}

/**
 * The host triple of `$RUSTC` when set, otherwise of `rustc` on PATH, run in `cwd` so that a
 * rustup directory override or toolchain file resolves as it does for Cargo. Cargo's
 * `build.rustc` and `CARGO_BUILD_RUSTC` settings are not consulted.
 */
export function nativeTarget({ env = process.env, cwd } = {}) {
  const rustc = env.RUSTC || "rustc";
  const result = spawnSync(rustc, ["-vV"], { cwd, env, encoding: "utf8", shell: false });
  if (result.error || result.status !== 0) {
    const reason = result.error ? result.error.message : `status ${result.status ?? "unknown"}`;
    throw new TargetError("target.native_unknown", `could not run ${rustc} -vV: ${reason}`);
  }
  return parseHostTarget(result.stdout);
}

/** The target to build: the native one, which a request may only confirm. */
export function selectTarget({ requested, native }) {
  const target = requested ?? native;
  if (!supportedTargets.includes(target)) {
    const machine = requested === undefined ? "" : `; this machine builds ${native}`;
    throw new TargetError(
      "target.unsupported",
      `${JSON.stringify(target)} is not a supported Backend target (supported: ${supportedTargets.join(", ")})${machine}`,
    );
  }
  if (target !== native) {
    throw new TargetError(
      "target.not_native",
      `requested ${target}, but this machine builds ${native}; cross-compilation is not supported`,
    );
  }
  return target;
}

/** Exit status for a script failure: 2 for a target refusal or usage error, 1 otherwise. */
export function exitCodeFor(error) {
  return error instanceof TargetError ? 2 : 1;
}
