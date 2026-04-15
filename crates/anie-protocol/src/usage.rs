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
