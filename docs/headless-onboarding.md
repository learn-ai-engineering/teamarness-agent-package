# Run the Agent on a headless Teamarness Host

This guide builds, installs and configures the Agent Package (`dev.teamarness.agent`) on a headless Teamarness Installation. Once it is set up, the Agent replies to Conversations Messages and Documents Comments that mention it, as a bound Workspace Agent. Every step uses the Host's generic operator commands and this Package's own declared requests. Nothing writes Package or Core files directly.

> **Verified on** Ubuntu 26.04 x86_64 with Host 72cb5f2, on 2026-11-21. Other glibc-based x86_64 Linux distributions are expected to work but have not been tested. The Agent request names and JSON bodies come from this Package's contract (`ui-contract.json`) and are stable. The Host's headless commands are not final; if one differs, the Host's documentation wins.

## Prerequisite: a running headless Host

You need a headless Teamarness Host that is installed, running, named and joined to a Workspace. Complete the Host's guide, "Install headless Teamarness on Linux" (`docs/headless/install-linux.md` in the Teamarness repository), through step 5, "Join the Workspace", first. Its step 3, "Define the shell helpers", defines the two helpers this guide uses:

- `tm …` runs the Host's operator client against its data directory.
- `tm_req WORKSPACE PACKAGE OPERATION` sends one Package request. It reads the JSON request on stdin and looks up the current contract digest for you.

Define both helpers in every shell you use below.

You also need:

- an OpenAI-compatible model endpoint, a model ID and an API key, such as an OpenRouter key; and
- the Teamarness desktop app on another machine, to create Workspace Invitations and to test the Agent.

Run every command below as the account that runs the Host, with `umask 077`.

## Machine tier: once per machine

### 1. Get the Package

Release archives do not exist yet. When they do, this step becomes "download and unpack". Update this guide then.

Until then, build the Package from a clean checkout of this repository on a Linux x86_64 build machine. The build machine can be the target machine. It needs rustup, Node.js and npm, git, and a C toolchain:

```sh
cd /path/to/agent-package
npm ci
npm run build
```

`npm run build` builds only the machine's own target (`x86_64-unknown-linux-gnu` here), validates the result and smoke-launches the Backend. It produces `dist/dev.teamarness.agent/`. The Backend links against the build machine's glibc, so build on a machine whose glibc is no newer than the target's. If several heavy builds share the build machine, run each one under the same lock, for example `flock ~/src/.heavy-build.lock npm run build`.

To run the full gates first, run `npm run validate`, `npm run typecheck`, `npm run lint` and `npm test` before you build.

### 2. Copy it into place

The Package Directory is the permanent location that the Host trusts. Copy the complete directory and never symlink it, and do not edit any file after the copy. Nothing in it may point back into a source tree.

```sh
umask 077
mkdir -p ~/.local/lib/teamarness-packages
chmod go-w ~/.local/lib/teamarness-packages
rsync -a --delete /path/to/agent-package/dist/dev.teamarness.agent/ \
  ~/.local/lib/teamarness-packages/dev.teamarness.agent/
```

From a separate build machine, use `target:.local/lib/teamarness-packages/dev.teamarness.agent/` as the destination instead. Every directory above the Package Directory must not be group- or other-writable. Step 1 of the Host's guide already removes that access from `~`, `~/.local` and `~/.local/lib`.

To record what you installed, save the digests:

```sh
(cd ~/.local/lib/teamarness-packages/dev.teamarness.agent && find . -type f -exec sha256sum {} + | sort -k2)
```

### 3. Inspect and install

Inspection reads the Package Directory and does not run the Backend:

```sh
PKG_DIR="$HOME/.local/lib/teamarness-packages/dev.teamarness.agent"
tm packages inspect --directory "$PKG_DIR"
```

The Backend runs as native code with your account's filesystem, network and process authority. It is not sandboxed. To consent and install:

```sh
printf '%s\n' '{"acceptedRisks":["filesystem","network","process"]}' |
  tm packages install --directory "$PKG_DIR" --package dev.teamarness.agent --input -
```

The result is `"kind":"personal_installed"`. A Personal Package installs once per machine and runs in every Workspace that the Installation joins.

