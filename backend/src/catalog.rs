//! Host-backed, complete per-turn metadata at the Catalog seam.
//!
//! `discovered` uses the canonical SDK's chunk, digest and schema validation. BundledCatalog
//! retains historical Provider fixtures for policy/schema regressions, never runtime discovery.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The query an agent-facing contract offers before first use.
pub const GUIDE_PORT: &str = "runtime.guide";

/// The bundled `ports.json` copies, by catalog file name.
const BUNDLED: [(&str, &str); 3] = [
    (
        "conversations.ports.json",
        include_str!("../catalog/conversations.ports.json"),
    ),
    (
        "documents.ports.json",
        include_str!("../catalog/documents.ports.json"),
    ),
    (
        "exhibits.ports.json",
        include_str!("../catalog/exhibits.ports.json"),
    ),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortKind {
    Command,
    Query,
    Event,
}

/// One port of a contract. An event has no request or response.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CatalogPort {
    pub name: String,
    pub kind: PortKind,
    pub description: String,
    #[serde(default)]
    pub request: Option<Value>,
    #[serde(default)]
    pub response: Option<Value>,
    #[serde(default)]
    pub event: Option<Value>,
}

/// One version of a Provider contract, with the `$defs` of the document that declared it.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogContract {
    pub identity: String,
    pub major: u16,
    pub minor: u16,
    pub description: String,
    pub ports: Vec<CatalogPort>,
    /// Whether the contract has a `runtime.guide` query, which makes it agent-facing.
    pub has_guide: bool,
    /// The declaring document's `$defs`, which the port schemas reference as `#/$defs/<name>`.
    pub defs: Map<String, Value>,
}

/// The last segment of a contract identity: `dev.teamarness.exhibits` → `exhibits`.
#[must_use]
pub fn alias(identity: &str) -> &str {
    identity.rsplit('.').next().unwrap_or(identity)
}

/// The contracts the Agent knows about.
pub trait Catalog {
    fn contracts(&self) -> &[CatalogContract];
}

/// A fixed list is a catalog; tests use it for fixture contracts.
impl Catalog for Vec<CatalogContract> {
    fn contracts(&self) -> &[CatalogContract] {
        self
    }
}

/// A catalog document could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogError(String);

impl fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "catalog: {}", self.0)
    }
}

impl std::error::Error for CatalogError {}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Document {
    schema_version: u16,
    #[serde(rename = "$defs", default)]
    defs: Map<String, Value>,
    contracts: Vec<RawContract>,
}

#[derive(Deserialize)]
struct RawContract {
    identity: String,
    major: u16,
    minor: u16,
    description: String,
    ports: Vec<CatalogPort>,
}

/// Reads one `ports.json` document.
///
/// # Errors
/// The document is not JSON of the expected shape, has another schema version, repeats a
/// contract or port, or declares a command or query without a request schema.
pub fn parse_document(text: &str) -> Result<Vec<CatalogContract>, CatalogError> {
    let document: Document =
        serde_json::from_str(text).map_err(|error| CatalogError(error.to_string()))?;
    if document.schema_version != 1 {
        return Err(CatalogError(format!(
            "unsupported schemaVersion {}",
            document.schema_version
        )));
    }
    let mut identities = BTreeSet::new();
    document
        .contracts
        .into_iter()
        .map(|contract| {
            if !identities.insert(contract.identity.clone()) {
                return Err(CatalogError(format!("{} repeated", contract.identity)));
            }
            let mut names = BTreeSet::new();
            for port in &contract.ports {
                if !names.insert(port.name.as_str()) {
                    return Err(CatalogError(format!(
                        "{}/{} repeated",
                        contract.identity, port.name
                    )));
                }
                if port.kind != PortKind::Event && port.request.is_none() {
                    return Err(CatalogError(format!(
                        "{}/{} has no request schema",
                        contract.identity, port.name
                    )));
                }
            }
            let has_guide = contract
                .ports
                .iter()
                .any(|port| port.name == GUIDE_PORT && port.kind == PortKind::Query);
            Ok(CatalogContract {
                identity: contract.identity,
                major: contract.major,
                minor: contract.minor,
                description: contract.description,
                ports: contract.ports,
                has_guide,
                defs: document.defs.clone(),
            })
        })
        .collect()
}

/// Historical Provider fixtures, never production discovery authority.
#[derive(Debug)]
pub struct BundledCatalog {
    contracts: Vec<CatalogContract>,
}

