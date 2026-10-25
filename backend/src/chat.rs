//! The Steward's direct chat with the configured model, outside every Workspace Channel.
//!
//! One transcript per activation, held in memory and never written to the Package Data
//! Directory. One turn is in flight at a time. `agent.chat.send` answers as soon as the turn
//! starts, because a completion may outlast the Host's UI request deadline; the reply reaches
//! the UI through `agent.chat.changes`. Every turn carries a generation, so a completion that
//! finishes after a clear or an interruption is dropped instead of appended.

use std::collections::VecDeque;
use std::sync::Mutex;

use serde::Serialize;
use tokio::sync::watch;

use crate::model::{ChatMessage, ModelFailure};
use crate::status::StatusError;

/// The contract bound on one message the Steward sends.
pub const MAX_INPUT_CHARS: usize = 8192;
/// The contract bound on one stored entry; longer replies are truncated.
pub const MAX_ENTRY_CHARS: usize = 16_384;
/// The contract bound on how many entries one view carries.
pub const MAX_ENTRIES: usize = 64;
/// Keeps every view well inside one protocol frame, whatever the content escapes to.
pub const MAX_TRANSCRIPT_CHARS: usize = 65_536;

/// A turn is already in flight; sending again after it settles may succeed.
pub const CHAT_BUSY: &str = "agent.chat_busy";
/// The endpoint, model or key is missing.
pub const NOT_CONFIGURED: &str = "agent.not_configured";
/// The message is empty after trimming or carries control characters.
pub const INVALID_MESSAGE: &str = "agent.invalid_message";
/// The turn was cancelled by suspend or dispose before the model answered.
pub const CHAT_INTERRUPTED: &str = "agent.chat_interrupted";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatRole {
    User,
    Assistant,
}

/// The exact UI-contract `chatEntry` shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChatEntry {
    pub id: u64,
    pub role: ChatRole,
    pub content: String,
}

/// The exact UI-contract `chat` shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatView {
    pub entries: Vec<ChatEntry>,
    pub pending: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<StatusError>,
}

/// What a started turn sends to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub generation: u64,
    pub messages: Vec<ChatMessage>,
}

/// The transcript and its single in-flight turn. Pure; [`ChatCell`] publishes it.
#[derive(Debug, Default)]
pub struct ChatSession {
    entries: VecDeque<ChatEntry>,
    next_id: u64,
    pending: bool,
    last_error: Option<StatusError>,
    generation: u64,
}

impl ChatSession {
    #[must_use]
    pub fn view(&self) -> ChatView {
        ChatView {
            entries: self.entries.iter().cloned().collect(),
            pending: self.pending,
            last_error: self.last_error.clone(),
        }
    }

    /// Records the Steward's message and returns what the model should see: the system message,
    /// up to `context` earlier entries, then this message.
    ///
    /// # Errors
    ///
    /// Returns [`INVALID_MESSAGE`] or [`CHAT_BUSY`]; the transcript is unchanged.
    pub fn begin(
        &mut self,
        content: &str,
        system: String,
        context: usize,
    ) -> Result<Turn, &'static str> {
        let content = content.trim();
        if content.is_empty()
            || content.chars().count() > MAX_INPUT_CHARS
            || content
                .chars()
                .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
        {
            return Err(INVALID_MESSAGE);
        }
        if self.pending {
            return Err(CHAT_BUSY);
        }
        self.push(ChatRole::User, content.to_owned());
        self.pending = true;
        self.last_error = None;
        self.generation += 1;
        let start = self.entries.len().saturating_sub(context.saturating_add(1));
        let messages = std::iter::once(ChatMessage::system(system))
            .chain(
                self.entries
                    .iter()
                    .skip(start)
                    .map(|entry| match entry.role {
                        ChatRole::User => ChatMessage::user(entry.content.clone()),
                        ChatRole::Assistant => ChatMessage::assistant(entry.content.clone()),
                    }),
            )
            .collect();
        Ok(Turn {
            generation: self.generation,
            messages,
        })
    }

    /// Settles the turn `generation` started. Returns whether it was still the current turn.
    pub fn finish(&mut self, generation: u64, outcome: Result<String, ModelFailure>) -> bool {
        if !self.pending || generation != self.generation {
            return false;
        }
        self.pending = false;
        match outcome {
            Ok(text) => {
                let text = truncate_chars(text.trim(), MAX_ENTRY_CHARS);
                if text.is_empty() {
                    self.fail(crate::model::MODEL_ERROR, false);
                } else {
                    self.push(ChatRole::Assistant, text);
                }
            }
            Err(failure) => self.fail(failure.code, failure.retryable),
        }
        true
    }

    /// Ends a turn that will never settle; the Steward may send again.
    pub fn interrupt(&mut self) {
        if self.pending {
            self.pending = false;
            self.generation += 1;
            self.fail(CHAT_INTERRUPTED, true);
        }
    }

    /// Forgets the transcript and any turn in flight.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.pending = false;
        self.last_error = None;
        self.generation += 1;
    }

    fn fail(&mut self, code: &str, retryable: bool) {
        self.last_error = Some(StatusError {
            code: code.to_owned(),
            retryable,
        });
    }

    fn push(&mut self, role: ChatRole, content: String) {
        self.next_id += 1;
        self.entries.push_back(ChatEntry {
            id: self.next_id,
            role,
            content,
        });
        let mut total: usize = self
            .entries
            .iter()
            .map(|entry| entry.content.chars().count())
            .sum();
        while self.entries.len() > MAX_ENTRIES || total > MAX_TRANSCRIPT_CHARS {
            let Some(oldest) = self.entries.pop_front() else {
                break;
            };
            total -= oldest.content.chars().count();
        }
    }
}

