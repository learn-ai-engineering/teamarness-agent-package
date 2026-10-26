//! The Agent Package through the SDK harness: settings and key storage, Agent Binding through the
//! capability, and the status state machine, with no model endpoint involved.

mod support;

use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use teamarness_agent_package::catalog::{BundledCatalog, Catalog, PortKind};
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
    current_failures: Arc<std::sync::atomic::AtomicUsize>,
    mutation_failure: Arc<Mutex<Option<&'static str>>>,
    /// Agents the Installation stewards: `(principalId, identity, name)`.
    stewarded: Arc<Mutex<Vec<(String, String, String)>>>,
}

impl FakeBindings {
    fn capability(&self) -> ScriptedCapability {
        let state = self.clone();
        ScriptedCapability::new().calls(move |method, payload| match method {
            "current" => {
                if state
                    .current_failures
                    .fetch_update(
                        std::sync::atomic::Ordering::SeqCst,
                        std::sync::atomic::Ordering::SeqCst,
                        |n| n.checked_sub(1),
                    )
                    .is_ok()
                {
                    return Err(BackendError::new("overloaded", true, empty()));
                }
                Ok(json!({"agent": state.bound.lock().unwrap().clone()}))
            }
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
                if let Some(code) = state.mutation_failure.lock().unwrap().take() {
                    return Err(BackendError::new(code, false, empty()));
                }
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
    start_with_principals(directory, bindings, principals_capability()).await
}

async fn start_with_principals(
    directory: &Path,
    bindings: &FakeBindings,
    principals: ScriptedCapability,
) -> Fixture {
    start_with_ports(directory, bindings, principals, ScriptedCapability::new()).await
}

async fn start_with_ports(
    directory: &Path,
    bindings: &FakeBindings,
    principals: ScriptedCapability,
    ports: ScriptedCapability,
) -> Fixture {
    let binding_capability = bindings.capability();
    let harness = HarnessBuilder::new(router().into_backend())
        .package_identity("dev.teamarness.agent")
        .workspace(WORKSPACE, "Harness Workspace")
        .data_directory(directory)
        .capability("teamarness.workspace-context", ScriptedCapability::new())
        .capability("teamarness.principals", principals.clone())
        .capability("teamarness.agent-binding", binding_capability.clone())
        .capability("teamarness.package-ports", ports)
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

#[test]
fn chat_activity_bound_matches_the_contract() {
    let contract: Value = serde_json::from_str(include_str!("../../ui-contract.json")).unwrap();
    // An answer's activity and a failed turn's activity share one bound and one item shape.
    for activity in [
        &contract["$defs"]["chatEntry"]["properties"]["activity"],
        &contract["$defs"]["chat"]["properties"]["failedActivity"],
    ] {
        assert_eq!(
            activity["maxItems"],
            json!(teamarness_agent_package::chat::MAX_ACTIVITY)
        );
        assert_eq!(activity["items"], json!({"$ref": "#/$defs/activity"}));
    }
    assert_eq!(
        contract["$defs"]["activity"]["additionalProperties"],
        json!(false)
    );
}

fn manifest() -> Value {
    serde_json::from_str(include_str!("../../teamarness.package.json")).expect("manifest is JSON")
}

/// A consumed contract: identity, major, minor range and `(port, kind)` pairs.
type Consumed = (String, u64, u64, u64, Vec<(String, String)>);

#[test]
fn fixed_harness_declaration_matches_its_historical_fixture() {
    let manifest = manifest();
    let declared: Vec<Consumed> = manifest["consumedPorts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|contract| {
            (
                contract["identity"].as_str().unwrap().to_owned(),
                contract["major"].as_u64().unwrap(),
                contract["minMinor"].as_u64().unwrap(),
                contract["maxMinor"].as_u64().unwrap(),
                contract["ports"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|port| {
                        (
                            port["name"].as_str().unwrap().to_owned(),
                            port["kind"].as_str().unwrap().to_owned(),
                        )
                    })
                    .collect(),
            )
        })
        .collect();
    let kind = |kind: PortKind| match kind {
        PortKind::Command => "command",
        PortKind::Query => "query",
        PortKind::Event => "event",
    };
    let bundled: Vec<Consumed> = BundledCatalog::get()
        .contracts()
        .iter()
        .filter(|contract| contract.identity == support::CONVERSATIONS)
        .map(|contract| {
            (
                contract.identity.clone(),
                u64::from(contract.major),
                u64::from(contract.minor),
                u64::from(contract.minor),
                contract
                    .ports
                    .iter()
                    .map(|port| (port.name.clone(), kind(port.kind).to_owned()))
                    .collect(),
            )
        })
        .collect();
    // Only the built-in harness keeps a fixed dependency, with all five original ports.
    let sorted = |mut contracts: Vec<Consumed>| {
        for contract in &mut contracts {
            contract.4.sort();
        }
        contracts.sort();
        contracts
    };
    assert_eq!(sorted(declared), sorted(bundled));
}

#[test]
fn fixed_consumed_ports_are_only_for_conversations_and_package_ports_negotiates_two() {
    let manifest = manifest();
    let required: Vec<(&str, bool)> = manifest["consumedPorts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|contract| {
            (
                contract["identity"].as_str().unwrap(),
                contract["required"].as_bool().unwrap_or(true),
            )
        })
        .collect();
    assert_eq!(required, [("dev.teamarness.conversations", true)]);
    let ports = manifest["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|capability| capability["identity"] == "teamarness.package-ports")
        .unwrap();
    assert_eq!(
        ports,
        &json!({"identity":"teamarness.package-ports","major":2,"minMinor":0,"maxMinor":0})
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

async fn wait_binding(harness: &mut Harness, expected: Option<&str>) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let status = harness.request("agent.status.get", empty()).await.unwrap();
            if status["agent"]["name"].as_str() == expected {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("binding reconciliation without a future event");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn roster_startup_snapshot_reconciles_a_missed_removal() {
    let data = tempfile::tempdir().unwrap();
    let bindings = stewarded();
    *bindings.bound.lock().unwrap() = Some(agent_row(AGENT_ID, AGENT_KEY, "Scout"));
    let changed = bindings.clone();
    let principals = ScriptedCapability::new().calls(move |method, _| {
        assert_eq!(method, "current");
        changed.remove_agent();
        Ok(roster(2, false))
    });
    let mut fixture = start_with_principals(data.path(), &bindings, principals).await;
    fixture.harness.activate().await.unwrap();
    fixture.principals.stream().opened().await;
    wait_binding(&mut fixture.harness, None).await;
    assert_eq!(fixture.principals.recorded_opens()[0]["afterCursor"], "2");
    fixture.harness.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn roster_reconnect_snapshot_reconciles_without_an_event() {
    let data = tempfile::tempdir().unwrap();
    let bindings = stewarded();
    *bindings.bound.lock().unwrap() = Some(agent_row(AGENT_ID, AGENT_KEY, "Scout"));
    let mut fixture = start(data.path(), &bindings).await;
    fixture.harness.activate().await.unwrap();
    fixture.principals.stream().opened().await;
    *bindings.bound.lock().unwrap() = Some(agent_row(AGENT_ID, AGENT_KEY, "Pathfinder"));
    fixture.principals.stream().close("completed").await;
    wait_binding(&mut fixture.harness, Some("Pathfinder")).await;
    assert!(fixture.principals.recorded_opens().len() >= 2);
    fixture.harness.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn roster_failed_refresh_retries_without_another_event() {
    let data = tempfile::tempdir().unwrap();
    let bindings = stewarded();
    *bindings.bound.lock().unwrap() = Some(agent_row(AGENT_ID, AGENT_KEY, "Scout"));
    let mut fixture = start(data.path(), &bindings).await;
    fixture.harness.activate().await.unwrap();
    fixture.principals.stream().opened().await;
    // Wait for the initial reconciliation to finish before failing the event refresh.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    bindings
        .current_failures
        .store(1, std::sync::atomic::Ordering::SeqCst);
    *bindings.bound.lock().unwrap() = Some(agent_row(AGENT_ID, AGENT_KEY, "Pathfinder"));
    assert!(fixture.principals.stream().send(roster(2, true)).await);
    wait_binding(&mut fixture.harness, Some("Pathfinder")).await;
    fixture.harness.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_binding_change_does_not_block_later_binding_changes() {
    let data = tempfile::tempdir().unwrap();
    let bindings = stewarded();
    let mut fixture = start(data.path(), &bindings).await;
    fixture.harness.activate().await.unwrap();
    fixture
        .harness
        .request("agent.binding.set", json!({"principalId":AGENT_ID}))
        .await
        .unwrap();
    *bindings.mutation_failure.lock().unwrap() = Some("host_unavailable");
    assert!(
        fixture
            .harness
            .request("agent.binding.set", json!({"principalId":OTHER_ID}))
            .await
            .is_err()
    );
    *bindings.mutation_failure.lock().unwrap() = None;
    let before = fixture
        .binding_capability
        .recorded_calls()
        .into_iter()
        .filter(|call| matches!(call.method.as_str(), "bind" | "unbind"))
        .count();
    let cleared = fixture
        .harness
        .request("agent.binding.clear", empty())
        .await
        .unwrap();
    assert!(cleared["agent"].is_null());
    let after = fixture
        .binding_capability
        .recorded_calls()
        .into_iter()
        .filter(|call| matches!(call.method.as_str(), "bind" | "unbind"))
        .count();
    assert_eq!(
        after,
        before + 1,
        "the later binding change reached the Host"
    );
    assert!(bindings.bound.lock().unwrap().is_none());
    fixture.harness.shutdown().await.unwrap();
}

const EXHIBITS: &str = "dev.teamarness.exhibits";

fn host_ports(catalog: &support::HostCatalog, bindings: &FakeBindings) -> ScriptedCapability {
    let catalog = catalog.clone();
    let bindings = bindings.clone();
    ScriptedCapability::new().calls(move |method, request| {
        if bindings.bound.lock().unwrap().is_none() {
            return Err(BackendError::new("agent_not_bound", false, empty()));
        }
        catalog
            .handle(method, &request)
            .unwrap_or_else(|| Err(BackendError::new("ungranted_port", false, empty())))
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn package_switches_use_host_success_pins_and_unavailable_disable_and_never_local_preferences()
 {
    let data = tempfile::tempdir().unwrap();
    let legacy = br#"{"schemaVersion":1,"enabled":["dev.teamarness.exhibits"]}"#;
    std::fs::write(data.path().join("packages.json"), legacy).unwrap();
    let bindings = stewarded();
    let catalog = support::HostCatalog::providers();
    let ports = host_ports(&catalog, &bindings);
    let Fixture { mut harness, .. } = start_with_ports(
        data.path(),
        &bindings,
        principals_capability(),
        ports.clone(),
    )
    .await;
    harness.activate().await.unwrap();
    let missing = harness
        .request("agent.packages.get", empty())
        .await
        .unwrap_err();
    assert_eq!(missing.code(), "agent.binding_required");
    harness
        .request("agent.binding.set", json!({"principalId":AGENT_ID}))
        .await
        .unwrap();
    let view = harness
        .request("agent.packages.get", empty())
        .await
        .unwrap();
    assert!(
        view["packages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["enabled"] == false)
    );
    let enable = json!({"contract": EXHIBITS,"enabled":true,"major":1});
    *catalog.refusal.lock().unwrap() = Some("storage_failed");
    assert!(
        harness
            .request("agent.packages.set", enable.clone())
            .await
            .is_err()
    );
    assert!(catalog.selections.lock().unwrap().is_empty());
    *catalog.refusal.lock().unwrap() = None;
    let enabled = harness
        .request("agent.packages.set", enable.clone())
        .await
        .unwrap();
    assert_eq!(enabled["packages"][1]["enabledMajor"], 1);
    // Metadata changes after the UI observed major 1. No implicit upgrade.
    catalog.documents.lock().unwrap().get_mut(EXHIBITS).unwrap()["contracts"][0]["major"] =
        json!(2);
    assert!(harness.request("agent.packages.set", enable).await.is_err());
    let changed = harness
        .request("agent.packages.get", empty())
        .await
        .unwrap();
    assert_eq!(changed["packages"][1]["major"], 2);
    assert_eq!(changed["packages"][1]["enabledMajor"], 1);
    assert_eq!(changed["packages"][1]["available"], false);
    catalog.documents.lock().unwrap().remove(EXHIBITS);
    let unavailable = harness
        .request("agent.packages.get", empty())
        .await
        .unwrap();
    assert_eq!(unavailable["packages"][1]["enabled"], true);
    harness.suspend().await.unwrap();
    harness.recover().await.unwrap();
    assert_eq!(
        harness
            .request("agent.packages.get", empty())
            .await
            .unwrap(),
        unavailable
    );
    harness.shutdown().await.unwrap();
    let Fixture { mut harness, .. } = start_with_ports(
        data.path(),
        &bindings,
        principals_capability(),
        host_ports(&catalog, &bindings),
    )
    .await;
    harness.activate().await.unwrap();
    assert_eq!(
        harness
            .request("agent.packages.get", empty())
            .await
            .unwrap(),
        unavailable
    );
    let disabled = harness
        .request(
            "agent.packages.set",
            json!({"contract":EXHIBITS,"enabled":false}),
        )
        .await
        .unwrap();
    assert!(
        disabled["packages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["contract"] != EXHIBITS)
    );
    assert_eq!(
        std::fs::read(data.path().join("packages.json")).unwrap(),
        legacy
    );
    for call in ports.recorded_calls() {
        if call.method == "enable" {
            assert_eq!(call.payload, json!({"contract":EXHIBITS,"major":1}));
        }
        if call.method == "available" {
            assert_eq!(call.payload, json!({"access":"agent"}));
        }
    }
    harness.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn package_enable_requires_observed_major_and_closed_payload() {
    let data = tempfile::tempdir().unwrap();
    let bindings = stewarded();
    *bindings.bound.lock().unwrap() = Some(agent_row(AGENT_ID, AGENT_KEY, "Scout"));
    let catalog = support::HostCatalog::providers();
    let ports = host_ports(&catalog, &bindings);
    let Fixture { mut harness, .. } = start_with_ports(
        data.path(),
        &bindings,
        principals_capability(),
        ports.clone(),
    )
    .await;
    harness.activate().await.unwrap();
    for payload in [
        json!({"contract":EXHIBITS,"enabled":true}),
        json!({"contract":EXHIBITS,"enabled":true,"major":0}),
        json!({"contract":EXHIBITS,"enabled":false,"major":1}),
        json!({"contract":EXHIBITS,"enabled":true,"major":1,"extra":true}),
    ] {
        assert!(
            harness
                .request("agent.packages.set", payload)
                .await
                .is_err()
        );
    }
    assert!(
        ports
            .recorded_calls()
            .iter()
            .all(|call| call.method != "enable" && call.method != "disable")
    );
    harness.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_discovery_lists_unknown_and_retained_selected_major_without_local_grants() {
    let data = tempfile::tempdir().unwrap();
    let legacy = br#"{"schemaVersion":1,"enabled":["dev.teamarness.exhibits"]}"#;
    std::fs::write(data.path().join("packages.json"), legacy).unwrap();
    let ports = ScriptedCapability::new().calls(|method, request| {
        let _ = (method, request);
        Ok(json!({"contracts":[{
            "contract":"com.example.unknown", "major":3, "minor":null,
            "description":null, "available":false, "enabled":true,
            "enabledMajor":2, "metadataSha256":null
        }]}))
    });
    let bindings = stewarded();
    *bindings.bound.lock().unwrap() = Some(agent_row(AGENT_ID, AGENT_KEY, "Scout"));
    let Fixture { mut harness, .. } = start_with_ports(
        data.path(),
        &bindings,
        principals_capability(),
        ports.clone(),
    )
    .await;
    harness.activate().await.unwrap();
    let view = harness
        .request("agent.packages.get", empty())
        .await
        .unwrap();
    assert_eq!(
        view,
        json!({"packages":[{
            "contract":"com.example.unknown", "name":"Unknown", "description":"",
            "major":3, "enabledMajor":2, "available":false, "enabled":true
        }]})
    );
    assert_eq!(
        std::fs::read(data.path().join("packages.json")).unwrap(),
        legacy
    );
    assert_eq!(ports.recorded_calls()[0].payload, json!({"access":"agent"}));
    harness.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_summary_pages_are_complete_sorted_and_bounded_before_ui_publication() {
    for malformed in [false, true] {
        let data = tempfile::tempdir().unwrap();
        let bindings = stewarded();
        *bindings.bound.lock().unwrap() = Some(agent_row(AGENT_ID, AGENT_KEY, "Scout"));
        let ports = ScriptedCapability::new().calls(move |_, request| {
            let identity = if request.get("cursor").is_none() || malformed { "com.example.alpha" } else { "com.example.beta" };
            let mut page = json!({"contracts":[{"contract":identity,"major":1,"minor":0,"description":"Discovered live.","available":true,"enabled":false,"enabledMajor":null,"metadataSha256":"a".repeat(64)}]});
            if request.get("cursor").is_none() { page["nextCursor"] = json!("second"); }
            Ok(page)
        });
        let Fixture { mut harness, .. } = start_with_ports(
            data.path(),
            &bindings,
            principals_capability(),
            ports.clone(),
        )
        .await;
        harness.activate().await.unwrap();
        let result = harness.request("agent.packages.get", empty()).await;
        if malformed {
            assert!(
                result.is_err(),
                "repeated/out-of-order identity must not publish partial state"
            );
        } else {
            let view = result.unwrap();
            assert_eq!(view["packages"][0]["contract"], "com.example.alpha");
            assert_eq!(view["packages"][1]["contract"], "com.example.beta");
        }
        assert_eq!(ports.recorded_calls().len(), 2);
        assert_eq!(
            ports.recorded_calls()[1].payload,
            json!({"access":"agent","cursor":"second"})
        );
        harness.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_summary_overflow_empty_continuation_and_cursor_cycles_refuse_without_partial_success()
{
    for case in ["overflow", "empty", "cursor-cycle", "reverse-order"] {
        let data = tempfile::tempdir().unwrap();
        let bindings = stewarded();
        *bindings.bound.lock().unwrap() = Some(agent_row(AGENT_ID, AGENT_KEY, "Scout"));
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = counter.clone();
        let ports = ScriptedCapability::new().calls(move |_, _| {
            let page = observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let rows = match case { "overflow" => 32, "empty" => 0, _ => 1 };
            let contracts: Vec<_> = (0..rows).map(|row| json!({
                "contract":format!("com.example.row-{:03}", if case == "reverse-order" { 100-page } else { page*32+row }),
                "major":1,"minor":0,"description":"Live.","available":true,"enabled":false,"enabledMajor":null,"metadataSha256":"a".repeat(64)
            })).collect();
            Ok(json!({"contracts":contracts,"nextCursor": if page.is_multiple_of(2) { "cursor-a" } else { "cursor-b" }}))
        });
        let Fixture { mut harness, .. } = start_with_ports(
            data.path(),
            &bindings,
            principals_capability(),
            ports.clone(),
        )
        .await;
        harness.activate().await.unwrap();
        let error = harness
            .request("agent.packages.get", empty())
            .await
            .unwrap_err();
        assert_eq!(error.code(), "agent.capability_unavailable");
        assert_eq!(error.details()["causeCode"], "agent.invalid_metadata");
        assert!(
            counter.load(std::sync::atomic::Ordering::SeqCst) <= 3,
            "bounded {case}"
        );
        assert!(
            ports
                .recorded_calls()
                .iter()
                .all(|call| call.method == "available")
        );
        assert!(!data.path().join("packages.json").exists());
        harness.shutdown().await.unwrap();
    }
}
