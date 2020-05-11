
// Ensure we're `no_std` when compiling for Wasm.
#![cfg_attr(not(feature = "std"), no_std)]

use codec::Compact;
use sp_std::vec::Vec;
use sp_std::prelude::*;
use frame_system as system;
use frame_support::{decl_module, decl_event, decl_storage};
use sp_runtime::DispatchResult;
use yp_relay::RelayTypes;

pub trait Trait: system::Trait {
    type Runtime: balances::Trait + assets::Trait;
}

decl_module!{
    pub struct Module<T: Trait> for enum Call where origin: T::Origin {
        #[weight = 0]
        pub fn transfer(_origin, relay_type: RelayTypes, tx: Vec<u8>, _number: Compact<u64>, _hash: T::Hash, _parent: T::Hash) -> DispatchResult{
            match relay_type {
                RelayTypes::Balance => {
                    <balances::Module<T::Runtime>>::relay_transfer(tx)
                },
                RelayTypes::Assets => {
                    <assets::Module<T::Runtime>>::relay_transfer(tx)
                }
            }
        }
    }
}