/// The system message for a direct chat, named after the bound Agent when there is one.
#[must_use]
pub fn system_message(agent_name: Option<&str>, system_prompt: &str) -> String {
    let name = agent_name.unwrap_or("the Agent");
    let mut text = format!(
        "You are {name}, an Agent in a team Workspace. Your Steward is chatting with you \
         directly, outside any Channel. Answer with the reply text only, in Markdown."
    );
    let prompt = system_prompt.trim();
    if !prompt.is_empty() {
        text.push_str("\n\n");
        text.push_str(prompt);
    }
    text
}

fn truncate_chars(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((index, _)) => text[..index].to_owned(),
        None => text.to_owned(),
    }
}

/// The session of one activation, observable through a watch channel.
#[derive(Debug)]
pub struct ChatCell {
    session: Mutex<ChatSession>,
    sender: watch::Sender<ChatView>,
}

impl Default for ChatCell {
    fn default() -> Self {
        let session = ChatSession::default();
        let (sender, _) = watch::channel(session.view());
        Self {
            session: Mutex::new(session),
            sender,
        }
    }
}

impl ChatCell {
    #[must_use]
    pub fn current(&self) -> ChatView {
        self.lock().view()
    }

    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<ChatView> {
        self.sender.subscribe()
    }

    /// Changes the session and publishes the result under the same lock, in mutation order.
    pub fn update<T>(&self, change: impl FnOnce(&mut ChatSession) -> T) -> (T, ChatView) {
        let mut session = self.lock();
        let result = change(&mut session);
        let view = session.view();
        self.sender.send_if_modified(|current| {
            if *current == view {
                false
            } else {
                *current = view.clone();
                true
            }
        });
        (result, view)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ChatSession> {
        self.session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn begin(session: &mut ChatSession, content: &str, context: usize) -> Turn {
        session.begin(content, "system".into(), context).unwrap()
    }

    #[test]
    fn a_turn_sees_the_system_message_bounded_context_and_the_new_message() {
        let mut session = ChatSession::default();
        let first = begin(&mut session, "  one  ", 20);
        assert_eq!(
            first.messages,
            vec![
                ChatMessage::system("system".into()),
                ChatMessage::user("one".into())
            ]
        );
        assert!(session.finish(first.generation, Ok(" first reply ".into())));
        let second = begin(&mut session, "two", 20);
        assert_eq!(
            second.messages,
            vec![
                ChatMessage::system("system".into()),
                ChatMessage::user("one".into()),
                ChatMessage::assistant("first reply".into()),
                ChatMessage::user("two".into()),
            ]
        );
        session.finish(second.generation, Ok("second".into()));

        // A smaller context sees fewer earlier entries; zero sees only the new message.
        let narrow = begin(&mut session, "three", 1);
        assert_eq!(
            narrow.messages,
            vec![
                ChatMessage::system("system".into()),
                ChatMessage::assistant("second".into()),
                ChatMessage::user("three".into()),
            ]
        );
        session.finish(narrow.generation, Ok("third".into()));
        let none = begin(&mut session, "four", 0);
        assert_eq!(
            none.messages,
            vec![
                ChatMessage::system("system".into()),
                ChatMessage::user("four".into())
            ]
        );
        let view = session.view();
        assert!(view.pending);
        let ids: Vec<u64> = view.entries.iter().map(|entry| entry.id).collect();
        assert_eq!(ids, vec![1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn invalid_or_overlapping_sends_leave_the_transcript_unchanged() {
        let mut session = ChatSession::default();
        assert_eq!(session.begin("   ", String::new(), 4), Err(INVALID_MESSAGE));
        assert_eq!(
            session.begin("bell\u{7}", String::new(), 4),
            Err(INVALID_MESSAGE)
        );
        let too_long = "x".repeat(MAX_INPUT_CHARS + 1);
        assert_eq!(
            session.begin(&too_long, String::new(), 4),
            Err(INVALID_MESSAGE)
        );
        assert!(session.view().entries.is_empty());
        assert!(
            session
                .begin(&"x".repeat(MAX_INPUT_CHARS), String::new(), 4)
                .is_ok()
        );
        let before = session.view();
        assert_eq!(session.begin("again", String::new(), 4), Err(CHAT_BUSY));
        assert_eq!(session.view(), before);
        // Newlines and tabs are ordinary text.
        session.clear();
        assert!(
            session
                .begin("line one\n\tline two", String::new(), 4)
                .is_ok()
        );
    }

    #[test]
    fn failures_are_recorded_and_the_next_send_clears_them() {
        let mut session = ChatSession::default();
        let turn = begin(&mut session, "hi", 4);
        let failure = ModelFailure {
            code: crate::model::ENDPOINT_UNREACHABLE,
            retryable: true,
        };
        assert!(session.finish(turn.generation, Err(failure)));
        let view = session.view();
        assert!(!view.pending);
        assert_eq!(view.entries.len(), 1);
        assert_eq!(
            view.last_error,
            Some(StatusError {
                code: crate::model::ENDPOINT_UNREACHABLE.into(),
                retryable: true
            })
        );
        let retry = begin(&mut session, "hi again", 4);
        assert_eq!(session.view().last_error, None);
        assert!(session.finish(retry.generation, Ok("   ".into())));
        assert_eq!(
            session.view().last_error.unwrap().code,
            crate::model::MODEL_ERROR
        );
    }

    #[test]
    fn a_completion_after_clear_or_interrupt_is_dropped() {
        let mut session = ChatSession::default();
        let cleared = begin(&mut session, "one", 4);
        session.clear();
        assert!(!session.finish(cleared.generation, Ok("late".into())));
        assert_eq!(session.view(), ChatSession::default().view());

        let interrupted = begin(&mut session, "two", 4);
        session.interrupt();
        let view = session.view();
        assert!(!view.pending);
        assert_eq!(view.last_error.unwrap().code, CHAT_INTERRUPTED);
        assert!(!session.finish(interrupted.generation, Ok("late".into())));
        assert_eq!(session.view().entries.len(), 1);

        // A new turn after an interruption settles normally.
        let next = begin(&mut session, "three", 4);
        assert!(session.finish(next.generation, Ok("answer".into())));
        assert_eq!(session.view().entries.last().unwrap().content, "answer");
    }

    #[test]
    fn the_transcript_stays_inside_its_count_and_size_bounds() {
        let mut session = ChatSession::default();
        for index in 0..MAX_ENTRIES {
            let turn = begin(&mut session, &format!("message {index}"), 0);
            session.finish(turn.generation, Ok(format!("reply {index}")));
        }
        let view = session.view();
        assert_eq!(view.entries.len(), MAX_ENTRIES);
        assert_eq!(
            view.entries.last().unwrap().content,
            format!("reply {}", MAX_ENTRIES - 1)
        );
        assert_eq!(
            view.entries.first().unwrap().content,
            format!("message {}", MAX_ENTRIES / 2)
        );

        let mut session = ChatSession::default();
        for _ in 0..8 {
            let turn = begin(&mut session, &"u".repeat(MAX_INPUT_CHARS), 0);
            session.finish(turn.generation, Ok("r".repeat(MAX_ENTRY_CHARS + 10)));
        }
        let view = session.view();
        let total: usize = view
            .entries
            .iter()
            .map(|entry| entry.content.chars().count())
            .sum();
        assert!(total <= MAX_TRANSCRIPT_CHARS);
        assert!(
            view.entries
                .iter()
                .all(|entry| entry.content.chars().count() <= MAX_ENTRY_CHARS)
        );
        assert_eq!(view.entries.last().unwrap().role, ChatRole::Assistant);
    }

    #[test]
    fn the_system_message_names_the_agent_and_appends_the_prompt() {
        assert_eq!(
            system_message(Some("Scout"), "  Be brief.  "),
            "You are Scout, an Agent in a team Workspace. Your Steward is chatting with you \
             directly, outside any Channel. Answer with the reply text only, in Markdown.\n\nBe brief."
        );
        assert!(system_message(None, "").starts_with("You are the Agent,"));
        assert!(!system_message(None, " ").contains("\n\n"));
    }

    #[test]
    fn the_cell_publishes_each_change_once() {
        let cell = ChatCell::default();
        let mut receiver = cell.subscribe();
        receiver.borrow_and_update();
        let (turn, view) = cell.update(|session| session.begin("hi", String::new(), 0));
        assert!(turn.is_ok());
        assert!(view.pending);
        assert!(receiver.has_changed().unwrap());
        assert_eq!(*receiver.borrow_and_update(), view);
        let _ = cell.update(|session| session.begin("again", String::new(), 0));
        assert!(!receiver.has_changed().unwrap());
    }
}
