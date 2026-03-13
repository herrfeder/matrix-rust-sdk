#![recursion_limit = "256"]

#[cfg(any(feature = "experimental-widgets", all(feature = "v4l2", target_os = "linux")))]
use std::sync::{Arc, Mutex};
use std::{env, fs};

use anyhow::{anyhow, Context};
#[cfg(any(feature = "e2ee-per-participant", feature = "e2e-encryption"))]
use futures_util::StreamExt;
#[cfg(feature = "e2e-encryption")]
use matrix_sdk::encryption::secret_storage::SecretStore;
use matrix_sdk::{
    config::SyncSettings,
    event_handler::EventHandlerDropGuard,
    ruma::{OwnedRoomId, OwnedServerName, RoomId, RoomOrAliasId, ServerName},
    Client, RoomState,
};
#[cfg(feature = "experimental-widgets")]
use matrix_sdk::{
    ruma::{DeviceId, UserId},
    widget::{
        element_call_member_content, element_call_send_event_message, start_element_call_widget,
        ClientProperties, ElementCallWidget, ElementCallWidgetOptions, EncryptionSystem, Intent,
    },
};
#[cfg(all(feature = "v4l2", target_os = "linux"))]
use matrix_sdk_rtc::LiveKitError;
use matrix_sdk_rtc::{LiveKitConnector, LiveKitResult};
#[cfg(feature = "e2ee-per-participant")]
use matrix_sdk_rtc_livekit::livekit::id::ParticipantIdentity;
#[cfg(feature = "e2ee-per-participant")]
use matrix_sdk_rtc_livekit::per_participant::{
    build_per_participant_e2ee, per_participant_key_grace_period_from_env,
    register_e2ee_to_device_handler, send_per_participant_keys, spawn_livekit_e2ee_event_resend,
    E2eeRoomOptionsProvider, PerParticipantE2eeContext,
};
use matrix_sdk_rtc_livekit::{
    handle_connection_update as handle_livekit_connection_update, resolve_connection_details,
    run_livekit_driver_with_handler, LiveKitConnectionUpdate, LiveKitRoomOptionsProvider,
    LiveKitSdkConnector, LiveKitTokenProvider, Room, RoomOptions,
};
#[cfg(feature = "experimental-widgets")]
use ruma::events::call::member::CallMemberStateKey;
#[cfg(feature = "e2ee-per-participant")]
use ruma::events::{AnySyncMessageLikeEvent, AnyToDeviceEvent};
#[cfg(feature = "e2ee-per-participant")]
use ruma::serde::Raw;
use tracing::{info, warn};
#[cfg(feature = "experimental-widgets")]
use uuid::Uuid;

#[cfg(all(feature = "v4l2", target_os = "linux"))]
mod utils;

struct EnvLiveKitTokenProvider {
    token: String,
}

struct DefaultRoomOptionsProvider;

#[async_trait::async_trait]
impl LiveKitTokenProvider for EnvLiveKitTokenProvider {
    async fn token(&self, _room: &matrix_sdk::Room) -> LiveKitResult<String> {
        Ok(self.token.clone())
    }
}

