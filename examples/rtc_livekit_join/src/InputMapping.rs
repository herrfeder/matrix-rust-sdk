use once_cell::sync::Lazy;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;

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
    let json_str = fs::read_to_string("inputmapping.json").expect("Failed to read JSON file");
    let parsed: MappingFile =
        serde_json::from_str(&json_str).expect("Failed to parse JSON into mapping structure");
    parsed.mappings
});

pub fn get_object(input: &str) -> Option<&str> {
    INPUTMAPPING.get(input).map(|entry| entry.object.as_str())
}

pub fn get_order(input: &str) -> Option<&str> {
    INPUTMAPPING.get(input).map(|entry| entry.order.as_str())
}
