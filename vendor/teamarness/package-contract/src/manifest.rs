//! The Package manifest rules the Package Host applies at admission.
//!
//! `parse_manifest` runs every rule that needs no file access, so an author can validate a
//! project source whose digests and sizes are still placeholders. `load_package_directory` is the
//! Host's admission path: it parses, then reads, bounds, and verifies every declared file.
//!
//! # Rule names
//!
//! | Rule | Code | What it holds |
//! |---|---|---|
//! | `manifest.unreadable` | `manifest_malformed` | `teamarness.package.json` is missing or unreadable |
//! | `manifest.too_large` | `manifest_malformed` | the manifest exceeds 256 KiB |
//! | `manifest.malformed` | `manifest_malformed` | not strict JSON, or an unknown or missing field |
//! | `manifest.schema_version` | `manifest_unsupported` | `schemaVersion` is not `1` |
//! | `manifest.identity` | `invalid_package_identity` | `identity` is not a Package Identity |
//! | `manifest.version_not_strict` | `manifest_unsupported` | `version` is not an exact semantic version |
//! | `manifest.display_name` | `manifest_malformed` | the display name is empty, padded, long or has control characters |
//! | `manifest.protocol_unsupported` | `manifest_unsupported` | the protocol range is invalid or excludes 1.0 |
//! | `manifest.extension_key` | `manifest_malformed` | an extension key is not a Package Identity |
//! | `manifest.capability_identity` | `manifest_malformed` | a capability identity is malformed |
//! | `manifest.capability_duplicate` | `manifest_malformed` | a capability is requested twice |
//! | `manifest.capability_range` | `manifest_unsupported` | a capability version range is invalid |
//! | `manifest.ui_bounds` | `manifest_malformed` | a declared UI size or count exceeds its bound |
//! | `manifest.target_missing` | `target_unsupported` | no single backend artifact matches the target |
//! | `manifest.duplicate_target` | `target_unsupported` | two backend artifacts declare the same target |
//! | `manifest.backend_target` | `manifest_malformed` | a backend artifact has an empty target |
//! | `manifest.duplicate_path` | `manifest_malformed` | two artifacts declare the same path |
//! | `manifest.path_not_normalized` | `manifest_malformed` | a path is absolute, escaping, or not normalized |
//! | `manifest.digest_format` | `manifest_malformed` | a `sha256` field is not 64 lowercase hex characters |
//! | `manifest.media_type` | `manifest_malformed` | an asset media type is not a MIME type |
//! | `manifest.ui_entrypoint_encoding` | `manifest_malformed` | the UI entrypoint is not UTF-8 |
//! | `manifest.artifact_missing` | `artifact_missing` | a declared backend file is absent, a symlink, or oversized |
//! | `manifest.ui_file_missing` | `ui_entrypoint_missing` | a declared UI file is absent, a symlink, or oversized |
//! | `manifest.contract_missing` | `ui_contract_violation` | the declared UI contract file is absent |
//! | `manifest.contract_too_large` | `ui_contract_violation` | the UI contract exceeds 1 MiB |
//! | `manifest.size_mismatch` | `artifact_changed` | a file's length differs from the declared size |
//! | `manifest.digest_mismatch` | `artifact_changed` | a file's digest differs from the declared digest |

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::{fs, io};

use semver::Version;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::{ContractError, ContractErrorCode};
use crate::identity::is_valid_package_identity;
use crate::json::strict_json;
use crate::port_contract::{CompiledPortContracts, PortKind, compile_port_contracts};

/// The manifest file name inside a Package Directory.
pub const MANIFEST_NAME: &str = "teamarness.package.json";
/// The Package Process Protocol version the Host speaks.
pub const SUPPORTED_PROTOCOL: (u16, u16) = (1, 0);

const MAX_MANIFEST_BYTES: usize = 256 * 1024;
const MAX_UI_ENTRYPOINT_BYTES: usize = 8 * 1024 * 1024;
const MAX_UI_ASSET_BYTES: usize = 8 * 1024 * 1024;
const MAX_UI_ASSETS: usize = 64;
const MAX_UI_CONTRACT_BYTES: usize = 1024 * 1024;
const MAX_UI_TOTAL_BYTES: usize = 32 * 1024 * 1024;

