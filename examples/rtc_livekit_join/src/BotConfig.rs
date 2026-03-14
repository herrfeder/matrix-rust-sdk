use once_cell::sync::Lazy;
use serde::Deserialize;
use std::fs;
use std::sync::OnceLock;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub home_server: String,
    pub username: String,
    pub password: String,
    pub secretkey: String,
    pub room_in_id: String,
    pub room_out_id: String,
    pub webserver_send: String,
    pub webserver_spawn: String,
    pub echo_commands: bool,
    pub on_mention_only: bool
}

// Lazy static instance
pub static BOTCONFIG: Lazy<Config> = Lazy::new(|| {
    let data = fs::read_to_string("botconfig.json")
        .expect("Failed to read strings.json");
    serde_json::from_str(&data)
        .expect("Failed to parse strings.json")
});