impl LiveKitRoomOptionsProvider for DefaultRoomOptionsProvider {
    fn room_options(&self) -> RoomOptions {
        RoomOptions::default()
    }
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
mod videosource;
#[cfg(all(feature = "v4l2", target_os = "linux"))]
use videosource::{v4l2_config_from_env, V4l2CameraPublisher, V4l2Config, V4l2PublishError};

#[cfg(not(all(feature = "v4l2", target_os = "linux")))]
fn v4l2_config_from_env() -> anyhow::Result<()> {
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    run_rtc_livekit_join().await
}

async fn run_rtc_livekit_join() -> anyhow::Result<()> {
    let homeserver_url = required_env("HOMESERVER_URL")?;
    let username = required_env("MATRIX_USERNAME")?;
    let password = required_env("MATRIX_PASSWORD")?;
    let room_id_or_alias = required_env("ROOM_ID")?;
    let device_id = optional_env("MATRIX_DEVICE_ID");
    let livekit_service_url_override = optional_env("LIVEKIT_SERVICE_URL");
    let livekit_sfu_get_url = optional_env("LIVEKIT_SFU_GET_URL");
    let v4l2_config = v4l2_config_from_env().context("read V4L2 config")?;

    let store_dir =
        env::current_dir().context("read current directory")?.join("matrix-sdk-store");
    if store_dir.is_file() {
        warn!(
            store_path = %store_dir.display(),
            "Removing file that conflicts with sqlite store directory."
        );
        fs::remove_file(&store_dir).context("remove sqlite store file")?;
    }
    fs::create_dir_all(&store_dir).context("create crypto store directory")?;

    let legacy_store_path = store_dir.join("matrix-sdk.sqlite");
    if legacy_store_path.exists() {
        warn!(
            store_path = %legacy_store_path.display(),
            "Removing legacy sqlite file path."
        );
        if legacy_store_path.is_dir() {
            fs::remove_dir_all(&legacy_store_path).context("remove legacy sqlite directory")?;
        } else {
            fs::remove_file(&legacy_store_path).context("remove legacy sqlite file")?;
        }
    }

    for sqlite_file in [
        "matrix-sdk-state.sqlite3",
        "matrix-sdk-crypto.sqlite3",
        "matrix-sdk-event-cache.sqlite3",
        "matrix-sdk-media.sqlite3",
    ] {
        let db_path = store_dir.join(sqlite_file);
        if db_path.is_file() {
            let header = fs::read(&db_path)
                .context("read sqlite header")?
                .into_iter()
                .take(16)
                .collect::<Vec<_>>();
            if header != b"SQLite format 3\0" {
                warn!(
                    store_path = %db_path.display(),
                    "Removing invalid sqlite store file."
                );
                fs::remove_file(&db_path).context("remove invalid sqlite file")?;
            }
        }
    }

    let mut client_builder = Client::builder().homeserver_url(homeserver_url);

    #[cfg(feature = "sqlite")]
    {
        client_builder = client_builder.sqlite_store(store_dir, None);
    }
    #[cfg(not(feature = "sqlite"))]
    {
        let _ = &store_path;
        warn!("sqlite feature disabled; crypto store will be in-memory.");
    }

    let client = client_builder.build().await.context("build Matrix client")?;

    let mut login_builder = client.matrix_auth().login_username(&username, &password);
    if let Some(device_id) = device_id.as_deref() {
        login_builder = login_builder.device_id(device_id);
    }
    login_builder.send().await.context("login Matrix user")?;
    import_recovery_key_if_set(&client).await.context("import recovery key")?;
    log_backup_state(&client).await;

    let room_id_or_alias = RoomOrAliasId::parse(room_id_or_alias).context("parse ROOM_ID")?;
    let via_servers = via_servers_from_env().context("parse VIA_SERVERS")?;
    let room = match RoomId::parse(room_id_or_alias.as_str()) {
        Ok(room_id) => match client.get_room(&room_id) {
            Some(room) if room.state() == RoomState::Joined => room,
            _ => client.join_room_by_id(&room_id).await.context("join room")?,
        },
        Err(_) => client
            .join_room_by_id_or_alias(&room_id_or_alias, &via_servers)
            .await
            .context("join room")?,
    };
    spawn_backup_diagnostics(client.clone(), room.room_id().to_owned());
    let element_call_url =
        optional_env("ELEMENT_CALL_URL").or_else(|| optional_env("ELEMENT_CALL_WIDGET"));
    #[cfg(feature = "experimental-widgets")]
    let widget = if let Some(element_call_url) = element_call_url {
        info!(%element_call_url, "Element Call widget URL set; starting widget bridge");

        let encryption_state = room
            .latest_encryption_state()
            .await
            .context("load room encryption state for element call")?;
        let encryption = if encryption_state.is_encrypted() {
            info!("room is encrypted; Element Call will be configured for E2EE");
            #[cfg(feature = "e2ee-per-participant")]
            {
                EncryptionSystem::PerParticipantKeys
            }
            #[cfg(not(feature = "e2ee-per-participant"))]
            {
                info!("room is encrypted but per-participant E2EE is disabled at compile time");
                EncryptionSystem::Unencrypted
            }
        } else {
            info!("room is not encrypted; Element Call will be configured unencrypted");
            EncryptionSystem::Unencrypted
        };

        let options = ElementCallWidgetOptions {
            widget_id: optional_env("ELEMENT_CALL_WIDGET_ID")
                .unwrap_or_else(|| "element-call".to_owned()),
            parent_url: optional_env("ELEMENT_CALL_PARENT_URL"),
            encryption,
            intent: Intent::JoinExisting,
            client_properties: ClientProperties::new("matrix-sdk-rtc-livekit-join", None, None),
        };

        Some(
            start_element_call_widget(room.clone(), element_call_url, options)
                .await
                .context("start element call widget")?,
        )
    } else if optional_env("ELEMENT_CALL_WIDGET_ID").is_some() {
        info!("ELEMENT_CALL_WIDGET_ID set but no Element Call URL provided");
        None
    } else {
        None
    };

    #[cfg(not(feature = "experimental-widgets"))]
    let widget: Option<()> = None;

    #[cfg(feature = "e2ee-per-participant")]
    let _to_device_probe_guard = register_any_to_device_probe_handler(&client);
    #[cfg(feature = "e2ee-per-participant")]
    let _room_message_probe_guard =
        register_room_message_key_probe_handler(&client, room.room_id().to_owned());

    let sync_client = client.clone();
    let sync_handle = tokio::spawn(async move { sync_client.sync(SyncSettings::new()).await });

    // NOTE: Joining a call requires publishing MatrixRTC memberships (m.call.member) for
    // this device. When the optional Element Call widget wiring is enabled, this example
    // publishes a membership via the widget API before starting the driver so the
    // membership is visible to other clients.
    //
    // The optional Element Call widget wiring is how a Rust client can integrate
    // with the Element Call webapp:
    // - The widget driver bridges postMessage traffic to/from the webview/iframe.
    // - Capabilities allow Element Call to send/receive to-device encryption keys
    //   (io.element.call.encryption_keys), which the Rust SDK consumes for per-participant
    //   E2EE when enabled.
    // - When running Element Call inside element-web directly (not embedded), the widget
    //   bridge logs below will not appear because the Rust SDK is not connected to that
    //   webview's postMessage channel.

    let static_livekit_token = optional_env("LIVEKIT_TOKEN");
    let connection_details = resolve_connection_details(
        &client,
        &room,
        livekit_sfu_get_url.as_deref(),
        livekit_service_url_override.as_deref(),
        static_livekit_token.as_deref(),
    )
    .await
    .context("resolve LiveKit connection details")?;
    let livekit_token = connection_details.token.clone();
    let service_url = connection_details
        .authenticated_service_url()
        .context("attach access_token to LiveKit service url")?;

    #[cfg(feature = "experimental-widgets")]
    if let Some(widget) = widget.as_ref() {
        publish_call_membership_via_widget(room.clone(), widget, &service_url)
            .await
            .context("publish MatrixRTC membership via widget api")?;
    }

    let token_provider = EnvLiveKitTokenProvider { token: livekit_token.clone() };
    #[cfg(feature = "e2ee-per-participant")]
    let e2ee_context = build_per_participant_e2ee(
        &room,
        bool_env("PER_PARTICIPANT_FORCE_BACKUP_DOWNLOAD"),
        retry_attempts_from_env("PER_PARTICIPANT_KEY_RETRIES", 0),
        std::time::Duration::from_secs(1),
    )
    .await?;
    #[cfg(feature = "e2ee-per-participant")]
    if let Some(context) = e2ee_context.as_ref() {
        if let (Some(user_id), Some(device_id)) = (client.user_id(), client.device_id()) {
            let identity = ParticipantIdentity(format!("{user_id}:{device_id}"));
            let key_set = context.key_provider.set_key(
                &identity,
                context.key_index,
                context.local_key.clone(),
            );
            info!(
                %identity,
                key_index = context.key_index,
                key_set,
                "seeded local per-participant E2EE key_provider key before LiveKit connect"
            );
        }
    }
    #[cfg(feature = "e2ee-per-participant")]
    let _e2ee_to_device_guard = e2ee_context.as_ref().map(|context| {
        register_e2ee_to_device_handler(
            &client,
            room.room_id().to_owned(),
            std::sync::Arc::clone(&context.key_provider),
        )
    });
    #[cfg(feature = "e2ee-per-participant")]
    if let Some(context) = e2ee_context.as_ref() {
        spawn_periodic_e2ee_key_resend(room.clone(), context.clone());
    }
    #[cfg(feature = "e2ee-per-participant")]
    let room_options_provider = E2eeRoomOptionsProvider { e2ee: e2ee_context.clone() };
    #[cfg(not(feature = "e2ee-per-participant"))]
    let room_options_provider = DefaultRoomOptionsProvider;
    #[cfg(not(feature = "e2ee-per-participant"))]
    info!(
        "`e2ee-per-participant` feature is disabled; this device will not send io.element.call.encryption_keys to-device messages"
    );
    let resolved_room_options = room_options_provider.room_options();
    info!(
        room_options_provider_type = std::any::type_name_of_val(&room_options_provider),
        room_options = ?resolved_room_options,
        has_encryption_key_provider = resolved_room_options.encryption.is_some(),
        "configured LiveKit room options provider"
    );
    let connector = LiveKitSdkConnector::new(token_provider, room_options_provider);

    #[cfg(feature = "experimental-widgets")]
    let shutdown_membership_state_key = if widget.is_some() {
        let own_user_id =
            client.user_id().context("missing user id for widget shutdown event")?.to_owned();
        let own_device_id =
            client.device_id().context("missing device id for widget shutdown event")?.to_owned();
        Some(CallMemberStateKey::new(own_user_id, Some(own_device_id.to_string()), true))
    } else {
        None
    };
    info!(
        room_id = ?room.room_id(),
        service_url = %service_url,
        token_len = livekit_token.len(),
        "starting LiveKit driver"
    );
    tokio::select! {
        run_result = run_livekit_driver_with_handler(
            room.clone(),
            &connector,
            &service_url,
            build_driver_state(
                room.clone(),
                #[cfg(all(feature = "v4l2", target_os = "linux"))]
                v4l2_config,
                #[cfg(feature = "e2ee-per-participant")]
                e2ee_context.clone(),
            ),
            |state, update| async move {
                handle_livekit_connection_update(state, update, &handle_driver_connection_update).await
            },
        ) => {
            let _ = run_result.context("run LiveKit room driver")?;
        }
        ctrlc_result = tokio::signal::ctrl_c() => {
            ctrlc_result.context("wait for ctrl+c")?;
            info!("received ctrl+c; shutting down rtc client");

            #[cfg(feature = "experimental-widgets")]
            if let Some(widget) = widget.as_ref() {
                if let Err(err) = send_hangup_via_widget(widget, shutdown_membership_state_key.as_ref()).await {
                    info!(?err, "failed to send shutdown membership send_event via widget api during shutdown");
                }
            }

            sync_handle.abort();
            info!("ctrl+c shutdown flow finished; exiting process");
            std::process::exit(0);
        }
    }

    sync_handle.abort();

    Ok(())
}

fn required_env(name: &str) -> anyhow::Result<String> {
    env::var(name).with_context(|| anyhow!("missing required env var: {name}"))
}

fn optional_env(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn bool_env(name: &str) -> bool {
    optional_env(name).is_some_and(|value| {
        matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
    })
}

fn retry_attempts_from_env(name: &str, default: usize) -> usize {
    optional_env(name).and_then(|value| value.parse::<usize>().ok()).unwrap_or(default)
}

fn retry_seconds_from_env(name: &str, default: u64) -> u64 {
    optional_env(name).and_then(|value| value.parse::<u64>().ok()).unwrap_or(default)
}

#[cfg(feature = "e2e-encryption")]
async fn import_recovery_key_if_set(client: &Client) -> anyhow::Result<()> {
    let Some(recovery_key) = optional_env("MATRIX_RECOVERY_KEY") else {
        return Ok(());
    };
    if recovery_key.trim_start().starts_with('{') {
        info!(
            "MATRIX_RECOVERY_KEY looks like JSON; provide the secret storage recovery key string instead"
        );
    }
    info!("MATRIX_RECOVERY_KEY set; attempting to import secrets from secret storage");
    let secret_store: SecretStore = client
        .encryption()
        .secret_storage()
        .open_secret_store(&recovery_key)
        .await
        .context("open secret storage with recovery key")?;
    secret_store.import_secrets().await.context("import secrets from secret storage")?;
    info!("recovery key import finished");
    Ok(())
}

#[cfg(feature = "e2e-encryption")]
async fn log_backup_state(client: &Client) {
    let backups = client.encryption().backups();
    let state = backups.state();
    let enabled = backups.are_enabled().await;
    let exists = backups.fetch_exists_on_server().await.unwrap_or(false);
    info!(?state, enabled, exists_on_server = exists, "backup state summary");
    if exists && !enabled {
        info!(
            "backup exists on the server but backups are not enabled; ensure the recovery key is available"
        );
    }
}

#[cfg(not(feature = "e2e-encryption"))]
async fn log_backup_state(_client: &Client) {}

#[cfg(feature = "e2e-encryption")]
fn spawn_backup_diagnostics(client: Client, room_id: OwnedRoomId) {
    let backup_client = client.clone();
    tokio::spawn(async move {
        let mut state_stream = backup_client.encryption().backups().state_stream();
        while let Some(update) = state_stream.next().await {
            match update {
                Ok(state) => info!(?state, "backup state updated"),
                Err(err) => info!(?err, "backup state stream error"),
            }
        }
        info!("backup state stream closed");
    });

    tokio::spawn(async move {
        let key_stream = client.encryption().backups().room_keys_for_room_stream(&room_id);
        futures_util::pin_mut!(key_stream);
        while let Some(update) = key_stream.next().await {
            match update {
                Ok(room_keys) => info!(?room_keys, "received room keys from backup"),
                Err(err) => info!(?err, "room key backup stream error"),
            }
        }
        info!("room key backup stream closed");
    });
}

#[cfg(not(feature = "e2e-encryption"))]
fn spawn_backup_diagnostics(_client: Client, _room_id: OwnedRoomId) {}

#[cfg(feature = "e2ee-per-participant")]
fn spawn_periodic_e2ee_key_resend(room: matrix_sdk::Room, context: PerParticipantE2eeContext) {
    let interval_secs = retry_seconds_from_env("PER_PARTICIPANT_KEY_RESEND_SECS", 0);
    if interval_secs == 0 {
        return;
    }
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        loop {
            interval.tick().await;
            info!(
                interval_secs,
                key_index = context.key_index,
                "periodic per-participant E2EE key resend"
            );
            if let Err(err) =
                send_per_participant_keys(&room, context.key_index, &context.local_key, None).await
            {
                info!(?err, "failed to resend per-participant E2EE keys");
            }
        }
    });
}

