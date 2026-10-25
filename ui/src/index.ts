import {
  adoptPackageStyles,
  describePackageError,
  installPackageUiKit,
  isAbortError,
  isPackageRequestError,
  packageErrorCode,
  typedPackageClient,
  type PackageClient,
  type PackageMountContext,
  type PackageSubscription,
  type PackageUnmount,
} from "@teamarness/package-ui-sdk";
import type { AgentContract, Candidate, Chat, PackageView, Settings, Status } from "./contract.generated";

export type { Candidate, Chat, PackageView, Settings, Status };

// ─── State ───────────────────────────────────────────────────────────────────

export type AgentUiState = Readonly<{
  status: Status | null;
  settings: Settings | null;
  hasKey: boolean;
  candidates: readonly Candidate[];
  busy: boolean;
  error: string | undefined;
  notice: string | undefined;
  chat: Chat | null;
  /** A chat request is on its way; the turn itself is `chat.pending`. */
  chatBusy: boolean;
  chatError: string | undefined;
  /** Agent-facing contracts; `null` until the first read answers. */
  packages: readonly PackageView[] | null;
  /** A Packages read or change is on its way. */
  packagesBusy: boolean;
  packagesError: string | undefined;
}>;

export type AgentController = Readonly<{
  start(): Promise<void>;
  saveSettings(settings: Settings): Promise<void>;
  setKey(apiKey: string): Promise<void>;
  clearKey(): Promise<void>;
  refreshCandidates(): Promise<void>;
  bind(principalId: string): Promise<void>;
  unbind(): Promise<void>;
  /** Resolves `true` when the Backend accepted the message. */
  sendChat(content: string): Promise<boolean>;
  clearChat(): Promise<void>;
  /** Enables or disables one agent-facing contract, then re-reads the list. */
  setPackage(contract: string, enabled: boolean): Promise<void>;
  dispose(): void;
}>;

/** One display sentence per status state. Never includes error details. */
export function describeStatus(status: Status | null): string {
  if (status === null) return "Loading...";
  switch (status.state) {
    case "unconfigured":
      return "Not configured: add the endpoint, model and API key.";
    case "no_agent":
      return "No Agent: choose one of your Agents below.";
    case "ready":
      return status.agent?.name
        ? `Ready: ${status.agent.name} replies when addressed.`
        : "Ready: the Agent replies when addressed.";
    case "replying":
      return "Replying...";
    case "error":
      return `Error: ${status.lastError?.code ?? "unknown"}`;
  }
}

/** One display sentence per chat failure code; unknown codes are shown as they are. */
export function describeChatError(code: string): string {
  switch (code) {
    case "agent.not_configured":
      return "Add the endpoint, model and API key on the Configuration tab first.";
    case "agent.chat_busy":
      return "Wait for the current reply before sending another message.";
    case "agent.invalid_message":
      return "Messages cannot be empty or contain control characters.";
    case "agent.chat_interrupted":
      return "The reply was interrupted. Send the message again.";
    case "agent.endpoint_unreachable":
      return "The endpoint could not be reached. Try again.";
    case "agent.endpoint_rejected":
      return "The endpoint rejected the request. Check the endpoint, model and API key.";
    case "agent.model_error":
      return "The model did not return a usable reply.";
    default:
      return code;
  }
}

/** The Package's own code, or the Teamarness description, as one display sentence. */
function describeFailure(error: unknown): string {
  if (packageErrorCode(error) === "agent.invalid_settings" && isPackageRequestError(error)) {
    const field = error.packageError?.details["field"];
    return typeof field === "string" ? `Check the ${field} field.` : "Check the settings.";
  }
  return describePackageError(error);
}

// ─── Controller ──────────────────────────────────────────────────────────────

