# Working on Agent

This repository contains the standalone Teamarness Package `dev.teamarness.agent`. Read this file before changing its contract, Backend, UI, persisted data, dependencies, or build output.

## The system this Package joins

Teamarness is a local-first, peer-to-peer team workspace. A team can use one desktop application while Packages provide separate product capabilities.

The Teamarness Core Runtime owns authority and behavior shared by Packages:

- Installation identity;
- Workspace creation and selection;
- canonical Workspace membership;
- Package lifecycle; and
- implementations of declared Semantic Capabilities.

The Teamarness Shell owns desktop navigation and Package UI mounting. The Package Host admits Package bytes, starts Package Backends, routes declared requests, supplies Workspace context and data paths, and supervises Backend processes.

This Package owns its product domain. That includes its request schemas, domain rules, validation, records, migrations, projections, policy, Backend behavior, and UI. Keep product-specific behavior here rather than expecting a change to the Teamarness Core Runtime, Shell, or Package Host.

## Runtime path

A normal request follows this path:

```text
Package UI
  -> mount-scoped PackageClient
  -> Teamarness Package Host
  -> supervised Package Backend
  -> Package-owned behavior or data
  -> response through the same declared contract
```

`ui-contract.json` defines the requests that may cross this path. Treat it as the shared boundary between the UI and Backend. Change the contract before implementing a new request on either side.

The Agent configures an OpenAI-compatible model endpoint and replies to addressed Workspace Messages through Conversations 1.2 Ports. It declares workspace-context, principals, agent-binding and package-ports 2.0 capabilities. Only the built-in Conversations harness retains a fixed consumed declaration; other eligible tools are discovered without manifest entries. It also offers the Steward a direct, in-memory chat with the configured model on a separate UI tab. It has one Backend, one ESM UI entrypoint, a UI contract with status and chat subscriptions, and no UI assets. It targets `aarch64-apple-darwin`. Personal registration changes distribution, not Workspace authority or data scope. Read README.md for setup and unresolved findings.

Chat and reply turns can call tools of Host-enabled Packages through package-ports 2.0. `backend/src/turn.rs` bounds the model/tool loop; `access::usable_now` obtains complete Host metadata for available selections pinned to the offered major. Discovery uses the existing Catalog seam and canonical SDK page/chunk/hash/schema checks. Events and streaming queries remain distinct and are not ordinary model tools. A turn snapshot is not a grant: every call/subscription uses explicit Agent access and contract major, and the Host authorizes dispatch.

The Steward changes Host-owned selections in Configuration's Packages section. Enable sends the observed offered major; unavailable saved selections remain disableable. Legacy `packages.json` is not opened, imported or changed. Conversations participation needs explicit enablement and a binding, never an automatic grant. Disable/rebind/unbind cancel affected work; stream terminals drain replies before explicit reconnect/reconciliation. Independent direct chat still works without a binding or Package tools. The model never gets Conversations `message.post` or Exhibits `file.read`/`file.put` (`access::WITHHELD`). The harness posts final reply text once; already-authorized commands retain the Host's original actor and uncertainty and are never replayed. `command_ownership` retains admitted actual work independently of turn/handler tasks. Binding changes refuse while commands remain pending; transport ambiguity blocks identity changes, not unrelated replies/chat. Binding ambiguity also refuses identity-dependent tools. No timer/settings/roster edit clears uncertainty. Restart/retirement recovery remains unqualified; never advertise it as safe without real Host evidence. README.md documents bounds and qualification limits.

## Project map

