//! The Package author's validator: the Package Host's admission verdict without the application.
//!
//! ```text
//! teamarness-package-validate source <project-root> [--json]
//! teamarness-package-validate directory <package-directory> [--target <triple>] [--smoke] [--json]
//! ```
//!
//! Exit `0` when the Package passes, `1` when a rule fails, `2` for a usage error.

use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use teamarness_package_contract::manifest::{
    MANIFEST_NAME, load_package_directory, parse_manifest,
};
use teamarness_package_contract::port_contract::compile_port_contracts;
use teamarness_package_contract::smoke::smoke;
use teamarness_package_contract::ui_contract::compile_ui_contract;
use teamarness_package_contract::{ContractError, ContractErrorCode};

const DEFAULT_TARGET: &str = "aarch64-apple-darwin";
const SOURCE_CONTRACT_NAME: &str = "ui-contract.json";
const ASSEMBLER_CONTRACT_PATH: &str = "ui/contract.json";

const USAGE: &str = "usage:\n  \
teamarness-package-validate source <project-root> [--json]\n  \
teamarness-package-validate directory <package-directory> [--target <triple>] [--smoke] [--json]";

struct Reported {
    file: String,
    error: ContractError,
}

#[derive(Default)]
struct Report {
    errors: Vec<Reported>,
    notes: Vec<String>,
}

impl Report {
    fn push(&mut self, file: &str, error: ContractError) {
        self.errors.push(Reported {
            file: file.to_owned(),
            error,
        });
    }

    fn render(&self, json: bool) -> String {
        if json {
            let errors: Vec<_> = self
                .errors
                .iter()
                .map(|reported| {
                    serde_json::json!({
                        "code": reported.error.code.as_str(),
                        "rule": reported.error.rule,
                        "file": reported.file,
                        "pointer": reported.error.pointer,
                    })
                })
                .collect();
            return serde_json::to_string_pretty(&serde_json::json!({
                "ok": self.errors.is_empty(),
                "errors": errors,
                "notes": self.notes,
            }))
            .unwrap_or_else(|_| "{\"ok\":false}".to_owned());
        }
        let mut text = String::new();
        for note in &self.notes {
            let _ = writeln!(text, "note: {note}");
        }
        for reported in &self.errors {
            let _ = write!(
                text,
                "error[{}/{}] {}",
                reported.error.code.as_str(),
                reported.error.rule,
                reported.file
            );
            if let Some(pointer) = &reported.error.pointer {
                let _ = write!(text, "#{pointer}");
            }
            text.push('\n');
        }
        if self.errors.is_empty() {
            text.push_str("ok\n");
        } else {
            let _ = writeln!(text, "{} error(s)", self.errors.len());
        }
        text
    }
}

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match run(&arguments) {
        Ok(report) => {
            print!("{}", report.render(arguments.iter().any(|a| a == "--json")));
            if report.errors.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(message) => {
            eprintln!("{message}\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn run(arguments: &[String]) -> Result<Report, String> {
    let mut mode = None;
    let mut root = None;
    let mut target = DEFAULT_TARGET.to_owned();
    let mut smoke_requested = false;
    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].as_str();
        match argument {
            "--json" => {}
            "--smoke" => smoke_requested = true,
            "--target" => {
                index += 1;
                target = arguments
                    .get(index)
                    .cloned()
                    .ok_or_else(|| "--target needs a value".to_owned())?;
            }
            "source" | "directory" if mode.is_none() => mode = Some(argument.to_owned()),
            _ if argument.starts_with('-') => {
                return Err(format!("unknown option {argument}"));
            }
            _ if root.is_none() && mode.is_some() => root = Some(PathBuf::from(argument)),
            _ => return Err(format!("unexpected argument {argument}")),
        }
        index += 1;
    }
    let (Some(mode), Some(root)) = (mode, root) else {
        return Err("a mode and a path are required".to_owned());
    };
    if mode == "source" {
        if smoke_requested {
            return Err("--smoke needs directory mode".to_owned());
        }
        Ok(validate_source(&root))
    } else {
        Ok(validate_directory(&root, &target, smoke_requested))
    }
}

fn read_source_port(root: &Path, relative: &str) -> std::io::Result<Vec<u8>> {
    let mut path = root.to_path_buf();
    // parse_manifest already checked normalization. Match directory Admission's
    // no-symlink policy for every declared path component in source mode too.
    for part in relative.split('/') {
        path.push(part);
        if std::fs::symlink_metadata(&path)?.file_type().is_symlink() {
            return Err(std::io::Error::other("port contract symlink"));
        }
    }
    let file = std::fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("port contract not a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn validate_source(root: &Path) -> Report {
    let mut report = Report::default();
    match std::fs::read(root.join(MANIFEST_NAME)) {
        Err(_) => report.push(
            MANIFEST_NAME,
            ContractError::new(ContractErrorCode::ManifestMalformed, "manifest.unreadable"),
        ),
        Ok(bytes) => match parse_manifest(&bytes) {
            Err(error) => report.push(MANIFEST_NAME, error),
            Ok(document) => {
                if let Some(path) = document.port_contract_path() {
                    let bytes = read_source_port(root, path);
                    match bytes {
                        Ok(bytes) => match compile_port_contracts(&bytes) {
                            Ok(compiled) => report.notes.extend(compiled.warnings),
                            Err(error) => report.push(path, error),
                        },
                        Err(_) => report.push(
                            path,
                            ContractError::new(
                                ContractErrorCode::PortContractViolation,
                                "port_contract.missing",
                            ),
                        ),
                    }
                }
                if document.ui_contract_path() != ASSEMBLER_CONTRACT_PATH {
                    report.notes.push(format!(
                        "the manifest declares ui.contract.path {:?}; the standard assembler writes {ASSEMBLER_CONTRACT_PATH:?}",
                        document.ui_contract_path()
                    ));
                }
            }
        },
    }
    match std::fs::read(root.join(SOURCE_CONTRACT_NAME)) {
        Err(_) => report.push(
            SOURCE_CONTRACT_NAME,
            ContractError::new(
                ContractErrorCode::UiContractViolation,
                "ui_contract.missing",
            ),
        ),
        Ok(bytes) => {
            if let Err(error) = compile_ui_contract(&bytes) {
                report.push(SOURCE_CONTRACT_NAME, error);
            }
        }
    }
    report
}

fn validate_directory(root: &Path, target: &str, smoke_requested: bool) -> Report {
    let mut report = Report::default();
    let loaded = match load_package_directory(root, target) {
        Ok(loaded) => loaded,
        Err(error) => {
            report.push(MANIFEST_NAME, error);
            return report;
        }
    };
    if let Err(error) = compile_ui_contract(&loaded.ui.contract.bytes) {
        report.push(&loaded.ui.contract.relative_path.clone(), error);
        return report;
    }
    if let Some(ports) = &loaded.port_contract {
        report.notes.extend(ports.warnings.clone());
    }
    if smoke_requested {
        match smoke(root, &loaded) {
            Ok(notes) => report.notes.extend(notes),
            Err(error) => report.push(&loaded.backend.relative_path.clone(), error),
        }
    }
    report
}