export function createAgentController(
  client: PackageClient,
  present: (state: AgentUiState) => void,
): AgentController {
  const typed = typedPackageClient<AgentContract>(client);
  let disposed = false;
  let subscription: PackageSubscription | null = null;
  let chatSubscription: PackageSubscription | null = null;
  let state: AgentUiState = {
    status: null,
    settings: null,
    hasKey: false,
    candidates: [],
    busy: false,
    error: undefined,
    notice: undefined,
    chat: null,
    chatBusy: false,
    chatError: undefined,
    packages: null,
    packagesBusy: false,
    packagesError: undefined,
  };

  const update = (changes: Partial<AgentUiState>): void => {
    if (disposed) return;
    state = { ...state, ...changes };
    present(state);
  };

  const fail = (error: unknown): void => {
    if (isAbortError(error)) return;
    update({ error: describeFailure(error), notice: undefined, busy: false });
  };

  const run = async (work: () => Promise<Partial<AgentUiState>>): Promise<void> => {
    if (disposed) return;
    update({ busy: true, error: undefined, notice: undefined });
    try {
      const changes = await work();
      update({ ...changes, busy: false });
    } catch (error) {
      fail(error);
    }
  };

  const chatRequest = async (work: () => Promise<Chat>): Promise<boolean> => {
    if (disposed) return false;
    update({ chatBusy: true, chatError: undefined });
    try {
      update({ chat: await work(), chatBusy: false });
      return true;
    } catch (error) {
      if (isAbortError(error)) return false;
      const code = packageErrorCode(error);
      update({ chatBusy: false, chatError: code ? describeChatError(code) : describePackageError(error) });
      return false;
    }
  };

  const packagesRequest = async (work: () => Promise<void>): Promise<void> => {
    if (disposed) return;
    update({ packagesBusy: true, packagesError: undefined });
    try {
      await work();
      const read = await typed.request("agent.packages.get", {});
      update({ packages: read.packages, packagesBusy: false });
    } catch (error) {
      if (isAbortError(error)) return;
      update({ packagesBusy: false, packagesError: describePackageError(error) });
    }
  };

  const loadCandidates = async (): Promise<readonly Candidate[]> =>
    (await typed.request("agent.binding.candidates", {})).agents;

  return Object.freeze({
    async start(): Promise<void> {
      await run(async () => {
        const settings = await typed.request("agent.settings.get", {});
        const status = await typed.request("agent.status.get", {});
        const candidates = await loadCandidates();
        const chat = await typed.request("agent.chat.get", {});
        subscription = await typed.subscribe("agent.status.changes", {}, (event) => {
          update({ status: event });
        });
        chatSubscription = await typed.subscribe("agent.chat.changes", {}, (event) => {
          update({ chat: event });
        });
        return { settings: settings.settings, hasKey: settings.hasKey, status, candidates, chat };
      });
      // A Packages failure stays in its own section; the rest of the page still works.
      await packagesRequest(async () => undefined);
    },

    async saveSettings(settings: Settings): Promise<void> {
      await run(async () => {
        const saved = await typed.request("agent.settings.set", { settings });
        return { settings: saved.settings, hasKey: saved.hasKey, notice: "Settings saved." };
      });
    },

    async setKey(apiKey: string): Promise<void> {
      await run(async () => {
        const key = await typed.request("agent.key.set", { apiKey });
        return { hasKey: key.hasKey, notice: "API key stored." };
      });
    },

    async clearKey(): Promise<void> {
      await run(async () => {
        const key = await typed.request("agent.key.clear", {});
        return { hasKey: key.hasKey, notice: "API key cleared." };
      });
    },

    async refreshCandidates(): Promise<void> {
      await run(async () => ({ candidates: await loadCandidates() }));
    },

    async bind(principalId: string): Promise<void> {
      await run(async () => {
        await typed.request("agent.binding.set", { principalId });
        const status = await typed.request("agent.status.get", {});
        return { status, candidates: await loadCandidates() };
      });
    },

    async unbind(): Promise<void> {
      await run(async () => {
        await typed.request("agent.binding.clear", {});
        const status = await typed.request("agent.status.get", {});
        return { status, candidates: await loadCandidates() };
      });
    },

    async sendChat(content: string): Promise<boolean> {
      return chatRequest(() => typed.request("agent.chat.send", { content }));
    },

    async clearChat(): Promise<void> {
      await chatRequest(() => typed.request("agent.chat.clear", {}));
    },

    async setPackage(contract: string, enabled: boolean): Promise<void> {
      await packagesRequest(async () => {
        await typed.request("agent.packages.set", { contract, enabled });
      });
    },

    dispose(): void {
      disposed = true;
      for (const open of [subscription, chatSubscription]) {
        if (open) void open.close().catch(() => undefined);
      }
      subscription = null;
      chatSubscription = null;
    },
  });
}

// ─── Styles ──────────────────────────────────────────────────────────────────

