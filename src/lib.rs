//! Poker on-chain application for Linera 0.15.6.
//!
//! Шаг 0: минимальный, но корректный on-chain слой поверх твоего off-chain
//! `poker-engine`.
//!
//! Здесь мы определяем:
//! - ABI (Operation / Message / Query / Response);
//! - связь с движком через DTO (`Command`, `CommandResponse`);
//! - упрощённый ServiceAbi (plain JSON вместо GraphQL, GraphQL добавим позже).

pub mod state;
pub mod orchestrator;
pub mod contract;
pub mod service;

use linera_sdk::abi::{ContractAbi, ServiceAbi};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use poker_engine::api::commands::Command;
use poker_engine::api::dto::CommandResponse;

/// ABI-маркер приложения Poker
#[derive(Clone, Debug)]
pub struct PokerAbi;

/// Единственный тип операции, который вызывается через контракт.
/// Это прямое отображение твоего off-chain `Command`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Operation {
    Command(Command),
}

/// Сообщения между цепями (на шаге 0 не используем)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Message {}

/// ABI контракта:
/// - Operation = наш enum `Operation`;
/// - Response = твой `CommandResponse` из off-chain движка.
impl ContractAbi for PokerAbi {
    type Operation = Operation;
    type Response = CommandResponse;
}

/// ABI сервиса:
/// Шаг 0: никаких GraphQL. Используем plain JSON запрос/ответ.
/// Потом можно будет безболезненно мигрировать на GraphQL.
impl ServiceAbi for PokerAbi {
    type Query = Value;
    type QueryResponse = Value;
}

/// Удобный реэкспорт состояния наружу.
pub use state::{HandEngineSnapshot, PokerState};