#[cfg(not(feature = "e2ee-per-participant"))]
fn spawn_periodic_e2ee_key_resend(_room: matrix_sdk::Room, _context: ()) {}

#[cfg(feature = "experimental-widgets")]
async fn publish_call_membership_via_widget(
    room: matrix_sdk::Room,
    widget: &ElementCallWidget,
    service_url: &str,
) -> anyhow::Result<()> {
    if !*widget.capabilities_ready().borrow() {
        let mut capabilities_ready = widget.capabilities_ready();
        let _ = capabilities_ready.changed().await;
    }
    let own_user_id = room
        .client()
        .user_id()
        .context("missing user id for widget membership publisher")?
        .to_owned();
    let own_device_id = room
        .client()
        .device_id()
        .context("missing device id for widget membership publisher")?
        .to_owned();
    let state_key =
        CallMemberStateKey::new(own_user_id.clone(), Some(own_device_id.to_string()), true);
    let content = element_call_member_content(room.room_id(), &own_device_id, service_url);
    let request_id = Uuid::new_v4().to_string();
    let send_event_message = element_call_send_event_message(
        widget.widget_id(),
        &request_id,
        state_key.as_ref(),
        &content,
    );

    let send_event_message_json = send_event_message.to_string();
    info!(
        request_body = send_event_message_json.as_str(),
        "Publishing MatrixRTC membership send_event via widget api"
    );

    if !widget.handle().send(send_event_message.to_string()).await {
        return Err(anyhow!("widget driver handle closed before sending membership send_event"));
    }

    info!(state_key = state_key.as_ref(), "published MatrixRTC membership via widget api");
    Ok(())
}