| Path | Responsibility |
|---|---|
| `teamarness.package.json` | Declares Package identity, compatibility, Backend artifacts, UI artifacts, the UI contract, assets, and required capabilities. |
| `ui-contract.json` | Contains closed JSON schemas for every UI request, response, subscription request, and event. |
| `backend/src/lib.rs` | Owns domain behavior, lifecycle handling, validation, and Package data. |
| `backend/src/command_ownership.rs` | Owns actual generic/final-post commands across observer cancellation; bounded atomic identity-transition admission, stale-intent refusal and explicit uncertainty. |
| `backend/src/turn.rs` | The sans-I/O turn engine and its bounds (`TurnBounds`) and stable codes. |
| `backend/src/model.rs` | The OpenAI-compatible model client, with tool definitions and tool calls. |
| `backend/src/tools.rs` | The generic port tool executor, `skill_read`, the system-message tool context, and `TurnTools`, which runs a chat or reply turn. |
| `backend/src/catalog.rs` | The `Catalog` seam, bounded Host discovery and historical Provider fixtures. |
| `backend/src/access.rs` | Decides which contracts are listed and usable. |
| `backend/src/packages.rs` | Historical `packages.json` storage regressions only; no production authorization. |
| `backend/src/skills.rs` and `backend/skills/` | The Agent's compiled-in skills. |
| `backend/catalog/` | Verbatim Provider `ports.json` copies and `provenance.json`. Refresh with `node scripts/refresh-catalog.mjs --from <teamarness-checkout>`; never edit by hand. |
| `backend/src/main.rs` | Starts the Backend through the vendored SDK. Keep it thin. |
| `backend/tests/` | Exercises Backend behavior through the SDK test harness, with no protocol frames. |
| `ui/src/index.ts` | Mounts the Package UI and communicates through the supplied `PackageClient`. |
| `ui/src/index.ts` | Also holds this Package's CSS, adopted into the mount and removed on unmount. |
| `ui/src/contract.generated.ts` | Generated request and response types. Regenerate it; never edit it. |
| `ui/src/index.test.ts` | Exercises UI behavior with the UI SDK's fake Package client. |
| `scripts/assemble-package.mjs` | Assembles final Backend, UI, contract, and manifest bytes into `dist/`. |
| `scripts/validate-dist.mjs` | Applies Package Admission's rules to `dist/` and smoke-launches the Backend. |
| `scripts/verify-vendor.mjs` | Fails when a vendored SDK file no longer matches recorded provenance. |
| `scripts/verify-catalog.mjs` | Fails when a bundled catalog file no longer matches `backend/catalog/provenance.json` (`npm run check:catalog`). |
| `scripts/refresh-catalog.mjs` | Copies Provider `ports.json` files from a Teamarness checkout and rewrites catalog provenance. |
| `vendor/teamarness/` | Contains the generated copies of the Package Process Protocol and SDK source used by this project. |
| `teamarness-authoring.json` | Records template, protocol, UI host, SDK provenance, and request namespace information. |
| `Cargo.lock` and `package-lock.json` | Lock this project's Rust and JavaScript dependency graphs. Commit both. |
| `dist/` | Generated Package Directory output. Never edit it by hand. |

The root Cargo and npm workspaces belong to this Package project. This project must build without a Teamarness checkout path.

## Design rules

### Start with the contract

For each product operation:

1. Give it a lowercase dotted name in `ui-contract.json`.
2. Define separate, closed request and response schemas.
3. Add Backend behavior and tests.
4. Add UI behavior and tests.
5. Build and inspect the final Package Directory.

Use IDs for stable domain choices and labels for presentation. Reject unknown fields where the contract allows. Version persisted schemas and plan migrations before changing stored records.

Do not add a request that bypasses the product model merely because it is convenient for the UI. Prefer a small operation that states user intent and lets the Backend enforce invariants.

### Keep domain behavior in the Backend

The Backend should own:

- domain rules and calculations;
- validation at trust boundaries;
- random seed or identifier creation when those values affect product meaning;
- persisted records and schema migrations;
- save-time verification and regeneration; and
- background work that must outlive a visible UI mount.

Prefer deterministic pure functions for rules that can be expressed that way. Test them with independent examples, invalid inputs, and boundary values.

When generated output includes a seed, revision, mode, or other value that is meant to affect the result, test the result itself. Use fixed inputs at the pure behavior seam and prove both directions: equal inputs produce equal output, and selected unequal inputs produce unequal output. A test is wrong if only metadata changes while the meaningful output stays fixed.


