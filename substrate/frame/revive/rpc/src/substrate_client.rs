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

//! Native Substrate client implementation for the ETH RPC server.
//!
//! This module provides a [`SubstrateClientT`] trait that abstracts over the
//! Substrate node client, allowing the ETH RPC server to be integrated directly
//! into a parachain node (e.g. Asset Hub) without requiring a separate `subxt`
//! connection.
use crate::{
	client::{Balance, SubscriptionType, SubstrateBlock, SubstrateBlockHash, SubstrateBlockNumber},
	subxt_client::SrcChainConfig,
};
use futures::TryStreamExt;
use jsonrpsee::core::async_trait;
use pallet_revive::{
	EthTransactInfo,
	evm::{
		Block as EthBlock, BlockNumberOrTagOrHash, GenericTransaction, ReceiptGasInfo, Trace,
		TracerType, U256,
	},
};
use sp_core::H256;
use sp_weights::Weight;
use std::{future::Future, sync::Arc};
use subxt::{
	OnlineClient,
	backend::{
		StreamOf, StreamOfResults,
		legacy::{LegacyRpcMethods, rpc_methods::TransactionStatus},
		rpc::RpcClient,
	},
	ext::subxt_rpcs::rpc_params,
};

/// A raw extrinsic payload with its index in the block.
#[derive(Clone, Debug)]
pub struct RawExtrinsic {
	/// The SCALE-encoded extrinsic bytes.
	pub payload: Vec<u8>,
	/// The extrinsic index within the block.
	pub index: usize,
}

#[derive(Clone, Debug)]
pub struct NodeHealth {
	pub peers: usize,
	pub is_syncing: bool,
	pub should_have_peers: bool,
}

impl From<subxt::backend::legacy::rpc_methods::SystemHealth> for NodeHealth {
	fn from(h: subxt::backend::legacy::rpc_methods::SystemHealth) -> Self {
		Self { peers: h.peers, is_syncing: h.is_syncing, should_have_peers: h.should_have_peers }
	}
}

#[derive(Clone, Debug, PartialEq)]
pub enum SubmitResult {
	Ready,
	Future,
	InBlock(SubstrateBlockHash),
	Finalized(SubstrateBlockHash),
	Invalid,
	Dropped,
	Usurped,
}

impl From<TransactionStatus<SubstrateBlockHash>> for SubmitResult {
	fn from(status: TransactionStatus<SubstrateBlockHash>) -> Self {
		match status {
			TransactionStatus::Ready => SubmitResult::Ready,
			TransactionStatus::Future => SubmitResult::Future,
			TransactionStatus::InBlock(h) => SubmitResult::InBlock(h),
			TransactionStatus::Finalized(h) => SubmitResult::Finalized(h),
			TransactionStatus::Invalid => SubmitResult::Invalid,
			TransactionStatus::Dropped => SubmitResult::Dropped,
			TransactionStatus::Usurped(_) => SubmitResult::Usurped,
			_ => SubmitResult::Ready,
		}
	}
}