#[cfg(feature = "experimental-widgets")]
async fn send_hangup_via_widget(
    widget: &ElementCallWidget,
    state_key: Option<&CallMemberStateKey>,
) -> anyhow::Result<()> {
    const SHUTDOWN_WIDGET_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

    if !*widget.capabilities_ready().borrow() {
        let mut capabilities_ready = widget.capabilities_ready();
        let _ =
            tokio::time::timeout(SHUTDOWN_WIDGET_WAIT_TIMEOUT, capabilities_ready.changed()).await;
    }

    let request_id = Uuid::new_v4().to_string();
    let response_rx = widget.track_pending_response(request_id.clone());
    let state_key = state_key.map(|state_key| state_key.as_ref()).unwrap_or_default();
    let shutdown_message = element_call_send_event_message(
        widget.widget_id(),
        &request_id,
        state_key,
        &serde_json::json!({}),
    );
    info!(
        request_body = shutdown_message.to_string().as_str(),
        "sending shutdown membership send_event via widget api"
    );

    match tokio::time::timeout(
        SHUTDOWN_WIDGET_WAIT_TIMEOUT,
        widget.handle().send(shutdown_message.to_string()),
    )
    .await
    {
        Ok(true) => info!("shutdown membership send_event sent via widget api"),
        Ok(false) => {
            widget.remove_pending_response(&request_id);
            return Err(anyhow!(
                "widget driver handle closed before sending shutdown membership send_event"
            ));
        }
        Err(_) => {
            widget.remove_pending_response(&request_id);
            info!(
                "timeout while sending shutdown membership send_event via widget api; continuing shutdown"
            );
            return Ok(());
        }
    }

    match tokio::time::timeout(SHUTDOWN_WIDGET_WAIT_TIMEOUT, response_rx).await {
        Ok(Ok(_response)) => {
            info!(request_id, "received widget response for shutdown membership send_event")
        }
        Ok(Err(_)) => info!(
            request_id,
            "shutdown membership send_event response channel closed; continuing shutdown"
        ),
        Err(_) => {
            widget.remove_pending_response(&request_id);
            info!(
                request_id,
                "timeout waiting for widget shutdown membership send_event response; continuing shutdown"
            );
        }
    }

    Ok(())
}

