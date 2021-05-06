#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(not(feature = "std"), feature(core_intrinsics))]
#![cfg_attr(not(feature = "std"), feature(alloc_error_handler))]
#![cfg_attr(feature = "log", feature(panic_info_message))]

#[cfg(not(feature = "std"))]
extern crate alloc;
#[cfg(not(feature = "std"))]
extern crate core;

pub mod meta_parsing;
pub mod parameters;
mod precompiles;
pub mod prelude;
pub mod storage;
pub mod transaction;
pub mod types;

#[cfg(feature = "contract")]
mod connector;
#[cfg(feature = "contract")]
mod deposit_event;
#[cfg(feature = "contract")]
mod engine;
#[cfg(feature = "contract")]
mod fungible_token;
#[cfg(feature = "contract")]
mod log_entry;
#[cfg(feature = "contract")]
mod prover;
#[cfg(feature = "contract")]
mod sdk;

#[cfg(feature = "contract")]
mod contract {
    use borsh::{BorshDeserialize, BorshSerialize};

    use crate::connector::EthConnectorContract;
    use crate::engine::{Engine, EngineResult, EngineState};
    #[cfg(feature = "evm_bully")]
    use crate::parameters::{BeginBlockArgs, BeginChainArgs};
    use crate::parameters::{FunctionCallArgs, GetStorageAtArgs, NewCallArgs, ViewCallArgs};
    use crate::prelude::{Address, H256, U256};
    use crate::sdk;
    use crate::types::{near_account_to_evm_address, u256_to_arr};

    #[global_allocator]
    static ALLOC: wee_alloc::WeeAlloc = wee_alloc::WeeAlloc::INIT;

    const CODE_KEY: &[u8; 5] = b"\0CODE";
    const CODE_STAGE_KEY: &[u8; 11] = b"\0CODE_STAGE";

    #[cfg(target_arch = "wasm32")]
    #[panic_handler]
    #[cfg_attr(not(feature = "log"), allow(unused_variables))]
    #[no_mangle]
    pub unsafe fn on_panic(info: &::core::panic::PanicInfo) -> ! {
        #[cfg(feature = "log")]
        {
            use alloc::{format, string::ToString};
            if let Some(msg) = info.message() {
                let msg = if let Some(log) = info.location() {
                    format!("{} [{}]", msg, log)
                } else {
                    msg.to_string()
                };
                sdk::log(&msg);
            } else if let Some(log) = info.location() {
                sdk::log(&log.to_string());
            }
        }

        ::core::intrinsics::abort();
    }

    #[cfg(target_arch = "wasm32")]
    #[alloc_error_handler]
    #[no_mangle]
    pub unsafe fn on_alloc_error(_: core::alloc::Layout) -> ! {
        ::core::intrinsics::abort();
    }

    ///
    /// ADMINISTRATIVE METHODS
    ///

    /// Sets the configuration for the Engine.
    /// Should be called on deployment.
    #[no_mangle]
    pub extern "C" fn new() {
        let state = Engine::get_state();
        if !state.owner_id.is_empty() {
            require_owner_only(&state);
        }
        let args = NewCallArgs::try_from_slice(&sdk::read_input()).sdk_expect("ERR_ARG_PARSE");
        Engine::set_state(args.into());
    }

    /// Get version of the contract.
    #[no_mangle]
    pub extern "C" fn get_version() {
        let version = match option_env!("NEAR_EVM_VERSION") {
            Some(v) => v.as_bytes(),
            None => include_bytes!("../VERSION"),
        };
        sdk::return_output(version)
    }

    /// Get owner account id for this contract.
    #[no_mangle]
    pub extern "C" fn get_owner() {
        let state = Engine::get_state();
        sdk::return_output(state.owner_id.as_bytes());
    }

    /// Get bridge prover id for this contract.
    #[no_mangle]
    pub extern "C" fn get_bridge_provider() {
        let state = Engine::get_state();
        sdk::return_output(state.bridge_prover_id.as_bytes());
    }

    /// Get chain id for this contract.
    #[no_mangle]
    pub extern "C" fn get_chain_id() {
        sdk::return_output(&Engine::get_state().chain_id)
    }

