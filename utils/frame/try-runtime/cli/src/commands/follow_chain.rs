// This file is part of Substrate.

// Copyright (C) 2021-2022 Parity Technologies (UK) Ltd.
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

use crate::{
	build_executor, ensure_matching_spec, extract_code, full_extensions, local_spec, parse,
	state_machine_call_with_proof, SharedParams, LOG_TARGET,
};
use jsonrpsee::{
	core::{async_trait, client::{Client, Subscription, SubscriptionClientT}},
	ws_client::WsClientBuilder,
};
use parity_scale_codec::Decode;
use remote_externalities::{rpc_api, Builder, Mode, OnlineConfig};
use sc_executor::NativeExecutionDispatch;
use sc_service::Configuration;
use serde::de::DeserializeOwned;
use sp_core::H256;
use sp_runtime::traits::{Block as BlockT, Header as HeaderT, NumberFor, One, Saturating};
use std::{
	collections::VecDeque, fmt::Debug, marker::PhantomData, ops::Sub, str::FromStr
};

const SUB: &str = "chain_subscribeFinalizedHeads";
const UN_SUB: &str = "chain_unsubscribeFinalizedHeads";

/// Configurations of the [`Command::FollowChain`].
#[derive(Debug, Clone, clap::Parser)]
pub struct FollowChainCmd {
	/// The url to connect to.
	#[clap(short, long, parse(try_from_str = parse::url))]
	uri: String,
}

/// Start listening for with `SUB` at `url`.
///
/// Returns a pair `(client, subscription)` - `subscription` alone will be useless, because it
/// relies on the related alive `client`.
async fn start_subscribing<Header: DeserializeOwned>(url: &str) -> (Client, Subscription<Header>) {
	let client = WsClientBuilder::default()
		.connection_timeout(std::time::Duration::new(20, 0))
		.max_notifs_per_subscription(1024)
		.max_request_body_size(u32::MAX)
		.build(url)
		.await
		.unwrap();

	log::info!(target: LOG_TARGET, "subscribing to {:?} / {:?}", SUB, UN_SUB);

	let sub =
		client.subscribe(SUB, None, UN_SUB).await.unwrap();
	(client, sub)
}

/// Abstraction over RPC calling for headers.
#[async_trait]
trait HeaderProvider<Block: BlockT> where Block::Header: HeaderT {
	async fn get_header(&self, hash: Block::Hash) -> Block::Header;
}

struct RpcHeaderProvider<Block: BlockT>{
	uri: String,
	_phantom: PhantomData<Block>,
}

#[async_trait]
impl<Block: BlockT> HeaderProvider<Block> for RpcHeaderProvider<Block>
	where Block::Header: DeserializeOwned
{
	async fn get_header(&self, hash: Block::Hash) -> Block::Header {
		rpc_api::get_header::<Block, _>(&self.uri, hash).await.unwrap()
	}
}

/// Stream of all finalized headers.
///
/// Returned headers are guaranteed to be ordered. There are no missing headers (even if some of
/// them lack justification).
struct FinalizedHeaders<Block: BlockT, HP: HeaderProvider<Block>> {
	subscription: Subscription<Block::Header>,
	header_provider: HP,
	fetched_headers: VecDeque<Block::Header>,
	last_returned: Option<<Block::Header as HeaderT>::Number>,
}

impl<Block: BlockT, HP: HeaderProvider<Block>> FinalizedHeaders<Block, HP>
where
	<Block as BlockT>::Header: DeserializeOwned
{
	pub fn new(subscription: Subscription<Block::Header>, header_provider: HP) -> Self {
		Self {
			subscription,
			header_provider,
			fetched_headers: VecDeque::new(),
			last_returned: None,
		}
	}

	/// Await for the next finalized header from the subscription.
	///
	/// Returns `None` if either the subscription has been closed or there was an error when reading
	/// an object from the client.
	async fn next_from_subscription(&mut self) -> Option<Block::Header> {
		match self.subscription.next().await {
			Some(Ok(header)) => Some(header),
			None => {
				log::warn!("subscription closed");
				None
			}
			Some(Err(why)) => {
				log::warn!("subscription returned error: {:?}. Probably decoding has failed.", why);
				None
			}
		}
	}

	/// Reads next finalized header from the subscription. If some headers (without justification)
	/// have been skipped, fetches them as well.
	///
	/// All fetched headers are stored in `self.fetched_headers`.
	async fn fetch(&mut self) {
		let last_finalized = match self.next_from_subscription().await {
			Some(header) => header,
			None => return,
		};

		self.fetched_headers.push_front(last_finalized.clone());

		let current_height = last_finalized.number();
		let parent_height = current_height.sub(One::one());
		let last_height = self.last_returned.unwrap_or(parent_height);

		let mut parent_hash = last_finalized.parent_hash().clone();
		for _ in 0u32..(parent_height.saturating_sub(last_height).try_into().unwrap_or_default()) {
			let parent_header = self.header_provider.get_header(parent_hash).await;
			self.fetched_headers.push_front(parent_header.clone());
			parent_hash = *parent_header.parent_hash();
		}
	}

	/// Get the next finalized header.
	pub async fn next(&mut self) -> Option<Block::Header> {
		if self.fetched_headers.is_empty() {
			self.fetch().await;
		}

		if let Some(header) = self.fetched_headers.pop_front() {
			self.last_returned = Some(*header.number());
			Some(header)
		} else {
			None
		}
	}
}

