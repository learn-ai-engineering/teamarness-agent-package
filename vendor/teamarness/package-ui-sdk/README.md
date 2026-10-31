# Teamarness Package UI SDK

This source is generated from the Teamarness checkout that created this project. It is vendored so the project remains buildable without that checkout. There is no supported in-place SDK upgrade yet; regenerate from a compatible Teamarness source when instructed.

`@teamarness/package-ui-sdk` is the TypeScript contract for a Package UI mounted by the Teamarness Shell. It provides the mount, client, and presentation types, the typed Package error, contract-typed requests, Package-owned style adoption, the authoritative theme-token names, an optional framework-neutral CSS kit, and a testing entry point. The client implementation is Shell-owned and lives in the Package UI Frame; the SDK carries no Shell or Tauri code.

A Package owns everything it renders inside its Shadow Root. The Shell supplies the mount and inherited semantic values; it does not own a Package-owned header, secondary navigation, content hierarchy, dialogs, tables, or domain patterns. The CSS kit styles only classes a Package explicitly applies.

## Theme tokens

`TEAMARNESS_THEME_TOKENS` is the exact frozen token-name authority. `TeamarnessThemeToken` is derived from that constant, and `PackageMountContext.themeTokens` is typed with it. The Shell sends values across the frame bridge and the frame runtime defines them on the Package UI Frame's document root, so changes reach the Shadow Root without a theme event.

| Token | Intended use |
|---|---|
| `--teamarness-color-background` | Main Package surface and input backgrounds |
| `--teamarness-color-panel` | Raised or grouped surfaces |
| `--teamarness-color-text` | Primary text |
| `--teamarness-color-text-muted` | Secondary and help text |
| `--teamarness-color-border` | Dividers and neutral control borders |
| `--teamarness-color-accent` | Primary actions and selected emphasis |
| `--teamarness-color-danger` | Errors, destructive emphasis, and validation |
| `--teamarness-color-focus` | Visible keyboard-focus outlines |
| `--teamarness-font-body` | Shell-compatible body font stack |
| `--teamarness-font-mono` | Values that benefit from monospaced alignment |
| `--teamarness-radius-control` | Controls, inputs, and notices |
| `--teamarness-radius-panel` | Larger grouped surfaces |

Use the semantic purpose rather than assuming a particular light or dark value:

```css
:host {
  color: var(--teamarness-color-text);
  background: var(--teamarness-color-background);
  font-family: var(--teamarness-font-body);
}
```

## Views

`context.view` is the view the mount shows: `"main"`, or `"settings"` for a Package whose manifest declares `"views": ["settings"]` in its `ui` object. A UI that declares no views can ignore it. `PACKAGE_UI_VIEWS` lists both values, and `PACKAGE_UI_HOST_VERSION` is the mount contract version this SDK describes, 2. Version 2 adds `context.view`, and a UI written for version 1 keeps working (ADR 0037). In tests, `createTestMountContext` takes an optional `view`, which defaults to `"main"`.

## Optional CSS kit

Call `installPackageUiKit(root)` during mount. It constructs one `CSSStyleSheet` and adds it to that Shadow Root's `adoptedStyleSheets`, even when installers overlap. Keep the returned cleanup function and call it during unmount. Each cleanup is idempotent. The sheet remains until every overlapping installer has cleaned up, and cleanup preserves every unrelated adopted sheet.

```ts
import { installPackageUiKit, type PackageMountContext } from "@teamarness/package-ui-sdk";

export function mount(context: PackageMountContext): () => void {
  const cleanupKit = installPackageUiKit(context.root);
  const content = document.createElement("section");
  content.className = "teamarness-panel teamarness-stack";
  context.root.append(content);

  let unmounted = false;
  return () => {
    if (unmounted) return;
    unmounted = true;
    content.remove();
    cleanupKit();
  };
}
```

The kit publishes these opt-in classes:

| Class | Purpose |
|---|---|
| `.teamarness-control` | Common button-like control foundation |
| `.teamarness-control--primary` | Primary-action treatment, used with the control foundation |
| `.teamarness-control--secondary` | Secondary-action treatment, used with the control foundation |
| `.teamarness-field` | Vertical field-label, input, and help flow |
| `.teamarness-input` | Textual input surface |
| `.teamarness-help` | Muted field guidance |
| `.teamarness-validation` | Validation or error text |
| `.teamarness-notice` | Status or informational notice |
| `.teamarness-notice--danger` | Danger treatment, used with the notice foundation |
| `.teamarness-panel` | Grouped Package-owned surface |
| `.teamarness-stack` | Vertical flow with a standard gap |
| `.teamarness-inline` | Wrapping horizontal flow with a standard gap |
| `.teamarness-focus` | Visible `:focus-visible` outline for a custom focusable element |

### Page structure

The kit also publishes opt-in page-structure classes. Their values mirror the Shell's own screens, so a Package page can sit comfortably next to Shell settings without copying Shell CSS. Nothing applies them automatically.

| Class | Purpose |
|---|---|
| `.teamarness-page` | Page padding, with spacing between top-level blocks |
| `.teamarness-page--readable` | Readable maximum width for form and document pages, used with the page foundation |
| `.teamarness-page-header` | Page header block |
| `.teamarness-eyebrow` | Small accent label above a title |
| `.teamarness-page-title` | Page title, usually the `h1` |
| `.teamarness-page-subtitle` | Muted one-sentence summary under the title |
| `.teamarness-section` | Grouped page section surface |
| `.teamarness-section-title` | Section heading, usually an `h2` |

