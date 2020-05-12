// Copyright (C) 2019 Yee Foundation.
//
// This file is part of YeeChain.
//
// YeeChain is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// YeeChain is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with YeeChain.  If not, see <https://www.gnu.org/licenses/>.

//! A simple, secure module for dealing with fungible assets.

// Ensure we're `no_std` when compiling for Wasm.
#![cfg_attr(not(feature = "std"), no_std)]

use codec::{Encode};
use frame_support::{StorageValue, StorageMap, Parameter, decl_module, decl_event, decl_storage, ensure};
use sp_runtime::{traits::{Member, AtLeast32Bit, Zero, One, StaticLookup}, DispatchResult, DispatchError};
use yp_sharding::ShardingInfo;
use frame_system::{self as system, ensure_signed};
use sp_std::prelude::Vec;
use sp_std::convert::TryInto;
use yp_relay::{RelayTypes, OriginExtrinsic, SHARD_CODE_SIZE};

pub trait Trait: sharding::Trait {
	/// The overarching event type.
	type Event: From<Event<Self>> + Into<<Self as system::Trait>::Event>;

	/// The units in which we record balances.
	type Balance: Member + Parameter + AtLeast32Bit + Default + Copy;

	/// The arithmetic type of asset identifier.
	type AssetId: Parameter + AtLeast32Bit + Default + Copy;

	type Sharding: ShardingInfo<Self::ShardNum>;
}

// type AssetId = u32;

type Decimals = u16;

const MAX_NAME_SIZE: usize = 16;

decl_module! {
	// Simple declaration of the `Module` type. Lets the macro know what its working on.
	pub struct Module<T: Trait> for enum Call where origin: T::Origin {
		fn deposit_event() = default;
		/// Issue a new class of fungible assets. There are, and will only ever be, `total`
		/// such assets and they'll all belong to the `origin` initially. It will have an
		/// identifier `AssetId` instance: this will be specified in the `Issued` event.
		#[weight = 0]
		fn issue(origin, name: Vec<u8>, #[compact] total: T::Balance, #[compact] decimals: Decimals) -> DispatchResult {
			let origin = ensure_signed(origin)?;

			if name.len() > MAX_NAME_SIZE {
				return Err(DispatchError::Other("Asset's name's length overflow."))
			}
			Self::issue_asset(origin, name, total, decimals);

			Ok(())
		}

		/// Move some assets from one holder to another.
		#[weight = 0]
		fn transfer(origin,
			shard_code: Vec<u8>,
			#[compact] id: T::AssetId,
			target: <T::Lookup as StaticLookup>::Source,
			#[compact] amount: T::Balance
		) {
			let origin = ensure_signed(origin)?;
			let origin_account = (shard_code.clone(), id, origin.clone());
			let origin_balance = <Balances<T>>::get(&origin_account);

			ensure!(!amount.is_zero(), "transfer amount should be non-zero");
			ensure!(origin_balance >= amount, "origin account balance must be greater than or equal to the transfer amount");

			// change amount about origin account
			<Balances<T>>::insert(origin_account, origin_balance - amount);
			let target = T::Lookup::lookup(target)?;

			let (cn, c) = (T::Sharding::get_curr_shard().expect("qed").try_into().ok().expect("qed") as u16,
									T::Sharding::get_shard_count().try_into().ok().expect("qed") as u16);
			let dn = yp_sharding::utils::shard_num_for(&target, c).expect("qed");
			// in same sharding
			if cn == dn {
				<Balances<T>>::mutate((shard_code.clone(), id, target.clone()), |balance| *balance += amount);
			}
			// event
			Self::deposit_event(RawEvent::Transferred(shard_code, id, origin, target, amount));
		}
	}
}

decl_event!(
	pub enum Event<T> where
		<T as system::Trait>::AccountId,
		<T as Trait>::Balance,
		<T as Trait>::AssetId
	{
		/// Some assets were issued.
		Issued(Vec<u8>, AssetId, Vec<u8>, AccountId, Balance, u16),
		/// Some assets were transferred.
		Transferred(Vec<u8>, AssetId, AccountId, AccountId, Balance),
	}
);

