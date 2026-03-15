#![recursion_limit = "256"]

// Stuff from Matrix Bot

mod AppState;
use serde::Deserialize;
use std::sync::{Arc, LazyLock};
use std::{env, fs};

mod BotConfig;
mod InputMapping;

use reqwest;
use url::Url;

use axum;
use axum::extract::State;
use axum::{extract::Query, response::IntoResponse, routing::get, Router};

use std::borrow::ToOwned;

use matrix_sdk::{
    config::SyncSettings,
    event_handler::EventHandlerDropGuard,
    room::Room,
    ruma::{OwnedRoomId, OwnedServerName, RoomId, RoomOrAliasId, ServerName},
    Client, RoomState,
};

use matrix_sdk::encryption::secret_storage::SecretStore;
use matrix_sdk::ruma::events::room::message::{
    MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent,
};

use tracing::Level;
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{filter, Layer};

use pulldown_cmark::{html, Event, Options, Parser, Tag};

const BWI_TARGET: &str = "BWI";
static RTC_RUNTIME: LazyLock<tokio::sync::Mutex<Option<Arc<RtcLiveKitRuntime>>>> =
    LazyLock::new(|| tokio::sync::Mutex::new(None));
static BOT_CONFIG: OnceLock<BotConfig::Config> = OnceLock::new();

fn bot_config() -> &'static BotConfig::Config {
    BOT_CONFIG.get().expect("bot config not initialized; call init_bot_config() first")
}

fn init_bot_config() -> anyhow::Result<&'static BotConfig::Config> {
    if let Some(config) = BOT_CONFIG.get() {
        return Ok(config);
    }

    let config = BotConfig::Config::from_env()?;
    let _ = BOT_CONFIG.set(config);

    BOT_CONFIG.get().ok_or_else(|| anyhow!("failed to initialize bot config"))
}

#[derive(Deserialize)]
pub struct SearchResultParams {
    pub result: String,
}

#[derive(Deserialize)]
pub struct ExploreResultParams {
    pub result: String,
}

fn markdown_to_html(markdown: &str) -> String {
    let parser = Parser::new_ext(markdown, Options::all());
    let mut html_output = String::new();
    html::push_html(&mut html_output, parser);
    html_output
}

fn markdown_to_plain(markdown: &str) -> String {
    let parser = Parser::new(markdown);
    let mut plain = String::new();

    for event in parser {
        match event {
            Event::Text(text) | Event::Code(text) => {
                plain.push_str(&text);
            }
            Event::Start(Tag::Link(_, _, title)) => {
                plain.push_str(&title);
            }
            _ => {}
        }
    }

    plain
}

fn emoji_to_unicode_codes(s: &str) -> String {
    s.chars().map(|c| format!("U+{:04X}", c as u32)).collect::<Vec<_>>().join(" ")
}

async fn send_message_to_bot(
    client: reqwest::Client,
    message: String,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut params = std::collections::HashMap::new();
    let mut url = Url::parse(&bot_config().webserver_send)?;
    let mut isKill = false;

    match InputMapping::get_order(&message) {
        Some(order) => {
            if order.eq("kill") {
                url.path_segments_mut().expect("cannot be base").push("jetbot_kill");
                isKill = true;
            } else {
                url.path_segments_mut().expect("cannot be base").push("jetbot_async");

                match InputMapping::get_object(&message) {
                    Some(object) => {
                        params.insert("order", order);
                        params.insert("object", object);
                    }
                    None => {
                        params.insert("order", order);
                        println!("Send parameterless order {}", order);
                    }
                }
            }
        }
        None => {
            eprintln!(
                "Error: No mapping found for input: {} Unicode: {}",
                &message,
                emoji_to_unicode_codes(&message)
            );
        }
    }

    if isKill == true || !params.is_empty() {
        println!("BOT::send_to_bot {}", &url);

        let _response = client.get(url).query(&params).send().await?.text().await?;

        println!("BOT::send_message_to_bot for {}", &message);
    }

    Ok("Request".to_string())
}

