// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
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

//! Native in-process [`SubstrateClientT`] implementation.
//!
//! [`NativeSubstrateClient`] drives the ETH-RPC server directly through the
//! Substrate node's in-memory client APIs (`sc_client_api`, `sp_api`, etc.),
//! removing the need for a separate `subxt`/WebSocket connection when the RPC
//! server is embedded inside the node binary.
use crate::{
	ClientError, SubstrateClientT,
	client::{Balance, SubscriptionType, SubstrateBlockHash, SubstrateBlockNumber},
	native_block_info_provider::NativeCachedBlock,
	substrate_client::{NodeHealth, RawExtrinsic, SubmitResult},
};
use codec::{Decode, Encode};
use futures::StreamExt;
use jsonrpsee::core::async_trait;
use pallet_revive::{
	EthTransactInfo, ReviveApi,
	evm::{
		Block as EthBlock, BlockNumberOrTagOrHash, GenericTransaction, ReceiptGasInfo, Trace,
		TracerType, U256,
	},
};
use sc_client_api::{BlockBackend, BlockchainEvents, HeaderBackend};
use sc_transaction_pool_api::TransactionPool;
use sp_api::ProvideRuntimeApi;
use sp_core::{H160, H256};
use sp_runtime::{
	OpaqueExtrinsic,
	traits::{Block as BlockT, Header as HeaderT},
};
use sp_weights::Weight;
use std::{future::Future, marker::PhantomData, sync::Arc};

/// Convenience bound that bundles all runtime-API traits required by the native client.
pub trait ReviveRuntimeApiT<Block: BlockT, Moment: codec::Codec>:
	pallet_revive::ReviveApi<Block, sp_core::H160, Balance, u32, u128, Moment>
	+ sp_api::Core<Block>
	+ sp_api::Metadata<Block>
{
}

impl<T, Block: BlockT, Moment: codec::Codec> ReviveRuntimeApiT<Block, Moment> for T where
	T: pallet_revive::ReviveApi<Block, sp_core::H160, Balance, u32, u128, Moment>
		+ sp_api::Core<Block>
		+ sp_api::Metadata<Block>
{
}

/// The opaque block type used by Asset Hub (and most Substrate parachains).
pub type OpaqueBlock = sp_runtime::generic::Block<
	sp_runtime::generic::Header<u32, sp_runtime::traits::BlakeTwo256>,
	sp_runtime::OpaqueExtrinsic,
>;

#[inline]
fn native_err(e: impl std::fmt::Display) -> ClientError {
	ClientError::NativeClientError(e.to_string())
}

/// A [`SubstrateClientT`] backed by the node's native in-process Substrate client.
#[derive(Clone)]
pub struct NativeSubstrateClient<Client, Pool, Block = OpaqueBlock, Moment = u64>
where
	Block: BlockT,
{
	client: Arc<Client>,
	pool: Arc<Pool>,
	chain_id: u64,
	max_block_weight: Weight,
	automine: bool,
	_block: PhantomData<Block>,
	_moment: PhantomData<Moment>,
}

