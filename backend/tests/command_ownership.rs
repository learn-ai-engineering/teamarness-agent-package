//! Async external Host seam. Tests use the same owner/PortCaller adapter as production;
//! the canonical SDK Harness pump cannot hold a capability and deliver concurrent UI results.
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use teamarness_agent_package::command_ownership::{
    BINDING_BUSY, BINDING_UNCERTAIN, COMMAND_BUSY, IdentityWork, MAX_PENDING_COMMANDS, STALE_INTENT,
};
use teamarness_agent_package::tools::PortCaller;
use teamarness_package_sdk::BackendError;
use tokio::sync::{mpsc, oneshot};

#[derive(Clone)]
struct Host {
    actor: Arc<Mutex<&'static str>>,
    effects: Arc<Mutex<Vec<Value>>>,
    queued: mpsc::UnboundedSender<Invocation>,
}
struct Invocation {
    release: oneshot::Sender<Result<(), BackendError>>,
}
fn host() -> (Host, mpsc::UnboundedReceiver<Invocation>) {
    let (queued, receive) = mpsc::unbounded_channel();
    (
        Host {
            actor: Arc::new(Mutex::new("A")),
            effects: Arc::default(),
            queued,
        },
        receive,
    )
}
impl PortCaller for Host {
    async fn query(&self, _: &str, _: u16, _: &str, _: Value) -> Result<Value, BackendError> {
        Ok(json!({"actor":*self.actor.lock().unwrap()}))
    }
    async fn command(
        &self,
        _: &str,
        _: u16,
        port: &str,
        request: Value,
    ) -> Result<Value, BackendError> {
        let (release, result) = oneshot::channel();
        self.queued.send(Invocation { release }).unwrap();
        // Models Host work still before actor resolution, NOT a supplied actor claim.
        result.await.expect("external Host panic")?;
        let effect = json!({"actor":*self.actor.lock().unwrap(),"port":port,"request":request});
        self.effects.lock().unwrap().push(effect.clone());
        Ok(effect)
    }
}
async fn bind(
    identity: &IdentityWork,
    host: &Host,
    actor: &'static str,
) -> Result<(), BackendError> {
    let host = host.clone();
    identity
        .change_binding(move || async move {
            *host.actor.lock().unwrap() = actor;
            Ok(())
        })
        .await
}
async fn eventually_bind(identity: &IdentityWork, host: &Host) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match bind(identity, host, "B").await {
                Ok(()) => break,
                Err(error) => {
                    assert_eq!(error.code(), BINDING_BUSY);
                    tokio::task::yield_now().await;
                }
            }
        }
    })
    .await
    .expect("command owner never settled");
}

#[tokio::test]
async fn final_and_generic_commands_survive_aborted_awaiters_and_reject_stale_intent() {
    for port in ["message.post", "draft.delete"] {
        let identity = IdentityWork::default();
        let (host, mut queue) = host();
        let intent = identity.intent(host.clone());
        let old = intent.clone();
        let waiter = tokio::spawn(async move {
            intent
                .command("provider", 1, port, json!({"text":"A work"}))
                .await
        });
        let held = queue.recv().await.unwrap();
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        for _ in 0..2 {
            assert_eq!(
                bind(&identity, &host, "B").await.unwrap_err().code(),
                BINDING_BUSY
            );
        }
        assert_eq!(*host.actor.lock().unwrap(), "A");
        held.release.send(Ok(())).unwrap();
        eventually_bind(&identity, &host).await;
        assert_eq!(
            *host.effects.lock().unwrap(),
            vec![json!({"actor":"A","port":port,"request":{"text":"A work"}})]
        );
        assert_eq!(
            old.command("provider", 1, port, json!({}))
                .await
                .unwrap_err()
                .code(),
            STALE_INTENT
        );
        assert!(queue.try_recv().is_err(), "stale A intent enqueued after B");
        let fresh = identity.intent(host.clone());
        let waiter = tokio::spawn(async move {
            fresh
                .command("provider", 1, port, json!({"text":"B work"}))
                .await
        });
        queue.recv().await.unwrap().release.send(Ok(())).unwrap();
        assert_eq!(waiter.await.unwrap().unwrap()["actor"], "B");
        assert_eq!(host.effects.lock().unwrap().len(), 2);
    }
}