async fn on_room_message(event: OriginalSyncRoomMessageEvent, room: Room, client: Client) {
    // First, we need to unpack the message: We only want messages from rooms we are
    // still in and that are regular text messages - ignoring everything else.

    let room_in_id = bot_config().room_in_id.parse::<OwnedRoomId>().expect("Invalid Room In ID");
    let room_out_id = bot_config().room_out_id.parse::<OwnedRoomId>().expect("Invalid Room Out ID");

    if room.state() != RoomState::Joined {
        return;
    }

    // we only listen to the input room
    if room.room_id() != room_in_id {
        println!("BOT::OnRoomMessage wrong room ID {}", room.room_id());
        return;
    }

    // for now we listen to text messages
    let MessageType::Text(text_content) = event.content.msgtype else { return };

    // only listen if we are mentioned
    if bot_config().on_mention_only == true {
        match event.content.mentions {
            Some(mentions) => {
                let user_ids = mentions.user_ids;
                let my_user_id = client.user_id().map(ToString::to_string).unwrap_or_default();

                let mut isMentioned = false;
                for item in &user_ids {
                    println!("BOT::- {}", item);
                    let cleaned = item.to_string();
                    if cleaned.eq(&my_user_id) {
                        println!("BOT:: Its Me ");
                        isMentioned = true;
                        break;
                    }
                }

                if isMentioned == false {
                    println!("BOT:: Its Not me");
                    return;
                }
            }
            None => {
                println!("BOT::No Mentions");
                return;
            }
        }
    }

    println!("BOT::sending");

    let message = markdown_to_plain(&text_content.body);

    // trim mentions from the command
    if let Some((_, command_message)) = message.split_once(':') {
        let cleaned_message = command_message.trim().to_string();

        if cleaned_message == "🎥" {
            let mut runtime_guard = RTC_RUNTIME.lock().await;

            if let Some(runtime) = runtime_guard.as_ref() {
                if let Err(err) = runtime.set_call_active(false).await {
                    eprintln!("Error deactivating rtc call: {err:#}");
                }
                runtime.shutdown_call_session();
                *runtime_guard = None;
            } else {
                match run_rtc_livekit_join(client.clone()).await {
                    Ok(runtime) => {
                        let runtime = Arc::new(runtime);
                        if let Err(err) = runtime.set_call_active(true).await {
                            eprintln!("Error activating rtc call: {err:#}");
                            runtime.shutdown_call_session();
                        } else {
                            *runtime_guard = Some(runtime);
                        }
                    }
                    Err(err) => eprintln!("Error starting rtc runtime: {err:#}"),
                }
            }
        } else {
            let reqwest_client = reqwest::Client::new();

            match send_message_to_bot(reqwest_client, cleaned_message).await {
                Ok(response) => println!("Response: {}", response),
                Err(err) => eprintln!("Error: {}", err),
            }
        }
    }

    if bot_config().echo_commands == true {
        if let Some(room_output) = client.get_room(&room_out_id) {
            let formatted_answer = if message.eq("left")
                || message.eq("right")
                || message.eq("forward")
                || message.eq("back")
            {
                format!("order: *{}*", message)
            } else {
                format!("search: *{}*", message)
            };

            let html_answer = markdown_to_html(&formatted_answer);
            let content_output =
                RoomMessageEventContent::text_html(&formatted_answer, &html_answer);
            room_output.send(content_output).await.unwrap();
        }
    }

    println!("BOT::message sent");
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

    // now that we've synced, let's attach a handler for incoming room messages, so
    // we can react on it
    client.add_event_handler(
        |event: OriginalSyncRoomMessageEvent, room: Room, client: Client| async move {
            on_room_message(event, room, client).await;
        },
    );

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

fn setup_logging(verbose: bool) {
    println!("with verbose logging {:?}", verbose);
    if verbose {
        tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .with_filter(filter::LevelFilter::from_level(Level::DEBUG)),
            )
            .init();
    } else {
        tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .with_filter(filter_fn(|meta| meta.target() == BWI_TARGET))
                    .with_filter(filter::LevelFilter::from_level(Level::INFO)),
            )
            .init();
    }
}

async fn setup_webserver(state: AppState::AppState) {
    let app = Router::new()
        .route("/explore", get(handle_explore_result))
        .route("/search", get(handle_search_result))
        .with_state(state);

    // run it
    let listener = tokio::net::TcpListener::bind(&bot_config().webserver_spawn).await.unwrap();

    println!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();
}

async fn handle_explore_result(
    State(state): State<AppState::AppState>,
    Query(params): Query<ExploreResultParams>,
) -> impl IntoResponse {
    println!("Received Explore");

    let matrix_client = state.matrix_client;
    let room_out_id = bot_config().room_out_id.parse::<OwnedRoomId>().expect("Invalid Room Out ID");

    if let Some(room_output) = matrix_client.get_room(&room_out_id) {
        let markdown = format!("{}", params.result);
        let html = markdown_to_html(&markdown);

        let content_output = RoomMessageEventContent::text_html(markdown, html);
        room_output.send(content_output).await.unwrap();
    }

    format!("Received Explore: {}", params.result)
}

async fn handle_search_result(
    State(state): State<AppState::AppState>,
    Query(params): Query<SearchResultParams>,
) -> impl IntoResponse {
    println!("Received search");

    let matrix_client = state.matrix_client;
    let room_out_id = bot_config().room_out_id.parse::<OwnedRoomId>().expect("Invalid Room Out ID");

    if let Some(room_output) = matrix_client.get_room(&room_out_id) {
        let markdown = format!("{}", params.result);
        let html = markdown_to_html(&markdown);

        let content_output = RoomMessageEventContent::text_html(markdown, html);
        room_output.send(content_output).await.unwrap();
    }

    format!("Received Search: {}", params.result)
}