/// A parsed, semantically validated manifest. No file has been read.
#[derive(Debug, Clone)]
pub struct ManifestDocument {
    pub identity: String,
    pub version: Version,
    pub display_name: String,
    pub protocol: VersionRange,
    pub capabilities: Vec<CapabilityRequirement>,
    pub consumed_ports: Vec<ConsumedContract>,
    pub activation: ActivationPolicy,
    port_contract: Option<DigestFile>,
    backends: Vec<FileArtifact>,
    ui: UiDocument,
    manifest_digest: String,
}

impl ManifestDocument {
    /// The declared path of the provided Port Contract file, if any.
    #[must_use]
    pub fn port_contract_path(&self) -> Option<&str> {
        self.port_contract.as_ref().map(|file| file.path.as_str())
    }

    /// The declared path of the UI contract.
    #[must_use]
    pub fn ui_contract_path(&self) -> &str {
        &self.ui.contract.path
    }

    /// The declared path of the UI entrypoint, relative to the Package Directory.
    #[must_use]
    pub fn ui_entrypoint_path(&self) -> &str {
        &self.ui.entrypoint.path
    }

    /// The digest of the manifest bytes themselves.
    #[must_use]
    pub fn manifest_digest(&self) -> &str {
        &self.manifest_digest
    }

    /// The declared backend path for `target`, when exactly one artifact matches.
    #[must_use]
    pub fn backend_path(&self, target: &str) -> Option<&str> {
        let mut matching = self
            .backends
            .iter()
            .filter(|artifact| artifact.target == target);
        let first = matching.next()?;
        matching.next().is_none().then_some(first.path.as_str())
    }
}

/// A Package Directory whose declared files have been read and verified.
#[derive(Debug, Clone)]
pub struct LoadedManifest {
    pub identity: String,
    pub version: Version,
    pub display_name: String,
    pub protocol: VersionRange,
    pub backend: VerifiedFile,
    pub ui: LoadedUi,
    pub capabilities: Vec<CapabilityRequirement>,
    pub consumed_ports: Vec<ConsumedContract>,
    pub activation: ActivationPolicy,
    pub port_contract: Option<Arc<CompiledPortContracts>>,
    pub port_contract_digest: Option<String>,
    pub manifest_digest: String,
}

/// Package Backend startup policy; legacy manifests start eagerly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivationPolicy {
    #[default]
    Eager,
    OnDemand,
}

/// An inclusive major plus minor-range version request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VersionRange {
    pub major: u16,
    pub min_minor: u16,
    pub max_minor: u16,
}

/// A file whose bytes matched their declared size and digest.
#[derive(Debug, Clone)]
pub struct VerifiedFile {
    pub relative_path: String,
    pub bytes: Vec<u8>,
    pub digest: String,
}

/// The verified UI half of a Package Directory.
#[derive(Debug, Clone)]
pub struct LoadedUi {
    pub entrypoint: VerifiedFile,
    pub assets: Vec<LoadedAsset>,
    pub contract: VerifiedFile,
}

/// A verified UI asset and its declared media type.
#[derive(Debug, Clone)]
pub struct LoadedAsset {
    pub file: VerifiedFile,
    pub media_type: String,
}

/// One Semantic Capability family the Package requests, with the versions of it it accepts.
///
/// The declared `identity` **is** the family name: a family evolves by version within a major,
/// and a different capability shape is a new family declared beside it. Naming it here is what
/// lets an unknown capability be reported as an unknown family rather than a version error.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CapabilityRequirement {
    #[serde(rename = "identity")]
    pub family: String,
    pub major: u16,
    pub min_minor: u16,
    pub max_minor: u16,
}

impl CapabilityRequirement {
    /// The versions of the family this Package accepts.
    #[must_use]
    pub const fn version(&self) -> VersionRange {
        VersionRange {
            major: self.major,
            min_minor: self.min_minor,
            max_minor: self.max_minor,
        }
    }
}

/// An exact set of consumed ports under one compatible contract version range.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConsumedContract {
    pub identity: String,
    pub major: u16,
    pub min_minor: u16,
    pub max_minor: u16,
    #[serde(default = "required_by_default")]
    pub required: bool,
    pub ports: Vec<ConsumedPort>,
}

fn required_by_default() -> bool {
    true
}

