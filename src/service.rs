#![cfg_attr(target_arch = "wasm32", no_main)]

use linera_sdk::{
    abi::WithServiceAbi,
    views::{RootView, View},
    Service,
    ServiceRuntime,
};
use serde_json::Value;

use crate::{PokerAbi, PokerState};

/// Read-only сервис для покерного приложения.
///
/// Шаг 0: упрощённый JSON API:
/// - запрос — произвольный JSON (игнорируем),
/// - ответ — простая статистика по цепи.
pub struct PokerService {
    pub state: PokerState,
    pub runtime: ServiceRuntime<Self>,
}

linera_sdk::service!(PokerService);

impl WithServiceAbi for PokerService {
    type Abi = PokerAbi;
}

impl Service for PokerService {
    type Parameters = ();

    async fn new(runtime: ServiceRuntime<Self>) -> Self {
        let state = PokerState::load(runtime.root_view_storage_context())
            .await
            .expect("Failed to load PokerState for service");

        PokerService { state, runtime }
    }

    async fn handle_query(&self, _query: Value) -> Value {
        // Шаг 0: просто отдаём базовую статистику по цепи.
        let total_hands_played = *self.state.total_hands_played.get();

        serde_json::json!({
            "total_hands_played": total_hands_played
        })
    }
}
