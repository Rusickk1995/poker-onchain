#![cfg_attr(target_arch = "wasm32", no_main)]

use std::sync::Arc;

use async_graphql::{
    EmptySubscription, Enum, Json, Object, Request, Response, Schema, SimpleObject,
};
use linera_sdk::{
    linera_base_types::WithServiceAbi,
    views::{View, ViewStorageContext},
    Service, ServiceRuntime,
};
use serde_json::Value as JsonValue;

use poker_engine::api::commands::{
    AdjustStackCommand,
    AnteTypeApi,
    Command as EngineCommand,
    CreateTableCommand,
    PlayerActionCommand,
    SeatPlayerCommand,
    StartHandCommand,
    TableCommand,
    TickTableCommand,
    UnseatPlayerCommand,
    // --- турнирные команды ---
    TournamentCommand,
    CreateTournamentCommand,
    RegisterPlayerInTournamentCommand,
    UnregisterPlayerFromTournamentCommand,
    StartTournamentCommand,
    AdvanceLevelCommand,
    CloseTournamentCommand,
};
use poker_engine::api::dto::{PlayerAtTableDto, TableViewDto, TournamentViewDto};
use poker_engine::domain::card::Card;
use poker_engine::domain::chips::Chips;
use poker_engine::domain::table::Table;
use poker_engine::domain::tournament::TournamentConfig;
use poker_engine::domain::{PlayerId, SeatIndex, TableId, TournamentId};
use poker_engine::engine::actions::{PlayerAction, PlayerActionKind};

use poker_onchain::{HandEngineSnapshot, Operation, PokerAbi, PokerState};
use poker_onchain::utils::build_tournament_view;

pub struct PokerService {
    state: PokerState,
    runtime: Arc<ServiceRuntime<Self>>,
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

        PokerService {
            state,
            runtime: Arc::new(runtime),
        }
    }

    /// ВАЖНО: теперь handle_query работает с async-graphql Request/Response.
    async fn handle_query(&self, request: Request) -> Response {
        let storage_context = self.runtime.root_view_storage_context();

        let schema = Schema::build(
            QueryRoot {
                runtime: self.runtime.clone(),
                storage_context: storage_context.clone(),
            },
            MutationRoot {
                runtime: self.runtime.clone(),
                storage_context,
            },
            EmptySubscription,
        )
        .finish();

        schema.execute(request).await
    }
}

// ============================================================================
//                               GQL DTO ТИПЫ
// ============================================================================

#[derive(SimpleObject, Clone)]
struct GqlCard {
    rank: String,
    suit: String,
}

#[derive(SimpleObject, Clone)]
struct GqlPlayerAtTable {
    player_id: i64,
    display_name: String,
    seat_index: i32,
    stack: i64,
    current_bet: i64,
    status: String,
    hole_cards: Option<Vec<GqlCard>>,
}

#[derive(SimpleObject, Clone)]
struct GqlTableView {
    table_id: i64,
    name: String,
    max_seats: i32,
    small_blind: i64,
    big_blind: i64,
    ante: i64,
    street: String,
    dealer_button: Option<i32>,
    total_pot: i64,
    board: Vec<GqlCard>,
    players: Vec<GqlPlayerAtTable>,
    hand_in_progress: bool,
    current_actor_seat: Option<i32>,
}

#[derive(SimpleObject, Clone)]
struct GqlTournamentView {
    tournament_id: i64,
    name: String,
    status: String,
    current_level: i32,
    players_registered: i32,
    tables_running: i32,
}

#[derive(SimpleObject)]
struct SummaryGql {
    total_hands_played: i64,
    tables_count: i32,
    tournaments_count: i32,
}

#[derive(SimpleObject)]
struct MutationAck {
    ok: bool,
    message: String,
}

// ============================================================================
//                       GQL ENUMЫ ДЛЯ INPUT (ANTE / ACTION)
// ============================================================================

#[derive(Enum, Copy, Clone, Debug, Eq, PartialEq)]
enum GqlAnteType {
    None,
    Classic,
    BigBlind,
}

