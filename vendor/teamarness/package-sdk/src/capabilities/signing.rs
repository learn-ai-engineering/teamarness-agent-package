//! The Signing capability: the Installation's own signature over a Package-scoped envelope.

use teamarness_package_protocol::JsonValue;

use super::{
    bool_field, encode_base64, field, grant_for, invalid_request, invalid_response, is_lower_hex,
    string_field,
};
use crate::{BackendError, CapabilityClient, Initialization, object};

/// The capability identity a manifest requires.
pub const IDENTITY: &str = "teamarness.signing";

/// The largest payload the contract signs, decoded.
const MAX_PAYLOAD_BYTES: usize = 262_144;

/// One signature over a Package-scoped envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub signer: String,
    pub envelope_version: u64,
    /// The base64url signature, as the contract carries it.
    pub signature: String,
}

/// A typed client for `teamarness.signing`.
#[derive(Clone)]
pub struct Signing {
    client: CapabilityClient,
    grant: String,
}

impl Signing {
    /// Binds the client to the grant the host gave this generation.
    ///
    /// # Errors
    ///
    /// Returns `package.capability_not_granted` when the manifest did not require it.
    pub fn from_initialization(
        initialization: &Initialization,
        client: CapabilityClient,
    ) -> Result<Self, BackendError> {
        Ok(Self {
            grant: grant_for(initialization, IDENTITY)?,
            client,
        })
    }

    /// Signs a payload under a Package-chosen purpose.
    ///
    /// # Errors
    ///
    /// Returns `package.invalid_capability_request` for a purpose or payload the contract
    /// refuses, the host's capability error, or `package.invalid_capability_response`.
    pub async fn sign(&self, purpose: &str, payload: &[u8]) -> Result<Signature, BackendError> {
        check_purpose(purpose)?;
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(invalid_request());
        }
        let answer = self
            .client
            .call(
                &self.grant,
                "sign",
                object([
                    ("purpose", JsonValue::String(purpose.to_owned())),
                    ("payload", JsonValue::String(encode_base64(payload))),
                ]),
            )
            .await?;
        let signer = string_field(&answer, "signer")?;
        if !is_lower_hex(&signer, 64) {
            return Err(invalid_response());
        }
        Ok(Signature {
            signer,
            envelope_version: field(&answer, "envelopeVersion")?
                .as_u64()
                .ok_or_else(invalid_response)?,
            signature: string_field(&answer, "signature")?,
        })
    }

    /// Verifies a signature another Installation made over the same envelope.
    ///
    /// # Errors
    ///
    /// Returns `package.invalid_capability_request` for arguments the contract refuses, the
    /// host's capability error, or `package.invalid_capability_response`.
    pub async fn verify(
        &self,
        signer: &str,
        purpose: &str,
        payload: &[u8],
        signature: &str,
    ) -> Result<bool, BackendError> {
        check_purpose(purpose)?;
        if !is_lower_hex(signer, 64) || signature.len() != 86 || payload.len() > MAX_PAYLOAD_BYTES {
            return Err(invalid_request());
        }
        let answer = self
            .client
            .call(
                &self.grant,
                "verify",
                object([
                    ("signer", JsonValue::String(signer.to_owned())),
                    ("purpose", JsonValue::String(purpose.to_owned())),
                    ("payload", JsonValue::String(encode_base64(payload))),
                    ("signature", JsonValue::String(signature.to_owned())),
                ]),
            )
            .await?;
        bool_field(&answer, "valid")
    }
}

/// The contract's purpose grammar: 1 to 64 printable ASCII characters.
fn check_purpose(purpose: &str) -> Result<(), BackendError> {
    if (1..=64).contains(&purpose.len())
        && purpose.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
    {
        Ok(())
    } else {
        Err(invalid_request())
    }
}