decl_storage! {
	trait Store for Module<T: Trait> as Assets {
		/// The number of units of assets held by any given account.
		Balances: map hasher(blake2_128_concat) (Vec<u8>, T::AssetId, T::AccountId) => T::Balance;
		/// The next asset identifier up for grabs.
		NextAssetId get(fn next_asset_id) config(): T::AssetId;
		/// The name of an asset.
		AssetsName: map hasher(twox_64_concat) T::AssetId => Vec<u8>;
		/// The total unit supply of an asset
		TotalSupply: map hasher(twox_64_concat) T::AssetId => T::Balance;
		/// The Asset's decimals.
		AssetsDecimals: map hasher(twox_64_concat) T::AssetId => Decimals;
		/// The asset's issuer.
		AssetsIssuer: map hasher(twox_64_concat) T::AssetId => T::AccountId;
	}
}

// The main implementation block for the module.
impl<T: Trait> Module<T> {
	fn issue_asset(origin: T::AccountId, name: Vec<u8>, total: T::Balance, decimals: Decimals) {
		let id = Self::next_asset_id();
		<NextAssetId<T>>::mutate(|id| *id += One::one());

		let issuer = origin.encode();
		let shard_code = issuer[issuer.len() - SHARD_CODE_SIZE..].to_vec();

		<Balances<T>>::insert((shard_code.clone(), id, origin.clone()), total.clone());
		<TotalSupply<T>>::insert(id, total.clone());
		<AssetsName<T>>::insert(id, name.clone());
		<AssetsDecimals<T>>::insert(id, decimals.clone());
		<AssetsIssuer<T>>::insert(id, origin.clone());

		// event
		Self::deposit_event(RawEvent::Issued(shard_code, id, name, origin, total, decimals));
	}

	/// Get the asset `id` balance of `who`.
	pub fn balance(shard_code: Vec<u8>, id: T::AssetId, who: T::AccountId) -> T::Balance {
		<Balances<T>>::get((shard_code, id, who))
	}

	/// Get the total supply of an asset `id`
	pub fn total_supply(id: T::AssetId) -> T::Balance {
		<TotalSupply<T>>::get(id)
	}

	/// Get the name of an asset `id`
	pub fn name(id: T::AssetId) -> Vec<u8> { <AssetsName<T>>::get(id) }

	/// Get the decimals of an asset `id`
	pub fn decimals(id: T::AssetId) -> Decimals { <AssetsDecimals<T>>::get(id) }

	/// Get the issuer of an asset `id`
	pub fn issuer(id: T::AssetId) -> T::AccountId { <AssetsIssuer<T>>::get(id) }

