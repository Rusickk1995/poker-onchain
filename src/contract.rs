// src/contract.rs
#![cfg_attr(target_arch = "wasm32", no_main)]

use linera_sdk::{
    abi::WithContractAbi,
    contract::ContractRuntime,
    views::{RootView as _, View as _},
    Contract,
};

use poker_engine::api::{
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
};
use poker_engine::domain::chips::Chips;
use poker_engine::domain::player::PlayerAtTable;
use poker_engine::domain::table::{Table, TableConfig, TableStakes, TableType};
use poker_engine::engine::{self, HandStatus};
use poker_engine::infra::mapping::ante_type_from_api;
use poker_engine::infra::rng::SystemRng;
use poker_engine::state::HandEngineSnapshot;

use poker_onchain::state::AppState;
use poker_onchain::{PokerAbi, PokerOperation, Storage};

/// Контракт покерного приложения.
/// Отвечает за мутирующие операции (Operations) и хранение состояния.
pub struct PokerContract {
    pub state: Storage,
    pub runtime: ContractRuntime<Self>,
}

/// Экспорт wasm-энтрипоинтов контракта.
linera_sdk::contract!(PokerContract);

impl WithContractAbi for PokerContract {
    type Abi = PokerAbi;
}

impl Contract for PokerContract {
    type Message = ();
    type Parameters = ();
    type InstantiationArgument = ();
    type EventValue = ();

    async fn load(runtime: ContractRuntime<Self>) -> Self {
        let state = Storage::load(runtime.root_view_storage_context())
            .await
            .expect("Failed to load PokerState");

        PokerContract { state, runtime }
    }

    async fn instantiate(&mut self, _argument: Self::InstantiationArgument) {
        use poker_engine::domain::TableId;

        let mut app: AppState = self.state.app.get().clone();

        app.total_hands_played = 0;

        let table_id: TableId = 1;

        if !app.tables.contains_key(&table_id) {
            let stakes = TableStakes::new(
                Chips(50),  // small blind
                Chips(100), // big blind
                ante_type_from_api(AnteTypeApi::None),
                Chips(0),   // ante
            );

            let config = TableConfig {
                max_seats: 9,
                table_type: TableType::Cash,
                stakes,
                allow_straddle: false,
                allow_run_it_twice: false,
            };

            let table = Table::new(table_id, "Default Table".to_string(), config);
            app.tables.insert(table_id, table);
        }

        self.state.app.set(app);
    }

    async fn execute_operation(&mut self, operation: Self::Operation) -> Self::Response {
        match operation {
            PokerOperation::Command(cmd) => {
                handle_command(&mut self.state, cmd).await;
            }
        }
    }

    async fn execute_message(&mut self, _message: Self::Message) {
        // Cross-app сообщения пока не используем
    }

    async fn store(self) {
        let mut state = self.state;
        state.save().await.expect("Failed to save PokerState");
    }
}

/// Диспетчер команд верхнего уровня.
async fn handle_command(state: &mut Storage, command: Command) {
    match command {
        Command::CreateTable(cmd) => handle_create_table(state, cmd).await,
        Command::TableCommand(cmd) => handle_table_command(state, cmd).await,
        Command::TournamentCommand(cmd) => handle_tournament_command(state, cmd).await,
    }
}

/// Создание кеш-стола.
async fn handle_create_table(state: &mut Storage, cmd: CreateTableCommand) {
    let mut app = state.app.get().clone();

    // small_blind, big_blind и ante уже Chips в API.
    let stakes = TableStakes::new(
        cmd.small_blind,
        cmd.big_blind,
        ante_type_from_api(cmd.ante_type),
        cmd.ante,
    );

    let config = TableConfig {
        max_seats: cmd.max_seats,
        // В CreateTableCommand нет table_type — считаем, что это кэш-стол.
        table_type: TableType::Cash,
        stakes,
        // В CreateTableCommand нет этих полей — ставим дефолт.
        allow_straddle: false,
        allow_run_it_twice: false,
    };

    let table = Table::new(cmd.table_id, cmd.name, config);

    app.tables.insert(cmd.table_id, table);

    state.app.set(app);
}

/// Маршрутизация команд по столу.
async fn handle_table_command(state: &mut Storage, cmd: TableCommand) {
    match cmd {
        TableCommand::SeatPlayer(c) => handle_seat_player(state, c).await,
        TableCommand::UnseatPlayer(c) => handle_unseat_player(state, c).await,
        TableCommand::AdjustStack(c) => handle_adjust_stack(state, c).await,
        TableCommand::StartHand(c) => handle_start_hand(state, c).await,
        TableCommand::PlayerAction(c) => handle_player_action(state, c).await,
    }
}

