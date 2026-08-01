//! The customer-facing priority-knob vocabulary (design doc: "The priority
//! knob").
//!
//! One enum shared by every carrier of the knob — the typed `zerorouter`
//! request object, the `:suffix` model carrier, the per-key default
//! (`api_keys.default_priority`), and the settled row
//! (`usage_events.priority`) — so a priority cannot mean different things on
//! different surfaces, and the migration-0004 CHECK constraints and this type
//! agree by construction.

use serde::{Deserialize, Serialize};

/// The three positions of the estimate-and-select knob.
///
/// Stage 3a (visibility-only) accepts, records, and echoes the knob; every
/// mode still walks the identity order, with health demotion applied last.
/// Mode-specific ordering arrives with the estimator in stage 3b, and
/// success-mode machinery (validators, escalation) in stage 5a.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    Cost,
    Balanced,
    Success,
}

impl Priority {
    /// Parse one keyword — the model-suffix carrier's segment after the last
    /// `:`, or the text a nullable database column carries. The words are
    /// exactly the enum's serde names, so a priority parsed here and one
    /// deserialized from the typed request object can never disagree.
    #[must_use]
    pub fn from_keyword(keyword: &str) -> Option<Self> {
        match keyword {
            "cost" => Some(Self::Cost),
            "balanced" => Some(Self::Balanced),
            "success" => Some(Self::Success),
            _ => None,
        }
    }

    /// The keyword this priority writes wherever it is stored or echoed.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cost => "cost",
            Self::Balanced => "balanced",
            Self::Success => "success",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keywords_round_trip_through_both_directions() {
        for priority in [Priority::Cost, Priority::Balanced, Priority::Success] {
            assert_eq!(Priority::from_keyword(priority.as_str()), Some(priority));
        }
    }

    #[test]
    fn serde_names_are_the_keywords() {
        for priority in [Priority::Cost, Priority::Balanced, Priority::Success] {
            let wire = serde_json::to_value(priority).expect("priority should serialize");
            assert_eq!(wire, serde_json::json!(priority.as_str()));
            let parsed: Priority =
                serde_json::from_value(wire).expect("own output should deserialize");
            assert_eq!(parsed, priority);
        }
    }

    #[test]
    fn unknown_words_are_refused_not_defaulted() {
        assert_eq!(Priority::from_keyword("fast"), None);
        assert_eq!(Priority::from_keyword("Balanced"), None);
        assert_eq!(Priority::from_keyword(""), None);
        assert!(serde_json::from_value::<Priority>(serde_json::json!("cheap")).is_err());
    }
}
