#![cfg_attr(target_arch = "wasm32", no_main)]

use linera_sdk::{
    abi::WithServiceAbi,
    views::View,
    Service,
    ServiceRuntime,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use poker_engine::api::dto::{PlayerAtTableDto, TableViewDto};
use poker_engine::domain::table::Table;
use poker_engine::domain::{PlayerId, SeatIndex, TableId};

use crate::{HandEngineSnapshot, PokerAbi, PokerState};

/// Read-only сервис для покерного приложения.
///
/// Этап 5:
/// - поддерживаем несколько "режимов" запросов через JSON:
///   - { "type": "summary" }  → общая статистика по приложению;
///   - { "type": "table",  "table_id": <TableId> } → один стол (TableViewDto);
///   - { "type": "tables" } → все столы (Vec<TableViewDto>).
///
/// Это соответствует GraphQL-идеям:
///   - QueryRoot.summary
///   - QueryRoot.table(table_id: ...) -> TableViewDto
///   - QueryRoot.tables() -> [TableViewDto]
pub struct PokerService {
    pub state: PokerState,
    pub runtime: ServiceRuntime<Self>,
}

linera_sdk::service!(PokerService);

impl WithServiceAbi for PokerService {
    type Abi = PokerAbi;
}

/// Внутренний тип запроса сервиса.
/// Его можно дергать напрямую JSON-ом с фронта.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServiceQuery {
    /// Базовая статистика (суммарно по приложению).
    Summary,

    /// Запрос одного стола.
    Table { table_id: TableId },

    /// Запрос всех столов.
    Tables,
}

impl Service for PokerService {
    type Parameters = ();

    async fn new(runtime: ServiceRuntime<Self>) -> Self {
        // Загружаем PokerState так же, как в контракте.
        let state = PokerState::load(runtime.root_view_storage_context())
            .await
            .expect("Failed to load PokerState for service");

        PokerService { state, runtime }
    }

    async fn handle_query(&self, query: Value) -> Value {
        // Пытаемся распарсить JSON в наш ServiceQuery.
        // Если формат не совпал — по умолчанию возвращаем summary.
        let parsed: Result<ServiceQuery, _> = serde_json::from_value(query.clone());
        let request = parsed.unwrap_or(ServiceQuery::Summary);

        match request {
            ServiceQuery::Summary => self.handle_summary().await,
            ServiceQuery::Table { table_id } => self.handle_table(table_id).await,
            ServiceQuery::Tables => self.handle_tables().await,
        }
    }
}

impl PokerService {
    /// "summary" – базовая статистика по приложению.
    async fn handle_summary(&self) -> Value {
        let total_hands_played = *self.state.total_hands_played.get();

        serde_json::json!({
            "total_hands_played": total_hands_played
        })
    }

    /// "table(table_id) -> TableViewDto".
    async fn handle_table(&self, table_id: TableId) -> Value {
        // Загружаем стол.
        let table_opt = self
            .state
            .tables
            .get(&table_id)
            .await
            .expect("View error (tables.get)");

        let Some(table) = table_opt else {
            return serde_json::json!({
                "error": "table_not_found",
                "table_id": table_id,
            });
        };

        // Загружаем активный снапшот, если есть.
        let active_snapshot = self
            .state
            .active_hands
            .get(&table_id)
            .await
            .expect("View error (active_hands.get)")
            .flatten();

        let dto = build_table_view_for_service(&self.state, &table, active_snapshot.as_ref())
            .await;

        serde_json::to_value(dto).expect("Failed to serialize TableViewDto")
    }

    /// "tables() -> [TableViewDto]".
    async fn handle_tables(&self) -> Value {
        // ПРАВИЛЬНЫЙ API: у MapView в 0.15.6 есть indices(), а не keys().
        let table_ids: Vec<TableId> = self
            .state
            .tables
            .indices()
            .await
            .expect("View error (tables.indices)");

        let mut result = Vec::<TableViewDto>::new();

        for table_id in table_ids {
            if let Some(table) = self
                .state
                .tables
                .get(&table_id)
                .await
                .expect("View error (tables.get in tables())")
            {
                let active_snapshot = self
                    .state
                    .active_hands
                    .get(&table_id)
                    .await
                    .expect("View error (active_hands.get in tables())")
                    .flatten();

                let dto =
                    build_table_view_for_service(&self.state, &table, active_snapshot.as_ref())
                        .await;

                result.push(dto);
            }
        }

        serde_json::to_value(result).expect("Failed to serialize Vec<TableViewDto>")
    }
}

/// Хелпер: делаем TableViewDto так же, как в on-chain оркестраторе.
///
/// ВАЖНО:
/// - Никакой логики engine, только преобразование доменной модели + snapshot → DTO.
/// - Логика должна быть максимально идентична `PokerOrchestrator::build_table_view`,
///   чтобы фронт видел одно и то же представление и после команд, и при чистом read-only запросе.
async fn build_table_view_for_service(
    state: &PokerState,
    table: &Table,
    active: Option<&HandEngineSnapshot>,
) -> TableViewDto {
    let current_actor_seat: Option<u8> = active
        .and_then(|s| s.current_actor)
        .map(|s: SeatIndex| s as u8);

    let mut players: Vec<PlayerAtTableDto> = Vec::new();

    for (idx, opt_player) in table.seats.iter().enumerate() {
        if let Some(p) = opt_player {
            let seat_index = idx as u8;
            let player_id: PlayerId = p.player_id;

            // Имя игрока для UI, если оно было сохранено.
            let display_name = state
                .player_names
                .get(&player_id)
                .await
                .expect("View error (player_names.get)")
                .unwrap_or_else(|| format!("Player #{}", player_id));

            players.push(PlayerAtTableDto {
                player_id,
                display_name,
                seat_index,
                stack: p.stack,
                current_bet: p.current_bet,
                status: p.status,
                // Hole cards в read-only сервисе не показываем.
                hole_cards: None,
            });
        }
    }

    TableViewDto {
        table_id: table.id,
        name: table.name.clone(),
        max_seats: table.config.max_seats,
        small_blind: table.config.stakes.small_blind,
        big_blind: table.config.stakes.big_blind,
        ante: table.config.stakes.ante,
        street: table.street,
        dealer_button: table.dealer_button.map(|s| s as u8),
        total_pot: table.total_pot,
        board: table.board.clone(),
        players,
        hand_in_progress: table.hand_in_progress,
        current_actor_seat,
    }
}