/// The core trait that the ETH RPC server requires from the underlying Substrate node.
#[async_trait]
pub trait SubstrateClientT: Send + Sync + Clone + 'static {
	/// Return the EVM chain-id constant.
	fn chain_id(&self) -> u64;

	/// Return the maximum block weight (used to compute the block gas limit).
	fn max_block_weight(&self) -> Weight;

	/// Fetch a block by its Substrate hash.
	async fn block_by_hash(
		&self,
		hash: &SubstrateBlockHash,
	) -> Result<Option<Arc<SubstrateBlock>>, crate::client::ClientError>;

	/// Fetch a block by its block number.
	async fn block_by_number(
		&self,
		number: SubstrateBlockNumber,
	) -> Result<Option<Arc<SubstrateBlock>>, crate::client::ClientError>;

	/// Return the latest best block (cached).
	async fn latest_block(&self) -> Arc<SubstrateBlock>;

	/// Return the latest finalized block (cached).
	async fn latest_finalized_block(&self) -> Arc<SubstrateBlock>;

	/// Dry-run a transaction at the given block.
	async fn dry_run(
		&self,
		block_hash: SubstrateBlockHash,
		tx: GenericTransaction,
		block: BlockNumberOrTagOrHash,
	) -> Result<EthTransactInfo<Balance>, crate::client::ClientError>;

	/// Return the current gas price.
	async fn gas_price(
		&self,
		block_hash: SubstrateBlockHash,
	) -> Result<U256, crate::client::ClientError>;

	/// Return the account balance.
	async fn balance(
		&self,
		block_hash: SubstrateBlockHash,
		address: sp_core::H160,
	) -> Result<U256, crate::client::ClientError>;

	/// Return the account nonce.
	async fn nonce(
		&self,
		block_hash: SubstrateBlockHash,
		address: sp_core::H160,
	) -> Result<U256, crate::client::ClientError>;

	/// Return the contract code.
	async fn code(
		&self,
		block_hash: SubstrateBlockHash,
		address: sp_core::H160,
	) -> Result<Vec<u8>, crate::client::ClientError>;

	/// Return contract storage at the given slot.
	async fn get_storage(
		&self,
		block_hash: SubstrateBlockHash,
		address: sp_core::H160,
		key: [u8; 32],
	) -> Result<Option<Vec<u8>>, crate::client::ClientError>;

	/// Return the Ethereum block representation for the given Substrate block.
	async fn eth_block(
		&self,
		block_hash: SubstrateBlockHash,
	) -> Result<EthBlock, crate::client::ClientError>;

	/// Return the Ethereum block hash for the given block number.
	async fn eth_block_hash(
		&self,
		block_hash: SubstrateBlockHash,
		number: U256,
	) -> Result<Option<H256>, crate::client::ClientError>;

	/// Return the per-transaction receipt gas info for the given block.
	async fn eth_receipt_data(
		&self,
		block_hash: SubstrateBlockHash,
	) -> Result<Vec<ReceiptGasInfo>, crate::client::ClientError>;

	/// Trace the given block.
	async fn trace_block(
		&self,
		block_hash: SubstrateBlockHash,
		block: sp_runtime::generic::Block<
			sp_runtime::generic::Header<u32, sp_runtime::traits::BlakeTwo256>,
			sp_runtime::OpaqueExtrinsic,
		>,
		config: TracerType,
	) -> Result<Vec<(u32, Trace)>, crate::client::ClientError>;

	/// Trace a single transaction within a block.
	async fn trace_tx(
		&self,
		block_hash: SubstrateBlockHash,
		block: sp_runtime::generic::Block<
			sp_runtime::generic::Header<u32, sp_runtime::traits::BlakeTwo256>,
			sp_runtime::OpaqueExtrinsic,
		>,
		transaction_index: u32,
		config: TracerType,
	) -> Result<Trace, crate::client::ClientError>;

	/// Trace a dry-run call.
	async fn trace_call(
		&self,
		block_hash: SubstrateBlockHash,
		transaction: GenericTransaction,
		config: TracerType,
	) -> Result<Trace, crate::client::ClientError>;

	/// Submit an unsigned `eth_transact` extrinsic.
	async fn submit_extrinsic(
		&self,
		payload: Vec<u8>,
	) -> Result<SubmitResult, crate::client::ClientError>;

	/// Return the node sync state.
	async fn sync_state(
		&self,
	) -> Result<sc_rpc::system::SyncState<SubstrateBlockNumber>, crate::client::ClientError>;

	/// Return the node health.
	async fn system_health(&self) -> Result<NodeHealth, crate::client::ClientError>;

	/// Return whether the node has automine enabled.
	async fn get_automine(&self) -> bool;

	/// Subscribe to new best or finalized blocks.
	async fn subscribe_blocks<F, Fut>(
		&self,
		subscription_type: SubscriptionType,
		callback: F,
	) -> Result<(), crate::client::ClientError>
	where
		F: Fn(SubstrateBlock) -> Fut + Send + Sync,
		Fut: Future<Output = Result<(), crate::client::ClientError>> + Send;

	/// Fetch an opaque signed block (used for tracing).
	async fn signed_block(
		&self,
		block_hash: SubstrateBlockHash,
	) -> Result<
		sp_runtime::generic::Block<
			sp_runtime::generic::Header<u32, sp_runtime::traits::BlakeTwo256>,
			sp_runtime::OpaqueExtrinsic,
		>,
		crate::client::ClientError,
	>;

	/// Return all raw extrinsics in the given block.
	async fn block_extrinsics(
		&self,
		block_hash: SubstrateBlockHash,
	) -> Result<Vec<RawExtrinsic>, crate::client::ClientError>;

	/// Scan block events for the given extrinsic and return its post-dispatch weight.
	async fn extrinsic_post_dispatch_weight(
		&self,
		_block_hash: SubstrateBlockHash,
		_extrinsic_index: usize,
	) -> Option<sp_weights::Weight> {
		None
	}

	/// Get the post dispatch weight for a given transaction hash.
	async fn post_dispatch_weight(
		&self,
		_tx_hash: &SubstrateBlockHash,
	) -> Option<sp_weights::Weight> {
		None
	}
}

