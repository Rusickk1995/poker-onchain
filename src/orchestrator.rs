use linera_sdk::linera_base_types::AccountOwner;
use thiserror::Error;

use poker_engine::api::commands::{
    AdjustStackCommand,
    AnteTypeApi,
    Command,
    CreateTableCommand,
    PlayerActionCommand,
    SeatPlayerCommand,
    StartHandCommand,
    TableCommand,
    TournamentCommand,
    UnseatPlayerCommand,
    CloseTournamentCommand,
    CreateTournamentCommand,
    RegisterPlayerInTournamentCommand,
    StartTournamentCommand,
    UnregisterPlayerFromTournamentCommand,
    TickTableCommand,
    AdvanceLevelCommand,
};
use poker_engine::api::dto::{
    CommandResponse,
    PlayerAtTableDto,
    TableViewDto,
    TournamentViewDto,
    map_hand_status_to_response,
};
use poker_engine::domain::blinds::{AnteType, BlindLevel};
use poker_engine::domain::chips::Chips;
use poker_engine::domain::hand::Street;
use poker_engine::domain::player::PlayerAtTable;
use poker_engine::domain::table::{Table, TableConfig, TableStakes, TableType};
use poker_engine::domain::tournament::{
    Tournament,
    TournamentConfig,
    TournamentError,
    TournamentStatus,
};
use poker_engine::domain::{PlayerId, SeatIndex, TableId, TournamentId};
use poker_engine::engine::{self, HandStatus};
use poker_engine::engine::actions::{PlayerAction, PlayerActionKind};
use poker_engine::infra::rng_seed::RngSeed;
use poker_engine::time_ctrl::{AutoActionDecision, TimeController, TimeProfile};

use crate::{HandEngineSnapshot, PokerState};
use std::collections::HashMap;

/// Ошибки on-chain уровня (storage, авторизация, валидация команд, турнирные ошибки).
#[derive(Debug, Error)]
pub enum OnchainError {
    #[error("storage error: {0}")]
    Storage(String),

    #[error("table {0} not found")]
    TableNotFound(TableId),

    #[error("tournament {0} not found")]
    TournamentNotFound(TournamentId),

    #[error("seat {seat} is not empty at table {table}")]
    SeatNotEmpty { table: TableId, seat: SeatIndex },

    #[error("seat {seat} is invalid at table {table}")]
    InvalidSeatIndex { table: TableId, seat: SeatIndex },

    #[error("no player at seat {seat} on table {table}")]
    NoPlayerAtSeat { table: TableId, seat: SeatIndex },

    #[error("hand already in progress at table {0}")]
    HandAlreadyInProgress(TableId),

    #[error("no active hand on table {0}")]
    NoActiveHand(TableId),

    #[error("engine error: {0}")]
    EngineError(String),

    #[error("unauthenticated signer")]
    Unauthenticated,

    #[error("unauthorized (only owner/admin can do this)")]
    Unauthorized,

    #[error("player id mismatch for this signer")]
    PlayerIdMismatch,

    #[error("tournament error: {0}")]
    Tournament(#[from] TournamentError),

    #[error("tournament already exists: {0}")]
    TournamentAlreadyExists(TournamentId),

    #[error("tournament not running: {0}")]
    TournamentNotRunning(TournamentId),
}

type OnchainResult<T> = Result<T, OnchainError>;

pub struct PokerOrchestrator<'a> {
    pub state: &'a mut PokerState,
    pub signer: Option<AccountOwner>,
}

impl<'a> PokerOrchestrator<'a> {
    pub fn new(state: &'a mut PokerState, signer: Option<AccountOwner>) -> Self {
        Self { state, signer }
    }

    /// Главная точка входа: применить high-level команду.
    /// Внутри работаем через Result, наружу всегда возвращаем CommandResponse.
    pub async fn execute_command(&mut self, cmd: Command) -> CommandResponse {
        let result: OnchainResult<CommandResponse> = match cmd {
            Command::CreateTable(c) => self.handle_create_table(c).await,
            Command::TableCommand(tc) => self.handle_table_command(tc).await,
            Command::TournamentCommand(tc) => {
                self.handle_tournament_command(tc).await
            }
        };

        match result {
            Ok(resp) => resp,
            Err(err) => self.error_response(err),
        }
    }

    /// Преобразование OnchainError → CommandResponse.
    /// Пока отдаём "специальный" TableViewDto с сообщением об ошибке в name.
    fn error_response(&self, err: OnchainError) -> CommandResponse {
        let table = TableViewDto {
            table_id: 0,
            name: format!("ERROR: {err}"),
            max_seats: 0,
            small_blind: Chips(0),
            big_blind: Chips(0),
            ante: Chips(0),
            street: Street::Preflop,
            dealer_button: None,
            total_pot: Chips(0),
            board: Vec::new(),
            players: Vec::new(),
            hand_in_progress: false,
            current_actor_seat: None,
        };

        CommandResponse::TableState(table)
    }

    // =====================================================================
    //                         ВСПОМОГАТЕЛЬНАЯ АВТОРИЗАЦИЯ
    // =====================================================================

    async fn app_owner(&self) -> Option<AccountOwner> {
        *self.state.owner.get()
    }

    async fn ensure_admin(&self) -> OnchainResult<()> {
        let signer = self.signer.ok_or(OnchainError::Unauthenticated)?;
        let owner = self
            .app_owner()
            .await
            .ok_or(OnchainError::Unauthorized)?;

        if signer != owner {
            return Err(OnchainError::Unauthorized);
        }

        Ok(())
    }

