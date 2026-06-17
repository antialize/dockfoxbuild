//! A module for parsing human-readable size strings (e.g., "100GB", "512MB") into a structured `Size` type.

use std::ops::Deref;

use serde::Deserialize;

/// A simple struct representing a size in bytes, with parsing from human-readable formats.
#[derive(Clone, Debug, Default)]
pub struct Size(pub u64);

impl Deref for Size {
    type Target = u64;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::str::FromStr for Size {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if let Some(v) = s.strip_suffix("KB") {
            Ok(Size((v.trim().parse::<f64>()? * 1024.0) as u64))
        } else if let Some(v) = s.strip_suffix("MB") {
            Ok(Size((v.trim().parse::<f64>()? * 1024.0 * 1024.0) as u64))
        } else if let Some(v) = s.strip_suffix("GB") {
            Ok(Size(
                (v.trim().parse::<f64>()? * 1024.0 * 1024.0 * 1024.0) as u64,
            ))
        } else {
            Ok(Size(s.parse::<u64>()?))
        }
    }
}

impl std::fmt::Display for Size {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.is_multiple_of(1024 * 1024 * 1024) {
            write!(f, "{}GB", self.0 / (1024 * 1024 * 1024))
        } else if self.0.is_multiple_of(1024 * 1024) {
            write!(f, "{}MB", self.0 / (1024 * 1024))
        } else if self.0.is_multiple_of(1024) {
            write!(f, "{}KB", self.0 / 1024)
        } else {
            write!(f, "{}B", self.0)
        }
    }
}

impl<'de> Deserialize<'de> for Size {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}