The Backend receives its Package Data Directory at activation, as `Activation::data_directory()`. Use that supplied path. Do not hard-code an application-data path, inspect Teamarness databases, or store Package records in the project directory.

A Package Data Directory belongs to one Package Identity, Workspace, and Installation. Data is not global across Workspaces. Teamarness does not inspect, migrate, back up, delete, synchronize, or share Package-owned files for you.

Keep lifecycle operations bounded. The vendored SDK handles the Package Process Protocol and control-frame machinery: build a `Router`, register typed handlers, and let the activation hook produce the state those handlers share. Do not hand-write protocol frames.

### Validate before you build

`npm run validate` applies Package Admission's manifest and UI contract rules to this project and prints the host's verdict, with the rule that failed and where. Run it after every contract or manifest change, before building. `npm run build` assembles `dist/` and then runs the same rules against the assembled Package Directory, with a Backend smoke launch.

### Blocking work and cancellation

Backend handlers run on an async runtime. Anything that blocks — a file read or write, SQLite, hashing, a directory walk — goes inside `ctx.blocking(...)`, which moves it off that runtime and hands the closure a cancellation token.

The host cancels a request, and the SDK cancels its token, when the caller cancels, when the request's deadline passes, and when the Package is suspended or disposed. Long work checks `cancel.is_cancelled()` and returns early. A handler that blocks the runtime freezes every other request, including the host's health checks.

### Errors

A Package error code is namespaced and stable: `agent.storage_failed`, not "error" and not a sentence. It reaches the Package UI unchanged, with `retryable` and bounded `details`, so the UI can tell "try again" from "this cannot work".

Mark an error `retryable` only when repeating the same request may succeed. Invalid input is not retryable; a failed write may be.

The SDK owns the codes that are not about this product: `package.not_active`, `package.unknown_operation`, `package.unknown_subscription`, `package.invalid_payload`, `package.blocking_task_failed`, and `cancelled`. Do not redefine them.

### Testing the Backend without frames

`backend/tests/` drives the Backend through `teamarness_package_sdk::testing::{HarnessBuilder, Harness}`: activate, request, subscribe, suspend, recover, dispose, shutdown — with a real temporary Package Data Directory and no hand-built frames. `assert_router_matches_contract` proves the router and `ui-contract.json` declare the same operations, so a contract change without a handler fails the tests.

### Typed UI requests

The UI calls `typedPackageClient<AgentContract>(context.client)`, whose types come from `ui-contract.json` through `teamarness-contract-types`. Change the contract first, then run `npm run contract-types --workspace @teamarness/agent-package-ui` to regenerate `ui/src/contract.generated.ts` and commit it. `npm run typecheck` fails when the committed types are stale, and the UI cannot name an operation or build a payload the contract does not describe.

Adopt Package CSS with `adoptPackageStyles(context.root, ...)` and drop it in the unmount. Present a failure with `describePackageError(error)`, and ignore an aborted request with `isAbortError(error)`.

### Keep the UI mount-scoped

Export `mount(context)` from the ESM entrypoint and return an idempotent unmount function.

Use `context.root` for rendered content and `context.client` for Package requests. Clean up listeners, subscriptions, timers, and DOM nodes during unmount. Present loading and request failures explicitly.

The UI should own interaction and presentation, not durable product authority. Do not calculate canonical records in the UI. The UI runs in a sandboxed frame, so Tauri is unreachable, browser storage and the network are unavailable, and the Shell-owned DOM is out of reach; `context.client` is the only way out.

Use semantic controls and accessible names. Mark purely decorative content outside the accessibility tree. Test behavior through user-visible state and PackageClient requests rather than internal function calls.

### Treat P2P and capabilities as explicit dependencies

Teamarness is a P2P application, but that does not make Package records synchronized. Do not open direct peer connections or assume access to Core Runtime transport, identities, keys, membership state, databases, or secrets.

A Package uses Teamarness-owned authority only through versioned Semantic Capabilities declared in `teamarness.package.json` and granted by the host. Add a capability only when the product requires it. Define the behavior and failure cases before changing the manifest.