impl<Client, Pool, Block, Moment> NativeSubstrateClient<Client, Pool, Block, Moment>
where
	Block: BlockT<Hash = H256>,
	Block::Header: HeaderT<Number = u32>,
	Moment: codec::Codec + Send + Sync + 'static,
	Client: HeaderBackend<Block>
		+ ProvideRuntimeApi<Block>
		+ BlockBackend<Block>
		+ BlockchainEvents<Block>
		+ Send
		+ Sync
		+ 'static,
	Client::Api: ReviveRuntimeApiT<Block, Moment>,
	Pool: TransactionPool<Block = Block> + Send + Sync + 'static,
{
	/// Create a new native client.
	pub fn new(
		client: Arc<Client>,
		pool: Arc<Pool>,
		chain_id: u64,
		automine: bool,
	) -> Result<Self, ClientError> {
		let best_hash = client.info().best_hash;
		let runtime_api = client.runtime_api();

		let block_gas_limit: u64 = runtime_api
			.block_gas_limit(best_hash)
			.map(|v: U256| v.min(U256::from(u64::MAX)).as_u64())
			.unwrap_or(u64::MAX);

		Ok(Self {
			client,
			pool,
			chain_id,
			max_block_weight: Weight::from_parts(block_gas_limit, u64::MAX),
			automine,
			_block: PhantomData,
			_moment: PhantomData,
		})
	}

	/// Build a [`NativeCachedBlock`] from a block hash by querying the in-process client.
	fn block_info_from_hash(
		&self,
		block_hash: H256,
	) -> Result<Option<NativeCachedBlock>, ClientError> {
		let Some(signed) = self.client.block(block_hash).map_err(native_err)? else {
			return Ok(None);
		};

		let encoded = signed.block.encode();
		let opaque = OpaqueBlock::decode(&mut &encoded[..]).map_err(native_err)?;

		let number: u32 = (*signed.block.header().number()).into();
		Ok(Some(NativeCachedBlock {
			hash: block_hash,
			number,
			parent_hash: *signed.block.header().parent_hash(),
			opaque,
		}))
	}
}

#[async_trait]
impl<Client, Pool, Block, Moment> SubstrateClientT
	for NativeSubstrateClient<Client, Pool, Block, Moment>
