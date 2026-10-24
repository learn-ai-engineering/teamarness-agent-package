export const TEAMARNESS_THEME_TOKENS = Object.freeze([
  "--teamarness-color-background",
  "--teamarness-color-panel",
  "--teamarness-color-text",
  "--teamarness-color-text-muted",
  "--teamarness-color-border",
  "--teamarness-color-accent",
  "--teamarness-color-danger",
  "--teamarness-color-focus",
  "--teamarness-font-body",
  "--teamarness-font-mono",
  "--teamarness-radius-control",
  "--teamarness-radius-panel",
] as const);

export type TeamarnessThemeToken = (typeof TEAMARNESS_THEME_TOKENS)[number];

const PACKAGE_UI_KIT_CSS = `
  :host, :host * { box-sizing: border-box; }

  /* Inherit the complete surrounding font instead of selecting var(--teamarness-font-body). */
  .teamarness-control {
    border: 1px solid var(--teamarness-color-border);
    border-radius: var(--teamarness-radius-control);
    background: var(--teamarness-color-panel);
    color: var(--teamarness-color-text);
    font: inherit;
    padding: 0.55rem 0.8rem;
  }
  .teamarness-control--primary {
    border-color: var(--teamarness-color-accent);
    background: var(--teamarness-color-accent);
    color: var(--teamarness-color-background);
  }
  .teamarness-control--secondary {
    border-color: var(--teamarness-color-border);
    background: var(--teamarness-color-panel);
    color: var(--teamarness-color-text);
  }
  .teamarness-field { display: grid; gap: 0.35rem; color: var(--teamarness-color-text); }
  .teamarness-input {
    width: 100%;
    border: 1px solid var(--teamarness-color-border);
    border-radius: var(--teamarness-radius-control);
    background: var(--teamarness-color-background);
    color: var(--teamarness-color-text);
    font: inherit;
    padding: 0.55rem 0.65rem;
  }
  .teamarness-help { color: var(--teamarness-color-text-muted); }
  .teamarness-validation { color: var(--teamarness-color-danger); font-family: var(--teamarness-font-mono); }
  .teamarness-notice {
    border: 1px solid var(--teamarness-color-border);
    border-radius: var(--teamarness-radius-control);
    background: var(--teamarness-color-panel);
    color: var(--teamarness-color-text);
    padding: 0.75rem;
  }
  .teamarness-notice--danger { border-color: var(--teamarness-color-danger); color: var(--teamarness-color-danger); }
  .teamarness-panel {
    border: 1px solid var(--teamarness-color-border);
    border-radius: var(--teamarness-radius-panel);
    background: var(--teamarness-color-panel);
    color: var(--teamarness-color-text);
    padding: 1rem;
  }
  .teamarness-stack { display: flex; flex-direction: column; gap: 0.75rem; }
  .teamarness-inline { display: flex; flex-wrap: wrap; align-items: center; gap: 0.75rem; }
  /* Page structure: opt-in primitives whose values mirror the Shell's own screens. */
  .teamarness-page { display: block; padding: clamp(1.25rem, 4vw, 3rem); color: var(--teamarness-color-text); }
  .teamarness-page > * + * { margin-top: 2rem; }
  .teamarness-page--readable { max-width: 48rem; }
  .teamarness-page-header { display: block; }
  .teamarness-eyebrow {
    margin: 0 0 0.4rem;
    color: var(--teamarness-color-accent);
    font-size: 0.75rem;
    font-weight: 800;
    letter-spacing: 0.08em;
    text-transform: uppercase;
  }
  .teamarness-page-title { margin: 0 0 0.5rem; font-size: clamp(1.7rem, 4vw, 2.5rem); }
  .teamarness-page-subtitle { max-width: 40rem; margin: 0; color: var(--teamarness-color-text-muted); }
  .teamarness-section {
    padding: 1.25rem;
    background: var(--teamarness-color-panel);
    border: 1px solid var(--teamarness-color-border);
    border-radius: var(--teamarness-radius-panel);
  }
  .teamarness-section-title { margin: 0 0 0.75rem; font-size: 1.05rem; }
  .teamarness-focus:focus-visible,
  .teamarness-control:focus-visible,
  .teamarness-input:focus-visible {
    outline: 3px solid var(--teamarness-color-focus);
    outline-offset: 2px;
  }
`;

