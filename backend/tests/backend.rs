//! The Agent Package through the SDK harness: settings and key storage, Agent Binding through the
//! capability, and the status state machine, with no model endpoint involved.

use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use teamarness_agent_package::router;
use teamarness_package_sdk::BackendError;
use teamarness_package_sdk::testing::{
    Harness, HarnessBuilder, HarnessSubscription, ScriptedCapability,
    assert_router_matches_contract,
};

const AGENT_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const AGENT_KEY: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const OTHER_ID: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const OTHER_KEY: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
const STEWARD_ID: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
const STEWARD_KEY: &str = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
const WORKSPACE: &str = "01234567890123456789012345678901";
const SECRET: &str = "sk-or-v1-SENTINEL-NEVER-SHOWN-0123456789";

fn empty() -> Value {
    json!({})
}

fn agent_row(principal_id: &str, identity: &str, name: &str) -> Value {
    json!({"principalId": principal_id, "identity": identity, "name": name})
}

/// The Agent Bindings the fake Host holds for this Package, mutable from the test.
#[derive(Clone, Default)]
struct FakeBindings {
    bound: Arc<Mutex<Option<Value>>>,
    /// Agents the Installation stewards: `(principalId, identity, name)`.
    stewarded: Arc<Mutex<Vec<(String, String, String)>>>,
}

impl FakeBindings {
    fn capability(&self) -> ScriptedCapability {
        let state = self.clone();
        ScriptedCapability::new().calls(move |method, payload| match method {
            "current" => Ok(json!({"agent": state.bound.lock().unwrap().clone()})),
            "candidates" => {
                let bound = state.bound.lock().unwrap().clone();
                let bound_id = bound
                    .as_ref()
                    .and_then(|agent| agent["principalId"].as_str().map(str::to_owned));
                let agents = state
                    .stewarded
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|(id, key, name)| {
                        let mut row = agent_row(id, key, name);
                        row["bound"] = json!(bound_id.as_deref() == Some(id.as_str()));
                        row
                    })
                    .collect::<Vec<_>>();
                Ok(json!({"agents": agents}))
            }
            "bind" => {
                let id = payload["principalId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let found = state
                    .stewarded
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|(candidate, _, _)| *candidate == id)
                    .cloned();
                match found {
                    Some((id, key, name)) => {
                        let row = agent_row(&id, &key, &name);
                        *state.bound.lock().unwrap() = Some(row.clone());
                        Ok(json!({"agent": row}))
                    }
                    None if id == OTHER_ID => {
                        Err(BackendError::new("agent_not_stewarded", false, empty()))
                    }
                    None => Err(BackendError::new("agent_not_found", false, empty())),
                }
            }
            "unbind" => {
                *state.bound.lock().unwrap() = None;
                Ok(json!({"agent": null}))
            }
            _ => Err(BackendError::new(
                "capability_request_invalid",
                false,
                empty(),
            )),
        })
    }

    fn remove_agent(&self) {
        *self.bound.lock().unwrap() = None;
        self.stewarded
            .lock()
            .unwrap()
            .retain(|(id, _, _)| id != AGENT_ID);
    }
}

fn principal(principal_id: &str, key: &str, kind: &str, name: Option<&str>) -> Value {
    json!({
        "principalId": principal_id,
        "kind": kind,
        "stewardPrincipalId": if kind == "agent" { json!(STEWARD_ID) } else { Value::Null },
        "keys": [key],
        "name": name,
        "fingerprint": &key[..8],
    })
}

fn roster(cursor: u64, with_agent: bool) -> Value {
    let mut principals = vec![principal(STEWARD_ID, STEWARD_KEY, "person", Some("Alice"))];
    if with_agent {
        principals.push(principal(AGENT_ID, AGENT_KEY, "agent", Some("Scout")));
    }
    json!({"workspaceId": WORKSPACE, "cursor": cursor.to_string(), "principals": principals})
}

fn principals_capability() -> ScriptedCapability {
    ScriptedCapability::new().calls(|method, _| match method {
        "current" => Ok(roster(1, true)),
        _ => Err(BackendError::new(
            "capability_request_invalid",
            false,
            empty(),
        )),
    })
}

struct Fixture {
    harness: Harness,
    bindings: FakeBindings,
    binding_capability: ScriptedCapability,
    principals: ScriptedCapability,
}

