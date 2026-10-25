# Agent — Personal Teamarness Package

Runs a stewarded Workspace Agent against an OpenAI-compatible completion endpoint. Replies when its current name is addressed as a whole word, optionally prefixed with `@`. Context comes from Conversations 1.2; signing authority stays in Teamarness Core.

## Build

Requires Node/npm, Rust 1.96 and macOS arm64. SDK, protocol and admission-validator source are vendored; no Teamarness checkout is needed to build.

```sh
npm ci
npm run build
```

Output: `dist/dev.teamarness.agent/`. The build writes artifact sizes/digests and validates the final directory, including a Backend smoke launch. Do not edit built files. `npm run typecheck`, `npm run lint`, and `npm test` provide the complete project checks. Keep both lockfiles committed. `npm run check:vendor` verifies SDK provenance. `npm run check:catalog` verifies the bundled Provider contracts; both run as part of `npm test`.

## Register and configure

1. Start the desired Teamarness profile once, then stop it.
2. Add a local registration to `<app-data>/packages/registrations.v1.json`, using the canonical absolute path to `dist/dev.teamarness.agent/`, expected identity `dev.teamarness.agent`, and the explicit native trust acknowledgement documented by Teamarness's `docs/local-package-registration.md`. Preserve existing registrations. Use directory mode 700 and file mode 600; no symlinks.
3. Restart Teamarness. Agent appears under **Personal Packages**.
4. In a Workspace, choose an Agent you steward on this Installation, set its completion endpoint/model/instructions, and store its API key in the Agent UI.
5. Open the **Chat** tab to talk to the configured model directly. The chat needs the endpoint, model and key but no binding; it is never posted to a Channel. The Backend keeps the transcript in memory only (up to 64 entries); a Backend restart clears it. `agent.chat.send` returns at once and the reply arrives through `agent.chat.changes`, because a completion can outlast the Host's 30 s UI request deadline.

