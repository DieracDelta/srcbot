use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Output from nix-eval-jobs for a single attribute
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EvalJobOutput {
    /// Attr path (examples: "hello", "python3Packages.requests")
    pub attr: String,
    /// Derivation path (final package drvPath)
    pub drv_path: Option<String>,
    /// Error message if evaluation failed
    pub error: Option<String>,
    /// Extra value from --apply containing intermediate drvPaths
    #[serde(default)]
    pub extra_value: Option<HashMap<String, Option<String>>>,
}