    #[no_mangle]
    pub extern "C" fn get_upgrade_index() {
        let state = Engine::get_state();
        let index = sdk::read_u64(CODE_STAGE_KEY).sdk_expect("ERR_NO_UPGRADE");
        sdk::return_output(&(index + state.upgrade_delay_blocks).to_le_bytes())
    }

    /// Stage new code for deployment.
    #[no_mangle]
    pub extern "C" fn stage_upgrade() {
        let state = Engine::get_state();
        require_owner_only(&state);
        sdk::read_input_and_store(CODE_KEY);
        sdk::write_storage(CODE_STAGE_KEY, &sdk::block_index().to_le_bytes());
    }

    /// Deploy staged upgrade.
    #[no_mangle]
    pub extern "C" fn deploy_upgrade() {
        let state = Engine::get_state();
        let index = sdk::read_u64(CODE_STAGE_KEY).sdk_unwrap();
        if sdk::block_index() <= index + state.upgrade_delay_blocks {
            sdk::panic_utf8(b"ERR_NOT_ALLOWED:TOO_EARLY");
        }
        sdk::self_deploy(CODE_KEY);
    }

    ///
    /// MUTATIVE METHODS
    ///

    /// Deploy code into the EVM.
    #[no_mangle]
    pub extern "C" fn deploy_code() {
        let input = sdk::read_input();
        let mut engine = Engine::new(predecessor_address());
        Engine::deploy_code_with_input(&mut engine, &input)
            .map(|res| res.try_to_vec().sdk_expect("ERR_SERIALIZE"))
            .sdk_process();
        // TODO: charge for storage
    }

    /// Call method on the EVM contract.
    #[no_mangle]
    pub extern "C" fn call() {
        let input = sdk::read_input();
        let args = FunctionCallArgs::try_from_slice(&input).sdk_expect("ERR_ARG_PARSE");
        let mut engine = Engine::new(predecessor_address());
        Engine::call_with_args(&mut engine, args)
            .map(|res| res.try_to_vec().sdk_expect("ERR_SERIALIZE"))
            .sdk_process();
        // TODO: charge for storage
    }

    /// Process signed Ethereum transaction.
    /// Must match CHAIN_ID to make sure it's signed for given chain vs replayed from another chain.
    #[no_mangle]
    pub extern "C" fn submit() {
        use crate::transaction::EthSignedTransaction;
        use rlp::{Decodable, Rlp};

        let input = sdk::read_input();
        let signed_transaction = EthSignedTransaction::decode(&Rlp::new(&input))
            .map_err(|_| ())
            .sdk_expect("ERR_INVALID_TX");

        let state = Engine::get_state();

        // Validate the chain ID, if provided inside the signature:
        if let Some(chain_id) = signed_transaction.chain_id() {
            if U256::from(chain_id) != U256::from(state.chain_id) {
                sdk::panic_utf8(b"ERR_INVALID_CHAIN_ID");
            }
        }

        // Retrieve the signer of the transaction:
        let sender = match signed_transaction.sender() {
            Some(sender) => sender,
            None => sdk::panic_utf8(b"ERR_INVALID_ECDSA_SIGNATURE"),
        };

        let next_nonce =
            Engine::check_nonce(&sender, &signed_transaction.transaction.nonce).sdk_unwrap();

        // Figure out what kind of a transaction this is, and execute it:
        let mut engine = Engine::new_with_state(state, sender);
        let value = signed_transaction.transaction.value;
        let data = signed_transaction.transaction.data;
        if let Some(receiver) = signed_transaction.transaction.to {
            let result = if data.is_empty() {
                // Execute a balance transfer. We need to save the incremented nonce in this case
                // because it is not handled internally by SputnikVM like it is in the case of
                // `call` and `deploy_code`.
                Engine::set_nonce(&sender, &next_nonce);
                Engine::transfer(&mut engine, &sender, &receiver, &value)
            } else {
                // Execute a contract call:
                Engine::call(&mut engine, sender, receiver, value, data)
                // TODO: charge for storage
            };
            result
                .map(|res| res.try_to_vec().sdk_expect("ERR_SERIALIZE"))
                .sdk_process();
        } else {
            // Execute a contract deployment:
            Engine::deploy_code(&mut engine, sender, value, &data)
                .map(|res| res.try_to_vec().sdk_expect("ERR_SERIALIZE"))
                .sdk_process();
            // TODO: charge for storage
        }
    }

