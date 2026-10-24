use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde_json::Value;

use crate::{
    Frame, FrameKind, INITIAL_STREAM_CREDIT, MAX_ID_BYTES, MAX_IN_FLIGHT_EACH_DIRECTION,
    ProtocolError,
};

/// Tombstone entries kept per originating direction.
///
/// The contract grants 32 in-flight requests and 32 events of credit in each
/// direction, so two windows is the most a conforming peer can have in flight
/// while this side retires the state they name. An ID evicted from the queue
/// falls back to "never seen" and is fatal again.
pub const TOMBSTONE_CAPACITY: usize = 2 * MAX_IN_FLIGHT_EACH_DIRECTION;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    HostToPackage,
    PackageToHost,
}

impl Direction {
    fn opposite(self) -> Self {
        match self {
            Self::HostToPackage => Self::PackageToHost,
            Self::PackageToHost => Self::HostToPackage,
        }
    }

    fn id_prefix(self) -> &'static str {
        match self {
            Self::HostToPackage => "h:",
            Self::PackageToHost => "p:",
        }
    }
}

/// What a receiver does with one observed frame.
///
/// The contract is a distributed state machine, so the two peers' views of it
/// legitimately differ for one round trip. A frame a conforming peer could have
/// sent before it saw this side retire the state it names is not a violation:
/// it is ignored. Anything else is either delivered or fatal.
///
/// The verdict is about *delivery*, so only a reader acts on `Ignore`. A writer
/// observes its own frames for the state they carry and for the one question it
/// asks — is this frame out of contract? — and then writes it. Withholding a
/// frame the adapter asked for would break the contract's own guarantee that
/// exactly one `response` or `error` is terminal for a request, and the peer
/// runs this same classifier over what arrives.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Verdict {
    /// The frame names live state and reaches the adapter.
    Deliver,
    /// The frame is in contract but names state this side has already retired.
    Ignore,
}

/// How many frames each tombstone class has absorbed.
///
/// A peer that is genuinely spraying late frames is visible here rather than
/// silent, which is the cost of tolerating the race at all.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IgnoredFrames {
    /// Terminal frames for a request this side had cancelled or abandoned.
    pub late_terminals: u64,
    /// `cancel` frames naming a request that was already retired.
    pub late_cancels: u64,
    /// `event`, `credit` or `stream_close` frames for a retired stream.
    pub late_stream_frames: u64,
}

/// A bounded FIFO set of IDs.
#[derive(Debug, Default)]
struct BoundedIds {
    order: VecDeque<String>,
    ids: BTreeSet<String>,
}

impl BoundedIds {
    fn insert(&mut self, id: &str) {
        if !self.ids.insert(id.to_owned()) {
            return;
        }
        self.order.push_back(id.to_owned());
        while self.order.len() > TOMBSTONE_CAPACITY {
            if let Some(evicted) = self.order.pop_front() {
                self.ids.remove(&evicted);
            }
        }
    }

    fn contains(&self, id: &str) -> bool {
        self.ids.contains(id)
    }

    fn remove(&mut self, id: &str) -> bool {
        if !self.ids.remove(id) {
            return false;
        }
        self.order.retain(|value| value != id);
        true
    }
}

/// One bounded set per originating direction.
///
/// IDs are namespaced by their origin, so one busy peer cannot evict the
/// other's tombstones and make its own late frames fatal.
#[derive(Debug, Default)]
struct Tombstones {
    host: BoundedIds,
    package: BoundedIds,
}

impl Tombstones {
    fn side(&mut self, id: &str) -> &mut BoundedIds {
        if id.starts_with(Direction::HostToPackage.id_prefix()) {
            &mut self.host
        } else {
            &mut self.package
        }
    }

    fn insert(&mut self, id: &str) {
        self.side(id).insert(id);
    }

    fn contains(&self, id: &str) -> bool {
        if id.starts_with(Direction::HostToPackage.id_prefix()) {
            self.host.contains(id)
        } else {
            self.package.contains(id)
        }
    }

    fn remove(&mut self, id: &str) -> bool {
        self.side(id).remove(id)
    }
}