## Workspace tier: once per Workspace

Configuration is per Workspace, by design. Each Workspace has its own Agent, settings, API key, binding and enabled Packages, and its own Package data. Repeat steps 4–9 for each Workspace the Installation joins. You do not need to reinstall the Package.

### 4. Select the Workspace

Join the Workspace with step 5 of the Host's guide, then set `W` to its ID:

```sh
tm workspace list
W=REPLACE_WITH_WORKSPACE_ID
echo '{}' | tm_req "$W" dev.teamarness.agent agent.status.get
```

Always select Workspaces by ID. The status call proves that the Package has an Instance in this Workspace. Expect `"state":"no_agent"` or `"unconfigured"`.

### 5. Create the Workspace Agent

```sh
tm agent add --workspace "$W" --name Factory
```

Set `AGENT` to the returned `id`, a 64-character hex Principal ID. This Installation's Person becomes the Agent's Steward. Replies trigger on the Agent's current name, so choose the name people will mention. Creating the Agent neither configures nor binds the Package.

### 6. Configure the model

```sh
printf '%s\n' '{"settings":{"endpointBaseUrl":"https://openrouter.ai/api/v1","model":"z-ai/glm-5.3-flash","systemPrompt":"You are a member of the team.","contextMessages":20}}' |
  tm_req "$W" dev.teamarness.agent agent.settings.set
```

All four fields are required:

| Field | Meaning |
|---|---|
| `endpointBaseUrl` | The OpenAI-compatible base URL, at most 2048 characters. |
| `model` | The model ID, at most 256 characters. |
| `systemPrompt` | At most 16,384 characters. |
| `contextMessages` | 0–64: how many earlier Messages the Agent reads for context. |

The response is the saved settings plus `hasKey`.

### 7. Set the API key

Never put the key in a command argument, shell history, a message or a log. Read it without echo, and let Python escape it as JSON:

```sh
IFS= read -rs KEY
printf '%s' "$KEY" | python3 -I -c 'import json,sys; print(json.dumps({"apiKey": sys.stdin.read()}))' |
  tm_req "$W" dev.teamarness.agent agent.key.set
unset KEY
```

The response is `{"hasKey":true}`. The Agent stores the key in an owner-only (`0600`) file in its Package data and never returns it.

### 8. Bind the Agent, then enable Packages

Bind first. Until the Agent is bound, `agent.packages.get` and `agent.packages.set` fail with `agent.binding_required` and change nothing.

```sh
echo '{}' | tm_req "$W" dev.teamarness.agent agent.binding.candidates
printf '{"principalId":"%s"}\n' "$AGENT" | tm_req "$W" dev.teamarness.agent agent.binding.set
```