pub(crate) async fn follow_chain<Block, ExecDispatch>(
	shared: SharedParams,
	command: FollowChainCmd,
	config: Configuration,
) -> sc_cli::Result<()>
where
	Block: BlockT<Hash = H256> + DeserializeOwned,
	Block::Hash: FromStr,
	Block::Header: DeserializeOwned,
	<Block::Hash as FromStr>::Err: Debug,
	NumberFor<Block>: FromStr,
	<NumberFor<Block> as FromStr>::Err: Debug,
	ExecDispatch: NativeExecutionDispatch + 'static,
{
	let mut maybe_state_ext = None;
	let (_client, subscription) = start_subscribing::<Block::Header>(&command.uri).await;

	let (code_key, code) = extract_code(&config.chain_spec)?;
	let executor = build_executor::<ExecDispatch>(&shared, &config);
	let execution = shared.execution;

	let header_provider: RpcHeaderProvider<Block> = RpcHeaderProvider {
		uri: command.uri.clone(),
		_phantom: PhantomData {}
	};
	let mut finalized_headers: FinalizedHeaders<Block, RpcHeaderProvider<Block>> =
		FinalizedHeaders::new(subscription, header_provider);

	while let Some(header) = finalized_headers.next().await {
		let hash = header.hash();
		let number = header.number();

		let block = rpc_api::get_block::<Block, _>(&command.uri, hash).await.unwrap();

		log::debug!(
			target: LOG_TARGET,
			"new block event: {:?} => {:?}, extrinsics: {}",
			hash,
			number,
			block.extrinsics().len()
		);

		// create an ext at the state of this block, whatever is the first subscription event.
		if maybe_state_ext.is_none() {
			let builder = Builder::<Block>::new().mode(Mode::Online(OnlineConfig {
				transport: command.uri.clone().into(),
				at: Some(*header.parent_hash()),
				..Default::default()
			}));

			let new_ext = builder
				.inject_hashed_key_value(&[(code_key.clone(), code.clone())])
				.build()
				.await?;
			log::info!(
				target: LOG_TARGET,
				"initialized state externalities at {:?}, storage root {:?}",
				number,
				new_ext.as_backend().root()
			);

			let (expected_spec_name, expected_spec_version, spec_state_version) =
				local_spec::<Block, ExecDispatch>(&new_ext, &executor);
			ensure_matching_spec::<Block>(
				command.uri.clone(),
				expected_spec_name,
				expected_spec_version,
				shared.no_spec_name_check,
			)
			.await;

			maybe_state_ext = Some((new_ext, spec_state_version));
		}

		let (state_ext, spec_state_version) =
			maybe_state_ext.as_mut().expect("state_ext either existed or was just created");

		let (mut changes, encoded_result) = state_machine_call_with_proof::<Block, ExecDispatch>(
			state_ext,
			&executor,
			execution,
			"TryRuntime_execute_block_no_check",
			block.encode().as_ref(),
			full_extensions(),
		)?;

		let consumed_weight = <u64 as Decode>::decode(&mut &*encoded_result)
			.map_err(|e| format!("failed to decode output: {:?}", e))?;

		let storage_changes = changes
			.drain_storage_changes(
				&state_ext.backend,
				&mut Default::default(),
				// Note that in case a block contains a runtime upgrade,
				// state version could potentially be incorrect here,
				// this is very niche and would only result in unaligned
				// roots, so this use case is ignored for now.
				*spec_state_version,
			)
			.unwrap();
		state_ext.backend.apply_transaction(
			storage_changes.transaction_storage_root,
			storage_changes.transaction,
		);

		log::info!(
			target: LOG_TARGET,
			"executed block {}, consumed weight {}, new storage root {:?}",
			number,
			consumed_weight,
			state_ext.as_backend().root(),
		);
	}

	log::error!(target: LOG_TARGET, "ws subscription must have terminated.");
	Ok(())
}