#[derive(Debug)]
struct StreamState {
    sender: Direction,
    credit: usize,
    last_sequence: u64,
    /// Whether this side has stopped serving the stream but has not yet
    /// observed the `stream_close` that ends it.
    retired: bool,
}

/// The one classifier both peers run over every frame, in both directions.
///
/// Tolerance for a lifecycle race lives here and nowhere else: an adapter that
/// kept its own escape hatch would let the two sides drift, which is how the
/// same race came to be fatal on one side and silent on the other.
#[derive(Debug, Default)]
pub struct ContractState {
    pending: BTreeMap<String, Direction>,
    /// IDs that received their one terminal frame. A second is fatal.
    answered: Tombstones,
    /// IDs the receiver stopped waiting for. A late first terminal is ignored
    /// and moves the ID to `answered`, so a second is still fatal.
    abandoned: Tombstones,
    /// Streams whose `stream_close` was observed in either direction.
    closed_streams: Tombstones,
    streams: BTreeMap<String, StreamState>,
    ignored: IgnoredFrames,
}

impl ContractState {
    pub fn observe(
        &mut self,
        direction: Direction,
        frame: &Frame,
    ) -> Result<Verdict, ProtocolError> {
        match frame.kind() {
            FrameKind::Initialize | FrameKind::Initialized => {
                self.observe_version(frame).map(|()| Verdict::Deliver)
            }
            FrameKind::Request => self
                .observe_request(direction, frame)
                .map(|()| Verdict::Deliver),
            FrameKind::Response | FrameKind::Error => self.observe_terminal(direction, frame),
            FrameKind::Cancel => self.observe_cancel(direction, frame),
            FrameKind::StreamOpen => self
                .observe_stream_open(direction, frame)
                .map(|()| Verdict::Deliver),
            FrameKind::Event => self.observe_event(direction, frame),
            FrameKind::Credit => self.observe_credit(direction, frame),
            FrameKind::StreamClose => self.observe_stream_close(frame),
            FrameKind::Ping | FrameKind::Pong | FrameKind::Shutdown | FrameKind::ShutdownAck => {
                Ok(Verdict::Deliver)
            }
        }
    }

    /// Reports how many frames each tombstone class has absorbed.
    #[must_use]
    pub const fn ignored(&self) -> IgnoredFrames {
        self.ignored
    }

    /// Retires a request this side has stopped waiting for.
    ///
    /// A late first terminal frame naming it is then ignored, and a second is
    /// fatal. A peer enters the tombstone at the moment it stops being willing
    /// to serve the request, not when some later grace period expires.
    pub fn retire_request(&mut self, id: &str) {
        if self.answered.contains(id) {
            return;
        }
        self.pending.remove(id);
        self.abandoned.insert(id);
    }

    /// Retires a stream this side has stopped serving.
    ///
    /// `event` and `credit` frames naming it are ignored from now on, because
    /// the peer released them before it could see the close. The one
    /// `stream_close` that ends the stream is still delivered.
    pub fn retire_stream(&mut self, id: &str) {
        if let Some(stream) = self.streams.get_mut(id) {
            stream.retired = true;
        }
    }

    fn observe_version(&self, frame: &Frame) -> Result<(), ProtocolError> {
        let major = frame
            .field("protocol")
            .and_then(Value::as_object)
            .and_then(|value| value.get("major"))
            .and_then(Value::as_u64)
            .ok_or_else(|| ProtocolError::new("malformed_frame"))?;
        if major != 1 {
            return Err(ProtocolError::new("incompatible_version"));
        }
        Ok(())
    }

    fn observe_request(
        &mut self,
        direction: Direction,
        frame: &Frame,
    ) -> Result<(), ProtocolError> {
        let id = id(frame, "id")?;
        validate_origin(id, direction)?;
        if self.pending.contains_key(id)
            || self.answered.contains(id)
            || self.abandoned.contains(id)
        {
            return Err(ProtocolError::new("duplicate_id"));
        }
        let in_flight = self
            .pending
            .values()
            .filter(|pending_direction| **pending_direction == direction)
            .count();
        if in_flight >= MAX_IN_FLIGHT_EACH_DIRECTION {
            return Err(ProtocolError::new("overloaded"));
        }
        self.pending.insert(id.to_owned(), direction);
        Ok(())
    }