#[tokio::test]
async fn dropped_deadline_and_joinset_shutdown_keep_actual_commands_owned() {
    for shutdown in [false, true] {
        let identity = IdentityWork::default();
        let (host, mut queue) = host();
        let intent = identity.intent(host.clone());
        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(async move {
            intent
                .command("provider", 1, "draft.delete", json!({"id":"one"}))
                .await
        });
        let held = queue.recv().await.unwrap();
        if shutdown {
            tasks.shutdown().await;
        } else {
            drop(tasks);
        }
        assert_eq!(
            bind(&identity, &host, "B").await.unwrap_err().code(),
            BINDING_BUSY
        );
        held.release.send(Ok(())).unwrap();
        eventually_bind(&identity, &host).await;
        assert_eq!(host.effects.lock().unwrap()[0]["actor"], "A");
    }
    // An expired outer observer drops its future, not the owned external command.
    let identity = IdentityWork::default();
    let (host, mut queue) = host();
    let intent = identity.intent(host.clone());
    let (expire, expired) = oneshot::channel();
    let waiter = tokio::spawn(async move {
        tokio::select! {
            result = intent.command("provider", 1, "message.post", json!({})) => { result.unwrap(); },
            _ = expired => {},
        }
    });
    let held = queue.recv().await.unwrap();
    expire.send(()).unwrap();
    waiter.await.unwrap();
    assert_eq!(
        bind(&identity, &host, "B").await.unwrap_err().code(),
        BINDING_BUSY
    );
    held.release.send(Ok(())).unwrap();
    eventually_bind(&identity, &host).await;
    assert_eq!(host.effects.lock().unwrap()[0]["actor"], "A");
}

#[tokio::test]
async fn certain_errors_release_but_transport_uncertainty_and_panics_do_not() {
    for (code, flag, ambiguous) in [
        ("exhibits.not_found", false, false),
        ("port_not_enabled", false, false),
        ("exhibits.deferred", true, true),
        ("host_unavailable", false, true),
        ("deadline_exceeded", false, true),
        ("invalid_capability_response", false, true),
        ("outcome_unknown", true, true),
        ("panic", false, true),
    ] {
        let identity = IdentityWork::default();
        let (host, mut queue) = host();
        let intent = identity.intent(host.clone());
        let waiter = tokio::spawn(async move {
            intent
                .command("provider", 1, "message.post", json!({}))
                .await
        });
        let held = queue.recv().await.unwrap();
        if code == "panic" {
            drop(held);
        } else {
            held.release
                .send(Err(
                    BackendError::new(code, false, json!({})).with_outcome_uncertain(flag)
                ))
                .unwrap();
        }
        assert!(waiter.await.unwrap().is_err());
        if ambiguous {
            assert_eq!(
                bind(&identity, &host, "B").await.unwrap_err().code(),
                BINDING_UNCERTAIN
            );
            // An unrelated successful command does not clear uncertainty. Nor does invalidation.
            identity.invalidate();
            let fresh = identity.intent(host.clone());
            let other =
                tokio::spawn(async move { fresh.command("provider", 1, "other", json!({})).await });
            queue.recv().await.unwrap().release.send(Ok(())).unwrap();
            assert_eq!(other.await.unwrap().unwrap()["actor"], "A");
            assert_eq!(
                bind(&identity, &host, "B").await.unwrap_err().code(),
                BINDING_UNCERTAIN
            );
        } else {
            bind(&identity, &host, "B").await.unwrap();
        }
        assert!(queue.try_recv().is_err(), "failed command replayed");
    }
}

