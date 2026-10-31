//! The turn engine: one bounded exchange of model calls and tool calls that ends in final text
//! or a stable code.
//!
//! [`Turn`] is a pure step machine with no I/O: it says what to do next ([`Step`]) and is told
//! what happened ([`Turn::model_answered`], [`Turn::tool_finished`]). It enforces every bound in
//! [`TurnBounds`] except the deadline, which [`drive`] applies while it runs the machine against
//! a [`Model`] and a [`ToolExecutor`]. Tool calls run one at a time, in the order the model gave
//! them. Nothing here retries a model call or a tool call, and nothing is truncated silently: a
//! bound either ends the turn with `agent.turn_limit_exceeded` or, for an oversized tool result,
//! replaces the result with a coded error the model can read. A turn that fails keeps the
//! activity of the tool calls that ran before it ended.
//!
//! The message bound covers only what a turn adds to its starting messages. The system message
//! and history are sent whatever their size, as they were before tools existed. A tool call whose
//! arguments would carry the turn past that bound is refused before it runs, so a call never
//! takes effect only for the turn to fail on its size afterwards.

use std::collections::VecDeque;
use std::future::Future;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use teamarness_package_sdk::CancellationToken;

pub use crate::model::{Assistant, ChatMessage, ModelFailure, ToolCall, ToolDefinition};

/// A turn-level bound was exceeded: model calls, tool calls, message size or the deadline.
pub const TURN_LIMIT_EXCEEDED: &str = "agent.turn_limit_exceeded";
/// A tool result was over the per-result cap; the model sees this code instead of the result.
pub const TOOL_RESULT_TOO_LARGE: &str = "agent.tool_result_too_large";
/// A tool call's arguments are not JSON or fail the port's request schema; never dispatched.
pub const TOOL_ARGUMENTS_INVALID: &str = "agent.tool_arguments_invalid";
/// A command was sent but its outcome is unknown; it is never re-sent.
pub const TOOL_OUTCOME_UNKNOWN: &str = "agent.tool_outcome_unknown";
/// The model named a tool that is unknown or not usable now.
pub const TOOL_UNAVAILABLE: &str = "agent.tool_unavailable";
/// A tool call's arguments would carry the turn past its message bound; never dispatched.
pub const TOOL_ARGUMENTS_TOO_LARGE: &str = "agent.tool_arguments_too_large";

/// What a refused call's arguments become in the turn's messages, so later model calls stay
/// inside the message bound.
const ELIDED_ARGUMENTS: &str = r#"{"elided":"agent.tool_arguments_too_large"}"#;
/// The share of `message_bytes` held back for each call's result when a batch's arguments are
/// measured: 1/128, which is 768 bytes under the production bounds.
const RESULT_RESERVE_SHARE: usize = 128;

/// The limits of one turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnBounds {
    /// Model calls per turn.
    pub model_calls: usize,
    /// Tool calls per turn, across every model call.
    pub tool_calls: usize,
    /// Characters in one tool result.
    pub result_chars: usize,
    /// Serialized bytes the turn's own tool exchange adds to its starting messages, checked
    /// before each model call.
    pub message_bytes: usize,
    /// Wall-clock time for the whole turn.
    pub deadline: Duration,
}

impl TurnBounds {
    pub const PRODUCTION: Self = Self {
        model_calls: 8,
        tool_calls: 16,
        result_chars: 16_384,
        message_bytes: 96 * 1024,
        deadline: Duration::from_secs(180),
    };
}

/// One Package operation a tool call used, as the Steward sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activity {
    pub package: String,
    pub port: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

/// What one tool call returned to the model, and what it did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    /// The text the model reads: a JSON result or a `{"error","retryable"}` object.
    pub content: String,
    pub activity: Option<Activity>,
}

/// A finished turn: the final text and every tool call's activity, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnResult {
    pub text: String,
    pub activity: Vec<Activity>,
}

/// What the caller must do next.
#[derive(Debug, PartialEq, Eq)]
pub enum Step<'a> {
    /// Ask the model, offering the executor's tools; answer with [`Turn::model_answered`].
    CallModel(&'a [ChatMessage]),
    /// Run this call; answer with [`Turn::tool_finished`].
    RunTool(ToolCall),
    /// Do not run this call: the turn refused it with `code`. Answer with [`Turn::tool_refused`]
    /// and the activity the call records.
    RefuseTool {
        call: ToolCall,
        code: &'static str,
    },
    Done(TurnResult),
    /// The turn ended with this code and retryability, keeping the activity of every tool call
    /// that ran before it ended.
    Failed {
        code: &'static str,
        retryable: bool,
        activity: Vec<Activity>,
    },
}

/// How a driven turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    Done(TurnResult),
    /// As [`Step::Failed`]: a code, its retryability, and the activity recorded before the end.
    Failed {
        code: &'static str,
        retryable: bool,
        activity: Vec<Activity>,
    },
    /// The caller's cancellation token fired; nothing further was called.
    Cancelled,
}

impl TurnOutcome {
    /// A failure before any tool call ran.
    #[must_use]
    pub fn failed(code: &'static str, retryable: bool) -> Self {
        Self::Failed {
            code,
            retryable,
            activity: Vec::new(),
        }
    }
}

