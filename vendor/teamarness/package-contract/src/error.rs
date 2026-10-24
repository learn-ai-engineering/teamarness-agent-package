//! The admission verdict vocabulary shared by the Package Host and the author validator.

use std::fmt;

/// Stable admission failure codes.
///
/// The Package Host maps each of these onto its own `PackageErrorCode`; the author validator
/// prints them. `SmokeFailed` belongs to the validator alone and is never produced by admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ContractErrorCode {
    ManifestMalformed,
    ManifestUnsupported,
    InvalidPackageIdentity,
    TargetUnsupported,
    ArtifactMissing,
    ArtifactChanged,
    UiEntrypointMissing,
    UiContractViolation,
    PortContractViolation,
    /// Produced only by `teamarness-package-validate --smoke`.
    SmokeFailed,
}

impl ContractErrorCode {
    /// Returns the snake_case wire name the Package Host uses for the same code.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ManifestMalformed => "manifest_malformed",
            Self::ManifestUnsupported => "manifest_unsupported",
            Self::InvalidPackageIdentity => "invalid_package_identity",
            Self::TargetUnsupported => "target_unsupported",
            Self::ArtifactMissing => "artifact_missing",
            Self::ArtifactChanged => "artifact_changed",
            Self::UiEntrypointMissing => "ui_entrypoint_missing",
            Self::UiContractViolation => "ui_contract_violation",
            Self::PortContractViolation => "port_contract_violation",
            Self::SmokeFailed => "smoke_failed",
        }
    }
}

impl fmt::Display for ContractErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One admission failure: the Host-visible `code`, a stable author-facing `rule`, and an
/// optional JSON pointer into the document that failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractError {
    pub code: ContractErrorCode,
    pub rule: &'static str,
    pub pointer: Option<String>,
}

impl ContractError {
    /// Creates an error without a pointer.
    #[must_use]
    pub const fn new(code: ContractErrorCode, rule: &'static str) -> Self {
        Self {
            code,
            rule,
            pointer: None,
        }
    }

    /// Attaches a JSON pointer, keeping the first pointer already set.
    #[must_use]
    pub fn at(mut self, pointer: impl Into<String>) -> Self {
        if self.pointer.is_none() {
            self.pointer = Some(pointer.into());
        }
        self
    }
}

impl fmt::Display for ContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "[{}/{}]", self.code, self.rule)?;
        if let Some(pointer) = &self.pointer {
            write!(formatter, " {pointer}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ContractError {}