#[tokio::test]
async fn provider_error_preserves_terminal_and_only_certain_refusal_allows_binding() {
    for uncertain in [false, true] {
        let identity = IdentityWork::default();
        let (host, mut queue) = host();
        let intent = identity.intent(host.clone());
        let waiter = tokio::spawn(async move {
            intent
                .command("provider", 3, "draft.delete", json!({"id":"refused"}))
                .await
        });
        let details = json!({"provider":{
            "code":"qualification.refused", "retryable":false, "details":{}
        }});
        queue
            .recv()
            .await
            .unwrap()
            .release
            .send(Err(BackendError::new(
                "provider_error",
                false,
                details.clone(),
            )
            .with_outcome_uncertain(uncertain)))
            .unwrap();
        let error = waiter.await.unwrap().unwrap_err();
        assert_eq!(error.code(), "provider_error");
        assert!(!error.retryable());
        assert_eq!(error.details(), &details);
        // Probe actual binding before checking the flag: a certain terminal must not latch.
        let binding = bind(&identity, &host, "B").await;
        if uncertain {
            assert_eq!(binding.unwrap_err().code(), BINDING_UNCERTAIN);
            assert_eq!(*host.actor.lock().unwrap(), "A");
        } else {
            binding.unwrap();
            assert_eq!(*host.actor.lock().unwrap(), "B");
        }
        assert_eq!(error.outcome_uncertain(), uncertain);
        assert!(
            host.effects.lock().unwrap().is_empty(),
            "refusal mutated Provider state"
        );
        assert!(queue.try_recv().is_err(), "refused command replayed");
        if !uncertain {
            let fresh = identity.intent(host.clone());
            let next = tokio::spawn(async move {
                fresh
                    .command("provider", 3, "draft.delete", json!({"id":"B work"}))
                    .await
            });
            queue.recv().await.unwrap().release.send(Ok(())).unwrap();
            assert_eq!(next.await.unwrap().unwrap()["actor"], "B");
            assert_eq!(
                *host.effects.lock().unwrap(),
                vec![json!({"actor":"B","port":"draft.delete","request":{"id":"B work"}})]
            );
            assert!(queue.try_recv().is_err(), "command replayed after binding");
        }
    }
}