    fn observe_terminal(
        &mut self,
        direction: Direction,
        frame: &Frame,
    ) -> Result<Verdict, ProtocolError> {
        let id = id(frame, "id")?;
        if self.answered.contains(id) {
            return Err(ProtocolError::new("second_terminal_response"));
        }
        // The contract's own sentence: a late first terminal frame for a
        // cancelled or timed-out ID is ignored without accepting a second one.
        if self.abandoned.remove(id) {
            self.answered.insert(id);
            self.ignored.late_terminals = self.ignored.late_terminals.saturating_add(1);
            return Ok(Verdict::Ignore);
        }
        let request_direction = self
            .pending
            .get(id)
            .copied()
            .ok_or_else(|| ProtocolError::new("unknown_response"))?;
        if request_direction == direction {
            return Err(ProtocolError::new("unknown_response"));
        }
        self.pending.remove(id);
        self.answered.insert(id);
        Ok(Verdict::Deliver)
    }

    fn observe_cancel(
        &mut self,
        direction: Direction,
        frame: &Frame,
    ) -> Result<Verdict, ProtocolError> {
        let control_id = id(frame, "id")?;
        validate_origin(control_id, direction)?;
        let target = id(frame, "targetId")?;
        // A cancel that crossed the answer, or a second cancel for the same
        // request, names state the sender had not yet seen retired.
        if self.answered.contains(target) || self.abandoned.contains(target) {
            self.ignored.late_cancels = self.ignored.late_cancels.saturating_add(1);
            return Ok(Verdict::Ignore);
        }
        if self.pending.get(target) != Some(&direction) {
            return Err(ProtocolError::new("unknown_request"));
        }
        // The canceller stops being willing to serve the request here, so a
        // late first terminal for it is ignored from now on.
        self.pending.remove(target);
        self.abandoned.insert(target);
        Ok(Verdict::Deliver)
    }

    fn observe_stream_open(
        &mut self,
        direction: Direction,
        frame: &Frame,
    ) -> Result<(), ProtocolError> {
        let id = id(frame, "id")?;
        validate_origin(id, direction)?;
        if self.streams.contains_key(id) || self.closed_streams.contains(id) {
            return Err(ProtocolError::new("duplicate_id"));
        }
        let credit = frame
            .field("initialCredit")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| ProtocolError::new("malformed_frame"))?;
        if credit != INITIAL_STREAM_CREDIT {
            return Err(ProtocolError::new("credit_violation"));
        }
        self.streams.insert(
            id.to_owned(),
            StreamState {
                sender: direction.opposite(),
                credit,
                last_sequence: 0,
                retired: false,
            },
        );
        Ok(())
    }

    fn observe_event(
        &mut self,
        direction: Direction,
        frame: &Frame,
    ) -> Result<Verdict, ProtocolError> {
        let stream_id = id(frame, "streamId")?;
        if self.closed_streams.contains(stream_id) {
            return Ok(self.ignore_stream_frame());
        }
        let stream = self
            .streams
            .get_mut(stream_id)
            .ok_or_else(|| ProtocolError::new("unknown_stream"))?;
        if stream.sender != direction {
            return Err(ProtocolError::new("unknown_stream"));
        }
        if stream.retired {
            return Ok(self.ignore_stream_frame());
        }
        if stream.credit == 0 {
            return Err(ProtocolError::new("credit_violation"));
        }
        let sequence = decimal(frame, "sequence")?;
        if sequence != stream.last_sequence.saturating_add(1) {
            return Err(ProtocolError::new("event_sequence"));
        }
        stream.credit -= 1;
        stream.last_sequence = sequence;
        Ok(Verdict::Deliver)
    }

    fn observe_credit(
        &mut self,
        direction: Direction,
        frame: &Frame,
    ) -> Result<Verdict, ProtocolError> {
        let stream_id = id(frame, "streamId")?;
        let amount = frame
            .field("amount")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| (1..=INITIAL_STREAM_CREDIT).contains(value))
            .ok_or_else(|| ProtocolError::new("credit_violation"))?;
        // Credit released for events the receiver had already accepted is
        // ordinary in-flight traffic, even when the stream has since closed.
        if self.closed_streams.contains(stream_id) {
            return Ok(self.ignore_stream_frame());
        }
        let stream = self
            .streams
            .get_mut(stream_id)
            .ok_or_else(|| ProtocolError::new("unknown_stream"))?;
        if stream.sender == direction {
            return Err(ProtocolError::new("unknown_stream"));
        }
        if stream.retired {
            return Ok(self.ignore_stream_frame());
        }
        // The window is a ceiling, not a starting balance. `credit` pays for
        // events the receiving adapter has already accepted, so a grant that
        // would carry the stream past the window it was opened with pays for
        // work that never happened, and the events it would admit are events
        // beyond the window.
        if stream.credit.saturating_add(amount) > INITIAL_STREAM_CREDIT {
            return Err(ProtocolError::new("credit_violation"));
        }
        stream.credit += amount;
        Ok(Verdict::Deliver)
    }

    fn observe_stream_close(&mut self, frame: &Frame) -> Result<Verdict, ProtocolError> {
        let stream_id = id(frame, "streamId")?;
        if self.streams.remove(stream_id).is_some() {
            self.closed_streams.insert(stream_id);
            return Ok(Verdict::Deliver);
        }
        if self.closed_streams.contains(stream_id) {
            return Ok(self.ignore_stream_frame());
        }
        Err(ProtocolError::new("unknown_stream"))
    }

    fn ignore_stream_frame(&mut self) -> Verdict {
        self.ignored.late_stream_frames = self.ignored.late_stream_frames.saturating_add(1);
        Verdict::Ignore
    }
}

