# Agent — Personal Teamarness Package

Runs a stewarded Workspace Agent against an OpenAI-compatible completion endpoint. Replies when its current name is addressed as a whole word, optionally prefixed with `@`. Context comes from Conversations 1.2; signing authority stays in Teamarness Core.

## Build

Requires Node/npm, Rust 1.96 and macOS arm64. SDK, protocol and admission-validator source are vendored; no Teamarness checkout is needed to build.

```sh
npm ci
npm run build
```

Output: `dist/dev.teamarness.agent/`. The build writes artifact sizes/digests and validates the final directory, including a Backend smoke launch. Do not edit built files. `npm run typecheck`, `npm run lint`, and `npm test` provide the complete project checks. Keep both lockfiles committed. `npm run check:vendor` verifies SDK provenance.

## Register and configure

1. Start the desired Teamarness profile once, then stop it.
2. Add a local registration to `<app-data>/packages/registrations.v1.json`, using the canonical absolute path to `dist/dev.teamarness.agent/`, expected identity `dev.teamarness.agent`, and the explicit native trust acknowledgement documented by Teamarness's `docs/local-package-registration.md`. Preserve existing registrations. Use directory mode 700 and file mode 600; no symlinks.
3. Restart Teamarness. Agent appears under **Personal Packages**.
4. In a Workspace, choose an Agent you steward on this Installation, set its completion endpoint/model/instructions, and store its API key in the Agent UI.

Rebuild and restart after code changes; no hot reload. Each Installation has separate configuration. Personal installation does not share keys or grant extra Workspace authority. The Backend has the current OS user's native authority and sends Channel context to the configured model endpoint.

Changing only the source from bundled to local preserves the existing data-directory identity. Keep `dev.teamarness.agent` unchanged and leave the existing profile data in place. Do not copy/read/reset keys or application databases to perform this extraction.

## Optional Teamarness integration qualification

From a compatible Teamarness checkout, after a fresh build of this project:

```sh
TEAMARNESS_AGENT_PACKAGE_DIR=/absolute/path/to/agent/dist/dev.teamarness.agent \
  cargo test -p teamarness-system-tests --test agent_reply_flow -- --ignored
```

This explicitly runs external-package checks, including the longer loopback two-profile reply/delivery scenario. The default Teamarness suite does not require this separate project. The focused `local_agent_package_loads_from_registration` check covers the packaging boundary without repeating model/backlog behavior.

## Provenance and open findings

Product source/tests/contracts were extracted without behavioral changes from Teamarness commit `c9304d0` on `don-smith/agent_package`. `teamarness-authoring.json` records the current generated SDK snapshot. The Agent identity, capabilities, UI contract, local data formats and Conversations Ports are unchanged.

Bounded cleanup implements these finding corrections, pending consolidated verification/review:

- CR-002: binding/readiness transitions serialize; old replies are cancelled and drained before changing the bound actor. Held-completion rebind and overlapping bind/unbind/key-clear regressions cover this schedule.
- CR-003: startup/reconnect reconcile binding after capturing the roster cursor and before subscribing; failed reconciliation retries without another event. Reply work waits for startup/resume reconciliation.
- CR-005: completion redirects are rejected, with loopback 307/308 no-forwarding regressions; explicitly configured loopback HTTP remains supported.
- CR-006: successful complete settings/key changes clear stale errors while preserving replies still active in another Channel.
- CR-008: status mutation and watch publication share one lock, preserving their order.
- SM-003: progress open/advance and key reads already use SDK blocking work; remaining activation/resume key metadata checks now do too. Storage formats and at-most-once revision settlement are unchanged.
- SM-002: Teamarness Packages R14 now conditions its local-Installation check on Person actors and references the existing Agent-authority rule.

Extraction alone did not resolve these findings. Focused cleanup regressions do not establish a full workstream Verify pass. Do not publish or push this project as verified until consolidated validation and fresh review are complete.