// End Stuff from Matrix Bot

use anyhow::{anyhow, Context};
#[cfg(feature = "experimental-widgets")]
use matrix_sdk::widget::{
    element_call_member_content, element_call_send_event_message, start_element_call_widget,
    ClientProperties, ElementCallWidget, ElementCallWidgetOptions, EncryptionSystem, Intent,
};
#[cfg(all(feature = "v4l2", target_os = "linux"))]
use matrix_sdk_rtc::LiveKitError;
use matrix_sdk_rtc::LiveKitResult;
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
    LiveKitSdkConnector, LiveKitTokenProvider, Room as LivekitRoom, RoomOptions,
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
    async fn token(&self, _room: &Room) -> LiveKitResult<String> {
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
    let bot_cfg = init_bot_config()?;
    setup_logging(false);

    println!("before matrix setup");

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

    println!("after matrix setup");

    let shared_state = AppState::AppState { matrix_client: client.clone() };

    let secret_store = client
        .encryption()
        .secret_storage()
        .open_secret_store(secret_key.as_deref().unwrap())
        .await?;
    import_known_secrets(&client, secret_store).await?;

    println!("before webserver setup");

    let _webserver_task = tokio::spawn(setup_webserver(shared_state));

    println!("after webserver setup");

    sync(client).await?;

    Ok(())
}

/*
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    Ok(())
}


    let rtc = run_rtc_livekit_join().await?;
    rtc.set_call_active(true).await?;

    //tokio::signal::ctrl_c().await.context("wait for ctrl+c")?;
   // info!("received ctrl+c; shutting down rtc client");

    thread::sleep(Duration::from_secs(10));
    rtc.set_call_active(false).await?;
    rtc.shutdown().await;

    thread::sleep(Duration::from_secs(10));

    let rtc = run_rtc_livekit_join().await?;
    rtc.set_call_active(true).await?;

    //tokio::signal::ctrl_c().await.context("wait for ctrl+c")?;
    //info!("received ctrl+c; shutting down rtc client");

    thread::sleep(Duration::from_secs(10));
    rtc.set_call_active(false).await?;
    rtc.shutdown().await;

*/

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
            Arc::clone(&context.key_provider),
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

    let room_for_driver = room.clone();
    let service_url_for_driver = service_url.clone();
    let driver_handle = tokio::spawn(async move {
        run_livekit_driver_with_handler(
            room_for_driver,
            &connector,
            &service_url_for_driver,
            build_driver_state(
                room,
                #[cfg(all(feature = "v4l2", target_os = "linux"))]
                v4l2_config,
                #[cfg(feature = "e2ee-per-participant")]
                e2ee_context,
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
        #[cfg(feature = "experimental-widgets")]
        shutdown_membership_state_key,
        sync_handle,
        driver_handle,
    })
}

struct RtcLiveKitRuntime {
    room: Room,
    service_url: String,
    #[cfg(feature = "experimental-widgets")]
    widget: Option<ElementCallWidget>,
    #[cfg(feature = "experimental-widgets")]
    shutdown_membership_state_key: Option<CallMemberStateKey>,
    sync_handle: tokio::task::JoinHandle<Result<(), matrix_sdk::Error>>,
    driver_handle: tokio::task::JoinHandle<anyhow::Result<DriverState>>,
}

impl RtcLiveKitRuntime {
    async fn set_call_active(&self, active: bool) -> anyhow::Result<()> {
        #[cfg(feature = "experimental-widgets")]
        {
            if let Some(widget) = self.widget.as_ref() {
                if active {
                    publish_call_membership_via_widget(
                        self.room.clone(),
                        widget,
                        self.service_url.as_str(),
                    )
                    .await
                    .context("publish MatrixRTC membership via widget api")?;
                } else {
                    send_hangup_via_widget(widget, self.shutdown_membership_state_key.as_ref())
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
        self.sync_handle.abort();
        self.driver_handle.abort();
    }

    async fn shutdown(self) {
        if let Err(err) = self.set_call_active(false).await {
            info!(?err, "failed to deactivate call while shutting down runtime");
        }

        self.shutdown_call_session();

        let _ = self.sync_handle.await;
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

#[cfg(feature = "e2ee-per-participant")]
fn spawn_periodic_e2ee_key_resend(room: Room, context: PerParticipantE2eeContext) {
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
fn spawn_periodic_e2ee_key_resend(_room: Room, _context: ()) {}

#[cfg(feature = "experimental-widgets")]
async fn publish_call_membership_via_widget(
    room: Room,
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