impl BundledCatalog {
    /// The bundled catalog, parsed on first use.
    ///
    /// # Panics
    /// The bundled files are malformed. They are verified copies and a unit test parses them,
    /// so this is a build defect, not a runtime condition.
    #[must_use]
    pub fn get() -> &'static Self {
        static CATALOG: OnceLock<BundledCatalog> = OnceLock::new();
        CATALOG.get_or_init(|| Self::load().expect("the bundled catalog parses"))
    }

    fn load() -> Result<Self, CatalogError> {
        let mut contracts: Vec<CatalogContract> = Vec::new();
        for (name, text) in BUNDLED {
            for contract in parse_document(text)
                .map_err(|CatalogError(cause)| CatalogError(format!("{name}: {cause}")))?
            {
                if contracts
                    .iter()
                    .any(|known| known.identity == contract.identity)
                {
                    return Err(CatalogError(format!("{} repeated", contract.identity)));
                }
                contracts.push(contract);
            }
        }
        Ok(Self { contracts })
    }
}

impl Catalog for BundledCatalog {
    fn contracts(&self) -> &[CatalogContract] {
        &self.contracts
    }
}

/// Complete, owned metadata for one turn. The SDK verifies chunks, digest and schemas;
/// this snapshot never grants dispatch authority and is not reused by another turn.
pub async fn discovered(
    ports: &teamarness_package_sdk::ports::PortClient,
) -> Result<Vec<CatalogContract>, teamarness_package_sdk::BackendError> {
    use teamarness_package_sdk::ports::PortAccess;
    let mut contracts = Vec::new();
    for summary in summaries(ports).await? {
        if !summary.available || !summary.enabled || summary.enabled_major != Some(summary.major) {
            continue;
        }
        let digest = summary.metadata_sha256.ok_or_else(invalid_metadata)?;
        let document = ports
            .describe(PortAccess::Agent, &summary.contract, &digest)
            .await?;
        if !document.has_guide(&summary.contract) || document.contracts[0].major != summary.major {
            return Err(invalid_metadata());
        }
        let parsed = tokio::task::spawn_blocking(move || {
            let value = serde_json::json!({
                "schemaVersion":1,"$defs":document.definitions,"contracts":document.contracts
            });
            parse_document(&value.to_string())
        })
        .await
        .map_err(|_| invalid_metadata())?
        .map_err(|_| invalid_metadata())?;
        contracts.extend(parsed);
    }
    Ok(contracts)
}

/// The UI contract and per-turn snapshot accept at most 32 identities. Fetch through the
/// SDK's validated pages and stop before accumulating more, even for a changing Host catalog.
pub async fn summaries(
    ports: &teamarness_package_sdk::ports::PortClient,
) -> Result<Vec<teamarness_package_sdk::ports::ContractSummary>, teamarness_package_sdk::BackendError>
{
    use teamarness_package_sdk::ports::PortAccess;
    let mut rows: Vec<teamarness_package_sdk::ports::ContractSummary> = Vec::new();
    let mut cursor = None::<String>;
    let mut cursors = BTreeSet::new();
    loop {
        let page = ports
            .available(PortAccess::Agent, cursor.as_deref())
            .await?;
        let entries: Vec<teamarness_package_sdk::ports::ContractSummary> =
            serde_json::from_value(page["contracts"].clone()).map_err(|_| invalid_metadata())?;
        if rows.len() + entries.len() > 32
            || (entries.is_empty() && page.get("nextCursor").is_some())
        {
            return Err(invalid_metadata());
        }
        for entry in entries {
            if rows
                .last()
                .is_some_and(|last| last.contract >= entry.contract)
            {
                return Err(invalid_metadata());
            }
            rows.push(entry);
        }
        let Some(next) = page.get("nextCursor").and_then(Value::as_str) else {
            return Ok(rows);
        };
        if rows.len() == 32 || !cursors.insert(next.to_owned()) {
            return Err(invalid_metadata());
        }
        cursor = Some(next.to_owned());
    }
}