    /// Привязать signer ↔ player_id (один раз) и проверять соответствие.
    async fn ensure_player_for_signer(
        &mut self,
        player_id: PlayerId,
    ) -> OnchainResult<PlayerId> {
        let signer = self.signer.ok_or(OnchainError::Unauthenticated)?;

        if let Some(existing) = self
            .state
            .account_players
            .get(&signer)
            .await
            .map_err(|e| OnchainError::Storage(e.to_string()))?
        {
            if existing != player_id {
                return Err(OnchainError::PlayerIdMismatch);
            }
            Ok(existing)
        } else {
            // Первая привязка.
            self.state
                .account_players
                .insert(&signer, player_id)
                .map_err(|e| OnchainError::Storage(e.to_string()))?;

            self.state
                .player_accounts
                .insert(&player_id, signer)
                .map_err(|e| OnchainError::Storage(e.to_string()))?;

            Ok(player_id)
        }
    }

    // =====================================================================
    //                           CASH / TABLE COMMANDS
    // =====================================================================

    async fn handle_create_table(
        &mut self,
        cmd: CreateTableCommand,
    ) -> OnchainResult<CommandResponse> {
        // Admin-only.
        self.ensure_admin().await?;

        if self
            .state
            .tables
            .get(&cmd.table_id)
            .await
            .map_err(|e| OnchainError::Storage(e.to_string()))?
            .is_some()
        {
            return Err(OnchainError::Storage(format!(
                "table {} already exists",
                cmd.table_id
            )));
        }

        let stakes = TableStakes::new(
            cmd.small_blind,
            cmd.big_blind,
            map_ante_type(cmd.ante_type),
            cmd.ante,
        );

        let config = TableConfig {
            max_seats: cmd.max_seats,
            table_type: TableType::Cash,
            stakes,
            allow_straddle: false,
            allow_run_it_twice: false,
        };

        let table = Table::new(cmd.table_id, cmd.name.clone(), config);

        self.state
            .tables
            .insert(&cmd.table_id, table.clone())
            .map_err(|e| OnchainError::Storage(e.to_string()))?;

        self.state
            .active_hands
            .insert(&cmd.table_id, None)
            .map_err(|e| OnchainError::Storage(e.to_string()))?;

        let table_view = self.build_table_view(&table, None).await?;

        Ok(CommandResponse::TableCreated(table_view))
    }

    async fn handle_table_command(
        &mut self,
        cmd: TableCommand,
    ) -> OnchainResult<CommandResponse> {
        match cmd {
            TableCommand::SeatPlayer(c) => self.handle_seat_player(c).await,
            TableCommand::UnseatPlayer(c) => self.handle_unseat_player(c).await,
            TableCommand::AdjustStack(c) => self.handle_adjust_stack(c).await,
            TableCommand::StartHand(c) => self.handle_start_hand(c).await,
            TableCommand::PlayerAction(c) => self.handle_player_action(c).await,
            TableCommand::TickTable(c) => self.handle_tick_table(c).await,
        }
    }

    async fn handle_seat_player(
        &mut self,
        cmd: SeatPlayerCommand,
    ) -> OnchainResult<CommandResponse> {
        // Привязываем signer ↔ player_id.
        let player_id = self.ensure_player_for_signer(cmd.player_id).await?;

        let mut table = self.load_table(cmd.table_id).await?;
        let seat: SeatIndex = cmd.seat_index as SeatIndex;

        if !table.is_seat_empty(seat) {
            return Err(OnchainError::SeatNotEmpty {
                table: table.id,
                seat,
            });
        }

        let pat = PlayerAtTable::new(player_id, cmd.initial_stack);

        if let Some(slot) = table.seats.get_mut(seat as usize) {
            *slot = Some(pat);
        } else {
            return Err(OnchainError::InvalidSeatIndex {
                table: table.id,
                seat,
            });
        }

        if !cmd.display_name.is_empty() {
            self.state
                .player_names
                .insert(&player_id, cmd.display_name.clone())
                .map_err(|e| OnchainError::Storage(e.to_string()))?;
        }

        self.save_table(table.clone())?;

        let active_snapshot = self.load_active_snapshot(table.id).await?;
        let table_view = self
            .build_table_view(&table, active_snapshot.as_ref())
            .await?;

        Ok(CommandResponse::TableState(table_view))
    }

    async fn handle_unseat_player(
        &mut self,
        cmd: UnseatPlayerCommand,
    ) -> OnchainResult<CommandResponse> {
        let mut table = self.load_table(cmd.table_id).await?;
        let seat: SeatIndex = cmd.seat_index as SeatIndex;

        if let Some(slot) = table.seats.get_mut(seat as usize) {
            *slot = None;
        } else {
            return Err(OnchainError::InvalidSeatIndex {
                table: table.id,
                seat,
            });
        }

        self.save_table(table.clone())?;

        let active_snapshot = self.load_active_snapshot(table.id).await?;
        let table_view = self
            .build_table_view(&table, active_snapshot.as_ref())
            .await?;

        Ok(CommandResponse::TableState(table_view))
    }

