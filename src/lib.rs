// src/lib.rs

use async_graphql::{Request, Response};
use linera_sdk::abi::{ContractAbi, ServiceAbi};
use serde::{Deserialize, Serialize};

pub mod state;

// Реэкспортируем типы состояния, чтобы они были доступны как poker_onchain::Storage и poker_onchain::AppState
pub use state::PokerState as Storage;
pub use state::AppState;

use poker_engine::api::Command;

/// ABI для покерного приложения.
pub struct PokerAbi;

/// Операции (Operation) для контракта.
///
/// Мы делаем один enum PokerOperation, который просто оборачивает
/// верхнеуровневую команду из движка.
#[derive(Debug, Serialize, Deserialize)]
pub enum PokerOperation {
    /// Унифицированная команда верхнего уровня (cash / tournament и т.д.).
    Command(Command),
}

impl ContractAbi for PokerAbi {
    type Operation = PokerOperation;
    type Response = ();
}

impl ServiceAbi for PokerAbi {
    type Query = Request;
    type QueryResponse = Response;
}
