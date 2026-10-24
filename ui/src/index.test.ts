// @vitest-environment happy-dom

import type { PackageValue } from "@teamarness/package-ui-sdk";
import {
  createFakePackageClient,
  createPackageRequestError,
  createTestMountContext,
} from "@teamarness/package-ui-sdk/testing";
import { describe, expect, it, vi } from "vitest";

import type { Settings, Status } from "./contract.generated";
import { createAgentController, describeStatus, mount, type AgentUiState } from "./index";

const AGENT_ID = "a".repeat(64);
const OTHER_ID = "c".repeat(64);
const SECRET = "sk-or-v1-SENTINEL";

function defaults(): Settings {
  return { endpointBaseUrl: "https://openrouter.ai/api/v1", model: "", systemPrompt: "", contextMessages: 20 };
}

/** A fake Backend with the same state machine the real one has, enough for the UI to be driven. */
function fakeBackend(input?: { status?: Status; candidates?: Array<{ principalId: string; name: string | null; bound: boolean }> }) {
  let settings = defaults();
  let hasKey = false;
  let status: Status = input?.status ?? { state: "unconfigured" };
  const candidates = input?.candidates ?? [{ principalId: AGENT_ID, name: "Scout", bound: false }];
  const fake = createFakePackageClient({
    requests: {
      "agent.settings.get": () => ({ settings, hasKey }),
      "agent.settings.set": (payload) => {
        const next = (payload as { settings: Settings }).settings;
        if (!next.endpointBaseUrl.startsWith("http")) {
          throw createPackageRequestError({
            stage: "ui",
            packageError: { code: "agent.invalid_settings", details: { field: "endpointBaseUrl" } },
          });
        }
        settings = next;
        return { settings, hasKey };
      },
      "agent.key.set": () => {
        hasKey = true;
        return { hasKey };
      },
      "agent.key.clear": () => {
        hasKey = false;
        return { hasKey };
      },
      "agent.binding.candidates": () => ({ agents: candidates }),
      "agent.binding.set": (payload) => {
        const id = (payload as { principalId: string }).principalId;
        const found = candidates.find((candidate) => candidate.principalId === id);
        if (!found) {
          throw createPackageRequestError({ stage: "ui", packageError: { code: "agent.not_stewarded" } });
        }
        for (const candidate of candidates) candidate.bound = candidate.principalId === id;
        status = { state: "ready", agent: { principalId: id, name: found.name } };
        return { agent: { principalId: id, name: found.name } };
      },
      "agent.binding.clear": () => {
        for (const candidate of candidates) candidate.bound = false;
        status = { state: "no_agent" };
        return { agent: null };
      },
      "agent.status.get": () => status,
    },
    subscriptions: { "agent.status.changes": () => undefined },
  });
  return { fake, client: fake.client, secretSeen: () => fake.calls.some((call) => JSON.stringify(call.payload).includes(SECRET)) };
}

function lastState(states: AgentUiState[]): AgentUiState {
  const state = states[states.length - 1];
  if (!state) throw new Error("no state presented");
  return state;
}