#[derive(Debug)]
enum Phase {
    NeedModel,
    AwaitingModel,
    AwaitingTool(ToolCall),
    /// A call refused before it ran, and the result the model reads instead.
    Refusing(ToolCall, String),
    Done(TurnResult),
    Failed(&'static str, bool),
}

/// One turn's state. Pure: it never calls the model or a tool itself.
#[derive(Debug)]
pub struct Turn {
    messages: Vec<ChatMessage>,
    /// The serialized size of the starting messages, which the message bound does not count.
    baseline: usize,
    bounds: TurnBounds,
    model_calls: usize,
    tool_calls: usize,
    /// The batch's calls still to answer, each with the refusal it gets instead of running.
    pending: VecDeque<(ToolCall, Option<String>)>,
    activity: Vec<Activity>,
    phase: Phase,
}

impl Turn {
    /// A turn that starts with the `system` message followed by `history`. Its tools are the
    /// executor's that [`drive`] runs it with.
    #[must_use]
    pub fn new(system: String, history: Vec<ChatMessage>, bounds: TurnBounds) -> Self {
        let mut messages = Vec::with_capacity(history.len() + 1);
        messages.push(ChatMessage::system(system));
        messages.extend(history);
        let baseline = message_bytes(&messages);
        Self {
            messages,
            baseline,
            bounds,
            model_calls: 0,
            tool_calls: 0,
            pending: VecDeque::new(),
            activity: Vec::new(),
            phase: Phase::NeedModel,
        }
    }

    #[must_use]
    pub fn bounds(&self) -> TurnBounds {
        self.bounds
    }

    /// The next step. Asking again before answering repeats the same step without counting it
    /// twice.
    #[expect(
        clippy::should_implement_trait,
        reason = "a turn is a step machine answered between steps, not an iterator"
    )]
    pub fn next(&mut self) -> Step<'_> {
        if matches!(self.phase, Phase::NeedModel) {
            let added = message_bytes(&self.messages).saturating_sub(self.baseline);
            self.phase = if self.model_calls >= self.bounds.model_calls
                || added > self.bounds.message_bytes
            {
                Phase::Failed(TURN_LIMIT_EXCEEDED, false)
            } else {
                self.model_calls += 1;
                Phase::AwaitingModel
            };
        }
        match &self.phase {
            Phase::NeedModel | Phase::AwaitingModel => Step::CallModel(&self.messages),
            Phase::AwaitingTool(call) => Step::RunTool(call.clone()),
            Phase::Refusing(call, _) => Step::RefuseTool {
                call: call.clone(),
                code: TOOL_ARGUMENTS_TOO_LARGE,
            },
            Phase::Done(result) => Step::Done(result.clone()),
            Phase::Failed(code, retryable) => Step::Failed {
                code,
                retryable: *retryable,
                activity: self.activity.clone(),
            },
        }
    }

    /// Records the model's answer. Ignored unless the turn is waiting for one.
    pub fn model_answered(&mut self, answer: Result<Assistant, ModelFailure>) {
        if !matches!(self.phase, Phase::AwaitingModel) {
            return;
        }
        let assistant = match answer {
            Ok(assistant) => assistant,
            Err(ModelFailure { code, retryable }) => {
                self.phase = Phase::Failed(code, retryable);
                return;
            }
        };
        if assistant.tool_calls.is_empty() {
            self.phase = match assistant.content {
                Some(text) => Phase::Done(TurnResult {
                    text,
                    activity: std::mem::take(&mut self.activity),
                }),
                None => Phase::Failed(crate::model::MODEL_ERROR, false),
            };
            return;
        }
        // A batch that would cross the budget, or that no later model call could read, runs
        // none of its calls.
        if self.model_calls >= self.bounds.model_calls
            || self.tool_calls + assistant.tool_calls.len() > self.bounds.tool_calls
        {
            self.phase = Phase::Failed(TURN_LIMIT_EXCEEDED, false);
            return;
        }
        self.tool_calls += assistant.tool_calls.len();
        let mut tool_calls = assistant.tool_calls;
        let refusals = self.refuse_oversized(assistant.content.as_ref(), &mut tool_calls);
        self.pending = tool_calls.iter().cloned().zip(refusals).collect();
        self.messages.push(ChatMessage::assistant_tool_calls(
            assistant.content,
            tool_calls,
        ));
        self.advance();
    }

    /// Refuses, in model order, each call whose arguments would carry the turn's own messages
    /// past `message_bytes`, holding back a share of the bound for every call's result. A refused
    /// call's arguments are elided in place; the returned list has its refusal at its position.
    fn refuse_oversized(
        &self,
        content: Option<&String>,
        calls: &mut [ToolCall],
    ) -> Vec<Option<String>> {
        let used = message_bytes(&self.messages).saturating_sub(self.baseline);
        // The batch as stored with every argument elided, plus the comma that joins it.
        let elided: Vec<ToolCall> = calls
            .iter()
            .map(|call| ToolCall {
                arguments: ELIDED_ARGUMENTS.into(),
                ..call.clone()
            })
            .collect();
        let batch = message_bytes(&[ChatMessage::assistant_tool_calls(content.cloned(), elided)])
            .saturating_sub(1);
        let reserve = calls
            .len()
            .saturating_mul(self.bounds.message_bytes / RESULT_RESERVE_SHARE);
        let mut remaining = self
            .bounds
            .message_bytes
            .saturating_sub(used.saturating_add(batch).saturating_add(reserve));
        let elided_size = json_string_bytes(ELIDED_ARGUMENTS);
        calls
            .iter_mut()
            .map(|call| {
                let extra = json_string_bytes(&call.arguments).saturating_sub(elided_size);
                if extra <= remaining {
                    remaining -= extra;
                    return None;
                }
                let refusal = serde_json::json!({
                    "error": TOOL_ARGUMENTS_TOO_LARGE,
                    "retryable": false,
                    "argumentBytes": call.arguments.len(),
                    "remainingBytes": remaining,
                })
                .to_string();
                call.arguments = ELIDED_ARGUMENTS.into();
                Some(refusal)
            })
            .collect()
    }

    /// Records the result of the call [`Step::RunTool`] named. Ignored unless the turn is
    /// waiting for one.
    pub fn tool_finished(&mut self, outcome: ToolOutcome) {
        let Phase::AwaitingTool(call) = std::mem::replace(&mut self.phase, Phase::NeedModel) else {
            return;
        };
        let ToolOutcome {
            mut content,
            mut activity,
        } = outcome;
        if content.chars().count() > self.bounds.result_chars {
            content =
                serde_json::json!({"error": TOOL_RESULT_TOO_LARGE, "retryable": false}).to_string();
            if let Some(activity) = &mut activity {
                activity.ok = false;
                activity.code = Some(TOOL_RESULT_TOO_LARGE.to_owned());
            }
        }
        self.messages.push(ChatMessage::tool(call.id, content));
        self.activity.extend(activity);
        self.advance();
    }

    /// Records the refusal [`Step::RefuseTool`] named, with the `activity` the call records.
    /// Ignored unless the turn is refusing a call.
    pub fn tool_refused(&mut self, activity: Option<Activity>) {
        if !matches!(self.phase, Phase::Refusing(..)) {
            return;
        }
        let Phase::Refusing(call, content) = std::mem::replace(&mut self.phase, Phase::NeedModel)
        else {
            return;
        };
        self.messages.push(ChatMessage::tool(call.id, content));
        self.activity.extend(activity);
        self.advance();
    }

    /// The next pending call, or another model call once the batch is done.
    fn advance(&mut self) {
        self.phase = match self.pending.pop_front() {
            Some((call, None)) => Phase::AwaitingTool(call),
            Some((call, Some(refusal))) => Phase::Refusing(call, refusal),
            None => Phase::NeedModel,
        };
    }

    /// The turn ended at the deadline, keeping the activity recorded so far.
    fn out_of_time(&self) -> TurnOutcome {
        TurnOutcome::Failed {
            code: TURN_LIMIT_EXCEEDED,
            retryable: false,
            activity: self.activity.clone(),
        }
    }
}

