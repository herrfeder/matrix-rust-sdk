#![recursion_limit = "256"]

use matrix_sdk::{
    Client, RoomState,
    config::SyncSettings,
    event_handler::EventHandlerDropGuard,
    room::Room,
    ruma::{OwnedServerName, RoomId, RoomOrAliasId, ServerName},
};
use std::borrow::ToOwned;
use std::sync::Arc;
use std::time::Duration;
use std::{env, fs};

use matrix_sdk::encryption::secret_storage::SecretStore;

use anyhow::{Context, anyhow};
#[cfg(all(feature = "v4l2", target_os = "linux"))]
use matrix_sdk_rtc_livekit::LiveKitError;
use matrix_sdk_rtc_livekit::LiveKitResult;
#[cfg(feature = "experimental-widgets")]
use matrix_sdk_rtc_livekit::element_call::{
    LiveKitElementCallWidget, start_element_call_widget_for_room,
};
#[cfg(feature = "e2ee-per-participant")]
use matrix_sdk_rtc_livekit::per_participant::{
    PerParticipantE2eeContext, per_participant_key_grace_period_from_env,
    prepare_per_participant_e2ee, send_per_participant_keys, spawn_livekit_e2ee_event_resend,
};
use matrix_sdk_rtc_livekit::{
    LiveKitConnectionUpdate, LiveKitRoomOptionsProvider, Room as LivekitRoom,
    handle_connection_update as handle_livekit_connection_update, prepare_livekit_sdk_connector,
    run_livekit_driver_with_handler,
};
use tracing::{info, warn};
#[cfg(all(feature = "v4l2", target_os = "linux"))]
mod videosource;
#[cfg(all(feature = "v4l2", target_os = "linux"))]
use videosource::{V4l2CameraPublisher, V4l2Config, V4l2PublishError, v4l2_config_from_env};

#[cfg(not(all(feature = "v4l2", target_os = "linux")))]
fn v4l2_config_from_env() -> anyhow::Result<()> {
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let homeserver_url = required_env("HOMESERVER_URL")?;
    let username = required_env("MATRIX_USERNAME")?;
    let password = required_env("MATRIX_PASSWORD")?;
    let device_id = optional_env("MATRIX_DEVICE_ID");
    let secret_key = optional_env("MATRIX_RECOVERY_KEY");
    let store_dir = env::current_dir().context("read current directory")?.join("matrix-sdk-store");
    prepare_sqlite_store_dir(&store_dir)?;

    // our actual runner
    let client = login(
        &homeserver_url,
        &username,
        &password,
        device_id.as_deref(),
        Some(&store_dir),
        "matrix-bot",
    )
    .await?;

    let secret_store = client
        .encryption()
        .secret_storage()
        .open_secret_store(secret_key.as_deref().unwrap())
        .await?;
    import_known_secrets(&client, secret_store).await?;

    println!("before webserver setup");

    let sync_handle = tokio::spawn({
        let client = client.clone();
        async move { sync(client).await }
    });

    let rtc = run_rtc_livekit_join(client.clone()).await?;
    rtc.set_call_active(true).await?;

    //tokio::signal::ctrl_c().await.context("wait for ctrl+c")?;
    // info!("received ctrl+c; shutting down rtc client");

    tokio::time::sleep(Duration::from_secs(10)).await;
    rtc.set_call_active(false).await?;
    rtc.shutdown().await;

    tokio::time::sleep(Duration::from_secs(10)).await;

    let rtc = run_rtc_livekit_join(client.clone()).await?;
    rtc.set_call_active(true).await?;

    //tokio::signal::ctrl_c().await.context("wait for ctrl+c")?;
    //info!("received ctrl+c; shutting down rtc client");

    tokio::time::sleep(Duration::from_secs(10)).await;
    rtc.set_call_active(false).await?;
    rtc.shutdown().await;

    sync_handle.abort();
    Ok(())
}

