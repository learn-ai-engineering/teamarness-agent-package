//! The Package admission rules, shared by the Package Host and by the author validator.
//!
//! The Package Host applies these rules when it admits a Package Directory; the
//! `teamarness-package-validate` binary applies the same code to a project source or a built
//! Package Directory, so an author gets the host's verdict without starting the application.
//!
//! Every failure carries a Host-visible [`ContractErrorCode`], a stable author-facing `rule`, and
//! a JSON pointer when one is known. The Host maps only `code`; `rule` and `pointer` are for the
//! command line, so Host diagnostics stay payload-free.

#![forbid(unsafe_code)]

pub mod error;
pub mod identity;
pub mod json;
pub mod manifest;
pub mod media_types;
pub mod port_contract;
pub mod smoke;
pub mod ui_contract;

pub use error::{ContractError, ContractErrorCode};