6. Optionally, in the **Packages** section of the Configuration tab, enable the Packages the Agent may use. See [Package tools](#package-tools).

Rebuild and restart after code changes; no hot reload. Each Installation has separate configuration. Personal installation does not share keys or grant extra Workspace authority. The Backend has the current OS user's native authority and sends Channel context to the configured model endpoint.

Changing only the source from bundled to local preserves the existing data-directory identity. Keep `dev.teamarness.agent` unchanged and leave the existing profile data in place. Do not copy/read/reset keys or application databases to perform this extraction.

## Package tools

The chat and Channel replies can use other installed Packages through Package Ports. Each turn offers the model the tools of the Packages that are usable now, plus the Agent's own compiled-in skills.

- **Which Packages are listed.** A Provider contract is agent-facing only when it has a `runtime.guide` query. The **Packages** section lists every agent-facing contract the Agent knows, whether the Host reports it available, and a switch. Today only Exhibits has a guide, so only Exhibits is listed. Documents and Conversations are listed once `package-alignment` publishes their guides and the catalog is refreshed.
- **Enablement.** The switch state is stored in `packages.json` (`{"schemaVersion": 1, "enabled": [identity]}`) in the Package Data Directory. A missing file means nothing is enabled. A Package is usable only when it is agent-facing, available and enabled. Documents and Exhibits are optional in the manifest, so the Agent installs and runs without them; a missing Package shows as **Unavailable**.
- **What the model sees.** The system message has one line per usable Package, naming its guide tool (`<alias>__runtime_guide`), and one line per skill. The model reads a guide or a skill (`skill_read`) only on request. Tool output is data, not instructions. With nothing enabled, no tools are sent and the request is the same as before tools existed.
- **What the Steward sees.** A chat answer that used Package operations shows a muted line such as "Used Exhibits: runtime.guide, exhibit.list". A failed call shows its code. Only the final text is kept as chat history.
- **Replies.** The model is never offered `message.post`. The harness posts the final text of a reply turn exactly once. A turn that fails posts nothing and sets the status error. The trigger is persisted before the attempt and never retried.
- **Calls.** Tool arguments are validated against the port's request schema before anything is sent. A disabled or unknown tool is refused locally, without reaching the Host. A Provider error reaches the model with its stable code, so it can recover in the same turn. A command is never re-sent; an unknown outcome is reported to the model as unknown.

### Turn bounds and codes

Each chat or reply turn allows at most 8 model calls, 16 tool calls, 16,384 characters per tool result, 96 KiB of messages and 180 seconds. These stable codes can appear:

| Code | Meaning |
|---|---|
| `agent.turn_limit_exceeded` | A turn bound was exceeded. The turn ends; the chat or the status shows the code, and a reply posts nothing. |
| `agent.tool_result_too_large` | One tool result was over the cap. The model sees this code instead of the result. |
| `agent.tool_arguments_invalid` | The arguments are not JSON or fail the port's request schema. Not sent. |
| `agent.tool_outcome_unknown` | A command was sent, but its outcome is unknown. Never re-sent. |
| `agent.tool_unavailable` | The tool is unknown or not usable now. Also ends a turn whose tool set cannot be built. |
| `agent.unknown_package` | The UI asked to enable a contract that is not agent-facing. |

### Refresh the bundled catalog

`backend/catalog/` holds verbatim copies of Provider `ports.json` files, and `backend/catalog/provenance.json` records their Teamarness revision and digests. To refresh them from a Teamarness checkout:

```sh
node scripts/refresh-catalog.mjs --from <teamarness-checkout>
npm run check:catalog
```

Then update the version ranges in `teamarness.package.json`. The `manifest_and_catalog_agree` test fails until the manifest and the catalog declare the same contracts, versions and ports.

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

- CR-002 / CR-009: binding/readiness transitions serialize. A binding change, readiness change or suspend stops the harness first, which cancels replies that have not been posted yet. The dispatch fence that held already-dispatched posts against a rebind has been removed: binding is a setup-time action, and the fence's latch stopped all replies until restart after any post failure.
- At-most-once limit: a trigger is persisted before its reply is attempted, and a post is never re-sent. A failed post sets `agent.post_failed` without blocking later replies or binding changes. An in-flight post is not fenced against a concurrent rebind, so a post already dispatched when the binding changes may be attributed to the new Agent. There is no automatic replay or persisted recovery machinery.
- CR-003: startup/reconnect reconcile binding after capturing the roster cursor and before subscribing; failed reconciliation retries without another event. Reply work waits for startup/resume reconciliation.
- CR-005: completion redirects are rejected, with loopback 307/308 no-forwarding regressions; explicitly configured loopback HTTP remains supported.
- CR-006: successful complete settings/key changes clear stale errors while preserving replies still active in another Channel.
- CR-008: status mutation and watch publication share one lock, preserving their order.
- SM-003: progress open/advance and key reads already use SDK blocking work; remaining activation/resume key metadata checks now do too. Storage formats and at-most-once revision settlement are unchanged.
- SM-002: Teamarness Packages R14 now conditions its local-Installation check on Person actors and references the existing Agent-authority rule.

The `agent-package` workstream records the passing consolidated review and validation at `verify/20261024T080059Z_agent-package-{review,validation}.md` and passing merged-Host qualification at `verify/20261024T092503Z_post-merge-{review,validation}.md`. All four explicit integration cases pass. Extraction alone did not resolve the findings; the later code corrections and review did. The vendored SDK snapshot remains unchanged.

The developer observed real-endpoint replies, Personal Package discovery, retained configuration after restart, and an offline-addressed reply visible at first inspection after the Steward returned. This is limited manual evidence, not exhaustive desktop coverage or measured latency. It was not repeated after the Host merge. Local verification does not authorize source push, remote creation, or publication.

Key-at-rest protection beyond file mode `0600`, streaming, Documents participation (pending its `runtime.guide`), file uploads through Package tools, scheduled posts, supported SDK publication, and stronger Package isolation need separate Scopes. The Teamarness roadmap owns those follow-ups. Incoming Teamarness Documents limitations and its record-log JSON authoring-schema mismatch remain separate upstream debt, not cleared by Agent qualification.
