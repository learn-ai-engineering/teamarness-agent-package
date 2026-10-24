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
import type { AgentContract, Candidate, Settings, Status } from "./contract.generated";

export type { Candidate, Settings, Status };

// ─── State ───────────────────────────────────────────────────────────────────

export type AgentUiState = Readonly<{
  status: Status | null;
  settings: Settings | null;
  hasKey: boolean;
  candidates: readonly Candidate[];
  busy: boolean;
  error: string | undefined;
  notice: string | undefined;
}>;

export type AgentController = Readonly<{
  start(): Promise<void>;
  saveSettings(settings: Settings): Promise<void>;
  setKey(apiKey: string): Promise<void>;
  clearKey(): Promise<void>;
  refreshCandidates(): Promise<void>;
  bind(principalId: string): Promise<void>;
  unbind(): Promise<void>;
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
  let state: AgentUiState = {
    status: null,
    settings: null,
    hasKey: false,
    candidates: [],
    busy: false,
    error: undefined,
    notice: undefined,
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

  const loadCandidates = async (): Promise<readonly Candidate[]> =>
    (await typed.request("agent.binding.candidates", {})).agents;

  return Object.freeze({
    async start(): Promise<void> {
      await run(async () => {
        const settings = await typed.request("agent.settings.get", {});
        const status = await typed.request("agent.status.get", {});
        const candidates = await loadCandidates();
        subscription = await typed.subscribe("agent.status.changes", {}, (event) => {
          update({ status: event });
        });
        return { settings: settings.settings, hasKey: settings.hasKey, status, candidates };
      });
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

    dispose(): void {
      disposed = true;
      const open = subscription;
      subscription = null;
      if (open) void open.close().catch(() => undefined);
    },
  });
}

// ─── Styles ──────────────────────────────────────────────────────────────────

const AGENT_CSS = `
  :host { color: var(--teamarness-color-text); font-family: var(--teamarness-font-body); }
  .teamarness-section p, .teamarness-section-title { margin: 0; }
  .agent-settings { display: grid; gap: 1.25rem; }
  .agent-settings > section { min-width: 0; }
  .agent-settings > section > button { align-self: flex-start; }
  .row { display: flex; flex-wrap: wrap; gap: .6rem; align-items: center; }
  textarea.teamarness-input { min-height: 6rem; resize: vertical; }
  .agent-picker { position: relative; display: block; }
  .agent-picker-control { appearance: none; -webkit-appearance: none; min-height: 2.65rem; padding-right: 2.5rem; cursor: pointer; }
  .agent-picker::after { content: ''; position: absolute; pointer-events: none; right: 1rem; top: 50%; width: .4rem; height: .4rem; border-right: 2px solid var(--teamarness-color-text-muted); border-bottom: 2px solid var(--teamarness-color-text-muted); transform: translateY(-75%) rotate(45deg); }
  .status { font-weight: 600; }
  .status[data-state="error"] { color: var(--teamarness-color-danger, #b42318); }
  .status[data-state="ready"] { color: var(--teamarness-color-accent); }
  .message { min-height: 1.2em; margin: 0; }
  .message:empty { min-height: 0; }
  .message[data-kind="error"] { color: var(--teamarness-color-danger, #b42318); }
  .message[data-kind="notice"] { color: var(--teamarness-color-text-muted); }
  button:disabled { opacity: .5; cursor: default; }
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
  page.className = "teamarness-page teamarness-page--readable";
  const header = document.createElement("header");
  header.className = "teamarness-page-header";
  const title = document.createElement("h1");
  title.className = "teamarness-page-title";
  title.textContent = context.package.displayName;
  const subtitle = document.createElement("p");
  subtitle.className = "teamarness-page-subtitle";
  subtitle.textContent = "Run one of your Workspace Agents against an OpenAI-compatible endpoint.";
  header.append(title, subtitle);

  const form = document.createElement("form");
  form.className = "agent-settings";
  form.noValidate = true;
  form.addEventListener("submit", (event) => event.preventDefault());

  // Status
  const statusSection = document.createElement("section");
  statusSection.className = "teamarness-section teamarness-stack";
  const statusTitle = document.createElement("h2");
  statusTitle.className = "teamarness-section-title";
  statusTitle.textContent = "Status";
  const statusLine = document.createElement("p");
  statusLine.className = "status";
  statusLine.dataset.role = "status";
  statusLine.setAttribute("role", "status");
  const message = document.createElement("p");
  message.className = "message";
  message.dataset.role = "message";
  message.setAttribute("role", "status");
  statusSection.append(statusTitle, statusLine, message);

  // Agent picker
  const agentSection = document.createElement("section");
  agentSection.className = "teamarness-section teamarness-stack";
  const agentTitle = document.createElement("h2");
  agentTitle.className = "teamarness-section-title";
  agentTitle.textContent = "Agent";
  const agentHelp = document.createElement("p");
  agentHelp.className = "teamarness-help";
  agentHelp.textContent = "Only Agents you steward on this Installation are offered.";
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
  agentSection.append(agentTitle, agentHelp, field("Workspace Agent", pickerControl), pickerRow);

  // Endpoint settings
  const settingsSection = document.createElement("section");
  settingsSection.className = "teamarness-section teamarness-stack";
  const settingsTitle = document.createElement("h2");
  settingsTitle.className = "teamarness-section-title";
  settingsTitle.textContent = "Model endpoint";
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
  settingsSection.append(
    settingsTitle,
    field("Endpoint base URL", endpointInput, "An OpenAI-compatible base URL; /chat/completions is appended."),
    field("Model", modelInput),
    field("System prompt", promptInput),
    field("Context messages", contextInput, "How many earlier Messages a reply sees, 0 to 64."),
    saveButton,
  );

  // API key
  const keySection = document.createElement("section");
  keySection.className = "teamarness-section teamarness-stack";
  const keyTitle = document.createElement("h2");
  keyTitle.className = "teamarness-section-title";
  keyTitle.textContent = "API key";
  const keyState = document.createElement("p");
  keyState.dataset.role = "key-state";
  const keyInput = input("apiKey", "password");
  const keyRow = document.createElement("div");
  keyRow.className = "row";
  const setKeyButton = button("Store key", "set-key", "primary");
  const clearKeyButton = button("Clear key", "clear-key", "secondary");
  keyRow.append(setKeyButton, clearKeyButton);
  keySection.append(keyTitle, keyState,
    field("New API key", keyInput, "Stored in this Package's data directory and never shown again."), keyRow);

  form.append(statusSection, agentSection, settingsSection, keySection);
  page.append(header, form);
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
  saveButton.addEventListener("click", onSave);
  setKeyButton.addEventListener("click", onSetKey);
  clearKeyButton.addEventListener("click", onClearKey);
  bindButton.addEventListener("click", onBind);
  unbindButton.addEventListener("click", onUnbind);
  refreshButton.addEventListener("click", onRefresh);

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
    page.remove();
    cleanupStyles();
    cleanupKit();
  };
}
