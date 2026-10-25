// Generated from ui-contract.json by teamarness-contract-types. Do not edit.

export type Activity = {
  readonly "code"?: string;
  readonly "ok": boolean;
  readonly "package": string;
  readonly "port": string;
};

export type Agent = {
  readonly "name": Name;
  readonly "principalId": PrincipalId;
};

export type BindingView = {
  readonly "agent": Agent | null;
};

export type Candidate = {
  readonly "bound": boolean;
  readonly "name": Name;
  readonly "principalId": PrincipalId;
};

export type Candidates = {
  readonly "agents": readonly Candidate[];
};

export type Chat = {
  readonly "entries": readonly ChatEntry[];
  readonly "lastError"?: StatusError;
  readonly "pending": boolean;
};

export type ChatEntry = {
  readonly "activity"?: readonly Activity[];
  readonly "content": string;
  readonly "id": number;
  readonly "role": "user" | "assistant";
};

export type ContractIdentity = string;

export type Empty = Record<string, never>;

export type KeyView = {
  readonly "hasKey": boolean;
};

export type Name = string | null;

export type PackageView = {
  readonly "available": boolean;
  readonly "contract": ContractIdentity;
  readonly "description": string;
  readonly "enabled": boolean;
  readonly "name": string;
};

export type Packages = {
  readonly "packages": readonly PackageView[];
};

export type PrincipalId = string;

export type Settings = {
  readonly "contextMessages": number;
  readonly "endpointBaseUrl": string;
  readonly "model": string;
  readonly "systemPrompt": string;
};

export type SettingsView = {
  readonly "hasKey": boolean;
  readonly "settings": Settings;
};

export type Status = {
  readonly "agent"?: Agent;
  readonly "lastError"?: StatusError;
  readonly "state": "unconfigured" | "no_agent" | "ready" | "replying" | "error";
};

export type StatusError = {
  readonly "code": string;
  readonly "retryable": boolean;
};

export type AgentContract = {
  readonly requests: {
    readonly "agent.binding.candidates": { readonly request: Empty; readonly response: Candidates };
    readonly "agent.binding.clear": { readonly request: Empty; readonly response: BindingView };
    readonly "agent.binding.set": { readonly request: {
        readonly "principalId": PrincipalId;
      }; readonly response: BindingView };
    readonly "agent.chat.clear": { readonly request: Empty; readonly response: Chat };
    readonly "agent.chat.get": { readonly request: Empty; readonly response: Chat };
    readonly "agent.chat.send": { readonly request: {
        readonly "content": string;
      }; readonly response: Chat };
    readonly "agent.key.clear": { readonly request: Empty; readonly response: KeyView };
    readonly "agent.key.set": { readonly request: {
        readonly "apiKey": string;
      }; readonly response: KeyView };
    readonly "agent.packages.get": { readonly request: Empty; readonly response: Packages };
    readonly "agent.packages.set": { readonly request: {
        readonly "contract": ContractIdentity;
        readonly "enabled": boolean;
      }; readonly response: Packages };
    readonly "agent.settings.get": { readonly request: Empty; readonly response: SettingsView };
    readonly "agent.settings.set": { readonly request: {
        readonly "settings": Settings;
      }; readonly response: SettingsView };
    readonly "agent.status.get": { readonly request: Empty; readonly response: Status };
  };
  readonly subscriptions: {
    readonly "agent.chat.changes": { readonly request: Empty; readonly event: Chat };
    readonly "agent.status.changes": { readonly request: Empty; readonly event: Status };
  };
};
