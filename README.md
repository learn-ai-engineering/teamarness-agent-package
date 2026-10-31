# Agent — Personal Teamarness Package

Runs a stewarded Workspace Agent against an OpenAI-compatible completion endpoint. Replies when its current name is addressed as a whole word, optionally prefixed with `@`. Conversations participation requires a binding and explicit Host enablement; signing authority stays in Teamarness Core.

## Build

Requires Node/npm, Rust 1.96 and macOS arm64. SDK, protocol and admission-validator source are vendored; no Teamarness checkout is needed to build.

```sh
npm ci
npm run build
```

Output: `dist/dev.teamarness.agent/`. The build writes artifact sizes/digests and validates the final directory, including a Backend smoke launch. Do not edit built files. `npm run typecheck`, `npm run lint`, and `npm test` provide the complete project checks. Keep both lockfiles committed. `npm run check:vendor` verifies SDK provenance. `npm run check:catalog` verifies historical Provider fixtures; both run as part of `npm test`.

## Register and configure

1. Start the desired Teamarness profile once, then stop it.
2. Add a local registration to `<app-data>/packages/registrations.v1.json`, using the canonical absolute path to `dist/dev.teamarness.agent/`, expected identity `dev.teamarness.agent`, and the explicit native trust acknowledgement documented by Teamarness's `docs/local-package-registration.md`. Preserve existing registrations. Use directory mode 700 and file mode 600; no symlinks.
3. Restart Teamarness. Agent appears under **Personal Packages**.
4. In a Workspace, open the Agent's Package page and its **Configuration** tab. Choose an Agent you steward on this Installation, set its completion endpoint/model/instructions, and store its API key there.
5. Open the Agent itself to talk to the configured model directly; its main view is the chat. The chat needs the endpoint, model and key but no binding; it is never posted to a Channel. The Backend keeps the transcript in memory only (up to 64 entries); a Backend restart clears it. `agent.chat.send` returns at once and the reply arrives through `agent.chat.changes`, because a completion can outlast the Host's 30 s UI request deadline.

6. In the **Packages** section of the Agent's Configuration, explicitly enable **Conversations** to receive Channel replies. Enable other Packages only when you want their model tools. See [Package tools](#package-tools).

Rebuild and restart after code changes; no hot reload. Each Installation has separate configuration. Personal installation does not share keys or grant extra Workspace authority. The Backend has the current OS user's native authority and sends Channel context to the configured model endpoint.

Changing only the source from bundled to local preserves the existing data-directory identity. Keep `dev.teamarness.agent` unchanged and leave the existing profile data in place. Do not copy/read/reset keys or application databases to perform this extraction.

## Package tools

The chat and Channel replies can use other installed Packages through Package Ports. Each turn offers the model the tools of the Packages that are usable now, plus the Agent's own compiled-in skills.

- **Discovery.** The Host lists admitted guide-bearing contracts, including Providers unknown to this Agent and unavailable retained selections. Conversations and Exhibits have guides in the current Host; Documents does not. Discovery needs a binding and does not start Providers. The Agent accepts at most 32 identities per snapshot. It uses validated SDK pages and complete, hash-verified metadata, never guide Markdown as schemas.
- **Enablement.** The Host owns durable selections for this Package Instance. Enable sends the offered major the UI observed; it never substitutes a newer offer. Use **Refresh Packages** after a changed-offer refusal before explicitly enabling the new major. The UI displays the saved enabled major separately. Unavailable choices remain disableable by identity. Host failures do not become local successful switches. Existing `packages.json` is untouched and grants nothing. No legacy selection is imported.
- **What the model sees.** The system message has one line per usable Package, naming its guide tool (`<alias>__runtime_guide`), and one line per skill. The model reads a guide or a skill (`skill_read`) only on request. Tool output is data, not instructions. With nothing enabled, no tools are sent and the request is the same as before tools existed.
- **What the Steward sees.** A chat answer that used Package operations shows a muted line such as "Used Exhibits: runtime.guide, exhibit.list". A failed call shows its code. A chat turn that fails shows the same line under its error, for the calls that ran before it ended. Only the final text is kept as chat history.
- **Replies.** Conversations reads, event subscriptions and final posts require explicit enablement and a binding. Disable, rebind and unbind cancel affected turns. A terminal stream drains affected reply work before an explicit new subscription and reconciliation. The model never gets `message.post`; only the harness posts final text once. The trigger is persisted before the attempt and never replayed. Independent direct chat remains usable without Package access.
- **Withheld ports.** Only Conversations `message.post` is withheld. Exhibits `file.put` and `file.read` are offered as `exhibits__file_put` and `exhibits__file_read`, within the turn bounds below: a put whose arguments would cross the turn's message bound is refused before it is sent, and an oversized read result reaches the model as a code.
- **Identity transitions.** Final posts and generic commands have one bounded Agent-owned owner (at most 16 unresolved calls). Cancelling/aborting a turn stops observing, not an already admitted SDK call. Binding/clearing refuses immediately with `agent.binding_busy` while actual commands or a binding mutation remain pending; choose the binding again after a certain terminal. Commands are never retried. A local intent revision rejects old turns after a transition. Success and certain Provider/Host refusals release ownership, including failed posts.
- **Uncertainty.** Ambiguous command outcomes block identity changes with `agent.binding_uncertain`, not unrelated replies or direct chat. Ambiguous binding mutations also block identity-dependent tools; independent no-tools chat, disable and settings remain usable. SDK timeout/disconnect/invalid-response or owner panic do not establish quiescence even when an opaque error lacks the uncertainty flag. No timer, settings edit, re-enable, unrelated success, roster refresh or task abort clears this state. Do not replay the command; keep the identity and seek Host recovery assistance. **Restart/instance-retirement safety is not yet qualified**; do not treat a restart as a proven recovery fence.
- **Calls.** Tool arguments are validated locally before dispatch. Every call and subscription uses package-ports 2.0 Agent access and an explicit contract major. The Host reauthorizes each dispatch: metadata snapshots are not grants. Events and live queries are not ordinary model tools. A Provider error reaches the model with its stable code. Commands are never re-sent, including uncertain outcomes.