    async fn handle_adjust_stack(
        &mut self,
        cmd: AdjustStackCommand,
    ) -> OnchainResult<CommandResponse> {
        // Admin-only.
        self.ensure_admin().await?;

        let mut table = self.load_table(cmd.table_id).await?;
        let seat: SeatIndex = cmd.seat_index as SeatIndex;

        let delta = cmd.delta;

        if let Some(Some(player)) = table.seats.get_mut(seat as usize) {
            if delta >= 0 {
                player.stack += Chips(delta as u64);
            } else {
                let abs = (-delta) as u64;
                if player.stack.0 >= abs {
                    player.stack -= Chips(abs);
                } else {
                    player.stack = Chips::ZERO;
                }
            }
        } else {
            return Err(OnchainError::NoPlayerAtSeat {
                table: table.id,
                seat,
            });
        }

        self.save_table(table.clone())?;

        let active_snapshot = self.load_active_snapshot(table.id).await?;
        let table_view = self
            .build_table_view(&table, active_snapshot.as_ref())
            .await?;

        Ok(CommandResponse::TableState(table_view))
    }

    async fn handle_start_hand(
        &mut self,
        cmd: StartHandCommand,
    ) -> OnchainResult<CommandResponse> {
        let mut table = self.load_table(cmd.table_id).await?;

        if table.hand_in_progress {
            return Err(OnchainError::HandAlreadyInProgress(table.id));
        }

        // Берём hand_id из глобального счётчика.
        let current_id = *self.state.next_hand_id.get();
        let hand_id = current_id.saturating_add(1);
        self.state.next_hand_id.set(hand_id);

        let base_seed = *self.state.base_seed.get();
        let seed = RngSeed::from_u64(base_seed ^ hand_id ^ table.id as u64);
        let mut rng = seed.to_rng();

        let mut engine =
            engine::start_hand(&mut table, &mut rng, hand_id).map_err(|e| {
                OnchainError::EngineError(format!("start_hand failed: {e:?}"))
            })?;

        let total = *self.state.total_hands_played.get();
        self.state.total_hands_played
            .set(total.saturating_add(1));

        let snapshot = HandEngineSnapshot::from_engine(&engine);
        self.state
            .active_hands
            .insert(&table.id, Some(snapshot.clone()))
            .map_err(|e| OnchainError::Storage(e.to_string()))?;

        self.save_table(table.clone())?;

        // Тайм-контроль: инициализируем или обновляем контроллер под первого актёра.
        self.update_time_controller_for_actor(&table, engine.current_actor)
            .await?;

        let table_view = self
            .build_table_view(&table, Some(&snapshot))
            .await?;

        Ok(CommandResponse::TableState(table_view))
    }

    async fn handle_player_action(
        &mut self,
        cmd: PlayerActionCommand,
    ) -> OnchainResult<CommandResponse> {
        let mut table = self.load_table(cmd.table_id).await?;

        let snapshot_opt = self
            .load_active_snapshot(cmd.table_id)
            .await?;
        let snapshot = snapshot_opt.ok_or(OnchainError::NoActiveHand(cmd.table_id))?;

        let mut engine = snapshot.into_engine();

        let mut status =
            engine::apply_action(&mut table, &mut engine, cmd.action.clone())
                .map_err(|e| {
                    OnchainError::EngineError(format!(
                        "apply_action failed: {e:?}"
                    ))
                })?;

        if let Ok(next_status) = engine::advance_if_needed(&mut table, &mut engine) {
            status = next_status;
        }

        let snapshot_after = HandEngineSnapshot::from_engine(&engine);
        self.save_table(table.clone())?;

        let response = match status {
            HandStatus::Ongoing => {
                // Обновляем active_hands и тайм-контроллер.
                self.state
                    .active_hands
                    .insert(&table.id, Some(snapshot_after.clone()))
                    .map_err(|e| OnchainError::Storage(e.to_string()))?;

                self.update_time_controller_for_actor(&table, engine.current_actor)
                    .await?;

                let table_view = self
                    .build_table_view(&table, Some(&snapshot_after))
                    .await?;
                CommandResponse::TableState(table_view)
            }
            finished_status => {
                self.state
                    .active_hands
                    .insert(&table.id, None)
                    .map_err(|e| OnchainError::Storage(e.to_string()))?;

                // Сбрасываем текущий ход, но не обнуляем таймбанк.
                if let Err(e) = self.clear_current_turn_for_table(table.id).await {
                    // Не ломаем игру, если что-то пошло не так с таймбанком.
                    eprintln!("time controller clear error: {e:?}");
                }

                // Турнирный хук.
                if let Some(tournament_id) =
                    self.table_tournament_id(table.id).await?
                {
                    self.handle_tournament_after_hand(
                        tournament_id,
                        &table,
                    )
                    .await?;
                }

                let table_view = self
                    .build_table_view(&table, Some(&snapshot_after))
                    .await?;
                map_hand_status_to_response(finished_status, table_view)
            }
        };

        Ok(response)
    }