#[tokio::test]
async fn cancelled_binding_waiter_retains_actual_mutation_exclusion_and_published_identity() {
    for uncertain in [false, true] {
        let identity = IdentityWork::default();
        let (host, mut queue) = host();
        let old = identity.intent(host.clone());
        let (entered, entry) = oneshot::channel();
        let (release, held) = oneshot::channel();
        let owner = identity.clone();
        let external = host.clone();
        let waiter = tokio::spawn(async move {
            owner
                .change_binding(move || async move {
                    entered.send(()).unwrap();
                    held.await.unwrap();
                    *external.actor.lock().unwrap() = "B";
                    if uncertain {
                        Err(BackendError::new("host_unavailable", true, json!({})))
                    } else {
                        Ok(())
                    }
                })
                .await
        });
        entry.await.unwrap();
        waiter.abort();
        let _ = waiter.await;
        assert_eq!(
            bind(&identity, &host, "C").await.unwrap_err().code(),
            BINDING_BUSY
        );
        assert_eq!(
            old.command("provider", 1, "write", json!({}))
                .await
                .unwrap_err()
                .code(),
            BINDING_BUSY
        );
        assert!(queue.try_recv().is_err());
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let result = bind(&identity, &host, "B").await;
                match result {
                    Err(error) if error.code() == BINDING_BUSY => tokio::task::yield_now().await,
                    Err(error) => {
                        assert!(uncertain);
                        assert_eq!(error.code(), BINDING_UNCERTAIN);
                        break;
                    }
                    Ok(()) => {
                        assert!(!uncertain);
                        break;
                    }
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(*host.actor.lock().unwrap(), "B");
        assert_eq!(
            old.command("provider", 1, "write", json!({}))
                .await
                .unwrap_err()
                .code(),
            if uncertain {
                BINDING_UNCERTAIN
            } else {
                STALE_INTENT
            }
        );
        if uncertain {
            assert!(!identity.identity_ready());
            assert_eq!(
                identity
                    .intent(host.clone())
                    .query("p", 1, "read", json!({}))
                    .await
                    .unwrap_err()
                    .code(),
                BINDING_UNCERTAIN
            );
        }
    }
}

#[tokio::test]
async fn overflow_refuses_before_enqueue_without_releasing_existing_owners() {
    let identity = IdentityWork::default();
    let (host, mut queue) = host();
    let mut tasks = Vec::new();
    let mut held = Vec::new();
    for n in 0..MAX_PENDING_COMMANDS {
        let intent = identity.intent(host.clone());
        tasks.push(tokio::spawn(async move {
            intent.command("p", 1, "write", json!({"n":n})).await
        }));
        held.push(queue.recv().await.unwrap());
    }
    assert_eq!(
        identity
            .intent(host.clone())
            .command("p", 1, "overflow", json!({}))
            .await
            .unwrap_err()
            .code(),
        COMMAND_BUSY
    );
    assert!(queue.try_recv().is_err());
    assert_eq!(
        bind(&identity, &host, "B").await.unwrap_err().code(),
        BINDING_BUSY
    );
    for invocation in held {
        invocation.release.send(Ok(())).unwrap();
    }
    for task in tasks {
        assert_eq!(task.await.unwrap().unwrap()["actor"], "A");
    }
    bind(&identity, &host, "B").await.unwrap();
    assert_eq!(host.effects.lock().unwrap().len(), MAX_PENDING_COMMANDS);
}

#[tokio::test]
async fn model_tool_command_owner_survives_real_turn_cancellation_and_outer_timeout() {
    use teamarness_agent_package::catalog::{BundledCatalog, Catalog};
    use teamarness_agent_package::tools::{PortTools, UsableContract};
    use teamarness_agent_package::turn::{
        self, Assistant, ChatMessage, Model, ModelFailure, ToolCall, ToolDefinition, Turn,
        TurnBounds, TurnOutcome,
    };
    use teamarness_package_sdk::CancellationToken;
    struct ScriptedModel;
    impl Model for ScriptedModel {
        async fn respond(
            &self,
            _: &[ChatMessage],
            _: &[ToolDefinition],
        ) -> Result<Assistant, ModelFailure> {
            Ok(Assistant {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "delete-1".into(),
                    name: "exhibits__draft_delete".into(),
                    arguments: r#"{"exhibitId":"site"}"#.into(),
                }],
            })
        }
    }
    for cancel_turn in [false, true] {
        let identity = IdentityWork::default();
        let (host, mut queue) = host();
        let usable: Vec<UsableContract> = BundledCatalog::get()
            .contracts()
            .iter()
            .filter(|c| c.identity == "dev.teamarness.exhibits")
            .map(UsableContract::from)
            .collect();
        let tools = PortTools::new(&usable, identity.intent(host.clone())).unwrap();
        let token = CancellationToken::new();
        let mut run = Box::pin(turn::drive(
            Turn::new(
                "system".into(),
                vec![ChatMessage::user("delete site".into())],
                TurnBounds::PRODUCTION,
            ),
            &ScriptedModel,
            &tools,
            &token,
        ));
        let held = tokio::select! { held = queue.recv() => held.unwrap(), outcome = &mut run => panic!("turn ended before external enqueue: {outcome:?}") };
        if cancel_turn {
            token.cancel();
            assert_eq!(run.await, TurnOutcome::Cancelled);
        } else {
            assert!(tokio::time::timeout(Duration::ZERO, run).await.is_err());
        }
        assert_eq!(
            bind(&identity, &host, "B").await.unwrap_err().code(),
            BINDING_BUSY
        );
        held.release.send(Ok(())).unwrap();
        eventually_bind(&identity, &host).await;
        assert_eq!(
            *host.effects.lock().unwrap(),
            vec![json!({"actor":"A","port":"draft.delete","request":{"exhibitId":"site"}})]
        );
        assert!(queue.try_recv().is_err());
    }
}
