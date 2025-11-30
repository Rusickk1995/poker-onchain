use linera_sdk::linera_base_types::AccountOwner;
use thiserror::Error;

use poker_engine::domain::hand::Street;

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
    CreateTournamentCommand,
    RegisterPlayerInTournamentCommand,
    UnregisterPlayerFromTournamentCommand,
    StartTournamentCommand,
    AdvanceLevelCommand,
    CloseTournamentCommand,
};
use poker_engine::api::dto::{
    CommandResponse,
    PlayerAtTableDto,
    TableViewDto,
    TournamentViewDto,
    map_hand_status_to_response,
};
use poker_engine::domain::blinds::AnteType;
use poker_engine::domain::chips::Chips;
use poker_engine::domain::player::PlayerAtTable;
use poker_engine::domain::table::{Table, TableConfig, TableStakes, TableType};
use poker_engine::domain::tournament::{
    Tournament, TournamentConfig, TournamentError, TournamentStatus,
};
use poker_engine::domain::{PlayerId, SeatIndex, TableId, TournamentId};
use poker_engine::engine::{self, HandStatus};
use poker_engine::infra::rng_seed::RngSeed;
use poker_engine::tournament::TournamentRuntime;

use crate::{HandEngineSnapshot, PokerState};

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
            // Любой базовый стрит, который есть в твоём enum Street.
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
        let hand_id = current_id + 1;
        self.state.next_hand_id.set(hand_id);

        let base_seed = *self.state.base_seed.get();
        let seed = RngSeed::from_u64(base_seed ^ hand_id ^ table.id as u64);
        let mut rng = seed.to_rng();

        let mut engine =
            engine::start_hand(&mut table, &mut rng, hand_id).map_err(|e| {
                OnchainError::EngineError(format!("start_hand failed: {e:?}"))
            })?;

        let total = *self.state.total_hands_played.get();
        self.state.total_hands_played.set(total.saturating_add(1));

        let snapshot = HandEngineSnapshot::from_engine(&engine);
        self.state
            .active_hands
            .insert(&table.id, Some(snapshot.clone()))
            .map_err(|e| OnchainError::Storage(e.to_string()))?;

        self.save_table(table.clone())?;

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

        let slot = self
            .state
            .active_hands
            .get(&cmd.table_id)
            .await
            .map_err(|e| OnchainError::Storage(e.to_string()))?
            .ok_or(OnchainError::NoActiveHand(cmd.table_id))?;

        let snapshot = slot.ok_or(OnchainError::NoActiveHand(cmd.table_id))?;

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
        let table_view = self
            .build_table_view(&table, Some(&snapshot_after))
            .await?;

        let response = match status {
            HandStatus::Ongoing => {
                self.state
                    .active_hands
                    .insert(&table.id, Some(snapshot_after))
                    .map_err(|e| OnchainError::Storage(e.to_string()))?;

                self.save_table(table.clone())?;
                CommandResponse::TableState(table_view)
            }
            finished_status => {
                self.state
                    .active_hands
                    .insert(&table.id, None)
                    .map_err(|e| OnchainError::Storage(e.to_string()))?;

                self.save_table(table.clone())?;

                // Если это турнирный стол — обновляем турнирное состояние.
                if let Some(tournament_id) =
                    self.table_tournament_id(table.id).await?
                {
                    self.handle_tournament_after_hand(
                        tournament_id,
                        &table,
                    )
                    .await?;
                }

                map_hand_status_to_response(finished_status, table_view)
            }
        };

        Ok(response)
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

    /// Создать новый турнир.
    ///
    /// ВАЖНО:
    /// - id турнира генерируем on-chain (автоинкремент по существующим id);
    /// - фронт получает id из ответа CommandResponse::TournamentState.
    async fn handle_create_tournament(
        &mut self,
        cmd: CreateTournamentCommand,
    ) -> OnchainResult<CommandResponse> {
        self.ensure_admin().await?;

        // Генерируем новый TournamentId: max(existing) + 1.
        let existing_ids: Vec<TournamentId> = self
            .state
            .tournaments
            .indices()
            .await
            .map_err(|e| OnchainError::Storage(e.to_string()))?;
        let next_id: TournamentId = existing_ids
            .into_iter()
            .max()
            .unwrap_or(0)
            .saturating_add(1);

        if self
            .state
            .tournaments
            .get(&next_id)
            .await
            .map_err(|e| OnchainError::Storage(e.to_string()))?
            .is_some()
        {
            return Err(OnchainError::TournamentAlreadyExists(next_id));
        }

        // Владелец турнира как player_id — пока просто 0 (системный),
        // логика призов/пули у тебя внутри движка.
        let owner_player: PlayerId = 0;

        let tournament = Tournament::new(
            next_id,
            owner_player,
            cmd.config.clone(),
        )?;

        // На старте статус Registering уже выставлен в Tournament::new.
        self.state
            .tournaments
            .insert(&next_id, tournament.clone())
            .map_err(|e| OnchainError::Storage(e.to_string()))?;

        // Пустой список столов для турнира.
        self.state
            .tournament_tables
            .insert(&next_id, Vec::new())
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

        // Обновляем имя игрока (если пришло) – пригодится во view столов.
        if !cmd.display_name.is_empty() {
            self.state
                .player_names
                .insert(&player_id, cmd.display_name.clone())
                .map_err(|e| OnchainError::Storage(e.to_string()))?;
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

    async fn handle_unregister_player_from_tournament(
        &mut self,
        cmd: UnregisterPlayerFromTournamentCommand,
    ) -> OnchainResult<CommandResponse> {
        let player_id = self.ensure_player_for_signer(cmd.player_id).await?;

        let mut tournament = self
            .load_tournament(cmd.tournament_id)
            .await?;

        // Разрешаем unregister только пока статус Registering.
        if tournament.status != TournamentStatus::Registering {
            return Err(OnchainError::Tournament(
                TournamentError::InvalidStatus {
                    expected: TournamentStatus::Registering,
                    found: tournament.status,
                },
            ));
        }

        if tournament
            .registrations
            .remove(&player_id)
            .is_none()
        {
            return Err(OnchainError::Tournament(
                TournamentError::NotRegistered {
                    player_id,
                    tournament_id: cmd.tournament_id,
                },
            ));
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

        // Доверяем доменной логике: стартуем турнир.
        // Для on-chain детерминизма используем now_ts = 0.
        tournament.start(0)?;

        // Генерируем столы через TournamentRuntime.
        // Выбираем базовый next_table_id = max(existing_tables) + 1.
        let existing_table_ids: Vec<TableId> = self
            .state
            .tables
            .indices()
            .await
            .map_err(|e| OnchainError::Storage(e.to_string()))?;
        let next_table_id: TableId = existing_table_ids
            .into_iter()
            .max()
            .unwrap_or(0)
            .saturating_add(1);

        let instances =
            TournamentRuntime::build_tables_for_tournament(&tournament, next_table_id);

        let mut new_table_ids = Vec::new();

        for instance in instances.into_iter() {
            let table_id = instance.table.id;
            new_table_ids.push(table_id);

            // Обновляем регистрацию игроков: table_id / seat_index / total_chips.
            for seat in &instance.seats {
                if let Some(reg) = tournament
                    .registrations
                    .get_mut(&seat.player_id)
                {
                    reg.table_id = Some(table_id);
                    reg.seat_index = Some(seat.seat_index as SeatIndex);
                    reg.total_chips = seat.stack;
                }
            }

            // Сохраняем сам стол и метаданные в state.
            self.state
                .tables
                .insert(&table_id, instance.table.clone())
                .map_err(|e| OnchainError::Storage(e.to_string()))?;

            self.state
                .active_hands
                .insert(&table_id, None)
                .map_err(|e| OnchainError::Storage(e.to_string()))?;

            self.state
                .table_tournament
                .insert(&table_id, cmd.tournament_id)
                .map_err(|e| OnchainError::Storage(e.to_string()))?;
        }

        // Сохраняем обновлённый турнир и список его столов.
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

        if tournament.status != TournamentStatus::Running {
            return Err(OnchainError::TournamentNotRunning(cmd.tournament_id));
        }

        // Простая ручная логика: повышаем current_level на 1,
        // если он не выходит за пределы blind_structure.
        let next_level = tournament.current_level.saturating_add(1);
        if tournament
            .config
            .blind_structure
            .level_by_number(next_level)
            .is_none()
        {
            // Больше уровней нет — просто возвращаем текущее состояние.
            let table_ids = self
                .state
                .tournament_tables
                .get(&cmd.tournament_id)
                .await
                .map_err(|e| OnchainError::Storage(e.to_string()))?
                .unwrap_or_default();

            let view =
                self.build_tournament_view(&tournament, table_ids).await?;
            return Ok(CommandResponse::TournamentState(view));
        }

        tournament.current_level = next_level;

        // Пересчитаем stakes для этого уровня.
        let new_blinds = tournament.current_blind_level().clone();

        let table_ids = self
            .state
            .tournament_tables
            .get(&cmd.tournament_id)
            .await
            .map_err(|e| OnchainError::Storage(e.to_string()))?
            .unwrap_or_default();

        for table_id in table_ids.iter().copied() {
            if let Some(mut table) = self
                .state
                .tables
                .get(&table_id)
                .await
                .map_err(|e| OnchainError::Storage(e.to_string()))?
            {
                table.config.stakes = TableStakes::new(
                    new_blinds.small_blind,
                    new_blinds.big_blind,
                    new_blinds.ante_type,
                    new_blinds.ante,
                );
                self.save_table(table)?;
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

    /// Хук, который вызывается после завершения раздачи на турнирном столе.
    ///
    /// Здесь:
    /// - синхронизируем стеки игроков из всех столов турнира в Tournament.registrations;
    /// - помечаем bust игроков (total_chips == 0);
    /// - если остался один игрок, турнир завершится через Tournament::check_and_finish_if_needed().
    async fn handle_tournament_after_hand(
        &mut self,
        tournament_id: TournamentId,
        last_table: &Table,
    ) -> OnchainResult<()> {
        let mut tournament = self
            .load_tournament(tournament_id)
            .await?;

        if tournament.status != TournamentStatus::Running {
            return Ok(());
        }

        // Получаем список всех турнирных столов.
        let table_ids = self
            .state
            .tournament_tables
            .get(&tournament_id)
            .await
            .map_err(|e| OnchainError::Storage(e.to_string()))?
            .unwrap_or_default();

        // Сбрасываем накопленные стеки и посадку.
        for reg in tournament.registrations.values_mut() {
            reg.total_chips = Chips(0);
            reg.table_id = None;
            reg.seat_index = None;
        }

        // Пробегаем по всем столам турнира и пересчитываем total_chips.
        for table_id in table_ids.iter().copied() {
            let table = if table_id == last_table.id {
                last_table.clone()
            } else {
                self.load_table(table_id).await?
            };

            for (seat_idx, seat_opt) in table.seats.iter().enumerate() {
                if let Some(p) = seat_opt {
                    if let Some(reg) =
                        tournament.registrations.get_mut(&p.player_id)
                    {
                        reg.total_chips += p.stack;
                        reg.table_id = Some(table.id);
                        reg.seat_index = Some(seat_idx as SeatIndex);
                    }
                }
            }
        }

        // Вычисляем bust-игроков: total_chips == 0.
        let busted_ids: Vec<PlayerId> = tournament
            .registrations
            .values()
            .filter(|r| !r.is_busted && r.total_chips.0 == 0)
            .map(|r| r.player_id)
            .collect();

        for pid in busted_ids {
            let _place = tournament.mark_player_busted(pid)?;
        }

        // Tournament сам внутри check_and_finish_if_needed завершит турнир,
        // если останется 0 или 1 активный игрок (см. доменную логику).
        tournament.check_and_finish_if_needed();

        self.state
            .tournaments
            .insert(&tournament_id, tournament)
            .map_err(|e| OnchainError::Storage(e.to_string()))?;

        Ok(())
    }

    // =====================================================================
    //                               HELPERS
    // =====================================================================

    async fn load_table(&mut self, id: TableId) -> OnchainResult<Table> {
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
        let opt = self.state.active_hands
            .get(&table_id)
            .await
            .map_err(|e| OnchainError::Storage(e.to_string()))?;

        Ok(opt.flatten())
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
}

/// Маппинг внешнего ante-типа из API в доменную модель.
fn map_ante_type(api: AnteTypeApi) -> AnteType {
    match api {
        AnteTypeApi::None => AnteType::None,
        AnteTypeApi::Classic => AnteType::Classic,
        AnteTypeApi::BigBlind => AnteType::BigBlind,
    }
}