/// One port, including its kind, not implicit permission to consume its siblings.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConsumedPort {
    pub name: String,
    pub kind: PortKind,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawManifest {
    schema_version: u16,
    identity: String,
    version: String,
    presentation: Presentation,
    protocol: VersionRange,
    backends: Vec<FileArtifact>,
    ui: UiDocument,
    #[serde(default)]
    capabilities: Vec<CapabilityRequirement>,
    #[serde(default)]
    consumed_ports: Vec<ConsumedContract>,
    #[serde(default)]
    activation: ActivationPolicy,
    port_contract: Option<DigestFile>,
    #[serde(default)]
    extensions: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Presentation {
    display_name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FileArtifact {
    target: String,
    path: String,
    size: u64,
    sha256: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UiDocument {
    entrypoint: SizedDigestFile,
    #[serde(default)]
    assets: Vec<AssetDocument>,
    contract: DigestFile,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SizedDigestFile {
    path: String,
    size: u64,
    sha256: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AssetDocument {
    path: String,
    media_type: String,
    size: u64,
    sha256: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DigestFile {
    path: String,
    sha256: String,
}

const fn malformed(rule: &'static str) -> ContractError {
    ContractError::new(ContractErrorCode::ManifestMalformed, rule)
}

const fn unsupported(rule: &'static str) -> ContractError {
    ContractError::new(ContractErrorCode::ManifestUnsupported, rule)
}

/// Applies every manifest rule that needs no file access.
///
/// Declared sizes and digests are checked for shape only, so a project source whose assembler has
/// not yet filled them in (size `0`, 64 zeros) passes.
///
/// # Errors
///
/// Returns the first rule the manifest breaks.
pub fn parse_manifest(bytes: &[u8]) -> Result<ManifestDocument, ContractError> {
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(malformed("manifest.too_large"));
    }
    let value = strict_json(bytes).map_err(|_| malformed("manifest.malformed"))?;
    let document: RawManifest =
        serde_json::from_value(value).map_err(|_| malformed("manifest.malformed"))?;
    if document.schema_version != 1 {
        return Err(unsupported("manifest.schema_version").at("/schemaVersion"));
    }
    if !is_valid_package_identity(&document.identity) {
        return Err(ContractError::new(
            ContractErrorCode::InvalidPackageIdentity,
            "manifest.identity",
        )
        .at("/identity"));
    }
    let version = Version::parse(&document.version)
        .map_err(|_| unsupported("manifest.version_not_strict").at("/version"))?;
    if version.to_string() != document.version {
        return Err(unsupported("manifest.version_not_strict").at("/version"));
    }
    validate_display_name(&document.presentation.display_name)?;
    validate_range(document.protocol, "manifest.protocol_unsupported")
        .map_err(|error| error.at("/protocol"))?;
    if document.protocol.major != SUPPORTED_PROTOCOL.0
        || !(document.protocol.min_minor..=document.protocol.max_minor)
            .contains(&SUPPORTED_PROTOCOL.1)
    {
        return Err(unsupported("manifest.protocol_unsupported").at("/protocol"));
    }
    validate_extensions(&document.extensions)?;
    validate_capabilities(&document.capabilities)?;
    validate_consumed_ports(&document.consumed_ports)?;
    if document.ui.entrypoint.size > MAX_UI_ENTRYPOINT_BYTES as u64
        || document.ui.assets.len() > MAX_UI_ASSETS
        || document
            .ui
            .assets
            .iter()
            .any(|asset| asset.size > MAX_UI_ASSET_BYTES as u64)
    {
        return Err(malformed("manifest.ui_bounds").at("/ui"));
    }

    let mut seen_targets = BTreeSet::new();
    for artifact in &document.backends {
        if !seen_targets.insert(artifact.target.clone()) {
            return Err(ContractError::new(
                ContractErrorCode::TargetUnsupported,
                "manifest.duplicate_target",
            )
            .at("/backends"));
        }
    }
    let mut seen_paths = BTreeSet::new();
    for (index, artifact) in document.backends.iter().enumerate() {
        if artifact.target.is_empty() {
            return Err(
                malformed("manifest.backend_target").at(format!("/backends/{index}/target"))
            );
        }
        if !seen_paths.insert(artifact.path.clone()) {
            return Err(malformed("manifest.duplicate_path").at(format!("/backends/{index}/path")));
        }
        normalized_relative_path(&artifact.path)
            .map_err(|error| error.at(format!("/backends/{index}/path")))?;
        validate_digest(&artifact.sha256)
            .map_err(|error| error.at(format!("/backends/{index}/sha256")))?;
    }
    normalized_relative_path(&document.ui.entrypoint.path)
        .map_err(|error| error.at("/ui/entrypoint/path"))?;
    validate_digest(&document.ui.entrypoint.sha256)
        .map_err(|error| error.at("/ui/entrypoint/sha256"))?;
    normalized_relative_path(&document.ui.contract.path)
        .map_err(|error| error.at("/ui/contract/path"))?;
    validate_digest(&document.ui.contract.sha256)
        .map_err(|error| error.at("/ui/contract/sha256"))?;
    if !seen_paths.insert(document.ui.entrypoint.path.clone()) {
        return Err(malformed("manifest.duplicate_path").at("/ui/entrypoint/path"));
    }
    if !seen_paths.insert(document.ui.contract.path.clone()) {
        return Err(malformed("manifest.duplicate_path").at("/ui/contract/path"));
    }
    if let Some(port_contract) = &document.port_contract {
        normalized_relative_path(&port_contract.path)
            .map_err(|error| error.at("/portContract/path"))?;
        validate_digest(&port_contract.sha256).map_err(|error| error.at("/portContract/sha256"))?;
        if !seen_paths.insert(port_contract.path.clone()) {
            return Err(malformed("manifest.duplicate_path").at("/portContract/path"));
        }
    }
    for (index, asset) in document.ui.assets.iter().enumerate() {
        normalized_relative_path(&asset.path)
            .map_err(|error| error.at(format!("/ui/assets/{index}/path")))?;
        validate_digest(&asset.sha256)
            .map_err(|error| error.at(format!("/ui/assets/{index}/sha256")))?;
        if !seen_paths.insert(asset.path.clone()) {
            return Err(malformed("manifest.duplicate_path").at(format!("/ui/assets/{index}/path")));
        }
        if mime::Mime::from_str(&asset.media_type).is_err() {
            return Err(
                malformed("manifest.media_type").at(format!("/ui/assets/{index}/mediaType"))
            );
        }
    }

    Ok(ManifestDocument {
        identity: document.identity,
        version,
        display_name: document.presentation.display_name,
        protocol: document.protocol,
        capabilities: document.capabilities,
        consumed_ports: document.consumed_ports,
        activation: document.activation,
        port_contract: document.port_contract,
        backends: document.backends,
        ui: document.ui,
        manifest_digest: sha256(bytes),
    })
}

/// Applies the Package Host's admission rules to a built Package Directory.
///
/// # Errors
///
/// Returns the first rule the directory breaks.
pub fn load_package_directory(root: &Path, target: &str) -> Result<LoadedManifest, ContractError> {
    let bytes = read_regular_file_bounded(
        root,
        MANIFEST_NAME,
        ContractErrorCode::ManifestMalformed,
        "manifest.unreadable",
        usize::MAX,
    )?;
    let document = parse_manifest(&bytes)?;

    let backend_artifact = document
        .backends
        .iter()
        .filter(|artifact| artifact.target == target)
        .collect::<Vec<_>>();
    if backend_artifact.len() != 1 {
        return Err(ContractError::new(
            ContractErrorCode::TargetUnsupported,
            "manifest.target_missing",
        )
        .at("/backends"));
    }
    let backend = verify_sized_file(
        root,
        backend_artifact[0],
        ContractErrorCode::ArtifactMissing,
        "manifest.artifact_missing",
        usize::MAX,
    )?;
    let entrypoint = verify_sized_file(
        root,
        &FileArtifact {
            target: String::new(),
            path: document.ui.entrypoint.path.clone(),
            size: document.ui.entrypoint.size,
            sha256: document.ui.entrypoint.sha256.clone(),
        },
        ContractErrorCode::UiEntrypointMissing,
        "manifest.ui_file_missing",
        MAX_UI_ENTRYPOINT_BYTES,
    )?;
    if entrypoint.bytes.len() > MAX_UI_ENTRYPOINT_BYTES {
        return Err(malformed("manifest.ui_bounds").at("/ui/entrypoint"));
    }
    if std::str::from_utf8(&entrypoint.bytes).is_err() {
        return Err(malformed("manifest.ui_entrypoint_encoding").at("/ui/entrypoint"));
    }
    let mut ui_total = entrypoint.bytes.len();
    let mut assets = Vec::with_capacity(document.ui.assets.len());
    for (index, asset) in document.ui.assets.iter().enumerate() {
        let file = verify_sized_file(
            root,
            &FileArtifact {
                target: String::new(),
                path: asset.path.clone(),
                size: asset.size,
                sha256: asset.sha256.clone(),
            },
            ContractErrorCode::UiEntrypointMissing,
            "manifest.ui_file_missing",
            MAX_UI_ASSET_BYTES,
        )?;
        if file.bytes.len() > MAX_UI_ASSET_BYTES {
            return Err(malformed("manifest.ui_bounds").at(format!("/ui/assets/{index}")));
        }
        ui_total = ui_total
            .checked_add(file.bytes.len())
            .ok_or_else(|| malformed("manifest.ui_bounds").at("/ui"))?;
        assets.push(LoadedAsset {
            file,
            media_type: asset.media_type.clone(),
        });
    }
    let contract = verify_digest_file(root, &document.ui.contract)?;
    ui_total = ui_total
        .checked_add(contract.bytes.len())
        .ok_or_else(|| malformed("manifest.ui_bounds").at("/ui"))?;
    if contract.bytes.len() > MAX_UI_CONTRACT_BYTES {
        return Err(ContractError::new(
            ContractErrorCode::UiContractViolation,
            "manifest.contract_too_large",
        )
        .at("/ui/contract"));
    }
    if ui_total > MAX_UI_TOTAL_BYTES {
        return Err(malformed("manifest.ui_bounds").at("/ui"));
    }

    let (port_contract, port_contract_digest) = if let Some(reference) = &document.port_contract {
        let file = verify_port_contract_file(root, reference)?;
        let compiled = compile_port_contracts(&file.bytes)?;
        (Some(Arc::new(compiled)), Some(file.digest))
    } else {
        (None, None)
    };

    Ok(LoadedManifest {
        identity: document.identity,
        version: document.version,
        display_name: document.display_name,
        protocol: document.protocol,
        backend,
        ui: LoadedUi {
            entrypoint,
            assets,
            contract,
        },
        capabilities: document.capabilities,
        consumed_ports: document.consumed_ports,
        activation: document.activation,
        port_contract,
        port_contract_digest,
        manifest_digest: document.manifest_digest,
    })
}

fn validate_display_name(value: &str) -> Result<(), ContractError> {
    // The schema publishes `"maxLength": 128`, which JSON Schema 2020-12
    // counts in code points. Counting bytes rejected a name the schema admits.
    if value.trim() != value
        || value.is_empty()
        || value.chars().count() > 128
        || value.chars().any(char::is_control)
    {
        return Err(malformed("manifest.display_name").at("/presentation/displayName"));
    }
    Ok(())
}

fn validate_range(range: VersionRange, rule: &'static str) -> Result<(), ContractError> {
    if range.major == 0 || range.min_minor > range.max_minor {
        return Err(unsupported(rule));
    }
    Ok(())
}

fn validate_capabilities(values: &[CapabilityRequirement]) -> Result<(), ContractError> {
    let mut identities = BTreeSet::new();
    for (index, value) in values.iter().enumerate() {
        if !is_valid_package_identity(&value.family) {
            return Err(malformed("manifest.capability_identity")
                .at(format!("/capabilities/{index}/identity")));
        }
        validate_range(value.version(), "manifest.capability_range")
            .map_err(|error| error.at(format!("/capabilities/{index}")))?;
        if !identities.insert(&value.family) {
            return Err(
                malformed("manifest.capability_duplicate").at(format!("/capabilities/{index}"))
            );
        }
    }
    Ok(())
}

fn validate_consumed_ports(values: &[ConsumedContract]) -> Result<(), ContractError> {
    if values.len() > 64 {
        return Err(malformed("manifest.consumed_bounds"));
    }
    let mut identities = BTreeSet::new();
    for contract in values {
        if !is_valid_package_identity(&contract.identity) || !identities.insert(&contract.identity)
        {
            return Err(malformed("manifest.consumed_identity"));
        }
        validate_range(
            VersionRange {
                major: contract.major,
                min_minor: contract.min_minor,
                max_minor: contract.max_minor,
            },
            "manifest.consumed_range",
        )?;
        if contract.ports.is_empty() || contract.ports.len() > 256 {
            return Err(malformed("manifest.consumed_bounds"));
        }
        let mut ports = BTreeSet::new();
        for port in &contract.ports {
            if !crate::ui_contract::valid_operation_name(&port.name) || !ports.insert(&port.name) {
                return Err(malformed("manifest.consumed_port"));
            }
        }
    }
    Ok(())
}

fn validate_extensions(values: &BTreeMap<String, Value>) -> Result<(), ContractError> {
    for key in values.keys() {
        if !is_valid_package_identity(key) {
            return Err(malformed("manifest.extension_key").at(format!("/extensions/{key}")));
        }
    }
    Ok(())
}

fn verify_sized_file(
    root: &Path,
    artifact: &FileArtifact,
    missing_code: ContractErrorCode,
    missing_rule: &'static str,
    maximum_bytes: usize,
) -> Result<VerifiedFile, ContractError> {
    let bytes = read_regular_file_bounded(
        root,
        &artifact.path,
        missing_code,
        missing_rule,
        maximum_bytes,
    )?;
    if u64::try_from(bytes.len()).ok() != Some(artifact.size) {
        return Err(ContractError::new(
            ContractErrorCode::ArtifactChanged,
            "manifest.size_mismatch",
        )
        .at(artifact.path.clone()));
    }
    if sha256(&bytes) != artifact.sha256 {
        return Err(ContractError::new(
            ContractErrorCode::ArtifactChanged,
            "manifest.digest_mismatch",
        )
        .at(artifact.path.clone()));
    }
    Ok(VerifiedFile {
        relative_path: artifact.path.clone(),
        bytes,
        digest: artifact.sha256.clone(),
    })
}

fn verify_digest_file(root: &Path, artifact: &DigestFile) -> Result<VerifiedFile, ContractError> {
    let bytes = read_regular_file_bounded(
        root,
        &artifact.path,
        ContractErrorCode::UiContractViolation,
        "manifest.contract_missing",
        MAX_UI_CONTRACT_BYTES,
    )?;
    if sha256(&bytes) != artifact.sha256 {
        return Err(ContractError::new(
            ContractErrorCode::ArtifactChanged,
            "manifest.digest_mismatch",
        )
        .at(artifact.path.clone()));
    }
    Ok(VerifiedFile {
        relative_path: artifact.path.clone(),
        bytes,
        digest: artifact.sha256.clone(),
    })
}

fn verify_port_contract_file(
    root: &Path,
    artifact: &DigestFile,
) -> Result<VerifiedFile, ContractError> {
    let bytes = read_regular_file_bounded(
        root,
        &artifact.path,
        ContractErrorCode::PortContractViolation,
        "port_contract.missing",
        1024 * 1024,
    )?;
    if sha256(&bytes) != artifact.sha256 {
        return Err(ContractError::new(
            ContractErrorCode::ArtifactChanged,
            "manifest.digest_mismatch",
        )
        .at(artifact.path.clone()));
    }
    Ok(VerifiedFile {
        relative_path: artifact.path.clone(),
        bytes,
        digest: artifact.sha256.clone(),
    })
}

fn validate_digest(value: &str) -> Result<(), ContractError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(malformed("manifest.digest_format"));
    }
    Ok(())
}

fn read_regular_file_bounded(
    root: &Path,
    relative: &str,
    missing_code: ContractErrorCode,
    missing_rule: &'static str,
    maximum_bytes: usize,
) -> Result<Vec<u8>, ContractError> {
    let missing = || ContractError::new(missing_code, missing_rule).at(relative.to_owned());
    let relative_path =
        normalized_relative_path(relative).map_err(|error| error.at(relative.to_owned()))?;
    let mut path = PathBuf::from(root);
    for component in relative_path.components() {
        let Component::Normal(component) = component else {
            return Err(malformed("manifest.path_not_normalized").at(relative.to_owned()));
        };
        path.push(component);
        let metadata = fs::symlink_metadata(&path).map_err(|_| missing())?;
        if metadata.file_type().is_symlink() {
            return Err(missing());
        }
    }
    let metadata = fs::metadata(&path).map_err(|_| missing())?;
    if !metadata.is_file() || metadata.len() > maximum_bytes as u64 {
        return Err(missing());
    }
    fs::read(path).map_err(|_: io::Error| missing())
}

fn normalized_relative_path(value: &str) -> Result<PathBuf, ContractError> {
    if value.is_empty()
        || value.starts_with('/')
        || value.ends_with('/')
        || value.contains('\\')
        || value.contains("//")
        || value
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || !value.is_ascii()
    {
        return Err(malformed("manifest.path_not_normalized"));
    }
    let path = PathBuf::from(value);
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(malformed("manifest.path_not_normalized"));
    }
    Ok(path)
}

/// Returns the lowercase hexadecimal SHA-256 digest of `bytes`.
#[must_use]
pub fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        use fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}