    #[no_mangle]
    pub extern "C" fn meta_call() {
        let input = sdk::read_input();
        let state = Engine::get_state();
        let domain_separator = crate::meta_parsing::near_erc712_domain(U256::from(state.chain_id));
        let meta_call_args = match crate::meta_parsing::parse_meta_call(
            &domain_separator,
            &sdk::current_account_id(),
            input,
        ) {
            Ok(args) => args,
            Err(_error_kind) => {
                sdk::panic_utf8(b"ERR_META_TX_PARSE");
            }
        };

        Engine::check_nonce(&meta_call_args.sender, &meta_call_args.nonce).sdk_unwrap();

        let mut engine = Engine::new_with_state(state, meta_call_args.sender);
        let result = engine.call(
            meta_call_args.sender,
            meta_call_args.contract_address,
            meta_call_args.value,
            meta_call_args.input,
        );
        result
            .map(|res| res.try_to_vec().sdk_expect("ERR_SERIALIZE"))
            .sdk_process();
    }

    #[cfg(feature = "testnet")]
    #[no_mangle]
    pub extern "C" fn make_it_rain() {
        let input = sdk::read_input();
        let address = Address::from_slice(&input);
        let mut engine = Engine::new(address);
        let result = engine.credit(&address);
        result.map(|_f| Vec::new()).sdk_process();
    }

    ///
    /// NONMUTATIVE METHODS
    ///

    #[no_mangle]
    pub extern "C" fn view() {
        let input = sdk::read_input();
        let args = ViewCallArgs::try_from_slice(&input).sdk_expect("ERR_ARG_PARSE");
        let engine = Engine::new(Address::from_slice(&args.sender));
        let result = Engine::view_with_args(&engine, args);
        result.sdk_process()
    }

    #[no_mangle]
    pub extern "C" fn get_code() {
        let address = sdk::read_input_arr20();
        let code = Engine::get_code(&Address(address));
        sdk::return_output(&code)
    }

    #[no_mangle]
    pub extern "C" fn get_balance() {
        let address = sdk::read_input_arr20();
        let balance = Engine::get_balance(&Address(address));
        sdk::return_output(&u256_to_arr(&balance))
    }

    #[no_mangle]
    pub extern "C" fn get_nonce() {
        let address = sdk::read_input_arr20();
        let nonce = Engine::get_nonce(&Address(address));
        sdk::return_output(&u256_to_arr(&nonce))
    }

    #[no_mangle]
    pub extern "C" fn get_storage_at() {
        let input = sdk::read_input();
        let args = GetStorageAtArgs::try_from_slice(&input).sdk_expect("ERR_ARG_PARSE");
        let value = Engine::get_storage(&Address(args.address), &H256(args.key));
        sdk::return_output(&value.0)
    }

    ///
    /// BENCHMARKING METHODS
    ///

    #[cfg(feature = "evm_bully")]
    #[no_mangle]
    pub extern "C" fn begin_chain() {
        let mut state = Engine::get_state();
        require_owner_only(&state);
        let input = sdk::read_input();
        let args = BeginChainArgs::try_from_slice(&input).sdk_expect("ERR_ARG_PARSE");
        state.chain_id = args.chain_id;
        Engine::set_state(state);
        // set genesis block balances
        for account_balance in args.genesis_alloc {
            Engine::set_balance(
                &Address(account_balance.address),
                &U256::from(account_balance.balance),
            )
        }
        // return new chain ID
        sdk::return_output(&Engine::get_state().chain_id)
    }

    #[cfg(feature = "evm_bully")]
    #[no_mangle]
    pub extern "C" fn begin_block() {
        let state = Engine::get_state();
        require_owner_only(&state);
        let input = sdk::read_input();
        let _args = BeginBlockArgs::try_from_slice(&input).sdk_expect("ERR_ARG_PARSE");
        // TODO: https://github.com/aurora-is-near/aurora-engine/issues/2
    }

    #[no_mangle]
    pub extern "C" fn new_eth_connector() {
        EthConnectorContract::init_contract()
    }

    #[no_mangle]
    pub extern "C" fn withdraw() {
        EthConnectorContract::get_instance().withdraw_near()
    }

    #[no_mangle]
    pub extern "C" fn deposit() {
        EthConnectorContract::get_instance().deposit()
    }

