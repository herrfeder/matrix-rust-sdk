//! LiveKit SDK integration for matrix-sdk-rtc.

use std::sync::Arc;

use async_trait::async_trait;
use futures_util::StreamExt;
use matrix_sdk::{Client, Room as MatrixRoom};
use matrix_sdk_rtc::{
    LiveKitConnection, LiveKitConnector, LiveKitError, LiveKitResult, livekit_service_url,
};
use reqwest::Client as HttpClient;
use ruma::{OwnedRoomId, api::client::account::request_openid_token};
use serde_json::Value as JsonValue;

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
use url::Url;

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

/// Connection details used by the LiveKit SDK connector.
#[derive(Debug, Clone)]
pub struct LiveKitConnectionDetails {
    /// LiveKit service URL.
    pub service_url: String,
    /// LiveKit JWT access token.
    pub token: String,
}

impl LiveKitConnectionDetails {
    /// Return the service URL with `access_token` appended if missing.
    pub fn authenticated_service_url(&self) -> LiveKitResult<String> {
        ensure_access_token_query(&self.service_url, &self.token)
    }
}

#[derive(Debug, thiserror::Error)]
enum LiveKitDetailsError {
    #[error("missing LiveKit token")]
    MissingToken,
    #[error("missing user id for OpenID token request")]
    MissingUserId,
    #[error("missing device id for /sfu/get request")]
    MissingDeviceId,
    #[error("missing LiveKit service url in /sfu/get response")]
    MissingSfuServiceUrl,
    #[error("missing LiveKit token in /sfu/get response")]
    MissingSfuToken,
    #[error(transparent)]
    UrlParse(#[from] url::ParseError),
}

/// Resolve LiveKit connection details either from `/sfu/get` or static env values.
pub async fn resolve_connection_details(
    client: &Client,
    room: &MatrixRoom,
    sfu_get_url: Option<&str>,
    service_url_override: Option<&str>,
    static_token: Option<&str>,
) -> LiveKitResult<LiveKitConnectionDetails> {
    if let Some(sfu_url) = sfu_get_url {
        let openid_token = request_openid_token(client).await?;
        let device_id = client
            .device_id()
            .ok_or_else(|| LiveKitError::connector(LiveKitDetailsError::MissingDeviceId))?
            .to_string();
        let (service_url, token) =
            fetch_sfu_token(sfu_url, room.room_id().to_owned(), device_id, &openid_token).await?;
        return Ok(LiveKitConnectionDetails { service_url, token });
    }

    let token = static_token
        .ok_or_else(|| LiveKitError::connector(LiveKitDetailsError::MissingToken))?
        .to_owned();
    let service_url = match service_url_override {
        Some(url) => url.to_owned(),
        None => livekit_service_url(client).await?,
    };

    Ok(LiveKitConnectionDetails { service_url, token })
}

async fn request_openid_token(
    client: &Client,
) -> LiveKitResult<request_openid_token::v3::Response> {
    let user_id = client
        .user_id()
        .ok_or_else(|| LiveKitError::connector(LiveKitDetailsError::MissingUserId))?;
    let request = request_openid_token::v3::Request::new(user_id.to_owned());
    let response = client.send(request).await?;
    Ok(response)
}

#[derive(serde::Serialize)]
struct SfuGetRequest {
    room: String,
    openid_token: OpenIdToken,
    device_id: String,
}

#[derive(serde::Serialize)]
struct OpenIdToken {
    access_token: String,
    expires_in: u64,
    matrix_server_name: String,
    token_type: String,
}

async fn fetch_sfu_token(
    url: &str,
    room_id: OwnedRoomId,
    device_id: String,
    openid_token: &request_openid_token::v3::Response,
) -> LiveKitResult<(String, String)> {
    let request_body = SfuGetRequest {
        room: room_id.to_string(),
        openid_token: OpenIdToken {
            access_token: openid_token.access_token.clone(),
            expires_in: openid_token.expires_in.as_secs(),
            matrix_server_name: openid_token.matrix_server_name.to_string(),
            token_type: openid_token.token_type.to_string(),
        },
        device_id,
    };
    let client = HttpClient::new();
    let request = client.post(url).json(&request_body);

    let response = request
        .send()
        .await
        .map_err(LiveKitError::connector)?
        .error_for_status()
        .map_err(LiveKitError::connector)?;
    let payload: JsonValue = response.json().await.map_err(LiveKitError::connector)?;

    let service_url = extract_string(
        &payload,
        &["service_url", "livekit_service_url", "livekit_url", "sfu_base_url", "sfu_url", "url"],
    )
    .ok_or_else(|| LiveKitError::connector(LiveKitDetailsError::MissingSfuServiceUrl))?;
    let token = extract_string(&payload, &["token", "jwt", "access_token"])
        .ok_or_else(|| LiveKitError::connector(LiveKitDetailsError::MissingSfuToken))?;

    Ok((service_url, token))
}

fn extract_string(payload: &JsonValue, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        payload.get(*key).and_then(|value| value.as_str()).map(|value| value.to_owned())
    })
}

