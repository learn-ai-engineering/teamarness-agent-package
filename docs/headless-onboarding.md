# Run the Agent on a headless Teamarness Host

This guide installs and configures the Agent Package (`dev.teamarness.agent`) on a headless Teamarness Installation. Once it is set up, the Agent replies to Conversations Messages and Documents Comments that mention it, as a bound Workspace Agent. Every step uses the Host's generic operator commands and this Package's own declared requests. Nothing writes Package or Core files directly.

> **Verified on** Ubuntu 26.04 x86_64 with Host 72cb5f2, on 2026-11-21. Other glibc-based x86_64 Linux distributions are expected to work but have not been tested. The Agent request names and JSON bodies come from this Package's contract (`ui-contract.json`) and are stable. The Host's headless commands are not final; if one differs, the Host's documentation wins.

## Prerequisite: a running headless Host

You need a headless Teamarness Host that is installed, running, named and joined to a Workspace. Complete the Host's guide, "Install headless Teamarness on Linux" (`docs/headless/install-linux.md` in the Teamarness repository), first: run its setup with a Workspace Invitation, or join later with its "Join another Workspace" section.

You also need:

- Bash and Python 3.11 or later on the target machine;
- an OpenAI-compatible model endpoint, a model ID and an API key, such as an OpenRouter key; and
- the Teamarness desktop app on another machine, to create Workspace Invitations and to test the Agent.

Run every command below as the account that runs the Host, with `umask 077`.