    #[no_mangle]
    pub extern "C" fn finish_deposit_near() {
        EthConnectorContract::get_instance().finish_deposit_near();
    }

    #[no_mangle]
    pub extern "C" fn ft_total_supply() {
        EthConnectorContract::get_instance().ft_total_supply();
    }

    #[no_mangle]
    pub extern "C" fn ft_total_supply_near() {
        EthConnectorContract::get_instance().ft_total_supply_near();
    }

    #[no_mangle]
    pub extern "C" fn ft_total_supply_eth() {
        EthConnectorContract::get_instance().ft_total_supply_eth();
    }

    #[no_mangle]
    pub extern "C" fn ft_balance_of() {
        EthConnectorContract::get_instance().ft_balance_of();
    }

    #[no_mangle]
    pub extern "C" fn ft_balance_of_eth() {
        EthConnectorContract::get_instance().ft_balance_of_eth();
    }

    #[no_mangle]
    pub extern "C" fn ft_transfer() {
        EthConnectorContract::get_instance().ft_transfer();
    }

    #[no_mangle]
    pub extern "C" fn ft_resolve_transfer() {
        EthConnectorContract::get_instance().ft_resolve_transfer();
    }

    #[no_mangle]
    pub extern "C" fn ft_transfer_call() {
        EthConnectorContract::get_instance().ft_transfer_call();
    }

    #[no_mangle]
    pub extern "C" fn storage_deposit() {
        EthConnectorContract::get_instance().storage_deposit()
    }

    #[no_mangle]
    pub extern "C" fn storage_withdraw() {
        EthConnectorContract::get_instance().storage_withdraw()
    }

    #[no_mangle]
    pub extern "C" fn storage_balance_of() {
        EthConnectorContract::get_instance().storage_balance_of()
    }

    #[no_mangle]
    pub extern "C" fn register_relayer() {
        EthConnectorContract::get_instance().register_relayer()
    }

    #[no_mangle]
    pub extern "C" fn ft_on_transfer() {
        EthConnectorContract::get_instance().ft_on_transfer()
    }

    #[cfg(feature = "integration-test")]
    #[no_mangle]
    pub extern "C" fn verify_log_entry() {
        #[cfg(feature = "log")]
        sdk::log("Call from verify_log_entry");
        let data = true.try_to_vec().unwrap();
        sdk::return_output(&data[..]);
    }

    ///
    /// Utility methods.
    ///

    fn require_owner_only(state: &EngineState) {
        if state.owner_id.as_bytes() != sdk::predecessor_account_id() {
            sdk::panic_utf8(b"ERR_NOT_ALLOWED");
        }
    }

    fn predecessor_address() -> Address {
        near_account_to_evm_address(&sdk::predecessor_account_id())
    }

    trait SdkExpect<T> {
        fn sdk_expect(self, msg: &str) -> T;
    }

    impl<T> SdkExpect<T> for Option<T> {
        fn sdk_expect(self, msg: &str) -> T {
            match self {
                Some(t) => t,
                None => sdk::panic_utf8(msg.as_ref()),
            }
        }
    }

    impl<T, E> SdkExpect<T> for Result<T, E> {
        fn sdk_expect(self, msg: &str) -> T {
            match self {
                Ok(t) => t,
                Err(_) => sdk::panic_utf8(msg.as_ref()),
            }
        }
    }

    trait SdkUnwrap<T> {
        fn sdk_unwrap(self) -> T;
    }

    impl<T> SdkUnwrap<T> for Option<T> {
        fn sdk_unwrap(self) -> T {
            match self {
                Some(t) => t,
                None => sdk::panic_utf8("ERR_UNWRAP".as_bytes()),
            }
        }
    }

    impl<T, E: AsRef<[u8]>> SdkUnwrap<T> for Result<T, E> {
        fn sdk_unwrap(self) -> T {
            match self {
                Ok(t) => t,
                Err(e) => sdk::panic_utf8(e.as_ref()),
            }
        }
    }

    trait SdkProcess<T> {
        fn sdk_process(self);
    }

    impl<T: AsRef<[u8]>> SdkProcess<T> for EngineResult<T> {
        fn sdk_process(self) {
            match self {
                Ok(r) => sdk::return_output(r.as_ref()),
                Err(e) => sdk::panic_utf8(e.as_ref()),
            }
        }
    }
}