const AGENT_CSS = `
  :host { color: var(--teamarness-color-text); font-family: var(--teamarness-font-body); }
  .agent-page { width: max(75%, min(100%, 40rem)); margin-inline: auto; }
  .agent-page > * + * { margin-top: 1.5rem; }
  .agent-settings { display: grid; }
  .agent-group { display: grid; grid-template-columns: minmax(10rem, 1fr) minmax(0, 2.2fr); gap: 1rem 3rem; padding-block: 2rem; border-top: 1px solid var(--teamarness-color-border); }
  .agent-group:first-child { border-top: 0; padding-top: .5rem; }
  .agent-group-title { margin: 0 0 .35rem; font-size: 1rem; font-weight: 600; }
  .agent-group-intro p { margin: 0; }
  .agent-group-body { min-width: 0; max-width: 40rem; display: flex; flex-direction: column; gap: 1.1rem; }
  .agent-group-body > p { margin: 0; }
  .agent-group-body > button { align-self: flex-start; }
  @media (max-width: 52rem) { .agent-group { grid-template-columns: 1fr; gap: .75rem; } }
  .row { display: flex; flex-wrap: wrap; gap: .6rem; align-items: center; }
  textarea.teamarness-input { min-height: 6rem; resize: vertical; }
  .agent-picker { position: relative; display: block; }
  .agent-picker-control { appearance: none; -webkit-appearance: none; min-height: 2.65rem; padding-right: 2.5rem; cursor: pointer; }
  .agent-picker::after { content: ''; position: absolute; pointer-events: none; right: 1rem; top: 50%; width: .4rem; height: .4rem; border-right: 2px solid var(--teamarness-color-text-muted); border-bottom: 2px solid var(--teamarness-color-text-muted); transform: translateY(-75%) rotate(45deg); }
  .status { font-weight: 600; display: flex; align-items: center; gap: .55rem; }
  .status::before { content: ''; flex: none; width: .55rem; height: .55rem; border-radius: 50%; background: var(--teamarness-color-text-muted); }
  .status[data-state="error"] { color: var(--teamarness-color-danger, #b42318); }
  .status[data-state="error"]::before { background: var(--teamarness-color-danger, #b42318); }
  .status[data-state="ready"] { color: var(--teamarness-color-accent); }
  .status[data-state="ready"]::before, .status[data-state="replying"]::before { background: var(--teamarness-color-accent); }
  .message { min-height: 1.2em; margin: 0; }
  .message:empty { min-height: 0; }
  .message[data-kind="error"] { color: var(--teamarness-color-danger, #b42318); }
  .message[data-kind="notice"] { color: var(--teamarness-color-text-muted); }
  button:disabled { opacity: .5; cursor: default; }
  .agent-tabs { display: flex; gap: .25rem; border-bottom: 1px solid var(--teamarness-color-border); }
  .agent-tab { appearance: none; background: none; border: 0; border-bottom: 2px solid transparent; margin-bottom: -1px; padding: .55rem .9rem; color: var(--teamarness-color-text-muted); font: inherit; font-weight: 600; cursor: pointer; }
  .agent-tab[aria-selected="true"] { color: var(--teamarness-color-text); border-bottom-color: var(--teamarness-color-accent); }
  .agent-tab:focus-visible { outline: 2px solid var(--teamarness-color-focus); outline-offset: 2px; }
  .agent-panel[hidden] { display: none; }
  .agent-page[data-tab="chat"] { display: flex; flex-direction: column; height: 100vh; }
  .agent-page[data-tab="chat"] > #agent-panel-chat { flex: 1; min-height: 0; display: flex; flex-direction: column; }
  .agent-chat { flex: 1; min-height: 0; display: flex; flex-direction: column; gap: 1rem; }
  .chat-log { list-style: none; margin: 0; padding: 0 .25rem 0 0; flex: 1 1 auto; min-height: 0; overflow-y: auto; display: flex; flex-direction: column; gap: 1.25rem; }
  .chat-log:empty { display: none; }
  .chat-form { margin-top: auto; }
  .chat-entry { max-width: 100%; }
  .chat-entry[data-role="user"] { align-self: flex-end; max-width: 75%; padding: .6rem .85rem; border-radius: var(--teamarness-radius-panel); background: var(--teamarness-color-panel); }
  .chat-entry[data-role="assistant"] { align-self: stretch; }
  .chat-author { display: block; font-size: .8em; font-weight: 600; color: var(--teamarness-color-text-muted); margin-bottom: .2rem; }
  .chat-content { margin: 0; white-space: pre-wrap; overflow-wrap: anywhere; }
  .chat-empty { color: var(--teamarness-color-text-muted); }
  textarea.chat-input { min-height: 4.5rem; }
  .package-list { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; }
  .package-list:empty { display: none; }
  .package-row { display: flex; gap: 1rem; align-items: flex-start; justify-content: space-between; padding-block: .85rem; border-top: 1px solid var(--teamarness-color-border); }
  .package-row:first-child { border-top: 0; padding-top: 0; }
  .package-text { min-width: 0; display: flex; flex-direction: column; gap: .2rem; }
  .package-heading { display: flex; flex-wrap: wrap; align-items: baseline; gap: .5rem; }
  .package-name { font-weight: 600; }
  .package-unavailable { font-size: .8em; font-weight: 600; color: var(--teamarness-color-text-muted); }
  .package-description { margin: 0; color: var(--teamarness-color-text-muted); overflow-wrap: anywhere; }
  .package-switch { appearance: none; -webkit-appearance: none; flex: none; position: relative; width: 2.5rem; height: 1.4rem; margin: .1rem 0 0; border-radius: 999px; background: var(--teamarness-color-border); cursor: pointer; transition: background .15s; }
  .package-switch::before { content: ''; position: absolute; top: .2rem; left: .2rem; width: 1rem; height: 1rem; border-radius: 50%; background: var(--teamarness-color-background); transition: transform .15s; }
  .package-switch:checked { background: var(--teamarness-color-accent); }
  .package-switch:checked::before { transform: translateX(1.1rem); }
  .package-switch:focus-visible { outline: 2px solid var(--teamarness-color-focus); outline-offset: 2px; }
  .package-switch:disabled { opacity: .5; cursor: default; }
  .packages-empty { margin: 0; color: var(--teamarness-color-text-muted); }
`;

