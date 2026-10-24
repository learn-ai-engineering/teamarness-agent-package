use std::sync::Arc;

use teamarness_package_protocol::{
    ContractState, Direction, Frame, JsonValue, MAX_FRAME_BYTES, MAX_IN_FLIGHT_EACH_DIRECTION,
    MAX_WRITER_BYTES, MAX_WRITER_FRAMES, encode_frame,
};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

use crate::SdkError;

pub(crate) type SharedState = Arc<Mutex<ContractState>>;

/// Slots on the data lane.
///
/// Worst case: every capability request the contract permits the Package,
/// queued at once. The stream frames that follow one are written by the
/// stream's own task, which waits for a slot rather than refusing.
const DATA_QUEUE: usize = MAX_IN_FLIGHT_EACH_DIRECTION;

/// Frames the dispatcher loop owes besides the terminal answers.
///
/// `pong`, a cancelled request's `error`, and `shutdown_ack`: three, which is
/// the lifecycle reserve `cb31744` deleted while leaving this comment behind.
const LIFECYCLE_RESERVE: usize = 3;

/// Every frame the control lane can owe at once.
///
/// One terminal answer for each of the in-flight requests the contract permits
/// the Host, plus the three lifecycle frames.
const CONTROL_WORST_CASE: usize = MAX_IN_FLIGHT_EACH_DIRECTION + LIFECYCLE_RESERVE;

/// Whether a full control lane makes its sender wait rather than refusing.
///
/// It does, and that is why the reserve is not restored: 35 control slots
/// beside the data lane's 32 is 67 against the 64 frames the contract
/// publishes for the whole writer, so the lifecycle frames wait behind the
/// terminal answers instead of having slots of their own. Both
/// [`PriorityWriter::send`] and [`PriorityWriter::control_waiting`] wait on
/// depth (`sender.send(..).await`); neither refuses a frame for want of a
/// slot. The wait is bounded by [`run`], which drains as the Host reads, and
/// by the Host's five-second health probe.
const CONTROL_WAITS_FOR_A_SLOT: bool = true;

/// Slots on the control lane: the rest of the writer's published frame budget.
///
/// The two lanes share the one writer budget the contract publishes and the
/// Host's handshake transmits, so their depths are a split of
/// `MAX_WRITER_FRAMES`, exactly as their byte budgets are a split of
/// `MAX_WRITER_BYTES`. The lane holds one terminal frame for each of the
/// in-flight requests the contract permits the Host; `initialized`, `pong` and
/// `shutdown_ack` wait for a slot behind them, because no send on this writer
/// refuses on depth.
const CONTROL_QUEUE: usize = MAX_WRITER_FRAMES - DATA_QUEUE;

/// The control lane's byte budget: two contract-maximum frames, so one maximum
/// answer never waits on another that is already queued.
const CONTROL_BYTES: usize = 2 * MAX_FRAME_BYTES;

/// The data lane keeps the rest of the writer budget the contract publishes.
const DATA_BYTES: usize = MAX_WRITER_BYTES - CONTROL_BYTES;

// Rule 2's floors for this crate, asserted beside the bounds they guard so a
// bound that stops admitting its worst case fails the build rather than a
// suite. ADR 0019 §3 names them.
//
// Each left-hand side is written from the traffic, independently of the bound
// it guards, so none of them can be satisfied by moving the bound it reads.
const _: () = {
    // The lane cannot hold its worst case, so it must wait for a slot. This is
    // the assertion the deleted lifecycle reserve should have left behind:
    // it cannot be met by widening, because the data lane's own worst case
    // already claims half the published budget.
    assert!(CONTROL_QUEUE >= CONTROL_WORST_CASE || CONTROL_WAITS_FOR_A_SLOT);
    assert!(CONTROL_QUEUE >= MAX_IN_FLIGHT_EACH_DIRECTION);
    assert!(DATA_QUEUE >= MAX_IN_FLIGHT_EACH_DIRECTION);
    // Both lanes' independently counted worst cases must fit the one budget
    // the handshake names. This is the assertion an SF2-001 recurrence breaks.
    assert!(DATA_QUEUE + MAX_IN_FLIGHT_EACH_DIRECTION <= MAX_WRITER_FRAMES);
    assert!(CONTROL_QUEUE + DATA_QUEUE == MAX_WRITER_FRAMES);
    assert!(CONTROL_BYTES >= 2 * MAX_FRAME_BYTES);
    assert!(DATA_BYTES >= 2 * MAX_FRAME_BYTES);
    assert!(CONTROL_BYTES + DATA_BYTES == MAX_WRITER_BYTES);
};