/**
 * One record per shadow root, owning everything that root's kit installation holds: the adopted
 * sheet, the input suppressor's teardown, and the number of live installers.
 *
 * The suppressor's teardown belongs here rather than to the caller that happened to create it.
 * Overlapping mounts share one root, so the cleanup that takes `users` to zero is not necessarily
 * the one that installed the suppressor, and it must still be able to stop it.
 */
const installedKits = new WeakMap<
  ShadowRoot,
  { readonly sheet: CSSStyleSheet; readonly stopSuppressor: () => void; users: number }
>();

/**
 * Attributes that suppress WebKit autocorrection / autocapitalisation /
 * spelling suggestions on macOS Safari / WKWebView.
 */
const SUPPRESS_ATTRS: Record<string, string> = {
  autocapitalize: "off",
  autocorrect: "off",
  spellcheck: "false",
  writingsuggestions: "false",
} as const;

const INPUT_SELECTORS = "input,textarea,[contenteditable]";

/**
 * Walk a node (or its subtree) and suppress autocorrect attributes on
 * every input, textarea, and contenteditable element.
 */
function suppressInputs(node: Node): void {
  // Use tagName checks so this works in environments without
  // global HTMLInputElement / HTMLTextAreaElement constructors.
  if (node.nodeType === 1 /* ELEMENT_NODE */) {
    const el = node as Element;
    const tag = el.tagName;
    if (tag === "INPUT" || tag === "TEXTAREA") {
      for (const [attr, value] of Object.entries(SUPPRESS_ATTRS)) {
        el.setAttribute(attr, value);
      }
    }
    for (const child of el.querySelectorAll<HTMLElement>(INPUT_SELECTORS)) {
      for (const [attr, value] of Object.entries(SUPPRESS_ATTRS)) {
        child.setAttribute(attr, value);
      }
    }
  }
}

/**
 * Start a MutationObserver that suppresses autocorrect on any
 * input/textarea/contenteditable added to or within this shadow root.
 *
 * Returns the idempotent teardown, which the caller is responsible for storing somewhere the last
 * cleanup of this root can reach.
 */
function installInputSuppressor(root: ShadowRoot): () => void {
  if (typeof MutationObserver !== "function") {
    // Environment (e.g. test runner) lacks MutationObserver: the per-install sweep below is all
    // the suppression this root gets, and there is nothing to disconnect.
    return () => undefined;
  }

  const observer = new MutationObserver((mutations) => {
    for (const mutation of mutations) {
      for (const node of mutation.addedNodes) {
        suppressInputs(node);
      }
    }
  });
  observer.observe(root, { childList: true, subtree: true });

  let disconnected = false;
  return () => {
    if (disconnected) return;
    disconnected = true;
    observer.disconnect();
  };
}

/**
 * The leading phrase of every stylesheet failure, so the message names what the caller asked for
 * rather than the Package that asked for it.
 */
const KIT_NEEDS = "The Package UI kit requires";
const STYLES_NEEDS = "Package styles require";

function currentAdoptedSheets(root: ShadowRoot, needs = KIT_NEEDS): readonly CSSStyleSheet[] {
  if (!("adoptedStyleSheets" in root) || root.adoptedStyleSheets === undefined) {
    throw new Error(`${needs} ShadowRoot.adoptedStyleSheets support`);
  }
  return root.adoptedStyleSheets;
}

function createSheet(css: string, needs = KIT_NEEDS): CSSStyleSheet {
  const StyleSheet = globalThis.CSSStyleSheet;
  if (typeof StyleSheet !== "function") {
    throw new Error(`${needs} constructed CSSStyleSheet support`);
  }
  let sheet: CSSStyleSheet;
  try {
    sheet = new StyleSheet();
  } catch (cause) {
    throw new Error(`${needs} constructed CSSStyleSheet support`, { cause });
  }
  if (typeof sheet.replaceSync !== "function") {
    throw new Error(`${needs} CSSStyleSheet.replaceSync support`);
  }
  sheet.replaceSync(css);
  return sheet;
}

function setAdoptedSheets(root: ShadowRoot, sheets: CSSStyleSheet[], needs = KIT_NEEDS): void {
  try {
    root.adoptedStyleSheets = sheets;
  } catch (cause) {
    throw new Error(`${needs} mutable ShadowRoot.adoptedStyleSheets support`, { cause });
  }
}

/**
 * Adopts one Package-owned stylesheet into a mount's shadow root.
 *
 * The Package keeps its CSS in its own bundle; this only puts it where the mount can use it. The
 * returned cleanup is idempotent and removes exactly this sheet, never anything else the root has
 * adopted.
 */