#[derive(Enum, Copy, Clone, Debug, Eq, PartialEq)]
enum GqlPlayerActionKind {
    Fold,
    Check,
    Call,
    Bet,
    Raise,
    AllIn,
}

// ============================================================================
//                         ВСПОМОГАТЕЛЬНЫЕ ФУНКЦИИ МАППИНГА
// ============================================================================

fn chips_to_i64(chips: Chips) -> i64 {
    serde_json::to_value(chips)
        .ok()
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as i64
}

fn card_to_gql(card: &Card) -> GqlCard {
    let val: JsonValue = serde_json::to_value(card).unwrap_or(JsonValue::Null);

    let (rank, suit) = match val {
        JsonValue::Object(map) => {
            let rank = match map.get("rank") {
                Some(JsonValue::String(s)) => s.clone(),
                _ => String::new(),
            };
            let suit = match map.get("suit") {
                Some(JsonValue::String(s)) => s.clone(),
                _ => String::new(),
            };
            (rank, suit)
        }
        _ => (String::new(), String::new()),
    };

    GqlCard { rank, suit }
}

fn table_dto_to_gql(dto: &TableViewDto) -> GqlTableView {
    // street как String без ссылок
    let street_val: JsonValue =
        serde_json::to_value(&dto.street).unwrap_or(JsonValue::Null);
    let street = match street_val {
        JsonValue::String(s) => s,
        _ => String::new(),
    };

    let players = dto
        .players
        .iter()
        .map(|p: &PlayerAtTableDto| {
            // status как String без ссылок
            let status_val: JsonValue =
                serde_json::to_value(&p.status).unwrap_or(JsonValue::Null);
            let status = match status_val {
                JsonValue::String(s) => s,
                _ => String::new(),
            };

            let hole_cards = p.hole_cards.as_ref().map(|cards| {
                cards.iter().map(card_to_gql).collect::<Vec<GqlCard>>()
            });

            GqlPlayerAtTable {
                player_id: p.player_id as i64,
                display_name: p.display_name.clone(),
                seat_index: p.seat_index as i32,
                stack: chips_to_i64(p.stack),
                current_bet: chips_to_i64(p.current_bet),
                status,
                hole_cards,
            }
        })
        .collect::<Vec<_>>();

    GqlTableView {
        table_id: dto.table_id as i64,
        name: dto.name.clone(),
        max_seats: dto.max_seats as i32,
        small_blind: chips_to_i64(dto.small_blind),
        big_blind: chips_to_i64(dto.big_blind),
        ante: chips_to_i64(dto.ante),
        street,
        dealer_button: dto.dealer_button.map(|d| d as i32),
        total_pot: chips_to_i64(dto.total_pot),
        board: dto.board.iter().map(card_to_gql).collect(),
        players,
        hand_in_progress: dto.hand_in_progress,
        current_actor_seat: dto.current_actor_seat.map(|s| s as i32),
    }
}

fn tournament_dto_to_gql(dto: &TournamentViewDto) -> GqlTournamentView {
    GqlTournamentView {
        tournament_id: dto.tournament_id as i64,
        name: dto.name.clone(),
        status: dto.status.clone(),
        current_level: dto.current_level as i32,
        players_registered: dto.players_registered as i32,
        tables_running: dto.tables_running as i32,
    }
}

fn to_chips(value: i32) -> Chips {
    Chips(value as u64)
}

// ============================================================================
//           ХЕЛПЕР: СБОРКА TableViewDto ИЗ СТЕЙТА + SNAPSHOT'А ENGINE
// ============================================================================

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

// ============================================================================
//                                 QUERY ROOT
// ============================================================================

struct QueryRoot {
    runtime: Arc<ServiceRuntime<PokerService>>,
    storage_context: ViewStorageContext,
}

#[Object]
impl QueryRoot {
    async fn summary(&self) -> SummaryGql {
        let state =
            PokerState::load(self.storage_context.clone())
                .await
                .expect("Failed to load state in summary");

        let total_hands_played = *state.total_hands_played.get() as i64;

        let tables_count = state
            .tables
            .indices()
            .await
            .unwrap_or_default()
            .len() as i32;

        let tournaments_count = state
            .tournaments
            .indices()
            .await
            .unwrap_or_default()
            .len() as i32;

        SummaryGql {
            total_hands_played,
            tables_count,
            tournaments_count,
        }
    }

