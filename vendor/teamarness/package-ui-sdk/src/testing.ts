/**
 * Test helpers for Package UIs.
 *
 * A Package UI test should exercise the UI, not a hand-built double of the Shell. These helpers
 * build the two things every such test needs — a client that answers, and a mount context — with
 * the same shapes the Shell uses.
 */

import {
  PACKAGE_REQUEST_ERROR_KIND,
  TEAMARNESS_THEME_TOKENS,
  type PackageClient,
  type PackageMountContext,
  type PackageRequestError,
  type PackageSubscribeOptions,
  type PackageSubscription,
  type PackageValue,
} from "./index";

/** Builds a rejected request the way the Shell's frame client builds one. */
export function createPackageRequestError(
  input?: Partial<Omit<PackageRequestError, "kind" | "packageError">> & {
    packageError?: {
      code: string;
      retryable?: boolean;
      details?: { readonly [key: string]: PackageValue };
    } | null;
  },
): PackageRequestError {
  const packageError = input?.packageError ?? null;
  return Object.freeze({
    kind: PACKAGE_REQUEST_ERROR_KIND,
    version: input?.version ?? 2,
    namespace: input?.namespace ?? "teamarness.package-host",
    code: input?.code ?? "backend_error",
    stage: input?.stage ?? "backend",
    retryable: input?.retryable ?? false,
    recovery: input?.recovery ?? "none",
    diagnosticId: input?.diagnosticId ?? "test-diagnostic",
    packageError: packageError === null
      ? null
      : Object.freeze({
        code: packageError.code,
        retryable: packageError.retryable ?? false,
        details: Object.freeze({ ...packageError.details }),
      }),
  });
}

/** One request a UI made through the fake client. */
export type RecordedCall = Readonly<{ operation: string; payload: PackageValue }>;

export type FakePackageClient = Readonly<{
  client: PackageClient;
  /** Every request and subscription the UI made, in order. */
  calls: ReadonlyArray<RecordedCall>;
  /** Delivers one event to every open subscription on `operation`. */
  emit(operation: string, event: PackageValue): Promise<void>;
  /** Ends every open subscription on `operation`, with a Package error or normally. */
  terminate(operation: string, error: PackageRequestError | null): void;
}>;

type RequestHandler = (payload: PackageValue, signal?: AbortSignal) => PackageValue | Promise<PackageValue>;
type SubscriptionHandler = (payload: PackageValue) => void | Promise<void>;

type OpenSubscription = {
  readonly listener: (event: PackageValue) => void | Promise<void>;
  readonly options?: PackageSubscribeOptions;
  closed: boolean;
};

/**
 * A client that answers from scripted handlers and records what it was asked.
 *
 * An operation with no handler rejects the way the Host does when a UI names something the
 * contract does not describe.
 */
export function createFakePackageClient(handlers?: {
  requests?: { readonly [operation: string]: RequestHandler };
  subscriptions?: { readonly [operation: string]: SubscriptionHandler };
}): FakePackageClient {
  const calls: RecordedCall[] = [];
  const open = new Map<string, Set<OpenSubscription>>();

  const unhandled = (): PackageRequestError =>
    createPackageRequestError({ code: "ui_contract_violation", stage: "ui" });

  const client: PackageClient = {
    async request(operation, payload, options) {
      calls.push(Object.freeze({ operation, payload }));
      const handler = handlers?.requests?.[operation];
      if (handler === undefined) throw unhandled();
      if (options?.signal?.aborted === true) {
        throw Object.assign(new Error("aborted"), { name: "AbortError" });
      }
      return (await handler(payload, options?.signal)) as never;
    },
    async subscribe(operation, payload, listener, options) {
      calls.push(Object.freeze({ operation, payload }));
      const handler = handlers?.subscriptions?.[operation];
      if (handler === undefined) throw unhandled();
      await handler(payload);
      const entry: OpenSubscription = {
        listener: listener as (event: PackageValue) => void | Promise<void>,
        options,
        closed: false,
      };
      const entries = open.get(operation) ?? new Set<OpenSubscription>();
      entries.add(entry);
      open.set(operation, entries);
      const subscription: PackageSubscription = {
        close: async () => {
          entry.closed = true;
          entries.delete(entry);
          await Promise.resolve();
        },
      };
      return subscription;
    },
  };

  return Object.freeze({
    client,
    calls,
    async emit(operation, event) {
      for (const entry of open.get(operation) ?? []) {
        if (entry.closed) continue;
        await entry.listener(event);
      }
    },
    terminate(operation, error) {
      for (const entry of open.get(operation) ?? []) {
        if (entry.closed) continue;
        entry.closed = true;
        entry.options?.onTerminated?.(error);
      }
      open.get(operation)?.clear();
    },
  });
}

/** Builds the mount context a Package UI's `mount` is given. */
export function createTestMountContext(input: {
  root: unknown;
  client: PackageClient;
  displayName?: string;
}): PackageMountContext {
  return Object.freeze({
    root: input.root as ShadowRoot,
    package: Object.freeze({
      identity: "dev.teamarness.test",
      version: "1.0.0",
      displayName: input.displayName ?? "Test Package",
      source: "bundled",
      trust: "application_bundled",
      lifecycle: "active",
      processGeneration: "1",
    }),
    workspace: Object.freeze({ id: "workspace1", name: "Test Workspace" }),
    uiHostVersion: 1,
    themeTokens: TEAMARNESS_THEME_TOKENS,
    client: input.client,
  });
}
