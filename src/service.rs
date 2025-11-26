// src/service.rs
#![cfg_attr(target_arch = "wasm32", no_main)]

use std::sync::Arc;

use async_graphql::{
    EmptyMutation, EmptySubscription, Object, Request, Response, Schema, SimpleObject,
};
use linera_sdk::{abi::WithServiceAbi, views::View, Service, ServiceRuntime};

use poker_engine::domain::table::Table;
use poker_engine::domain::TableId;
use poker_onchain::{AppState, PokerAbi, Storage};

/// Сервисный бинари (GraphQL-API) покерного приложения.
pub struct PokerService {
    /// Снэпшот состояния.
    state: Storage,
    /// Рантайм сервиса (можно использовать для query_application, но в v0.15.6 — только чтение).
    #[allow(dead_code)]
    runtime: Arc<ServiceRuntime<Self>>,
}

/// Экспорт wasm-энтрипоинтов сервиса.
linera_sdk::service!(PokerService);

impl WithServiceAbi for PokerService {
    type Abi = PokerAbi;
}

impl Service for PokerService {
    type Parameters = ();

    async fn new(runtime: ServiceRuntime<Self>) -> Self {
        let state = Storage::load(runtime.root_view_storage_context())
            .await
            .expect("Failed to load PokerState");

        PokerService {
            state,
            runtime: Arc::new(runtime),
        }
    }

    async fn handle_query(&self, request: Request) -> Response {
        // Берём снапшот AppState для чтения.
        let app: &AppState = self.state.app.get();

        let total_hands_played = app.total_hands_played;
        let tables = collect_cash_tables_snapshot(app);
        let tournaments: Vec<TournamentInfo> = Vec::new(); // пока турниров нет

        let schema = Schema::build(
            QueryRoot {
                total_hands_played,
                tables,
                tournaments,
            },
            EmptyMutation,
            EmptySubscription,
        )
        .finish();

        schema.execute(request).await
    }
}

/// DTO для кеш-стола, который видит фронт.
#[derive(Clone, SimpleObject)]
pub struct CashTableInfo {
    #[graphql(name = "tableId")]
    pub table_id: TableId,
    pub name: String,
    #[graphql(name = "smallBlind")]
    pub small_blind: u64,
    #[graphql(name = "bigBlind")]
    pub big_blind: u64,
    pub ante: u64,
    /// Максимум мест за столом (GraphQL Int, в Rust используем u32)
    #[graphql(name = "maxSeats")]
    pub max_seats: u32,
    /// Сколько игроков сейчас сидит за столом.
    #[graphql(name = "numPlayers")]
    pub num_players: u32,
    /// Идёт ли сейчас активная раздача.
    #[graphql(name = "inProgress")]
    pub in_progress: bool,
}

/// DTO турниров (пока заглушка).
#[derive(Clone, SimpleObject)]
pub struct TournamentInfo {
    #[graphql(name = "tournamentId")]
    pub tournament_id: u64,
    pub name: String,
}

/// Корневой Query-объект GraphQL.
#[derive(Clone)]
struct QueryRoot {
    total_hands_played: u64,
    tables: Vec<CashTableInfo>,
    tournaments: Vec<TournamentInfo>,
}

#[Object]
impl QueryRoot {
    /// Общее количество сыгранных раздач.
    async fn totalHandsPlayed(&self) -> u64 {
        self.total_hands_played
    }

    /// Список всех кеш-столов.
    async fn listTables(&self) -> &Vec<CashTableInfo> {
        &self.tables
    }

    /// Список турниров (пока пустой).
    async fn listTournaments(&self) -> &Vec<TournamentInfo> {
        &self.tournaments
    }

    /// Получить информацию по конкретному столу.
    async fn getTable(&self, #[graphql(name = "tableId")] table_id: TableId) -> Option<CashTableInfo> {
        self.tables
            .iter()
            .cloned()
            .find(|t| t.table_id == table_id)
    }
}

/// Собираем снапшоты кеш-столов для фронта.
fn collect_cash_tables_snapshot(app: &AppState) -> Vec<CashTableInfo> {
    app.tables
        .iter()
        .map(|(&table_id, table)| build_cash_table_info(app, table_id, table))
        .collect()
}

/// Конструируем CashTableInfo из доменного Table + AppState.
fn build_cash_table_info(app: &AppState, table_id: TableId, table: &Table) -> CashTableInfo {
    let num_players: u32 = table.seats.iter().filter(|s| s.is_some()).count() as u32;

    let stakes = &table.config.stakes;

    let in_progress = app
        .active_hands
        .get(&table_id)
        .and_then(|opt| opt.clone())
        .is_some();

    CashTableInfo {
        table_id,
        name: table.name.clone(),
        small_blind: stakes.small_blind.0,
        big_blind: stakes.big_blind.0,
        ante: stakes.ante.0,
        max_seats: table.config.max_seats as u32, // u8 -> u32
        num_players,
        in_progress,
    }
}