pub(crate) struct QueuedFrame {
    frame: Frame,
    _bytes: OwnedSemaphorePermit,
}

#[derive(Clone)]
pub(crate) struct PriorityWriter {
    control: mpsc::Sender<QueuedFrame>,
    data: mpsc::Sender<QueuedFrame>,
    control_bytes: Arc<Semaphore>,
    data_bytes: Arc<Semaphore>,
}

impl PriorityWriter {
    pub(crate) fn channels() -> (
        Self,
        mpsc::Receiver<QueuedFrame>,
        mpsc::Receiver<QueuedFrame>,
    ) {
        let (control, control_rx) = mpsc::channel(CONTROL_QUEUE);
        let (data, data_rx) = mpsc::channel(DATA_QUEUE);
        (
            Self {
                control,
                data,
                control_bytes: Arc::new(Semaphore::new(CONTROL_BYTES)),
                data_bytes: Arc::new(Semaphore::new(DATA_BYTES)),
            },
            control_rx,
            data_rx,
        )
    }

    pub(crate) async fn control(&self, value: JsonValue) -> Result<(), SdkError> {
        self.send(value, true).await
    }

    pub(crate) async fn data(&self, value: JsonValue) -> Result<(), SdkError> {
        self.send(value, false).await
    }

    /// Queues a control frame, waiting for the lane's byte budget rather than
    /// refusing it.
    ///
    /// The wait this adds is for *bytes*. Every send on this writer already
    /// waits for a slot (`CONTROL_WAITS_FOR_A_SLOT`), which is bounded by the
    /// writer task draining to the Host. A saturated byte budget behind a slow
    /// reader is ordinary backpressure too, but it can hold a whole
    /// contract-maximum frame, so only a task that owns its own work may wait
    /// for it — the dispatcher loop must not, because a Package that stopped
    /// reading would stop answering everything else too. The request's one
    /// terminal frame is exactly such a
    /// task's work: the contract says a sender never withholds a frame it
    /// owes, and the handler has already claimed this request's only one, so
    /// refusing it leaves the request with no terminal frame at all.
    pub(crate) async fn control_waiting(&self, value: JsonValue) -> Result<(), SdkError> {
        let (frame, permits) = self.measure(value)?;
        let permit = self
            .control_bytes
            .clone()
            .acquire_many_owned(permits)
            .await
            .map_err(|_| SdkError::ParentClosed)?;
        self.control
            .send(QueuedFrame {
                frame,
                _bytes: permit,
            })
            .await
            .map_err(|_| SdkError::ParentClosed)
    }

    async fn send(&self, value: JsonValue, control: bool) -> Result<(), SdkError> {
        let (frame, permits) = self.measure(value)?;
        let semaphore = if control {
            self.control_bytes.clone()
        } else {
            self.data_bytes.clone()
        };
        let permit = semaphore
            .try_acquire_many_owned(permits)
            .map_err(|_| SdkError::Protocol("overloaded"))?;
        let queued = QueuedFrame {
            frame,
            _bytes: permit,
        };
        let sender = if control { &self.control } else { &self.data };
        sender
            .send(queued)
            .await
            .map_err(|_| SdkError::ParentClosed)
    }