    async fn table(&self, table_id: i32) -> Option<GqlTableView> {
        let mut state =
            PokerState::load(self.storage_context.clone())
                .await
                .expect("Failed to load state in table query");

        let table_id: TableId = table_id as u64;

        let table_opt = state
            .tables
            .get(&table_id)
            .await
            .expect("tables.get error");

        let table = match table_opt {
            Some(t) => t,
            None => return None,
        };

        let active = state
            .active_hands
            .get(&table_id)
            .await
            .expect("active_hands.get error")
            .flatten();

        let dto =
            build_table_view_for_service(&state, &table, active.as_ref()).await;

        Some(table_dto_to_gql(&dto))
    }

    async fn tables(&self) -> Vec<GqlTableView> {
        let mut state =
            PokerState::load(self.storage_context.clone())
                .await
                .expect("Failed to load state in tables query");

        let ids = state
            .tables
            .indices()
            .await
            .expect("tables.indices error");

        let mut out = Vec::new();

        for id in ids {
            if let Some(table) =
                state.tables.get(&id).await.unwrap_or(None)
            {
                let active = state
                    .active_hands
                    .get(&id)
                    .await
                    .unwrap_or(None)
                    .flatten();

                let dto = build_table_view_for_service(
                    &state,
                    &table,
                    active.as_ref(),
                )
                .await;

                out.push(table_dto_to_gql(&dto));
            }
        }

        out
    }

    async fn tournaments(&self) -> Vec<GqlTournamentView> {
        let mut state =
            PokerState::load(self.storage_context.clone())
                .await
                .expect("Failed to load state in tournaments query");

        let ids = state
            .tournaments
            .indices()
            .await
            .expect("tournaments.indices error");

        let mut out = Vec::new();

        for id in ids {
            if let Some(t) =
                state.tournaments.get(&id).await.unwrap_or(None)
            {
                let tables_running = state
                    .tournament_tables
                    .get(&id)
                    .await
                    .unwrap_or(None)
                    .map(|v| v.len() as u32)
                    .unwrap_or(0);

                let dto = build_tournament_view(&t, tables_running);
                out.push(tournament_dto_to_gql(&dto));
            }
        }

        out
    }

    async fn tournament_by_id(
        &self,
        tournament_id: i32,
    ) -> Option<GqlTournamentView> {
        let mut state =
            PokerState::load(self.storage_context.clone())
                .await
                .expect("Failed to load state in tournament_by_id query");

        let tournament_id: TournamentId = tournament_id as u64;

        let t_opt = state
            .tournaments
            .get(&tournament_id)
            .await
            .unwrap_or(None);

        let t = match t_opt {
            Some(t) => t,
            None => return None,
        };

        let tables_running = state
            .tournament_tables
            .get(&tournament_id)
            .await
            .unwrap_or(None)
            .map(|v| v.len() as u32)
            .unwrap_or(0);

        let dto = build_tournament_view(&t, tables_running);
        Some(tournament_dto_to_gql(&dto))
    }

    async fn tournament_tables(
        &self,
        tournament_id: i32,
    ) -> Vec<GqlTableView> {
        let mut state =
            PokerState::load(self.storage_context.clone())
                .await
                .expect("Failed to load state in tournament_tables query");

        let tournament_id: TournamentId = tournament_id as u64;

        let table_ids_opt = state
            .tournament_tables
            .get(&tournament_id)
            .await
            .unwrap_or(None);

        let table_ids = match table_ids_opt {
            Some(v) => v,
            None => return Vec::new(),
        };

        let mut out = Vec::new();

        for tid in table_ids {
            if let Some(table) =
                state.tables.get(&tid).await.unwrap_or(None)
            {
                let active = state
                    .active_hands
                    .get(&tid)
                    .await
                    .unwrap_or(None)
                    .flatten();

                let dto = build_table_view_for_service(
                    &state,
                    &table,
                    active.as_ref(),
                )
                .await;

                out.push(table_dto_to_gql(&dto));
            }
        }

        out
    }
}