    /// Tick-команда для тайм-контроля (ЭТАП 7):
    /// - двигаем часы;
    /// - если произошёл timeout — делаем auto-fold от имени игрока;
    /// - возвращаем актуальное состояние стола.
    async fn handle_tick_table(
        &mut self,
        cmd: TickTableCommand,
    ) -> OnchainResult<CommandResponse> {
        let mut table = self.load_table(cmd.table_id).await?;

        let snapshot_opt = self.load_active_snapshot(cmd.table_id).await?;
        let snapshot = match snapshot_opt {
            Some(s) => s,
            None => {
                // Нет активной раздачи — просто вернуть состояние стола.
                let table_view = self.build_table_view(&table, None).await?;
                return Ok(CommandResponse::TableState(table_view));
            }
        };

        let mut engine = snapshot.into_engine();
        let mut ctrl = self.ensure_time_controller(&table).await?;

        let decision = ctrl.on_time_passed(cmd.delta_secs);

        match decision {
            AutoActionDecision::None => {
                // Просто обновляем контроллер и отдаём текущее состояние.
                self.state
                    .time_controllers
                    .insert(&table.id, ctrl)
                    .map_err(|e| OnchainError::Storage(e.to_string()))?;

                let snapshot = HandEngineSnapshot::from_engine(&engine);
                let table_view = self
                    .build_table_view(&table, Some(&snapshot))
                    .await?;
                Ok(CommandResponse::TableState(table_view))
            }
            AutoActionDecision::TimeoutCheckOrFold { player_id } => {
                // Ищем seat этого игрока.
                let seat = self
                    .find_seat_by_player(table.id, player_id)
                    .await?;

                let action = PlayerAction {
                    seat,
                    player_id,
                    kind: PlayerActionKind::Fold,
                };

                let mut status =
                    engine::apply_action(&mut table, &mut engine, action)
                        .map_err(|e| {
                            OnchainError::EngineError(format!(
                                "auto-fold failed: {e:?}"
                            ))
                        })?;

                if let Ok(next_status) =
                    engine::advance_if_needed(&mut table, &mut engine)
                {
                    status = next_status;
                }

                let snapshot_after = HandEngineSnapshot::from_engine(&engine);
                self.save_table(table.clone())?;

                let response = match status {
                    HandStatus::Ongoing => {
                        self.state
                            .active_hands
                            .insert(&table.id, Some(snapshot_after.clone()))
                            .map_err(|e| OnchainError::Storage(e.to_string()))?;

                        // Переинициализируем ход для нового актёра.
                        self.update_time_controller_for_actor(
                            &table,
                            engine.current_actor,
                        )
                        .await?;

                        let table_view = self
                            .build_table_view(&table, Some(&snapshot_after))
                            .await?;
                        CommandResponse::TableState(table_view)
                    }
                    finished_status => {
                        self.state
                            .active_hands
                            .insert(&table.id, None)
                            .map_err(|e| OnchainError::Storage(e.to_string()))?;

                        // Сбрасываем current_turn в тайм-контроллере.
                        if let Err(e) =
                            self.clear_current_turn_for_table(table.id).await
                        {
                            eprintln!("time controller clear error: {e:?}");
                        }

                        if let Some(tournament_id) =
                            self.table_tournament_id(table.id).await?
                        {
                            self.handle_tournament_after_hand(
                                tournament_id,
                                &table,
                            )
                            .await?;
                        }

                        let table_view = self
                            .build_table_view(&table, Some(&snapshot_after))
                            .await?;
                        map_hand_status_to_response(
                            finished_status,
                            table_view,
                        )
                    }
                };

                Ok(response)
            }
        }
    }

    // =====================================================================
    //                          TOURNAMENT COMMANDS
    // =====================================================================

    async fn handle_tournament_command(
        &mut self,
        cmd: TournamentCommand,
    ) -> OnchainResult<CommandResponse> {
        match cmd {
            TournamentCommand::CreateTournament(c) => {
                self.handle_create_tournament(c).await
            }
            TournamentCommand::RegisterPlayer(c) => {
                self.handle_register_player_in_tournament(c).await
            }
            TournamentCommand::UnregisterPlayer(c) => {
                self.handle_unregister_player_from_tournament(c).await
            }
            TournamentCommand::StartTournament(c) => {
                self.handle_start_tournament(c).await
            }
            TournamentCommand::AdvanceLevel(c) => {
                self.handle_advance_tournament_level(c).await
            }
            TournamentCommand::CloseTournament(c) => {
                self.handle_close_tournament(c).await
            }
        }
    }

    async fn handle_create_tournament(
        &mut self,
        cmd: CreateTournamentCommand,
    ) -> OnchainResult<CommandResponse> {
        self.ensure_admin().await?;

        if self
            .state
            .tournaments
            .get(&cmd.tournament_id)
            .await
            .map_err(|e| OnchainError::Storage(e.to_string()))?
            .is_some()
        {
            return Err(OnchainError::TournamentAlreadyExists(
                cmd.tournament_id,
            ));
        }

        // Владелец турнира как player_id — пока просто 0 (системный),
        // логика призов/пули у тебя внутри движка.
        let owner_player: PlayerId = 0;

        let tournament = Tournament::new(
            cmd.tournament_id,
            owner_player,
            cmd.config.clone(),
        )?;

        self.state
            .tournaments
            .insert(&cmd.tournament_id, tournament.clone())
            .map_err(|e| OnchainError::Storage(e.to_string()))?;

        self.state
            .tournament_tables
            .insert(&cmd.tournament_id, Vec::new())
            .map_err(|e| OnchainError::Storage(e.to_string()))?;

        let view =
            self.build_tournament_view(&tournament, Vec::new()).await?;

        Ok(CommandResponse::TournamentState(view))
    }

