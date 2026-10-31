// @vitest-environment happy-dom

import type { PackageValue } from "@teamarness/package-ui-sdk";
import {
  createFakePackageClient,
  createPackageRequestError,
  createTestMountContext,
} from "@teamarness/package-ui-sdk/testing";
import { describe, expect, it, vi } from "vitest";

import type { Chat, PackageView, Settings, Status } from "./contract.generated";
import { createAgentController, describeChatError, describeStatus, mount, type AgentUiState } from "./index";

const AGENT_ID = "a".repeat(64);
const OTHER_ID = "c".repeat(64);
const SECRET = "sk-or-v1-SENTINEL";

function defaults(): Settings {
  return { endpointBaseUrl: "https://openrouter.ai/api/v1", model: "", systemPrompt: "", contextMessages: 20 };
}

/** A fake Backend with the same state machine the real one has, enough for the UI to be driven. */
function fakeBackend(input?: {
  status?: Status;
  candidates?: Array<{ principalId: string; name: string | null; bound: boolean }>;
  packages?: PackageView[];
  /** Every `agent.packages.set` fails with this code. */
  refusePackageSet?: string;
  refuseBinding?: string;
  /** Every `agent.packages.set` waits for this before it answers. */
  holdPackageSet?: Promise<void>;
  holdFirstPackageGet?: Promise<void>;
  /** Every `agent.packages.get` after the first fails with this code. */
  failLaterPackageGets?: string;
  /** The draft the Backend remembers at mount. */
  draft?: string;
  /** Every `agent.chat.draft.get` fails. */
  refuseDraftGet?: boolean;
  /** Every `agent.chat.draft.get` waits for this before it answers. */
  holdDraftGet?: Promise<void>;
  /** Every `agent.chat.send` fails with this code. */
  refuseSend?: string;
}) {
  let settings = defaults();
  let hasKey = false;
  let status: Status = input?.status ?? { state: "unconfigured" };
  const candidates = input?.candidates ?? [{ principalId: AGENT_ID, name: "Scout", bound: false }];
  let chat: Chat = { entries: [], pending: false };
  let packages: PackageView[] = input?.packages ?? [];
  let packageGets = 0;
  let draft = input?.draft ?? "";
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
        if (input?.refuseBinding) throw createPackageRequestError({ stage: "ui", packageError: { code: input.refuseBinding } });
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
      "agent.chat.get": () => chat,
      "agent.chat.draft.get": async () => {
        await input?.holdDraftGet;
        if (input?.refuseDraftGet) throw createPackageRequestError({ stage: "ui", packageError: { code: "agent.storage_failed", retryable: true } });
        return { text: draft };
      },
      "agent.chat.draft.set": (payload) => {
        draft = (payload as { text: string }).text;
        return {};
      },
      "agent.chat.send": (payload) => {
        if (input?.refuseSend) throw createPackageRequestError({ stage: "ui", packageError: { code: input.refuseSend, retryable: true } });
        if (status.state === "unconfigured") {
          throw createPackageRequestError({ stage: "ui", packageError: { code: "agent.not_configured" } });
        }
        if (chat.pending) {
          throw createPackageRequestError({ stage: "ui", packageError: { code: "agent.chat_busy", retryable: true } });
        }
        const content = (payload as { content: string }).content;
        chat = { entries: [...chat.entries, { id: chat.entries.length + 1, role: "user", content }], pending: true };
        return chat;
      },
      "agent.chat.clear": () => {
        chat = { entries: [], pending: false };
        return chat;
      },
      "agent.packages.get": async () => {
        packageGets += 1;
        if (input?.failLaterPackageGets && packageGets > 1) {
          throw createPackageRequestError({ stage: "ui", packageError: { code: input.failLaterPackageGets, retryable: true } });
        }
        const snapshot = packages;
        if (packageGets === 1) await input?.holdFirstPackageGet;
        return { packages: snapshot };
      },
      "agent.packages.set": async (payload) => {
        const { contract, enabled } = payload as { contract: string; enabled: boolean };
        await input?.holdPackageSet;
        if (input?.refusePackageSet) {
          throw createPackageRequestError({ stage: "ui", packageError: { code: input.refusePackageSet, retryable: true } });
        }
        if (!packages.some((entry) => entry.contract === contract)) {
          throw createPackageRequestError({ stage: "ui", packageError: { code: "agent.unknown_package" } });
        }
        packages = packages.map((entry) => (entry.contract === contract ? { ...entry, enabled, enabledMajor: enabled ? entry.major : null } : entry));
        return { packages };
      },
    },
    subscriptions: { "agent.status.changes": () => undefined, "agent.chat.changes": () => undefined },
  });
  /** What the Backend publishes when the model answers the turn in flight. */
  const reply = async (content: string): Promise<void> => {
    chat = { entries: [...chat.entries, { id: chat.entries.length + 1, role: "assistant", content }], pending: false };
    await fake.emit("agent.chat.changes", chat satisfies PackageValue);
  };
  return { fake, client: fake.client, reply, draft: () => draft, updatePackages: (next: PackageView[]) => { packages = next; }, secretSeen: () => fake.calls.some((call) => JSON.stringify(call.payload).includes(SECRET)) };
}

function lastState(states: AgentUiState[]): AgentUiState {
  const state = states[states.length - 1];
  if (!state) throw new Error("no state presented");
  return state;
}