// ============================================================================
//                               MUTATION ROOT
// ============================================================================

struct MutationRoot {
    runtime: Arc<ServiceRuntime<PokerService>>,
    storage_context: ViewStorageContext,
}

#[Object]
impl MutationRoot {
    /// 1) Создать стол.
    async fn create_table(
        &self,
        table_id: i32,
        name: String,
        max_seats: i32,
        small_blind: i32,
        big_blind: i32,
        ante: i32,
        ante_type: GqlAnteType,
    ) -> MutationAck {
        let table_id: TableId = table_id as u64;

        let ante_type_api = match ante_type {
            GqlAnteType::None => AnteTypeApi::None,
            GqlAnteType::Classic => AnteTypeApi::Classic,
            GqlAnteType::BigBlind => AnteTypeApi::BigBlind,
        };

        let cmd = EngineCommand::CreateTable(CreateTableCommand {
            table_id,
            name,
            max_seats: max_seats as u8,
            small_blind: to_chips(small_blind),
            big_blind: to_chips(big_blind),
            ante: to_chips(ante),
            ante_type: ante_type_api,
        });

        self.runtime
            .schedule_operation(&Operation::Command(cmd));

        MutationAck {
            ok: true,
            message: "CreateTable scheduled".to_string(),
        }
    }

    /// 2) Посадить игрока.
    async fn seat_player(
        &self,
        table_id: i32,
        player_id: i32,
        seat_index: i32,
        display_name: String,
        initial_stack: i32,
    ) -> MutationAck {
        let table_id: TableId = table_id as u64;
        let player_id: PlayerId = player_id as u64;

        let cmd = EngineCommand::TableCommand(TableCommand::SeatPlayer(
            SeatPlayerCommand {
                table_id,
                player_id,
                seat_index: seat_index as u8,
                display_name,
                initial_stack: to_chips(initial_stack),
            },
        ));

        self.runtime
            .schedule_operation(&Operation::Command(cmd));

        MutationAck {
            ok: true,
            message: "SeatPlayer scheduled".to_string(),
        }
    }

    /// 3) Убрать игрока с места.
    async fn unseat_player(
        &self,
        table_id: i32,
        seat_index: i32,
    ) -> MutationAck {
        let table_id: TableId = table_id as u64;

        let cmd = EngineCommand::TableCommand(TableCommand::UnseatPlayer(
            UnseatPlayerCommand {
                table_id,
                seat_index: seat_index as u8,
            },
        ));

        self.runtime
            .schedule_operation(&Operation::Command(cmd));

        MutationAck {
            ok: true,
            message: "UnseatPlayer scheduled".to_string(),
        }
    }

    /// 4) Изменить стек игрока (кэш-ин/кэш-аут).
    async fn adjust_stack(
        &self,
        table_id: i32,
        seat_index: i32,
        delta: i32,
    ) -> MutationAck {
        let table_id: TableId = table_id as u64;

        let cmd = EngineCommand::TableCommand(TableCommand::AdjustStack(
            AdjustStackCommand {
                table_id,
                seat_index: seat_index as u8,
                delta: delta as i64,
            },
        ));

        self.runtime
            .schedule_operation(&Operation::Command(cmd));

        MutationAck {
            ok: true,
            message: "AdjustStack scheduled".to_string(),
        }
    }

    /// 5) Запустить раздачу.
    async fn start_hand(
        &self,
        table_id: i32,
        hand_id: i32,
    ) -> MutationAck {
        let table_id: TableId = table_id as u64;

        let cmd = EngineCommand::TableCommand(TableCommand::StartHand(
            StartHandCommand {
                table_id,
                hand_id: hand_id as u64,
            },
        ));

        self.runtime
            .schedule_operation(&Operation::Command(cmd));

        MutationAck {
            ok: true,
            message: "StartHand scheduled".to_string(),
        }
    }