    async fn handle_register_player_in_tournament(
        &mut self,
        cmd: RegisterPlayerInTournamentCommand,
    ) -> OnchainResult<CommandResponse> {
        let player_id = self.ensure_player_for_signer(cmd.player_id).await?;

        let mut tournament = self
            .load_tournament(cmd.tournament_id)
            .await?;

        tournament.register_player(player_id)?;

        self.state
            .tournaments
            .insert(&cmd.tournament_id, tournament.clone())
            .map_err(|e| OnchainError::Storage(e.to_string()))?;

        if !cmd.display_name.is_empty() {
            self.state
                .player_names
                .insert(&player_id, cmd.display_name.clone())
                .map_err(|e| OnchainError::Storage(e.to_string()))?;
        }

        let table_ids = self
            .state
            .tournament_tables
            .get(&cmd.tournament_id)
            .await
            .map_err(|e| OnchainError::Storage(e.to_string()))?
            .unwrap_or_default();

        let view = self.build_tournament_view(&tournament, table_ids).await?;

        Ok(CommandResponse::TournamentState(view))
    }

    async fn handle_unregister_player_from_tournament(
        &mut self,
        cmd: UnregisterPlayerFromTournamentCommand,
    ) -> OnchainResult<CommandResponse> {
        let player_id = self.ensure_player_for_signer(cmd.player_id).await?;

        let mut tournament = self
            .load_tournament(cmd.tournament_id)
            .await?;

        // Разрегистрация реализована здесь, т.к. в домене метода нет.
        if tournament.status != TournamentStatus::Registering {
            return Err(TournamentError::InvalidStatus {
                expected: TournamentStatus::Registering,
                found: tournament.status,
            }
            .into());
        }

        if tournament
            .registrations
            .remove(&player_id)
            .is_none()
        {
            return Err(TournamentError::NotRegistered {
                player_id,
                tournament_id: cmd.tournament_id,
            }
            .into());
        }

        self.state
            .tournaments
            .insert(&cmd.tournament_id, tournament.clone())
            .map_err(|e| OnchainError::Storage(e.to_string()))?;

        let table_ids = self
            .state
            .tournament_tables
            .get(&cmd.tournament_id)
            .await
            .map_err(|e| OnchainError::Storage(e.to_string()))?
            .unwrap_or_default();

        let view = self.build_tournament_view(&tournament, table_ids).await?;

        Ok(CommandResponse::TournamentState(view))
    }

    async fn handle_start_tournament(
        &mut self,
        cmd: StartTournamentCommand,
    ) -> OnchainResult<CommandResponse> {
        self.ensure_admin().await?;

        let mut tournament = self
            .load_tournament(cmd.tournament_id)
            .await?;

        let config = &tournament.config;
        let max_seats = config.table_size;

        // Все зарегистрированные игроки.
        let registrations = tournament.registrations.clone();
        let mut player_ids: Vec<PlayerId> =
            registrations.keys().cloned().collect();
        player_ids.sort_unstable();

        let mut new_table_ids = Vec::new();
        let mut tables_to_insert = Vec::new();

        let mut chunk_index: u32 = 0;
        for chunk in player_ids.chunks(max_seats as usize) {
            if chunk.is_empty() {
                continue;
            }

            // Простая схема: кодируем table_id из tournament_id + локального индекса.
            let table_id: TableId =
                ((cmd.tournament_id as u64) << 32 | (chunk_index as u64))
                    as TableId;
            chunk_index += 1;

            let stakes =
                stakes_for_tournament_level(config, tournament.current_level);

            let table_config = TableConfig {
                max_seats,
                table_type: TableType::Tournament,
                stakes,
                allow_straddle: false,
                allow_run_it_twice: false,
            };

            let mut table = Table::new(
                table_id,
                format!("T#{}/{}", cmd.tournament_id, chunk_index),
                table_config,
            );

            for (seat_idx, pid) in chunk.iter().enumerate() {
                if let Some(reg) = tournament.registrations.get_mut(pid) {
                    let stack = reg.total_chips;

                    reg.table_id = Some(table_id);
                    reg.seat_index = Some(seat_idx as SeatIndex);

                    let pat = PlayerAtTable::new(*pid, stack);
                    if let Some(slot) = table.seats.get_mut(seat_idx) {
                        *slot = Some(pat);
                    }
                }
            }

            new_table_ids.push(table_id);
            tables_to_insert.push(table);
        }

        for table in tables_to_insert.into_iter() {
            let id = table.id;
            self.state
                .tables
                .insert(&id, table)
                .map_err(|e| OnchainError::Storage(e.to_string()))?;

            self.state
                .active_hands
                .insert(&id, None)
                .map_err(|e| OnchainError::Storage(e.to_string()))?;

            self.state
                .table_tournament
                .insert(&id, cmd.tournament_id)
                .map_err(|e| OnchainError::Storage(e.to_string()))?;
        }

        // Переводим турнир в Running через доменный метод.
        // now_ts = 0 (для dev/теста); при реальном запуске можно прокинуть реальное время.
        tournament.start(0)?;

        self.state
            .tournaments
            .insert(&cmd.tournament_id, tournament.clone())
            .map_err(|e| OnchainError::Storage(e.to_string()))?;

        self.state
            .tournament_tables
            .insert(&cmd.tournament_id, new_table_ids.clone())
            .map_err(|e| OnchainError::Storage(e.to_string()))?;

        let view =
            self.build_tournament_view(&tournament, new_table_ids).await?;

        Ok(CommandResponse::TournamentState(view))
    }

