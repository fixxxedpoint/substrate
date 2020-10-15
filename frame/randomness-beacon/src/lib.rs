// This file is part of Substrate.

// Copyright (C) 2019-2020 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![cfg_attr(not(feature = "std"), no_std)]

use codec::{Decode, Encode};
use frame_support::{
	decl_error, decl_module, decl_storage, traits::Randomness as RandomnessT, weights::Weight,
};
use frame_system::ensure_none;
use sp_inherents::{InherentData, InherentIdentifier, ProvideInherent};
use sp_randomness_beacon::{
	inherents::{InherentError, INHERENT_IDENTIFIER},
	Randomness, VerifyKey, START_BEACON_HEIGHT,
};
use sp_runtime::print;
use sp_std::convert::TryInto;
use sp_std::{result, vec::Vec};

pub trait Trait: frame_system::Trait {}

decl_storage! {
	trait Store for Module<T: Trait> as RandomnessBeacon {
		/// Random Bytes for the current block
		Seed: Vec<u8>;
		/// Was Seed set in this block?
		DidUpdate: bool;
		/// Stores verifier needed to check randomness in blocks
		RandomnessVerifier get(fn verifier): VerifyKey;

		/// store for invalid keys (issuer, secret_share, already_disputed)
		InvalidSecretShares: Vec<(u64, Vec<u8>, bool)>;
	}
}

decl_error! {
	pub enum Error for Module<T: Trait> {
		SeedNotAvailable,
	}
}

decl_module! {
	pub struct Module<T: Trait> for enum Call where origin: T::Origin {
		type Error = Error<T>;

		fn on_initialize(now: T::BlockNumber) -> Weight {
			if !<Self as Store>::RandomnessVerifier::exists() {
				// TODO: this add default verify_key, refactor to add the proper one
				 <Self as Store>::RandomnessVerifier::set(sp_randomness_beacon::VerifyKey::default());
			}

			0
		}

		#[weight = 0]
		fn set_random_bytes(origin, random_bytes: Vec<u8>)  {
			ensure_none(origin)?;

			assert!(!<Self as Store>::DidUpdate::exists(), "Randomness must be set only once in the block");

			<Self as Store>::Seed::set(random_bytes);
			<Self as Store>::DidUpdate::put(true);
		}

		// this extrinsic is called from within an offchain worker
		#[weight = 0]
		fn share_secrets(origin, encoded_secret_shares: Vec<Vec<u8>>, commitments: Vec<Vec<u8>>) {
			// If there is a share for us decode it and check if it is valid, if not, issue a dispute!
			// Mark a share for dispute by storing it in InvalidSecretShares store.
		}

		// this extrinsic is called from within an offchain worker
		#[weight = 0]
		fn issue_dispute(origin, decoded_share: Vec<u8>, encoding_key: Vec<u8>, zkp: Vec<u8>) {}

		fn on_finalize(bn: T::BlockNumber) {
			if bn >= START_BEACON_HEIGHT.into() {
				assert!(<Self as Store>::DidUpdate::take(), "Randomness must be put into the block");
			}
		}

		fn offchain_worker(block_number: T::BlockNumber) {
			// 1. share secret if not shared yet

			// 2. check if there is a dispute that needs to be started
		}
	}
}

fn extract_random_bytes(inherent_data: &InherentData) -> Vec<u8> {
	let randomness: Result<Option<Randomness>, _> = inherent_data.get_data(&INHERENT_IDENTIFIER);
	assert!(
		randomness.is_ok(),
		"Panic because of error in retrieving inherent_data with err {:?}.",
		randomness.err().unwrap()
	);
	let randomness = randomness.unwrap();
	assert!(
		randomness.is_some(),
		"Panic because no random_bytes found in inherent_data."
	);
	Randomness::encode(&randomness.unwrap())
}

impl<T: Trait> ProvideInherent for Module<T> {
	type Call = Call<T>;
	type Error = InherentError;
	const INHERENT_IDENTIFIER: InherentIdentifier = INHERENT_IDENTIFIER;

	fn create_inherent(data: &InherentData) -> Option<Self::Call> {
		let now = <frame_system::Module<T>>::block_number();
		print((
			"create_inherent block height: ",
			now.try_into().unwrap_or_default(),
		));
		if now >= T::BlockNumber::from(START_BEACON_HEIGHT) {
			return Some(Self::Call::set_random_bytes(extract_random_bytes(data)));
		}
		None
	}