async fn start(directory: &Path, bindings: &FakeBindings) -> Fixture {
    let principals = principals_capability();
    let binding_capability = bindings.capability();
    let harness = HarnessBuilder::new(router().into_backend())
        .package_identity("dev.teamarness.agent")
        .workspace(WORKSPACE, "Harness Workspace")
        .data_directory(directory)
        .capability("teamarness.workspace-context", ScriptedCapability::new())
        .capability("teamarness.principals", principals.clone())
        .capability("teamarness.agent-binding", binding_capability.clone())
        .capability("teamarness.package-ports", ScriptedCapability::new())
        .start()
        .await;
    Fixture {
        harness,
        bindings: bindings.clone(),
        binding_capability,
        principals,
    }
}

fn stewarded() -> FakeBindings {
    let bindings = FakeBindings::default();
    bindings
        .stewarded
        .lock()
        .unwrap()
        .push((AGENT_ID.into(), AGENT_KEY.into(), "Scout".into()));
    bindings
}

fn complete_settings() -> Value {
    json!({
        "endpointBaseUrl": "https://openrouter.ai/api/v1",
        "model": "deepseek/deepseek-chat",
        "systemPrompt": "You are Scout.",
        "contextMessages": 12,
    })
}

async fn next_state(subscription: &mut HarnessSubscription) -> Value {
    subscription
        .next_event()
        .await
        .expect("the status stream is open")
        .expect("the status stream carries no error")
}

/// Waits for the status stream to reach `state`, skipping intermediate events.
async fn status_until(subscription: &mut HarnessSubscription, state: &str) -> Value {
    loop {
        let event = next_state(subscription).await;
        if event["state"] == state {
            return event;
        }
    }
}