    async fn handle_advance_tournament_level(
        &mut self,
        cmd: AdvanceLevelCommand,
    ) -> OnchainResult<CommandResponse> {
        self.ensure_admin().await?;

        let mut tournament = self
            .load_tournament(cmd.tournament_id)
            .await?;

        // Простая логика: ручной перевод на следующий уровень,
        // если он существует в blind_structure.
        let next_level = tournament.current_level.saturating_add(1);
        if tournament
            .config
            .blind_structure
            .level_by_number(next_level)
            .is_some()
        {
            tournament.current_level = next_level;
        } else {
            // Нет следующего уровня – просто возвращаем текущее состояние.
        }

        let table_ids = self
            .state
            .tournament_tables
            .get(&cmd.tournament_id)
            .await
            .map_err(|e| OnchainError::Storage(e.to_string()))?
            .unwrap_or_default();

        let stakes =
            stakes_for_tournament_level(&tournament.config, tournament.current_level);

        for table_id in table_ids.iter().copied() {
            if let Some(mut table) = self
                .state
                .tables
                .get(&table_id)
                .await
                .map_err(|e| OnchainError::Storage(e.to_string()))?
            {
                if table.config.table_type == TableType::Tournament {
                    table.config.stakes = stakes.clone();
                    self.save_table(table)?;
                }
            }
        }

        self.state
            .tournaments
            .insert(&cmd.tournament_id, tournament.clone())
            .map_err(|e| OnchainError::Storage(e.to_string()))?;

        let view = self.build_tournament_view(&tournament, table_ids).await?;
        Ok(CommandResponse::TournamentState(view))
    }

    async fn handle_close_tournament(
        &mut self,
        cmd: CloseTournamentCommand,
    ) -> OnchainResult<CommandResponse> {
        self.ensure_admin().await?;

        let mut tournament = self
            .load_tournament(cmd.tournament_id)
            .await?;

        tournament.status = TournamentStatus::Finished;

        self.state
            .tournaments
            .insert(&cmd.tournament_id, tournament.clone())
            .map_err(|e| OnchainError::Storage(e.to_string()))?;

        let table_ids = self
            .state
            .tournament_tables
            .get(&cmd.tournament_id)
            .await
            .map_err(|e| OnchainError::Storage(e.to_string()))?
            .unwrap_or_default();

        let view = self.build_tournament_view(&tournament, table_ids).await?;
        Ok(CommandResponse::TournamentState(view))
    }