export function adoptPackageStyles(root: ShadowRoot, css: string): () => void {
  const sheet = createSheet(css, STYLES_NEEDS);
  setAdoptedSheets(root, [...currentAdoptedSheets(root, STYLES_NEEDS), sheet], STYLES_NEEDS);
  if (!currentAdoptedSheets(root, STYLES_NEEDS).includes(sheet)) {
    throw new Error(`${STYLES_NEEDS} mutable ShadowRoot.adoptedStyleSheets support`);
  }
  let cleaned = false;
  return () => {
    if (cleaned) return;
    cleaned = true;
    const remaining = currentAdoptedSheets(root, STYLES_NEEDS).filter((entry) => entry !== sheet);
    setAdoptedSheets(root, remaining, STYLES_NEEDS);
  };
}

export function installPackageUiKit(root: ShadowRoot): () => void {
  let installation = installedKits.get(root);
  if (installation === undefined) {
    const sheet = createSheet(PACKAGE_UI_KIT_CSS);
    setAdoptedSheets(root, [...currentAdoptedSheets(root), sheet]);
    if (!currentAdoptedSheets(root).includes(sheet)) {
      throw new Error("The Package UI kit requires mutable ShadowRoot.adoptedStyleSheets support");
    }
    // The suppressor starts alongside the kit and is torn down by whichever cleanup takes the
    // last user away, so its teardown is stored on the installation, not captured per caller.
    installation = { sheet, stopSuppressor: installInputSuppressor(root), users: 0 };
    installedKits.set(root, installation);
  }
  installation.users += 1;
  // Suppress the elements this installer's mount has already put in the root; the observer only
  // sees what arrives after it starts.
  suppressInputs(root);

  let cleaned = false;
  return () => {
    if (cleaned) return;
    cleaned = true;
    const current = installedKits.get(root);
    if (current !== installation) return;
    current.users -= 1;
    if (current.users === 0) {
      // Release the root before removing the sheet: an environment that refuses the write must
      // not be able to strand the observer or leave a spent record behind.
      installedKits.delete(root);
      current.stopSuppressor();
      setAdoptedSheets(root, currentAdoptedSheets(root).filter((sheet) => sheet !== current.sheet));
    }
  };
}

export type PackageValue = null | boolean | number | string | readonly PackageValue[] | { readonly [key: string]: PackageValue };

export type PackagePresentation = Readonly<{
  identity: string;
  version: string;
  displayName: string;
  source: "bundled" | "local";
  trust: "application_bundled" | "trusted_native_code_v1";
  lifecycle: "active";
  /** A decimal string, as the Package Process Protocol and the Rust SDK carry it. */
  processGeneration: string;
}>;
export type WorkspacePresentation = Readonly<{ id: string; name: string }>;

/**
 * The brand on a rejected Package request.
 *
 * The Shell builds the frame runtime and each Package bundles its own copy of this module, so
 * class identity differs between the two. A branded frozen plain object with a structural guard
 * works across both copies and stays structured-clone safe.
 */
export const PACKAGE_REQUEST_ERROR_KIND = "teamarness.package-request-error";

/** A Package Backend's own terminal error, forwarded unchanged within the host's bounds. */
export type PackageOwnedError = Readonly<{
  code: string;
  retryable: boolean;
  details: { readonly [key: string]: PackageValue };
}>;

/** What `PackageClient.request` rejects with when the Package Host answers with an error. */
export type PackageRequestError = Readonly<{
  kind: typeof PACKAGE_REQUEST_ERROR_KIND;
  version: number;
  namespace: string;
  code: string;
  stage: string;
  retryable: boolean;
  recovery: string;
  diagnosticId: string;
  /** The Package's own error, or `null` when the host owns the failure. */
  packageError: PackageOwnedError | null;
}>;

function isPlainRecord(value: unknown): value is Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return false;
  const prototype = Object.getPrototypeOf(value) as unknown;
  return prototype === Object.prototype || prototype === null;
}

function isPackageOwnedError(value: unknown): value is PackageOwnedError {
  if (!isPlainRecord(value)) return false;
  return typeof value.code === "string"
    && typeof value.retryable === "boolean"
    && isPlainRecord(value.details);
}