/// Турнирные команды — заглушка на будущее.
async fn handle_tournament_command(_state: &mut Storage, _cmd: TournamentCommand) {
    // TODO: здесь в будущем реализуешь полноценную турнирную логику
}

/// Посадить игрока за кеш-стол.
async fn handle_seat_player(state: &mut Storage, cmd: SeatPlayerCommand) {
    let mut app = state.app.get().clone();
    let table_id = cmd.table_id;

    let Some(table) = app.tables.get_mut(&table_id) else {
        state.app.set(app);
        return;
    };

    if (cmd.seat_index as usize) >= table.seats.len() {
        state.app.set(app);
        return;
    }

    if table.seats[cmd.seat_index as usize].is_some() {
        state.app.set(app);
        return;
    }

    // В домене PlayerAtTable хранит только id и стек.
    // display_name кладём отдельно в app.player_names.
    let player = PlayerAtTable::new(cmd.player_id, cmd.initial_stack);

    table.seats[cmd.seat_index as usize] = Some(player);
    app.player_names.insert(cmd.player_id, cmd.display_name);

    state.app.set(app);
}

/// Убрать игрока со стола.
async fn handle_unseat_player(state: &mut Storage, cmd: UnseatPlayerCommand) {
    let mut app = state.app.get().clone();
    let table_id = cmd.table_id;

    let Some(table) = app.tables.get_mut(&table_id) else {
        state.app.set(app);
        return;
    };

    if (cmd.seat_index as usize) >= table.seats.len() {
        state.app.set(app);
        return;
    }

    table.seats[cmd.seat_index as usize] = None;

    state.app.set(app);
}

/// Изменить стек игрока (delta — i64).
async fn handle_adjust_stack(state: &mut Storage, cmd: AdjustStackCommand) {
    let mut app = state.app.get().clone();
    let table_id = cmd.table_id;

    let Some(table) = app.tables.get_mut(&table_id) else {
        state.app.set(app);
        return;
    };

    if (cmd.seat_index as usize) >= table.seats.len() {
        state.app.set(app);
        return;
    }

    if let Some(player) = &mut table.seats[cmd.seat_index as usize] {
        let old = player.stack.0;
        let new = if cmd.delta < 0 {
            let dec = (-cmd.delta) as u64;
            old.saturating_sub(dec)
        } else {
            old.saturating_add(cmd.delta as u64)
        };
        player.stack = Chips(new);
    }

    state.app.set(app);
}

/// Начать новую раздачу за столом.
async fn handle_start_hand(state: &mut Storage, cmd: StartHandCommand) {
    let mut app = state.app.get().clone();
    let table_id = cmd.table_id;

    let Some(table) = app.tables.get_mut(&table_id) else {
        state.app.set(app);
        return;
    };

    let mut rng = SystemRng::default();

    // Сигнатура движка: start_hand(table: &mut Table, rng: &mut R, hand_id: HandId)
    let result = engine::start_hand(table, &mut rng, cmd.hand_id);
    let engine = match result {
        Ok(e) => e,
        Err(_) => {
            state.app.set(app);
            return;
        }
    };

    app.total_hands_played = app.total_hands_played.wrapping_add(1);

    let snapshot = HandEngineSnapshot::from_engine(&engine);
    app.active_hands.insert(table_id, Some(snapshot));

    // history можно использовать позже для on-chain истории, пока игнорируем
    let _ = engine.history;

    state.app.set(app);
}

/// Действие игрока в активной раздаче.
async fn handle_player_action(state: &mut Storage, cmd: PlayerActionCommand) {
    let mut app = state.app.get().clone();
    let table_id = cmd.table_id;

    let Some(table) = app.tables.get_mut(&table_id) else {
        state.app.set(app);
        return;
    };

    let Some(hand_slot) = app.active_hands.get_mut(&table_id) else {
        state.app.set(app);
        return;
    };

    let Some(snapshot) = hand_slot.take() else {
        state.app.set(app);
        return;
    };

    let mut engine = snapshot.into_engine();

    // Сигнатура движка:
    // apply_action(table: &mut Table, engine: &mut HandEngine, action: PlayerAction)
    let status = match engine::apply_action(table, &mut engine, cmd.action) {
        Ok(s) => s,
        Err(_) => {
            *hand_slot = Some(HandEngineSnapshot::from_engine(&engine));
            state.app.set(app);
            return;
        }
    };

    match status {
        HandStatus::Ongoing => {
            let new_snapshot = HandEngineSnapshot::from_engine(&engine);
            app.active_hands.insert(table_id, Some(new_snapshot));
        }
        HandStatus::Finished(_summary, _history) => {
            app.active_hands.insert(table_id, None);
        }
    }

    state.app.set(app);
}
