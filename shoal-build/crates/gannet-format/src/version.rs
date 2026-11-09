//! Format versioning for all durable Gannet files.
//!
//! Rules (normative, mirrored in `docs/storage-format.md`):
//!
//! * A format version is `MAJOR.MINOR`, serialized as the string
//!   `"MAJOR.MINOR"` (e.g. `"1.0"`).
//! * **Same major, minor ≤ ours** → fully understood ([`Compatibility::Full`]).
//! * **Same major, minor > ours** → readable; unknown fields must be
//!   preserved on rewrite, never dropped ([`Compatibility::ForwardCompatible`]).
//! * **Different major** → reject ([`Compatibility::Incompatible`]). A major
//!   bump means the file cannot be safely interpreted by this build.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The format version written by this build for all new durable files.
pub const CURRENT_FORMAT: FormatVersion = FormatVersion { major: 1, minor: 0 };

/// A `MAJOR.MINOR` format version attached to every durable file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FormatVersion {
    pub major: u16,
    pub minor: u16,
}

/// Result of comparing a file's format version against [`CURRENT_FORMAT`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compatibility {
    /// Same major version, minor is at or below ours: fully understood.
    Full,
    /// Same major version but a newer minor: readable, but unknown fields
    /// must be preserved verbatim if the file is rewritten.
    ForwardCompatible,
    /// Different major version: this build must refuse to interpret the file.
    Incompatible,
}

impl FormatVersion {
    pub const fn new(major: u16, minor: u16) -> Self {
        Self { major, minor }
    }

    /// Classify this (file) version against the version this build writes.
    pub fn compatibility_with_current(self) -> Compatibility {
        if self.major != CURRENT_FORMAT.major {
            Compatibility::Incompatible
        } else if self.minor <= CURRENT_FORMAT.minor {
            Compatibility::Full
        } else {
            Compatibility::ForwardCompatible
        }
    }

    /// True if a file with this version may be read at all by this build.
    pub fn is_readable(self) -> bool {
        self.compatibility_with_current() != Compatibility::Incompatible
    }
}

impl fmt::Display for FormatVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// Error returned when a version string is not of the form `"MAJOR.MINOR"`.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid format version {0:?}: expected \"MAJOR.MINOR\" (e.g. \"1.0\")")]
pub struct ParseFormatVersionError(pub String);

impl FromStr for FormatVersion {
    type Err = ParseFormatVersionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split('.').collect();
        if parts.len() != 2 {
            return Err(ParseFormatVersionError(s.to_string()));
        }
        let major = parts[0]
            .parse::<u16>()
            .map_err(|_| ParseFormatVersionError(s.to_string()))?;
        let minor = parts[1]
            .parse::<u16>()
            .map_err(|_| ParseFormatVersionError(s.to_string()))?;
        Ok(FormatVersion { major, minor })
    }
}

impl Serialize for FormatVersion {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for FormatVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_displays() {
        let v: FormatVersion = "1.0".parse().unwrap();
        assert_eq!(v, FormatVersion::new(1, 0));
        assert_eq!(v.to_string(), "1.0");

        let v: FormatVersion = "12.34".parse().unwrap();
        assert_eq!(v, FormatVersion::new(12, 34));
    }

    #[test]
    fn rejects_malformed_versions() {
        for bad in ["", "1", "1.0.0", "a.b", "1.", ".1", "-1.0", "1.0 "] {
            assert!(bad.parse::<FormatVersion>().is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn serde_round_trip_as_string() {
        let v = FormatVersion::new(1, 3);
        let json = serde_json::to_string(&v).unwrap();
        assert_eq!(json, "\"1.3\"");
        let back: FormatVersion = serde_json::from_str(&json).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn compatibility_rules() {
        assert_eq!(
            FormatVersion::new(1, 0).compatibility_with_current(),
            Compatibility::Full
        );
        assert_eq!(
            FormatVersion::new(1, 9).compatibility_with_current(),
            Compatibility::ForwardCompatible
        );
        assert_eq!(
            FormatVersion::new(2, 0).compatibility_with_current(),
            Compatibility::Incompatible
        );
        assert_eq!(
            FormatVersion::new(0, 9).compatibility_with_current(),
            Compatibility::Incompatible
        );
        assert!(FormatVersion::new(1, 99).is_readable());
        assert!(!FormatVersion::new(2, 0).is_readable());
    }
}