        /// Хук, вызываемый после завершения раздачи на турнирном столе.
    ///
    /// Здесь мы:
    /// 1) синхронизируем Tournament с реальным состоянием столов (стеки, места);
    /// 2) отмечаем bust игроков с нулевым стеком;
    /// 3) считаем и применяем ребалансировку столов (compute_rebalance_moves);
    /// 4) физически пересаживаем игроков между столами;
    /// 5) чистим пустые столы и обновляем tournament_tables.
    async fn handle_tournament_after_hand(
        &mut self,
        tournament_id: TournamentId,
        _table: &Table,
    ) -> OnchainResult<()> {
        // 1. Загружаем турнир и проверяем статус.
        let mut tournament = self.load_tournament(tournament_id).await?;

        if tournament.status != TournamentStatus::Running {
            // В регистрационной, паузе или после завершения — ничего не делаем.
            return Ok(());
        }

        // 2. Берём список столов турнира.
        let table_ids = self
            .state
            .tournament_tables
            .get(&tournament_id)
            .await
            .map_err(|e| OnchainError::Storage(e.to_string()))?
            .unwrap_or_default();

        if table_ids.is_empty() {
            // Нет столов, но турнир почему-то Running — не ломаемся, просто выходим.
            return Ok(());
        }

        // 3. Грузим все столы турнира в память.
        let mut tables: HashMap<TableId, Table> = HashMap::new();

        for tid in &table_ids {
            if let Some(table) = self
                .state
                .tables
                .get(tid)
                .await
                .map_err(|e| OnchainError::Storage(e.to_string()))?
            {
                tables.insert(*tid, table);
            }
        }

        if tables.is_empty() {
            // Столы не нашлись в storage — защитный выход.
            return Ok(());
        }

        // 4. Строим карту: player_id -> (table_id, seat_index, stack).
        let mut player_locations: HashMap<PlayerId, (TableId, SeatIndex, Chips)> =
            HashMap::new();

        for (tid, table) in tables.iter() {
            for (idx, seat_opt) in table.seats.iter().enumerate() {
                if let Some(p) = seat_opt {
                    player_locations.insert(
                        p.player_id,
                        (*tid, idx as SeatIndex, p.stack),
                    );
                }
            }
        }

        // 5. Синхронизируем Tournament.registrations со стеками/местами
        //    и собираем кандидатов на bust (stack == 0).
        let mut busted_candidates: Vec<PlayerId> = Vec::new();

        for (player_id, reg) in tournament.registrations.iter_mut() {
            if reg.is_busted {
                continue;
            }

            if let Some((tid, seat, stack)) = player_locations.get(player_id) {
                // Игрок реально сидит за каким-то столом — синхронизируем данные.
                reg.table_id = Some(*tid);
                reg.seat_index = Some(*seat);
                reg.total_chips = *stack;

                // Нулевой стек → кандидат на вылет.
                if stack.is_zero() {
                    busted_candidates.push(*player_id);
                }
            } else if reg.total_chips.is_zero() {
                // Игрок нигде не сидит и у него 0 фишек — считаем вылетевшим.
                busted_candidates.push(*player_id);
            }
        }

        // 6. Отмечаем bust в Tournament + убираем игроков со столов.
        for player_id in busted_candidates.into_iter() {
            // Убираем игрока со стола, если он там ещё числится.
            if let Some((tid, seat, _stack)) = player_locations.get(&player_id).copied() {
                if let Some(table) = tables.get_mut(&tid) {
                    if let Some(slot) = table.seats.get_mut(seat as usize) {
                        *slot = None;
                    }
                }
            }

            // Помечаем вылет в доменной модели турнира.
            if let Err(err) = tournament.mark_player_busted(player_id) {
                match err {
                    // Защитный кейс: домен не даёт выбить последнего живого игрока.
                    TournamentError::CannotBustLastPlayer { .. } => {
                        // Просто игнорируем этот конкретный вызов.
                    }
                    other => {
                        return Err(OnchainError::Tournament(other));
                    }
                }
            }
        }

        // После возможных вылетов домен сам проверит,
        // не нужно ли завершить турнир (check_and_finish_if_needed внутри).

        // 7. Считаем ребалансировку столов по доменной логике.
        let moves = tournament.compute_rebalance_moves();

        if !moves.is_empty() {
            // Карта: player_id -> новый seat_index (по факту, как посадили за стол).
            let mut new_seats: HashMap<PlayerId, SeatIndex> = HashMap::new();

            // 7.1. Физически пересаживаем игроков между столами (таблицы в памяти).
            for m in &moves {
                // Считываем исходный стол.
                let from_table_opt = tables.get_mut(&m.from_table);
                if from_table_opt.is_none() {
                    continue;
                }
                let from_table = from_table_opt.unwrap();

                // Ищем игрока на исходном столе.
                let mut moved_player: Option<PlayerAtTable> = None;
                for (idx, seat_opt) in from_table.seats.iter_mut().enumerate() {
                    if let Some(p) = seat_opt {
                        if p.player_id == m.player_id {
                            moved_player = Some(p.clone());
                            *seat_opt = None;
                            break;
                        }
                    }
                }

                let moved_player = match moved_player {
                    Some(p) => p,
                    None => continue,
                };

                // Садим игрока на целевой стол в первое свободное место.
                if let Some(to_table) = tables.get_mut(&m.to_table) {
                    if let Some((seat_idx, slot)) = to_table
                        .seats
                        .iter_mut()
                        .enumerate()
                        .find(|(_, s)| s.is_none())
                    {
                        *slot = Some(moved_player);
                        new_seats.insert(m.player_id, seat_idx as SeatIndex);
                    }
                }
            }

            // 7.2. Обновляем логическое состояние турнира (table_id / seat_index).
            tournament.apply_rebalance_moves(&moves);

            // В apply_rebalance_moves seat_index сбрасывается в None.
            // Здесь мы проставляем фактические места по тем переносам,
            // которые реально смогли выполнить на столах.
            for (player_id, seat_index) in new_seats.into_iter() {
                if let Some(reg) = tournament.registrations.get_mut(&player_id) {
                    reg.seat_index = Some(seat_index);
                }
            }
        }

        // 8. Чистим пустые столы и сохраняем обновлённые.
        let mut new_table_ids: Vec<TableId> = Vec::new();

        for (tid, table) in tables.into_iter() {
            if table.seated_count() == 0 {
                // Полностью пустой стол — убираем из стораджа и индексов турнира.
                self.state
                    .tables
                    .remove(&tid)
                    .map_err(|e| OnchainError::Storage(e.to_string()))?;
                self.state
                    .active_hands
                    .remove(&tid)
                    .map_err(|e| OnchainError::Storage(e.to_string()))?;
                self.state
                    .table_tournament
                    .remove(&tid)
                    .map_err(|e| OnchainError::Storage(e.to_string()))?;
                self.state
                    .time_controllers
                    .remove(&tid)
                    .map_err(|e| OnchainError::Storage(e.to_string()))?;
                continue;
            }

            // Стол живой — сохраняем его обратно.
            self.save_table(table)?;
            new_table_ids.push(tid);
        }

        // 9. Обновляем mapping: турнир → его столы.
        self.state
            .tournament_tables
            .insert(&tournament_id, new_table_ids)
            .map_err(|e| OnchainError::Storage(e.to_string()))?;

        // 10. Сохраняем обновлённый турнир.
        self.state
            .tournaments
            .insert(&tournament_id, tournament)
            .map_err(|e| OnchainError::Storage(e.to_string()))?;

        Ok(())
    }


    // =====================================================================
    //                               HELPERS
    // =====================================================================

    async fn load_table(&self, id: TableId) -> OnchainResult<Table> {
        self.state
            .tables
            .get(&id)
            .await
            .map_err(|e| OnchainError::Storage(e.to_string()))?
            .ok_or(OnchainError::TableNotFound(id))
    }

    fn save_table(&mut self, table: Table) -> OnchainResult<()> {
        let id = table.id;
        self.state
            .tables
            .insert(&id, table)
            .map_err(|e| OnchainError::Storage(e.to_string()))
    }

    async fn load_active_snapshot(
        &self,
        table_id: TableId,
    ) -> OnchainResult<Option<HandEngineSnapshot>> {
        let maybe = self
            .state
            .active_hands
            .get(&table_id)
            .await
            .map_err(|e| OnchainError::Storage(e.to_string()))?;
        Ok(maybe.flatten())
    }

    async fn table_tournament_id(
        &self,
        table_id: TableId,
    ) -> OnchainResult<Option<TournamentId>> {
        self.state
            .table_tournament
            .get(&table_id)
            .await
            .map_err(|e| OnchainError::Storage(e.to_string()))
    }