The short version: get the Package, copy it into place, then run `agent-setup` once per Workspace with an answers file. The [manual setup](#manual-setup-reference) at the end shows the same steps one request at a time.

## 1. Get the Package

You need two things on the target machine, side by side in one directory:

- the Package Directory `dev.teamarness.agent/`; and
- the setup script `agent-setup`.

For example:

```text
~/release-staging/agent/
├── agent-setup
└── dev.teamarness.agent/
```

> **Note:** Release archives do not exist yet. Until they do, someone builds both on a Linux x86_64 build machine (see [Build the Package](#build-the-package)) and copies them to the target machine. When archives exist, this step becomes "download and unpack".

The target machine does not need a build toolchain.

## 2. Copy it into place

The Package Directory is the permanent location that the Host trusts. Copy the complete directory and never symlink it, and do not edit any file after the copy. Copy the setup script beside it. In these commands, `/path/to/dist/` is the directory that holds the downloaded or copied distribution, for example `~/release-staging/agent/`:

```sh
umask 077
mkdir -p ~/.local/lib/teamarness-packages
chmod go-w ~/.local/lib/teamarness-packages
rsync -a --delete /path/to/dist/dev.teamarness.agent/ \
  ~/.local/lib/teamarness-packages/dev.teamarness.agent/
rsync -a /path/to/dist/agent-setup ~/.local/lib/teamarness-packages/agent-setup
```

Every directory above the Package Directory must not be group- or other-writable. The Host's setup already removes that access from the directories you own above its install location, including `~/.local/lib`.

To record what you installed, save the digests:

```sh
(cd ~/.local/lib/teamarness-packages/dev.teamarness.agent && find . -type f -exec sha256sum {} + | sort -k2)
```

## 3. Set up a Workspace with `agent-setup`

Configuration is per Workspace, by design. Each Workspace has its own Agent, settings, API key, binding and enabled Packages, and its own Package data. Run setup once for each Workspace, each with its own answers file. The Package itself installs once per machine; the first setup run installs it.

1. Write the answers template. Setup refuses to overwrite an existing file:

   ```sh
   S=~/.local/lib/teamarness-packages/agent-setup
   "$S" --print-template ~/agent-setup.toml
   ```

2. Fill it in. Set `workspace_id` to the ID from `workspace list`, choose `agent_name` and set `[model]`. Under `[packages]`, the template lists every Package the Agent can be enabled for: Backlog, Conversations, Documents and Exhibits. Remove the Packages this Agent should not access. Read the native-code risks in the template, then set `accept_native_code_risks = true` so that setup may install the Package. The answers file holds no secrets.

3. Put the API key in a private file. It holds only the key:

   ```sh
   (umask 077; IFS= read -rs KEY; printf '%s\n' "$KEY" > ~/agent-api-key; unset KEY)
   ```

   Setup checks that the file is a regular file, not a symlink, owned by you, with mode `0600`. It deletes the file once the key is saved. Instead of a file, you can set `key_file = "-"` and pipe the key on stdin.

4. Apply it:

   ```sh
   "$S" --answers ~/agent-setup.toml
   ```

   Setup reads the current state first and changes only what differs. It prints one line per item and a summary:

   ```text
   unchanged: host ready, Workspace joined
   changed: package installed from /home/you/.local/lib/teamarness-packages/dev.teamarness.agent (version 1.0.1)
   changed: agent Factory created
   changed: settings (endpointBaseUrl, model, systemPrompt)
   changed: api key (key file /home/you/agent-api-key deleted)
   changed: binding to Factory
   changed: package dev.teamarness.backlog enabled at major 1
   changed: package dev.teamarness.conversations enabled at major 1
   changed: package dev.teamarness.documents enabled at major 1
   changed: package dev.teamarness.exhibits enabled at major 1
   ready: agent Factory in Workspace aad43be122caa818738f763c373ba820
   summary: 9 changed, 1 unchanged
   ```

5. Test it: from the desktop app in the same Workspace, mention the Agent in a Conversation, for example "@Factory, are you there?". Exactly one reply should arrive, authored by the Agent.

Run setup again with the same answers at any time. It reports every item as `unchanged`. A missing key file is fine once the key is set. To change something, edit the answers and rerun. To rotate the key, write a new key file and rerun.

Setup uses these rules:

- An Agent with exactly `agent_name` is reused. Otherwise setup creates it.
- Contracts not listed under `[packages]` are left as they are. Setup never disables one.
- Every Package listed under `[packages]` must be installed and available in the Workspace. Otherwise setup stops before it enables any of them and names each missing one. Remove it from the list, or install it, then rerun setup.
- If a different Agent is already bound, setup stops and prints the unbind command. It does not rebind, because rebinding cancels the Agent's work.
- If the Package is registered from a different directory, setup stops. It does not reinstall.
- If a change's outcome is unknown, setup stops at once and prints `stopped: <step> outcome unknown — inspect with: <command>, then rerun setup`. It never retries. Run the inspect command, then rerun setup, which reads the state again and continues from there.
- Exit codes match the Host client: `0` ok, `1` operational failure or unknown outcome, `2` invalid answers or input. Invalid answers change nothing.

## Manual setup (reference)

These are the steps that `agent-setup` performs, one request at a time. Use them to inspect state or when you can't run setup. They use the `tm` and `tm_req` operator helpers that the Host's setup writes. Load them in each shell first:

```sh
source ~/.config/teamarness-headless/helpers.sh
```

The helpers are:

- `tm …` runs the Host's operator client against its data directory.
- `tm_req WORKSPACE PACKAGE OPERATION` sends one Package request. It reads the JSON request on stdin and looks up the current contract digest for you.

### Inspect and install, once per machine

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

### Configure each Workspace

Repeat steps 4–9 for each Workspace.

### 4. Select the Workspace

Set `W` to the Workspace ID:

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
rsync -a --delete /path/to/dist/dev.teamarness.agent/ \
  ~/.local/lib/teamarness-packages/dev.teamarness.agent/
rsync -a /path/to/dist/agent-setup ~/.local/lib/teamarness-packages/agent-setup
tm packages reload --package dev.teamarness.agent
```

The reload restarts the Backend in every Workspace. It keeps each Workspace's settings, API key, binding, enabled Packages and reply progress, and Messages that were already answered are not answered again. Afterwards, rerun `agent-setup` with each Workspace's answers file. It should report every item `unchanged` and end `ready`. Setup and `tm_req` both re-read the contract digest, so a reload never leaves you with a stale one.

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

- **Rotate the key:** write a new key file and rerun `agent-setup`, or repeat step 7. Clear it with `echo '{}' | tm_req "$W" dev.teamarness.agent agent.key.clear`.
- **Unbind:** `echo '{}' | tm_req "$W" dev.teamarness.agent agent.binding.clear`. This stops replies and cancels in-flight work.
- **Remove the Package:** `tm packages remove --package dev.teamarness.agent`. This removes it from every Workspace. Its data, including settings, key and reply progress, is kept for a later reinstall.

## Build the Package

This section is for whoever builds the distribution. The target machine does not need it.

Build from a clean checkout of this repository on a Linux x86_64 build machine. The build machine needs rustup, Node.js and npm, git, and a C toolchain:

```sh
cd /path/to/agent-package
npm ci
npm run build
```

To run the full gates first, run `npm run validate`, `npm run typecheck`, `npm run lint` and `npm test` before you build.

`npm run build` builds only the machine's own target (`x86_64-unknown-linux-gnu` here), validates the result and smoke-launches the Backend. It produces both parts of the distribution in `dist/`: the Package Directory `dev.teamarness.agent/`, and beside it `agent-setup`. Nothing in the Package Directory points back into the source tree.

- The Backend links against the build machine's glibc. Build on a machine whose glibc is no newer than the target's.
- If several heavy builds share the build machine, run each one under the same lock, for example `flock ~/src/.heavy-build.lock npm run build`.

Copy both parts to the target machine, for example:

```sh
ssh target 'umask 077; mkdir -p release-staging/agent'
rsync -a --delete dist/dev.teamarness.agent dist/agent-setup target:release-staging/agent/
```

## Limits

- Headless operation is request/response only. The Agent's direct chat and its live status and chat subscriptions are desktop UI features and are not available.
- Configuration requests carry at most 256 KiB.
- Replies, model calls and Port tools run in the background with no UI mounted. The Agent's turn budgets and post-once rules are the same as on the desktop.
- After an outage, the Agent answers every mention it missed. Whether a long outage should produce a burst of late replies is an open follow-up.
- One Installation can run one Package with the identity `dev.teamarness.agent`. A second Agent Package needs a distinct Package Identity. This is an open follow-up.
