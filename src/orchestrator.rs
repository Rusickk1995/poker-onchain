use std::fmt;

use linera_sdk::views::ViewError;

use poker_engine::api::commands::{
    AnteTypeApi,
    Command,
    TableCommand,
    TournamentCommand,
    CreateTableCommand,
    SeatPlayerCommand,
    UnseatPlayerCommand,
    AdjustStackCommand,
    StartHandCommand,
    PlayerActionCommand,
};
use poker_engine::api::dto::{
    CommandResponse,
    PlayerAtTableDto,
    TableViewDto,
    map_hand_status_to_response,
};
use poker_engine::domain::blinds::AnteType;
use poker_engine::domain::chips::Chips;
use poker_engine::domain::player::PlayerAtTable;
use poker_engine::domain::table::{Table, TableConfig, TableStakes, TableType};
use poker_engine::domain::{SeatIndex, TableId};
use poker_engine::engine::{self, HandStatus};
use poker_engine::engine::errors::EngineError;
use poker_engine::infra::rng_seed::RngSeed;

use crate::{HandEngineSnapshot, PokerState};

/// Унифицированная ошибка on-chain слоя.
/// Все `View`/engine/логические ошибки сворачиваются сюда,
/// а сверху маппятся в `CommandResponse::Error(...)`.
#[derive(Debug)]
pub enum OnchainError {
    /// Ошибка работы с View (хранилище Linera).
    View(ViewError),

    /// Ошибка движка (engine).
    Engine(EngineError),

    /// Команда некорректна с точки зрения бизнес-логики.
    InvalidCommand(String),

    /// Несоответствие состояния (отсутствующий стол, раздача и т.п.).
    State(String),

    /// Внутренняя ошибка, которую трудно типизировать.
    Internal(String),
}

impl fmt::Display for OnchainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OnchainError::View(e) => write!(f, "View error: {e}"),
            OnchainError::Engine(e) => write!(f, "Engine error: {e}"),
            OnchainError::InvalidCommand(msg) => write!(f, "Invalid command: {msg}"),
            OnchainError::State(msg) => write!(f, "State error: {msg}"),
            OnchainError::Internal(msg) => write!(f, "Internal error: {msg}"),
        }
    }
}

impl std::error::Error for OnchainError {}

impl From<ViewError> for OnchainError {
    fn from(e: ViewError) -> Self {
        OnchainError::View(e)
    }
}

impl From<EngineError> for OnchainError {
    fn from(e: EngineError) -> Self {
        OnchainError::Engine(e)
    }
}

type OnchainResult<T> = Result<T, OnchainError>;

/// Высокоуровневый оркестратор:
/// Command (API) → изменение PokerState → CommandResponse (DTO для фронта).
///
/// Внутри:
/// - работа с MapView / RegisterView;
/// - запуск/продолжение раздач через `poker_engine::engine`;
/// - НЕТ `panic!/expect` на пользовательских путях, только `OnchainError` → `CommandResponse::Error`.
pub struct PokerOrchestrator<'a> {
    pub state: &'a mut PokerState,
}

impl<'a> PokerOrchestrator<'a> {
    pub fn new(state: &'a mut PokerState) -> Self {
        Self { state }
    }

    /// Главная точка входа для контракта.
    ///
    /// Внутри работаем через Result, наружу всегда возвращаем CommandResponse:
    /// либо нормальный ответ, либо `CommandResponse::Error(...)`.
    pub async fn execute_command(&mut self, cmd: Command) -> CommandResponse {
        match self.execute_command_inner(cmd).await {
            Ok(resp) => resp,
            Err(err) => {
                // ВАЖНО: для компиляции должен существовать вариант
                // `CommandResponse::Error(String)` в движке (api/dto.rs).
                CommandResponse::Error(err.to_string())
            }
        }
    }