fn invalid_metadata() -> teamarness_package_sdk::BackendError {
    teamarness_package_sdk::BackendError::new(
        "agent.invalid_metadata",
        false,
        serde_json::json!({}),
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn find<'a>(catalog: &'a impl Catalog, identity: &str) -> &'a CatalogContract {
        catalog
            .contracts()
            .iter()
            .find(|contract| contract.identity == identity)
            .unwrap_or_else(|| panic!("{identity} missing"))
    }

    fn ports(contract: &CatalogContract) -> Vec<(&str, PortKind)> {
        contract
            .ports
            .iter()
            .map(|port| (port.name.as_str(), port.kind))
            .collect()
    }

    #[test]
    fn bundled_catalog_parses() {
        let catalog = BundledCatalog::load().expect("bundle parses");
        let identities: Vec<_> = catalog
            .contracts()
            .iter()
            .map(|contract| (contract.identity.as_str(), contract.major, contract.minor))
            .collect();
        assert_eq!(
            identities,
            [
                ("dev.teamarness.conversations", 1, 2),
                ("dev.teamarness.documents", 1, 1),
                ("dev.teamarness.exhibits", 1, 0),
            ]
        );

        let conversations = find(&catalog, "dev.teamarness.conversations");
        assert_eq!(
            ports(conversations),
            [
                ("message.post", PortKind::Command),
                ("message.page", PortKind::Query),
                ("messages.changed", PortKind::Event),
                ("message.changes", PortKind::Query),
                ("channel.list", PortKind::Query),
            ]
        );
        let changed = &conversations.ports[2];
        assert!(changed.request.is_none() && changed.response.is_none());

        let exhibits = find(&catalog, "dev.teamarness.exhibits");
        assert_eq!(alias(&exhibits.identity), "exhibits");
        assert_eq!(exhibits.ports.len(), 8);
        assert!(exhibits.defs.contains_key("sha256"));
        assert!(exhibits.description.starts_with("Author Exhibits"));
        for port in &exhibits.ports {
            assert!(
                port.request.is_some() && port.response.is_some(),
                "{}",
                port.name
            );
        }

        // Parsed once and shared.
        assert!(std::ptr::eq(BundledCatalog::get(), BundledCatalog::get()));
        assert_eq!(BundledCatalog::get().contracts(), catalog.contracts());
    }

    #[test]
    fn only_exhibits_has_a_guide_in_the_bundle() {
        let guided: Vec<_> = BundledCatalog::get()
            .contracts()
            .iter()
            .filter(|contract| contract.has_guide)
            .map(|contract| contract.identity.as_str())
            .collect();
        assert_eq!(guided, ["dev.teamarness.exhibits"]);
    }

    #[test]
    fn fixture_contract_with_and_without_guide() {
        let request = json!({"type": "object", "additionalProperties": false});
        let document = json!({
            "schemaVersion": 1,
            "$defs": {"id": {"type": "string"}},
            "contracts": [
                {"identity": "dev.example.guided", "major": 1, "minor": 3, "description": "Guided.",
                 "ports": [
                    {"name": "runtime.guide", "kind": "query", "description": "Read first.",
                     "request": request, "response": {"type": "object"}},
                    {"name": "thing.changed", "kind": "event", "description": "Changed.",
                     "event": {"type": "object"}}
                 ]},
                {"identity": "dev.example.plain", "major": 2, "minor": 0, "description": "Plain.",
                 "ports": [
                    {"name": "thing.list", "kind": "query", "description": "List.",
                     "request": request, "response": {"type": "object"}}
                 ]},
                {"identity": "dev.example.commanded", "major": 1, "minor": 0, "description": "No.",
                 "ports": [
                    {"name": "runtime.guide", "kind": "command", "description": "Not a query.",
                     "request": request, "response": {"type": "object"}}
                 ]}
            ]
        });
        let contracts = parse_document(&document.to_string()).expect("fixture parses");
        let flags: Vec<_> = contracts
            .iter()
            .map(|contract| (contract.identity.as_str(), contract.has_guide))
            .collect();
        assert_eq!(
            flags,
            [
                ("dev.example.guided", true),
                ("dev.example.plain", false),
                ("dev.example.commanded", false),
            ]
        );
        assert_eq!(contracts[0].defs["id"], json!({"type": "string"}));
        assert_eq!(contracts[0].ports[1].kind, PortKind::Event);
        assert_eq!(find(&contracts, "dev.example.plain").major, 2);

        let mut repeated = document.clone();
        repeated["contracts"][1]["identity"] = json!("dev.example.guided");
        assert!(parse_document(&repeated.to_string()).is_err());
        let mut no_request = document.clone();
        no_request["contracts"][1]["ports"][0]
            .as_object_mut()
            .unwrap()
            .remove("request");
        assert!(parse_document(&no_request.to_string()).is_err());
        let mut version = document;
        version["schemaVersion"] = json!(2);
        assert!(parse_document(&version.to_string()).is_err());
    }
}