	fn check_inherent(call: &Self::Call, _: &InherentData) -> result::Result<(), Self::Error> {
		let now = <frame_system::Module<T>>::block_number();
		print((
			"check_inherent block height: ",
			now.try_into().unwrap_or_default(),
		));
		if now < T::BlockNumber::from(START_BEACON_HEIGHT) {
			return Ok(());
		}
		if !<Self as Store>::RandomnessVerifier::exists() {
			return Err(sp_randomness_beacon::inherents::InherentError::VerifyKeyNotSet);
		}
		let random_bytes = match call {
			Call::set_random_bytes(ref random_bytes) => random_bytes.clone(),
			_ => return Ok(()),
		};
		let verify_key = Self::verifier();
		let randomness = Randomness::decode(&mut &*random_bytes).unwrap();
		if !sp_randomness_beacon::verify_randomness(&verify_key, &randomness) {
			return Err(sp_randomness_beacon::inherents::InherentError::InvalidRandomBytes);
		}
		Ok(())
	}
}

impl<T: Trait> RandomnessT<T::Hash> for Module<T> {
	// TODO: implement
	fn random(_subject: &[u8]) -> T::Hash {
		T::Hash::default()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use frame_support::traits::{OnFinalize, OnInitialize};
	use frame_support::{assert_ok, impl_outer_origin, parameter_types, weights::Weight};
	use sp_core::H256;
	use sp_io::TestExternalities;
	use sp_runtime::{
		testing::Header,
		traits::{BlakeTwo256, IdentityLookup},
		Perbill,
	};

	pub fn new_test_ext() -> TestExternalities {
		let t = frame_system::GenesisConfig::default()
			.build_storage::<Test>()
			.unwrap();
		TestExternalities::new(t)
	}

	impl_outer_origin! {
		pub enum Origin for Test where system = frame_system {}
	}

	#[derive(Clone, Eq, PartialEq)]
	pub struct Test;
	parameter_types! {
		pub const BlockHashCount: u64 = 250;
		pub const MaximumBlockWeight: Weight = 1024;
		pub const MaximumBlockLength: u32 = 2 * 1024;
		pub const AvailableBlockRatio: Perbill = Perbill::one();
	}
	impl frame_system::Trait for Test {
		type BaseCallFilter = ();
		type Origin = Origin;
		type Index = u64;
		type BlockNumber = u64;
		type Call = ();
		type Hash = H256;
		type Hashing = BlakeTwo256;
		type AccountId = u64;
		type Lookup = IdentityLookup<Self::AccountId>;
		type Header = Header;
		type Event = ();
		type BlockHashCount = BlockHashCount;
		type MaximumBlockWeight = MaximumBlockWeight;
		type DbWeight = ();
		type BlockExecutionWeight = ();
		type ExtrinsicBaseWeight = ();
		type MaximumExtrinsicWeight = MaximumBlockWeight;
		type AvailableBlockRatio = AvailableBlockRatio;
		type MaximumBlockLength = MaximumBlockLength;
		type Version = ();
		type PalletInfo = ();
		type AccountData = ();
		type OnNewAccount = ();
		type OnKilledAccount = ();
		type SystemWeightInfo = ();
	}
	parameter_types! {
		pub const MinimumPeriod: u64 = 5;
	}
	impl Trait for Test {}
	type RBeacon = Module<Test>;

	#[test]
	fn randomness_beacon_works() {
		new_test_ext().execute_with(|| {
			assert_eq!(RBeacon::on_initialize(0), 0);
			assert_ok!(RBeacon::set_random_bytes(
				Origin::none(),
				vec![0, 1, 2, 3, 4, 5, 6, 7]
			));
		});
	}

	#[test]
	#[should_panic(expected = "Randomness must be set only once in the block")]
	fn double_randomness_should_fail() {
		new_test_ext().execute_with(|| {
			assert_eq!(RBeacon::on_initialize(0), 0);
			assert_ok!(RBeacon::set_random_bytes(
				Origin::none(),
				vec![0, 1, 2, 3, 4, 5, 6, 7]
			));
			let _ = RBeacon::set_random_bytes(Origin::none(), vec![0, 1, 2, 3, 4, 5, 6, 0]);
		});
	}

	#[test]
	fn verifier_correctly_initialized() {
		new_test_ext().execute_with(|| {
			assert_eq!(RBeacon::on_initialize(0), 0);
			assert!(<RBeacon as Store>::RandomnessVerifier::exists());
		});
	}

	#[test]
	#[should_panic(expected = "Randomness must be put into the block")]
	fn no_randomness_should_fail() {
		new_test_ext().execute_with(|| {
			assert_eq!(RBeacon::on_initialize(5), 0);
			let _ = RBeacon::on_finalize(5);
		});
	}
}
