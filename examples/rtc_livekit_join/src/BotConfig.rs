use anyhow::{anyhow, Context};

#[derive(Debug, Clone)]
pub struct Config {
    pub room_in_id: String,
    pub room_out_id: String,
    pub webserver_send: String,
    pub webserver_spawn: String,
    pub secret_key: Option<String>,
    pub echo_commands: bool,
    pub on_mention_only: bool,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            room_in_id: required_env("BOT_ROOM_IN_ID")?,
            room_out_id: required_env("BOT_ROOM_OUT_ID")?,
            webserver_send: required_env("BOT_WEBSERVER_SEND")?,
            webserver_spawn: required_env("BOT_WEBSERVER_SPAWN")?,
            secret_key: optional_env("MATRIX_SECRET_KEY"),
            echo_commands: bool_env("BOT_ECHO_COMMANDS"),
            on_mention_only: bool_env("BOT_ON_MENTION_ONLY"),
        })
    }
}

fn required_env(name: &str) -> anyhow::Result<String> {
    std::env::var(name).with_context(|| anyhow!("missing required env var: {name}"))
}

fn optional_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn bool_env(name: &str) -> bool {
    optional_env(name).is_some_and(|value| {
        matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
    })
}