    async fn load_tournament(
        &mut self,
        id: TournamentId,
    ) -> OnchainResult<Tournament> {
        self.state
            .tournaments
            .get(&id)
            .await
            .map_err(|e| OnchainError::Storage(e.to_string()))?
            .ok_or(OnchainError::TournamentNotFound(id))
    }

    /// Собрать TableViewDto из доменного Table + опционального снапшота раздачи.
    pub async fn build_table_view(
        &self,
        table: &Table,
        active: Option<&HandEngineSnapshot>,
    ) -> OnchainResult<TableViewDto> {
        let current_actor_seat = active
            .and_then(|s| s.current_actor)
            .map(|s| s as u8);

        let mut players = Vec::new();

        for (idx, opt_player) in table.seats.iter().enumerate() {
            if let Some(p) = opt_player {
                let seat_index = idx as u8;
                let player_id = p.player_id;

                let display_name = self
                    .state
                    .player_names
                    .get(&player_id)
                    .await
                    .map_err(|e| OnchainError::Storage(e.to_string()))?
                    .unwrap_or_else(|| format!("Player #{}", player_id));

                players.push(PlayerAtTableDto {
                    player_id,
                    display_name,
                    seat_index,
                    stack: p.stack,
                    current_bet: p.current_bet,
                    status: p.status,
                    hole_cards: None,
                });
            }
        }

        Ok(TableViewDto {
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
        })
    }

    async fn build_tournament_view(
        &self,
        tournament: &Tournament,
        table_ids: Vec<TableId>,
    ) -> OnchainResult<TournamentViewDto> {
        Ok(TournamentViewDto {
            tournament_id: tournament.id,
            name: tournament.config.name.clone(),
            status: format!("{:?}", tournament.status),
            current_level: tournament.current_level,
            players_registered: tournament.registrations.len() as u32,
            tables_running: table_ids.len() as u32,
        })
    }

    /// Обеспечить наличие TimeController для стола.
        /// Обеспечить наличие TimeController для стола.
    async fn ensure_time_controller(
        &self,
        table: &Table,
    ) -> OnchainResult<TimeController> {
        let existing = self
            .state
            .time_controllers
            .get(&table.id)
            .await
            .map_err(|e| OnchainError::Storage(e.to_string()))?;

        if let Some(ctrl) = existing {
            Ok(ctrl)
        } else {
            let mut ctrl = TimeController::new(TimeProfile::Standard);
            let players = table
                .seats
                .iter()
                .filter_map(|s| s.as_ref().map(|p| p.player_id));
            ctrl.init_players(players);
            Ok(ctrl)
        }
    }

    /// Обновить таймеры под конкретного актёра (начало хода).
    async fn update_time_controller_for_actor(
        &mut self,
        table: &Table,
        current_actor: Option<SeatIndex>,
    ) -> OnchainResult<()> {
        let mut ctrl = self.ensure_time_controller(table).await?;

        ctrl.clear_current_turn();

        if let Some(seat_idx) = current_actor {
            if let Some(p) = table
                .seats
                .get(seat_idx as usize)
                .and_then(|s| s.as_ref())
            {
                ctrl.start_player_turn(p.player_id);
            }
        }

        self.state
            .time_controllers
            .insert(&table.id, ctrl)
            .map_err(|e| OnchainError::Storage(e.to_string()))
    }

    /// Сбросить текущий ход в таймере (когда раздача завершилась).
    async fn clear_current_turn_for_table(
        &mut self,
        table_id: TableId,
    ) -> OnchainResult<()> {
        if let Some(mut ctrl) = self
            .state
            .time_controllers
            .get(&table_id)
            .await
            .map_err(|e| OnchainError::Storage(e.to_string()))?
        {
            ctrl.clear_current_turn();
            self.state
                .time_controllers
                .insert(&table_id, ctrl)
                .map_err(|e| OnchainError::Storage(e.to_string()))?;
        }
        Ok(())
    }

    /// Найти seat игрока на конкретном столе.
    async fn find_seat_by_player(
        &self,
        table_id: TableId,
        player_id: PlayerId,
    ) -> OnchainResult<SeatIndex> {
        let table = self.load_table(table_id).await?;
        for (idx, seat_opt) in table.seats.iter().enumerate() {
            if let Some(p) = seat_opt {
                if p.player_id == player_id {
                    return Ok(idx as SeatIndex);
                }
            }
        }
        Err(OnchainError::NoPlayerAtSeat {
            table: table_id,
            seat: 255,
        })
    }
}

/// Маппинг внешнего ante-типа из API в доменную модель.
fn map_ante_type(api: AnteTypeApi) -> AnteType {
    match api {
        AnteTypeApi::None => AnteType::None,
        AnteTypeApi::Classic => AnteType::Classic,
        AnteTypeApi::BigBlind => AnteType::BigBlind,
    }
}

/// Вытянуть стейки для уровня турнира из BlindStructure.
fn stakes_for_tournament_level(
    config: &TournamentConfig,
    level: u32,
) -> TableStakes {
    // Если уровня нет – падаем обратно на первый.
    let blind: BlindLevel = config
        .blind_structure
        .level_by_number(level)
        .or_else(|| config.blind_structure.level_by_number(1))
        .expect("blind_structure must have at least one level")
        .clone();

    TableStakes::new(
        blind.small_blind,
        blind.big_blind,
        blind.ante_type,
        blind.ante,
    )
}
