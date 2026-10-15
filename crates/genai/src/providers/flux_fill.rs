//! FLUX.1 Fill [pro] parameters shared by its three hosts (BFL, fal, Replicate).

use serde_json::{Map, Value, json};

use crate::providers::common::prompt_or_description;

pub const MODELS: &[(&str, &str)] = &[("flux-pro-1.0-fill", "FLUX.1 Fill [pro]")];

pub fn params(prompt: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("prompt".into(), json!(prompt_or_description(prompt)));
    m.insert("steps".into(), json!(50));
    m.insert("guidance".into(), json!(60));
    m.insert("output_format".into(), json!("png"));
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_parameters() {
        let p = params("  a boat ");
        assert_eq!(p["prompt"], "a boat");
        assert_eq!((p["steps"].as_u64(), p["guidance"].as_u64(), p["output_format"].as_str()), (Some(50), Some(60), Some("png")));
        assert_eq!(params("")["prompt"], crate::providers::common::CONTINUE_DESCRIPTION);
    }
}