async fn login(
    homeserver_url: &str,
    username: &str,
    password: &str,
    device_id: Option<&str>,
    store_dir: Option<&std::path::Path>,
    initial_device_display_name: &str,
) -> anyhow::Result<Client> {
    let mut client_builder = Client::builder().homeserver_url(homeserver_url);

    if let Some(store_dir) = store_dir {
        #[cfg(feature = "sqlite")]
        {
            client_builder = client_builder.sqlite_store(store_dir, None);
        }
        #[cfg(not(feature = "sqlite"))]
        {
            let _ = store_dir;
            warn!("sqlite feature disabled; crypto store will be in-memory.");
        }
    }

    let client = client_builder.build().await.context("build Matrix client")?;

    let mut login_builder = client
        .matrix_auth()
        .login_username(username, password)
        .initial_device_display_name(initial_device_display_name);

    if let Some(device_id) = device_id {
        login_builder = login_builder.device_id(device_id);
    }

    login_builder.send().await.context("login Matrix user")?;

    // It worked!
    println!("logged in as {username}");

    Ok(client)
}

fn prepare_sqlite_store_dir(store_dir: &std::path::Path) -> anyhow::Result<()> {
    if store_dir.is_file() {
        warn!(
            store_path = %store_dir.display(),
            "Removing file that conflicts with sqlite store directory."
        );
        fs::remove_file(store_dir).context("remove sqlite store file")?;
    }
    fs::create_dir_all(store_dir).context("create crypto store directory")?;

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

    Ok(())
}

async fn sync(client: Client) -> anyhow::Result<()> {
    // An initial sync to set up state and so our bot doesn't respond to old
    // messages. If the `StateStore` finds saved state in the location given the
    // initial sync will be skipped in favor of loading state from the store
    let sync_token = client.sync_once(SyncSettings::default()).await.unwrap().next_batch;

    // since we called `sync_once` before we entered our sync loop we must pass
    // that sync token to `sync`
    let settings = SyncSettings::default().token(sync_token);
    // this keeps state from the server streaming in to the bot via the
    // EventHandler trait
    client.sync(settings).await?; // this essentially loops until we kill the bot

    Ok(())
}

async fn import_known_secrets(client: &Client, secret_store: SecretStore) -> anyhow::Result<()> {
    secret_store.import_secrets().await?;

    let status = client
        .encryption()
        .cross_signing_status()
        .await
        .expect("We should be able to get our cross-signing status");

    if status.is_complete() {
        println!("Successfully imported all the cross-signing keys");
    } else {
        eprintln!("Couldn't import all the cross-signing keys: {status:?}");
    }

    Ok(())
}

