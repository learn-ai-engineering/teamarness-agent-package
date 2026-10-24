/// Lifecycle operations serialized by the SDK dispatcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleOperation {
    Activate,
    Suspend,
    Recover,
    Dispose,
}

impl LifecycleOperation {
    pub(crate) fn parse(operation: &str) -> Option<Self> {
        match operation {
            "lifecycle.activate" => Some(Self::Activate),
            "lifecycle.suspend" => Some(Self::Suspend),
            "lifecycle.recover" => Some(Self::Recover),
            "lifecycle.dispose" => Some(Self::Dispose),
            _ => None,
        }
    }
}
