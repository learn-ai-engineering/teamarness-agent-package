use crate::ProtocolError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

impl ProtocolVersion {
    pub const fn new(major: u16, minor: u16) -> Self {
        Self { major, minor }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProtocolRange {
    pub major: u16,
    pub min_minor: u16,
    pub max_minor: u16,
}

impl ProtocolRange {
    pub fn new(major: u16, min_minor: u16, max_minor: u16) -> Result<Self, ProtocolError> {
        if major == 0 || min_minor > max_minor {
            return Err(ProtocolError::new("invalid_version_range"));
        }
        Ok(Self {
            major,
            min_minor,
            max_minor,
        })
    }
}

pub fn negotiate_version(
    host: ProtocolRange,
    package: ProtocolRange,
) -> Result<ProtocolVersion, ProtocolError> {
    if host.major != package.major {
        return Err(ProtocolError::new("incompatible_version"));
    }
    let min = host.min_minor.max(package.min_minor);
    let max = host.max_minor.min(package.max_minor);
    if min > max {
        return Err(ProtocolError::new("incompatible_version"));
    }
    Ok(ProtocolVersion::new(host.major, max))
}
