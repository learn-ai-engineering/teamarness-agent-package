//! The generic port tool executor: every usable contract's commands and queries as model tools.
//!
//! [`PortTools`] turns [`UsableContract`]s into [`ToolDefinition`]s named `<alias>__<port>` with
//! self-contained parameter schemas ([`inline_refs`]), and runs the model's calls. A call is
//! looked up, parsed and validated against the port's request schema before anything reaches the
//! Host; a call that fails any of those is refused locally with a stable `agent.*` code. A
//! dispatched command is never re-sent: when its outcome cannot be known, the model is told so.
//! Every outcome is a JSON text the model reads; nothing here is an error to the turn.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;

use jsonschema::{Draft, Validator};
use serde_json::{Map, Value, json};
use teamarness_package_sdk::ports::PortClient;
use teamarness_package_sdk::{BackendError, CancellationToken};

use crate::catalog::{CatalogContract, CatalogPort, PortKind, alias};
use crate::turn::{
    Activity, TOOL_ARGUMENTS_INVALID, TOOL_OUTCOME_UNKNOWN, TOOL_UNAVAILABLE, ToolCall,
    ToolDefinition, ToolExecutor, ToolOutcome,
};

/// How many `$ref`s may be written out inside one another.
pub const MAX_INLINE_DEPTH: usize = 8;
/// Validator messages returned with `agent.tool_arguments_invalid`.
const MAX_MESSAGES: usize = 3;
/// Characters in one validator message.
const MAX_MESSAGE_CHARS: usize = 200;
/// The SDK's code for work stopped by cancellation.
const CANCELLED: &str = "cancelled";

/// A contract the model may use now, minus any withheld ports.
///
/// Phase 4's `access::usable` decides which contracts these are; this module only turns them
/// into tools.
#[derive(Debug, Clone, PartialEq)]
pub struct UsableContract {
    pub identity: String,
    pub description: String,
    pub ports: Vec<CatalogPort>,
    /// The declaring document's `$defs`.
    pub defs: Map<String, Value>,
}

impl From<&CatalogContract> for UsableContract {
    fn from(contract: &CatalogContract) -> Self {
        Self {
            identity: contract.identity.clone(),
            description: contract.description.clone(),
            ports: contract.ports.clone(),
            defs: contract.defs.clone(),
        }
    }
}

/// The usable contracts could not be turned into tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadError(String);

impl fmt::Display for LoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "tools: {}", self.0)
    }
}

impl std::error::Error for LoadError {}