fn ensure_access_token_query(service_url: &str, token: &str) -> LiveKitResult<String> {
    let mut url = Url::parse(service_url)
        .map_err(|e| LiveKitError::connector(LiveKitDetailsError::UrlParse(e)))?;
    let has_access_token = url.query_pairs().any(|(key, _)| key == "access_token");
    if !has_access_token {
        url.query_pairs_mut().append_pair("access_token", token);
    }
    Ok(url.into())
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

/// Update event emitted by [`update_connection`].
#[derive(Debug)]
pub enum LiveKitConnectionUpdate {
    Joined { room: Arc<Room>, events: Option<tokio::sync::mpsc::UnboundedReceiver<RoomEvent>> },
    Left,
    Unchanged,
}

/// Update an existing LiveKit connection based on room call memberships.
pub async fn update_connection<T, O>(
    room: &matrix_sdk::Room,
    connector: &LiveKitSdkConnector<T, O>,
    service_url: &str,
    room_info: &matrix_sdk::RoomInfo,
    connection: &mut Option<Arc<Room>>,
) -> LiveKitResult<LiveKitConnectionUpdate>
where
    T: LiveKitTokenProvider,
    O: LiveKitRoomOptionsProvider,
{
    let has_memberships = room_info.has_active_room_call();

    if has_memberships {
        if connection.is_none() {
            info!(room_id = ?room.room_id(), "joining LiveKit room for active call");
            let new_connection = connector.connect(service_url, room).await?;
            let livekit_events = new_connection.take_events().await;
            let room_handle = Arc::new(new_connection.into_room());
            *connection = Some(Arc::clone(&room_handle));
            return Ok(LiveKitConnectionUpdate::Joined {
                room: room_handle,
                events: livekit_events,
            });
        }
    } else if connection.take().is_some() {
        info!(room_id = ?room.room_id(), "leaving LiveKit room because the call ended");
        return Ok(LiveKitConnectionUpdate::Left);
    }

    Ok(LiveKitConnectionUpdate::Unchanged)
}

/// Run the LiveKit room driver until the room info stream ends.
pub async fn run_livekit_driver<T, O>(
    room: matrix_sdk::Room,
    connector: &LiveKitSdkConnector<T, O>,
    service_url: &str,
) -> LiveKitResult<()>
where
    T: LiveKitTokenProvider,
    O: LiveKitRoomOptionsProvider,
{
    let mut connection: Option<Arc<Room>> = None;
    let mut info_stream = room.subscribe_info();

    let _ = update_connection(&room, connector, service_url, &room.clone_info(), &mut connection)
        .await?;

    while let Some(room_info) = info_stream.next().await {
        let _ =
            update_connection(&room, connector, service_url, &room_info, &mut connection).await?;
    }

    let _ = connection.take();

    Ok(())
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
        #[allow(deprecated)]
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