/// Client backed by a `subxt` connection to a remote Substrate node.
#[derive(Clone)]
pub struct SubxtClient {
	pub(crate) api: OnlineClient<SrcChainConfig>,
	pub(crate) rpc_client: RpcClient,
	pub(crate) rpc: LegacyRpcMethods<SrcChainConfig>,
	pub(crate) chain_id: u64,
	pub(crate) max_block_weight: Weight,
}

impl SubxtClient {
	pub async fn new(
		api: OnlineClient<SrcChainConfig>,
		rpc_client: RpcClient,
		rpc: LegacyRpcMethods<SrcChainConfig>,
	) -> Result<Self, crate::client::ClientError> {
		let latest = api.blocks().at_latest().await?;
		let _runtime_api = api.runtime_api().at(latest.hash());

		let chain_id = {
			let query = crate::subxt_client::constants().revive().chain_id();
			api.constants().at(&query)?
		};

		let max_block_weight = {
			let query = crate::subxt_client::constants().system().block_weights();
			let weights = api.constants().at(&query)?;
			let max_block = weights.per_class.normal.max_extrinsic.unwrap_or(weights.max_block);
			max_block.0
		};

		Ok(Self { api, rpc_client, rpc, chain_id, max_block_weight })
	}
}

#[async_trait]
impl SubstrateClientT for SubxtClient {
	fn chain_id(&self) -> u64 {
		self.chain_id
	}

	fn max_block_weight(&self) -> Weight {
		self.max_block_weight
	}

	async fn block_by_hash(
		&self,
		hash: &SubstrateBlockHash,
	) -> Result<Option<Arc<SubstrateBlock>>, crate::client::ClientError> {
		match self.api.blocks().at(*hash).await {
			Ok(b) => Ok(Some(Arc::new(b))),
			Err(subxt::Error::Block(subxt::error::BlockError::NotFound(_))) => Ok(None),
			Err(e) => Err(e.into()),
		}
	}

	async fn block_by_number(
		&self,
		number: SubstrateBlockNumber,
	) -> Result<Option<Arc<SubstrateBlock>>, crate::client::ClientError> {
		let Some(hash) = self.rpc.chain_get_block_hash(Some(number.into())).await? else {
			return Ok(None);
		};
		self.block_by_hash(&hash).await
	}

	async fn latest_block(&self) -> Arc<SubstrateBlock> {
		Arc::new(self.api.blocks().at_latest().await.expect("latest block"))
	}

	async fn latest_finalized_block(&self) -> Arc<SubstrateBlock> {
		Arc::new(self.api.blocks().at_latest().await.expect("latest finalized block"))
	}

	async fn dry_run(
		&self,
		block_hash: SubstrateBlockHash,
		tx: GenericTransaction,
		block: BlockNumberOrTagOrHash,
	) -> Result<EthTransactInfo<Balance>, crate::client::ClientError> {
		use crate::client::runtime_api::RuntimeApi;
		let runtime_api = RuntimeApi::new(self.api.runtime_api().at(block_hash));
		runtime_api.dry_run(tx, block).await
	}

	async fn gas_price(
		&self,
		block_hash: SubstrateBlockHash,
	) -> Result<U256, crate::client::ClientError> {
		use crate::client::runtime_api::RuntimeApi;
		RuntimeApi::new(self.api.runtime_api().at(block_hash)).gas_price().await
	}

	async fn balance(
		&self,
		block_hash: SubstrateBlockHash,
		address: sp_core::H160,
	) -> Result<U256, crate::client::ClientError> {
		use crate::client::runtime_api::RuntimeApi;
		RuntimeApi::new(self.api.runtime_api().at(block_hash)).balance(address).await
	}

	async fn nonce(
		&self,
		block_hash: SubstrateBlockHash,
		address: sp_core::H160,
	) -> Result<U256, crate::client::ClientError> {
		use crate::client::runtime_api::RuntimeApi;
		RuntimeApi::new(self.api.runtime_api().at(block_hash)).nonce(address).await
	}

