//! The single place Package enablement is decided.
//!
//! A contract is offered to the model ([`usable`]) only when it is agent-facing (its catalog
//! entry has `runtime.guide`), the Host reports it declared and available, and the Steward has
//! enabled it. Ports withheld in this slice are removed. [`listed`] is the Steward's view of the
//! same facts: every agent-facing contract, whatever its availability or enablement.
//!
//! "Declared" is read from the Host: the package-ports `available` answer lists only contracts
//! this Package's manifest declares, so a contract missing from it is neither declared nor
//! available. `manifest_and_catalog_agree` keeps the manifest and the bundled catalog equal.

use serde::Serialize;
use serde_json::Value;
use teamarness_package_sdk::BackendError;

use crate::catalog::{Catalog, CatalogContract, alias};
use crate::tools::UsableContract;

/// The UI asked to enable or disable a contract that is not agent-facing. Not retryable.
pub const UNKNOWN_PACKAGE: &str = "agent.unknown_package";

/// Ports never offered to the model, as `(contract, port)`. The harness posts replies itself.
pub const WITHHELD: [(&str, &str); 1] = [("dev.teamarness.conversations", "message.post")];

/// The contracts the Host reports available, by identity and major.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Availability {
    available: Vec<(String, Option<u64>)>,
}

impl Availability {
    /// Nothing is available: no Port grant, or the Host could not answer.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Reads the package-ports `available` answer:
    /// `{"contracts": [{"contract", "major", "minor", "available", "ports"}]}`.
    ///
    /// Only entries with a string `contract` and `available: true` count; anything else in the
    /// answer is ignored, so an unexpected shape makes contracts unavailable, never usable.
    #[must_use]
    pub fn parse(answer: &Value) -> Self {
        let available = answer
            .get("contracts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|entry| entry.get("available") == Some(&Value::Bool(true)))
            .filter_map(|entry| {
                let contract = entry.get("contract")?.as_str()?;
                Some((
                    contract.to_owned(),
                    entry.get("major").and_then(Value::as_u64),
                ))
            })
            .collect();
        Self { available }
    }

    /// The outcome of `PortClient::available()`: an error means every contract is unavailable.
    #[must_use]
    pub fn from_result(result: &Result<Value, BackendError>) -> Self {
        result.as_ref().map_or_else(|_| Self::none(), Self::parse)
    }

    /// Whether the Host reports `contract` available at its catalog major.
    #[must_use]
    pub fn includes(&self, contract: &CatalogContract) -> bool {
        self.available.iter().any(|(identity, major)| {
            *identity == contract.identity
                && major.is_none_or(|major| major == u64::from(contract.major))
        })
    }
}

/// One agent-facing contract as the Steward sees it. The exact UI-contract `packageView` shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageView {
    pub contract: String,
    pub name: String,
    pub description: String,
    pub available: bool,
    pub enabled: bool,
}