fn id<'a>(frame: &'a Frame, field: &str) -> Result<&'a str, ProtocolError> {
    frame
        .field(field)
        .and_then(Value::as_str)
        .filter(|value| value.len() <= MAX_ID_BYTES)
        .ok_or_else(|| ProtocolError::new("malformed_frame"))
}

fn validate_origin(id: &str, direction: Direction) -> Result<(), ProtocolError> {
    if !id.starts_with(direction.id_prefix()) || id.len() <= 2 {
        return Err(ProtocolError::new("invalid_id_namespace"));
    }
    if !id[2..]
        .bytes()
        .all(|value| value.is_ascii_alphanumeric() || b"._~-".contains(&value))
    {
        return Err(ProtocolError::new("invalid_id_namespace"));
    }
    Ok(())
}

fn decimal(frame: &Frame, field: &str) -> Result<u64, ProtocolError> {
    let text = frame
        .field(field)
        .and_then(Value::as_str)
        .ok_or_else(|| ProtocolError::new("invalid_decimal"))?;
    if text != "0" && text.starts_with('0') || !text.bytes().all(|value| value.is_ascii_digit()) {
        return Err(ProtocolError::new("invalid_decimal"));
    }
    text.parse()
        .map_err(|_| ProtocolError::new("invalid_decimal"))
}

#[cfg(test)]
mod tests {
    use super::{ContractState, Direction, TOMBSTONE_CAPACITY, Verdict};
    use crate::Frame;
    use serde_json::{Value, json};

    fn frame(value: Value) -> Frame {
        Frame::try_from(value).expect("valid frame")
    }

    fn request(id: &str) -> Frame {
        frame(json!({
            "kind":"request", "id":id, "operation":"ui.read",
            "deadlineUnixMs":"1", "payload":{}
        }))
    }

    fn response(id: &str) -> Frame {
        frame(json!({"kind":"response", "id":id, "payload":{}}))
    }

    fn error(id: &str) -> Frame {
        frame(json!({"kind":"error", "id":id, "code":"fixture.failed", "retryable":false}))
    }

    fn cancel(target: &str) -> Frame {
        frame(json!({"kind":"cancel", "id":"h:cancel-1", "targetId":target}))
    }