#[test]
fn router_matches_the_ui_contract() {
    assert_router_matches_contract(
        &router().operations(),
        include_str!("../../ui-contract.json"),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settings_round_trip_without_the_key_and_survive_a_restart() {
    let data = tempfile::tempdir().unwrap();
    let bindings = stewarded();
    let Fixture { mut harness, .. } = start(data.path(), &bindings).await;
    harness.activate().await.unwrap();

    let initial = harness
        .request("agent.settings.get", empty())
        .await
        .unwrap();
    assert_eq!(initial["hasKey"], false);
    assert_eq!(
        initial["settings"]["endpointBaseUrl"],
        "https://openrouter.ai/api/v1"
    );
    assert_eq!(initial["settings"]["model"], "");
    assert_eq!(initial["settings"]["contextMessages"], 20);

    let saved = harness
        .request(
            "agent.settings.set",
            json!({"settings": complete_settings()}),
        )
        .await
        .unwrap();
    assert_eq!(saved["settings"], complete_settings());
    assert_eq!(saved["hasKey"], false);

    let key_set = harness
        .request("agent.key.set", json!({"apiKey": SECRET}))
        .await
        .unwrap();
    assert_eq!(key_set, json!({"hasKey": true}));
    let read_back = harness
        .request("agent.settings.get", empty())
        .await
        .unwrap();
    assert_eq!(read_back["hasKey"], true);
    assert!(!read_back.to_string().contains(SECRET));
    assert!(!read_back.to_string().contains("apiKey"));
    let status = harness.request("agent.status.get", empty()).await.unwrap();
    assert!(!status.to_string().contains(SECRET));

    // An invalid document is refused with a field name and nothing else.
    let mut invalid = complete_settings();
    invalid["endpointBaseUrl"] = json!("ftp://example.invalid");
    let error = harness
        .request("agent.settings.set", json!({"settings": invalid}))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "agent.invalid_settings");
    assert_eq!(error.details(), &json!({"field": "endpointBaseUrl"}));
    assert!(!error.details().to_string().contains(SECRET));
    let kept = harness
        .request("agent.settings.get", empty())
        .await
        .unwrap();
    assert_eq!(kept["settings"], complete_settings());

    // A key with control characters or only whitespace is refused without being stored.
    let error = harness
        .request("agent.key.set", json!({"apiKey": "bad\nkey"}))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "agent.invalid_key");
    assert!(!error.details().to_string().contains("bad"));
    let error = harness
        .request("agent.key.set", json!({"apiKey": "   "}))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "agent.invalid_key");

    // The settings document never holds the key.
    let document = std::fs::read_to_string(data.path().join("settings.json")).unwrap();
    assert!(!document.contains(SECRET));
    assert!(document.contains("deepseek/deepseek-chat"));
    let key_file = std::fs::read_to_string(data.path().join("api-key")).unwrap();
    assert_eq!(key_file, SECRET);

    harness.shutdown().await.unwrap();

    // A new process generation reads the same settings and key back.
    let Fixture { mut harness, .. } = start(data.path(), &bindings).await;
    harness.activate().await.unwrap();
    let restored = harness
        .request("agent.settings.get", empty())
        .await
        .unwrap();
    assert_eq!(restored["settings"], complete_settings());
    assert_eq!(restored["hasKey"], true);

    let cleared = harness.request("agent.key.clear", empty()).await.unwrap();
    assert_eq!(cleared, json!({"hasKey": false}));
    assert!(!data.path().join("api-key").exists());
    let cleared_again = harness.request("agent.key.clear", empty()).await.unwrap();
    assert_eq!(cleared_again, json!({"hasKey": false}));
    harness.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_key_file_is_owner_only_from_its_first_byte() {
    use std::os::unix::fs::PermissionsExt;

    let data = tempfile::tempdir().unwrap();
    let bindings = stewarded();
    let Fixture { mut harness, .. } = start(data.path(), &bindings).await;
    harness.activate().await.unwrap();

    harness
        .request("agent.key.set", json!({"apiKey": SECRET}))
        .await
        .unwrap();
    let path = data.path().join("api-key");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "api-key mode was {mode:o}");
    assert!(!path.with_extension("tmp").exists());
    assert!(!data.path().join("api-key.tmp").exists());

    // Replacing the key keeps the mode.
    harness
        .request("agent.key.set", json!({"apiKey": "sk-second"}))
        .await
        .unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "sk-second");
    harness.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_moves_through_unconfigured_no_agent_ready_and_back_on_removal() {
    let data = tempfile::tempdir().unwrap();
    let bindings = stewarded();
    let Fixture {
        mut harness,
        bindings,
        binding_capability,
        principals,
    } = start(data.path(), &bindings).await;
    harness.activate().await.unwrap();

    let mut status = harness
        .subscribe("agent.status.changes", empty())
        .await
        .unwrap();
    assert_eq!(
        next_state(&mut status).await,
        json!({"state": "unconfigured"})
    );
    assert_eq!(
        harness.request("agent.status.get", empty()).await.unwrap(),
        json!({"state": "unconfigured"})
    );

    // Configured but unbound: settings alone are not enough.
    harness
        .request(
            "agent.settings.set",
            json!({"settings": complete_settings()}),
        )
        .await
        .unwrap();
    harness
        .request("agent.key.set", json!({"apiKey": SECRET}))
        .await
        .unwrap();
    assert_eq!(
        status_until(&mut status, "no_agent").await,
        json!({"state": "no_agent"})
    );

    // Only this Installation's Agents are offered, and binding one makes the Package ready.
    let candidates = harness
        .request("agent.binding.candidates", empty())
        .await
        .unwrap();
    assert_eq!(
        candidates,
        json!({"agents": [{"principalId": AGENT_ID, "name": "Scout", "bound": false}]})
    );
    let refused = harness
        .request("agent.binding.set", json!({"principalId": OTHER_ID}))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), "agent.not_stewarded");
    let missing = harness
        .request("agent.binding.set", json!({"principalId": OTHER_KEY}))
        .await
        .unwrap_err();
    assert_eq!(missing.code(), "agent.agent_not_found");
    let bound = harness
        .request("agent.binding.set", json!({"principalId": AGENT_ID}))
        .await
        .unwrap();
    assert_eq!(
        bound,
        json!({"agent": {"principalId": AGENT_ID, "name": "Scout"}})
    );
    let ready = status_until(&mut status, "ready").await;
    assert_eq!(
        ready,
        json!({"state": "ready", "agent": {"principalId": AGENT_ID, "name": "Scout"}})
    );
    assert_eq!(
        harness
            .request("agent.binding.candidates", empty())
            .await
            .unwrap()["agents"][0]["bound"],
        true
    );

    // Clearing the key drops back to unconfigured while the binding is kept.
    harness.request("agent.key.clear", empty()).await.unwrap();
    let unconfigured = status_until(&mut status, "unconfigured").await;
    assert_eq!(unconfigured["agent"]["principalId"], AGENT_ID);
    harness
        .request("agent.key.set", json!({"apiKey": SECRET}))
        .await
        .unwrap();
    status_until(&mut status, "ready").await;

    // The Steward removes the Agent: the binding reads as absent and the roster notifies.
    bindings.remove_agent();
    principals.stream().opened().await;
    assert!(principals.stream().send(roster(2, false)).await);
    assert_eq!(
        status_until(&mut status, "no_agent").await,
        json!({"state": "no_agent"})
    );
    assert_eq!(
        harness.request("agent.status.get", empty()).await.unwrap(),
        json!({"state": "no_agent"})
    );
    assert_eq!(
        harness
            .request("agent.binding.candidates", empty())
            .await
            .unwrap(),
        json!({"agents": []})
    );

    // Clearing an absent binding is idempotent and stays no_agent.
    assert_eq!(
        harness
            .request("agent.binding.clear", empty())
            .await
            .unwrap(),
        json!({"agent": null})
    );
    assert!(
        binding_capability
            .recorded_calls()
            .iter()
            .any(|call| call.method == "unbind")
    );
    status.close().await;
    harness.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rename_reaches_the_status_and_a_restart_keeps_the_binding() {
    let data = tempfile::tempdir().unwrap();
    let bindings = stewarded();
    let Fixture {
        mut harness,
        bindings,
        principals,
        ..
    } = start(data.path(), &bindings).await;
    harness.activate().await.unwrap();
    harness
        .request(
            "agent.settings.set",
            json!({"settings": complete_settings()}),
        )
        .await
        .unwrap();
    harness
        .request("agent.key.set", json!({"apiKey": SECRET}))
        .await
        .unwrap();
    harness
        .request("agent.binding.set", json!({"principalId": AGENT_ID}))
        .await
        .unwrap();
    let mut status = harness
        .subscribe("agent.status.changes", empty())
        .await
        .unwrap();
    assert_eq!(next_state(&mut status).await["state"], "ready");

    // The Steward renames the Agent; the binding read carries the new name.
    bindings.stewarded.lock().unwrap()[0].2 = "Pathfinder".into();
    *bindings.bound.lock().unwrap() = Some(agent_row(AGENT_ID, AGENT_KEY, "Pathfinder"));
    principals.stream().opened().await;
    assert!(principals.stream().send(roster(2, true)).await);
    let renamed = next_state(&mut status).await;
    assert_eq!(
        renamed,
        json!({"state": "ready", "agent": {"principalId": AGENT_ID, "name": "Pathfinder"}})
    );
    status.close().await;

    // Suspend stops the watcher; resume restarts it and re-reads the binding.
    harness.suspend().await.unwrap();
    let error = harness
        .request("agent.status.get", empty())
        .await
        .unwrap_err();
    assert_eq!(error.code(), "package.not_active");
    harness.recover().await.unwrap();
    principals.stream().opened().await;
    assert_eq!(
        harness.request("agent.status.get", empty()).await.unwrap()["state"],
        "ready"
    );
    harness.shutdown().await.unwrap();

    // A new generation: nothing is stored locally about the Agent, the capability answers.
    let Fixture { mut harness, .. } = start(data.path(), &bindings).await;
    harness.activate().await.unwrap();
    assert_eq!(
        harness.request("agent.status.get", empty()).await.unwrap(),
        json!({"state": "ready", "agent": {"principalId": AGENT_ID, "name": "Pathfinder"}})
    );
    harness.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_operations_and_malformed_payloads_use_sdk_codes() {
    let data = tempfile::tempdir().unwrap();
    let bindings = stewarded();
    let Fixture { mut harness, .. } = start(data.path(), &bindings).await;
    harness.activate().await.unwrap();
    let error = harness.request("agent.reply", empty()).await.unwrap_err();
    assert_eq!(error.code(), "package.unknown_operation");
    let error = harness
        .request("agent.key.set", json!({"apiKey": SECRET, "extra": 1}))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "package.invalid_payload");
    assert!(!error.details().to_string().contains(SECRET));
    let error = harness
        .request("agent.binding.set", json!({"principalId": "short"}))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "agent.invalid_request");
    harness.shutdown().await.unwrap();
}
