// src/state.rs

use std::collections::BTreeMap;

use linera_sdk::views::{RegisterView, RootView, ViewStorageContext};
use serde::{Deserialize, Serialize};

use poker_engine::domain::{PlayerId, TableId};
use poker_engine::domain::table::Table;
use poker_engine::state::HandEngineSnapshot;

/// Высокоуровневое состояние покерного приложения.
/// Храним ВНУТРИ одного RegisterView<AppState>.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppState {
    /// Сколько всего раздач было сыграно по всем столам.
    pub total_hands_played: u64,

    /// Кеш-столы: TableId -> Table (структура из poker_engine::domain::table).
    pub tables: BTreeMap<TableId, Table>,

    /// Активные раздачи по столам: TableId -> Option<HandEngineSnapshot>.
    ///
    /// - None       => сейчас на столе нет активной раздачи;
    /// - Some(...)  => есть активная раздача, снапшот движка сохранён.
    pub active_hands: BTreeMap<TableId, Option<HandEngineSnapshot>>,

    /// Отображение игрока на его display name.
    pub player_names: BTreeMap<PlayerId, String>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            total_hands_played: 0,
            tables: BTreeMap::new(),
            active_hands: BTreeMap::new(),
            player_names: BTreeMap::new(),
        }
    }
}

/// Корневой Linera-view, который реально лежит в сторадже.
/// Внутри один RegisterView<AppState>.
#[derive(RootView)]
#[view(context = ViewStorageContext)]
pub struct PokerState {
    /// Высокоуровневое состояние приложения.
    #[view(register)]
    pub app: RegisterView<AppState>,
}
