use std::sync::Arc;

use matrix_sdk::Client;

use crate::RtcLiveKitRuntime;

#[derive(Clone)]
pub struct AppState {
    pub matrix_client: Client,
    pub rtc_runtime: Arc<RtcLiveKitRuntime>,
}