describe("Agent Package UI controller", () => {
  it.each([
    ["agent.binding_busy", "A command or binding change is still pending. Wait for it to settle, then choose the binding again."],
    ["agent.binding_uncertain", "An earlier command or binding change has an unknown outcome. Binding changes are blocked. Do not replay it; keep this identity and seek Host recovery assistance. Restart safety has not been qualified."],
  ])("shows safe binding refusal guidance for %s without changing identity", async (code, guidance) => {
    const status: Status = { state: "ready", agent: { principalId: AGENT_ID, name: "Scout" } };
    const { client } = fakeBackend({ status, refuseBinding: code });
    const states: AgentUiState[] = [];
    const controller = createAgentController(client, (state) => { states.push(state); }, "settings");
    await controller.start();
    await controller.bind(OTHER_ID);
    expect(lastState(states).error).toBe(guidance);
    expect(lastState(states).status).toEqual(status);
    expect(lastState(states).busy).toBe(false);
    controller.dispose();
  });
  it("settings loads settings, status, candidates and Packages, then follows status changes", async () => {
    const { fake, client } = fakeBackend();
    const states: AgentUiState[] = [];
    const controller = createAgentController(client, (state) => { states.push(state); }, "settings");
    await controller.start();

    const operations = fake.calls.map((call) => call.operation);
    expect(operations).toEqual([
      "agent.settings.get",
      "agent.status.get",
      "agent.binding.candidates",
      "agent.status.changes",
      "agent.packages.get",
    ]);
    const state = lastState(states);
    expect(state.chat).toBeNull();
    expect(state.settings?.contextMessages).toBe(20);
    expect(state.hasKey).toBe(false);
    expect(state.status).toEqual({ state: "unconfigured" });
    expect(state.candidates).toHaveLength(1);

    await fake.emit("agent.status.changes", { state: "no_agent" });
    expect(lastState(states).status).toEqual({ state: "no_agent" });
    controller.dispose();
  });

  it("main loads only the status and the chat, and follows both", async () => {
    const { fake, client } = fakeBackend();
    const states: AgentUiState[] = [];
    const controller = createAgentController(client, (state) => { states.push(state); }, "main");
    await controller.start();

    expect(fake.calls.map((call) => call.operation)).toEqual([
      "agent.chat.draft.get",
      "agent.status.get",
      "agent.chat.get",
      "agent.status.changes",
      "agent.chat.changes",
    ]);
    expect(lastState(states).settings).toBeNull();
    expect(lastState(states).packages).toBeNull();
    expect(lastState(states).chat).toEqual({ entries: [], pending: false });

    await fake.emit("agent.status.changes", { state: "no_agent" });
    expect(lastState(states).status).toEqual({ state: "no_agent" });
    await fake.emit("agent.chat.changes", { entries: [{ id: 1, role: "user", content: "hi" }], pending: true });
    expect(lastState(states).chat?.pending).toBe(true);
    controller.dispose();
  });

  it("saves settings, stores the key without keeping it, and reports invalid fields", async () => {
    const { fake, client, secretSeen } = fakeBackend();
    const states: AgentUiState[] = [];
    const controller = createAgentController(client, (state) => { states.push(state); }, "settings");
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
    const controller = createAgentController(client, (state) => { states.push(state); }, "settings");
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

  it("sends chat messages, follows replies and explains refusals", async () => {
    const { fake, client, reply } = fakeBackend({ status: { state: "ready", agent: { principalId: AGENT_ID, name: "Scout" } } });
    const states: AgentUiState[] = [];
    const controller = createAgentController(client, (state) => { states.push(state); }, "main");
    await controller.start();

    expect(await controller.sendChat("hello")).toBe(true);
    expect(lastState(states).chat).toEqual({ entries: [{ id: 1, role: "user", content: "hello" }], pending: true });
    expect(await controller.sendChat("again")).toBe(false);
    expect(lastState(states).chatError).toBe("Wait for the current reply before sending another message.");
    expect(lastState(states).error).toBeUndefined();

    await reply("Hi there");
    expect(lastState(states).chat?.entries.at(-1)).toEqual({ id: 2, role: "assistant", content: "Hi there" });
    expect(lastState(states).chat?.pending).toBe(false);

    await controller.clearChat();
    expect(lastState(states).chat).toEqual({ entries: [], pending: false });
    expect(lastState(states).chatError).toBeUndefined();
    expect(fake.calls.filter((call) => call.operation === "agent.chat.send")).toHaveLength(2);
    controller.dispose();
  });

  it("describes chat failures as sentences and passes unknown codes through", () => {
    expect(describeChatError("agent.not_configured")).toBe("Add the endpoint, model and API key in the Agent's Configuration on its Package page first.");
    expect(describeChatError("agent.endpoint_unreachable")).toBe("The endpoint could not be reached. Try again.");
    expect(describeChatError("agent.endpoint_timed_out")).toBe(
      "The model took longer than 5 minutes to answer. Try again, or ask for something smaller.",
    );
    expect(describeChatError("agent.chat_interrupted")).toBe("The reply was interrupted. Send the message again.");
    expect(describeChatError("agent.turn_limit_exceeded")).toBe("The reply needed more steps or time than one turn allows. Ask for something smaller.");
    expect(describeChatError("agent.something_new")).toBe("agent.something_new");
  });

  it("ignores updates after dispose", async () => {
    const { fake, client } = fakeBackend();
    const states: AgentUiState[] = [];
    const controller = createAgentController(client, (state) => { states.push(state); }, "settings");
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
    expect(describeStatus({ state: "ready", agent: { principalId: AGENT_ID, name: "Scout" } })).toBe("Ready: Scout.");
    expect(describeStatus({ state: "ready", agent: { principalId: AGENT_ID, name: null } })).toBe("Ready: the Agent is configured.");
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
      unmount = await mount(createTestMountContext({ root, client, view: "settings" }));
      const form = root.querySelector("form.agent-settings");
      expect(form?.querySelectorAll(":scope > section")).toHaveLength(5);
      const picker = form?.querySelector<HTMLSelectElement>('select[name="agent"]');
      expect(picker?.closest(".agent-picker")).not.toBeNull();
      expect(picker?.classList.contains("agent-picker-control")).toBe(true);
      expect(picker?.closest("label")?.textContent).toContain("Workspace Agent");
      const css = sheets.get(root.adoptedStyleSheets.at(-1)!) ?? "";
      // Sections are ruled groups, not boxed panels.
      expect(form?.querySelectorAll(".teamarness-section")).toHaveLength(0);
      expect(Array.from(form?.querySelectorAll(".agent-group-title") ?? []).map((heading) => heading.textContent)).toEqual(
        ["Status", "Agent", "Model endpoint", "API key", "Packages"],
      );
      expect(css).toMatch(/\.agent-group\s*\{[^}]*display:\s*grid[^}]*border-top:/s);
      expect(css).toMatch(/\.agent-picker-control\s*\{[^}]*appearance:\s*none/s);
      expect(css).toContain("var(--teamarness-color-text)");
      expect(css).not.toContain("display: contents");
      // Only the transcript scrolls; the chat page is pinned to the pane height.
      expect(css).toMatch(/\.agent-page\s*\{[^}]*width:\s*max\(75%[^}]*margin-inline:\s*auto/s);
      expect(css).toMatch(/\.agent-page\[data-view="main"\]\s*\{[^}]*height:\s*100vh/s);
      expect(css).not.toContain("agent-tab");
      expect(css).toMatch(/\.chat-log\s*\{[^}]*overflow-y:\s*auto/s);
      expect(css).toMatch(/\.chat-form\s*\{[^}]*margin-top:\s*auto/s);
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
    const context = createTestMountContext({ root, client, displayName: "Agent", view: "settings" });

    const unmount = await mount(context);
    expect(root.querySelectorAll("form.agent-settings")).toHaveLength(1);
    expect(root.querySelector("h1")).toBeNull();
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
    expect(root.querySelector('[data-role="status"]')?.textContent).toBe("Ready: Scout.");

    await fake.emit("agent.status.changes", { state: "error", lastError: { code: "agent.endpoint_rejected", retryable: false } } satisfies PackageValue);
    expect(root.querySelector('[data-role="status"]')?.textContent).toBe("Error: agent.endpoint_rejected");

    await unmount();
    await unmount();
    expect(root.childElementCount).toBe(0);
    host.remove();
  });
});

describe("Agent Package UI views and chat", () => {
  async function mounted(status?: Status, view: "main" | "settings" = "main") {
    const backend = fakeBackend(status ? { status } : undefined);
    const host = document.createElement("div");
    document.body.append(host);
    const root = host.attachShadow({ mode: "open" });
    const unmount = await mount(createTestMountContext({ root, client: backend.client, view }));
    const cleanup = async () => {
      await unmount();
      host.remove();
    };
    return { ...backend, root, chat: root, cleanup };
  }

  it("mounts the main view as the chat only, without configuration", async () => {
    const { fake, root, cleanup } = await mounted();
    expect(root.querySelector('[role="tab"], [role="tablist"]')).toBeNull();
    expect(root.querySelector("article.agent-page")?.getAttribute("data-view")).toBe("main");
    expect(root.querySelector("header.teamarness-page-header h1")?.textContent).toBe("Test Package");
    expect(root.querySelector('section.agent-chat textarea[name="chatMessage"]')).not.toBeNull();
    expect(root.querySelector("form.agent-settings")).toBeNull();
    for (const name of ["agent", "endpointBaseUrl", "model", "apiKey"]) {
      expect(root.querySelector(`[name="${name}"]`)).toBeNull();
    }
    expect(root.querySelector('[data-role="packages"]')).toBeNull();
    const operations = fake.calls.map((call) => call.operation);
    expect(operations).not.toContain("agent.settings.get");
    expect(operations).not.toContain("agent.packages.get");
    await cleanup();
  });

  it("mounts the settings view as the configuration only, without chat", async () => {
    const { fake, root, cleanup } = await mounted(undefined, "settings");
    expect(root.querySelector('[role="tab"], [role="tablist"]')).toBeNull();
    expect(root.querySelector("article.agent-page")?.getAttribute("data-view")).toBe("settings");
    // The Package page names the Package above its tabs; the view starts with its first section.
    expect(root.querySelector(".teamarness-page-header, h1")).toBeNull();
    expect(root.querySelector("form.agent-settings > .agent-group:first-child h2")?.textContent).toBe("Status");
    expect(root.querySelector("form.agent-settings")).not.toBeNull();
    expect(root.querySelector('select[name="agent"]')).not.toBeNull();
    expect(root.querySelector('input[name="apiKey"]')).not.toBeNull();
    expect(root.querySelector('[data-role="packages"]')).not.toBeNull();
    expect(root.querySelector(".agent-chat, [data-role=\"chat-log\"], textarea[name=\"chatMessage\"]")).toBeNull();
    const operations = fake.calls.map((call) => call.operation);
    expect(operations).not.toContain("agent.chat.get");
    expect(operations).not.toContain("agent.chat.changes");
    await cleanup();
  });

  it("asks for configuration before chatting", async () => {
    const { chat, cleanup } = await mounted();
    expect(chat.querySelector('[data-role="chat-empty"]')?.textContent).toBe(
      "Add the endpoint, model and API key in the Agent's Configuration on its Package page before chatting.",
    );
    expect(chat.querySelector<HTMLButtonElement>('button[data-action="send-chat"]')?.disabled).toBe(true);
    await cleanup();
  });

  it("leaves the unconfigured chat when a status change arrives", async () => {
    const { fake, chat, cleanup } = await mounted();
    const send = () => chat.querySelector<HTMLButtonElement>('button[data-action="send-chat"]');
    expect(send()?.disabled).toBe(true);
    // Configuration saved in the settings view reaches the chat through its status subscription.
    await fake.emit("agent.status.changes", { state: "ready", agent: { principalId: AGENT_ID, name: "Scout" } } satisfies PackageValue);
    expect(chat.querySelector('[data-role="chat-empty"]')?.textContent).toBe("No messages yet. Say hello.");
    expect(send()?.disabled).toBe(false);
    await cleanup();
  });

  it("sends with Enter, shows the reply under the Agent's name and clears the conversation", async () => {
    const { fake, reply, chat, cleanup } = await mounted({ state: "ready", agent: { principalId: AGENT_ID, name: "Scout" } });
    const input = chat.querySelector<HTMLTextAreaElement>('textarea[name="chatMessage"]');
    if (!input) throw new Error("chat input missing");
    expect(input.closest("label")?.textContent).toContain("Message");
    expect(chat.querySelector('[data-role="chat-empty"]')?.textContent).toBe("No messages yet. Say hello.");

    // Shift+Enter is a newline, not a send.
    input.value = "line one";
    input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", shiftKey: true, bubbles: true }));
    await flush();
    expect(fake.calls.some((call) => call.operation === "agent.chat.send")).toBe(false);

    input.value = "  What can you do?  ";
    input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
    await flush();
    const sent = fake.calls.filter((call) => call.operation === "agent.chat.send");
    expect(sent.map((call) => call.payload)).toEqual([{ content: "What can you do?" }]);
    expect(input.value).toBe("");
    expect(chat.querySelector('[data-role="chat-message"]')?.textContent).toBe("Scout is replying...");
    expect(chat.querySelector<HTMLButtonElement>('button[data-action="send-chat"]')?.disabled).toBe(true);

    await reply("I can answer questions.");
    const entries = Array.from(chat.querySelectorAll<HTMLElement>(".chat-entry"));
    expect(entries.map((entry) => [entry.dataset.role, entry.querySelector(".chat-author")?.textContent, entry.querySelector(".chat-content")?.textContent?.trim()])).toEqual([
      ["user", "You", "What can you do?"],
      ["assistant", "Scout", "I can answer questions."],
    ]);
    expect(chat.querySelector('[data-role="chat-message"]')?.textContent).toBe("");
    expect(chat.querySelector<HTMLButtonElement>('button[data-action="send-chat"]')?.disabled).toBe(false);

    await fake.emit("agent.chat.changes", { entries: [], pending: false, lastError: { code: "agent.endpoint_rejected", retryable: false } } satisfies PackageValue);
    expect(chat.querySelector('[data-role="chat-message"]')?.textContent).toBe(
      "The endpoint rejected the request. Check the endpoint, model and API key.",
    );

    await reply("Back again.");
    chat.querySelector<HTMLButtonElement>('button[data-action="clear-chat"]')?.click();
    await flush();
    expect(fake.calls.some((call) => call.operation === "agent.chat.clear")).toBe(true);
    expect(chat.querySelectorAll(".chat-entry")).toHaveLength(0);
    await cleanup();
  });
});

describe("Agent Package UI chat activity", () => {
  async function chatWith(entries: Chat["entries"], failure?: Pick<Chat, "lastError" | "failedActivity">) {
    const backend = fakeBackend({ status: { state: "ready", agent: { principalId: AGENT_ID, name: "Scout" } } });
    const host = document.createElement("div");
    document.body.append(host);
    const root = host.attachShadow({ mode: "open" });
    const unmount = await mount(createTestMountContext({ root, client: backend.client }));
    const chat: Chat = { entries, pending: false, ...failure };
    await backend.fake.emit("agent.chat.changes", chat satisfies PackageValue);
    const items = Array.from(root.querySelectorAll<HTMLElement>(".chat-entry"));
    const cleanup = async () => {
      await unmount();
      host.remove();
    };
    return { root, items, cleanup };
  }

  it("shows the Package operations an answer used, grouped by package", async () => {
    const { items, cleanup } = await chatWith([
      { id: 1, role: "user", content: "What exhibits are there?" },
      {
        id: 2,
        role: "assistant",
        content: "One: Q3 review.",
        activity: [
          { package: "exhibits", port: "runtime.guide", ok: true },
          { package: "agent", port: "skill_read", ok: true },
          { package: "exhibits", port: "exhibit.list", ok: true },
        ],
      },
    ]);
    const line = items[1]?.querySelector<HTMLElement>('[data-role="chat-activity"]');
    expect(line?.textContent).toBe("Used Exhibits: runtime.guide, exhibit.list; Agent: skill_read");
    // Plain text after the answer, read by screen readers in order.
    expect(line?.getAttribute("aria-hidden")).toBeNull();
    expect(line?.previousElementSibling?.classList.contains("chat-content")).toBe(true);
    expect(line?.classList.contains("teamarness-section")).toBe(false);
    await cleanup();
  });

  it("shows no activity line for an answer without activity", async () => {
    const { items, cleanup } = await chatWith([
      { id: 1, role: "user", content: "Hello" },
      { id: 2, role: "assistant", content: "Hi." },
    ]);
    expect(items).toHaveLength(2);
    for (const item of items) expect(item.querySelector('[data-role="chat-activity"]')).toBeNull();
    await cleanup();
  });

  it("shows the code of a failed call", async () => {
    const { items, cleanup } = await chatWith([
      {
        id: 1,
        role: "assistant",
        content: "That type is not allowed.",
        activity: [
          { package: "exhibits", port: "draft.update", ok: false, code: "exhibits.type_refused" },
          { package: "exhibits", port: "exhibit.list", ok: true },
        ],
      },
    ]);
    expect(items[0]?.querySelector('[data-role="chat-activity"]')?.textContent).toBe(
      "Used Exhibits: draft.update (failed: exhibits.type_refused), exhibit.list",
    );
    await cleanup();
  });

  it("shows a failed turn's activity under its error", async () => {
    const { root, cleanup } = await chatWith([{ id: 1, role: "user", content: "What exhibits are there?" }], {
      lastError: { code: "agent.endpoint_unreachable", retryable: true },
      failedActivity: [{ package: "exhibits", port: "exhibit.list", ok: true }],
    });
    const message = root.querySelector<HTMLElement>('[data-role="chat-message"]');
    expect(message?.textContent).toBe("The endpoint could not be reached. Try again.");
    const line = root.querySelector<HTMLElement>('[data-role="chat-failed-activity"]');
    expect(line?.hidden).toBe(false);
    expect(line?.textContent).toBe("Used Exhibits: exhibit.list");
    expect(line?.previousElementSibling).toBe(message);
    expect(line?.getAttribute("aria-hidden")).toBeNull();
    await cleanup();
  });

  it("shows no failed-turn activity line without failedActivity", async () => {
    const { root, cleanup } = await chatWith([{ id: 1, role: "user", content: "Hello" }], {
      lastError: { code: "agent.endpoint_unreachable", retryable: true },
    });
    const line = root.querySelector<HTMLElement>('[data-role="chat-failed-activity"]');
    expect(line === null || line.hidden).toBe(true);
    await cleanup();
  });
});

describe("Agent Package UI packages", () => {
  const EXHIBITS: PackageView = {
    contract: "dev.teamarness.exhibits",
    major: 3,
    enabledMajor: null,
    name: "Exhibits",
    description: "Author Exhibits. Call runtime.guide first.",
    available: false,
    enabled: false,
  };
  const NOTES: PackageView = {
    contract: "com.example.notes",
    major: 1,
    enabledMajor: 1,
    name: "Notes",
    description: "Notes for the Workspace.",
    available: true,
    enabled: true,
  };

  async function mountedWith(
    packages: PackageView[],
    options?: { refusePackageSet?: string; holdPackageSet?: Promise<void>; failLaterPackageGets?: string },
  ) {
    const backend = fakeBackend({ packages, ...options });
    const host = document.createElement("div");
    document.body.append(host);
    const root = host.attachShadow({ mode: "open" });
    const unmount = await mount(createTestMountContext({ root, client: backend.client, view: "settings" }));
    const section = Array.from(root.querySelectorAll<HTMLElement>("section.agent-group")).find(
      (element) => element.querySelector(".agent-group-title")?.textContent === "Packages",
    );
    if (!section) throw new Error("Packages section missing");
    const cleanup = async () => {
      await unmount();
      host.remove();
    };
    return { ...backend, root, section, cleanup };
  }

  it("lists one row per package with an unavailable marker and a named switch", async () => {
    const { fake, section, cleanup } = await mountedWith([EXHIBITS, NOTES]);
    expect(fake.calls.some((call) => call.operation === "agent.packages.get")).toBe(true);
    // Rows are ruled list items, not boxed panels.
    expect(section.querySelectorAll(".teamarness-section")).toHaveLength(0);
    const rows = Array.from(section.querySelectorAll<HTMLElement>('[data-role="package"]'));
    expect(rows.map((row) => [
      row.querySelector(".package-name")?.textContent,
      row.querySelector(".package-description")?.textContent,
      row.querySelector('[data-role="package-unavailable"]')?.textContent ?? null,
    ])).toEqual([
      ["Exhibits", "Author Exhibits. Call runtime.guide first.", "Unavailable"],
      ["Notes", "Notes for the Workspace.", null],
    ]);
    const switches = Array.from(section.querySelectorAll<HTMLInputElement>('input[role="switch"]'));
    expect(switches.map((element) => [element.type, element.getAttribute("aria-label"), element.checked])).toEqual([
      ["checkbox", "Exhibits", false],
      ["checkbox", "Notes", true],
    ]);
    expect(section.querySelector('[data-role="packages-empty"]')).toBeNull();
    await cleanup();
  });

  it("toggling sends agent.packages.set and renders the stored state it returns", async () => {
    // A later read would fail: the switch and the section show only what the change returned.
    const { fake, section, cleanup } = await mountedWith([EXHIBITS, NOTES], { failLaterPackageGets: "agent.storage_failed" });
    const exhibits = () => section.querySelector<HTMLInputElement>('input[role="switch"][aria-label="Exhibits"]');
    const toggle = exhibits();
    if (!toggle) throw new Error("Exhibits switch missing");
    toggle.checked = true;
    toggle.dispatchEvent(new Event("change", { bubbles: true }));
    await flush();
    const sets = fake.calls.filter((call) => call.operation === "agent.packages.set");
    expect(sets.map((call) => call.payload)).toEqual([{ contract: "dev.teamarness.exhibits", enabled: true, major: 3 }]);
    expect(section.querySelector('[data-role="packages-message"]')?.textContent).toBe("");
    expect(exhibits()?.checked).toBe(true);
    expect(exhibits()?.disabled).toBe(false);

    const notes = section.querySelector<HTMLInputElement>('input[role="switch"][aria-label="Notes"]');
    if (!notes) throw new Error("Notes switch missing");
    notes.checked = false;
    notes.dispatchEvent(new Event("change", { bubbles: true }));
    await flush();
    expect(section.querySelector<HTMLInputElement>('input[role="switch"][aria-label="Notes"]')?.checked).toBe(false);
    await cleanup();
  });

  it("shows a refused change as a Package error and keeps the last view", async () => {
    const { client } = fakeBackend({ packages: [EXHIBITS] });
    const states: AgentUiState[] = [];
    const controller = createAgentController(client, (state) => { states.push(state); }, "settings");
    await controller.start();
    expect(lastState(states).packages).toEqual([EXHIBITS]);
    await controller.setPackage("com.example.gone", false);
    expect(lastState(states).packagesError).toBe("agent.unknown_package");
    expect(lastState(states).packages).toEqual([EXHIBITS]);
    expect(lastState(states).packagesBusy).toBe(false);
    controller.dispose();
  });

  it("shows the requested state while a change is in flight", async () => {
    let release = (): void => undefined;
    const held = new Promise<void>((resolve) => { release = resolve; });
    const { fake, section, cleanup } = await mountedWith([EXHIBITS, NOTES], { holdPackageSet: held });
    const switches = () => Array.from(section.querySelectorAll<HTMLInputElement>('input[role="switch"]'));
    const [toggle] = switches();
    if (!toggle) throw new Error("Exhibits switch missing");
    toggle.checked = true;
    toggle.dispatchEvent(new Event("change", { bubbles: true }));
    await flush();
    expect(fake.calls.filter((call) => call.operation === "agent.packages.set")).toHaveLength(1);
    // The pending switch keeps what was asked for; every switch waits for the answer.
    expect(switches().map((element) => [element.checked, element.disabled])).toEqual([[true, true], [true, true]]);

    release();
    await flush();
    expect(switches().map((element) => [element.checked, element.disabled])).toEqual([[true, false], [true, false]]);
    expect(fake.calls.filter((call) => call.operation === "agent.packages.set")).toHaveLength(1);
    await cleanup();
  });

  it("returns a refused switch to the stored state", async () => {
    let release = (): void => undefined;
    const held = new Promise<void>((resolve) => { release = resolve; });
    const { fake, section, cleanup } = await mountedWith([EXHIBITS, NOTES], {
      refusePackageSet: "agent.storage_failed",
      holdPackageSet: held,
    });
    const exhibits = () => section.querySelector<HTMLInputElement>('input[role="switch"][aria-label="Exhibits"]');
    const toggle = exhibits();
    if (!toggle) throw new Error("Exhibits switch missing");
    toggle.checked = true;
    toggle.dispatchEvent(new Event("change", { bubbles: true }));
    await flush();
    expect(toggle.checked).toBe(true);
    expect(toggle.disabled).toBe(true);
    release();
    await flush();
    expect(fake.calls.filter((call) => call.operation === "agent.packages.set")).toHaveLength(1);
    expect(section.querySelector('[data-role="packages-message"]')?.textContent).toBe("agent.storage_failed");
    // The switch shows what is stored, not what was asked for.
    expect(exhibits()).toBe(toggle);
    expect(toggle.checked).toBe(false);
    expect(toggle.disabled).toBe(false);
    await cleanup();
  });

  it("disables an unavailable retained major by identity without silently adopting the offer", async () => {
    const retained = { ...EXHIBITS, major: 4, enabled: true, enabledMajor: 2 };
    const { fake, section, cleanup } = await mountedWith([retained]);
    expect(section.querySelector('[data-role="package-version"]')?.textContent).toBe("Enabled major 2; offered major 4");
    const toggle = section.querySelector<HTMLInputElement>('input[role="switch"]');
    if (!toggle) throw new Error("switch missing");
    toggle.checked = false;
    toggle.dispatchEvent(new Event("change", { bubbles: true }));
    await flush();
    expect(fake.calls.filter((call) => call.operation === "agent.packages.set").map((call) => call.payload)).toEqual([
      { contract: EXHIBITS.contract, enabled: false },
    ]);
    expect(toggle.checked).toBe(false);
    await cleanup();
  });

  it("does not let an older Packages read overwrite a newer successful change", async () => {
    let release = (): void => undefined;
    const held = new Promise<void>((resolve) => { release = resolve; });
    const { fake, client } = fakeBackend({ packages: [{ ...EXHIBITS, enabled: true, enabledMajor: 3 }], holdFirstPackageGet: held });
    const states: AgentUiState[] = [];
    const controller = createAgentController(client, (state) => { states.push(state); }, "settings");
    const starting = controller.start();
    await flush();
    expect(fake.calls.some((call) => call.operation === "agent.packages.get")).toBe(true);
    await controller.setPackage(EXHIBITS.contract, false);
    expect(lastState(states).packages?.[0]?.enabled).toBe(false);
    release(); await starting;
    expect(lastState(states).packages?.[0]?.enabled).toBe(false);
    expect(lastState(states).packagesBusy).toBe(false);
    controller.dispose();
  });

  it("refreshes an offer after a stale-major refusal before another explicit enable", async () => {
    const { fake, section, updatePackages, cleanup } = await mountedWith([EXHIBITS], { refusePackageSet: "agent.capability_unavailable" });
    const toggle = section.querySelector<HTMLInputElement>('input[role="switch"]');
    if (!toggle) throw new Error("switch missing");
    toggle.checked = true; toggle.dispatchEvent(new Event("change", { bubbles: true }));
    await flush();
    expect(toggle.checked).toBe(false);
    updatePackages([{ ...EXHIBITS, major: 4 }]);
    const refresh = section.querySelector<HTMLButtonElement>('[data-action="refresh-packages"]');
    expect(refresh).not.toBeNull();
    refresh?.click(); await flush();
    expect(section.querySelector('[data-role="package-version"]')?.textContent).toBe("Offered major 4");
    const freshToggle = section.querySelector<HTMLInputElement>('input[role="switch"]');
    if (!freshToggle) throw new Error("refreshed switch missing");
    freshToggle.checked = true; freshToggle.dispatchEvent(new Event("change", { bubbles: true }));
    await flush();
    expect(fake.calls.filter((call) => call.operation === "agent.packages.set").map((call) => call.payload)).toEqual([
      { contract: EXHIBITS.contract, enabled: true, major: 3 },
      { contract: EXHIBITS.contract, enabled: true, major: 4 },
    ]);
    expect(freshToggle.checked).toBe(false);
    await cleanup();
  });

  it("renders the empty state", async () => {
    const { section, cleanup } = await mountedWith([]);
    expect(section.querySelectorAll('[data-role="package"]')).toHaveLength(0);
    expect(section.querySelector('[data-role="packages-empty"]')?.textContent).toBe("No packages offer agent guidance yet.");
    await cleanup();
  });
});

// Verifies: AC12 (better-doc-agent) — chat entries render the Conversations Markdown subset,
// sanitized the same way; status and error text stays plain text.
describe("Agent Package UI chat Markdown", () => {
  async function chatWith(entries: Chat["entries"], failure?: Pick<Chat, "lastError">) {
    const backend = fakeBackend({ status: { state: "ready", agent: { principalId: AGENT_ID, name: "Scout" } } });
    const host = document.createElement("div");
    document.body.append(host);
    const root = host.attachShadow({ mode: "open" });
    const unmount = await mount(createTestMountContext({ root, client: backend.client }));
    await backend.fake.emit("agent.chat.changes", { entries, pending: false, ...failure } satisfies PackageValue);
    const bodies = Array.from(root.querySelectorAll<HTMLElement>(".chat-entry .message-markdown"));
    const cleanup = async () => {
      await unmount();
      host.remove();
    };
    return { root, bodies, cleanup };
  }

  it("renders user and Agent entries as Markdown: headings, lists, code and safe links", async () => {
    const { bodies, cleanup } = await chatWith([
      { id: 1, role: "user", content: "# Plan\n\n- one\n- **two**" },
      { id: 2, role: "assistant", content: "1. first\n2. second\n\n```\nlet x = 1;\n```\n\nUse `x` and [docs](https://example.invalid/docs)." },
    ]);
    expect(bodies).toHaveLength(2);
    const [user, agent] = bodies;
    expect(user?.querySelector("h1")?.textContent).toBe("Plan");
    expect(Array.from(user?.querySelectorAll("ul > li") ?? [], (item) => item.textContent)).toEqual(["one", "two"]);
    expect(user?.querySelector("li strong")?.textContent).toBe("two");
    expect(Array.from(agent?.querySelectorAll("ol > li") ?? [], (item) => item.textContent)).toEqual(["first", "second"]);
    expect(agent?.querySelector("pre > code")?.textContent).toBe("let x = 1;\n");
    expect(agent?.querySelector("p > code")?.textContent).toBe("x");
    const link = agent?.querySelector("a");
    expect(link?.getAttribute("href")).toBe("https://example.invalid/docs");
    expect(link?.getAttribute("target")).toBe("_blank");
    // `rel` is asked for too, but happy-dom's DOMPurify run drops it; the Conversations copy
    // behaves the same here, so only the real-WebKit manual check (AC12) can show it.
    await cleanup();
  });

  it("renders the supported formatting while escaping raw HTML", async () => {
    const { bodies, cleanup } = await chatWith([
      { id: 1, role: "assistant", content: "# Heading\n\n**bold** and *emphasis*\n\n<script>alert(1)</script>" },
    ]);
    const body = bodies[0];
    expect(body?.querySelector("h1")?.textContent).toBe("Heading");
    expect(body?.querySelector("strong")?.textContent).toBe("bold");
    expect(body?.querySelector("em")?.textContent).toBe("emphasis");
    expect(body?.querySelector("script")).toBeNull();
    expect(body?.textContent).toContain("<script>alert(1)</script>");
    await cleanup();
  });

  it("turns images into alt text and drops unsafe and encoded link schemes", async () => {
    const { bodies, cleanup } = await chatWith([
      { id: 1, role: "assistant", content: "![a & label](https://example.invalid/a.png) <img src=x onerror=alert(1)>" },
      { id: 2, role: "assistant", content: "[bad](javascript:alert(1)) [also bad](%6a%61vascript:alert(1))" },
    ]);
    const [image, links] = bodies;
    expect(image?.querySelector("img")).toBeNull();
    expect(image?.innerHTML).not.toContain("<img");
    expect(image?.textContent).toContain("a & label");
    expect(links?.innerHTML).not.toContain("javascript:");
    for (const anchor of links?.querySelectorAll("a") ?? []) {
      expect(anchor.getAttribute("href")?.toLowerCase() ?? "").not.toContain("javascript");
    }
    await cleanup();
  });

  it("renders GitHub-style tables without alignment styles, and strips unsafe cell content", async () => {
    const { bodies, cleanup } = await chatWith([
      { id: 1, role: "assistant", content: "| Name | Count |\n| :--- | ---: |\n| **apples** | 3 |\n| pears | 5 |" },
      { id: 2, role: "assistant", content: "| a | b |\n| - | - |\n| <script>alert(1)</script> | [x](javascript:alert(1)) ![i](https://example.invalid/i.png) |" },
    ]);
    const [table, unsafe] = bodies;
    expect(Array.from(table?.querySelectorAll("thead th") ?? [], (cell) => cell.textContent)).toEqual(["Name", "Count"]);
    expect(Array.from(table?.querySelectorAll("tbody tr") ?? [], (row) => Array.from(row.querySelectorAll("td"), (cell) => cell.textContent))).toEqual([
      ["apples", "3"],
      ["pears", "5"],
    ]);
    expect(table?.querySelector("tbody td strong")?.textContent).toBe("apples");
    expect(table?.querySelector("[style]")).toBeNull();
    expect(unsafe?.querySelector("table")).not.toBeNull();
    expect(unsafe?.querySelector("script, img")).toBeNull();
    expect(unsafe?.innerHTML).not.toContain("javascript:");
    await cleanup();
  });

  it("keeps status and error text as plain text", async () => {
    const { root, cleanup } = await chatWith([], { lastError: { code: "**agent.odd**", retryable: false } });
    const message = root.querySelector<HTMLElement>('[data-role="chat-message"]');
    expect(message?.textContent).toBe("**agent.odd**");
    expect(message?.querySelector("strong")).toBeNull();
    await cleanup();
  });
});

// Verifies: AC11 (better-doc-agent) — the chat draft survives a remount through the Backend's
// memory, is cleared by a successful send and kept by a failed one.
describe("Agent Package UI remembered chat draft", () => {
  const READY: Status = { state: "ready", agent: { principalId: AGENT_ID, name: "Scout" } };
  const WAIT = { timeout: 2000, interval: 10 };

  async function mountOn(backend: ReturnType<typeof fakeBackend>) {
    const host = document.createElement("div");
    document.body.append(host);
    const root = host.attachShadow({ mode: "open" });
    const unmount = await mount(createTestMountContext({ root, client: backend.client }));
    const input = root.querySelector<HTMLTextAreaElement>('textarea[name="chatMessage"]');
    if (!input) throw new Error("chat input missing");
    const type = (text: string): void => {
      input.value = text;
      input.dispatchEvent(new Event("input", { bubbles: true }));
    };
    const cleanup = async () => {
      await unmount();
      host.remove();
    };
    return { root, input, type, cleanup };
  }

  const draftSets = (backend: ReturnType<typeof fakeBackend>) =>
    backend.fake.calls.filter((call) => call.operation === "agent.chat.draft.set").map((call) => call.payload);

  it("restores the remembered draft on mount and saves a burst of typing once", async () => {
    const backend = fakeBackend({ status: READY, draft: "half a *thought*" });
    const first = await mountOn(backend);
    await vi.waitFor(() => expect(first.input.value).toBe("half a *thought*"), WAIT);
    expect(draftSets(backend)).toEqual([]);

    first.type("half a *thought* and");
    first.type("half a *thought* and more");
    await vi.waitFor(() => expect(draftSets(backend)).toEqual([{ text: "half a *thought* and more" }]), WAIT);
    await first.cleanup();

    const second = await mountOn(backend);
    await vi.waitFor(() => expect(second.input.value).toBe("half a *thought* and more"), WAIT);
    await second.cleanup();
  });

  it("sends a pending save when unmounted", async () => {
    const backend = fakeBackend({ status: READY });
    const first = await mountOn(backend);
    first.type("unsaved");
    expect(draftSets(backend)).toEqual([]);
    await first.cleanup();
    await vi.waitFor(() => expect(backend.draft()).toBe("unsaved"), WAIT);
    expect(draftSets(backend)).toEqual([{ text: "unsaved" }]);
  });

  it("clears the draft after a successful send, and clearing the chat leaves the draft alone", async () => {
    const backend = fakeBackend({ status: READY });
    const view = await mountOn(backend);
    view.type("Send me");
    await vi.waitFor(() => expect(backend.draft()).toBe("Send me"), WAIT);
    view.input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
    await vi.waitFor(() => expect(backend.draft()).toBe(""), WAIT);
    expect(view.input.value).toBe("");
    await backend.reply("Done.");

    view.type("next thought");
    await vi.waitFor(() => expect(backend.draft()).toBe("next thought"), WAIT);
    const before = draftSets(backend).length;
    view.root.querySelector<HTMLButtonElement>('button[data-action="clear-chat"]')?.click();
    await vi.waitFor(() => expect(backend.fake.calls.some((call) => call.operation === "agent.chat.clear")).toBe(true), WAIT);
    await new Promise((resolve) => setTimeout(resolve, 200));
    expect(view.input.value).toBe("next thought");
    expect(draftSets(backend)).toHaveLength(before);
    expect(backend.draft()).toBe("next thought");
    await view.cleanup();
  });

  it("keeps the draft after a failed send, also across a remount", async () => {
    const backend = fakeBackend({ status: READY, refuseSend: "agent.endpoint_unreachable" });
    const first = await mountOn(backend);
    first.type("Keep me");
    await vi.waitFor(() => expect(backend.draft()).toBe("Keep me"), WAIT);
    first.input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
    await vi.waitFor(() => expect(first.root.querySelector('[data-role="chat-message"]')?.textContent)
      .toBe("The endpoint could not be reached. Try again."), WAIT);
    expect(first.input.value).toBe("Keep me");
    expect(draftSets(backend)).toEqual([{ text: "Keep me" }]);
    await first.cleanup();

    const second = await mountOn(backend);
    await vi.waitFor(() => expect(second.input.value).toBe("Keep me"), WAIT);
    await second.cleanup();
  });

  it("forgets the draft when the Member empties the box", async () => {
    const backend = fakeBackend({ status: READY, draft: "old" });
    const view = await mountOn(backend);
    await vi.waitFor(() => expect(view.input.value).toBe("old"), WAIT);
    view.type("");
    await vi.waitFor(() => expect(draftSets(backend)).toEqual([{ text: "" }]), WAIT);
    await view.cleanup();
  });

  it("treats an unreadable draft as empty and never lets a late read overwrite typing", async () => {
    const refused = fakeBackend({ status: READY, draft: "unreadable", refuseDraftGet: true });
    const view = await mountOn(refused);
    await flush();
    expect(view.input.value).toBe("");
    expect(view.root.querySelector('[data-role="chat-message"]')?.textContent).toBe("");
    await view.cleanup();

    let release = (): void => undefined;
    const held = new Promise<void>((resolve) => { release = resolve; });
    const late = fakeBackend({ status: READY, draft: "stale", holdDraftGet: held });
    const mounting = mountOn(late);
    // The status and chat answer first; the draft read is still on its way.
    await vi.waitFor(() => expect(late.fake.calls.some((call) => call.operation === "agent.chat.draft.get")).toBe(true), WAIT);
    const root = document.body.lastElementChild?.shadowRoot;
    const input = root?.querySelector<HTMLTextAreaElement>('textarea[name="chatMessage"]');
    if (!input) throw new Error("chat input missing");
    input.value = "typed first";
    input.dispatchEvent(new Event("input", { bubbles: true }));
    release();
    const mounted = await mounting;
    await flush();
    expect(mounted.input.value).toBe("typed first");
    await vi.waitFor(() => expect(late.draft()).toBe("typed first"), WAIT);
    await mounted.cleanup();
  });
});

/** Lets a click's chained Backend round trips settle; the fake client answers in microtasks. */
async function flush(): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, 0));
  await new Promise((resolve) => setTimeout(resolve, 0));
}