/// A contract's display name: the title-cased last identity segment
/// (`dev.teamarness.exhibits` → `Exhibits`, `com.example.field-notes` → `Field Notes`).
#[must_use]
pub fn display_name(identity: &str) -> String {
    alias(identity)
        .split(['-', '_'])
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut characters = word.chars();
            characters.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(characters).collect()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether `identity` names an agent-facing contract the catalog knows.
#[must_use]
pub fn is_agent_facing(catalog: &(impl Catalog + ?Sized), identity: &str) -> bool {
    agent_facing(catalog).any(|contract| contract.identity == identity)
}

fn agent_facing(catalog: &(impl Catalog + ?Sized)) -> impl Iterator<Item = &CatalogContract> {
    catalog
        .contracts()
        .iter()
        .filter(|contract| contract.has_guide)
}

fn is_enabled(enabled: &[String], contract: &CatalogContract) -> bool {
    enabled.contains(&contract.identity)
}

fn is_withheld(contract: &str, port: &str) -> bool {
    WITHHELD.contains(&(contract, port))
}

/// The contracts the model may use now, with withheld ports removed.
#[must_use]
pub fn usable(
    catalog: &(impl Catalog + ?Sized),
    availability: &Availability,
    enabled: &[String],
) -> Vec<UsableContract> {
    agent_facing(catalog)
        .filter(|contract| availability.includes(contract) && is_enabled(enabled, contract))
        .map(|contract| {
            let mut usable = UsableContract::from(contract);
            usable
                .ports
                .retain(|port| !is_withheld(&contract.identity, &port.name));
            usable
        })
        .collect()
}

/// Every agent-facing contract, in catalog order, with its availability and enablement.
#[must_use]
pub fn listed(
    catalog: &(impl Catalog + ?Sized),
    availability: &Availability,
    enabled: &[String],
) -> Vec<PackageView> {
    agent_facing(catalog)
        .map(|contract| PackageView {
            contract: contract.identity.clone(),
            name: display_name(&contract.identity),
            description: contract.description.clone(),
            available: availability.includes(contract),
            enabled: is_enabled(enabled, contract),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, json};

    use super::*;
    use crate::catalog::{BundledCatalog, CatalogPort, GUIDE_PORT, PortKind};

    fn port(name: &str, kind: PortKind) -> CatalogPort {
        CatalogPort {
            name: name.into(),
            kind,
            description: format!("{name} port"),
            request: (kind != PortKind::Event).then(|| json!({"type": "object"})),
            response: (kind != PortKind::Event).then(|| json!({"type": "object"})),
        }
    }

    fn contract(identity: &str, guide: bool, ports: &[(&str, PortKind)]) -> CatalogContract {
        let mut ports: Vec<CatalogPort> =
            ports.iter().map(|(name, kind)| port(name, *kind)).collect();
        if guide {
            ports.push(port(GUIDE_PORT, PortKind::Query));
        }
        CatalogContract {
            identity: identity.into(),
            major: 1,
            minor: 0,
            description: format!("{identity} contract"),
            ports,
            has_guide: guide,
            defs: Map::new(),
        }
    }

    fn fixture() -> Vec<CatalogContract> {
        vec![
            contract(
                "dev.teamarness.conversations",
                true,
                &[
                    ("message.post", PortKind::Command),
                    ("message.list", PortKind::Query),
                ],
            ),
            contract(
                "com.example.guided",
                true,
                &[("item.list", PortKind::Query)],
            ),
            contract(
                "com.example.unguided",
                false,
                &[("item.list", PortKind::Query)],
            ),
            contract(
                "com.example.offline",
                true,
                &[("item.list", PortKind::Query)],
            ),
            contract(
                "com.example.undeclared",
                true,
                &[("item.list", PortKind::Query)],
            ),
        ]
    }

    fn answer(entries: &[(&str, u64, bool)]) -> Value {
        json!({
            "contracts": entries
                .iter()
                .map(|(contract, major, available)| json!({
                    "contract": contract, "major": major, "minor": 0,
                    "available": available, "ports": [],
                }))
                .collect::<Vec<_>>(),
        })
    }

    fn all(identities: &[&str]) -> Vec<String> {
        identities
            .iter()
            .map(|&identity| identity.to_owned())
            .collect()
    }

    fn identities(usable: &[UsableContract]) -> Vec<&str> {
        usable
            .iter()
            .map(|contract| contract.identity.as_str())
            .collect()
    }

    #[test]
    fn usable_requires_guide_declared_available_enabled() {
        let catalog = fixture();
        let availability = Availability::parse(&answer(&[
            ("dev.teamarness.conversations", 1, true),
            ("com.example.guided", 1, true),
            ("com.example.unguided", 1, true),
            ("com.example.offline", 1, false),
        ]));
        let everything = all(&[
            "dev.teamarness.conversations",
            "com.example.guided",
            "com.example.unguided",
            "com.example.offline",
            "com.example.undeclared",
        ]);
        assert_eq!(
            identities(&usable(&catalog, &availability, &everything)),
            ["dev.teamarness.conversations", "com.example.guided"]
        );
        // Not enabled → not usable.
        assert_eq!(
            identities(&usable(
                &catalog,
                &availability,
                &all(&["com.example.guided"])
            )),
            ["com.example.guided"]
        );
        assert!(usable(&catalog, &availability, &[]).is_empty());
        // Another major is not this contract.
        let other_major = Availability::parse(&answer(&[("com.example.guided", 2, true)]));
        assert!(usable(&catalog, &other_major, &everything).is_empty());
        // A Host error or an unexpected answer makes nothing usable.
        let failed =
            Availability::from_result(&Err(BackendError::new("port_unavailable", true, json!({}))));
        assert!(usable(&catalog, &failed, &everything).is_empty());
        for odd in [
            json!(null),
            json!([]),
            json!({"contracts": {}}),
            json!({"contracts": [{"contract": "com.example.guided", "available": "yes"}]}),
            json!({"contracts": [{"contract": 7, "available": true}]}),
        ] {
            assert!(
                usable(&catalog, &Availability::parse(&odd), &everything).is_empty(),
                "{odd}"
            );
        }
        // An entry without a major still counts; a future Host may omit it.
        let no_major = Availability::parse(
            &json!({"contracts": [{"contract": "com.example.guided", "available": true}]}),
        );
        assert_eq!(
            identities(&usable(&catalog, &no_major, &everything)),
            ["com.example.guided"]
        );
    }

    #[test]
    fn message_post_is_withheld() {
        let catalog = fixture();
        let availability =
            Availability::parse(&answer(&[("dev.teamarness.conversations", 1, true)]));
        let usable = usable(
            &catalog,
            &availability,
            &all(&["dev.teamarness.conversations"]),
        );
        let ports: Vec<&str> = usable[0]
            .ports
            .iter()
            .map(|port| port.name.as_str())
            .collect();
        assert_eq!(ports, ["message.list", GUIDE_PORT]);
        assert_eq!(
            usable[0].description,
            "dev.teamarness.conversations contract"
        );
    }

    #[test]
    fn listed_includes_unavailable_agent_facing_only() {
        let catalog = fixture();
        let availability = Availability::parse(&answer(&[
            ("com.example.guided", 1, true),
            ("com.example.unguided", 1, true),
            ("com.example.offline", 1, false),
        ]));
        let views = listed(
            &catalog,
            &availability,
            &all(&["com.example.offline", "com.example.gone"]),
        );
        let rows: Vec<(&str, &str, bool, bool)> = views
            .iter()
            .map(|view| {
                (
                    view.contract.as_str(),
                    view.name.as_str(),
                    view.available,
                    view.enabled,
                )
            })
            .collect();
        assert_eq!(
            rows,
            [
                (
                    "dev.teamarness.conversations",
                    "Conversations",
                    false,
                    false
                ),
                ("com.example.guided", "Guided", true, false),
                ("com.example.offline", "Offline", false, true),
                ("com.example.undeclared", "Undeclared", false, false),
            ]
        );
        assert_eq!(views[1].description, "com.example.guided contract");
        assert!(is_agent_facing(&catalog, "com.example.guided"));
        assert!(!is_agent_facing(&catalog, "com.example.unguided"));
        assert!(!is_agent_facing(&catalog, "com.example.gone"));

        // The bundle lists Exhibits only, unavailable when the Host cannot answer.
        let bundled = listed(BundledCatalog::get(), &Availability::none(), &[]);
        assert_eq!(
            bundled,
            [PackageView {
                contract: "dev.teamarness.exhibits".into(),
                name: "Exhibits".into(),
                description: BundledCatalog::get()
                    .contracts()
                    .iter()
                    .find(|contract| contract.identity == "dev.teamarness.exhibits")
                    .unwrap()
                    .description
                    .clone(),
                available: false,
                enabled: false,
            }]
        );
    }

    #[test]
    fn display_names_title_case_the_last_segment() {
        assert_eq!(display_name("dev.teamarness.exhibits"), "Exhibits");
        assert_eq!(display_name("com.example.field-notes"), "Field Notes");
        assert_eq!(display_name("com.example.field_notes"), "Field Notes");
        assert_eq!(display_name("plain"), "Plain");
        assert_eq!(alias("dev.teamarness.exhibits"), "exhibits");
    }
}