/// The tool name of `port` on the contract `identity`: `<alias>__<port with . → _>`.
///
/// # Errors
/// The name does not match `^[a-zA-Z0-9_-]{1,64}$`, the tool-name rule model endpoints apply.
pub fn tool_name(identity: &str, port: &str) -> Result<String, LoadError> {
    let name = format!("{}__{}", alias(identity), port.replace('.', "_"));
    let valid = (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    if valid {
        Ok(name)
    } else {
        Err(LoadError(format!(
            "{identity}/{port} has no valid tool name"
        )))
    }
}

/// `schema` with every `#/$defs/<name>` reference replaced by that definition, so the model gets
/// a self-contained parameter schema.
///
/// # Errors
/// A reference is not local, names a missing definition, refers back to a definition it is
/// inside of, or is nested more than [`MAX_INLINE_DEPTH`] deep.
pub fn inline_refs(schema: &Value, defs: &Map<String, Value>) -> Result<Value, LoadError> {
    inline(schema, defs, &mut Vec::new())
}

fn inline(
    schema: &Value,
    defs: &Map<String, Value>,
    open: &mut Vec<String>,
) -> Result<Value, LoadError> {
    match schema {
        Value::Object(fields) => {
            let mut inlined = Map::with_capacity(fields.len());
            for (key, value) in fields {
                if key != "$ref" {
                    inlined.insert(key.clone(), inline(value, defs, open)?);
                }
            }
            let Some(reference) = fields.get("$ref") else {
                return Ok(Value::Object(inlined));
            };
            let target = reference
                .as_str()
                .and_then(|reference| reference.strip_prefix("#/$defs/"))
                .filter(|name| !name.contains('/'))
                .map(|name| name.replace("~1", "/").replace("~0", "~"))
                .ok_or_else(|| {
                    LoadError(format!("reference {reference} is not a local definition"))
                })?;
            if open.contains(&target) {
                return Err(LoadError(format!("definition {target} is recursive")));
            }
            if open.len() >= MAX_INLINE_DEPTH {
                return Err(LoadError(format!("definition {target} is nested too deep")));
            }
            let definition = defs
                .get(&target)
                .ok_or_else(|| LoadError(format!("definition {target} is missing")))?;
            open.push(target);
            let written = inline(definition, defs, open)?;
            open.pop();
            if inlined.is_empty() {
                Ok(written)
            } else {
                // A reference beside other keywords applies alongside them.
                inlined.insert("allOf".into(), json!([written]));
                Ok(Value::Object(inlined))
            }
        }
        Value::Array(items) => items
            .iter()
            .map(|item| inline(item, defs, open))
            .collect::<Result<_, _>>()
            .map(Value::Array),
        other => Ok(other.clone()),
    }
}

/// Sends port requests to the Host. [`PortClient`] is the production caller; tests record calls.
pub trait PortCaller: Send + Sync {
    fn query(
        &self,
        contract: &str,
        port: &str,
        request: Value,
    ) -> impl Future<Output = Result<Value, BackendError>> + Send;

    fn command(
        &self,
        contract: &str,
        port: &str,
        request: Value,
    ) -> impl Future<Output = Result<Value, BackendError>> + Send;
}

impl PortCaller for PortClient {
    fn query(
        &self,
        contract: &str,
        port: &str,
        request: Value,
    ) -> impl Future<Output = Result<Value, BackendError>> + Send {
        PortClient::query(self, contract, port, request)
    }

    fn command(
        &self,
        contract: &str,
        port: &str,
        request: Value,
    ) -> impl Future<Output = Result<Value, BackendError>> + Send {
        PortClient::command(self, contract, port, request)
    }
}

struct PortTool {
    contract: String,
    alias: String,
    port: String,
    kind: PortKind,
    validator: Validator,
}

impl PortTool {
    fn activity(&self, code: Option<&str>) -> Option<Activity> {
        Some(Activity {
            package: self.alias.clone(),
            port: self.port.clone(),
            ok: code.is_none(),
            code: code.map(str::to_owned),
        })
    }

    fn refused(&self, code: &str, retryable: bool) -> ToolOutcome {
        ToolOutcome {
            content: refusal(code, retryable),
            activity: self.activity(Some(code)),
        }
    }
}

/// The tools of every usable contract's commands and queries, with their validators.
pub struct PortTools<P> {
    ports: P,
    definitions: Vec<ToolDefinition>,
    tools: HashMap<String, PortTool>,
}

impl<P: PortCaller> PortTools<P> {
    /// Builds a tool for each command and query of `usable`. Events are never tools.
    ///
    /// # Errors
    /// A port has no valid tool name, two ports share one, or a request schema cannot be
    /// written out or compiled.
    pub fn new(usable: &[UsableContract], ports: P) -> Result<Self, LoadError> {
        let mut definitions = Vec::new();
        let mut tools = HashMap::new();
        for contract in usable {
            for port in &contract.ports {
                if port.kind == PortKind::Event {
                    continue;
                }
                let name = tool_name(&contract.identity, &port.name)?;
                let request = port.request.as_ref().ok_or_else(|| {
                    LoadError(format!(
                        "{}/{} has no request schema",
                        contract.identity, port.name
                    ))
                })?;
                let parameters = inline_refs(request, &contract.defs)
                    .map_err(|LoadError(cause)| LoadError(format!("{name}: {cause}")))?;
                let validator = compile(request, &contract.defs)
                    .map_err(|cause| LoadError(format!("{name}: {cause}")))?;
                let tool = PortTool {
                    contract: contract.identity.clone(),
                    alias: alias(&contract.identity).to_owned(),
                    port: port.name.clone(),
                    kind: port.kind,
                    validator,
                };
                if tools.insert(name.clone(), tool).is_some() {
                    return Err(LoadError(format!("tool name {name} is used twice")));
                }
                definitions.push(ToolDefinition {
                    name,
                    description: port.description.clone(),
                    parameters,
                });
            }
        }
        Ok(Self {
            ports,
            definitions,
            tools,
        })
    }
}

/// Compiles `request` with the document's `$defs` attached at its root, so its local
/// references resolve. Remote references are refused: no resolver fetches anything.
fn compile(request: &Value, defs: &Map<String, Value>) -> Result<Validator, String> {
    let schema = match request {
        Value::Object(fields) => {
            let mut fields = fields.clone();
            fields.insert("$defs".into(), Value::Object(defs.clone()));
            Value::Object(fields)
        }
        other => json!({"$defs": defs, "allOf": [other]}),
    };
    jsonschema::options()
        .with_draft(Draft::Draft202012)
        .build(&schema)
        .map_err(|error| error.to_string())
}

fn refusal(code: &str, retryable: bool) -> String {
    json!({"error": code, "retryable": retryable}).to_string()
}

fn bounded(message: String) -> String {
    if message.chars().count() <= MAX_MESSAGE_CHARS {
        message
    } else {
        message.chars().take(MAX_MESSAGE_CHARS).collect()
    }
}

fn arguments_invalid(tool: &PortTool, messages: Vec<String>) -> ToolOutcome {
    ToolOutcome {
        content: json!({
            "error": TOOL_ARGUMENTS_INVALID,
            "retryable": false,
            "messages": messages,
        })
        .to_string(),
        activity: tool.activity(Some(TOOL_ARGUMENTS_INVALID)),
    }
}

impl<P: PortCaller> ToolExecutor for PortTools<P> {
    fn definitions(&self) -> &[ToolDefinition] {
        &self.definitions
    }

    async fn run(&self, call: &ToolCall, cancel: &CancellationToken) -> ToolOutcome {
        let Some(tool) = self.tools.get(&call.name) else {
            return ToolOutcome {
                content: refusal(TOOL_UNAVAILABLE, false),
                activity: None,
            };
        };
        // Some endpoints send no text at all for a call without arguments.
        let arguments = if call.arguments.trim().is_empty() {
            Ok(json!({}))
        } else {
            serde_json::from_str::<Value>(&call.arguments)
        };
        let request = match arguments {
            Ok(request) => request,
            Err(error) => {
                return arguments_invalid(
                    tool,
                    vec![bounded(format!("arguments are not JSON: {error}"))],
                );
            }
        };
        let messages: Vec<String> = tool
            .validator
            .iter_errors(&request)
            .take(MAX_MESSAGES)
            .map(|error| bounded(format!("{}: {error}", error.instance_path())))
            .collect();
        if !messages.is_empty() {
            return arguments_invalid(tool, messages);
        }
        if cancel.is_cancelled() {
            return tool.refused(CANCELLED, false);
        }
        let result = match tool.kind {
            PortKind::Query => tokio::select! {
                biased;
                () = cancel.cancelled() => return tool.refused(CANCELLED, false),
                result = self.ports.query(&tool.contract, &tool.port, request) => result,
            },
            // Sent once. Once polled it may have taken effect, so cancellation from here on
            // leaves its outcome unknown; it is never re-sent either way.
            PortKind::Command => tokio::select! {
                biased;
                () = cancel.cancelled() => return tool.refused(TOOL_OUTCOME_UNKNOWN, false),
                result = self.ports.command(&tool.contract, &tool.port, request) => result,
            },
            PortKind::Event => return tool.refused(TOOL_UNAVAILABLE, false),
        };
        match result {
            Ok(value) => ToolOutcome {
                content: value.to_string(),
                activity: tool.activity(None),
            },
            Err(error) if tool.kind == PortKind::Command && error.outcome_uncertain() => {
                tool.refused(TOOL_OUTCOME_UNKNOWN, false)
            }
            Err(error) => {
                let mut content = json!({"error": error.code(), "retryable": error.retryable()});
                // Bounded by the SDK; a Provider uses it to say how to recover.
                if error
                    .details()
                    .as_object()
                    .is_some_and(|details| !details.is_empty())
                {
                    content["details"] = error.details().clone();
                }
                ToolOutcome {
                    content: content.to_string(),
                    activity: tool.activity(Some(error.code())),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::sync::Notify;

    use super::*;
    use crate::catalog::{BundledCatalog, Catalog, parse_document};

    const EXHIBITS: &str = "dev.teamarness.exhibits";

    /// One recorded port request: contract, port, kind and request.
    type Sent = (String, String, &'static str, Value);

    enum Answer {
        Reply(Result<Value, BackendError>),
        /// Never answers, after telling the test it was called.
        Hang,
    }

    /// Records every port request and answers from a script.
    #[derive(Clone)]
    struct FakePorts {
        calls: Arc<Mutex<Vec<Sent>>>,
        answer: Arc<dyn Fn(&str) -> Answer + Send + Sync>,
        called: Arc<Notify>,
    }

    impl FakePorts {
        fn new(answer: impl Fn(&str) -> Answer + Send + Sync + 'static) -> Self {
            Self {
                calls: Arc::default(),
                answer: Arc::new(answer),
                called: Arc::new(Notify::new()),
            }
        }

        fn ok() -> Self {
            Self::new(|_| Answer::Reply(Ok(json!({"ok": true}))))
        }

        fn calls(&self) -> Vec<Sent> {
            self.calls.lock().unwrap().clone()
        }

        async fn invoke(
            &self,
            contract: &str,
            port: &str,
            kind: &'static str,
            request: Value,
        ) -> Result<Value, BackendError> {
            self.calls
                .lock()
                .unwrap()
                .push((contract.into(), port.into(), kind, request));
            self.called.notify_one();
            match (self.answer)(port) {
                Answer::Reply(result) => result,
                Answer::Hang => std::future::pending().await,
            }
        }
    }

    impl PortCaller for FakePorts {
        fn query(
            &self,
            contract: &str,
            port: &str,
            request: Value,
        ) -> impl Future<Output = Result<Value, BackendError>> + Send {
            self.invoke(contract, port, "query", request)
        }

        fn command(
            &self,
            contract: &str,
            port: &str,
            request: Value,
        ) -> impl Future<Output = Result<Value, BackendError>> + Send {
            self.invoke(contract, port, "command", request)
        }
    }

    fn usable(identities: &[&str]) -> Vec<UsableContract> {
        identities
            .iter()
            .map(|identity| {
                BundledCatalog::get()
                    .contracts()
                    .iter()
                    .find(|contract| contract.identity == *identity)
                    .map(UsableContract::from)
                    .unwrap()
            })
            .collect()
    }

    fn exhibits(ports: &FakePorts) -> PortTools<FakePorts> {
        PortTools::new(&usable(&[EXHIBITS]), ports.clone()).expect("exhibits load")
    }

    fn call(name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: "call_1".into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }

    fn content(outcome: &ToolOutcome) -> Value {
        serde_json::from_str(&outcome.content).expect("content is JSON")
    }

    fn activity(port: &str, code: Option<&str>) -> Option<Activity> {
        Some(Activity {
            package: "exhibits".into(),
            port: port.into(),
            ok: code.is_none(),
            code: code.map(str::to_owned),
        })
    }

    fn contains_ref(value: &Value) -> bool {
        match value {
            Value::Object(fields) => {
                fields.contains_key("$ref") || fields.values().any(contains_ref)
            }
            Value::Array(items) => items.iter().any(contains_ref),
            _ => false,
        }
    }

    #[test]
    fn names_are_valid_and_unique_for_bundle() {
        let everything: Vec<UsableContract> = BundledCatalog::get()
            .contracts()
            .iter()
            .map(UsableContract::from)
            .collect();
        let tools = PortTools::new(&everything, FakePorts::ok()).expect("the bundle loads");
        let names: Vec<&str> = tools
            .definitions()
            .iter()
            .map(|definition| definition.name.as_str())
            .collect();
        // Every command and query: 4 of Conversations, 3 of Documents, 8 of Exhibits.
        assert_eq!(names.len(), 15);
        let unique: std::collections::BTreeSet<_> = names.iter().collect();
        assert_eq!(unique.len(), names.len());
        for name in &names {
            assert!(
                (1..=64).contains(&name.len())
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
                "{name}"
            );
        }
        assert!(names.contains(&"exhibits__runtime_guide"));
        assert!(names.contains(&"documents__document_list"));
        assert!(names.contains(&"conversations__message_post"));

        assert_eq!(
            tool_name("dev.teamarness.exhibits", "exhibit.list").unwrap(),
            "exhibits__exhibit_list"
        );
        assert!(tool_name("dev.teamarness.exhibits", &"x".repeat(60)).is_err());
        assert!(tool_name("dev.teamarness.ex hibits", "list").is_err());

        // Two contracts with one alias clash.
        let mut twin = usable(&[EXHIBITS]).remove(0);
        twin.identity = "dev.other.exhibits".into();
        let clash = PortTools::new(&[usable(&[EXHIBITS]).remove(0), twin], FakePorts::ok());
        assert!(clash.is_err());
    }

    #[test]
    fn refs_are_inlined_and_recursion_is_an_error() {
        let tools = exhibits(&FakePorts::ok());
        for definition in tools.definitions() {
            assert!(!contains_ref(&definition.parameters), "{}", definition.name);
        }
        let update = tools
            .definitions()
            .iter()
            .find(|definition| definition.name == "exhibits__draft_update")
            .unwrap();
        assert_eq!(
            update.parameters["properties"]["files"]["items"]["properties"]["sha256"],
            json!({"type": "string", "pattern": "^[0-9a-f]{64}$"})
        );
        assert_eq!(update.parameters["properties"]["title"]["maxLength"], 120);

        let defs = |value: Value| value.as_object().unwrap().clone();
        // Nested references are written out; a reference beside other keywords keeps them.
        let nested = defs(json!({"a": {"$ref": "#/$defs/b"}, "b": {"type": "integer"}}));
        assert_eq!(
            inline_refs(
                &json!({"properties": {"x": {"$ref": "#/$defs/a", "minimum": 1}}}),
                &nested
            )
            .unwrap(),
            json!({"properties": {"x": {"minimum": 1, "allOf": [{"type": "integer"}]}}})
        );
        // Reusing one definition twice side by side is not recursion.
        assert!(
            inline_refs(
                &json!({"items": [{"$ref": "#/$defs/b"}, {"$ref": "#/$defs/b"}]}),
                &nested
            )
            .is_ok()
        );

        let recursive = defs(json!({"node": {"properties": {"next": {"$ref": "#/$defs/node"}}}}));
        assert!(inline_refs(&json!({"$ref": "#/$defs/node"}), &recursive).is_err());
        let mutual =
            defs(json!({"a": {"$ref": "#/$defs/b"}, "b": {"items": {"$ref": "#/$defs/a"}}}));
        assert!(inline_refs(&json!({"$ref": "#/$defs/a"}), &mutual).is_err());

        let mut chain = Map::new();
        for level in 0..MAX_INLINE_DEPTH {
            chain.insert(
                format!("d{level}"),
                json!({"$ref": format!("#/$defs/d{}", level + 1)}),
            );
        }
        chain.insert(format!("d{MAX_INLINE_DEPTH}"), json!({"type": "string"}));
        assert!(inline_refs(&json!({"$ref": "#/$defs/d1"}), &chain).is_ok());
        assert!(inline_refs(&json!({"$ref": "#/$defs/d0"}), &chain).is_err());

        assert!(inline_refs(&json!({"$ref": "#/$defs/missing"}), &nested).is_err());
        assert!(inline_refs(&json!({"$ref": "https://example.com/s.json"}), &nested).is_err());

        // A recursive request schema is a load error.
        let document = json!({
            "schemaVersion": 1,
            "$defs": {"node": {"type": "object", "properties": {"next": {"$ref": "#/$defs/node"}}}},
            "contracts": [{"identity": "dev.example.tree", "major": 1, "minor": 0,
                "description": "Tree.", "ports": [
                    {"name": "node.put", "kind": "command", "description": "Put.",
                     "request": {"$ref": "#/$defs/node"}, "response": {"type": "object"}}]}]
        });
        let contracts = parse_document(&document.to_string()).unwrap();
        let contracts: Vec<UsableContract> = contracts.iter().map(UsableContract::from).collect();
        assert!(PortTools::new(&contracts, FakePorts::ok()).is_err());
    }

    #[tokio::test]
    async fn invalid_arguments_never_dispatch() {
        let ports = FakePorts::ok();
        let tools = exhibits(&ports);
        let cancel = CancellationToken::new();
        let oversized = format!(
            r#"{{"sha256":"{}","size":1,"chunkIndex":0,"chunkCount":1,"data":"{}"}}"#,
            "a".repeat(64),
            "A".repeat(200_000)
        );
        for (name, arguments) in [
            ("exhibits__exhibit_list", r#"{"limit": 0}"#),
            ("exhibits__exhibit_list", r#"{"limit": 5, "extra": true}"#),
            ("exhibits__exhibit_list", "not json"),
            ("exhibits__exhibit_list", "[]"),
            ("exhibits__exhibit_read", "{}"),
            (
                "exhibits__draft_update",
                r#"{"exhibitId": "Bad Id", "title": "", "entry": 1, "files": []}"#,
            ),
            ("exhibits__file_put", oversized.as_str()),
        ] {
            let outcome = tools.run(&call(name, arguments), &cancel).await;
            let body = content(&outcome);
            assert_eq!(
                body["error"], TOOL_ARGUMENTS_INVALID,
                "{name} {arguments:.40}"
            );
            assert_eq!(body["retryable"], false);
            let messages = body["messages"].as_array().unwrap();
            assert!((1..=MAX_MESSAGES).contains(&messages.len()), "{messages:?}");
            for message in messages {
                assert!(message.as_str().unwrap().chars().count() <= MAX_MESSAGE_CHARS);
            }
            let port = name.trim_start_matches("exhibits__").replace('_', ".");
            assert_eq!(
                outcome.activity,
                activity(&port, Some(TOOL_ARGUMENTS_INVALID))
            );
        }
        assert!(ports.calls().is_empty());

        // The same port with valid arguments is dispatched.
        let outcome = tools
            .run(&call("exhibits__exhibit_list", r#"{"limit": 5}"#), &cancel)
            .await;
        assert_eq!(content(&outcome), json!({"ok": true}));
        assert_eq!(ports.calls().len(), 1);
    }

    #[tokio::test]
    async fn unknown_tool_is_refused_without_host_call() {
        let ports = FakePorts::ok();
        let tools = exhibits(&ports);
        let cancel = CancellationToken::new();
        for name in [
            "documents__document_list",
            "exhibits__missing",
            "skill_read",
            "",
        ] {
            let outcome = tools.run(&call(name, "{}"), &cancel).await;
            assert_eq!(
                content(&outcome),
                json!({"error": TOOL_UNAVAILABLE, "retryable": false})
            );
            assert_eq!(outcome.activity, None);
        }
        assert!(ports.calls().is_empty());

        let nothing = PortTools::new(&[], ports.clone()).unwrap();
        assert!(nothing.definitions().is_empty());
        let outcome = nothing
            .run(&call("exhibits__runtime_guide", "{}"), &cancel)
            .await;
        assert_eq!(content(&outcome)["error"], TOOL_UNAVAILABLE);
        assert!(ports.calls().is_empty());
    }

    #[tokio::test]
    async fn guide_dispatches_as_query() {
        let guide = json!({"markdown": "# Exhibits", "idPattern": "^[a-z]+$"});
        let answer = guide.clone();
        let ports = FakePorts::new(move |_| Answer::Reply(Ok(answer.clone())));
        let tools = exhibits(&ports);
        let cancel = CancellationToken::new();
        let outcome = tools
            .run(&call("exhibits__runtime_guide", "{}"), &cancel)
            .await;
        assert_eq!(content(&outcome), guide);
        assert_eq!(outcome.activity, activity("runtime.guide", None));
        // No text at all means no arguments.
        tools
            .run(&call("exhibits__runtime_guide", ""), &cancel)
            .await;
        assert_eq!(
            ports.calls(),
            [
                (EXHIBITS.into(), "runtime.guide".into(), "query", json!({})),
                (EXHIBITS.into(), "runtime.guide".into(), "query", json!({})),
            ]
        );
    }

    #[tokio::test]
    async fn provider_error_reaches_model_with_code() {
        let ports = FakePorts::new(|port| {
            Answer::Reply(Err(match port {
                "draft.update" => BackendError::new("exhibits.type_refused", false, json!({})),
                _ => BackendError::new("exhibits.not_found", true, json!({"exhibitId": "a"})),
            }))
        });
        let tools = exhibits(&ports);
        let cancel = CancellationToken::new();
        let arguments = json!({
            "exhibitId": "site", "title": "Site", "entry": "index.exe",
            "files": [{"path": "index.exe", "sha256": "a".repeat(64)}], "expectedRevision": 0
        });
        let outcome = tools
            .run(
                &call("exhibits__draft_update", &arguments.to_string()),
                &cancel,
            )
            .await;
        assert_eq!(
            content(&outcome),
            json!({"error": "exhibits.type_refused", "retryable": false})
        );
        assert_eq!(
            outcome.activity,
            activity("draft.update", Some("exhibits.type_refused"))
        );

        let outcome = tools
            .run(
                &call("exhibits__exhibit_read", r#"{"exhibitId": "a"}"#),
                &cancel,
            )
            .await;
        assert_eq!(
            content(&outcome),
            json!({"error": "exhibits.not_found", "retryable": true, "details": {"exhibitId": "a"}})
        );
        assert_eq!(ports.calls().len(), 2);
        assert_eq!(ports.calls()[0].2, "command");
        assert_eq!(ports.calls()[0].3, arguments);
    }

    #[tokio::test]
    async fn uncertain_or_cancelled_command_is_unknown_and_sent_once() {
        let delete = r#"{"exhibitId": "site"}"#;
        let unknown = json!({"error": TOOL_OUTCOME_UNKNOWN, "retryable": false});

        // The Host says the command may have taken effect.
        let ports = FakePorts::new(|_| {
            Answer::Reply(Err(
                BackendError::new("timeout", true, json!({})).with_outcome_uncertain(true)
            ))
        });
        let tools = exhibits(&ports);
        let outcome = tools
            .run(
                &call("exhibits__draft_delete", delete),
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(content(&outcome), unknown);
        assert_eq!(
            outcome.activity,
            activity("draft.delete", Some(TOOL_OUTCOME_UNKNOWN))
        );
        assert_eq!(ports.calls().len(), 1);

        // Cancelled after the command was sent.
        let ports = FakePorts::new(|_| Answer::Hang);
        let tools = exhibits(&ports);
        let cancel = CancellationToken::new();
        let trigger = async {
            ports.called.notified().await;
            cancel.cancel();
        };
        let sent = call("exhibits__draft_delete", delete);
        let (outcome, ()) = tokio::join!(tools.run(&sent, &cancel), trigger);
        assert_eq!(content(&outcome), unknown);
        assert_eq!(
            outcome.activity,
            activity("draft.delete", Some(TOOL_OUTCOME_UNKNOWN))
        );
        assert_eq!(ports.calls().len(), 1);

        // Cancelled before: nothing is sent.
        let ports = FakePorts::ok();
        let tools = exhibits(&ports);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let outcome = tools
            .run(&call("exhibits__draft_delete", delete), &cancel)
            .await;
        assert_eq!(content(&outcome)["error"], CANCELLED);
        assert!(ports.calls().is_empty());
    }

    #[tokio::test]
    async fn events_are_not_tools() {
        let ports = FakePorts::ok();
        let tools = PortTools::new(
            &usable(&["dev.teamarness.conversations", "dev.teamarness.documents"]),
            ports.clone(),
        )
        .unwrap();
        let names: Vec<&str> = tools
            .definitions()
            .iter()
            .map(|definition| definition.name.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "conversations__message_post",
                "conversations__message_page",
                "conversations__message_changes",
                "conversations__channel_list",
                "documents__document_list",
                "documents__document_markdown",
                "documents__document_revise",
            ]
        );
        for name in [
            "conversations__messages_changed",
            "documents__documents_changed",
        ] {
            let outcome = tools
                .run(&call(name, "{}"), &CancellationToken::new())
                .await;
            assert_eq!(content(&outcome)["error"], TOOL_UNAVAILABLE);
        }
        assert!(ports.calls().is_empty());
    }
}