    /// Вся реальная логика команд — через `OnchainResult`.
    async fn execute_command_inner(&mut self, cmd: Command) -> OnchainResult<CommandResponse> {
        match cmd {
            Command::CreateTable(c) => self.handle_create_table(c).await,
            Command::TableCommand(tc) => self.handle_table_command(tc).await,
            Command::TournamentCommand(tc) => self.handle_tournament_command(tc).await,
        }
    }

    // ======================= TABLE LIFECYCLE (CASH) =======================

    async fn handle_create_table(
        &mut self,
        cmd: CreateTableCommand,
    ) -> OnchainResult<CommandResponse> {
        // Защита от двойного создания одного и того же стола.
        let existing = self
            .state
            .tables
            .get(&cmd.table_id)
            .await
            .map_err(OnchainError::from)?;

        if existing.is_some() {
            return Err(OnchainError::InvalidCommand(format!(
                "CreateTable: table {} already exists",
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

        // Сохраняем стол.
        self.state
            .tables
            .insert(&cmd.table_id, table.clone())
            .map_err(OnchainError::from)?;

        // Инициализируем слот для активной раздачи (пока None).
        self.state
            .active_hands
            .insert(&cmd.table_id, None)
            .map_err(OnchainError::from)?;

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
        let mut table = self.load_table(cmd.table_id).await?;
        let seat: SeatIndex = cmd.seat_index as SeatIndex;

        if !table.is_seat_empty(seat) {
            return Err(OnchainError::InvalidCommand(format!(
                "SeatPlayer: seat {} is not empty at table {}",
                seat, table.id
            )));
        }

        let pat = PlayerAtTable::new(cmd.player_id, cmd.initial_stack);

        if let Some(slot) = table.seats.get_mut(seat as usize) {
            *slot = Some(pat);
        } else {
            return Err(OnchainError::InvalidCommand(format!(
                "SeatPlayer: invalid seat index {} for table {}",
                seat, table.id
            )));
        }

        // Читаемое имя игрока для UI.
        self.state
            .player_names
            .insert(&cmd.player_id, cmd.display_name.clone())
            .map_err(OnchainError::from)?;

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
            return Err(OnchainError::InvalidCommand(format!(
                "UnseatPlayer: invalid seat index {} for table {}",
                seat, table.id
            )));
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
            return Err(OnchainError::InvalidCommand(format!(
                "AdjustStack: no player at seat {} on table {}",
                seat, table.id
            )));
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
            return Err(OnchainError::InvalidCommand(format!(
                "StartHand: hand already in progress at table {}",
                table.id
            )));
        }

        // Детерминированный RNG: derive(global_seed, table_id, hand_id).
        // TODO: вынести `base_seed` в PokerState, чтобы конфигурировать seed per app.
        let base_seed = RngSeed::from_u64(1);
        let derived_seed = base_seed.derive(table.id, cmd.hand_id, cmd.hand_id);
        let mut rng = derived_seed.to_rng();

        let new_hand_id = cmd.hand_id;

        let engine = engine::start_hand(&mut table, &mut rng, new_hand_id)
            .map_err(OnchainError::from)?;

        // Увеличиваем глобальный счётчик раздач.
        let current_total = *self.state.total_hands_played.get();
        self.state
            .total_hands_played
            .set(current_total.saturating_add(1));

        // Сохраняем снапшот.
        let snapshot = HandEngineSnapshot::from_engine(&engine);
        self.state
            .active_hands
            .insert(&table.id, Some(snapshot))
            .map_err(OnchainError::from)?;

        self.save_table(table.clone())?;

        let table_view = self
            .build_table_view(&table, Some(&HandEngineSnapshot::from_engine(&engine)))
            .await?;

        Ok(CommandResponse::TableState(table_view))
    }

    async fn handle_player_action(
        &mut self,
        cmd: PlayerActionCommand,
    ) -> OnchainResult<CommandResponse> {
        let mut table = self.load_table(cmd.table_id).await?;

        // Достаём snapshot для этого стола.
        let slot_opt = self
            .state
            .active_hands
            .get(&cmd.table_id)
            .await
            .map_err(OnchainError::from)?;

        let slot = slot_opt.ok_or_else(|| {
            OnchainError::State(format!(
                "PlayerAction: no active_hands entry found for table {}",
                cmd.table_id
            ))
        })?;

        let snapshot = slot.ok_or_else(|| {
            OnchainError::State(format!(
                "PlayerAction: no active hand snapshot for table {}",
                cmd.table_id
            ))
        })?;

        let mut engine = snapshot.into_engine();

        // Применяем action в движке.
        let mut status =
            engine::apply_action(&mut table, &mut engine, cmd.action.clone())
                .map_err(OnchainError::from)?;

        // Пытаемся авто-двинуть улицу / завершить раздачу.
        if let Ok(next_status) = engine::advance_if_needed(&mut table, &mut engine) {
            status = next_status;
        }

        // Обновлённый snapshot и view.
        let snapshot_after = HandEngineSnapshot::from_engine(&engine);
        let table_view = self
            .build_table_view(&table, Some(&snapshot_after))
            .await?;

        match status {
            HandStatus::Ongoing => {
                // Раздача продолжается — сохраняем обновлённый снапшот.
                self.state
                    .active_hands
                    .insert(&table.id, Some(snapshot_after))
                    .map_err(OnchainError::from)?;

                self.save_table(table.clone())?;

                Ok(CommandResponse::TableState(table_view))
            }
            finished_status => {
                // Раздача закончена — очищаем active_hands.
                self.state
                    .active_hands
                    .insert(&table.id, None)
                    .map_err(OnchainError::from)?;

                self.save_table(table.clone())?;

                // map_hand_status_to_response уже есть в DTO-слое движка.
                Ok(map_hand_status_to_response(finished_status, table_view))
            }
        }
    }

    async fn handle_tournament_command(
        &mut self,
        _cmd: TournamentCommand,
    ) -> OnchainResult<CommandResponse> {
        // На ЭТАПЕ 1 турниры on-chain НЕ реализуем.
        // Возвращаем аккуратную ошибку, без паники и без трогания полей,
        // которых нет в твоём TournamentCommand.
        Err(OnchainError::Internal(
            "TournamentCommand is not implemented on-chain yet".to_string(),
        ))
    }

    // ============================= HELPERS =============================

    /// Загрузить стол по `TableId` или вернуть осмысленную ошибку.
    async fn load_table(&mut self, id: TableId) -> OnchainResult<Table> {
        let maybe_table = self
            .state
            .tables
            .get(&id)
            .await
            .map_err(OnchainError::from)?;

        maybe_table.ok_or_else(|| OnchainError::State(format!("Table {} not found", id)))
    }

    /// Сохранить `Table` обратно в `tables` MapView.
    fn save_table(&mut self, table: Table) -> OnchainResult<()> {
        let id = table.id;
        self.state
            .tables
            .insert(&id, table)
            .map_err(OnchainError::from)
    }

    /// Загрузить текущий активный снапшот раздачи (если есть).
    async fn load_active_snapshot(
        &self,
        table_id: TableId,
    ) -> OnchainResult<Option<HandEngineSnapshot>> {
        let v = self
            .state
            .active_hands
            .get(&table_id)
            .await
            .map_err(OnchainError::from)?;

        Ok(v.flatten())
    }

    /// Собрать `TableViewDto` из доменного `Table` + опционального снапшота раздачи.
    async fn build_table_view(
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
                    .map_err(OnchainError::from)?
                    .unwrap_or_else(|| format!("Player #{}", player_id));

                players.push(PlayerAtTableDto {
                    player_id,
                    display_name,
                    seat_index,
                    stack: p.stack,
                    current_bet: p.current_bet,
                    status: p.status,
                    // Hole cards здесь намеренно не раскрываем.
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
}

/// Маппинг внешнего ante-типа из API в доменную модель.
fn map_ante_type(api: AnteTypeApi) -> AnteType {
    match api {
        AnteTypeApi::None => AnteType::None,
        AnteTypeApi::Classic => AnteType::Classic,
        AnteTypeApi::BigBlind => AnteType::BigBlind,
    }
}
