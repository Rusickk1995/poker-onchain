use std::collections::HashMap;

use linera_sdk::linera_base_types::AccountOwner;
use linera_sdk::views::{
    MapView, RegisterView, RootView, ViewStorageContext,
};
use serde::{Deserialize, Serialize};

use poker_engine::domain::chips::Chips;
use poker_engine::domain::deck::Deck;
use poker_engine::domain::table::Table;
use poker_engine::domain::tournament::Tournament;
use poker_engine::domain::{
    HandId, PlayerId, SeatIndex, TableId, TournamentId,
};
use poker_engine::engine::betting::BettingState;
use poker_engine::engine::game_loop;
use poker_engine::engine::hand_history::HandHistory;
use poker_engine::engine::pot::Pot;
use poker_engine::engine::side_pots::SidePot;


/// Полный снапшот HandEngine для хранения в Chain View.
///
/// Важное правило:
/// - здесь только чистые данные (Serialize + Deserialize),
/// - никакой логики / RNG / IO.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HandEngineSnapshot {
    pub table_id: TableId,
    pub hand_id: HandId,
    pub deck: Deck,
    pub betting: BettingState,
    pub pot: Pot,
    pub side_pots: Vec<SidePot>,
    pub contributions: HashMap<SeatIndex, Chips>,
    pub current_actor: Option<SeatIndex>,
    pub history: HandHistory,
}

impl HandEngineSnapshot {
    /// Оборачивает живой HandEngine → snapshot (для записи on-chain).
    pub fn from_engine(engine: &game_loop::HandEngine) -> Self {
        Self {
            table_id: engine.table_id,
            hand_id: engine.hand_id,
            deck: engine.deck.clone(),
            betting: engine.betting.clone(),
            pot: engine.pot.clone(),
            side_pots: engine.side_pots.clone(),
            contributions: engine.contributions.clone(),
            current_actor: engine.current_actor,
            history: engine.history.clone(),
        }
    }

    /// Разворачивает snapshot → HandEngine (в оперативной памяти).
    pub fn into_engine(self) -> game_loop::HandEngine {
        game_loop::HandEngine {
            table_id: self.table_id,
            hand_id: self.hand_id,
            deck: self.deck,
            betting: self.betting,
            pot: self.pot,
            side_pots: self.side_pots,
            contributions: self.contributions,
            current_actor: self.current_actor,
            history: self.history,
        }
    }
}

/// Глобальное состояние on-chain приложения Poker.
/// Это единственное, что реально сохраняется в состоянии цепи Linera.
#[derive(RootView)]
#[view(context = ViewStorageContext)]
pub struct PokerState {
    /// Столы: cash + tournament.
    #[view(map)]
    pub tables: MapView<TableId, Table>,

    /// Активные раздачи по столам.
    /// Если None — сейчас на столе нет активной раздачи.
    #[view(map)]
    pub active_hands: MapView<TableId, Option<HandEngineSnapshot>>,

    /// Турниры (доменные структуры из движка).
    #[view(map)]
    pub tournaments: MapView<TournamentId, Tournament>,

    /// Маппинг: турнир → список его столов.
    #[view(map)]
    pub tournament_tables: MapView<TournamentId, Vec<TableId>>,

    /// Маппинг: стол → турнир (если стол турнирный).
    #[view(map)]
    pub table_tournament: MapView<TableId, TournamentId>,

    /// Глобальный счётчик раздач (для статистики / мониторинга).
    #[view(register)]
    pub total_hands_played: RegisterView<u64>,

    /// Следующий hand_id для on-chain RNG/движка.
    #[view(register)]
    pub next_hand_id: RegisterView<u64>,

    /// Базовый seed для RNG (можно задать при инстансе).
    #[view(register)]
    pub base_seed: RegisterView<u64>,

    /// Владелец приложения (account owner), задаётся в ApplicationParameters.
    #[view(register)]
    pub owner: RegisterView<Option<AccountOwner>>,

    /// Отображаемые имена игроков для UI: PlayerId -> String.
    #[view(map)]
    pub player_names: MapView<PlayerId, String>,

    /// Привязка player_id → аккаунт в Linera.
    #[view(map)]
    pub player_accounts: MapView<PlayerId, AccountOwner>,

    /// Обратная привязка: аккаунт → player_id.
    #[view(map)]
    pub account_players: MapView<AccountOwner, PlayerId>,
}