	async fn code(
		&self,
		block_hash: SubstrateBlockHash,
		address: sp_core::H160,
	) -> Result<Vec<u8>, crate::client::ClientError> {
		use crate::client::runtime_api::RuntimeApi;
		RuntimeApi::new(self.api.runtime_api().at(block_hash)).code(address).await
	}

	async fn get_storage(
		&self,
		block_hash: SubstrateBlockHash,
		address: sp_core::H160,
		key: [u8; 32],
	) -> Result<Option<Vec<u8>>, crate::client::ClientError> {
		use crate::client::runtime_api::RuntimeApi;
		RuntimeApi::new(self.api.runtime_api().at(block_hash))
			.get_storage(address, key)
			.await
	}

	async fn eth_block(
		&self,
		block_hash: SubstrateBlockHash,
	) -> Result<EthBlock, crate::client::ClientError> {
		use crate::client::runtime_api::RuntimeApi;
		RuntimeApi::new(self.api.runtime_api().at(block_hash)).eth_block().await
	}

	async fn eth_block_hash(
		&self,
		block_hash: SubstrateBlockHash,
		number: U256,
	) -> Result<Option<H256>, crate::client::ClientError> {
		use crate::client::runtime_api::RuntimeApi;
		RuntimeApi::new(self.api.runtime_api().at(block_hash))
			.eth_block_hash(number)
			.await
	}

	async fn eth_receipt_data(
		&self,
		block_hash: SubstrateBlockHash,
	) -> Result<Vec<ReceiptGasInfo>, crate::client::ClientError> {
		use crate::client::runtime_api::RuntimeApi;
		RuntimeApi::new(self.api.runtime_api().at(block_hash)).eth_receipt_data().await
	}

	async fn trace_block(
		&self,
		_block_hash: SubstrateBlockHash,
		block: sp_runtime::generic::Block<
			sp_runtime::generic::Header<u32, sp_runtime::traits::BlakeTwo256>,
			sp_runtime::OpaqueExtrinsic,
		>,
		config: TracerType,
	) -> Result<Vec<(u32, Trace)>, crate::client::ClientError> {
		use crate::client::runtime_api::RuntimeApi;
		let parent = block.header.parent_hash;
		RuntimeApi::new(self.api.runtime_api().at(parent))
			.trace_block(block, config)
			.await
	}

	async fn trace_tx(
		&self,
		_block_hash: SubstrateBlockHash,
		block: sp_runtime::generic::Block<
			sp_runtime::generic::Header<u32, sp_runtime::traits::BlakeTwo256>,
			sp_runtime::OpaqueExtrinsic,
		>,
		transaction_index: u32,
		config: TracerType,
	) -> Result<Trace, crate::client::ClientError> {
		use crate::client::runtime_api::RuntimeApi;
		let parent = block.header.parent_hash;
		RuntimeApi::new(self.api.runtime_api().at(parent))
			.trace_tx(block, transaction_index, config)
			.await
	}

	async fn trace_call(
		&self,
		block_hash: SubstrateBlockHash,
		transaction: GenericTransaction,
		config: TracerType,
	) -> Result<Trace, crate::client::ClientError> {
		use crate::client::runtime_api::RuntimeApi;
		RuntimeApi::new(self.api.runtime_api().at(block_hash))
			.trace_call(transaction, config)
			.await
	}

	async fn submit_extrinsic(
		&self,
		payload: Vec<u8>,
	) -> Result<SubmitResult, crate::client::ClientError> {
		use crate::subxt_client::tx;
		let call = tx().revive().eth_transact(payload);
		let ext = self.api.tx().create_unsigned(&call).map_err(crate::client::ClientError::from)?;

		let sub = self
			.rpc_client
			.subscribe(
				"author_submitAndWatchExtrinsic",
				rpc_params![to_hex(ext.encoded())],
				"author_unwatchExtrinsic",
			)
			.await?;

		let mut stream: StreamOfResults<TransactionStatus<SubstrateBlockHash>> =
			StreamOf::new(Box::pin(sub.map_err(|e| e.into())));

		tokio::time::timeout(std::time::Duration::from_secs(5), async {
			if let Some(status) = stream.next().await {
				match status {
					Ok(
						tx @ (TransactionStatus::Future |
						TransactionStatus::Ready |
						TransactionStatus::Broadcast(_) |
						TransactionStatus::InBlock(_) |
						TransactionStatus::FinalityTimeout(_) |
						TransactionStatus::Retracted(_) |
						TransactionStatus::Finalized(_)),
					) => return Ok(tx.into()),
					Ok(
						tx @ (TransactionStatus::Usurped(_) |
						TransactionStatus::Dropped |
						TransactionStatus::Invalid),
					) => {
						return Err(crate::client::ClientError::SubmitError(
							crate::client::SubmitError::from(tx),
						));
					},
					Err(e) => return Err(crate::client::ClientError::from(e)),
				}
			}
			Err(crate::client::ClientError::SubmitError(crate::client::SubmitError::StreamEnded))
		})
		.await
		.map_err(|_| crate::client::ClientError::Timeout)?
	}