async fn run_rtc_livekit_join(client: Client) -> anyhow::Result<RtcLiveKitRuntime> {
    let room_id_or_alias = required_env("ROOM_ID")?;
    let livekit_service_url_override = optional_env("LIVEKIT_SERVICE_URL");
    let livekit_sfu_get_url = optional_env("LIVEKIT_SFU_GET_URL");
    let v4l2_config = v4l2_config_from_env().context("read V4L2 config")?;

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
    let room_for_activation = room.clone();
    let element_call_url = optional_env("ELEMENT_CALL_URL");
    #[cfg(feature = "experimental-widgets")]
    let widget = if let Some(element_call_url) = element_call_url {
        info!(%element_call_url, "Element Call widget URL set; starting widget bridge");

        Some(
            start_element_call_widget_for_room(room.clone(), element_call_url)
                .await
                .context("start element call widget")?,
        )
    } else {
        None
    };

    #[cfg(not(feature = "experimental-widgets"))]
    let widget: Option<()> = None;

    let static_livekit_token = optional_env("LIVEKIT_TOKEN");
    #[cfg(feature = "e2ee-per-participant")]
    let prepared_e2ee = prepare_per_participant_e2ee(
        &room,
        bool_env("PER_PARTICIPANT_FORCE_BACKUP_DOWNLOAD"),
        retry_attempts_from_env("PER_PARTICIPANT_KEY_RETRIES", 0),
        Duration::from_secs(1),
        retry_seconds_from_env("PER_PARTICIPANT_KEY_RESEND_SECS", 0),
    )
    .await?;
    #[cfg(feature = "e2ee-per-participant")]
    let room_options_provider = prepared_e2ee.room_options_provider;
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
    let prepared_livekit = prepare_livekit_sdk_connector(
        &client,
        &room,
        livekit_sfu_get_url.as_deref(),
        livekit_service_url_override.as_deref(),
        static_livekit_token.as_deref(),
        room_options_provider,
    )
    .await
    .context("prepare LiveKit SDK connector")?;
    let service_url = prepared_livekit.service_url.clone();
    let token_len = prepared_livekit.token_len;

    #[cfg(feature = "e2ee-per-participant")]
    let e2ee_to_device_guard = prepared_e2ee.to_device_guard;
    #[cfg(feature = "e2ee-per-participant")]
    let e2ee_context_for_driver = prepared_e2ee.context;

    info!(
        room_id = ?room.room_id(),
        service_url = %service_url,
        token_len,
        "starting LiveKit driver"
    );

    let room_for_driver = room.clone();
    let service_url_for_driver = service_url.clone();
    let driver_handle = tokio::spawn(async move {
        run_livekit_driver_with_handler(
            room_for_driver,
            &prepared_livekit.connector,
            &service_url_for_driver,
            build_driver_state(
                room,
                #[cfg(all(feature = "v4l2", target_os = "linux"))]
                v4l2_config,
                #[cfg(feature = "e2ee-per-participant")]
                e2ee_context_for_driver,
            ),
            |state, update| async move {
                handle_livekit_connection_update(state, update, &handle_driver_connection_update)
                    .await
            },
        )
        .await
        .context("run LiveKit room driver")
    });

    Ok(RtcLiveKitRuntime {
        room: room_for_activation,
        service_url,
        #[cfg(feature = "experimental-widgets")]
        widget,
        #[cfg(feature = "e2ee-per-participant")]
        e2ee_to_device_guard,
        driver_handle,
    })
}

struct RtcLiveKitRuntime {
    room: Room,
    service_url: String,
    #[cfg(feature = "experimental-widgets")]
    widget: Option<LiveKitElementCallWidget>,
    #[cfg(feature = "e2ee-per-participant")]
    e2ee_to_device_guard: EventHandlerDropGuard,
    driver_handle: tokio::task::JoinHandle<anyhow::Result<DriverState>>,
}

impl RtcLiveKitRuntime {
    async fn set_call_active(&self, active: bool) -> anyhow::Result<()> {
        #[cfg(feature = "experimental-widgets")]
        {
            if let Some(widget) = self.widget.as_ref() {
                if active {
                    widget
                        .publish_membership(self.service_url.as_str())
                        .await
                        .context("publish MatrixRTC membership via widget api")?;
                } else {
                    widget
                        .send_hangup()
                        .await
                        .context("send shutdown membership via widget api")?;
                }

                return Ok(());
            }
        }

        if active {
            warn!(
                "set_call_active(true) requested without experimental widget support; activation must come from room call memberships"
            );
        } else {
            info!(
                "set_call_active(false) requested without experimental widget support; no local hangup message can be sent"
            );
        }

        Ok(())
    }

    fn shutdown_call_session(&self) {
        self.driver_handle.abort();
    }

    async fn shutdown(self) {
        if let Err(err) = self.set_call_active(false).await {
            info!(?err, "failed to deactivate call while shutting down runtime");
        }

        self.shutdown_call_session();

        let _ = self.driver_handle.await;
    }
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

struct DriverState {
    room: Room,
    #[cfg(all(feature = "v4l2", target_os = "linux"))]
    v4l2_config: Option<V4l2Config>,
    #[cfg(all(feature = "v4l2", target_os = "linux"))]
    v4l2_publisher: Option<V4l2CameraPublisher>,
    #[cfg(feature = "e2ee-per-participant")]
    e2ee_context: Option<PerParticipantE2eeContext>,
}

fn build_driver_state(
    room: Room,
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
    room_handle: Option<Arc<LivekitRoom>>,
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
        } else if let Some(mut publisher) = state.v4l2_publisher.take() {
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
