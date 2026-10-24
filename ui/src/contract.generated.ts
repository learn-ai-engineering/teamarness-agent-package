// Generated from ui-contract.json by teamarness-contract-types. Do not edit.

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

export type Empty = Record<string, never>;

export type KeyView = {
  readonly "hasKey": boolean;
};

export type Name = string | null;

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
    readonly "agent.key.clear": { readonly request: Empty; readonly response: KeyView };
    readonly "agent.key.set": { readonly request: {
        readonly "apiKey": string;
      }; readonly response: KeyView };
    readonly "agent.settings.get": { readonly request: Empty; readonly response: SettingsView };
    readonly "agent.settings.set": { readonly request: {
        readonly "settings": Settings;
      }; readonly response: SettingsView };
    readonly "agent.status.get": { readonly request: Empty; readonly response: Status };
  };
  readonly subscriptions: {
    readonly "agent.status.changes": { readonly request: Empty; readonly event: Status };
  };
};