    /// 6) Игровое действие за столом (fold/check/call/bet/raise/all-in).
    ///
    /// seat / player_id берём из current_actor и стола.
    async fn player_action(
        &self,
        table_id: i32,
        action: GqlPlayerActionKind,
        amount: Option<i32>,
    ) -> MutationAck {
        let table_id: TableId = table_id as u64;

        // 1) Лоадим стейт.
        let state =
            match PokerState::load(self.storage_context.clone()).await {
                Ok(s) => s,
                Err(e) => {
                    return MutationAck {
                        ok: false,
                        message: format!("Failed to load state: {e:?}"),
                    }
                }
            };

        // 2) Берём стол.
        let table_opt = match state.tables.get(&table_id).await {
            Ok(t) => t,
            Err(e) => {
                return MutationAck {
                    ok: false,
                    message: format!("tables.get error: {e:?}"),
                }
            }
        };

        let table = match table_opt {
            Some(t) => t,
            None => {
                return MutationAck {
                    ok: false,
                    message: format!("table_not_found: {table_id}"),
                }
            }
        };

        // 3) Берём active hand snapshot, чтобы понять current_actor.
        let active_snapshot = match state.active_hands.get(&table_id).await {
            Ok(opt) => opt.flatten(),
            Err(e) => {
                return MutationAck {
                    ok: false,
                    message: format!("active_hands.get error: {e:?}"),
                }
            }
        };

        let snapshot = match active_snapshot {
            Some(s) => s,
            None => {
                return MutationAck {
                    ok: false,
                    message: "no_active_hand_for_table".to_string(),
                }
            }
        };

        let seat_index_u8 = match snapshot.current_actor {
            Some(s) => s,
            None => {
                return MutationAck {
                    ok: false,
                    message: "no_current_actor_for_table".to_string(),
                }
            }
        };

        let idx = seat_index_u8 as usize;
        if idx >= table.seats.len() {
            return MutationAck {
                ok: false,
                message: "current_actor_seat_out_of_bounds".to_string(),
            };
        }

        let player_at_seat = match &table.seats[idx] {
            Some(p) => p,
            None => {
                return MutationAck {
                    ok: false,
                    message: "current_actor_seat_empty".to_string(),
                }
            }
        };

        let player_id: PlayerId = player_at_seat.player_id;
        let seat: SeatIndex = seat_index_u8;

        // 4) Маппим экшен + amount в PlayerActionKind.
        let kind: PlayerActionKind = match action {
            GqlPlayerActionKind::Fold => PlayerActionKind::Fold,
            GqlPlayerActionKind::Check => PlayerActionKind::Check,
            GqlPlayerActionKind::Call => PlayerActionKind::Call,
            GqlPlayerActionKind::Bet => {
                let chips = to_chips(amount.unwrap_or(0));
                PlayerActionKind::Bet(chips)
            }
            GqlPlayerActionKind::Raise => {
                let chips = to_chips(amount.unwrap_or(0));
                PlayerActionKind::Raise(chips)
            }
            GqlPlayerActionKind::AllIn => PlayerActionKind::AllIn,
        };

        let pa = PlayerAction {
            player_id,
            seat,
            kind,
        };

        let cmd = EngineCommand::TableCommand(TableCommand::PlayerAction(
            PlayerActionCommand { table_id, action: pa },
        ));

        self.runtime
            .schedule_operation(&Operation::Command(cmd));

        MutationAck {
            ok: true,
            message: "PlayerAction scheduled".to_string(),
        }
    }

    /// 7) Tick таймера стола.
    async fn tick_table(
        &self,
        table_id: i32,
        delta_secs: i32,
    ) -> MutationAck {
        let table_id: TableId = table_id as u64;

        let cmd = EngineCommand::TableCommand(TableCommand::TickTable(
            TickTableCommand {
                table_id,
                delta_secs,
            },
        ));

        self.runtime
            .schedule_operation(&Operation::Command(cmd));

        MutationAck {
            ok: true,
            message: "TickTable scheduled".to_string(),
        }
    }

    // ========================================================================
    //                           ТУРНИРНЫЕ МУТАЦИИ
    // ========================================================================