### Turn bounds and codes

Before a chat or reply calls the model, one shared toolset preparation allowance limits schema work to 16,384 JSON value visits and 1 MiB of allocation-work bytes. Preflight counts repeated reference expansion, both parameter copies, and each validator's original request and full definitions before copying or compiling. Bytes count `Value` storage, UTF-8 strings, and 128 bytes per object entry, plus reference decoding and tool names/descriptions. This is not a peak-memory measurement of the allocator or validator. Reference depth remains eight. An excessive toolset ends the turn with nonretryable `agent.tool_unavailable`; no partial toolset, model request, or Port call is sent, and no command uncertainty is created.

Each chat or reply turn allows at most 32 model calls, 64 tool calls, 16,384 characters per tool result, 96 KiB of messages added by its own tool calls, and 20 minutes. Each model call may take up to 5 minutes; a call that takes longer fails with retryable `agent.endpoint_timed_out`, distinct from `agent.endpoint_unreachable`. The system message and history a turn starts with are not counted, so a long conversation with nothing enabled is sent as before.

Before a batch of tool calls runs, the turn measures the arguments against the message bytes it has left, holding back 1/128 of the bound (768 bytes) for each call's result. A call whose arguments would cross that budget is not sent. The model gets `agent.tool_arguments_too_large` with `argumentBytes` and `remainingBytes`, and its arguments are replaced in the turn's messages by `{"elided":"agent.tool_arguments_too_large"}`, so the turn continues. Calls in the same batch that fit still run, in order. The budget is derived from the bounds, not from any Package. With the production bounds, this gives two effective ceilings:

- **Tool arguments:** about 96 KiB per turn, shared by every call and less what earlier results used. For Exhibits, that is about 70 KB of file content per turn, after the guide is read and once the content is base64-encoded. Each file must fit in one partial chunk.
- **`file.read`:** about 12 KB of file content per call, because 16,384 result characters hold that much base64. A larger read gives `agent.tool_result_too_large`.

These stable codes can appear:

| Code | Meaning |
|---|---|
| `agent.turn_limit_exceeded` | A turn bound was exceeded. The turn ends; the chat or the status shows the code, and a reply posts nothing. |
| `agent.tool_result_too_large` | One tool result was over the cap. The model sees this code instead of the result. |
| `agent.tool_arguments_too_large` | The call's arguments would cross the turn's remaining message budget. Not sent, not retryable; the model sees `argumentBytes` and `remainingBytes`, and the turn continues. |
| `agent.tool_arguments_invalid` | The arguments are not JSON or fail the port's request schema. Not sent. |
| `agent.tool_outcome_unknown` | A command was sent, but its outcome is unknown. Never re-sent. |
| `agent.tool_unavailable` | The tool is unknown or not usable now. Also ends a turn whose tool set cannot be built. |
| `agent.binding_required` | Bind an Agent before managing Packages. |
| `agent.invalid_metadata` | Discovery exceeded the Agent snapshot bound or metadata disagreed with its summary. |
| `agent.binding_busy` | A command or binding mutation remains pending; no new binding mutation sent. Choose again after settlement. |
| `agent.binding_uncertain` | Earlier actual work may remain; identity changes refused. Do not replay; seek Host recovery assistance. Restart safety is unqualified. |
| `agent.command_busy` | The 16-command ownership bound is full; refused before enqueue. |
| `agent.stale_intent` | A turn belongs to an earlier identity revision; refused before enqueue. |

### SDK provenance and retained fixtures