	/// relay transfer
	pub fn relay_transfer(input: Vec<u8>) -> DispatchResult {
		if let Some(tx) = OriginExtrinsic::<T::AccountId, T::Balance>::decode(RelayTypes::Assets, input) {
			let asset_id: T::AssetId = tx.asset_id().unwrap().into();
			<Balances<T>>::mutate((tx.shard_code(), asset_id, tx.to()), |balance| *balance += tx.amount());
			Self::deposit_event(RawEvent::Transferred(tx.shard_code(), asset_id, tx.from(), tx.to(), tx.amount()));
			Ok(())
		} else{
			Err(DispatchError::Other("transfer is invalid."))
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	use runtime_io::with_externalities;
	use srml_support::{impl_outer_origin, assert_ok, assert_noop};
	use substrate_primitives::{H256, Blake2Hasher};
	// The testing primitives are very useful for avoiding having to work with signatures
	// or public keys. `u64` is used as the `AccountId` and no `Signature`s are required.
	use primitives::{
		BuildStorage,
		traits::{BlakeTwo256, IdentityLookup},
		testing::{Digest, DigestItem, Header}
	};

	impl_outer_origin! {
		pub enum Origin for Test {}
	}

	// For testing the module, we construct most of a mock runtime. This means
	// first constructing a configuration type (`Test`) which `impl`s each of the
	// configuration traits of modules we want to use.
	#[derive(Clone, Eq, PartialEq)]
	pub struct Test;
	impl system::Trait for Test {
		type Origin = Origin;
		type Index = u64;
		type BlockNumber = u64;
		type Hash = H256;
		type Hashing = BlakeTwo256;
		type Digest = Digest;
		type AccountId = u64;
		type Lookup = IdentityLookup<Self::AccountId>;
		type Header = Header;
		type Event = ();
		type Log = DigestItem;
	}
	impl Trait for Test {
		type Event = ();
		type Balance = u64;
		type Sharding = ();
	}
	type Assets = Module<Test>;

	// This function basically just builds a genesis storage key/value store according to
	// our desired mockup.
	fn new_test_ext() -> runtime_io::TestExternalities<Blake2Hasher> {
		system::GenesisConfig::<Test>::default().build_storage().unwrap().0.into()
	}

	#[test]
	fn issuing_asset_units_to_issuer_should_work() {
		with_externalities(&mut new_test_ext(), || {
			assert_ok!(Assets::issue(Origin::signed(1), 100));
			assert_eq!(Assets::balance(0, 1), 100);
		});
	}

	#[test]
	fn querying_total_supply_should_work() {
		with_externalities(&mut new_test_ext(), || {
			assert_ok!(Assets::issue(Origin::signed(1), 100));
			assert_eq!(Assets::balance(0, 1), 100);
			assert_ok!(Assets::transfer(Origin::signed(1), 0, 2, 50));
			assert_eq!(Assets::balance(0, 1), 50);
			assert_eq!(Assets::balance(0, 2), 50);
			assert_ok!(Assets::transfer(Origin::signed(2), 0, 3, 31));
			assert_eq!(Assets::balance(0, 1), 50);
			assert_eq!(Assets::balance(0, 2), 19);
			assert_eq!(Assets::balance(0, 3), 31);
			assert_ok!(Assets::destroy(Origin::signed(3), 0));
			assert_eq!(Assets::total_supply(0), 69);
		});
	}

	#[test]
	fn transferring_amount_above_available_balance_should_work() {
		with_externalities(&mut new_test_ext(), || {
			assert_ok!(Assets::issue(Origin::signed(1), 100));
			assert_eq!(Assets::balance(0, 1), 100);
			assert_ok!(Assets::transfer(Origin::signed(1), 0, 2, 50));
			assert_eq!(Assets::balance(0, 1), 50);
			assert_eq!(Assets::balance(0, 2), 50);
		});
	}

	#[test]
	fn transferring_amount_less_than_available_balance_should_not_work() {
		with_externalities(&mut new_test_ext(), || {
			assert_ok!(Assets::issue(Origin::signed(1), 100));
			assert_eq!(Assets::balance(0, 1), 100);
			assert_ok!(Assets::transfer(Origin::signed(1), 0, 2, 50));
			assert_eq!(Assets::balance(0, 1), 50);
			assert_eq!(Assets::balance(0, 2), 50);
			assert_ok!(Assets::destroy(Origin::signed(1), 0));
			assert_eq!(Assets::balance(0, 1), 0);
			assert_noop!(Assets::transfer(Origin::signed(1), 0, 1, 50), "origin account balance must be greater than or equal to the transfer amount");
		});
	}

	#[test]
	fn transferring_less_than_one_unit_should_not_work() {
		with_externalities(&mut new_test_ext(), || {
			assert_ok!(Assets::issue(Origin::signed(1), 100));
			assert_eq!(Assets::balance(0, 1), 100);
			assert_noop!(Assets::transfer(Origin::signed(1), 0, 2, 0), "transfer amount should be non-zero");
		});
	}

	#[test]
	fn transferring_more_units_than_total_supply_should_not_work() {
		with_externalities(&mut new_test_ext(), || {
			assert_ok!(Assets::issue(Origin::signed(1), 100));
			assert_eq!(Assets::balance(0, 1), 100);
			assert_noop!(Assets::transfer(Origin::signed(1), 0, 2, 101), "origin account balance must be greater than or equal to the transfer amount");
		});
	}

	#[test]
	fn destroying_asset_balance_with_positive_balance_should_work() {
		with_externalities(&mut new_test_ext(), || {
			assert_ok!(Assets::issue(Origin::signed(1), 100));
			assert_eq!(Assets::balance(0, 1), 100);
			assert_ok!(Assets::destroy(Origin::signed(1), 0));
		});
	}

	#[test]
	fn destroying_asset_balance_with_zero_balance_should_not_work() {
		with_externalities(&mut new_test_ext(), || {
			assert_ok!(Assets::issue(Origin::signed(1), 100));
			assert_eq!(Assets::balance(0, 2), 0);
			assert_noop!(Assets::destroy(Origin::signed(2), 0), "origin balance should be non-zero");
		});
	}
}