describe("Agent Package UI controller", () => {
  it("loads settings, status and candidates, then follows status changes", async () => {
    const { fake, client } = fakeBackend();
    const states: AgentUiState[] = [];
    const controller = createAgentController(client, (state) => { states.push(state); });
    await controller.start();

    const operations = fake.calls.map((call) => call.operation);
    expect(operations).toContain("agent.settings.get");
    expect(operations).toContain("agent.status.get");
    expect(operations).toContain("agent.binding.candidates");
    expect(operations).toContain("agent.status.changes");
    const state = lastState(states);
    expect(state.settings?.contextMessages).toBe(20);
    expect(state.hasKey).toBe(false);
    expect(state.status).toEqual({ state: "unconfigured" });
    expect(state.candidates).toHaveLength(1);

    await fake.emit("agent.status.changes", { state: "no_agent" });
    expect(lastState(states).status).toEqual({ state: "no_agent" });
    controller.dispose();
  });

  it("saves settings, stores the key without keeping it, and reports invalid fields", async () => {
    const { fake, client, secretSeen } = fakeBackend();
    const states: AgentUiState[] = [];
    const controller = createAgentController(client, (state) => { states.push(state); });
    await controller.start();

    await controller.saveSettings({ ...defaults(), model: "deepseek/deepseek-chat", contextMessages: 8 });
    expect(lastState(states).settings?.model).toBe("deepseek/deepseek-chat");
    expect(lastState(states).notice).toBe("Settings saved.");
    expect(lastState(states).error).toBeUndefined();

    await controller.setKey(SECRET);
    expect(secretSeen()).toBe(true);
    expect(lastState(states).hasKey).toBe(true);
    expect(JSON.stringify(lastState(states))).not.toContain(SECRET);

    await controller.saveSettings({ ...defaults(), endpointBaseUrl: "ftp://nope" });
    expect(lastState(states).error).toBe("Check the endpointBaseUrl field.");
    expect(lastState(states).settings?.endpointBaseUrl).toBe("https://openrouter.ai/api/v1");

    await controller.clearKey();
    expect(lastState(states).hasKey).toBe(false);
    const keyCalls = fake.calls.filter((call) => call.operation === "agent.key.set");
    expect(keyCalls).toHaveLength(1);
    controller.dispose();
  });

  it("binds and clears the Agent and shows refusals", async () => {
    const { client } = fakeBackend();
    const states: AgentUiState[] = [];
    const controller = createAgentController(client, (state) => { states.push(state); });
    await controller.start();

    await controller.bind(AGENT_ID);
    expect(lastState(states).status).toEqual({ state: "ready", agent: { principalId: AGENT_ID, name: "Scout" } });
    expect(lastState(states).candidates[0]?.bound).toBe(true);

    await controller.bind(OTHER_ID);
    expect(lastState(states).error).toBe("agent.not_stewarded");

    await controller.unbind();
    expect(lastState(states).status).toEqual({ state: "no_agent" });
    expect(lastState(states).candidates[0]?.bound).toBe(false);
    controller.dispose();
  });

  it("ignores updates after dispose", async () => {
    const { fake, client } = fakeBackend();
    const states: AgentUiState[] = [];
    const controller = createAgentController(client, (state) => { states.push(state); });
    await controller.start();
    controller.dispose();
    const count = states.length;
    await fake.emit("agent.status.changes", { state: "ready" });
    await controller.saveSettings(defaults());
    expect(states).toHaveLength(count);
  });

  it("describes every status state without exposing details", () => {
    expect(describeStatus({ state: "unconfigured" })).toBe("Not configured: add the endpoint, model and API key.");
    expect(describeStatus({ state: "no_agent" })).toBe("No Agent: choose one of your Agents below.");
    expect(describeStatus({ state: "ready", agent: { principalId: AGENT_ID, name: "Scout" } })).toBe("Ready: Scout replies when addressed.");
    expect(describeStatus({ state: "ready", agent: { principalId: AGENT_ID, name: null } })).toBe("Ready: the Agent replies when addressed.");
    expect(describeStatus({ state: "replying" })).toBe("Replying...");
    expect(describeStatus({ state: "error", lastError: { code: "agent.endpoint_rejected", retryable: false } })).toBe("Error: agent.endpoint_rejected");
    expect(describeStatus(null)).toBe("Loading...");
  });
});