where
	Block: BlockT<Hash = H256, Extrinsic = OpaqueExtrinsic> + Send + Sync + 'static,
	Block::Header: HeaderT<Number = u32, Hash = H256> + Send + Sync,
	Moment: codec::Codec + Clone + Send + Sync + 'static,
	Client: HeaderBackend<Block>
		+ ProvideRuntimeApi<Block>
		+ BlockBackend<Block>
		+ BlockchainEvents<Block>
		+ Clone
		+ Send
		+ Sync
		+ 'static,
	Client::Api: ReviveRuntimeApiT<Block, Moment>,
	Pool: TransactionPool<Block = Block> + Clone + Send + Sync + 'static,
{
	type BlockInfo = NativeCachedBlock;

	fn chain_id(&self) -> u64 {
		self.chain_id
	}

	fn max_block_weight(&self) -> Weight {
		self.max_block_weight
	}

	async fn block_by_hash(
		&self,
		hash: &SubstrateBlockHash,
	) -> Result<Option<NativeCachedBlock>, ClientError> {
		self.block_info_from_hash(*hash)
	}

	async fn block_by_number(
		&self,
		number: SubstrateBlockNumber,
	) -> Result<Option<NativeCachedBlock>, ClientError> {
		let hash = self.client.block_hash(number.into()).map_err(native_err)?;
		match hash {
			Some(h) => self.block_info_from_hash(h),
			None => Ok(None),
		}
	}

	async fn latest_block(&self) -> Result<NativeCachedBlock, ClientError> {
		let info = self.client.info();
		self.block_info_from_hash(info.best_hash)?.ok_or(ClientError::BlockNotFound)
	}

	async fn latest_finalized_block(&self) -> Result<NativeCachedBlock, ClientError> {
		let info = self.client.info();
		self.block_info_from_hash(info.finalized_hash)?
			.ok_or(ClientError::BlockNotFound)
	}

	async fn dry_run(
		&self,
		block_hash: SubstrateBlockHash,
		tx: GenericTransaction,
		_block: BlockNumberOrTagOrHash,
	) -> Result<EthTransactInfo<Balance>, ClientError> {
		self.client
			.runtime_api()
			.eth_transact(block_hash, tx)
			.map_err(native_err)?
			.map_err(ClientError::TransactError)
	}

	async fn gas_price(&self, block_hash: SubstrateBlockHash) -> Result<U256, ClientError> {
		self.client.runtime_api().gas_price(block_hash).map_err(native_err)
	}

	async fn balance(
		&self,
		block_hash: SubstrateBlockHash,
		address: H160,
	) -> Result<U256, ClientError> {
		self.client.runtime_api().balance(block_hash, address).map_err(native_err)
	}

	async fn nonce(
		&self,
		block_hash: SubstrateBlockHash,
		address: H160,
	) -> Result<U256, ClientError> {
		self.client
			.runtime_api()
			.nonce(block_hash, address)
			.map(|n: u32| U256::from(n))
			.map_err(native_err)
	}

	async fn code(
		&self,
		block_hash: SubstrateBlockHash,
		address: H160,
	) -> Result<Vec<u8>, ClientError> {
		self.client.runtime_api().code(block_hash, address).map_err(native_err)
	}

	async fn get_storage(
		&self,
		block_hash: SubstrateBlockHash,
		address: H160,
		key: [u8; 32],
	) -> Result<Option<Vec<u8>>, ClientError> {
		self.client
			.runtime_api()
			.get_storage(block_hash, address, key)
			.map_err(native_err)?
			.map_err(|_| ClientError::ContractNotFound)
	}

	async fn eth_block(&self, block_hash: SubstrateBlockHash) -> Result<EthBlock, ClientError> {
		self.client.runtime_api().eth_block(block_hash).map_err(native_err)
	}

	async fn eth_block_hash(
		&self,
		block_hash: SubstrateBlockHash,
		number: U256,
	) -> Result<Option<H256>, ClientError> {
		self.client.runtime_api().eth_block_hash(block_hash, number).map_err(native_err)
	}

	async fn eth_receipt_data(
		&self,
		block_hash: SubstrateBlockHash,
	) -> Result<Vec<ReceiptGasInfo>, ClientError> {
		self.client.runtime_api().eth_receipt_data(block_hash).map_err(native_err)
	}

	async fn trace_block(
		&self,
		_block_hash: SubstrateBlockHash,
		block: OpaqueBlock,
		config: TracerType,
	) -> Result<Vec<(u32, Trace)>, ClientError> {
		let parent = *block.header().parent_hash();
		let block_generic: Block = Block::decode(&mut &block.encode()[..]).map_err(native_err)?;
		self.client
			.runtime_api()
			.trace_block(parent, block_generic, config)
			.map_err(native_err)
	}

	async fn trace_tx(
		&self,
		_block_hash: SubstrateBlockHash,
		block: OpaqueBlock,
		transaction_index: u32,
		config: TracerType,
	) -> Result<Trace, ClientError> {
		let parent = *block.header().parent_hash();
		let block_generic: Block = Block::decode(&mut &block.encode()[..]).map_err(native_err)?;
		self.client
			.runtime_api()
			.trace_tx(parent, block_generic, transaction_index, config)
			.map_err(native_err)?
			.ok_or(ClientError::EthExtrinsicNotFound)
	}

	async fn trace_call(
		&self,
		block_hash: SubstrateBlockHash,
		transaction: GenericTransaction,
		config: TracerType,
	) -> Result<Trace, ClientError> {
		self.client
			.runtime_api()
			.trace_call(block_hash, transaction, config)
			.map_err(native_err)?
			.map_err(ClientError::TransactError)
	}

	async fn submit_extrinsic(&self, payload: Vec<u8>) -> Result<SubmitResult, ClientError> {
		let opaque_xt = OpaqueExtrinsic::try_from_encoded_extrinsic(&payload)
			.map_err(|_| ClientError::TxDecodingFailed)?;

		let at = self.client.info().best_hash;

		self.pool
			.submit_one(at, sc_transaction_pool_api::TransactionSource::External, opaque_xt)
			.await
			.map(|_| sc_transaction_pool_api::TransactionStatus::Ready)
			.map_err(native_err)
	}

	async fn sync_state(
		&self,
	) -> Result<sc_rpc::system::SyncState<SubstrateBlockNumber>, ClientError> {
		let info = self.client.info();
		Ok(sc_rpc::system::SyncState {
			starting_block: 0,
			current_block: info.best_number,
			highest_block: info.best_number,
		})
	}

	async fn system_health(&self) -> Result<NodeHealth, ClientError> {
		Ok(NodeHealth { peers: 0, is_syncing: false, should_have_peers: false })
	}

	async fn get_automine(&self) -> bool {
		self.automine
	}

	async fn subscribe_blocks<F, Fut>(
		&self,
		subscription_type: SubscriptionType,
		callback: F,
	) -> Result<(), ClientError>
	where
		F: Fn(NativeCachedBlock) -> Fut + Send + Sync,
		Fut: Future<Output = Result<(), ClientError>> + Send,
	{
		log::debug!(
			target: crate::LOG_TARGET,
			"NativeSubstrateClient::subscribe_blocks ({subscription_type:?}): \
			 native notification stream active"
		);

		match subscription_type {
			SubscriptionType::BestBlocks => {
				let mut stream = self.client.import_notification_stream();
				while let Some(notification) = stream.next().await {
					if !notification.is_new_best {
						continue;
					}

					let hash = notification.hash;
					let block_info = match self.block_info_from_hash(hash) {
						Ok(Some(b)) => b,
						Ok(None) => {
							log::warn!(
								target: crate::LOG_TARGET,
								"NativeSubstrateClient: best-block notification for unknown hash {:?}",
								hash
							);
							continue;
						},
						Err(err) => {
							log::error!(
								target: crate::LOG_TARGET,
								"NativeSubstrateClient: failed to fetch best-block info: {err:?}"
							);
							continue;
						},
					};

					log::trace!(
						target: crate::LOG_TARGET,
						"NativeSubstrateClient: new best block #{} ({:?})",
						block_info.number,
						block_info.hash
					);

					if let Err(err) = callback(block_info).await {
						log::error!(
							target: crate::LOG_TARGET,
							"NativeSubstrateClient: best-block callback error: {err:?}"
						);
					}
				}
			},

			SubscriptionType::FinalizedBlocks => {
				let mut stream = self.client.finality_notification_stream();
				while let Some(notification) = stream.next().await {
					let hash = notification.hash;
					let block_info = match self.block_info_from_hash(hash) {
						Ok(Some(b)) => b,
						Ok(None) => {
							log::warn!(
								target: crate::LOG_TARGET,
								"NativeSubstrateClient: finalized-block notification for unknown hash {:?}",
								hash
							);
							continue;
						},
						Err(err) => {
							log::error!(
								target: crate::LOG_TARGET,
								"NativeSubstrateClient: failed to fetch finalized-block info: {err:?}"
							);
							continue;
						},
					};

					log::trace!(
						target: crate::LOG_TARGET,
						"NativeSubstrateClient: finalized block #{} ({:?})",
						block_info.number,
						block_info.hash
					);

					if let Err(err) = callback(block_info).await {
						log::error!(
							target: crate::LOG_TARGET,
							"NativeSubstrateClient: finalized-block callback error: {err:?}"
						);
					}
				}
			},
		}
		Ok(())
	}

	async fn signed_block(
		&self,
		block_hash: SubstrateBlockHash,
	) -> Result<OpaqueBlock, ClientError> {
		let signed = self
			.client
			.block(block_hash)
			.map_err(native_err)?
			.ok_or(ClientError::BlockNotFound)?;

		let encoded = signed.block.encode();
		OpaqueBlock::decode(&mut &encoded[..]).map_err(native_err)
	}

	async fn block_extrinsics(
		&self,
		block_hash: SubstrateBlockHash,
	) -> Result<Vec<RawExtrinsic>, ClientError> {
		let body = self
			.client
			.block_body(block_hash)
			.map_err(native_err)?
			.ok_or(ClientError::BlockNotFound)?;

		Ok(body
			.into_iter()
			.enumerate()
			.map(|(index, ext)| RawExtrinsic { payload: ext.encode(), index })
			.collect())
	}
}
