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
  /** Every `agent.packages.set` waits for this before it answers. */
  holdPackageSet?: Promise<void>;
  holdFirstPackageGet?: Promise<void>;
  /** Every `agent.packages.get` after the first fails with this code. */
  failLaterPackageGets?: string;
}) {
  let settings = defaults();
  let hasKey = false;
  let status: Status = input?.status ?? { state: "unconfigured" };
  const candidates = input?.candidates ?? [{ principalId: AGENT_ID, name: "Scout", bound: false }];
  let chat: Chat = { entries: [], pending: false };
  let packages: PackageView[] = input?.packages ?? [];
  let packageGets = 0;
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
      "agent.chat.get": () => chat,
      "agent.chat.send": (payload) => {
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
  return { fake, client: fake.client, reply, updatePackages: (next: PackageView[]) => { packages = next; }, secretSeen: () => fake.calls.some((call) => JSON.stringify(call.payload).includes(SECRET)) };
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
    expect(operations).toContain("agent.chat.get");
    expect(operations).toContain("agent.chat.changes");
    const state = lastState(states);
    expect(state.chat).toEqual({ entries: [], pending: false });
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

  it("sends chat messages, follows replies and explains refusals", async () => {
    const { fake, client, reply } = fakeBackend({ status: { state: "ready", agent: { principalId: AGENT_ID, name: "Scout" } } });
    const states: AgentUiState[] = [];
    const controller = createAgentController(client, (state) => { states.push(state); });
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
    expect(describeChatError("agent.not_configured")).toBe("Add the endpoint, model and API key on the Configuration tab first.");
    expect(describeChatError("agent.endpoint_unreachable")).toBe("The endpoint could not be reached. Try again.");
    expect(describeChatError("agent.chat_interrupted")).toBe("The reply was interrupted. Send the message again.");
    expect(describeChatError("agent.turn_limit_exceeded")).toBe("The reply needed more steps than one turn allows. Ask for something smaller.");
    expect(describeChatError("agent.something_new")).toBe("agent.something_new");
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
      unmount = await mount(createTestMountContext({ root, client }));
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
      expect(css).toMatch(/\.agent-page\[data-tab="chat"\]\s*\{[^}]*height:\s*100vh/s);
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
    const context = createTestMountContext({ root, client, displayName: "Agent" });

    const unmount = await mount(context);
    expect(root.querySelectorAll("form.agent-settings")).toHaveLength(1);
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
    expect(root.querySelector('[data-role="status"]')?.textContent).toBe("Ready: Scout.");

    await fake.emit("agent.status.changes", { state: "error", lastError: { code: "agent.endpoint_rejected", retryable: false } } satisfies PackageValue);
    expect(root.querySelector('[data-role="status"]')?.textContent).toBe("Error: agent.endpoint_rejected");

    await unmount();
    await unmount();
    expect(root.childElementCount).toBe(0);
    host.remove();
  });
});

describe("Agent Package UI tabs and chat", () => {
  async function mounted(status?: Status) {
    const backend = fakeBackend(status ? { status } : undefined);
    const host = document.createElement("div");
    document.body.append(host);
    const root = host.attachShadow({ mode: "open" });
    const unmount = await mount(createTestMountContext({ root, client: backend.client }));
    const tab = (name: string) => {
      const found = Array.from(root.querySelectorAll<HTMLButtonElement>("[role=tab]")).find((element) => element.textContent === name);
      if (!found) throw new Error(`tab ${name} missing`);
      return found;
    };
    const panel = (name: string) => {
      const found = root.getElementById(tab(name).getAttribute("aria-controls") ?? "");
      if (!found) throw new Error(`panel ${name} missing`);
      return found;
    };
    const cleanup = async () => {
      await unmount();
      host.remove();
    };
    return { ...backend, root, tab, panel, cleanup };
  }

  it("opens on Configuration and switches panels by click and arrow keys", async () => {
    const { root, tab, panel, cleanup } = await mounted();
    expect(Array.from(root.querySelectorAll("[role=tab]")).map((element) => element.textContent)).toEqual(["Configuration", "Chat"]);
    expect(tab("Configuration").getAttribute("aria-selected")).toBe("true");
    expect(panel("Configuration").hidden).toBe(false);
    expect(panel("Chat").hidden).toBe(true);
    expect(panel("Configuration").querySelector("form.agent-settings")).not.toBeNull();

    // One centered page frame for both tabs, so the header never moves between them.
    const page = root.querySelector<HTMLElement>("article.agent-page");
    const frame = page?.className;
    expect(page?.dataset.tab).toBe("configuration");
    tab("Chat").click();
    expect(tab("Chat").getAttribute("aria-selected")).toBe("true");
    expect(page?.dataset.tab).toBe("chat");
    expect(page?.className).toBe(frame);
    expect(tab("Configuration").tabIndex).toBe(-1);
    expect(panel("Chat").hidden).toBe(false);
    expect(panel("Configuration").hidden).toBe(true);

    tab("Chat").dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowRight", bubbles: true }));
    expect(tab("Configuration").getAttribute("aria-selected")).toBe("true");
    expect(root.activeElement).toBe(tab("Configuration"));
    expect(page?.className).toBe(frame);
    await cleanup();
  });

  it("asks for configuration before chatting", async () => {
    const { tab, panel, cleanup } = await mounted();
    tab("Chat").click();
    expect(panel("Chat").querySelector('[data-role="chat-empty"]')?.textContent).toBe(
      "Add the endpoint, model and API key on the Configuration tab before chatting.",
    );
    expect(panel("Chat").querySelector<HTMLButtonElement>('button[data-action="send-chat"]')?.disabled).toBe(true);
    await cleanup();
  });

  it("sends with Enter, shows the reply under the Agent's name and clears the conversation", async () => {
    const { fake, reply, panel, tab, cleanup } = await mounted({ state: "ready", agent: { principalId: AGENT_ID, name: "Scout" } });
    tab("Chat").click();
    const chat = panel("Chat");
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
    expect(entries.map((entry) => [entry.dataset.role, entry.querySelector(".chat-author")?.textContent, entry.querySelector(".chat-content")?.textContent])).toEqual([
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
    expect(line?.previousElementSibling?.className).toBe("chat-content");
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
    const unmount = await mount(createTestMountContext({ root, client: backend.client }));
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
    const controller = createAgentController(client, (state) => { states.push(state); });
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
    const controller = createAgentController(client, (state) => { states.push(state); });
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

/** Lets a click's chained Backend round trips settle; the fake client answers in microtasks. */
async function flush(): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, 0));
  await new Promise((resolve) => setTimeout(resolve, 0));
}