```html
<main class="teamarness-page teamarness-page--readable">
  <header class="teamarness-page-header">
    <h1 class="teamarness-page-title">Milestones</h1>
    <p class="teamarness-page-subtitle">Track what the team is shipping next.</p>
  </header>
  <section class="teamarness-section" aria-labelledby="current-milestone">
    <h2 class="teamarness-section-title" id="current-milestone">Current milestone</h2>
    <!-- Package-owned domain content -->
  </section>
</main>
```

The installer adds only local box sizing outside those classes. It does not select or modify Package content, and it does not install styles in `document`.

The host's `style-src` rejects runtime inline `<style>` elements and `style` attributes. The installer does not bypass that policy. It fails the Package mount if constructed stylesheets or mutable `ShadowRoot.adoptedStyleSheets` are unavailable. Do not add an inline fallback, nonce, hash, `data:` style, or `blob:` style.

## Package-owned styles

Package-owned CSS follows the same `style-src` policy. `adoptPackageStyles(root, css)` constructs one `CSSStyleSheet`, adds it to that Shadow Root's `adoptedStyleSheets`, and returns the idempotent cleanup that removes only that sheet:

```ts
import { adoptPackageStyles } from "@teamarness/package-ui-sdk";

const cleanupStyles = adoptPackageStyles(context.root, PACKAGE_STYLES);
// ... and in the unmount:
cleanupStyles();
```

Keep Package rules in their own sheet, separate from the optional kit, so ownership and cleanup remain clear.

## Package errors

`PackageClient.request` rejects with a `PackageRequestError`: the Teamarness `code`, `namespace`, `stage`, `retryable`, `recovery` and `diagnosticId` that say what Teamarness did with the failure, plus `packageError` — the Package Backend's own `code`, `retryable` and bounded `details`, forwarded unchanged — or `null` when Teamarness owns the failure.

```ts
import { describePackageError, isAbortError, isPackageRequestError, packageErrorCode } from "@teamarness/package-ui-sdk";

try {
  await client.request("example.save", payload);
} catch (error) {
  if (isAbortError(error)) return;                       // the mount went away
  if (packageErrorCode(error) === "example.storage_failed") retryLater();
  else if (isPackageRequestError(error)) show(describePackageError(error));
}
```

`packageErrorCode` returns the Package's own code or `null`; `describePackageError` returns a display sentence and never includes `details`. Treat `details` as the Package's data, not display text.

`subscribe`'s `onTerminated` is called once when a stream ends: with the Package's terminal `PackageRequestError`, or `null` when the Package completed the stream. Neither recovers the Package UI Mount by itself; a Teamarness-owned terminal stream error still does.

## Contract-typed requests

`teamarness-contract-types` generates a `PackageContractShape` from `ui-contract.json`, so the types the UI programs against and the shapes the Host validates come from one file:

```sh
teamarness-contract-types --contract ui-contract.json --out ui/src/contract.generated.ts --type-name PackageContract
```

Commit the generated module and run the same command with `--check` from `typecheck`, so a stale module fails the build. `typedPackageClient<PackageContract>(context.client)` then views the mount's client through those types. It is the same object and adds no runtime work: the Host already validates every request, response and event against the contract it admitted. What it adds is that a UI cannot name an operation, or build a payload, the contract does not describe.

## Testing a Package UI

`@teamarness/package-ui-sdk/testing` is a second entry point, outside the main export list, for a Package UI's own tests: `createTestMountContext` builds a `PackageMountContext` over a real Shadow Root, `createFakePackageClient` answers requests and subscriptions from handlers and records every call as a `RecordedCall`, and `createPackageRequestError` builds the rejection a Host would send, including a `packageError`. The fake client's `emit` delivers one event to every open subscription on an operation, and `terminate` ends them with a Package error or normally, so a UI's `onTerminated` path is testable without a Host.

## Package-owned composition

Build the Package's information architecture in Package markup. For example, this Package-owned header and secondary navigation use tokens and flow helpers without asking the Shell to interpret Package routes:

```html
<article class="teamarness-panel teamarness-stack">
  <header class="package-header teamarness-stack">
    <p class="teamarness-help">Planning</p>
    <h1>Milestones</h1>
    <nav class="teamarness-inline" aria-label="Milestone sections">
      <button class="teamarness-control teamarness-control--primary" aria-current="page">Board</button>
      <button class="teamarness-control teamarness-control--secondary">Activity</button>
    </nav>
  </header>
  <section aria-labelledby="current-milestone">
    <h2 id="current-milestone">Current milestone</h2>
    <!-- Package-owned domain content -->
  </section>
</article>
```

The Shell continues to own Workspace and Package selection, the right-pane mount, the Package UI Frame and its runtime, delivered token values, and generic mount recovery. Hiding or unmounting this UI does not suspend its Package Backend. Package UI code runs in a sandboxed frame with an opaque origin: the Shell's DOM, Tauri, browser storage, and the network are all unreachable, and `context.client` is the only way out. A Shadow Root still provides style locality, not a security boundary.
