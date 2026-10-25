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
5. Open the **Chat** tab to talk to the configured model directly. The chat needs the endpoint, model and key but no binding; it is never posted to a Channel. The Backend keeps the transcript in memory only (up to 64 entries); a Backend restart clears it. `agent.chat.send` returns at once and the reply arrives through `agent.chat.changes`, because a completion can outlast the Host's 30 s UI request deadline.

Rebuild and restart after code changes; no hot reload. Each Installation has separate configuration. Personal installation does not share keys or grant extra Workspace authority. The Backend has the current OS user's native authority and sends Channel context to the configured model endpoint.

Changing only the source from bundled to local preserves the existing data-directory identity. Keep `dev.teamarness.agent` unchanged and leave the existing profile data in place. Do not copy/read/reset keys or application databases to perform this extraction.

## Optional Teamarness integration qualification

From a compatible Teamarness checkout, after a fresh build of this project:

```sh
TEAMARNESS_AGENT_PACKAGE_DIR=/absolute/path/to/agent/dist/dev.teamarness.agent \
  cargo test -p teamarness-system-tests --test agent_reply_flow --locked -- --ignored
```

This explicitly runs four external-package checks: registration, loopback two-profile reply and backlog delivery, queued-post actor attribution during rebinding, and cancelled-binding-waiter ownership. The default Teamarness suite does not require this separate project. The focused `local_agent_package_loads_from_registration` check covers the packaging boundary without repeating model/backlog behavior.

## Provenance and verification

Product source/tests/contracts were extracted without behavioral changes from Teamarness commit `c9304d0` on `don-smith/agent_package`. `teamarness-authoring.json` records the current generated SDK snapshot. The Agent identity, capabilities, UI contract, local data formats and Conversations Ports are unchanged.

Bounded cleanup resolves the following findings. Consolidated verification and independent review passed for Backend/test source `ed3c016`, then passed again against merged Teamarness Host `a67f3c4` with the unchanged Agent distribution:

- CR-002 / CR-009: binding/readiness transitions serialize. Posts still waiting to dispatch are cancelled; independent owners fence already-dispatched posts and binding RPCs through safe outcomes, even if their local waiter is aborted. Pending posts promptly refuse bind/unbind with `agent.post_failed`; uncertain outcomes stay latched across configuration edits and suspend/resume. The compatible Host must fence old-generation actor lookups and binding writes before initializing replacement Backends. Enqueue-before-actor-read attribution and cancelled-binding regressions use a real test Host, alongside held-completion and lifecycle tests.
- Fail-closed limit: every non-success post terminal is conservatively uncertain; binding errors other than named pre-write rejections also latch dispatch closed. No local timeout, status reset, or later binding read proves remote quiescence. Full application-process restart is the conservative operational recovery; Backend-only replacement is safe only with the corrected Host generation fence. There is no automatic replay or persisted recovery machinery.
- CR-003: startup/reconnect reconcile binding after capturing the roster cursor and before subscribing; failed reconciliation retries without another event. Reply work waits for startup/resume reconciliation.
- CR-005: completion redirects are rejected, with loopback 307/308 no-forwarding regressions; explicitly configured loopback HTTP remains supported.
- CR-006: successful complete settings/key changes clear stale errors while preserving replies still active in another Channel.
- CR-008: status mutation and watch publication share one lock, preserving their order.
- SM-003: progress open/advance and key reads already use SDK blocking work; remaining activation/resume key metadata checks now do too. Storage formats and at-most-once revision settlement are unchanged.
- SM-002: Teamarness Packages R14 now conditions its local-Installation check on Person actors and references the existing Agent-authority rule.

The `agent-package` workstream records the passing consolidated review and validation at `verify/20261024T080059Z_agent-package-{review,validation}.md` and passing merged-Host qualification at `verify/20261024T092503Z_post-merge-{review,validation}.md`. All four explicit integration cases pass. Extraction alone did not resolve the findings; the later code corrections and review did. The vendored SDK snapshot remains unchanged.

The developer observed real-endpoint replies, Personal Package discovery, retained configuration after restart, and an offline-addressed reply visible at first inspection after the Steward returned. This is limited manual evidence, not exhaustive desktop coverage or measured latency. It was not repeated after the Host merge. Local verification does not authorize source push, remote creation, or publication.

Key-at-rest protection beyond file mode `0600`, streaming, tools, Documents participation, scheduled posts, supported SDK publication, and stronger Package isolation need separate Scopes. The Teamarness roadmap owns those follow-ups. Incoming Teamarness Documents limitations and its record-log JSON authoring-schema mismatch remain separate upstream debt, not cleared by Agent qualification.
