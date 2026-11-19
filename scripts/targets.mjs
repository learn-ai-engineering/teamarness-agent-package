// The Backend targets this Package builds, and which one this machine can build.
//
// Builds are native only: the selected target is always the Rust host triple. Asking for another
// target refuses rather than producing an artifact whose declared target is false.

import { spawnSync } from "node:child_process";

export const supportedTargets = Object.freeze(["aarch64-apple-darwin", "x86_64-unknown-linux-gnu"]);

export class TargetError extends Error {
  constructor(code, message) {
    super(`${code}: ${message}`);
    this.name = "TargetError";
    this.code = code;
  }
}

/// The `host:` triple from `rustc -vV` output.
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

/// The target this machine's Rust toolchain builds by default.
export function nativeTarget({ env = process.env } = {}) {
  const result = spawnSync("rustc", ["-vV"], { env, encoding: "utf8", shell: false });
  if (result.error || result.status !== 0) {
    throw new TargetError("target.native_unknown", `rustc -vV failed with status ${result.status ?? "unknown"}`);
  }
  return parseHostTarget(result.stdout);
}

/// The target to build: the native one, which a request may only confirm.
export function selectTarget({ requested, native }) {
  const target = requested ?? native;
  if (!supportedTargets.includes(target)) {
    throw new TargetError(
      "target.unsupported",
      `${JSON.stringify(target)} is not a supported Backend target (supported: ${supportedTargets.join(", ")})`,
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