Package source, trust, Workspace enablement, synchronization, sharing, networking, and collaboration are separate decisions. A local registration provides none of them automatically.

## Artifact rules

`npm run build` creates `dist/dev.teamarness.agent/`. That Package Directory is the unit Teamarness validates and runs. The assembler writes final sizes and lowercase SHA-256 digests after copying the final files.

Do not edit `dist/` after assembly. Any byte change makes the manifest declaration stale.

The foundation assembler copies one Backend executable, `ui/dist/index.js`, and `ui-contract.json`. The manifest declares no assets. Historical catalog fixtures and skills are compiled into the Backend, not shipped as files. Keep the UI inside the single ESM bundle unless the product needs a different artifact shape.

If you add UI assets, another Backend, or another target platform, update all of the following together:

- `teamarness.package.json`;
- `scripts/assemble-package.mjs`;
- build dependencies and commands; and
- tests that verify final paths, permissions, sizes, and digests.

Do not add this project to the Teamarness Cargo or npm workspaces. Do not edit Teamarness application Package metadata for an installation-local Package.

## Vendored SDK policy

The source under `vendor/teamarness/` was copied from the Teamarness checkout that created this project. It makes the project independent of that checkout.

Treat vendored SDK files as external infrastructure. Do not modify them to implement Package behavior or to work around a contract mistake. Change Package code first. If an SDK defect blocks correct implementation, report the exact defect and the smallest upstream change needed.

`teamarness-authoring.json` records which checkout the copy came from — `sdkRevision`, `sdkRevisionDirty` — and a digest of every vendored tree. `npm run check:vendor` recomputes those digests as part of `npm test`, so a failure there means a vendored file was edited: restore it rather than weakening the check. From a Teamarness checkout, `npm run create:package -- --vendor-report <project>` reports which trees differ from that checkout, and changes nothing.

There is no general supported in-place SDK upgrade. The approved package-ports 2.0 migration uses `scripts/refresh-sdk.mjs` to reproduce the exact clean canonical Host 4e0249b generator snapshot; it refuses other revisions and records every tree digest. Teamarness SDK fixes do not arrive in this repository automatically. Preserve source notices, license metadata, relative dependency paths, and `teamarness-authoring.json` provenance.

## Security model

A Package Backend runs with the current user's ambient filesystem, network, and process authority. Process supervision is a recovery boundary, not a sandbox. Avoid unnecessary dependencies and operating-system access. Validate paths and untrusted input.

A Package UI runs in a sandboxed frame with an opaque origin. Tauri is unreachable, the Shell's DOM and storage are unreachable, and the frame's own policy denies network requests, nested frames, form navigation, and inline code. Treat the mount root and PackageClient as the supported interface. Frame isolation separates realms, not threads: a UI that blocks its thread freezes the Shell.

Review both Backend and UI dependencies before shipping or registering the Package.

## Working method for agents

Before editing:

1. Read `teamarness.package.json`, `ui-contract.json`, and `teamarness-authoring.json`.
2. Read the relevant Backend and UI tests.
3. Read the local SDK README for any interface you will use.
4. State the product behavior, ownership boundary, files to change, and tests to add.
5. Confirm whether the artifact shape or declared capabilities must change.

While editing:

- work only in this Package project;
- keep the contract, Backend, UI, and tests aligned;
- write a failing behavior test before production behavior when the seam is testable;
- use real temporary directories for persistence tests;
- keep lockfile changes deliberate; and
- do not weaken schemas, tests, permissions, or digest checks to make a build pass.

Before reporting completion, run:

```sh
npm run validate
npm run typecheck
npm run lint
npm test
npm run build
```

`npm run lint` already runs `cargo fmt --all --check` and `cargo clippy`.

Then inspect `dist/dev.teamarness.agent/` and report:

- which behavior changed;
- which contract operations changed;
- which persisted schema or migration changed;
- whether capabilities or artifact shape changed;
- the tests and checks that passed; and
- any remaining runtime or manual verification.

Do not claim synchronization, team sharing, sandboxing, another target platform, or compatibility beyond what the manifest and tests prove.