The candidates list shows the Agents this Installation stewards. The `agent.binding.set` response names the bound Agent. Binding changes are refused while earlier commands are unsettled. If that happens, wait, inspect, then try again (see [Errors and uncertain outcomes](#errors-and-uncertain-outcomes)).

List what the Host offers, then enable each contract with the `major` it offers. Do not guess the major:

```sh
echo '{}' | tm_req "$W" dev.teamarness.agent agent.packages.get
for c in conversations documents; do
  printf '{"contract":"dev.teamarness.%s","enabled":true,"major":1}\n' "$c" |
    tm_req "$W" dev.teamarness.agent agent.packages.set
done
```

- **Conversations:** the Agent replies to Messages that mention it.
- **Documents:** the Agent replies under Comments that mention it, and can edit Documents.
- **Exhibits** and others are optional and offer more tools.

To disable a contract, send `{"contract":"<contract>","enabled":false}`. Disabling cancels the Agent's work in that Package.

### 9. Verify

```sh
echo '{}' | tm_req "$W" dev.teamarness.agent agent.status.get
echo '{}' | tm_req "$W" dev.teamarness.agent agent.settings.get
```

Expect `"state":"ready"` with your Agent's `name`, and `"hasKey":true`. Other states mean:

- `unconfigured`: the endpoint, model or key is missing;
- `no_agent`: nothing is bound;
- `error`: `lastError.code` names the failure, for example `agent.endpoint_rejected`.

Then, from the desktop app in the same Workspace, mention the Agent in a Conversation, for example "@Factory, are you there?". Exactly one reply should arrive, authored by the Agent. Mentions that arrive while the Host is offline are answered when it comes back.

## Update the Package

Get the new version (step 1), then copy it over the same Package Directory and reload:

```sh
rsync -a --delete /path/to/agent-package/dist/dev.teamarness.agent/ \
  ~/.local/lib/teamarness-packages/dev.teamarness.agent/
tm packages reload --package dev.teamarness.agent
```

The reload restarts the Backend in every Workspace. It keeps each Workspace's settings, API key, binding, enabled Packages and reply progress, and Messages that were already answered are not answered again. Afterwards, run step 9 in each Workspace. `tm_req` re-reads the contract digest, so a reload never leaves you with a stale one.

## Errors and uncertain outcomes

The headless client never retries a mutation. When a request fails after the Host has validated its input, the operator error is `outcomeUncertain: true`, even for a read. The request may or may not have taken effect. Before you act again, read the state back:

| After a failed | Inspect with |
|---|---|
| `agent.settings.set`, `agent.key.set`, `agent.key.clear` | `agent.settings.get` (`settings`, `hasKey`) |
| `agent.binding.set`, `agent.binding.clear` | `agent.binding.candidates` (`bound`) and `agent.status.get` |
| `agent.packages.set` | `agent.packages.get` (`enabled`, `enabledMajor`) |

Send the request again only if the state shows that it did not apply. A `busy` refusal is certain and did not run: another operator request holds this Package Instance, so try again once it finishes.

The Agent's own error codes are stable and namespaced (`agent.*`). The original Package error is in the operator error's `details`. Its `retryable` flag says whether the same request may succeed later. It is not permission to replay a mutation whose outcome is uncertain.

Keep the client's stdout and stderr draining. Don't merge them into one pipe with `2>&1`. Explicit success and error output can contain sensitive Package output, so protect any file you redirect it to.

## Change or remove the configuration

- **Rotate the key:** repeat step 7. Clear it with `echo '{}' | tm_req "$W" dev.teamarness.agent agent.key.clear`.
- **Unbind:** `echo '{}' | tm_req "$W" dev.teamarness.agent agent.binding.clear`. This stops replies and cancels in-flight work.
- **Remove the Package:** `tm packages remove --package dev.teamarness.agent`. This removes it from every Workspace. Its data, including settings, key and reply progress, is kept for a later reinstall.

## Planned: answers-file setup

A later workstream replaces steps 4–9 with an idempotent, non-interactive setup. There will be no wizard. `setup --print-template` writes a commented answers file. You fill it in, and `setup --answers <file>` applies it and reports what it did. Running it again with the same answers changes nothing. The Agent implements its own setup; Packages share no code.

The template would hold:

```toml
# Agent setup answers. Secrets never go in this file.
workspace = "aad43be122caa818738f763c373ba820"  # Workspace ID, never a name

[agent]
name = "Factory"                     # created if missing, then bound

[model]
endpoint_base_url = "https://openrouter.ai/api/v1"
model = "z-ai/glm-5.3-flash"
system_prompt = "You are a member of the team."
context_messages = 20                # 0-64

[key]
file = "~/agent-api-key"             # 0600 file, deleted after success; or "-" for stdin

[packages]
enable = ["dev.teamarness.conversations", "dev.teamarness.documents"]  # majors taken from the Host's offer
```

Open questions for that workstream:

- whether setup renames or refuses an existing Agent whose name differs;
- whether contracts not listed under `enable` are disabled;
- how setup reports and recovers from an uncertain outcome partway through; and
- where setup runs, either as Host operator commands or through the Package's requests.

## Limits

- Headless operation is request/response only. The Agent's direct chat and its live status and chat subscriptions are desktop UI features and are not available.
- Configuration requests carry at most 256 KiB.
- Replies, model calls and Port tools run in the background with no UI mounted. The Agent's turn budgets and post-once rules are the same as on the desktop.
- After an outage, the Agent answers every mention it missed. Whether a long outage should produce a burst of late replies is an open follow-up.
- One Installation can run one Package with the identity `dev.teamarness.agent`. A second Agent Package needs a distinct Package Identity. This is an open follow-up.