	async fn sync_state(
		&self,
	) -> Result<sc_rpc::system::SyncState<SubstrateBlockNumber>, crate::client::ClientError> {
		let sync_state: sc_rpc::system::SyncState<SubstrateBlockNumber> =
			self.rpc_client.request("system_syncState", Default::default()).await?;
		Ok(sync_state)
	}

	async fn system_health(&self) -> Result<NodeHealth, crate::client::ClientError> {
		Ok(self.rpc.system_health().await?.into())
	}

	async fn get_automine(&self) -> bool {
		match self.rpc_client.request::<bool>("getAutomine", rpc_params![]).await {
			Ok(v) => v,
			Err(_) => false,
		}
	}

	async fn subscribe_blocks<F, Fut>(
		&self,
		subscription_type: SubscriptionType,
		callback: F,
	) -> Result<(), crate::client::ClientError>
	where
		F: Fn(SubstrateBlock) -> Fut + Send + Sync,
		Fut: Future<Output = Result<(), crate::client::ClientError>> + Send,
	{
		let mut block_stream = match subscription_type {
			SubscriptionType::BestBlocks => self.api.blocks().subscribe_best().await,
			SubscriptionType::FinalizedBlocks => self.api.blocks().subscribe_finalized().await,
		}?;

		while let Some(block) = block_stream.next().await {
			let block = match block {
				Ok(b) => b,
				Err(err) => {
					if err.is_disconnected_will_reconnect() {
						log::warn!(
							target: crate::LOG_TARGET,
							"RPC connection lost ({subscription_type:?}): {err:?}"
						);
						continue;
					}
					return Err(err.into());
				},
			};
			if let Err(err) = callback(block).await {
				log::error!(
					target: crate::LOG_TARGET,
					"block callback failed ({subscription_type:?}): {err:?}"
				);
			}
		}
		Ok(())
	}

	async fn signed_block(
		&self,
		block_hash: SubstrateBlockHash,
	) -> Result<
		sp_runtime::generic::Block<
			sp_runtime::generic::Header<u32, sp_runtime::traits::BlakeTwo256>,
			sp_runtime::OpaqueExtrinsic,
		>,
		crate::client::ClientError,
	> {
		let signed: sp_runtime::generic::SignedBlock<
			sp_runtime::generic::Block<
				sp_runtime::generic::Header<u32, sp_runtime::traits::BlakeTwo256>,
				sp_runtime::OpaqueExtrinsic,
			>,
		> = self.rpc_client.request("chain_getBlock", rpc_params![block_hash]).await?;
		Ok(signed.block)
	}

	async fn block_extrinsics(
		&self,
		block_hash: SubstrateBlockHash,
	) -> Result<Vec<RawExtrinsic>, crate::client::ClientError> {
		let block = self.api.blocks().at(block_hash).await?;
		let extrinsics = block.extrinsics().await?;
		Ok(extrinsics
			.iter()
			.enumerate()
			.map(|(index, ext)| RawExtrinsic { payload: ext.bytes().to_vec(), index })
			.collect())
	}

	async fn extrinsic_post_dispatch_weight(
		&self,
		block_hash: SubstrateBlockHash,
		extrinsic_index: usize,
	) -> Option<sp_weights::Weight> {
		use crate::subxt_client::system::events::ExtrinsicSuccess;
		let block = self.api.blocks().at(block_hash).await.ok()?;
		let ext = block.extrinsics().await.ok()?.iter().nth(extrinsic_index)?;
		let event = ext.events().await.ok()?.find_first::<ExtrinsicSuccess>().ok()??;
		Some(event.dispatch_info.weight.0)
	}
}

fn to_hex(bytes: impl AsRef<[u8]>) -> String {
	format!("0x{}", hex::encode(bytes.as_ref()))
}
