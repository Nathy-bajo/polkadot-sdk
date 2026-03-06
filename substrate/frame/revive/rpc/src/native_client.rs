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
	/// Create a new native client.
	pub fn new(client: Arc<Client>, pool: Arc<Pool>, chain_id: u64) -> Result<Self, ClientError> {
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

	async fn block_by_hash(
		&self,
		_hash: &SubstrateBlockHash,
	) -> Result<Option<Arc<SubstrateBlock>>, ClientError> {
		// Cannot return a subxt `SubstrateBlock` without a live `OnlineClient`.
		// Return `None` so the `SubxtBlockInfoProvider` fallback handles cache misses.
		Ok(None)
	}

	async fn block_by_number(
		&self,
		_number: SubstrateBlockNumber,
	) -> Result<Option<Arc<SubstrateBlock>>, ClientError> {
		Ok(None)
	}

	async fn latest_block(&self) -> Arc<SubstrateBlock> {
		panic!(
			"NativeSubstrateClient::latest_block must not be called directly; \
			 use SubxtBlockInfoProvider"
		)
	}

	async fn latest_finalized_block(&self) -> Arc<SubstrateBlock> {
		panic!(
			"NativeSubstrateClient::latest_finalized_block must not be called directly; \
			 use SubxtBlockInfoProvider"
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

	async fn subscribe_blocks<F, Fut>(
		&self,
		_subscription_type: SubscriptionType,
		_callback: F,
	) -> Result<(), ClientError>
	where
		F: Fn(SubstrateBlock) -> Fut + Send + Sync,
		Fut: Future<Output = Result<(), ClientError>> + Send,
	{
		// TODO: implement a proper native block notification path. For now callers
		// that need block subscriptions in the embedded path should use the
		// SubxtClient-backed block provider.
		log::warn!(
			target: crate::LOG_TARGET,
			"NativeSubstrateClient: block subscription not yet implemented for in-process path"
		);
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
		_block_hash: SubstrateBlockHash,
	) -> Result<
		Vec<subxt::blocks::ExtrinsicDetails<SrcChainConfig, OnlineClient<SrcChainConfig>>>,
		ClientError,
	> {
		// The subxt `ExtrinsicDetails` wrapper is not constructible without an
		// `OnlineClient`. A future refactor should introduce a native receipt
		// extractor that works with `sc_client_api` directly.
		Err(ClientError::SubxtError(subxt::Error::Other(
			"NativeSubstrateClient: ExtrinsicDetails not available in native path; \
			 use SubxtClient for receipt extraction"
				.into(),
		)))
	}
}