describe("Agent Package UI mount", () => {
  it("separates configuration sections and themes the native Agent picker", async () => {
    const { client } = fakeBackend();
    const host = document.createElement("div");
    const root = host.attachShadow({ mode: "open" });
    const sheets = new WeakMap<CSSStyleSheet, string>();
    const replaceSync = CSSStyleSheet.prototype.replaceSync;
    const spy = vi.spyOn(CSSStyleSheet.prototype, "replaceSync").mockImplementation(function (this: CSSStyleSheet, css) {
      sheets.set(this, css);
      replaceSync.call(this, css);
    });
    let unmount: Awaited<ReturnType<typeof mount>> | undefined;
    try {
      unmount = await mount(createTestMountContext({ root, client }));
      const form = root.querySelector("form.agent-settings");
      expect(form?.querySelectorAll(":scope > section")).toHaveLength(4);
      const picker = form?.querySelector<HTMLSelectElement>('select[name="agent"]');
      expect(picker?.closest(".agent-picker")).not.toBeNull();
      expect(picker?.classList.contains("agent-picker-control")).toBe(true);
      expect(picker?.closest("label")?.textContent).toContain("Workspace Agent");
      const css = sheets.get(root.adoptedStyleSheets.at(-1)!) ?? "";
      expect(css).toMatch(/\.agent-settings\s*\{[^}]*display:\s*grid[^}]*gap:\s*1\.25rem/s);
      expect(css).toMatch(/\.agent-picker-control\s*\{[^}]*appearance:\s*none/s);
      expect(css).toContain("var(--teamarness-color-text)");
      expect(css).not.toContain("display: contents");
    } finally {
      await unmount?.();
      spy.mockRestore();
    }
  });

  it("renders one settings form in the Shadow Root, drives the Backend and unmounts cleanly", async () => {
    const { fake, client } = fakeBackend();
    const host = document.createElement("div");
    document.body.append(host);
    const root = host.attachShadow({ mode: "open" });
    const context = createTestMountContext({ root, client, displayName: "Agent" });

    const unmount = await mount(context);
    expect(root.querySelectorAll("form")).toHaveLength(1);
    expect(root.querySelector("h1")?.textContent).toBe("Agent");
    expect(root.querySelector('[data-role="status"]')?.textContent).toBe(
      "Not configured: add the endpoint, model and API key.",
    );
    const picker = root.querySelector<HTMLSelectElement>('select[name="agent"]');
    expect(picker).not.toBeNull();
    expect(Array.from(picker?.options ?? []).map((option) => option.value)).toEqual(["", AGENT_ID]);
    expect(root.querySelector<HTMLInputElement>('input[name="apiKey"]')?.type).toBe("password");

    const model = root.querySelector<HTMLInputElement>('input[name="model"]');
    if (!model) throw new Error("model field missing");
    model.value = "deepseek/deepseek-chat";
    root.querySelector<HTMLButtonElement>('button[data-action="save"]')?.click();
    await flush();
    const saved = fake.calls.find((call) => call.operation === "agent.settings.set");
    expect((saved?.payload as { settings: Settings }).settings.model).toBe("deepseek/deepseek-chat");

    const key = root.querySelector<HTMLInputElement>('input[name="apiKey"]');
    if (!key) throw new Error("key field missing");
    key.value = SECRET;
    root.querySelector<HTMLButtonElement>('button[data-action="set-key"]')?.click();
    await flush();
    expect(key.value).toBe("");
    expect(root.querySelector('[data-role="key-state"]')?.textContent).toBe("A key is stored.");
    expect(root.innerHTML).not.toContain(SECRET);

    if (picker) picker.value = AGENT_ID;
    root.querySelector<HTMLButtonElement>('button[data-action="bind"]')?.click();
    await flush();
    expect(root.querySelector('[data-role="status"]')?.textContent).toBe("Ready: Scout replies when addressed.");

    await fake.emit("agent.status.changes", { state: "error", lastError: { code: "agent.endpoint_rejected", retryable: false } } satisfies PackageValue);
    expect(root.querySelector('[data-role="status"]')?.textContent).toBe("Error: agent.endpoint_rejected");

    await unmount();
    await unmount();
    expect(root.childElementCount).toBe(0);
    host.remove();
  });
});

/** Lets a click's chained Backend round trips settle; the fake client answers in microtasks. */
async function flush(): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, 0));
  await new Promise((resolve) => setTimeout(resolve, 0));
}
