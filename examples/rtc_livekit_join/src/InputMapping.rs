use anyhow::Context;
use once_cell::sync::Lazy;
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Deserialize)]
pub struct MappingEntry {
    pub object: String,
    pub order: String,
}

#[derive(Debug, Deserialize)]
struct MappingFile {
    mappings: HashMap<String, MappingEntry>,
}

pub static INPUTMAPPING: Lazy<HashMap<String, MappingEntry>> = Lazy::new(|| {
    let json_str = std::env::var("BOT_INPUT_MAPPING_JSON")
        .expect("missing required env var: BOT_INPUT_MAPPING_JSON");
    let parsed: MappingFile =
        serde_json::from_str(&json_str).expect("Failed to parse BOT_INPUT_MAPPING_JSON");
    parsed.mappings
});

pub fn validate_input_mapping_from_env() -> anyhow::Result<()> {
    let json_str = std::env::var("BOT_INPUT_MAPPING_JSON")
        .context("missing required env var: BOT_INPUT_MAPPING_JSON")?;
    let _: MappingFile =
        serde_json::from_str(&json_str).context("parse BOT_INPUT_MAPPING_JSON as mapping file")?;
    Ok(())
}

pub fn get_object(input: &str) -> Option<&str> {
    INPUTMAPPING.get(input).map(|entry| entry.object.as_str())
}

pub fn get_order(input: &str) -> Option<&str> {
    INPUTMAPPING.get(input).map(|entry| entry.order.as_str())
}