    /// 8) Создать турнир с полным TournamentConfig.
    ///
    /// config — это JSON, который 1:1 маппится в твой TournamentConfig
    /// через serde. Пример аргумента в GraphQL:
    ///
    ///   config: {
    ///     "name": "...",
    ///     "starting_stack": 5000,
    ///     ...
    ///   }
    async fn create_tournament(
        &self,
        tournament_id: i32,
        config: Json<JsonValue>,
    ) -> MutationAck {
        let tournament_id: TournamentId = tournament_id as u64;

        let cfg: TournamentConfig = match serde_json::from_value(config.0) {
            Ok(c) => c,
            Err(e) => {
                return MutationAck {
                    ok: false,
                    message: format!("Invalid TournamentConfig JSON: {e}"),
                }
            }
        };

        let cmd = EngineCommand::TournamentCommand(
            TournamentCommand::CreateTournament(CreateTournamentCommand {
                tournament_id,
                config: cfg,
            }),
        );

        self.runtime
            .schedule_operation(&Operation::Command(cmd));

        MutationAck {
            ok: true,
            message: "CreateTournament scheduled".to_string(),
        }
    }

    /// 9) Зарегистрировать игрока в турнир.
    async fn register_player_to_tournament(
        &self,
        tournament_id: i32,
        player_id: i32,
        display_name: String,
    ) -> MutationAck {
        let tournament_id: TournamentId = tournament_id as u64;
        let player_id: PlayerId = player_id as u64;

        let cmd = EngineCommand::TournamentCommand(
            TournamentCommand::RegisterPlayer(RegisterPlayerInTournamentCommand {
                tournament_id,
                player_id,
                display_name,
            }),
        );

        self.runtime
            .schedule_operation(&Operation::Command(cmd));

        MutationAck {
            ok: true,
            message: "RegisterPlayer scheduled".to_string(),
        }
    }

    /// 10) Отменить регистрацию игрока в турнире.
    async fn unregister_player_from_tournament(
        &self,
        tournament_id: i32,
        player_id: i32,
    ) -> MutationAck {
        let tournament_id: TournamentId = tournament_id as u64;
        let player_id: PlayerId = player_id as u64;

        let cmd = EngineCommand::TournamentCommand(
            TournamentCommand::UnregisterPlayer(UnregisterPlayerFromTournamentCommand {
                tournament_id,
                player_id,
            }),
        );

        self.runtime
            .schedule_operation(&Operation::Command(cmd));

        MutationAck {
            ok: true,
            message: "UnregisterPlayer scheduled".to_string(),
        }
    }

    /// 11) Старт турнира.
    async fn start_tournament(
        &self,
        tournament_id: i32,
    ) -> MutationAck {
        let tournament_id: TournamentId = tournament_id as u64;

        let cmd = EngineCommand::TournamentCommand(
            TournamentCommand::StartTournament(StartTournamentCommand {
                tournament_id,
            }),
        );

        self.runtime
            .schedule_operation(&Operation::Command(cmd));

        MutationAck {
            ok: true,
            message: "StartTournament scheduled".to_string(),
        }
    }

    /// 12) Перевести турнир на следующий уровень блайндов.
    async fn advance_tournament_level(
        &self,
        tournament_id: i32,
    ) -> MutationAck {
        let tournament_id: TournamentId = tournament_id as u64;

        let cmd = EngineCommand::TournamentCommand(
            TournamentCommand::AdvanceLevel(AdvanceLevelCommand {
                tournament_id,
            }),
        );

        self.runtime
            .schedule_operation(&Operation::Command(cmd));

        MutationAck {
            ok: true,
            message: "AdvanceLevel scheduled".to_string(),
        }
    }

    /// 13) Закрыть турнир (финальный флаг Finished).
    async fn close_tournament(
        &self,
        tournament_id: i32,
    ) -> MutationAck {
        let tournament_id: TournamentId = tournament_id as u64;

        let cmd = EngineCommand::TournamentCommand(
            TournamentCommand::CloseTournament(CloseTournamentCommand {
                tournament_id,
            }),
        );

        self.runtime
            .schedule_operation(&Operation::Command(cmd));

        MutationAck {
            ok: true,
            message: "CloseTournament scheduled".to_string(),
        }
    }
}
