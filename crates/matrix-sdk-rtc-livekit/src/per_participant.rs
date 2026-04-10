//! Per-participant E2EE helpers for LiveKit + MatrixRTC.

use std::{sync::Arc, time::Duration};

use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD},
};
use livekit::{
    RoomEvent,
    e2ee::{
        E2eeOptions, EncryptionType,
        key_provider::{KeyDerivationFunction, KeyProvider, KeyProviderOptions},
    },
    id::ParticipantIdentity,
};
use matrix_sdk::{Client, Room, RoomMemberships, event_handler::EventHandlerDropGuard};
use matrix_sdk_base::crypto::CollectStrategy;
use rand::{RngCore, rngs::OsRng};
use ruma::serde::Raw;
use ruma::{OwnedRoomId, events::AnyToDeviceEvent};
use serde::Deserialize;
use thiserror::Error;
use tracing::{info, warn};

use crate::LiveKitRoomOptionsProvider;

/// Runtime context for per-participant LiveKit E2EE.
#[derive(Clone)]
pub struct PerParticipantE2eeContext {
    pub key_provider: Arc<KeyProvider>,
    pub key_index: i32,
    pub local_key: Vec<u8>,
}

/// LiveKit room options provider with optional per-participant E2EE.
#[derive(Clone)]
pub struct E2eeRoomOptionsProvider {
    pub e2ee: Option<PerParticipantE2eeContext>,
}

impl LiveKitRoomOptionsProvider for E2eeRoomOptionsProvider {
    fn room_options(&self) -> livekit::RoomOptions {
        let mut options = livekit::RoomOptions::default();
        if let Some(context) = &self.e2ee {
            options.encryption = Some(E2eeOptions {
                encryption_type: EncryptionType::Gcm,
                key_provider: KeyProvider::clone(context.key_provider.as_ref()),
            });
        }
        options
    }
}

#[derive(Debug, Error)]
pub enum PerParticipantE2eeError {
    #[error("missing device id for per-participant E2EE")]
    MissingDeviceId,
    #[error("missing user id for per-participant E2EE")]
    MissingUserId,
    #[error(transparent)]
    Matrix(#[from] matrix_sdk::Error),
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
}

/// Build the initial per-participant E2EE context for a Matrix room.
pub async fn build_per_participant_e2ee(
    room: &Room,
    _force_backup_download: bool,
    _retries: usize,
    _retry_delay: Duration,
) -> Result<Option<PerParticipantE2eeContext>, PerParticipantE2eeError> {
    let mut key_provider_options = KeyProviderOptions::default();
    key_provider_options.ratchet_window_size = 10;
    key_provider_options.key_ring_size = 256;
    key_provider_options.key_derivation_function = KeyDerivationFunction::HKDF;

    let key_provider = Arc::new(KeyProvider::new(key_provider_options));
    let local_key = derive_per_participant_key();
    send_per_participant_keys(room, 0, &local_key, None).await?;

    Ok(Some(PerParticipantE2eeContext { key_provider, key_index: 0, local_key }))
}

/// Generate local key material for per-participant media E2EE.
pub fn derive_per_participant_key() -> Vec<u8> {
    let mut key = [0u8; 16];
    OsRng.fill_bytes(&mut key);
    key.to_vec()
}

/// Send `io.element.call.encryption_keys` to room members' devices.
pub async fn send_per_participant_keys(
    room: &Room,
    key_index: i32,
    key: &[u8],
    target_device_id: Option<&str>,
) -> Result<(), PerParticipantE2eeError> {
    if key.is_empty() {
        return Ok(());
    }

    let key = if key.len() >= 16 { &key[..16] } else { key };
    let client = room.client();
    let own_device_id =
        client.device_id().ok_or(PerParticipantE2eeError::MissingDeviceId)?.to_owned();
    let own_user_id = client.user_id().map(|id| id.to_owned());

    let members = room.members(RoomMemberships::JOIN).await?;
    let mut recipients = Vec::new();
    for member in members {
        let devices = client.encryption().get_user_devices(member.user_id()).await?;
        for device in devices.devices() {
            if let Some(own_user_id) = own_user_id.as_ref()
                && device.user_id() == own_user_id
                && device.device_id() == &own_device_id
            {
                continue;
            }
            if target_device_id.is_none_or(|target| device.device_id().as_str() == target) {
                recipients.push(device);
            }
        }
    }

    if recipients.is_empty() {
        return Ok(());
    }

    let key_b64 = URL_SAFE_NO_PAD.encode(key);
    let sent_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let own_user_id = client.user_id().ok_or(PerParticipantE2eeError::MissingUserId)?;
    let claimed = own_device_id.as_str();
    let member_id = format!("{own_user_id}:{claimed}");

    let content_raw = Raw::new(&serde_json::json!({
        "keys": { "index": key_index, "key": key_b64 },
        "device_id": claimed,
        "member": { "claimed_device_id": claimed, "id": member_id },
        "room_id": room.room_id().to_string(),
        "session": {
            "application": "m.call",
            "call_id": "",
            "scope": "m.room"
        },
        "sent_ts": sent_ts,
    }))?
    .cast_unchecked();

    let _ = client
        .encryption()
        .encrypt_and_send_raw_to_device(
            recipients.iter().collect(),
            "io.element.call.encryption_keys",
            content_raw,
            CollectStrategy::AllDevices,
        )
        .await?;

    Ok(())
}

/// Register a to-device handler that applies received per-participant keys.
pub fn register_e2ee_to_device_handler(
    client: &Client,
    room_id: OwnedRoomId,
    key_provider: Arc<KeyProvider>,
) -> EventHandlerDropGuard {
    let handle = client.add_event_handler(move |raw: Raw<AnyToDeviceEvent>| {
        let key_provider = Arc::clone(&key_provider);
        let room_id = room_id.clone();
        async move {
            let Ok(event) = raw.deserialize_as::<IncomingKeyToDeviceEvent>() else {
                return;
            };

            if event.event_type != "io.element.call.encryption_keys"
                || event.content.room_id != room_id.as_str()
            {
                return;
            }

            let identity =
                ParticipantIdentity(format!("{}:{}", event.sender, event.content.device_id));
            for key_entry in event.content.keys {
                let key_bytes = STANDARD_NO_PAD
                    .decode(&key_entry.key)
                    .or_else(|_| STANDARD.decode(&key_entry.key))
                    .or_else(|_| URL_SAFE_NO_PAD.decode(&key_entry.key))
                    .or_else(|_| URL_SAFE.decode(&key_entry.key));
                let Ok(key_bytes) = key_bytes else {
                    warn!(
                        %identity,
                        key_index = key_entry.index,
                        "failed to decode per-participant E2EE key"
                    );
                    continue;
                };
                let key_set = key_provider.set_key(&identity, key_entry.index, key_bytes);
                info!(
                    %identity,
                    key_index = key_entry.index,
                    key_set,
                    "installed per-participant E2EE key from to-device event"
                );
            }
        }
    });

    client.event_handler_drop_guard(handle)
}

#[derive(Deserialize)]
struct IncomingKeyToDeviceEvent {
    #[serde(rename = "type")]
    event_type: String,
    sender: String,
    content: IncomingKeyToDeviceContent,
}

#[derive(Deserialize)]
struct IncomingKeyToDeviceContent {
    room_id: String,
    device_id: String,
    #[serde(deserialize_with = "deserialize_key_entries")]
    keys: Vec<IncomingKeyEntry>,
}

#[derive(Deserialize)]
struct IncomingKeyEntry {
    index: i32,
    key: String,
}

fn deserialize_key_entries<'de, D>(deserializer: D) -> Result<Vec<IncomingKeyEntry>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Keys {
        One(IncomingKeyEntry),
        Many(Vec<IncomingKeyEntry>),
    }

    match Keys::deserialize(deserializer)? {
        Keys::One(entry) => Ok(vec![entry]),
        Keys::Many(entries) => Ok(entries),
    }
}

