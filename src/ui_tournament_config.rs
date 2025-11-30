// poker-onchain/src/ui_tournament_config.rs

use serde::{Deserialize, Serialize};

use poker_engine::domain::chips::Chips;
use poker_engine::domain::tournament::{
    AnteType,
    BlindPace,
    BlindLevelConfig,
    TournamentConfig,
};

/// Один уровень блайндов из фронта
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiBlindLevel {
    pub level: u32,
    pub small_blind: u64,
    pub big_blind: u64,
    pub ante: u64,
}

/// Полный конфиг турнира, 1-в-1 (по смыслу) с твоим TS `TournamentConfig`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiTournamentConfig {
    // Basic info
    pub name: String,
    pub description: String,
    pub prize_description: String,
    pub start_time: Option<String>,
    pub reg_close_time: Option<String>,

    // Structure & timing
    pub table_size: u8,
    pub action_time: u32,
    pub blind_level_duration: u32,
    pub blind_pace: BlindPace,

    // Stacks & players
    pub starting_stack: u64,
    pub max_players: u32,
    pub late_reg_minutes: u32,

    // Antes / blinds
    pub ante_type: AnteType,
    pub is_progressive_ante: bool,

    // Payouts
    pub payout_type: String,
    pub min_payout_places: u32,
    pub guaranteed_prize_pool: u64,

    // Bounty / final table
    pub is_bounty: bool,
    pub bounty_amount: u64,
    pub has_final_table_bonus: bool,
    pub final_table_bonus: u64,

    // Timebank / breaks
    pub time_bank_seconds: u32,
    pub break_every_minutes: u32,
    pub break_duration_minutes: u32,

    // Registration / re-entry
    pub instant_registration: bool,
    pub re_entry_allowed: bool,
    pub rebuys_allowed: bool,

    // Список уровней блайндов, который фронт уже посчитал
    pub blind_levels: Vec<UiBlindLevel>,
}

impl From<UiTournamentConfig> for TournamentConfig {
    fn from(ui: UiTournamentConfig) -> Self {
        // Перегоняем UiBlindLevel -> BlindLevelConfig движка
        let levels: Vec<BlindLevelConfig> = ui
            .blind_levels
            .into_iter()
            .map(|lvl| BlindLevelConfig {
                level: lvl.level,
                small_blind: Chips::from(lvl.small_blind),
                big_blind: Chips::from(lvl.big_blind),
                ante: Chips::from(lvl.ante),
            })
            .collect();

        // Базовые поля – через helper из ШАГА 1
        TournamentConfig::from_frontend_basic(
            ui.name,
            ui.table_size,
            Chips::from(ui.starting_stack),
            ui.max_players,
            ui.action_time,
            ui.blind_level_duration,
            ui.blind_pace,
            levels,
            ui.ante_type,
            ui.time_bank_seconds,
            ui.break_every_minutes,
            ui.break_duration_minutes,
            ui.re_entry_allowed,
            ui.rebuys_allowed,
        )

        // Остальные поля (`description`, `prize_description`, bounty, payouts и т.п.)
        // если у тебя уже есть в TournamentConfig — добавь их в from_frontend_basic
        // и прокинь туда. Если в движке их пока нет — оставь как "pure UI" инфу:
        // можешь временно проигнорировать или сохранить отдельно в метаданных.
    }
}