fn via_servers_from_env() -> anyhow::Result<Vec<OwnedServerName>> {
    let value = match env::var("VIA_SERVERS") {
        Ok(value) => value,
        Err(env::VarError::NotPresent) => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };

    value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| ServerName::parse(entry).context("parse server name"))
        .collect()
}

#[cfg(feature = "e2ee-per-participant")]
fn register_any_to_device_probe_handler(client: &Client) -> EventHandlerDropGuard {
    info!("registering probe handler for all to-device events");

    let handle = client.add_event_handler(move |raw: Raw<AnyToDeviceEvent>| async move {
        let event_type = raw
            .get_field::<String>("type")
            .ok()
            .flatten()
            .unwrap_or_else(|| "<missing>".to_owned());
        let sender = raw
            .get_field::<String>("sender")
            .ok()
            .flatten()
            .unwrap_or_else(|| "<missing>".to_owned());
        info!(event_type, sender, "probe observed to-device event");
    });

    client.event_handler_drop_guard(handle)
}

#[cfg(feature = "e2ee-per-participant")]
fn register_room_message_key_probe_handler(
    client: &Client,
    room_id: OwnedRoomId,
) -> EventHandlerDropGuard {
    info!(%room_id, "registering room message-like probe for encryption keys");

    let room_id_for_handler = room_id.clone();
    let handle = client.add_room_event_handler(
        &room_id_for_handler,
        move |raw: Raw<AnySyncMessageLikeEvent>| {
            let room_id = room_id.clone();
            async move {
                let event_type = raw
                    .get_field::<String>("type")
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| "<missing>".to_owned());

                if event_type == "io.element.call.encryption_keys" {
                    info!(%room_id, "probe observed room message-like encryption key event");
                }
            }
        },
    );

    client.event_handler_drop_guard(handle)
}