/** Whether `value` is a Package request error, whichever bundle built it. */
export function isPackageRequestError(value: unknown): value is PackageRequestError {
  if (typeof value !== "object" || value === null) return false;
  const candidate = value as Record<string, unknown>;
  if (candidate.kind !== PACKAGE_REQUEST_ERROR_KIND) return false;
  if (typeof candidate.version !== "number") return false;
  if (typeof candidate.namespace !== "string" || typeof candidate.code !== "string") return false;
  if (typeof candidate.stage !== "string" || typeof candidate.recovery !== "string") return false;
  if (typeof candidate.diagnosticId !== "string" || typeof candidate.retryable !== "boolean") return false;
  return candidate.packageError === null || isPackageOwnedError(candidate.packageError);
}

/** The Package Backend's own error code, or `null` when the host owns the failure. */
export function packageErrorCode(error: unknown): string | null {
  return isPackageRequestError(error) ? error.packageError?.code ?? null : null;
}

/** Whether `error` is the cancellation a Package request rejects with. */
export function isAbortError(error: unknown): boolean {
  return typeof error === "object" && error !== null
    && (error as { name?: unknown }).name === "AbortError";
}

/**
 * A short, presentable description of a failed request.
 *
 * It never includes `details`: those are the Package's own data, not display text.
 */
export function describePackageError(error: unknown): string {
  if (isPackageRequestError(error)) {
    const packageCode = error.packageError?.code;
    if (packageCode !== undefined) return packageCode;
    return `${error.code} (${error.stage})`;
  }
  if (error instanceof Error && error.message.length > 0) return error.message;
  return "Request failed";
}

export interface PackageSubscription {
  close(): Promise<void>;
}

/** Options for `PackageClient.subscribe`. */
export type PackageSubscribeOptions = Readonly<{
  /**
   * Called once when the stream ends: with the Package's own terminal error, or `null` when the
   * Package completed the stream normally.
   */
  onTerminated?: (error: PackageRequestError | null) => void;
}>;

export interface PackageClient {
  request<TResponse extends PackageValue = PackageValue>(
    operation: string,
    payload: PackageValue,
    options?: Readonly<{ signal?: AbortSignal }>,
  ): Promise<TResponse>;
  subscribe<TEvent extends PackageValue = PackageValue>(
    operation: string,
    payload: PackageValue,
    listener: (event: TEvent) => void | Promise<void>,
    options?: PackageSubscribeOptions,
  ): Promise<PackageSubscription>;
}

export type PackageMountContext = Readonly<{
  root: ShadowRoot;
  package: PackagePresentation;
  workspace: WorkspacePresentation;
  uiHostVersion: number;
  themeTokens: readonly TeamarnessThemeToken[];
  client: PackageClient;
}>;

export type PackageUnmount = () => void | Promise<void>;
export type PackageUiModule = Readonly<{
  mount(context: PackageMountContext): PackageUnmount | Promise<PackageUnmount>;
}>;

/**
 * The shape a generated contract type has: every operation with its request and answer.
 *
 * `teamarness-contract-types` writes one of these from `ui-contract.json`, so the types a UI
 * programs against and the shapes the Host validates come from the same file.
 */
export type PackageContractShape = {
  readonly requests: {
    readonly [operation: string]: { readonly request: PackageValue; readonly response: PackageValue };
  };
  readonly subscriptions: {
    readonly [operation: string]: { readonly request: PackageValue; readonly event: PackageValue };
  };
};

/** A [`PackageClient`] whose operation names, payloads and answers come from one contract. */
export interface TypedPackageClient<C extends PackageContractShape> {
  request<K extends keyof C["requests"] & string>(
    operation: K,
    payload: C["requests"][K]["request"],
    options?: Readonly<{ signal?: AbortSignal }>,
  ): Promise<C["requests"][K]["response"]>;
  subscribe<K extends keyof C["subscriptions"] & string>(
    operation: K,
    payload: C["subscriptions"][K]["request"],
    listener: (event: C["subscriptions"][K]["event"]) => void | Promise<void>,
    options?: PackageSubscribeOptions,
  ): Promise<PackageSubscription>;
}

/**
 * Views the mount's client through one contract's types.
 *
 * It is the same object: nothing is validated here, because the Host already validates every
 * request, response and event against the contract it admitted. What this adds is that a UI
 * cannot name an operation, or build a payload, the contract does not describe.
 */
export function typedPackageClient<C extends PackageContractShape>(
  client: PackageClient,
): TypedPackageClient<C> {
  return client as TypedPackageClient<C>;
}
