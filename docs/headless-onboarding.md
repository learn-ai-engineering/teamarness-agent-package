# Run the Agent on a headless Teamarness node

This guide configures the Agent Package on a headless Teamarness Installation, so that the Agent replies to Conversations Messages and Documents Comments as a bound Workspace Agent. Every step uses the headless application's generic operator interface and the Agent's own declared requests. Nothing writes Package or Core files directly.

> **Provisional Host syntax.** The `teamarness-headless` commands below come from the headless application's documentation at Teamarness commit `8f3f25b` (`docs/headless/README.md`). That application is not final. Until its Host handoff is complete, treat every command line in this guide as a placeholder, and check it against the shipped headless documentation. The Agent request names and JSON bodies are this Package's contract (`ui-contract.json`) and are stable.

## Before you start

You need:

- a running headless Installation (`teamarness-headless serve`) that has joined the Workspace, as described in the headless application's documentation;
- this Package built natively on the same machine (see [Build](../README.md#build)). On Ubuntu 26.04 x86_64, `npm run build` produces `dist/dev.teamarness.agent/` with an `x86_64-unknown-linux-gnu` Backend;
- an OpenAI-compatible model endpoint and its API key.

Set these shell variables. Use absolute paths, and create every input file with `umask 077`:

```sh
umask 077
APP=/absolute/path/to/teamarness-headless
DATA=/absolute/path/to/private/data-dir
WORKSPACE=REPLACE_WITH_WORKSPACE_ID   # from: "$APP" --data-dir "$DATA" workspace list
PACKAGE=dev.teamarness.agent
PACKAGE_DIR=/absolute/path/to/agent/dist/dev.teamarness.agent
```

Each `package request` below needs the operation name and the contract digest that the Host admitted. A helper keeps the commands short. It reads the digest from `package schema` each time, so a reload never leaves you with a stale one:

```sh
agent_request() { # usage: agent_request <operation> <input-file|->
  digest=$("$APP" --data-dir "$DATA" package schema --workspace "$WORKSPACE" --package "$PACKAGE" \
    --operation "$1" | node -e '
      let s = ""; process.stdin.on("data", (d) => (s += d)).on("end", () => {
        const find = (o) => o && typeof o === "object"
          ? ("contractDigest" in o ? o.contractDigest : Object.values(o).map(find).find(Boolean)) : undefined;
        console.log(find(JSON.parse(s)) ?? "");
      });') || return
  [ -n "$digest" ] || { echo "no contractDigest in package schema output" >&2; return 2; }
  "$APP" --data-dir "$DATA" package request --workspace "$WORKSPACE" --package "$PACKAGE" \
    --operation "$1" --contract-digest "$digest" --input "$2"
}
```

The Host's schema result carries the admitted digest as `contractDigest`. The helper finds that field wherever it sits in the printed JSON. It needs Node, which you already have for the build.

## 1. Create the Workspace Agent

```sh
"$APP" --data-dir "$DATA" agent add --workspace "$WORKSPACE" --name Scout
"$APP" --data-dir "$DATA" agent list --workspace "$WORKSPACE"
```

This Installation's Person becomes the Agent's Steward. Creating the Agent neither configures nor binds the Package. Replies trigger on the Agent's current name, so choose the name people will address.

## 2. Install the Package

Inspect it first. Inspection does not run the Backend.

```sh
"$APP" --data-dir "$DATA" packages inspect --directory "$PACKAGE_DIR"
```

The Backend runs as native code with your OS user's filesystem, network and process authority. It is not sandboxed. To consent and install:

```sh
printf '%s\n' '{"acceptedRisks":["filesystem","network","process"]}' \
  | "$APP" --data-dir "$DATA" packages install --directory "$PACKAGE_DIR" --package "$PACKAGE" --input -
"$APP" --data-dir "$DATA" package operations --workspace "$WORKSPACE" --package "$PACKAGE"
```

The operation list should include every request used below.

## 3. Configure the model

Write the settings to a private file, then send it:

```sh
cat > settings.json <<'EOF'
{"settings":{"endpointBaseUrl":"https://api.example.com/v1","model":"example-model","systemPrompt":"Be brief and helpful.","contextMessages":20}}
EOF
agent_request agent.settings.set settings.json
```

All four fields are required. `contextMessages` is 0–64: the number of earlier Messages the Agent reads for context. The response is the saved settings plus `hasKey`.

## 4. Set the API key

Never put the key in a command argument, a shell history line, or a log. Read it without echo and pipe it in:

```sh
IFS= read -rs API_KEY
printf '{"apiKey":"%s"}\n' "$API_KEY" | agent_request agent.key.set -
unset API_KEY
```

`printf` is a shell builtin, so the key does not appear in the process list. A key that contains `"` or `\` must be JSON-escaped first. The response is `{"hasKey":true}`. The Agent never returns the key.

## 5. Bind the Agent

List the Agents this Installation stewards, and copy the `principalId` of the one you created:

```sh
printf '{}\n' | agent_request agent.binding.candidates -
printf '{"principalId":"%s"}\n' "<64-hex-principal-id>" | agent_request agent.binding.set -
```

The response names the bound Agent. Binding changes refuse while earlier commands are unsettled. If the request is refused, wait and inspect, then try again (see [Errors](#errors-and-uncertain-outcomes)).

## 6. Enable the Packages the Agent uses

The Agent works only in the Packages you enable for it. Bind the Agent (step 5) first. Without a binding, `agent.packages.get` and `agent.packages.set` fail with `agent.binding_required`, and nothing changes.

List what the Host offers:

```sh
printf '{}\n' | agent_request agent.packages.get -
```

For each Package to enable, send its `contract` and the offered `major` from that list. Do not guess the major.

```sh
printf '{"contract":"dev.teamarness.conversations","enabled":true,"major":%s}\n' 1 | agent_request agent.packages.set -
printf '{"contract":"dev.teamarness.documents","enabled":true,"major":%s}\n' 1 | agent_request agent.packages.set -
```

- **Conversations:** the Agent replies to Messages that address it.
- **Documents:** the Agent replies under Comments that mention it.
- **Exhibits:** this is optional and offers file tools.

To disable a Package, send `{"contract":"<contract>","enabled":false}`. Disabling cancels the Agent's work in that Package.

## 7. Verify

```sh
printf '{}\n' | agent_request agent.status.get -
printf '{}\n' | agent_request agent.settings.get -
```

Expect `"state":"ready"` with your Agent's `name`, and `"hasKey":true`. Other states mean:

- `unconfigured`: the endpoint, model or key is missing;
- `no_agent`: nothing is bound;
- `error`: `lastError.code` names the failure, for example `agent.endpoint_rejected`.

Then, from a desktop peer in the same Workspace, address the Agent by name in a Conversations Channel, for example "Scout, are you there?". Exactly one reply should arrive, authored by the Agent. History from before the Agent started is never answered.

## Errors and uncertain outcomes

The headless client never retries a mutation. When `package request` fails after the Host has validated the input, the operator error is `outcomeUncertain: true`. The request may or may not have taken effect. Before you act again, read the state back:

| After a failed | Inspect with |
|---|---|
| `agent.settings.set`, `agent.key.set`, `agent.key.clear` | `agent.settings.get` (`settings`, `hasKey`) |
| `agent.binding.set`, `agent.binding.clear` | `agent.binding.candidates` (`bound`) and `agent.status.get` |
| `agent.packages.set` | `agent.packages.get` (`enabled`, `enabledMajor`) |

Send the request again only if the state shows it did not apply. A `busy` refusal is certain and did not run: another operator request holds this Package Instance, so try again once it finishes.

The Agent's own error codes are stable and namespaced (`agent.*`). Its `retryable` flag says whether the same request may succeed later. It is not permission to replay a mutation whose outcome is uncertain.

## Change or remove the configuration

- **Rotate the key:** repeat step 4. Clear it with `printf '{}\n' | agent_request agent.key.clear -`.
- **Unbind:** `printf '{}\n' | agent_request agent.binding.clear -`. This stops replies and cancels in-flight work.
- **After a Package reload:** rediscover operations. The helper re-reads the digest for you.
- **Remove the Package:** `"$APP" --data-dir "$DATA" packages remove --package "$PACKAGE"`. The Package's data, including settings, key and reply progress, is kept for a later reinstall.

## Limits

- Headless operation is request/response only. The Agent's direct chat and its live status and chat subscriptions are desktop UI features and are not available.
- Configuration requests carry at most 256 KiB.
- Replies, model calls and Port tools run in the background with no UI mounted. The Agent's turn budgets and post-once rules are the same as on the desktop.