struct DriverState {
    room: matrix_sdk::Room,
    #[cfg(all(feature = "v4l2", target_os = "linux"))]
    v4l2_config: Option<V4l2Config>,
    #[cfg(all(feature = "v4l2", target_os = "linux"))]
    v4l2_publisher: Option<V4l2CameraPublisher>,
    #[cfg(feature = "e2ee-per-participant")]
    e2ee_context: Option<PerParticipantE2eeContext>,
}

fn build_driver_state(
    room: matrix_sdk::Room,
    #[cfg(all(feature = "v4l2", target_os = "linux"))] v4l2_config: Option<V4l2Config>,
    #[cfg(feature = "e2ee-per-participant")] e2ee_context: Option<PerParticipantE2eeContext>,
) -> DriverState {
    DriverState {
        room,
        #[cfg(all(feature = "v4l2", target_os = "linux"))]
        v4l2_config,
        #[cfg(all(feature = "v4l2", target_os = "linux"))]
        v4l2_publisher: None,
        #[cfg(feature = "e2ee-per-participant")]
        e2ee_context,
    }
}

async fn set_video_stream_enabled(
    state: &mut DriverState,
    room_handle: Option<Arc<Room>>,
    enabled: bool,
) -> LiveKitResult<()> {
    #[cfg(not(all(feature = "v4l2", target_os = "linux")))]
    let _ = (&state, room_handle, enabled);

    #[cfg(all(feature = "v4l2", target_os = "linux"))]
    {
        if enabled {
            if state.v4l2_publisher.is_none() {
                if let (Some(room_handle), Some(config)) =
                    (room_handle, state.v4l2_config.as_ref().cloned())
                {
                    info!(device = %config.device, "starting V4L2 camera publisher");
                    let publisher = V4l2CameraPublisher::start(room_handle, config)
                        .await
                        .map_err(|err| LiveKitError::connector(V4l2PublishError(err)))?;
                    state.v4l2_publisher = Some(publisher);
                }
            }
        } else if let Some(publisher) = state.v4l2_publisher.take() {
            publisher.stop().await.map_err(|err| LiveKitError::connector(V4l2PublishError(err)))?;
        }
    }

    Ok(())
}

