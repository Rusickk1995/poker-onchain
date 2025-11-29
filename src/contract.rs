#![cfg_attr(target_arch = "wasm32", no_main)]

use linera_sdk::{
    abi::WithContractAbi,
    views::{RootView, View},
    Contract,
    ContractRuntime,
};

use poker_engine::api::dto::CommandResponse;

use crate::{Message, Operation, PokerAbi};
use crate::orchestrator::PokerOrchestrator;
use crate::state::PokerState;

/// Contract entry point для покерного приложения.
///
/// Здесь минимум логики:
/// - загрузка / сохранение `PokerState`;
/// - делегирование бизнес-логики в `PokerOrchestrator`.
pub struct PokerContract {
    pub state: PokerState,
    pub runtime: ContractRuntime<Self>,
}

linera_sdk::contract!(PokerContract);

impl WithContractAbi for PokerContract {
    type Abi = PokerAbi;
}

impl Contract for PokerContract {
    type Message = Message;
    type Parameters = ();
    type InstantiationArgument = ();
    type EventValue = ();

    async fn load(runtime: ContractRuntime<Self>) -> Self {
        let state = PokerState::load(runtime.root_view_storage_context())
            .await
            .expect("Failed to load PokerState");

        PokerContract { state, runtime }
    }

    async fn instantiate(&mut self, _argument: Self::InstantiationArgument) {
        // Здесь можно валидировать application parameters,
        // owner, currency-id, лимиты и пр.
        let _params = self.runtime.application_parameters();
    }

    async fn execute_operation(&mut self, operation: Operation) -> CommandResponse {
        match operation {
            Operation::Command(cmd) => {
                let mut orchestrator = PokerOrchestrator::new(&mut self.state);
                orchestrator.execute_command(cmd).await
            }
        }
    }

    async fn execute_message(&mut self, _message: Self::Message) {
        // Зарезервировано под cross-chain сообщения (например,
        // турниры между несколькими цепями, глобальные лидерборды и т.п.).
    }

    /// Здесь self передаётся ПО ЗНАЧЕНИЮ, поэтому делаем его `mut self`,
    /// чтобы иметь право мутировать `self.state` и вызвать `save()`.
    async fn store(mut self) {
        self.state
            .save()
            .await
            .expect("Failed to save PokerState");
    }
}
