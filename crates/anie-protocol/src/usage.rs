use serde::{Deserialize, Serialize};

/// Token and billing usage reported by a provider.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    /// Input tokens consumed.
    pub input_tokens: u64,
    /// Output tokens consumed.
    pub output_tokens: u64,
    /// Cached prompt tokens read.
    pub cache_read_tokens: u64,
    /// Cached prompt tokens written.
    pub cache_write_tokens: u64,
    /// Total tokens, if directly reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    /// Cost information.
    #[serde(default)]
    pub cost: Cost,
}

/// Monetary cost information associated with a request.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Cost {
    /// Input token cost.
    pub input: f64,
    /// Output token cost.
    pub output: f64,
    /// Cache read cost.
    pub cache_read: f64,
    /// Cache write cost.
    pub cache_write: f64,
    /// Total cost.
    pub total: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_with_populated_cost_roundtrips_through_session_schema_v4() {
        // An older entry written before cost was populated has either no
        // `cost` key or an all-zero one; both must deserialize with a
        // defaulted Cost and never error (no schema bump). And a usage
        // carrying a real cost round-trips byte-for-byte.
        let legacy: Usage = serde_json::from_str(
            r#"{"input_tokens":10,"output_tokens":20,"cache_read_tokens":0,"cache_write_tokens":0}"#,
        )
        .expect("legacy entry without a cost field");
        assert_eq!(legacy.cost, Cost::default());
        assert_eq!(legacy.input_tokens, 10);

        let mut priced = Usage {
            input_tokens: 100,
            output_tokens: 50,
            ..Usage::default()
        };
        priced.cost = Cost {
            input: 0.0003,
            output: 0.00075,
            cache_read: 0.0,
            cache_write: 0.0,
            total: 0.00105,
        };
        let json = serde_json::to_string(&priced).expect("serialize");
        let back: Usage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(priced, back);
    }
}
