use matrix_sdk::Client;

#[derive(Clone)]
pub struct AppState {
    pub matrix_client: Client,
}
