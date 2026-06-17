//! A module for parsing human-readable duration strings (e.g., "6h", "2d") into a structured `Duration` type.

use std::ops::Deref;

/// A simple struct representing a duration in seconds, with parsing from human-readable formats.
#[derive(Clone, Debug)]
pub struct Duration(pub u64);

impl Deref for Duration {
    type Target = u64;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::str::FromStr for Duration {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if let Some(v) = s.strip_suffix("m") {
            Ok(Duration(v.trim().parse::<u64>()? * 60))
        } else if let Some(v) = s.strip_suffix("h") {
            Ok(Duration(v.trim().parse::<u64>()? * 3600))
        } else if let Some(v) = s.strip_suffix("d") {
            Ok(Duration(v.trim().parse::<u64>()? * 3600 * 24))
        } else if let Some(v) = s.strip_suffix("w") {
            Ok(Duration(v.trim().parse::<u64>()? * 3600 * 24 * 7))
        } else {
            Ok(Duration(s.parse::<u64>()?))
        }
    }
}

impl std::fmt::Display for Duration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.is_multiple_of(3600 * 24 * 7) {
            write!(f, "{}w", self.0 / (3600 * 24 * 7))
        } else if self.0.is_multiple_of(3600 * 24) {
            write!(f, "{}d", self.0 / (3600 * 24))
        } else if self.0.is_multiple_of(3600) {
            write!(f, "{}h", self.0 / 3600)
        } else if self.0.is_multiple_of(60) {
            write!(f, "{}m", self.0 / 60)
        } else {
            write!(f, "{}", self.0)
        }
    }
}
