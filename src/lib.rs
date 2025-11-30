//! Poker on-chain application for Linera 0.15.6.

pub mod state;
pub mod orchestrator;
pub mod contract;
pub mod service;
pub mod utils;


use linera_sdk::abi::{ContractAbi, ServiceAbi};
use linera_sdk::linera_base_types::AccountOwner;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use poker_engine::api::commands::Command;
use poker_engine::api::dto::CommandResponse;

/// Параметры приложения, задаются при деплое.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApplicationParameters {
    pub owner: AccountOwner,
    /// Опциональный базовый seed для RNG. Если None — возьмём 1.
    pub base_seed: Option<u64>,
}

/// ABI-маркер приложения Poker.
#[derive(Clone, Debug)]
pub struct PokerAbi;

/// Единственный тип операции.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Operation {
    Command(Command),
}

/// Сообщения между цепями (пока не используем).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Message {}

impl ContractAbi for PokerAbi {
    type Operation = Operation;
    type Response = CommandResponse;
}

impl ServiceAbi for PokerAbi {
    type Query = Value;
    type QueryResponse = Value;
}

/// Удобный реэкспорт состояния.
pub use state::{HandEngineSnapshot, PokerState};
