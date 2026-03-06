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
	client::{Balance, SubscriptionType, SubstrateBlock, SubstrateBlockHash, SubstrateBlockNumber},
	subxt_client::SrcChainConfig,
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
use subxt::{OnlineClient, backend::legacy::rpc_methods::TransactionStatus};

/// Convenience alias for the pallet-revive runtime API trait objects.
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
	online_client: Option<OnlineClient<SrcChainConfig>>,
	_block: PhantomData<Block>,
	_moment: PhantomData<Moment>,
}

impl<Client, Pool, Block, Moment> NativeSubstrateClient<Client, Pool, Block, Moment>
where
	Block: BlockT<Hash = H256>,
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
	/// Create a new native client without a subxt `OnlineClient`.
	pub fn new(client: Arc<Client>, pool: Arc<Pool>, chain_id: u64) -> Result<Self, ClientError> {
		Self::new_with_online_client(client, pool, chain_id, None)
	}

	/// Create a new native client with an optional subxt `OnlineClient`.
	pub fn new_with_online_client(
		client: Arc<Client>,
		pool: Arc<Pool>,
		chain_id: u64,
		online_client: Option<OnlineClient<SrcChainConfig>>,
	) -> Result<Self, ClientError> {
		let best_hash = client.info().best_hash;
		let runtime_api = client.runtime_api();

		// `block_gas_limit` returns `U256`; we clamp to `u64::MAX`.
		let block_gas_limit: u64 = runtime_api
			.block_gas_limit(best_hash)
			.map(|v: U256| v.min(U256::from(u64::MAX)).as_u64())
			.unwrap_or(u64::MAX);

		Ok(Self {
			client,
			pool,
			chain_id,
			// Use the EVM block gas limit as the ref_time upper bound.
			// Set proof_size to u64::MAX so we never artificially limit PoV.
			max_block_weight: Weight::from_parts(block_gas_limit, u64::MAX),
			online_client,
			_block: PhantomData,
			_moment: PhantomData,
		})
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
	fn chain_id(&self) -> u64 {
		self.chain_id
	}

	fn max_block_weight(&self) -> Weight {
		self.max_block_weight
	}

	/// Fetch a block by its Substrate hash.
	async fn block_by_hash(
		&self,
		hash: &SubstrateBlockHash,
	) -> Result<Option<Arc<SubstrateBlock>>, ClientError> {
		let Some(ref api) = self.online_client else {
			return Ok(None);
		};

		// Confirm the block exists in the native backend before hitting subxt.
		match self.client.block(*hash) {
			Ok(None) | Err(_) => return Ok(None),
			Ok(Some(_)) => {},
		}

		match api.blocks().at(*hash).await {
			Ok(b) => Ok(Some(Arc::new(b))),
			Err(subxt::Error::Block(subxt::error::BlockError::NotFound(_))) => Ok(None),
			Err(e) => Err(ClientError::SubxtError(e)),
		}
	}

	/// Fetch a block by its block number.
	async fn block_by_number(
		&self,
		number: SubstrateBlockNumber,
	) -> Result<Option<Arc<SubstrateBlock>>, ClientError> {
		// Resolve block number → hash using the native BlockBackend.
		let hash = self
			.client
			.block_hash(number.into())
			.map_err(|e| ClientError::SubxtError(subxt::Error::Other(e.to_string())))?;

		match hash {
			Some(h) => self.block_by_hash(&h).await,
			None => Ok(None),
		}
	}

	/// Return the latest best block.
	async fn latest_block(&self) -> Arc<SubstrateBlock> {
		let best_hash = self.client.info().best_hash;
		self.block_by_hash(&best_hash)
			.await
			.expect("latest_block: block_by_hash failed")
			.expect(
				"latest_block: best block not found — \
				 supply an OnlineClient or use SubxtBlockInfoProvider",
			)
	}

	/// Return the latest finalized block.
	async fn latest_finalized_block(&self) -> Arc<SubstrateBlock> {
		let finalized_hash = self.client.info().finalized_hash;
		self.block_by_hash(&finalized_hash)
			.await
			.expect("latest_finalized_block: block_by_hash failed")
			.expect(
				"latest_finalized_block: finalized block not found — \
				 supply an OnlineClient or use SubxtBlockInfoProvider",
			)
	}

	async fn dry_run(
		&self,
		block_hash: SubstrateBlockHash,
		tx: GenericTransaction,
		_block: BlockNumberOrTagOrHash,
	) -> Result<EthTransactInfo<Balance>, ClientError> {
		let api = self.client.runtime_api();
		let result = api
			.eth_transact(block_hash, tx)
			.map_err(|e| ClientError::SubxtError(subxt::Error::Other(e.to_string())))?;
		result.map_err(ClientError::TransactError)
	}

	async fn gas_price(&self, block_hash: SubstrateBlockHash) -> Result<U256, ClientError> {
		let api = self.client.runtime_api();
		api.gas_price(block_hash)
			.map_err(|e| ClientError::SubxtError(subxt::Error::Other(e.to_string())))
	}

	async fn balance(
		&self,
		block_hash: SubstrateBlockHash,
		address: H160,
	) -> Result<U256, ClientError> {
		let api = self.client.runtime_api();
		api.balance(block_hash, address)
			.map_err(|e| ClientError::SubxtError(subxt::Error::Other(e.to_string())))
	}

	async fn nonce(
		&self,
		block_hash: SubstrateBlockHash,
		address: H160,
	) -> Result<U256, ClientError> {
		let api = self.client.runtime_api();
		api.nonce(block_hash, address)
			.map(|n: u32| U256::from(n))
			.map_err(|e| ClientError::SubxtError(subxt::Error::Other(e.to_string())))
	}

	async fn code(
		&self,
		block_hash: SubstrateBlockHash,
		address: H160,
	) -> Result<Vec<u8>, ClientError> {
		let api = self.client.runtime_api();
		api.code(block_hash, address)
			.map_err(|e| ClientError::SubxtError(subxt::Error::Other(e.to_string())))
	}

	async fn get_storage(
		&self,
		block_hash: SubstrateBlockHash,
		address: H160,
		key: [u8; 32],
	) -> Result<Option<Vec<u8>>, ClientError> {
		let api = self.client.runtime_api();
		api.get_storage(block_hash, address, key)
			.map_err(|e| ClientError::SubxtError(subxt::Error::Other(e.to_string())))?
			.map_err(|_| ClientError::ContractNotFound)
	}

	async fn eth_block(&self, block_hash: SubstrateBlockHash) -> Result<EthBlock, ClientError> {
		let api = self.client.runtime_api();
		api.eth_block(block_hash)
			.map_err(|e| ClientError::SubxtError(subxt::Error::Other(e.to_string())))
	}

	async fn eth_block_hash(
		&self,
		block_hash: SubstrateBlockHash,
		number: U256,
	) -> Result<Option<H256>, ClientError> {
		let api = self.client.runtime_api();
		api.eth_block_hash(block_hash, number)
			.map_err(|e| ClientError::SubxtError(subxt::Error::Other(e.to_string())))
	}

	async fn eth_receipt_data(
		&self,
		block_hash: SubstrateBlockHash,
	) -> Result<Vec<ReceiptGasInfo>, ClientError> {
		let api = self.client.runtime_api();
		api.eth_receipt_data(block_hash)
			.map_err(|e| ClientError::SubxtError(subxt::Error::Other(e.to_string())))
	}

	async fn trace_block(
		&self,
		_block_hash: SubstrateBlockHash,
		block: sp_runtime::generic::Block<
			sp_runtime::generic::Header<u32, sp_runtime::traits::BlakeTwo256>,
			OpaqueExtrinsic,
		>,
		config: TracerType,
	) -> Result<Vec<(u32, Trace)>, ClientError> {
		let parent = *block.header().parent_hash();
		let block_generic: Block = Block::decode(&mut &block.encode()[..])
			.map_err(|e| ClientError::SubxtError(subxt::Error::Other(e.to_string())))?;

		let api = self.client.runtime_api();
		api.trace_block(parent, block_generic, config)
			.map_err(|e| ClientError::SubxtError(subxt::Error::Other(e.to_string())))
	}

	async fn trace_tx(
		&self,
		_block_hash: SubstrateBlockHash,
		block: sp_runtime::generic::Block<
			sp_runtime::generic::Header<u32, sp_runtime::traits::BlakeTwo256>,
			OpaqueExtrinsic,
		>,
		transaction_index: u32,
		config: TracerType,
	) -> Result<Trace, ClientError> {
		let parent = *block.header().parent_hash();
		let block_generic: Block = Block::decode(&mut &block.encode()[..])
			.map_err(|e| ClientError::SubxtError(subxt::Error::Other(e.to_string())))?;

		let api = self.client.runtime_api();
		api.trace_tx(parent, block_generic, transaction_index, config)
			.map_err(|e| ClientError::SubxtError(subxt::Error::Other(e.to_string())))?
			.ok_or(ClientError::EthExtrinsicNotFound)
	}

	async fn trace_call(
		&self,
		block_hash: SubstrateBlockHash,
		transaction: GenericTransaction,
		config: TracerType,
	) -> Result<Trace, ClientError> {
		let api = self.client.runtime_api();
		api.trace_call(block_hash, transaction, config)
			.map_err(|e| ClientError::SubxtError(subxt::Error::Other(e.to_string())))?
			.map_err(ClientError::TransactError)
	}

	async fn submit_extrinsic(
		&self,
		payload: Vec<u8>,
	) -> Result<TransactionStatus<SubstrateBlockHash>, ClientError> {
		let opaque_xt = OpaqueExtrinsic::try_from_encoded_extrinsic(&payload)
			.map_err(|_| ClientError::TxDecodingFailed)?;

		let at = self.client.info().best_hash;

		self.pool
			.submit_one(at, sc_transaction_pool_api::TransactionSource::External, opaque_xt)
			.await
			.map(|_| TransactionStatus::Ready)
			.map_err(|e| ClientError::SubxtError(subxt::Error::Other(e.to_string())))
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

	async fn system_health(
		&self,
	) -> Result<subxt::backend::legacy::rpc_methods::SystemHealth, ClientError> {
		Ok(subxt::backend::legacy::rpc_methods::SystemHealth {
			peers: 0,
			is_syncing: false,
			should_have_peers: false,
		})
	}

	async fn get_automine(&self) -> bool {
		false
	}

	/// Subscribe to new best or finalized blocks.
	async fn subscribe_blocks<F, Fut>(
		&self,
		subscription_type: SubscriptionType,
		callback: F,
	) -> Result<(), ClientError>
	where
		F: Fn(SubstrateBlock) -> Fut + Send + Sync,
		Fut: Future<Output = Result<(), ClientError>> + Send,
	{
		let online_client = self.online_client.clone();

		match subscription_type {
			SubscriptionType::BestBlocks => {
				let mut stream = self.client.import_notification_stream();
				while let Some(notification) = stream.next().await {
					// Only process notifications for the current best chain.
					if !notification.is_new_best {
						continue;
					}
					let hash = notification.hash;
					let block = Self::fetch_subxt_block(&online_client, hash).await?;
					if let Some(b) = block {
						if let Err(err) = callback(b).await {
							log::error!(
								target: crate::LOG_TARGET,
								"best-block callback failed for {hash:?}: {err:?}"
							);
						}
					}
				}
			},
			SubscriptionType::FinalizedBlocks => {
				let mut stream = self.client.finality_notification_stream();
				while let Some(notification) = stream.next().await {
					let hash = notification.hash;
					let block = Self::fetch_subxt_block(&online_client, hash).await?;
					if let Some(b) = block {
						if let Err(err) = callback(b).await {
							log::error!(
								target: crate::LOG_TARGET,
								"finalized-block callback failed for {hash:?}: {err:?}"
							);
						}
					}
				}
			},
		}

		Ok(())
	}

	async fn signed_block(
		&self,
		block_hash: SubstrateBlockHash,
	) -> Result<
		sp_runtime::generic::Block<
			sp_runtime::generic::Header<u32, sp_runtime::traits::BlakeTwo256>,
			OpaqueExtrinsic,
		>,
		ClientError,
	> {
		let signed = self
			.client
			.block(block_hash)
			.map_err(|e| ClientError::SubxtError(subxt::Error::Other(e.to_string())))?
			.ok_or(ClientError::BlockNotFound)?;

		let encoded = signed.block.encode();
		sp_runtime::generic::Block::<
			sp_runtime::generic::Header<u32, sp_runtime::traits::BlakeTwo256>,
			OpaqueExtrinsic,
		>::decode(&mut &encoded[..])
		.map_err(|e| ClientError::SubxtError(subxt::Error::Other(e.to_string())))
	}

	async fn block_extrinsics(
		&self,
		block_hash: SubstrateBlockHash,
	) -> Result<
		Vec<subxt::blocks::ExtrinsicDetails<SrcChainConfig, OnlineClient<SrcChainConfig>>>,
		ClientError,
	> {
		match self.client.block_body(block_hash) {
			Err(e) => {
				return Err(ClientError::SubxtError(subxt::Error::Other(format!(
					"block_body({block_hash:?}) failed: {e}"
				))));
			},
			Ok(None) => return Err(ClientError::BlockNotFound),
			Ok(Some(_)) => {},
		}

		let Some(ref api) = self.online_client else {
			return Err(ClientError::SubxtError(subxt::Error::Other(
				"NativeSubstrateClient: no OnlineClient available for ExtrinsicDetails \
				 construction; supply one via new_with_online_client or use SubxtClient \
				 for receipt extraction"
					.into(),
			)));
		};

		let block = api.blocks().at(block_hash).await.map_err(ClientError::SubxtError)?;
		let extrinsics = block.extrinsics().await.map_err(ClientError::SubxtError)?;

		Ok(extrinsics.iter().collect())
	}
}

impl<Client, Pool, Block, Moment> NativeSubstrateClient<Client, Pool, Block, Moment>
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
	/// Attempt to fetch a `SubstrateBlock` from the subxt `OnlineClient`.
	///
	/// Returns `Ok(None)` when no client is available or the block is not found,
	/// allowing callers to decide whether to skip or error.
	async fn fetch_subxt_block(
		online_client: &Option<OnlineClient<SrcChainConfig>>,
		hash: H256,
	) -> Result<Option<SubstrateBlock>, ClientError> {
		let Some(api) = online_client else {
			log::debug!(
				target: crate::LOG_TARGET,
				"NativeSubstrateClient: no OnlineClient available, skipping block fetch for {hash:?}"
			);
			return Ok(None);
		};

		match api.blocks().at(hash).await {
			Ok(b) => Ok(Some(b)),
			Err(subxt::Error::Block(subxt::error::BlockError::NotFound(_))) => Ok(None),
			Err(e) => Err(ClientError::SubxtError(e)),
		}
	}
}