    fn stream_open(id: &str) -> Frame {
        frame(json!({
            "kind":"stream_open", "id":id, "topic":"ui.events",
            "initialCredit":32, "payload":{}
        }))
    }

    fn event(stream: &str, sequence: u64) -> Frame {
        frame(json!({
            "kind":"event", "streamId":stream,
            "sequence":sequence.to_string(), "payload":{}
        }))
    }

    fn credit(stream: &str, amount: u64) -> Frame {
        frame(json!({"kind":"credit", "streamId":stream, "amount":amount}))
    }

    fn stream_close(stream: &str) -> Frame {
        frame(json!({"kind":"stream_close", "streamId":stream, "code":"closed"}))
    }

    /// CR-005 and CR-008: a cancelled request tolerates one late terminal.
    #[test]
    fn a_cancelled_request_ignores_one_late_terminal_and_fails_the_second() {
        let mut state = ContractState::default();
        state
            .observe(Direction::HostToPackage, &request("h:one"))
            .expect("request");
        state
            .observe(Direction::HostToPackage, &cancel("h:one"))
            .expect("cancel");
        assert_eq!(
            state.observe(Direction::PackageToHost, &response("h:one")),
            Ok(Verdict::Ignore)
        );
        assert_eq!(state.ignored().late_terminals, 1);
        assert_eq!(
            state
                .observe(Direction::PackageToHost, &error("h:one"))
                .unwrap_err()
                .code(),
            "second_terminal_response"
        );
    }

    /// The Host's own outbound `cancel` after the answer arrived.
    #[test]
    fn a_cancel_that_races_the_answer_is_ignored() {
        let mut state = ContractState::default();
        state
            .observe(Direction::HostToPackage, &request("h:one"))
            .expect("request");
        state
            .observe(Direction::PackageToHost, &response("h:one"))
            .expect("response");
        assert_eq!(
            state.observe(Direction::HostToPackage, &cancel("h:one")),
            Ok(Verdict::Ignore)
        );
        assert_eq!(state.ignored().late_cancels, 1);
    }

    #[test]
    fn a_cancel_for_a_request_that_never_existed_stays_fatal() {
        let mut state = ContractState::default();
        assert_eq!(
            state
                .observe(Direction::HostToPackage, &cancel("h:ghost"))
                .unwrap_err()
                .code(),
            "unknown_request"
        );
    }

    /// CR-008: a request the receiver abandoned without sending `cancel`.
    #[test]
    fn retiring_a_request_ignores_its_late_answer() {
        let mut state = ContractState::default();
        state
            .observe(Direction::PackageToHost, &request("p:call-1"))
            .expect("request");
        state.retire_request("p:call-1");
        assert_eq!(
            state.observe(Direction::HostToPackage, &error("p:call-1")),
            Ok(Verdict::Ignore)
        );
    }

    /// CR-002 and V-01: credit released for accepted events crosses the close.
    #[test]
    fn credit_and_events_after_a_close_are_ignored_in_either_direction() {
        for (opener, follower) in [
            (Direction::HostToPackage, Direction::PackageToHost),
            (Direction::PackageToHost, Direction::HostToPackage),
        ] {
            let id = if opener == Direction::HostToPackage {
                "h:stream-1"
            } else {
                "p:stream-1"
            };
            let mut state = ContractState::default();
            state.observe(opener, &stream_open(id)).expect("open");
            state.observe(follower, &event(id, 1)).expect("event");
            state
                .observe(follower, &stream_close(id))
                .expect("stream close");
            assert_eq!(state.observe(opener, &credit(id, 1)), Ok(Verdict::Ignore));
            assert_eq!(state.observe(follower, &event(id, 2)), Ok(Verdict::Ignore));
            assert_eq!(
                state.observe(opener, &stream_close(id)),
                Ok(Verdict::Ignore)
            );
            assert_eq!(state.ignored().late_stream_frames, 3);
        }
    }