// ─── Mount ───────────────────────────────────────────────────────────────────

function button(text: string, action: string, variant: "primary" | "secondary"): HTMLButtonElement {
  const element = document.createElement("button");
  element.type = "button";
  element.className = `teamarness-control teamarness-control--${variant}`;
  element.dataset.action = action;
  element.textContent = text;
  return element;
}

function field(labelText: string, control: HTMLElement, help?: string): HTMLLabelElement {
  const label = document.createElement("label");
  label.className = "teamarness-field";
  const caption = document.createElement("span");
  caption.textContent = labelText;
  label.append(caption, control);
  if (help) {
    const hint = document.createElement("span");
    hint.className = "teamarness-help";
    hint.textContent = help;
    label.append(hint);
  }
  return label;
}

/** A configuration section: heading and description beside its controls, ruled from the next. */
function group(titleText: string, description: string | null, ...body: HTMLElement[]): HTMLElement {
  const section = document.createElement("section");
  section.className = "agent-group";
  const intro = document.createElement("div");
  intro.className = "agent-group-intro";
  const heading = document.createElement("h2");
  heading.className = "agent-group-title";
  heading.textContent = titleText;
  intro.append(heading);
  if (description) {
    const help = document.createElement("p");
    help.className = "teamarness-help";
    help.textContent = description;
    intro.append(help);
  }
  const content = document.createElement("div");
  content.className = "agent-group-body";
  content.append(...body);
  section.append(intro, content);
  return section;
}

function input(name: string, type: string, placeholder?: string): HTMLInputElement {
  const element = document.createElement("input");
  element.className = "teamarness-input";
  element.name = name;
  element.type = type;
  if (placeholder) element.placeholder = placeholder;
  if (type === "password") {
    element.autocomplete = "off";
    element.spellcheck = false;
  }
  return element;
}

