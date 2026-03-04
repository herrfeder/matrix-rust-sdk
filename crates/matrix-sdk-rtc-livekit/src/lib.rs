//! LiveKit SDK integration for matrix-sdk-rtc.

use async_trait::async_trait;
use matrix_sdk::Room as MatrixRoom;
use matrix_sdk_rtc::{LiveKitConnection, LiveKitConnector, LiveKitError, LiveKitResult};

#[cfg(feature = "crypto")]
pub mod matrix_keys;
#[cfg(feature = "crypto")]
pub mod per_participant;

pub use livekit;
use livekit::RoomEvent;
pub use livekit::e2ee;
use livekit::id::ParticipantIdentity;
pub use livekit::{ConnectionState, Room, RoomOptions};
use tokio::sync::Mutex;
use tracing::info;

/// A token provider for joining LiveKit rooms.
#[async_trait]
pub trait LiveKitTokenProvider: Send + Sync {
    /// Provide a LiveKit access token for the given Matrix room.
    async fn token(&self, room: &MatrixRoom) -> LiveKitResult<String>;
}

/// Provides LiveKit room options.
pub trait LiveKitRoomOptionsProvider: Send + Sync {
    /// Create the LiveKit room options used when connecting to LiveKit.
    fn room_options(&self) -> RoomOptions;
}

impl<F> LiveKitRoomOptionsProvider for F
where
    F: Fn() -> RoomOptions + Send + Sync,
{
    fn room_options(&self) -> RoomOptions {
        (self)()
    }
}

/// A LiveKit connection backed by the LiveKit Rust SDK.
#[derive(Debug)]
pub struct LiveKitSdkConnection {
    room: Room,
    events: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<RoomEvent>>>,
}

impl LiveKitSdkConnection {
    /// Access the underlying LiveKit room handle.
    pub fn room(&self) -> &Room {
        &self.room
    }

    /// Consume and return the LiveKit room event stream.
    ///
    /// Returns `None` if the stream has already been taken.
    pub async fn take_events(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<RoomEvent>> {
        self.events.lock().await.take()
    }

    /// Consume this connection and return the underlying LiveKit room.
    pub fn into_room(self) -> Room {
        self.room
    }
}

#[async_trait]
impl LiveKitConnection for LiveKitSdkConnection {
    async fn disconnect(self) -> LiveKitResult<()> {
        drop(self);
        Ok(())
    }
}

/// Connector implementation that joins rooms using the LiveKit Rust SDK.
#[derive(Debug)]
pub struct LiveKitSdkConnector<T, O> {
    token_provider: T,
    room_options: O,
}

impl<T, O> LiveKitSdkConnector<T, O> {
    /// Create a new connector using the provided token provider and room options.
    pub fn new(token_provider: T, room_options: O) -> Self {
        Self { token_provider, room_options }
    }

    /// Access the configured token provider.
    pub fn token_provider(&self) -> &T {
        &self.token_provider
    }

    /// Access the configured room options provider.
    pub fn room_options_provider(&self) -> &O {
        &self.room_options
    }
}

#[async_trait]
impl<T, O> LiveKitConnector for LiveKitSdkConnector<T, O>
where
    T: LiveKitTokenProvider,
    O: LiveKitRoomOptionsProvider,
{
    type Connection = LiveKitSdkConnection;

    async fn connect(
        &self,
        service_url: &str,
        room: &MatrixRoom,
    ) -> LiveKitResult<Self::Connection> {
        let token = self.token_provider.token(room).await?;
        let mut room_options = self.room_options_provider().room_options();
        if room_options.encryption.is_none() {
            room_options.encryption = room_options.e2ee.clone();
        }

        if let Some(encryption) = room_options.encryption.as_ref() {
            let key_provider = &encryption.key_provider;
            let key_index = key_provider.get_latest_key_index();
            let maybe_local_key = room
                .client()
                .user_id()
                .zip(room.client().device_id())
                .map(|(user_id, device_id)| ParticipantIdentity(format!("{user_id}:{device_id}")))
                .and_then(|identity| key_provider.get_key(&identity, key_index));
            info!(
                room_id = ?room.room_id(),
                key_index,
                key_provider_key = ?maybe_local_key,
                "resolved LiveKit encryption key_provider key before connect"
            );
        }
        info!(
            room_id = ?room.room_id(),
            service_url,
            room_options = ?room_options,
            "connecting to LiveKit room with resolved room options"
        );
        let (livekit_room, events) = Room::connect(service_url, &token, room_options)
            .await
            .map_err(LiveKitError::connector)?;
        Ok(LiveKitSdkConnection { room: livekit_room, events: Mutex::new(Some(events)) })
    }
}
