//! Optional, bounded Provider-side Port diagnostics at the SDK dispatcher.
use tokio::sync::broadcast;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderPortStage {
    Received,
    Completed,
    Failed,
    Opened,
    Event,
    Closed,
}

/// Wire IDs and stable operation/code only; payloads and actor context are never copied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderPortObservation {
    pub provider_id: String,
    pub operation: &'static str,
    pub stage: ProviderPortStage,
    pub code: Option<String>,
    pub outcome_uncertain: bool,
}

pub type ProviderPortObservations = broadcast::Receiver<ProviderPortObservation>;

pub(crate) fn operation(name: &str) -> Option<&'static str> {
    match name {
        "teamarness.port.command.v1" => Some("command"),
        "teamarness.port.query.v1" => Some("query"),
        "teamarness.port.event.v1" => Some("event"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slow_observer_lags_without_a_producer_wait() {
        let (sender, mut receiver) = broadcast::channel(4);
        for n in 0..16 {
            sender
                .send(ProviderPortObservation {
                    provider_id: format!("h:port-{n}"),
                    operation: "command",
                    stage: ProviderPortStage::Completed,
                    code: None,
                    outcome_uncertain: false,
                })
                .unwrap();
        }
        assert!(matches!(
            receiver.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(_))
        ));
        assert!(
            sender
                .send(ProviderPortObservation {
                    provider_id: "h:port-next".into(),
                    operation: "query",
                    stage: ProviderPortStage::Received,
                    code: None,
                    outcome_uncertain: false,
                })
                .is_ok()
        );
    }
}