/// Resend keys on selected LiveKit room events.
pub fn spawn_livekit_e2ee_event_resend(
    room: Room,
    mut events: tokio::sync::mpsc::UnboundedReceiver<RoomEvent>,
    context: PerParticipantE2eeContext,
) {
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            match event {
                RoomEvent::Reconnected => {
                    let _ = send_per_participant_keys(
                        &room,
                        context.key_index,
                        &context.local_key,
                        None,
                    )
                    .await;
                }
                RoomEvent::ParticipantConnected(participant)
                | RoomEvent::TrackPublished { participant, .. }
                | RoomEvent::TrackSubscribed { participant, .. } => {
                    let target_device_id = participant
                        .identity()
                        .as_str()
                        .rsplit_once(':')
                        .map(|(_, device_id)| device_id.to_owned());
                    let _ = send_per_participant_keys(
                        &room,
                        context.key_index,
                        &context.local_key,
                        target_device_id.as_deref(),
                    )
                    .await;
                }
                _ => {}
            }
        }
    });
}

/// Read per-participant grace period from an env var.
pub fn per_participant_key_grace_period_from_env(var: &str, default_ms: u64) -> Duration {
    let grace_ms =
        std::env::var(var).ok().and_then(|value| value.parse::<u64>().ok()).unwrap_or(default_ms);
    Duration::from_millis(grace_ms)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        E2eeRoomOptionsProvider, derive_per_participant_key,
        per_participant_key_grace_period_from_env,
    };
    use crate::LiveKitRoomOptionsProvider;

    #[test]
    fn derive_per_participant_key_returns_expected_length() {
        let key = derive_per_participant_key();

        assert_eq!(key.len(), 16);
        assert!(key.iter().any(|byte| *byte != 0));
    }

    #[test]
    fn room_options_provider_without_context_leaves_encryption_disabled() {
        let provider = E2eeRoomOptionsProvider { e2ee: None };
        let options = provider.room_options();

        assert!(options.encryption.is_none());
    }

    #[test]
    fn per_participant_key_grace_period_from_env_uses_default_on_missing_or_invalid_values() {
        let missing_var = "MATRIX_SDK_LIVEKIT_TEST_GRACE_PERIOD_MISSING";
        let invalid_var = "MATRIX_SDK_LIVEKIT_TEST_GRACE_PERIOD_INVALID";

        // SAFETY: Tests are the only callers using these dedicated variable names.
        unsafe { std::env::remove_var(missing_var) };
        // SAFETY: Tests are the only callers using these dedicated variable names.
        unsafe { std::env::set_var(invalid_var, "not-a-number") };

        assert_eq!(
            per_participant_key_grace_period_from_env(missing_var, 500),
            Duration::from_millis(500)
        );
        assert_eq!(
            per_participant_key_grace_period_from_env(invalid_var, 750),
            Duration::from_millis(750)
        );
    }

    #[test]
    fn per_participant_key_grace_period_from_env_uses_valid_value() {
        let var = "MATRIX_SDK_LIVEKIT_TEST_GRACE_PERIOD_VALID";

        // SAFETY: Tests are the only callers using these dedicated variable names.
        unsafe { std::env::set_var(var, "1200") };

        assert_eq!(
            per_participant_key_grace_period_from_env(var, 300),
            Duration::from_millis(1200)
        );
    }
}