async fn handle_driver_connection_update(
    mut state: DriverState,
    update: LiveKitConnectionUpdate,
) -> LiveKitResult<DriverState> {
    match update {
        LiveKitConnectionUpdate::Joined { room: room_handle, events } => {
            info!(room_name = %room_handle.name(), "LiveKit room connected");
            #[cfg(feature = "e2ee-per-participant")]
            let livekit_events = events;
            #[cfg(not(feature = "e2ee-per-participant"))]
            let _ = events;
            #[cfg(feature = "e2ee-per-participant")]
            if let Some(context) = state.e2ee_context.as_ref() {
                let identity = room_handle.local_participant().identity();
                let key_set = context.key_provider.set_key(
                    &identity,
                    context.key_index,
                    context.local_key.clone(),
                );
                room_handle.e2ee_manager().set_enabled(true);
                info!(
                    %identity,
                    key_index = context.key_index,
                    key_set,
                    "enabled per-participant E2EE for local participant"
                );

                if let Err(err) = send_per_participant_keys(
                    &state.room,
                    context.key_index,
                    &context.local_key,
                    None,
                )
                .await
                {
                    info!(
                        ?err,
                        "failed to send per-participant E2EE keys immediately after room connect"
                    );
                }

                let key_grace_period = per_participant_key_grace_period_from_env(
                    "PER_PARTICIPANT_KEY_GRACE_PERIOD_MS",
                    300,
                );
                if !key_grace_period.is_zero() {
                    info!(
                        key_grace_period_ms = key_grace_period.as_millis(),
                        "waiting for per-participant E2EE key grace period before publishing media"
                    );
                    tokio::time::sleep(key_grace_period).await;
                }

                if let Some(events) = livekit_events {
                    spawn_livekit_e2ee_event_resend(state.room.clone(), events, context.clone());
                }
            }
            set_video_stream_enabled(&mut state, Some(room_handle), true).await?;
        }
        LiveKitConnectionUpdate::Left => {
            set_video_stream_enabled(&mut state, None, false).await?;
        }
        LiveKitConnectionUpdate::Unchanged => {}
    }

    Ok(state)
}