fn message_bytes(messages: &[ChatMessage]) -> usize {
    // A message list that cannot be serialized cannot be sent either.
    serde_json::to_vec(messages).map_or(usize::MAX, |bytes| bytes.len())
}

/// The bytes `text` takes as a JSON string, quotes and escapes included.
fn json_string_bytes(text: &str) -> usize {
    serde_json::to_vec(text).map_or(usize::MAX, |bytes| bytes.len())
}

/// The model a turn calls.
pub trait Model {
    fn respond(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
    ) -> impl Future<Output = Result<Assistant, ModelFailure>> + Send;
}

/// Runs the tool calls a turn asks for.
pub trait ToolExecutor {
    /// The tools a turn offers the model.
    fn definitions(&self) -> &[ToolDefinition];

    /// Runs one call. Every failure is an outcome the model reads, never an error here.
    fn run(
        &self,
        call: &ToolCall,
        cancel: &CancellationToken,
    ) -> impl Future<Output = ToolOutcome> + Send;

    /// The activity `call` records when the turn refused it with `code` without running it;
    /// `None` when it names no tool offered here.
    fn refused(&self, call: &ToolCall, code: &str) -> Option<Activity>;
}

/// Runs `turn` to its end under `cancel` and the turn's deadline.
///
/// Every model call offers `executor`'s tools. Cancellation wins over everything: once `cancel`
/// fires no further model or tool call starts, and the result is [`TurnOutcome::Cancelled`]. The
/// deadline ends the turn with `agent.turn_limit_exceeded` and the activity recorded so far.
pub async fn drive<M: Model, E: ToolExecutor>(
    mut turn: Turn,
    model: &M,
    executor: &E,
    cancel: &CancellationToken,
) -> TurnOutcome {
    let deadline = turn.bounds().deadline;
    let tools = executor.definitions();
    let run = async {
        loop {
            if cancel.is_cancelled() {
                return TurnOutcome::Cancelled;
            }
            match turn.next() {
                Step::CallModel(messages) => {
                    let answer = model.respond(messages, tools).await;
                    turn.model_answered(answer);
                }
                Step::RunTool(call) => {
                    let outcome = executor.run(&call, cancel).await;
                    turn.tool_finished(outcome);
                }
                Step::RefuseTool { call, code } => {
                    turn.tool_refused(executor.refused(&call, code));
                }
                Step::Done(result) => return TurnOutcome::Done(result),
                Step::Failed {
                    code,
                    retryable,
                    activity,
                } => {
                    return TurnOutcome::Failed {
                        code,
                        retryable,
                        activity,
                    };
                }
            }
        }
    };
    let ended = tokio::select! {
        biased;
        () = cancel.cancelled() => Some(TurnOutcome::Cancelled),
        outcome = tokio::time::timeout(deadline, run) => outcome.ok(),
    };
    // `None` is the deadline; the run is dropped, so the turn can say what it did.
    ended.unwrap_or_else(|| turn.out_of_time())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::time::Duration;

    use super::*;

    enum Scripted {
        Answer(Result<Assistant, ModelFailure>),
        Hang,
    }

    /// Answers from a script and records every request it was given.
    struct FakeModel {
        script: Mutex<VecDeque<Scripted>>,
        requests: Mutex<Vec<(Vec<ChatMessage>, Vec<ToolDefinition>)>>,
        /// Answered when the script runs out.
        fallback: Option<Assistant>,
    }

    impl FakeModel {
        fn new(script: Vec<Scripted>) -> Self {
            Self {
                script: Mutex::new(script.into()),
                requests: Mutex::new(Vec::new()),
                fallback: None,
            }
        }

        fn repeating(answer: Assistant) -> Self {
            Self {
                fallback: Some(answer),
                ..Self::new(Vec::new())
            }
        }

        fn requests(&self) -> Vec<Vec<ChatMessage>> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .map(|(messages, _)| messages.clone())
                .collect()
        }
    }

    impl Model for FakeModel {
        async fn respond(
            &self,
            messages: &[ChatMessage],
            tools: &[ToolDefinition],
        ) -> Result<Assistant, ModelFailure> {
            self.requests
                .lock()
                .unwrap()
                .push((messages.to_vec(), tools.to_vec()));
            let next = self.script.lock().unwrap().pop_front();
            match next {
                Some(Scripted::Answer(answer)) => answer,
                Some(Scripted::Hang) => std::future::pending().await,
                None => Ok(self.fallback.clone().expect("script exhausted")),
            }
        }
    }

    /// Records calls in order and answers each from `outcome`.
    struct FakeTools {
        definitions: Vec<ToolDefinition>,
        calls: Mutex<Vec<ToolCall>>,
        outcome: Box<dyn Fn(&ToolCall) -> ToolOutcome + Send + Sync>,
        cancel_on_run: Option<CancellationToken>,
    }

    impl FakeTools {
        fn new(outcome: impl Fn(&ToolCall) -> ToolOutcome + Send + Sync + 'static) -> Self {
            Self {
                definitions: vec![definition("t__a"), definition("t__b")],
                calls: Mutex::new(Vec::new()),
                outcome: Box::new(outcome),
                cancel_on_run: None,
            }
        }

        fn names(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|call| call.name.clone())
                .collect()
        }
    }

    impl ToolExecutor for FakeTools {
        fn definitions(&self) -> &[ToolDefinition] {
            &self.definitions
        }

        async fn run(&self, call: &ToolCall, _cancel: &CancellationToken) -> ToolOutcome {
            self.calls.lock().unwrap().push(call.clone());
            if let Some(cancel) = &self.cancel_on_run {
                cancel.cancel();
            }
            (self.outcome)(call)
        }

        fn refused(&self, call: &ToolCall, code: &str) -> Option<Activity> {
            Some(activity(&call.name, false, Some(code)))
        }
    }

    fn definition(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            description: format!("{name} tool"),
            parameters: serde_json::json!({"type":"object"}),
        }
    }

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: "{}".into(),
        }
    }

    fn calls(calls: Vec<ToolCall>) -> Scripted {
        Scripted::Answer(Ok(Assistant {
            content: None,
            tool_calls: calls,
        }))
    }

    fn text(text: &str) -> Scripted {
        Scripted::Answer(Ok(Assistant {
            content: Some(text.into()),
            tool_calls: Vec::new(),
        }))
    }

    fn activity(port: &str, ok: bool, code: Option<&str>) -> Activity {
        Activity {
            package: "t".into(),
            port: port.into(),
            ok,
            code: code.map(Into::into),
        }
    }

    fn ok_result(content: &str) -> impl Fn(&ToolCall) -> ToolOutcome + Send + Sync + 'static {
        let content = content.to_owned();
        move |call| ToolOutcome {
            content: content.clone(),
            activity: Some(activity(&call.name, true, None)),
        }
    }

    fn turn(bounds: TurnBounds) -> Turn {
        Turn::new(
            "system".into(),
            vec![ChatMessage::user("question".into())],
            bounds,
        )
    }

    async fn run(model: &FakeModel, tools: &FakeTools, bounds: TurnBounds) -> TurnOutcome {
        drive(turn(bounds), model, tools, &CancellationToken::new()).await
    }

    #[test]
    fn production_bounds_are_the_planned_ones() {
        let bounds = TurnBounds::PRODUCTION;
        assert_eq!(bounds.model_calls, 8);
        assert_eq!(bounds.tool_calls, 16);
        assert_eq!(bounds.result_chars, 16_384);
        assert_eq!(bounds.message_bytes, 96 * 1024);
        assert_eq!(bounds.deadline, Duration::from_secs(180));
    }

    #[tokio::test]
    async fn tool_call_then_final_text() {
        let model = FakeModel::new(vec![calls(vec![call("c1", "t__a")]), text("done")]);
        let tools = FakeTools::new(ok_result(r#"{"items":[]}"#));
        let outcome = run(&model, &tools, TurnBounds::PRODUCTION).await;
        assert_eq!(
            outcome,
            TurnOutcome::Done(TurnResult {
                text: "done".into(),
                activity: vec![activity("t__a", true, None)],
            })
        );
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].1, tools.definitions);
        assert_eq!(
            requests[0].0,
            vec![
                ChatMessage::system("system".into()),
                ChatMessage::user("question".into()),
            ]
        );
        assert_eq!(
            requests[1].0[2..],
            [
                ChatMessage::assistant_tool_calls(None, vec![call("c1", "t__a")]),
                ChatMessage::tool("c1".into(), r#"{"items":[]}"#.into()),
            ]
        );
    }

    #[tokio::test]
    async fn calls_run_in_model_order() {
        let model = FakeModel::new(vec![
            calls(vec![
                call("c1", "t__b"),
                call("c2", "t__a"),
                call("c3", "t__b"),
            ]),
            text("done"),
        ]);
        let tools = FakeTools::new(|call: &ToolCall| ToolOutcome {
            content: format!("result of {}", call.id),
            activity: None,
        });
        let outcome = run(&model, &tools, TurnBounds::PRODUCTION).await;
        assert!(matches!(outcome, TurnOutcome::Done(_)));
        assert_eq!(tools.names(), ["t__b", "t__a", "t__b"]);
        let second = &model.requests()[1];
        assert_eq!(
            second[3..],
            [
                ChatMessage::tool("c1".into(), "result of c1".into()),
                ChatMessage::tool("c2".into(), "result of c2".into()),
                ChatMessage::tool("c3".into(), "result of c3".into()),
            ]
        );
    }

    #[tokio::test]
    async fn provider_error_result_reaches_next_model_request() {
        let refused = r#"{"error":"exhibits.type_refused","retryable":false}"#;
        let model = FakeModel::new(vec![
            calls(vec![call("c1", "t__a")]),
            text("I could not add that type."),
        ]);
        let tools = FakeTools::new(move |call: &ToolCall| ToolOutcome {
            content: refused.into(),
            activity: Some(activity(&call.name, false, Some("exhibits.type_refused"))),
        });
        let outcome = run(&model, &tools, TurnBounds::PRODUCTION).await;
        assert_eq!(
            outcome,
            TurnOutcome::Done(TurnResult {
                text: "I could not add that type.".into(),
                activity: vec![activity("t__a", false, Some("exhibits.type_refused"))],
            })
        );
        assert_eq!(
            model.requests()[1].last(),
            Some(&ChatMessage::tool("c1".into(), refused.into()))
        );
    }

    #[tokio::test]
    async fn model_failures_pass_through() {
        let failure = ModelFailure {
            code: crate::model::ENDPOINT_UNREACHABLE,
            retryable: true,
        };
        let model = FakeModel::new(vec![Scripted::Answer(Err(failure))]);
        let tools = FakeTools::new(ok_result("{}"));
        let outcome = run(&model, &tools, TurnBounds::PRODUCTION).await;
        assert_eq!(
            outcome,
            TurnOutcome::failed(crate::model::ENDPOINT_UNREACHABLE, true)
        );
    }

    #[tokio::test]
    async fn model_call_budget_exceeded() {
        let model = FakeModel::repeating(Assistant {
            content: None,
            tool_calls: vec![call("c", "t__a")],
        });
        let tools = FakeTools::new(ok_result("{}"));
        let bounds = TurnBounds {
            model_calls: 3,
            ..TurnBounds::PRODUCTION
        };
        let outcome = run(&model, &tools, bounds).await;
        // The third call's batch could never be read, so it does not run.
        assert_eq!(
            outcome,
            TurnOutcome::Failed {
                code: TURN_LIMIT_EXCEEDED,
                retryable: false,
                activity: vec![activity("t__a", true, None); 2],
            }
        );
        assert_eq!(model.requests().len(), 3);
        assert_eq!(tools.names().len(), 2);
    }

    #[tokio::test]
    async fn last_model_call_tool_batch_never_runs() {
        // The only model call asks for tools: none runs, and the turn ends at the limit.
        let model = FakeModel::new(vec![calls(vec![call("c1", "t__a"), call("c2", "t__b")])]);
        let tools = FakeTools::new(ok_result("{}"));
        let bounds = TurnBounds {
            model_calls: 1,
            ..TurnBounds::PRODUCTION
        };
        let outcome = run(&model, &tools, bounds).await;
        assert_eq!(outcome, TurnOutcome::failed(TURN_LIMIT_EXCEEDED, false));
        assert!(tools.names().is_empty());
        assert_eq!(model.requests().len(), 1);

        // Text beside the last batch does not finish the turn either; earlier calls are kept.
        let model = FakeModel::new(vec![
            calls(vec![call("c1", "t__a")]),
            Scripted::Answer(Ok(Assistant {
                content: Some("partial".into()),
                tool_calls: vec![call("c2", "t__b")],
            })),
        ]);
        let tools = FakeTools::new(ok_result("{}"));
        let bounds = TurnBounds {
            model_calls: 2,
            ..TurnBounds::PRODUCTION
        };
        let outcome = run(&model, &tools, bounds).await;
        assert_eq!(
            outcome,
            TurnOutcome::Failed {
                code: TURN_LIMIT_EXCEEDED,
                retryable: false,
                activity: vec![activity("t__a", true, None)],
            }
        );
        assert_eq!(tools.names(), ["t__a"]);
        assert_eq!(model.requests().len(), 2);
    }

    #[tokio::test]
    async fn tool_call_budget_exceeded() {
        // One call per round: the 17th call ends the turn before it runs.
        let model = FakeModel::repeating(Assistant {
            content: None,
            tool_calls: vec![call("c", "t__a")],
        });
        let tools = FakeTools::new(ok_result("{}"));
        let bounds = TurnBounds {
            model_calls: 100,
            ..TurnBounds::PRODUCTION
        };
        let outcome = run(&model, &tools, bounds).await;
        assert_eq!(
            outcome,
            TurnOutcome::Failed {
                code: TURN_LIMIT_EXCEEDED,
                retryable: false,
                activity: vec![activity("t__a", true, None); 16],
            }
        );
        assert_eq!(tools.names().len(), 16);

        // A batch that would cross the budget runs none of its calls.
        let model = FakeModel::new(vec![calls(vec![
            call("c1", "t__a"),
            call("c2", "t__a"),
            call("c3", "t__a"),
        ])]);
        let tools = FakeTools::new(ok_result("{}"));
        let bounds = TurnBounds {
            tool_calls: 2,
            ..TurnBounds::PRODUCTION
        };
        let outcome = run(&model, &tools, bounds).await;
        assert_eq!(outcome, TurnOutcome::failed(TURN_LIMIT_EXCEEDED, false));
        assert!(tools.names().is_empty());
    }

    #[tokio::test]
    async fn oversized_result_becomes_error_not_truncation() {
        let model = FakeModel::new(vec![
            calls(vec![call("c1", "t__a"), call("c2", "t__b")]),
            text("done"),
        ]);
        // `t__a` returns exactly the cap (in characters, not bytes); `t__b` one more.
        let tools = FakeTools::new(|call: &ToolCall| ToolOutcome {
            content: if call.name == "t__a" {
                "é".repeat(10)
            } else {
                "é".repeat(11)
            },
            activity: Some(activity(&call.name, true, None)),
        });
        let bounds = TurnBounds {
            result_chars: 10,
            ..TurnBounds::PRODUCTION
        };
        let outcome = run(&model, &tools, bounds).await;
        assert_eq!(
            outcome,
            TurnOutcome::Done(TurnResult {
                text: "done".into(),
                activity: vec![
                    activity("t__a", true, None),
                    activity("t__b", false, Some(TOOL_RESULT_TOO_LARGE)),
                ],
            })
        );
        assert_eq!(
            model.requests()[1][3..],
            [
                ChatMessage::tool("c1".into(), "é".repeat(10)),
                ChatMessage::tool(
                    "c2".into(),
                    r#"{"error":"agent.tool_result_too_large","retryable":false}"#.into()
                ),
            ]
        );
    }

    #[tokio::test]
    async fn message_bound_exceeded() {
        // A tool result pushes the turn's own messages over the bound before the next model
        // call; the call that ran is kept.
        let model = FakeModel::new(vec![calls(vec![call("c1", "t__a")]), text("never")]);
        let tools = FakeTools::new(ok_result(&"y".repeat(2000)));
        let bounds = TurnBounds {
            message_bytes: 1024,
            ..TurnBounds::PRODUCTION
        };
        let outcome = run(&model, &tools, bounds).await;
        assert_eq!(
            outcome,
            TurnOutcome::Failed {
                code: TURN_LIMIT_EXCEEDED,
                retryable: false,
                activity: vec![activity("t__a", true, None)],
            }
        );
        assert_eq!(model.requests().len(), 1);
        assert_eq!(tools.names().len(), 1);

        // Growth inside the bound is sent.
        let model = FakeModel::new(vec![calls(vec![call("c1", "t__a")]), text("done")]);
        let tools = FakeTools::new(ok_result(&"y".repeat(500)));
        let outcome = run(&model, &tools, bounds).await;
        assert!(matches!(outcome, TurnOutcome::Done(_)), "{outcome:?}");
        assert_eq!(model.requests().len(), 2);
    }

    fn call_with(id: &str, name: &str, arguments: String) -> ToolCall {
        ToolCall {
            arguments,
            ..call(id, name)
        }
    }

    fn refusal_of(message: &ChatMessage) -> serde_json::Value {
        let ChatMessage::Tool { content, .. } = message else {
            panic!("not a tool result: {message:?}");
        };
        serde_json::from_str(content).unwrap()
    }

    /// What `messages` adds to the turn's two starting messages, as the bound counts it.
    fn added_bytes(messages: &[ChatMessage]) -> usize {
        message_bytes(messages) - message_bytes(&messages[..2])
    }

    /// The `remainingBytes` a refusal of one oversized `t__a` call reports under `bounds`.
    async fn remaining_for_one_call(bounds: TurnBounds) -> usize {
        let model = FakeModel::new(vec![
            calls(vec![call_with("c1", "t__a", "x".repeat(1 << 20))]),
            text("done"),
        ]);
        let tools = FakeTools::new(ok_result("{}"));
        assert!(matches!(
            run(&model, &tools, bounds).await,
            TurnOutcome::Done(_)
        ));
        let remaining = refusal_of(&model.requests()[1][3])["remainingBytes"].clone();
        usize::try_from(remaining.as_u64().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn oversized_arguments_are_refused_and_the_turn_continues() {
        let bounds = TurnBounds {
            message_bytes: 4096,
            ..TurnBounds::PRODUCTION
        };
        let big = "x".repeat(5000);
        let model = FakeModel::new(vec![
            calls(vec![call_with("c1", "t__a", big.clone())]),
            text("That file is too large for one turn."),
        ]);
        let tools = FakeTools::new(ok_result("{}"));
        let outcome = run(&model, &tools, bounds).await;
        // Never dispatched, recorded as a failed call, and the model answers.
        assert!(tools.names().is_empty());
        assert_eq!(
            outcome,
            TurnOutcome::Done(TurnResult {
                text: "That file is too large for one turn.".into(),
                activity: vec![activity("t__a", false, Some(TOOL_ARGUMENTS_TOO_LARGE))],
            })
        );
        let refusal = refusal_of(&model.requests()[1][3]);
        assert_eq!(refusal["error"], TOOL_ARGUMENTS_TOO_LARGE);
        assert_eq!(refusal["retryable"], false);
        assert_eq!(refusal["argumentBytes"], 5000);
        assert!(refusal["remainingBytes"].as_u64().unwrap() < 4096);
        assert_eq!(
            refusal.as_object().unwrap().len(),
            4,
            "{refusal}: only the code, retryability and the two sizes"
        );
    }

    #[tokio::test]
    async fn refused_arguments_are_elided_so_the_next_request_fits() {
        let bounds = TurnBounds {
            message_bytes: 4096,
            ..TurnBounds::PRODUCTION
        };
        let model = FakeModel::new(vec![
            calls(vec![call_with("c1", "t__a", "x".repeat(50_000))]),
            calls(vec![call("c2", "t__b")]),
            text("done"),
        ]);
        let tools = FakeTools::new(ok_result("{}"));
        let outcome = run(&model, &tools, bounds).await;
        assert!(matches!(outcome, TurnOutcome::Done(_)), "{outcome:?}");
        let requests = model.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests[1][2],
            ChatMessage::assistant_tool_calls(
                None,
                vec![call_with("c1", "t__a", ELIDED_ARGUMENTS.into())]
            )
        );
        for request in &requests[1..] {
            assert!(added_bytes(request) <= bounds.message_bytes);
        }
        // A later call in the same turn still runs.
        assert_eq!(tools.names(), ["t__b"]);
    }

    #[tokio::test]
    async fn mixed_batch_runs_the_calls_that_fit() {
        let bounds = TurnBounds {
            message_bytes: 4096,
            ..TurnBounds::PRODUCTION
        };
        let small = r#"{"path":"index.html"}"#.to_owned();
        let model = FakeModel::new(vec![
            calls(vec![
                call_with("c1", "t__a", small.clone()),
                call_with("c2", "t__b", "x".repeat(8000)),
                call_with("c3", "t__a", small.clone()),
            ]),
            text("done"),
        ]);
        let tools = FakeTools::new(ok_result("{}"));
        let outcome = run(&model, &tools, bounds).await;
        assert_eq!(
            outcome,
            TurnOutcome::Done(TurnResult {
                text: "done".into(),
                activity: vec![
                    activity("t__a", true, None),
                    activity("t__b", false, Some(TOOL_ARGUMENTS_TOO_LARGE)),
                    activity("t__a", true, None),
                ],
            })
        );
        let ran = tools.calls.lock().unwrap().clone();
        assert_eq!(
            ran,
            [
                call_with("c1", "t__a", small.clone()),
                call_with("c3", "t__a", small)
            ]
        );
        let second = &model.requests()[1];
        assert_eq!(second[3], ChatMessage::tool("c1".into(), "{}".into()));
        assert_eq!(refusal_of(&second[4])["error"], TOOL_ARGUMENTS_TOO_LARGE);
        assert_eq!(second[5], ChatMessage::tool("c3".into(), "{}".into()));
    }

    #[tokio::test]
    async fn fitting_arguments_are_unaffected() {
        let arguments = format!(r#"{{"content":"{}"}}"#, "y".repeat(2000));
        let model = FakeModel::new(vec![
            calls(vec![call_with("c1", "t__a", arguments.clone())]),
            text("done"),
        ]);
        let tools = FakeTools::new(ok_result("{}"));
        let outcome = run(&model, &tools, TurnBounds::PRODUCTION).await;
        assert!(matches!(outcome, TurnOutcome::Done(_)), "{outcome:?}");
        assert_eq!(
            *tools.calls.lock().unwrap(),
            [call_with("c1", "t__a", arguments.clone())]
        );
        assert_eq!(
            model.requests()[1][2],
            ChatMessage::assistant_tool_calls(None, vec![call_with("c1", "t__a", arguments)])
        );
    }

    #[tokio::test]
    async fn argument_budget_follows_the_bounds() {
        let small = TurnBounds {
            message_bytes: 16_384,
            ..TurnBounds::PRODUCTION
        };
        let large = TurnBounds {
            message_bytes: 32_768,
            ..TurnBounds::PRODUCTION
        };
        let remaining = remaining_for_one_call(small).await;
        // The extra 16,384 bytes of bound less the extra 1/128 reserved for the call's result.
        assert_eq!(
            remaining_for_one_call(large).await,
            remaining + 16_384 - 16_384 / 128
        );

        // Exactly the remaining budget runs; one byte more is refused.
        let elided = json_string_bytes(ELIDED_ARGUMENTS);
        for (extra, runs) in [(0, true), (1, false)] {
            let arguments = "x".repeat(remaining + elided - 2 + extra);
            let model = FakeModel::new(vec![
                calls(vec![call_with("c1", "t__a", arguments)]),
                text("done"),
            ]);
            let tools = FakeTools::new(ok_result("{}"));
            let outcome = run(&model, &tools, small).await;
            assert!(matches!(outcome, TurnOutcome::Done(_)), "{outcome:?}");
            assert_eq!(tools.names().len(), usize::from(runs), "extra {extra}");
        }
    }

    #[tokio::test]
    async fn large_initial_history_is_not_bounded() {
        // The starting messages alone are far over the bound: the model is still asked, with
        // every one of them.
        let bounds = TurnBounds {
            message_bytes: 64,
            ..TurnBounds::PRODUCTION
        };
        let history = vec![
            ChatMessage::user("x".repeat(10_000)),
            ChatMessage::assistant("中".repeat(10_000)),
            ChatMessage::user("question".into()),
        ];
        let model = FakeModel::new(vec![text("answer")]);
        let tools = FakeTools::new(ok_result("{}"));
        let big = Turn::new("system".into(), history.clone(), bounds);
        let outcome = drive(big, &model, &tools, &CancellationToken::new()).await;
        assert_eq!(
            outcome,
            TurnOutcome::Done(TurnResult {
                text: "answer".into(),
                activity: Vec::new(),
            })
        );
        assert_eq!(model.requests()[0][1..], history[..]);

        // The same turn still bounds what its own tool exchange adds.
        let model = FakeModel::new(vec![calls(vec![call("c1", "t__a")]), text("never")]);
        let tools = FakeTools::new(ok_result(&"y".repeat(200)));
        let big = Turn::new("system".into(), history, bounds);
        let outcome = drive(big, &model, &tools, &CancellationToken::new()).await;
        assert!(
            matches!(
                outcome,
                TurnOutcome::Failed {
                    code: TURN_LIMIT_EXCEEDED,
                    ..
                }
            ),
            "{outcome:?}"
        );
        assert_eq!(model.requests().len(), 1);
    }

    #[tokio::test]
    async fn failed_turn_keeps_activity() {
        // A model failure after a tool call keeps that call's activity.
        let failure = ModelFailure {
            code: crate::model::ENDPOINT_UNREACHABLE,
            retryable: true,
        };
        let model = FakeModel::new(vec![
            calls(vec![call("c1", "t__a")]),
            Scripted::Answer(Err(failure)),
        ]);
        let tools = FakeTools::new(ok_result("{}"));
        let outcome = run(&model, &tools, TurnBounds::PRODUCTION).await;
        assert_eq!(
            outcome,
            TurnOutcome::Failed {
                code: crate::model::ENDPOINT_UNREACHABLE,
                retryable: true,
                activity: vec![activity("t__a", true, None)],
            }
        );

        // A batch that crosses the tool-call budget keeps the calls that already ran.
        let model = FakeModel::new(vec![
            calls(vec![call("c1", "t__a")]),
            calls(vec![call("c2", "t__b"), call("c3", "t__a")]),
        ]);
        let tools = FakeTools::new(ok_result("{}"));
        let bounds = TurnBounds {
            tool_calls: 2,
            ..TurnBounds::PRODUCTION
        };
        let outcome = run(&model, &tools, bounds).await;
        assert_eq!(
            outcome,
            TurnOutcome::Failed {
                code: TURN_LIMIT_EXCEEDED,
                retryable: false,
                activity: vec![activity("t__a", true, None)],
            }
        );

        // The step machine reports the same activity, and repeats it if asked again.
        let mut turn = turn(TurnBounds::PRODUCTION);
        assert!(matches!(turn.next(), Step::CallModel(_)));
        turn.model_answered(Ok(Assistant {
            content: None,
            tool_calls: vec![call("c1", "t__b")],
        }));
        assert_eq!(turn.next(), Step::RunTool(call("c1", "t__b")));
        turn.tool_finished(ToolOutcome {
            content: "{}".into(),
            activity: Some(activity("t__b", false, Some("t.refused"))),
        });
        assert!(matches!(turn.next(), Step::CallModel(_)));
        turn.model_answered(Err(failure));
        let failed = Step::Failed {
            code: crate::model::ENDPOINT_UNREACHABLE,
            retryable: true,
            activity: vec![activity("t__b", false, Some("t.refused"))],
        };
        assert_eq!(turn.next(), failed);
        assert_eq!(turn.next(), failed);
    }

    #[tokio::test]
    async fn deadline_exceeded_with_short_bound() {
        let model = FakeModel::new(vec![calls(vec![call("c1", "t__a")]), Scripted::Hang]);
        let tools = FakeTools::new(ok_result("{}"));
        let bounds = TurnBounds {
            deadline: Duration::from_millis(50),
            ..TurnBounds::PRODUCTION
        };
        let outcome = tokio::time::timeout(Duration::from_secs(5), run(&model, &tools, bounds))
            .await
            .expect("the turn deadline did not end the turn");
        // The call that ran before the deadline is kept.
        assert_eq!(
            outcome,
            TurnOutcome::Failed {
                code: TURN_LIMIT_EXCEEDED,
                retryable: false,
                activity: vec![activity("t__a", true, None)],
            }
        );
        assert_eq!(model.requests().len(), 2);
    }

    #[tokio::test]
    async fn cancellation_stops_without_further_calls() {
        // Cancelled while a tool runs: the model is not asked again.
        let cancel = CancellationToken::new();
        let model = FakeModel::new(vec![calls(vec![call("c1", "t__a"), call("c2", "t__b")])]);
        let mut tools = FakeTools::new(ok_result("{}"));
        tools.cancel_on_run = Some(cancel.clone());
        let outcome = drive(turn(TurnBounds::PRODUCTION), &model, &tools, &cancel).await;
        assert_eq!(outcome, TurnOutcome::Cancelled);
        assert_eq!(model.requests().len(), 1);
        assert_eq!(tools.names(), ["t__a"]);

        // Cancelled while the model is answering.
        let cancel = CancellationToken::new();
        let model = FakeModel::new(vec![Scripted::Hang]);
        let tools = FakeTools::new(ok_result("{}"));
        let cancelling = cancel.clone();
        tokio::spawn(async move {
            // simulated-delay: lets the model call start before cancelling it.
            tokio::time::sleep(Duration::from_millis(20)).await;
            cancelling.cancel();
        });
        let outcome = drive(turn(TurnBounds::PRODUCTION), &model, &tools, &cancel).await;
        assert_eq!(outcome, TurnOutcome::Cancelled);
        assert!(tools.names().is_empty());

        // Cancelled before the turn starts: nothing is called.
        let cancel = CancellationToken::new();
        cancel.cancel();
        let model = FakeModel::new(vec![text("never")]);
        let outcome = drive(turn(TurnBounds::PRODUCTION), &model, &tools, &cancel).await;
        assert_eq!(outcome, TurnOutcome::Cancelled);
        assert!(model.requests().is_empty());
    }
}