export async function mount(context: PackageMountContext): Promise<PackageUnmount> {
  const cleanupKit = installPackageUiKit(context.root);
  let cleanupStyles: () => void;
  try {
    cleanupStyles = adoptPackageStyles(context.root, AGENT_CSS);
  } catch (error) {
    cleanupKit();
    throw error;
  }

  const page = document.createElement("article");
  page.className = "teamarness-page agent-page";
  const header = document.createElement("header");
  header.className = "teamarness-page-header";
  const title = document.createElement("h1");
  title.className = "teamarness-page-title";
  title.textContent = context.package.displayName;
  const subtitle = document.createElement("p");
  subtitle.className = "teamarness-page-subtitle";
  subtitle.textContent = "Run one of your Workspace Agents against an OpenAI-compatible endpoint.";
  header.append(title, subtitle);

  // Tabs
  const tabList = document.createElement("div");
  tabList.className = "agent-tabs";
  tabList.setAttribute("role", "tablist");
  tabList.setAttribute("aria-label", "Agent sections");
  const configPanel = document.createElement("div");
  const chatPanel = document.createElement("div");
  const tabs = ([["configuration", "Configuration", configPanel], ["chat", "Chat", chatPanel]] as const).map(
    ([id, text, panel]) => {
      const tab = document.createElement("button");
      tab.type = "button";
      tab.className = "agent-tab";
      tab.id = `agent-tab-${id}`;
      tab.dataset.tab = id;
      tab.setAttribute("role", "tab");
      tab.setAttribute("aria-controls", `agent-panel-${id}`);
      tab.textContent = text;
      panel.id = `agent-panel-${id}`;
      panel.className = "agent-panel";
      panel.setAttribute("role", "tabpanel");
      panel.setAttribute("aria-labelledby", tab.id);
      tabList.append(tab);
      return { id, tab, panel };
    },
  );
  const selectTab = (id: string, focus = false): void => {
    // The chat fills the pane so only its transcript scrolls; configuration scrolls as a page.
    page.dataset.tab = id;
    for (const entry of tabs) {
      const selected = entry.id === id;
      entry.tab.setAttribute("aria-selected", String(selected));
      entry.tab.tabIndex = selected ? 0 : -1;
      entry.panel.hidden = !selected;
      if (selected && focus) entry.tab.focus();
    }
  };
  selectTab("configuration");

  const form = document.createElement("form");
  form.className = "agent-settings";
  form.noValidate = true;
  form.addEventListener("submit", (event) => event.preventDefault());

  // Status
  const statusLine = document.createElement("p");
  statusLine.className = "status";
  statusLine.dataset.role = "status";
  statusLine.setAttribute("role", "status");
  const message = document.createElement("p");
  message.className = "message";
  message.dataset.role = "message";
  message.setAttribute("role", "status");
  const statusSection = group("Status", "What the Agent is doing in this Workspace.", statusLine, message);

  // Agent picker
  const picker = document.createElement("select");
  picker.className = "teamarness-input agent-picker-control";
  picker.name = "agent";
  const pickerControl = document.createElement("span");
  pickerControl.className = "agent-picker";
  pickerControl.append(picker);
  const pickerRow = document.createElement("div");
  pickerRow.className = "row";
  const bindButton = button("Use this Agent", "bind", "primary");
  const unbindButton = button("Clear binding", "unbind", "secondary");
  const refreshButton = button("Refresh", "refresh", "secondary");
  pickerRow.append(bindButton, unbindButton, refreshButton);
  const agentSection = group("Agent", "Only Agents you steward on this Installation are offered.",
    field("Workspace Agent", pickerControl), pickerRow);

  // Endpoint settings
  const endpointInput = input("endpointBaseUrl", "url", "https://openrouter.ai/api/v1");
  const modelInput = input("model", "text", "deepseek/deepseek-chat");
  const promptInput = document.createElement("textarea");
  promptInput.className = "teamarness-input";
  promptInput.name = "systemPrompt";
  const contextInput = input("contextMessages", "number");
  contextInput.min = "0";
  contextInput.max = "64";
  contextInput.step = "1";
  const saveButton = button("Save settings", "save", "primary");
  const settingsSection = group(
    "Model endpoint",
    "The OpenAI-compatible model that answers Channel replies and the chat.",
    field("Endpoint base URL", endpointInput, "An OpenAI-compatible base URL; /chat/completions is appended."),
    field("Model", modelInput),
    field("System prompt", promptInput),
    field("Context messages", contextInput, "How many earlier Messages a reply sees, 0 to 64."),
    saveButton,
  );

  // API key
  const keyState = document.createElement("p");
  keyState.dataset.role = "key-state";
  const keyInput = input("apiKey", "password");
  const keyRow = document.createElement("div");
  keyRow.className = "row";
  const setKeyButton = button("Store key", "set-key", "primary");
  const clearKeyButton = button("Clear key", "clear-key", "secondary");
  keyRow.append(setKeyButton, clearKeyButton);
  const keySection = group("API key", "Sent only to the configured endpoint.", keyState,
    field("New API key", keyInput, "Stored in this Package's data directory and never shown again."), keyRow);

  // Packages
  const packageList = document.createElement("ul");
  packageList.className = "package-list";
  packageList.dataset.role = "packages";
  packageList.setAttribute("aria-label", "Packages");
  const packagesEmpty = document.createElement("p");
  packagesEmpty.className = "packages-empty";
  packagesEmpty.dataset.role = "packages-empty";
  packagesEmpty.textContent = "No packages offer agent guidance yet.";
  const packagesMessage = document.createElement("p");
  packagesMessage.className = "message";
  packagesMessage.dataset.role = "packages-message";
  packagesMessage.setAttribute("role", "status");
  const packagesSection = group("Packages",
    "Packages whose tools the Agent may use once enabled. A package must be installed in this Workspace to be available.",
    packageList, packagesMessage);

  form.append(statusSection, agentSection, settingsSection, keySection, packagesSection);
  configPanel.append(form);

  // Chat
  const chatSection = document.createElement("section");
  chatSection.className = "agent-chat";
  chatSection.setAttribute("aria-label", "Chat with the Agent");
  const chatIntro = document.createElement("p");
  chatIntro.className = "teamarness-help";
  chatIntro.textContent = "A private conversation with the configured model. Nothing here is posted to a Channel, and it is kept only until the Package restarts.";
  const chatEmpty = document.createElement("p");
  chatEmpty.className = "chat-empty";
  chatEmpty.dataset.role = "chat-empty";
  const chatLog = document.createElement("ol");
  chatLog.className = "chat-log";
  chatLog.dataset.role = "chat-log";
  chatLog.setAttribute("role", "log");
  chatLog.setAttribute("aria-label", "Conversation");
  const chatMessage = document.createElement("p");
  chatMessage.className = "message";
  chatMessage.dataset.role = "chat-message";
  chatMessage.setAttribute("role", "status");
  const chatForm = document.createElement("form");
  chatForm.className = "teamarness-stack chat-form";
  chatForm.noValidate = true;
  const chatInput = document.createElement("textarea");
  chatInput.className = "teamarness-input chat-input";
  chatInput.name = "chatMessage";
  chatInput.maxLength = 8192;
  const chatRow = document.createElement("div");
  chatRow.className = "row";
  const sendButton = button("Send", "send-chat", "primary");
  sendButton.type = "submit";
  const clearChatButton = button("Clear conversation", "clear-chat", "secondary");
  chatRow.append(sendButton, clearChatButton);
  chatForm.append(field("Message", chatInput, "Enter sends; Shift+Enter adds a line."), chatRow);
  chatSection.append(chatIntro, chatEmpty, chatLog, chatMessage, chatForm);
  chatPanel.append(chatSection);

  page.append(header, tabList, configPanel, chatPanel);
  context.root.append(page);

  let settingsLoaded = false;
  const render = (state: AgentUiState): void => {
    statusLine.textContent = describeStatus(state.status);
    statusLine.dataset.state = state.status?.state ?? "loading";
    if (state.error) {
      message.textContent = state.error;
      message.dataset.kind = "error";
    } else if (state.notice) {
      message.textContent = state.notice;
      message.dataset.kind = "notice";
    } else {
      message.textContent = "";
      delete message.dataset.kind;
    }

    const boundId = state.status?.agent?.principalId ?? state.candidates.find((candidate) => candidate.bound)?.principalId ?? "";
    const selected = picker.value || boundId;
    picker.replaceChildren();
    const none = document.createElement("option");
    none.value = "";
    none.textContent = state.candidates.length === 0 ? "No Agents stewarded here" : "Choose an Agent";
    picker.append(none);
    for (const candidate of state.candidates) {
      const option = document.createElement("option");
      option.value = candidate.principalId;
      option.textContent = `${candidate.name ?? candidate.principalId.slice(0, 12)}${candidate.bound ? " (bound)" : ""}`;
      picker.append(option);
    }
    picker.value = state.candidates.some((candidate) => candidate.principalId === selected) ? selected : "";

    if (state.settings && !settingsLoaded) {
      endpointInput.value = state.settings.endpointBaseUrl;
      modelInput.value = state.settings.model;
      promptInput.value = state.settings.systemPrompt;
      contextInput.value = String(state.settings.contextMessages);
      settingsLoaded = true;
    }
    keyState.textContent = state.hasKey ? "A key is stored." : "No key is stored.";

    const busy = state.busy;
    bindButton.disabled = busy;
    unbindButton.disabled = busy || !boundId;
    refreshButton.disabled = busy;
    saveButton.disabled = busy;
    setKeyButton.disabled = busy;
    clearKeyButton.disabled = busy || !state.hasKey;

    renderPackages(state);
    renderChat(state);
  };

  let renderedPackages = "";
  const renderPackages = (state: AgentUiState): void => {
    const packages = state.packages ?? [];
    const key = JSON.stringify(packages);
    if (key !== renderedPackages) {
      renderedPackages = key;
      // Keep keyboard focus on the same switch across a re-render.
      const focused = (context.root.activeElement as HTMLElement | null)?.dataset?.contract;
      packageList.replaceChildren(...packages.map((entry) => {
        const row = document.createElement("li");
        row.className = "package-row";
        row.dataset.role = "package";
        const text = document.createElement("div");
        text.className = "package-text";
        const heading = document.createElement("div");
        heading.className = "package-heading";
        const name = document.createElement("span");
        name.className = "package-name";
        name.textContent = entry.name;
        heading.append(name);
        if (!entry.available) {
          const marker = document.createElement("span");
          marker.className = "package-unavailable";
          marker.dataset.role = "package-unavailable";
          marker.textContent = "Unavailable";
          heading.append(marker);
        }
        const description = document.createElement("p");
        description.className = "package-description";
        description.textContent = entry.description;
        text.append(heading, description);
        const toggle = document.createElement("input");
        toggle.type = "checkbox";
        toggle.className = "package-switch";
        toggle.setAttribute("role", "switch");
        toggle.setAttribute("aria-label", entry.name);
        toggle.dataset.contract = entry.contract;
        toggle.checked = entry.enabled;
        row.append(text, toggle);
        return row;
      }));
      if (focused) {
        Array.from(packageList.querySelectorAll<HTMLElement>(".package-switch"))
          .find((toggle) => toggle.dataset.contract === focused)?.focus();
      }
    }
    for (const toggle of packageList.querySelectorAll<HTMLInputElement>(".package-switch")) {
      toggle.disabled = state.packagesBusy;
    }
    packagesEmpty.remove();
    if (state.packages !== null && packages.length === 0) packageList.before(packagesEmpty);
    packagesMessage.textContent = state.packagesError ?? "";
    if (state.packagesError) packagesMessage.dataset.kind = "error";
    else delete packagesMessage.dataset.kind;
  };

  let renderedEntries = "";
  const renderChat = (state: AgentUiState): void => {
    const chat = state.chat;
    const entries = chat?.entries ?? [];
    const agentName = state.status?.agent?.name ?? "Agent";
    const configured = state.status !== null && state.status.state !== "unconfigured";
    const key = JSON.stringify([agentName, entries.map((entry) => entry.id)]);
    if (key !== renderedEntries) {
      renderedEntries = key;
      chatLog.replaceChildren(...entries.map((entry) => {
        const item = document.createElement("li");
        item.className = "chat-entry";
        item.dataset.role = entry.role;
        const author = document.createElement("span");
        author.className = "chat-author";
        author.textContent = entry.role === "user" ? "You" : agentName;
        const content = document.createElement("p");
        content.className = "chat-content";
        content.textContent = entry.content;
        item.append(author, content);
        return item;
      }));
      chatLog.scrollTop = chatLog.scrollHeight;
    }
    chatEmpty.hidden = entries.length > 0;
    chatEmpty.textContent = configured
      ? "No messages yet. Say hello."
      : "Add the endpoint, model and API key on the Configuration tab before chatting.";

    const pending = chat?.pending ?? false;
    const failure = state.chatError ?? (chat?.lastError ? describeChatError(chat.lastError.code) : undefined);
    if (failure) {
      chatMessage.textContent = failure;
      chatMessage.dataset.kind = "error";
    } else if (pending) {
      chatMessage.textContent = `${agentName} is replying...`;
      chatMessage.dataset.kind = "notice";
    } else {
      chatMessage.textContent = "";
      delete chatMessage.dataset.kind;
    }
    chatLog.setAttribute("aria-busy", String(pending));
    sendButton.disabled = !configured || pending || state.chatBusy;
    clearChatButton.disabled = state.chatBusy || (entries.length === 0 && !pending);
  };

  const controller = createAgentController(context.client, render);

  const readSettings = (): Settings => ({
    endpointBaseUrl: endpointInput.value.trim(),
    model: modelInput.value.trim(),
    systemPrompt: promptInput.value,
    contextMessages: Math.min(64, Math.max(0, Math.trunc(Number(contextInput.value) || 0))),
  });
  const onSave = (): void => { void controller.saveSettings(readSettings()); };
  const onSetKey = (): void => {
    const value = keyInput.value;
    keyInput.value = "";
    if (value.trim()) void controller.setKey(value);
  };
  const onClearKey = (): void => { void controller.clearKey(); };
  const onBind = (): void => { if (picker.value) void controller.bind(picker.value); };
  const onUnbind = (): void => { void controller.unbind(); };
  const onRefresh = (): void => { void controller.refreshCandidates(); };
  const showTab = (id: string, focus = false): void => {
    selectTab(id, focus);
    // A hidden transcript has no height, so the latest message is scrolled to once shown.
    if (id === "chat") chatLog.scrollTop = chatLog.scrollHeight;
  };
  const onTabClick = (event: MouseEvent): void => {
    const tab = (event.target as Element | null)?.closest<HTMLElement>("[role=tab]");
    if (tab?.dataset.tab) showTab(tab.dataset.tab);
  };
  const onTabKey = (event: KeyboardEvent): void => {
    const index = tabs.findIndex((entry) => entry.tab === event.target);
    if (index < 0) return;
    const step = event.key === "ArrowRight" ? 1 : event.key === "ArrowLeft" ? -1 : 0;
    const next = event.key === "Home" ? tabs[0] : event.key === "End" ? tabs[tabs.length - 1] : step ? tabs[(index + step + tabs.length) % tabs.length] : undefined;
    if (!next) return;
    event.preventDefault();
    showTab(next.id, true);
  };
  const onSendChat = (event: SubmitEvent): void => {
    event.preventDefault();
    const content = chatInput.value.trim();
    if (!content || sendButton.disabled) return;
    void controller.sendChat(content).then((accepted) => {
      if (accepted && chatInput.value.trim() === content) chatInput.value = "";
    });
  };
  const onChatKey = (event: KeyboardEvent): void => {
    if (event.key === "Enter" && !event.shiftKey && !event.isComposing) {
      event.preventDefault();
      chatForm.requestSubmit();
    }
  };
  const onClearChat = (): void => { void controller.clearChat(); };
  const onPackageToggle = (event: Event): void => {
    const toggle = event.target as HTMLInputElement | null;
    const contract = toggle?.dataset.contract;
    if (contract) void controller.setPackage(contract, toggle.checked);
  };
  saveButton.addEventListener("click", onSave);
  setKeyButton.addEventListener("click", onSetKey);
  clearKeyButton.addEventListener("click", onClearKey);
  bindButton.addEventListener("click", onBind);
  unbindButton.addEventListener("click", onUnbind);
  refreshButton.addEventListener("click", onRefresh);
  tabList.addEventListener("click", onTabClick);
  tabList.addEventListener("keydown", onTabKey);
  chatForm.addEventListener("submit", onSendChat);
  chatInput.addEventListener("keydown", onChatKey);
  clearChatButton.addEventListener("click", onClearChat);
  packageList.addEventListener("change", onPackageToggle);

  await controller.start();

  let unmounted = false;
  return () => {
    if (unmounted) return;
    unmounted = true;
    controller.dispose();
    saveButton.removeEventListener("click", onSave);
    setKeyButton.removeEventListener("click", onSetKey);
    clearKeyButton.removeEventListener("click", onClearKey);
    bindButton.removeEventListener("click", onBind);
    unbindButton.removeEventListener("click", onUnbind);
    refreshButton.removeEventListener("click", onRefresh);
    tabList.removeEventListener("click", onTabClick);
    tabList.removeEventListener("keydown", onTabKey);
    chatForm.removeEventListener("submit", onSendChat);
    chatInput.removeEventListener("keydown", onChatKey);
    clearChatButton.removeEventListener("click", onClearChat);
    packageList.removeEventListener("change", onPackageToggle);
    page.remove();
    cleanupStyles();
    cleanupKit();
  };
}
