//! Matrix key material providers for LiveKit E2EE integrations.

use async_trait::async_trait;
use matrix_sdk::Room as MatrixRoom;
use matrix_sdk::ruma::RoomId;
use matrix_sdk_crypto::{
    OlmMachine, olm::ExportedRoomKey, store::CryptoStoreError, types::room_history::RoomKeyBundle,
};
use thiserror::Error;

/// Errors returned while exporting Matrix key material.
#[derive(Debug, Error)]
pub enum MatrixKeyMaterialError {
    /// The Matrix SDK crypto machine is not initialized.
    #[error("missing olm machine; crypto not initialized")]
    NoOlmMachine,
    /// The crypto store returned an error while exporting keys.
    #[error(transparent)]
    CryptoStore(#[from] CryptoStoreError),
}

/// Provides Matrix room key material for E2EE integrations.
#[async_trait]
pub trait MatrixKeyMaterialProvider: Send + Sync {
    /// Export the key material for the given Matrix room.
    async fn export_room_keys(
        &self,
        room: &MatrixRoom,
    ) -> Result<Vec<ExportedRoomKey>, MatrixKeyMaterialError>;
}

/// Provides per-participant key bundles for a Matrix room (MSC4268).
#[async_trait]
pub trait PerParticipantKeyMaterialProvider: Send + Sync {
    /// Build a per-participant key bundle for the room.
    async fn per_participant_key_bundle(
        &self,
        room_id: &RoomId,
    ) -> Result<RoomKeyBundle, MatrixKeyMaterialError>;
}

/// Matrix key material provider backed by an `OlmMachine`.
#[derive(Debug, Clone)]
pub struct OlmMachineKeyMaterialProvider {
    olm_machine: OlmMachine,
}

impl OlmMachineKeyMaterialProvider {
    /// Create a provider from an initialized `OlmMachine`.
    pub fn new(olm_machine: OlmMachine) -> Self {
        Self { olm_machine }
    }
}

#[async_trait]
impl MatrixKeyMaterialProvider for OlmMachineKeyMaterialProvider {
    async fn export_room_keys(
        &self,
        room: &MatrixRoom,
    ) -> Result<Vec<ExportedRoomKey>, MatrixKeyMaterialError> {
        let room_id = room.room_id();
        let keys = self
            .olm_machine
            .store()
            .export_room_keys(|session| session.room_id() == room_id)
            .await?;
        Ok(keys)
    }
}

#[async_trait]
impl PerParticipantKeyMaterialProvider for OlmMachineKeyMaterialProvider {
    async fn per_participant_key_bundle(
        &self,
        room_id: &RoomId,
    ) -> Result<RoomKeyBundle, MatrixKeyMaterialError> {
        let bundle = self.olm_machine.store().build_room_key_bundle(room_id).await?;
        Ok(bundle)
    }
}

/// Fetch the current `OlmMachine` for a room's client.
pub async fn room_olm_machine(room: &MatrixRoom) -> Result<OlmMachine, MatrixKeyMaterialError> {
    let client = room.client();
    let olm_machine = client.olm_machine().await;
    let Some(olm_machine) = olm_machine.as_ref() else {
        return Err(MatrixKeyMaterialError::NoOlmMachine);
    };
    Ok(olm_machine.clone())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use matrix_sdk::ruma::{device_id, room_id, user_id};
    use matrix_sdk_crypto::{OlmMachine, store::MemoryStore};

    use super::{OlmMachineKeyMaterialProvider, PerParticipantKeyMaterialProvider};

    #[tokio::test]
    async fn per_participant_key_bundle_empty_without_sessions() {
        let user_id = user_id!("@alice:example.org");
        let device_id = device_id!("DEVICEID");
        let store = Arc::new(MemoryStore::new());
        let olm_machine = OlmMachine::with_store(user_id, device_id, store, None)
            .await
            .expect("create olm machine");
        let provider = OlmMachineKeyMaterialProvider::new(olm_machine);
        let room_id = room_id!("!room:example.org");

        let bundle = provider.per_participant_key_bundle(room_id).await.unwrap();

        assert!(bundle.is_empty());
    }
}
