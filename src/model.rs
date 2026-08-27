use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SnapshotKind {
    Frequently,
    Hourly,
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

impl SnapshotKind {
    pub const ALL: [Self; 6] = [
        Self::Yearly,
        Self::Monthly,
        Self::Weekly,
        Self::Daily,
        Self::Hourly,
        Self::Frequently,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Frequently => "frequently",
            Self::Hourly => "hourly",
            Self::Daily => "daily",
            Self::Weekly => "weekly",
            Self::Monthly => "monthly",
            Self::Yearly => "yearly",
        }
    }

    pub const fn approximate_period_seconds(self, frequent_period_minutes: u32) -> i64 {
        match self {
            Self::Frequently => frequent_period_minutes as i64 * 60,
            Self::Hourly => 60 * 60,
            Self::Daily => 24 * 60 * 60,
            Self::Weekly => 7 * 24 * 60 * 60,
            Self::Monthly => 31 * 24 * 60 * 60,
            Self::Yearly => 31_557_600, // 365.25 days, matching Sanoid's retention window.
        }
    }
}

impl fmt::Display for SnapshotKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotStrategy {
    Individual,
    RecursiveRoot,
    CoveredByRecursive(String),
}