    /// CR-007: the sender retires the stream before its own close reaches the peer.
    #[test]
    fn a_retired_stream_ignores_events_before_its_close_is_observed() {
        let mut state = ContractState::default();
        state
            .observe(Direction::HostToPackage, &stream_open("h:stream-1"))
            .expect("open");
        state.retire_stream("h:stream-1");
        assert_eq!(
            state.observe(Direction::PackageToHost, &event("h:stream-1", 1)),
            Ok(Verdict::Ignore)
        );
        // The close that ends the stream is still delivered exactly once.
        assert_eq!(
            state.observe(Direction::PackageToHost, &stream_close("h:stream-1")),
            Ok(Verdict::Deliver)
        );
    }

    #[test]
    fn a_frame_for_a_stream_that_never_opened_stays_fatal() {
        let mut state = ContractState::default();
        assert_eq!(
            state
                .observe(Direction::PackageToHost, &event("h:ghost", 1))
                .unwrap_err()
                .code(),
            "unknown_stream"
        );
        assert_eq!(
            state
                .observe(Direction::HostToPackage, &credit("h:ghost", 1))
                .unwrap_err()
                .code(),
            "unknown_stream"
        );
        assert_eq!(
            state
                .observe(Direction::HostToPackage, &stream_close("h:ghost"))
                .unwrap_err()
                .code(),
            "unknown_stream"
        );
    }

    /// The window is a ceiling: credit buys back only what was spent.
    #[test]
    fn credit_cannot_carry_a_stream_past_the_window_it_was_opened_with() {
        let mut state = ContractState::default();
        state
            .observe(Direction::HostToPackage, &stream_open("h:stream-1"))
            .expect("open");
        // Nothing has been consumed, so the window is already full.
        assert_eq!(
            state
                .observe(Direction::HostToPackage, &credit("h:stream-1", 1))
                .unwrap_err()
                .code(),
            "credit_violation"
        );
        for sequence in 1..=4 {
            state
                .observe(Direction::PackageToHost, &event("h:stream-1", sequence))
                .expect("event within the window");
        }
        state
            .observe(Direction::HostToPackage, &credit("h:stream-1", 4))
            .expect("credit for the four events accepted");
        assert_eq!(
            state
                .observe(Direction::HostToPackage, &credit("h:stream-1", 1))
                .unwrap_err()
                .code(),
            "credit_violation"
        );
        // A peer that repeats a full grant cannot inflate the window either.
        for _ in 0..1000 {
            assert_eq!(
                state
                    .observe(Direction::HostToPackage, &credit("h:stream-1", 32))
                    .unwrap_err()
                    .code(),
                "credit_violation"
            );
        }
    }

    #[test]
    fn a_live_stream_keeps_its_credit_and_sequence_rules() {
        let mut state = ContractState::default();
        state
            .observe(Direction::HostToPackage, &stream_open("h:stream-1"))
            .expect("open");
        for sequence in 1..=32 {
            state
                .observe(Direction::PackageToHost, &event("h:stream-1", sequence))
                .expect("event within the window");
        }
        assert_eq!(
            state
                .observe(Direction::PackageToHost, &event("h:stream-1", 33))
                .unwrap_err()
                .code(),
            "credit_violation"
        );
    }

    /// Decision D2: an evicted tombstone is out of contract again.
    #[test]
    fn an_evicted_tombstone_is_fatal_again() {
        let mut state = ContractState::default();
        state
            .observe(Direction::HostToPackage, &request("h:first"))
            .expect("request");
        state.retire_request("h:first");
        for index in 0..TOMBSTONE_CAPACITY {
            let id = format!("h:filler-{index}");
            state
                .observe(Direction::HostToPackage, &request(&id))
                .expect("request");
            state.retire_request(&id);
        }
        assert_eq!(
            state
                .observe(Direction::PackageToHost, &response("h:first"))
                .unwrap_err()
                .code(),
            "unknown_response"
        );
    }

    #[test]
    fn a_retired_id_cannot_be_reused_for_a_new_request() {
        let mut state = ContractState::default();
        state
            .observe(Direction::HostToPackage, &request("h:one"))
            .expect("request");
        state.retire_request("h:one");
        assert_eq!(
            state
                .observe(Direction::HostToPackage, &request("h:one"))
                .unwrap_err()
                .code(),
            "duplicate_id"
        );
    }
}