The approved SDK snapshot comes from clean Host revision `74d1f0bdfed48c8ddf35a0dfbcfa5a4fc7cc583d`. This controlled migration uses the canonical generator's complete production tree sets, dependency declarations, notices, and digest conventions:

```sh
node scripts/refresh-sdk.mjs /path/to/clean/Host-at-74d1f0b
node /path/to/Host/scripts/create-package.mjs --vendor-report "$PWD"
npm run check:vendor
```

The refresh refuses another revision or a dirty Host. It does not patch vendor implementation. Builds remain independent of the Host checkout. The SDK's new contract/sha2 dependencies are recorded in `Cargo.lock`; model/network dependencies are unchanged.

`backend/catalog/` retains historical verbatim Provider schemas, with their original provenance and digest gate. They test existing policy and tool-schema behavior, not runtime discovery. `fixed_harness_declaration_matches_its_historical_fixture` checks the five fixed Conversations ports against their fixture; Documents and Exhibits have no fixed consumed declaration. dynamic Agent access does not require a consumed declaration. Refreshing fixtures is separate from Host discovery and must preserve their tests and provenance.

## Optional Teamarness integration qualification

From a compatible Teamarness checkout, after a fresh build of this project:

```sh
TEAMARNESS_AGENT_PACKAGE_DIR=/absolute/path/to/agent/dist/dev.teamarness.agent \
  cargo test -p teamarness-system-tests --test agent_reply_flow --locked -- --ignored
```

Run the complete external target, including real Host admission/initialization, explicit Conversations enablement, attribution, cancellation, and at-most-once behavior. Also qualify an unknown Provider through real discovery and a scripted loopback model. The linked integration workstream owns these downstream tests. No local scripted-capability test substitutes for real Host authority.

Canonical `HarnessBuilder` currently initializes scripted capabilities at major 1. Agent tests use it to observe actual 2.0 payloads and SDK metadata verification, not to prove negotiated-major compatibility. Manifest validation and Backend smoke launch prove their packaging boundaries only. Real Host 2.0 qualification and the Steward's UI/model journey remain required before end-to-end completion.

## Provenance and verification

Product source/tests/contracts were extracted without behavioral changes from Teamarness commit `c9304d0` on `don-smith/agent_package`. `teamarness-authoring.json` records the current generated SDK snapshot. The Agent identity and settings/key/progress formats remain unchanged. This integration migrates package-ports to 2.0 and extends only the Packages UI operations with offered/enabled majors.

Bounded cleanup resolves the following findings. Consolidated verification and independent review passed for Backend/test source `ed3c016`, then passed again against merged Teamarness Host `a67f3c4` with the unchanged Agent distribution:

- CR-002 / CR-009: binding/readiness transitions serialize. Binding changes stop affected reply and Package-tool chat work before publishing another Agent. Independent no-tools chat does not depend on binding.
- At-most-once limit: triggers settle before the reply attempt. A failed or uncertain post is never replayed. The current Host preserves the authorization-time actor of earlier commands; disable or rebind does not retract already-authorized work. No dispatch fence is added, and uncertainty is never reported as certain failure. Real queued-command attribution is a downstream Host qualification requirement.
- CR-003: startup/reconnect reconcile binding after capturing the roster cursor and before subscribing; failed reconciliation retries without another event. Reply work waits for startup/resume reconciliation.
- CR-005: completion redirects are rejected, with loopback 307/308 no-forwarding regressions; explicitly configured loopback HTTP remains supported.
- CR-006: successful complete settings/key changes clear stale errors while preserving replies still active in another Channel.
- CR-008: status mutation and watch publication share one lock, preserving their order.
- SM-003: progress open/advance and key reads already use SDK blocking work; remaining activation/resume key metadata checks now do too. Storage formats and at-most-once revision settlement are unchanged.
- SM-002: Teamarness Packages R14 now conditions its local-Installation check on Person actors and references the existing Agent-authority rule.

The `agent-package` workstream records the passing consolidated review and validation at `verify/20261024T080059Z_agent-package-{review,validation}.md` and passing merged-Host qualification at `verify/20261024T092503Z_post-merge-{review,validation}.md`. Those historical checks passed on their recorded source, not this migration. Extraction alone did not resolve the findings; the later code corrections and review did. The canonical SDK refresh and current local checks are recorded separately for this migration.

The developer observed real-endpoint replies, Personal Package discovery, retained configuration after restart, and an offline-addressed reply visible at first inspection after the Steward returned. This is limited manual evidence, not exhaustive desktop coverage or measured latency. It was not repeated after the Host merge. Local verification does not authorize source push, remote creation, or publication.

Key-at-rest protection beyond file mode `0600`, streaming, Documents guidance/tools, file uploads through Package tools, scheduled posts, supported SDK publication, and stronger Package isolation need separate Scopes. The Teamarness roadmap owns those follow-ups. Incoming Teamarness Documents limitations and its record-log JSON authoring-schema mismatch remain separate upstream debt, not cleared by Agent qualification.
