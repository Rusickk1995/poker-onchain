#![cfg_attr(target_arch = "wasm32", no_main)]

use linera_sdk::{
    abi::WithServiceAbi,
    views::View,
    Service,
    ServiceRuntime,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use poker_engine::api::dto::{
    PlayerAtTableDto,
    TableViewDto,
    TournamentViewDto,
};
use poker_engine::domain::table::Table;
use poker_engine::domain::{
    PlayerId,
    SeatIndex,
    TableId,
    TournamentId,
};

use poker_onchain::{
    HandEngineSnapshot,
    PokerAbi,
    PokerState,
};
use poker_onchain::utils::build_tournament_view;


#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServiceQuery {
    Summary,

    Table { table_id: TableId },
    Tables,

    Tournaments,
    TournamentById { tournament_id: TournamentId },
    TournamentTables { tournament_id: TournamentId },
}

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
        let state =
            PokerState::load(runtime.root_view_storage_context())
                .await
                .expect("Failed to load PokerState for service");

        PokerService { state, runtime }
    }

    async fn handle_query(&self, query: Value) -> Value {
        let parsed: Result<ServiceQuery, _> = serde_json::from_value(query.clone());
        let request = parsed.unwrap_or(ServiceQuery::Summary);

        match request {
            ServiceQuery::Summary => self.handle_summary().await,

            ServiceQuery::Table { table_id } => self.handle_table(table_id).await,
            ServiceQuery::Tables => self.handle_tables().await,

            ServiceQuery::Tournaments => self.handle_tournaments().await,
            ServiceQuery::TournamentById { tournament_id } =>
                self.handle_tournament_by_id(tournament_id).await,
            ServiceQuery::TournamentTables { tournament_id } =>
                self.handle_tournament_tables(tournament_id).await,
        }
    }
}

impl PokerService {
    async fn handle_summary(&self) -> Value {
        let total_hands_played = *self.state.total_hands_played.get();

        let tables_count =
            self.state.tables.indices().await.unwrap_or_default().len();

        let tournaments_count =
            self.state.tournaments.indices().await.unwrap_or_default().len();

        serde_json::json!({
            "total_hands_played": total_hands_played,
            "tables_count": tables_count,
            "tournaments_count": tournaments_count,
        })
    }

    async fn handle_table(&self, table_id: TableId) -> Value {
        let Some(table) =
            self.state.tables.get(&table_id).await.expect("tables.get error")
        else {
            return serde_json::json!({
                "error": "table_not_found",
                "table_id": table_id
            });
        };

        let active = self
            .state
            .active_hands
            .get(&table_id)
            .await
            .expect("active_hands.get error")
            .flatten();

        let dto =
            build_table_view_for_service(&self.state, &table, active.as_ref()).await;

        serde_json::to_value(dto).unwrap()
    }

    async fn handle_tables(&self) -> Value {
        let ids = self
            .state
            .tables
            .indices()
            .await
            .expect("tables.indices error");

        let mut out = Vec::new();

        for id in ids {
            if let Some(table) =
                self.state.tables.get(&id).await.unwrap_or(None)
            {
                let active = self
                    .state
                    .active_hands
                    .get(&id)
                    .await
                    .unwrap_or(None)
                    .flatten();

                let dto = build_table_view_for_service(
                    &self.state,
                    &table,
                    active.as_ref(),
                )
                .await;

                out.push(dto);
            }
        }

        serde_json::to_value(out).unwrap()
    }

    async fn handle_tournaments(&self) -> Value {
        let ids = self
            .state
            .tournaments
            .indices()
            .await
            .expect("tournaments.indices error");

        let mut out = Vec::new();

        for id in ids {
            if let Some(t) =
                self.state.tournaments.get(&id).await.unwrap_or(None)
            {
                let tables_running = self
                    .state
                    .tournament_tables
                    .get(&id)
                    .await
                    .unwrap_or(None)
                    .map(|v| v.len() as u32)
                    .unwrap_or(0);

                out.push(build_tournament_view(&t, tables_running));
            }
        }

        serde_json::to_value(out).unwrap()
    }

    async fn handle_tournament_by_id(&self, tournament_id: TournamentId) -> Value {
        let Some(t) =
            self.state.tournaments.get(&tournament_id).await.unwrap_or(None)
        else {
            return serde_json::json!({
                "error": "tournament_not_found",
                "tournament_id": tournament_id
            });
        };

        let tables_running = self
            .state
            .tournament_tables
            .get(&tournament_id)
            .await
            .unwrap_or(None)
            .map(|v| v.len() as u32)
            .unwrap_or(0);

        serde_json::to_value(build_tournament_view(&t, tables_running)).unwrap()
    }

    async fn handle_tournament_tables(
        &self,
        tournament_id: TournamentId,
    ) -> Value {
        let Some(table_ids) =
            self.state.tournament_tables.get(&tournament_id).await.unwrap_or(None)
        else {
            return serde_json::json!({
                "error": "tournament_not_found",
                "tournament_id": tournament_id
            });
        };

        let mut out = Vec::new();

        for tid in table_ids {
            if let Some(table) =
                self.state.tables.get(&tid).await.unwrap_or(None)
            {
                let active = self
                    .state
                    .active_hands
                    .get(&tid)
                    .await
                    .unwrap_or(None)
                    .flatten();

                let dto = build_table_view_for_service(
                    &self.state,
                    &table,
                    active.as_ref(),
                )
                .await;

                out.push(dto);
            }
        }

        serde_json::to_value(out).unwrap()
    }
}

async fn build_table_view_for_service(
    state: &PokerState,
    table: &Table,
    active: Option<&HandEngineSnapshot>,
) -> TableViewDto {
    let current_actor_seat: Option<u8> =
        active.and_then(|s| s.current_actor).map(|s| s as u8);

    let mut players = Vec::new();

    for (idx, opt) in table.seats.iter().enumerate() {
        if let Some(p) = opt {
            let player_id = p.player_id;

            let display_name = state
                .player_names
                .get(&player_id)
                .await
                .unwrap_or_else(|_| Some(format!("Player #{}", player_id)))
                .unwrap_or_else(|| format!("Player #{}", player_id));

            players.push(PlayerAtTableDto {
                player_id,
                display_name,
                seat_index: idx as u8,
                stack: p.stack,
                current_bet: p.current_bet,
                status: p.status,
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
        dealer_button: table.dealer_button.map(|b| b as u8),
        total_pot: table.total_pot,
        board: table.board.clone(),
        players,
        hand_in_progress: table.hand_in_progress,
        current_actor_seat,
    }
}