    /// Validates one frame and reports the byte permits it needs.
    fn measure(&self, value: JsonValue) -> Result<(Frame, u32), SdkError> {
        let frame = Frame::try_from(value).map_err(SdkError::protocol)?;
        let size = encode_frame(&frame).map_err(SdkError::protocol)?.len();
        let permits = u32::try_from(size).map_err(|_| SdkError::Protocol("overloaded"))?;
        Ok((frame, permits))
    }
}

/// Writes queued frames until every sender is gone or `stop` fires.
///
/// Package-owned background work may keep `CapabilityClient` clones (and so
/// writer senders) alive indefinitely, so the dispatcher's end of the session
/// cannot rely on the channels closing. When `stop` fires, the writer flushes
/// only frames already queued (such as `shutdown_ack`) and returns.
pub(crate) async fn run<W>(
    mut output: W,
    mut control: mpsc::Receiver<QueuedFrame>,
    mut data: mpsc::Receiver<QueuedFrame>,
    mut stop: oneshot::Receiver<()>,
    state: SharedState,
) -> Result<(), SdkError>
where
    W: AsyncWrite + Unpin,
{
    loop {
        let queued = tokio::select! {
            biased;
            _ = &mut stop => {
                while let Ok(queued) = control.try_recv().or_else(|_| data.try_recv()) {
                    write(&mut output, &queued, &state).await?;
                }
                return Ok(());
            }
            value = control.recv() => value,
            value = data.recv() => value,
        };
        let Some(queued) = queued else {
            return Ok(());
        };
        write(&mut output, &queued, &state).await?;
    }
}

/// Writes one frame, after the shared classifier has seen it.
///
/// Frames are observed here, in the order they reach the wire, so the Package's
/// view of the contract is the one the Host will see. The verdict is about
/// delivery, which a writer has no part in: it writes every frame the
/// classifier accepts, and ends the session only on one the classifier calls
/// out of contract.
async fn write<W>(output: &mut W, queued: &QueuedFrame, state: &SharedState) -> Result<(), SdkError>
where
    W: AsyncWrite + Unpin,
{
    state
        .lock()
        .await
        .observe(Direction::PackageToHost, &queued.frame)
        .map_err(SdkError::protocol)?;
    let bytes = encode_frame(&queued.frame).map_err(SdkError::protocol)?;
    output
        .write_all(&bytes)
        .await
        .map_err(|_| SdkError::ParentClosed)?;
    output.flush().await.map_err(|_| SdkError::ParentClosed)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::{CONTROL_QUEUE, PriorityWriter};

    /// The three lifecycle frames have no slots of their own, so they wait
    /// behind a full lane of terminal answers instead of being refused.
    ///
    /// This is the property the lane's comment claims and the floor's
    /// `CONTROL_WAITS_FOR_A_SLOT` stands on. `cb31744` deleted the reserve the
    /// comment named and nothing noticed, because nothing asserted either the
    /// reserve or the wait that replaced it. A send changed back to `try_send`
    /// fails here.
    #[tokio::test]
    async fn no_control_send_refuses_for_want_of_a_slot() {
        let (writer, mut control, _data) = PriorityWriter::channels();
        let pong = || json!({"kind":"pong", "nonce":"bm9uY2U"});
        for index in 0..CONTROL_QUEUE {
            writer
                .control(pong())
                .await
                .unwrap_or_else(|error| panic!("lifecycle frame {index} was refused: {error:?}"));
        }

        let waiting = tokio::spawn({
            let writer = writer.clone();
            async move { writer.control(pong()).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !waiting.is_finished(),
            "the send did not wait for a slot on a full lane"
        );

        control.recv().await.expect("a queued frame");
        tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("the waiting send settles once a slot frees")
            .expect("the waiting task")
            .expect("the frame is queued, not refused");
    }
}
