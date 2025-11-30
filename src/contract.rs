#![cfg_attr(target_arch = "wasm32", no_main)]

use linera_sdk::{
    abi::WithContractAbi,
    views::{RootView, View},
    Contract,
    ContractRuntime,
};
use linera_sdk::linera_base_types::AccountOwner;

use poker_engine::api::dto::CommandResponse;

use crate::{ApplicationParameters, Message, Operation, PokerAbi};
use crate::orchestrator::PokerOrchestrator;
use crate::state::PokerState;

/// Contract entry point для покерного приложения.
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
    type Parameters = ApplicationParameters;
    type InstantiationArgument = ();
    type EventValue = ();

    async fn load(runtime: ContractRuntime<Self>) -> Self {
        let state = PokerState::load(runtime.root_view_storage_context())
            .await
            .expect("Failed to load PokerState");

        PokerContract { state, runtime }
    }

    async fn instantiate(&mut self, _argument: Self::InstantiationArgument) {
        let params: ApplicationParameters = self.runtime.application_parameters();

        // Владелец обязателен.
        self.state.owner.set(Some(params.owner));

        // Базовый seed: из параметров или 1.
        let seed = params.base_seed.unwrap_or(1);
        self.state.base_seed.set(seed);

        // Стартовый hand_id.
        self.state.next_hand_id.set(0);
    }

    async fn execute_operation(&mut self, operation: Operation) -> CommandResponse {
        let signer: Option<AccountOwner> = self.runtime.authenticated_signer();
        let mut orchestrator = PokerOrchestrator::new(&mut self.state, signer);

        match operation {
            Operation::Command(cmd) => orchestrator.execute_command(cmd).await,
        }
    }

    async fn execute_message(&mut self, _message: Self::Message) {
        // Пока не используем cross-chain сообщения.
    }

    async fn store(mut self) {
        self.state
            .save()
            .await
            .expect("Failed to save PokerState");
    }
}
