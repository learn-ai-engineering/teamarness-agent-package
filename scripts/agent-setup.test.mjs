import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { chmod, mkdir, mkdtemp, readFile, stat, symlink, writeFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const setup = join(root, "headless/agent-setup");
const fakeHost = join(root, "scripts/fixtures/fake-teamarness-headless");
const WORKSPACE = "aad43be122caa818738f763c373ba820";
const KEY = "sk-or-test-SECRET-4f9c1e";
const MUTATIONS = ["packages install", "agent add", "agent.settings.set", "agent.key.set", "agent.binding.set", "agent.packages.set"];

const offered = () => [
  { available: true, contract: "dev.teamarness.backlog", enabled: false, enabledMajor: null, major: 1, name: "Backlog" },
  { available: true, contract: "dev.teamarness.conversations", enabled: false, enabledMajor: null, major: 1, name: "Conversations" },
  { available: true, contract: "dev.teamarness.documents", enabled: false, enabledMajor: null, major: 1, name: "Documents" },
  { available: true, contract: "dev.teamarness.exhibits", enabled: false, enabledMajor: null, major: 1, name: "Exhibits" },
];

async function fixture({ state = {}, answers = {}, key = KEY, keyMode = 0o600 } = {}) {
  const dir = await mkdtemp(join(tmpdir(), "agent-setup-test-"));
  const dataDir = join(dir, "data");
  const packageDir = join(dir, "packages/dev.teamarness.agent");
  await mkdir(packageDir, { recursive: true });
  const keyFile = join(dir, "api-key");
  if (key !== null) {
    await writeFile(keyFile, `${key}\n`, { mode: keyMode });
    await chmod(keyFile, keyMode);
  }
  const initial = {
    dataDir, readiness: "ready", workspaces: [WORKSPACE], registrations: [], agents: [],
    settings: { endpointBaseUrl: "", model: "", systemPrompt: "", contextMessages: 20 },
    hasKey: false, bound: null, packages: offered(), fail: {}, ...state,
  };
  await writeFile(join(dir, "state.json"), JSON.stringify(initial));
  const values = {
    host_app: fakeHost, host_data_dir: dataDir, package_dir: packageDir, accept_native_code_risks: true,
    workspace_id: WORKSPACE, agent_name: "Factory", key_file: keyFile, ...answers,
  };
  const toml = Object.entries(values)
    .filter(([, value]) => value !== undefined)
    .map(([name, value]) => `${name} = ${JSON.stringify(value)}`)
    .join("\n");
  const model = answers.model ?? `endpoint_base_url = "https://openrouter.ai/api/v1"\nmodel = "z-ai/glm-5.3-flash"\nsystem_prompt = "Be brief."\ncontext_messages = 20`;
  const packages = answers.packages ?? `enable = ["dev.teamarness.conversations", "dev.teamarness.documents"]`;
  const answersFile = join(dir, "answers.toml");
  const extra = answers.extra ?? "";
  await writeFile(answersFile, `${toml.replace(/^(model|packages|extra) = .*$/gm, "")}\n${extra}\n[model]\n${model}\n\n[packages]\n${packages}\n`);
  return { dir, keyFile, answersFile, packageDir };
}

function run(f, { input } = {}) {
  const result = spawnSync("bash", [setup, "--answers", f.answersFile], {
    env: { ...process.env, FAKE_HOST_STATE: join(f.dir, "state.json"), FAKE_HOST_LOG: join(f.dir, "argv.log") },
    input: input ?? "",
    encoding: "utf8",
  });
  return { code: result.status, stdout: result.stdout, stderr: result.stderr };
}

async function state(f) {
  return JSON.parse(await readFile(join(f.dir, "state.json"), "utf8"));
}

async function calls(f) {
  if (!existsSync(join(f.dir, "argv.log"))) return [];
  return (await readFile(join(f.dir, "argv.log"), "utf8")).trim().split("\n").filter(Boolean).map((line) => JSON.parse(line));
}

const nameOf = (argv) => {
  const command = argv.slice(2);
  if (command[0] === "package" && command[1] === "request") return command[command.indexOf("--operation") + 1];
  return command.slice(0, 2).join(" ");
};

async function mutations(f) {
  return (await calls(f)).map(nameOf).filter((name) => MUTATIONS.includes(name));
}

test("a fresh install converges to a ready Agent and deletes the key file", async () => {
  const f = await fixture();
  const result = run(f);
  assert.equal(result.code, 0, result.stdout + result.stderr);
  assert.match(result.stdout, /^changed: package installed from /m);
  assert.match(result.stdout, /^changed: agent Factory created$/m);
  assert.match(result.stdout, /^changed: settings \(/m);
  assert.match(result.stdout, /^changed: api key \(key file .* deleted\)$/m);
  assert.match(result.stdout, /^changed: binding to Factory$/m);
  assert.match(result.stdout, /^changed: package dev\.teamarness\.conversations enabled at major 1$/m);
  assert.match(result.stdout, /^changed: package dev\.teamarness\.documents enabled at major 1$/m);
  assert.match(result.stdout, /^ready: agent Factory in Workspace /m);
  assert.match(result.stdout, /^summary: 7 changed, 1 unchanged$/m);
  const after = await state(f);
  assert.equal(after.key, KEY);
  assert.equal(after.settings.model, "z-ai/glm-5.3-flash");
  assert.equal(after.packages.find((p) => p.contract === "dev.teamarness.exhibits").enabled, false);
  assert.equal(existsSync(f.keyFile), false);
});

test("a rerun with the same answers changes nothing", async () => {
  const f = await fixture();
  assert.equal(run(f).code, 0);
  const before = (await calls(f)).length;
  const result = run(f);
  assert.equal(result.code, 0, result.stdout);
  assert.doesNotMatch(result.stdout, /^changed:/m);
  assert.match(result.stdout, /^unchanged: api key \(already set; key file absent\)$/m);
  assert.match(result.stdout, /^summary: 0 changed, 8 unchanged$/m);
  const rerun = (await calls(f)).slice(before).map(nameOf);
  assert.deepEqual(rerun.filter((name) => MUTATIONS.includes(name)), []);
});

test("a changed answer changes only that item", async () => {
  const f = await fixture();
  assert.equal(run(f).code, 0);
  const answers = await readFile(f.answersFile, "utf8");
  await writeFile(f.answersFile, answers.replace("context_messages = 20", "context_messages = 5"));
  const result = run(f);
  assert.equal(result.code, 0, result.stdout);
  assert.match(result.stdout, /^changed: settings \(contextMessages\)$/m);
  assert.match(result.stdout, /^summary: 1 changed, 7 unchanged$/m);
});

test("the API key never appears in argv, output or the answers file", async () => {
  const f = await fixture();
  const result = run(f);
  assert.equal(result.code, 0);
  assert.equal((result.stdout + result.stderr).includes(KEY), false);
  assert.equal((await readFile(join(f.dir, "argv.log"), "utf8")).includes(KEY), false);
  assert.equal((await readFile(f.answersFile, "utf8")).includes(KEY), false);
});

test('key_file "-" reads the key from stdin only when no key is set', async () => {
  const f = await fixture({ answers: { key_file: "-" }, key: null });
  const first = run(f, { input: `${KEY}\n` });
  assert.equal(first.code, 0, first.stdout);
  assert.match(first.stdout, /^changed: api key$/m);
  assert.equal((await state(f)).key, KEY);
  const second = run(f, { input: "sk-other\n" });
  assert.match(second.stdout, /^unchanged: api key \(already set; stdin not read\)$/m);
  assert.equal((await state(f)).key, KEY);
});

for (const [name, answers, message] of [
  ["an unknown key", { extra: "colour = \"blue\"\n" }, /unknown answers key: colour/],
  ["a workspace name in workspace_id", { workspace_id: "headless" }, /workspace_id must be/],
  ["both workspace and workspace_id", { workspace: "headless" }, /set exactly one of workspace/],
  ["context_messages out of range", { model: `endpoint_base_url = "https://x"\nmodel = "m"\nsystem_prompt = ""\ncontext_messages = 65` }, /context_messages must be/],
  ["an unknown model key", { model: `endpoint_base_url = "https://x"\nmodel = "m"\nsystem_prompt = ""\ncontext_messages = 1\ntemperature = 1` }, /unknown answers key: model\.temperature/],
  ["an empty agent name", { agent_name: "" }, /agent_name must not be empty/],
  ["a string consent", { accept_native_code_risks: "yes" }, /accept_native_code_risks must be true or false/],
]) {
  test(`${name} is refused with exit 2 before any Host call`, async () => {
    const f = await fixture({ answers });
    const result = run(f);
    assert.equal(result.code, 2, result.stdout);
    assert.match(result.stdout, message);
    assert.match(result.stdout, /^summary: nothing changed$/m);
    assert.deepEqual(await calls(f), []);
  });
}

test("a group-readable or symlinked key file is refused before any Host call", async () => {
  const readable = await fixture({ keyMode: 0o640 });
  const first = run(readable);
  assert.equal(first.code, 2);
  assert.match(first.stdout, /chmod 600/);
  assert.deepEqual(await calls(readable), []);

  const linked = await fixture({ key: null });
  const target = join(linked.dir, "real-key");
  await writeFile(target, KEY, { mode: 0o600 });
  await symlink(target, linked.keyFile);
  const second = run(linked);
  assert.equal(second.code, 2);
  assert.match(second.stdout, /must not be a symlink/);
  assert.deepEqual(await calls(linked), []);
});

test("installing requires accept_native_code_risks = true", async () => {
  const f = await fixture({ answers: { accept_native_code_risks: false } });
  const result = run(f);
  assert.equal(result.code, 2);
  assert.match(result.stdout, /^stopped: the Agent Package is not installed\. To install it, set accept_native_code_risks = true/m);
  assert.deepEqual(await mutations(f), []);
});

test("an uncertain mutation stops at once with an inspect command and is not retried", async () => {
  const f = await fixture({ state: { fail: { "agent.settings.set": "uncertain" } } });
  const result = run(f);
  assert.equal(result.code, 1);
  assert.match(
    result.stdout,
    /^stopped: agent\.settings\.set outcome unknown \(host_failure, agent\.storage_failed\) — inspect with: printf .* --operation agent\.settings\.get .*, then rerun setup$/m,
  );
  assert.match(result.stdout, /^summary: 2 changed, 1 unchanged, stopped$/m);
  const names = await mutations(f);
  assert.equal(names.filter((name) => name === "agent.settings.set").length, 1);
  assert.equal(names.includes("agent.key.set"), false);
  assert.equal(existsSync(f.keyFile), true);
});

test("a certain refusal says nothing changed and exits 1", async () => {
  const f = await fixture({ state: { fail: { "agent add": "busy" } } });
  const result = run(f);
  assert.equal(result.code, 1);
  assert.match(result.stdout, /^stopped: agent add refused, nothing changed \(busy\)$/m);
});

test("a different bound Agent stops setup with the unbind command", async () => {
  const other = { id: "c".repeat(64), name: "Scout" };
  const f = await fixture({
    state: { registrations: [], agents: [other], bound: other.id, hasKey: true },
    key: null,
  });
  const result = run(f);
  assert.equal(result.code, 1);
  assert.match(result.stdout, /^stopped: Agent 'Scout' is bound, not 'Factory'\. Setup does not rebind, .* --operation agent\.binding\.clear /m);
  assert.equal((await mutations(f)).includes("agent.binding.set"), false);
  assert.equal((await state(f)).bound, other.id);
});

test("a registration from another directory stops setup", async () => {
  const f = await fixture({ state: { registrations: [{ directory: "/elsewhere/dev.teamarness.agent" }] } });
  const result = run(f);
  assert.equal(result.code, 1);
  assert.match(result.stdout, /^stopped: the Agent Package is registered from \/elsewhere\/dev\.teamarness\.agent, not /m);
  assert.deepEqual(await mutations(f), []);
});

test("a Workspace the Installation has not joined stops setup with exit 2", async () => {
  const f = await fixture({ state: { workspaces: ["0".repeat(32)] } });
  const result = run(f);
  assert.equal(result.code, 2);
  assert.match(result.stdout, /^stopped: this Installation has not joined a Workspace named or with ID '[0-9a-f]{32}'\. Joined Workspaces: 'w0' \(0{32}\)\./m);
  assert.deepEqual(await mutations(f), []);
});

test("workspace accepts the Workspace name and resolves it to its ID", async () => {
  const other = "1".repeat(32);
  const f = await fixture({
    state: { workspaces: [other, WORKSPACE], workspaceNames: ["numbertwo", "headless"] },
    answers: { workspace_id: undefined, workspace: "headless" },
  });
  const result = run(f);
  assert.equal(result.code, 0, result.stdout + result.stderr);
  assert.match(result.stdout, new RegExp(`^unchanged: host ready, Workspace headless \\(${WORKSPACE}\\) joined$`, "m"));
  assert.match(result.stdout, new RegExp(`^ready: agent Factory in Workspace ${WORKSPACE}$`, "m"));
  const requests = (await calls(f)).filter((argv) => argv.includes("--workspace"));
  assert.ok(requests.length > 0);
  assert.ok(requests.every((argv) => argv[argv.indexOf("--workspace") + 1] === WORKSPACE));
});

test("workspace also accepts an ID", async () => {
  const f = await fixture({ answers: { workspace_id: undefined, workspace: WORKSPACE } });
  assert.equal(run(f).code, 0);
});

test("a Workspace name shared by several Workspaces stops with exit 2 and lists their IDs", async () => {
  const other = "1".repeat(32);
  const f = await fixture({
    state: { workspaces: [WORKSPACE, other], workspaceNames: ["team", "team"] },
    answers: { workspace_id: undefined, workspace: "team" },
  });
  const result = run(f);
  assert.equal(result.code, 2);
  assert.match(result.stdout, new RegExp(`^stopped: 2 joined Workspaces are named 'team': 'team' \\(${WORKSPACE}\\), 'team' \\(${other}\\)\\. Set workspace to one of these IDs`, "m"));
  assert.deepEqual(await mutations(f), []);
});

test("--print-template writes a valid, private template and never overwrites", async () => {
  const dir = await mkdtemp(join(tmpdir(), "agent-setup-template-"));
  const path = join(dir, "agent.toml");
  const first = spawnSync("bash", [setup, "--print-template", path], { encoding: "utf8" });
  assert.equal(first.status, 0, first.stderr);
  assert.equal((await stat(path)).mode & 0o777, 0o600);
  const template = await readFile(path, "utf8");
  assert.match(template, /^accept_native_code_risks = false$/m);
  assert.match(template, /^host_app = "~\/\.local\/lib\/teamarness-headless\/teamarness-headless"$/m);
  assert.match(template, /^host_data_dir = "~\/\.local\/share\/teamarness-headless"$/m);
  const parsed = spawnSync("python3", ["-I", "-c", "import sys,tomllib,json;print(json.dumps(tomllib.load(open(sys.argv[1],'rb'))))", path], { encoding: "utf8" });
  assert.equal(parsed.status, 0, parsed.stderr);
  assert.deepEqual(Object.keys(JSON.parse(parsed.stdout)).sort(), [
    "accept_native_code_risks", "agent_name", "host_app", "host_data_dir", "key_file", "model", "package_dir", "packages", "workspace",
  ]);
  const again = spawnSync("bash", [setup, "--print-template", path], { encoding: "utf8" });
  assert.equal(again.status, 2);
  assert.match(again.stderr, /already exists/);
  assert.equal(await readFile(path, "utf8"), template);

  assert.deepEqual(JSON.parse(parsed.stdout).packages.enable, [
    "dev.teamarness.backlog", "dev.teamarness.conversations", "dev.teamarness.documents", "dev.teamarness.exhibits",
  ]);
  assert.match(template, /^# Remove the Packages this Agent should not access\.$/m);

  const stdout = spawnSync("bash", [setup, "--print-template"], { encoding: "utf8" });
  assert.equal(stdout.stdout, template);
});

test("an unfilled template is refused with exit 2", async () => {
  const dir = await mkdtemp(join(tmpdir(), "agent-setup-template-"));
  const path = join(dir, "agent.toml");
  spawnSync("bash", [setup, "--print-template", path]);
  const result = spawnSync("bash", [setup, "--answers", path], { encoding: "utf8" });
  assert.equal(result.status, 2);
  assert.match(result.stdout, /^invalid: /m);
});

test("the template's full Package list enables every offered Package", async () => {
  const f = await fixture({ answers: { packages: `enable = [\n  "dev.teamarness.backlog",\n  "dev.teamarness.conversations",\n  "dev.teamarness.documents",\n  "dev.teamarness.exhibits",\n]` } });
  const result = run(f);
  assert.equal(result.code, 0, result.stdout + result.stderr);
  assert.match(result.stdout, /^summary: 9 changed, 1 unchanged$/m);
  assert.ok((await state(f)).packages.every((p) => p.enabled === true && p.enabledMajor === 1));
});

test("a listed Package that is missing or unavailable stops before any Package is enabled", async () => {
  const packages = offered().filter((p) => p.contract !== "dev.teamarness.backlog");
  packages.find((p) => p.contract === "dev.teamarness.exhibits").available = false;
  const f = await fixture({
    state: { packages },
    answers: { packages: `enable = ["dev.teamarness.conversations", "dev.teamarness.backlog", "dev.teamarness.exhibits"]` },
  });
  const result = run(f);
  assert.equal(result.code, 1, result.stdout);
  assert.match(result.stdout, /^stopped: packages \(not installed in this Workspace: dev\.teamarness\.backlog; installed but not available now: dev\.teamarness\.exhibits\)\. No Package was changed\. Remove them from \[packages\] enable, or install them, then rerun setup\. Inspect with: .*agent\.packages\.get/m);
  assert.match(result.stdout, /^summary: \d+ changed, \d+ unchanged, stopped$/m);
  assert.doesNotMatch(result.stdout, /^ready:/m);
  assert.ok(!(await mutations(f)).includes("agent.packages.set"));
  assert.ok((await state(f)).packages.every((p) => p.enabled === false));
});

test("the enable list is authoritative: a removed Package is turned off, and a rerun changes nothing", async () => {
  const all = `enable = ["dev.teamarness.backlog", "dev.teamarness.conversations", "dev.teamarness.documents", "dev.teamarness.exhibits"]`;
  const f = await fixture({ answers: { packages: all } });
  assert.equal(run(f).code, 0);
  const answers = await readFile(f.answersFile, "utf8");
  await writeFile(f.answersFile, answers.replace(all, `enable = ["dev.teamarness.conversations", "dev.teamarness.documents"]`));
  const before = (await calls(f)).length;
  const removed = run(f);
  assert.equal(removed.code, 0, removed.stdout + removed.stderr);
  assert.match(removed.stdout, /^changed: package dev\.teamarness\.backlog turned off \(not listed\)$/m);
  assert.match(removed.stdout, /^changed: package dev\.teamarness\.exhibits turned off \(not listed\)$/m);
  assert.match(removed.stdout, /^summary: 2 changed, \d+ unchanged$/m);
  const sets = (await calls(f)).slice(before).filter((argv) => nameOf(argv) === "agent.packages.set");
  assert.equal(sets.length, 2);
  const enabled = (await state(f)).packages.filter((p) => p.enabled).map((p) => p.contract).sort();
  assert.deepEqual(enabled, ["dev.teamarness.conversations", "dev.teamarness.documents"]);
  const again = run(f);
  assert.equal(again.code, 0);
  assert.match(again.stdout, /^summary: 0 changed, \d+ unchanged$/m);
  assert.doesNotMatch(again.stdout, /^changed:/m);
});

test("a Package enabled elsewhere is turned off, and a typo in the list changes nothing", async () => {
  const packages = offered();
  packages.find((p) => p.contract === "dev.teamarness.exhibits").enabled = true;
  packages.find((p) => p.contract === "dev.teamarness.exhibits").enabledMajor = 1;
  const f = await fixture({ state: { packages }, answers: { packages: `enable = ["dev.teamarness.conversation"]` } });
  const typo = run(f);
  assert.equal(typo.code, 1);
  assert.match(typo.stdout, /not installed in this Workspace: dev\.teamarness\.conversation\)\. No Package was changed\./);
  assert.ok(!(await mutations(f)).includes("agent.packages.set"));
  assert.equal((await state(f)).packages.find((p) => p.contract === "dev.teamarness.exhibits").enabled, true);
});
