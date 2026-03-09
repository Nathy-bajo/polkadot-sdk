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
//! The generated subxt client.
//! Generated against a substrate chain configured with [`pallet_revive`] using:
//! subxt metadata  --url ws://localhost:9944 -o rpc/revive_chain.scale
pub use subxt::config::PolkadotConfig as SrcChainConfig;

#[subxt::subxt(
	runtime_metadata_path = "$OUT_DIR/revive_chain.scale",
	// TODO remove once subxt uses the same U256 type
	substitute_type(
		path = "primitive_types::U256",
		with = "::subxt::utils::Static<::sp_core::U256>"
	),

	substitute_type(
		path = "sp_runtime::generic::block::Block<A, B, C, D, E>",
		with = "::subxt::utils::Static<::sp_runtime::generic::Block<
		::sp_runtime::generic::Header<u32, sp_runtime::traits::BlakeTwo256>,
		::sp_runtime::OpaqueExtrinsic
		>>"
	),
	substitute_type(
		path = "pallet_revive::evm::api::debug_rpc_types::Trace",
		with = "::subxt::utils::Static<::pallet_revive::evm::Trace>"
	),
	substitute_type(
		path = "pallet_revive::evm::api::debug_rpc_types::TracerType",
		with = "::subxt::utils::Static<::pallet_revive::evm::TracerType>"
	),

	substitute_type(
		path = "pallet_revive::evm::api::rpc_types_gen::GenericTransaction",
		with = "::subxt::utils::Static<::pallet_revive::evm::GenericTransaction>"
	),
	substitute_type(
		path = "pallet_revive::evm::api::rpc_types::DryRunConfig<M>",
		with = "::subxt::utils::Static<::pallet_revive::evm::DryRunConfig<M>>"
	),
	substitute_type(
		path = "pallet_revive::primitives::EthTransactInfo<B>",
		with = "::subxt::utils::Static<::pallet_revive::EthTransactInfo<B>>"
	),
	substitute_type(
		path = "pallet_revive::primitives::EthTransactError",
		with = "::subxt::utils::Static<::pallet_revive::EthTransactError>"
	),
	substitute_type(
		path = "pallet_revive::primitives::ExecReturnValue",
		with = "::subxt::utils::Static<::pallet_revive::ExecReturnValue>"
	),
	substitute_type(
		path = "sp_weights::weight_v2::Weight",
		with = "::subxt::utils::Static<::sp_weights::Weight>"
	),
	substitute_type(
		path = "pallet_revive::evm::api::rpc_types_gen::Block",
		with = "::subxt::utils::Static<::pallet_revive::evm::Block>"
	),
	substitute_type(
		path = "pallet_revive::evm::block_hash::ReceiptGasInfo",
		with = "::subxt::utils::Static<::pallet_revive::evm::ReceiptGasInfo>"
	),
	derive_for_all_types = "codec::Encode, codec::Decode"
)]
mod src_chain {}
pub use src_chain::*;

use crate::{
	client::{
		Balance, ClientError, SubscriptionType, SubstrateBlock, SubstrateBlockHash,
		SubstrateBlockNumber,
	},
	substrate_client::{BlockInfo, NodeHealth, RawExtrinsic, SubmitResult, SubstrateClientT},
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
use std::future::Future;
use subxt::{
	OnlineClient,
	backend::{
		StreamOf, StreamOfResults,
		legacy::{LegacyRpcMethods, rpc_methods::TransactionStatus as SubxtTxStatus},
		rpc::RpcClient,
	},
	ext::subxt_rpcs::rpc_params,
};

fn system_health_from_subxt(h: subxt::backend::legacy::rpc_methods::SystemHealth) -> NodeHealth {
	NodeHealth { peers: h.peers, is_syncing: h.is_syncing, should_have_peers: h.should_have_peers }
}

/// [`SubstrateClientT`] implementation backed by a `subxt` WebSocket connection.
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
	) -> Result<Self, ClientError> {
		let latest = api.blocks().at_latest().await?;
		let _runtime_api = api.runtime_api().at(latest.hash());

		let chain_id = {
			let query = constants().revive().chain_id();
			api.constants().at(&query)?
		};

		let max_block_weight = {
			let query = constants().system().block_weights();
			let weights = api.constants().at(&query)?;
			let max_block = weights.per_class.normal.max_extrinsic.unwrap_or(weights.max_block);
			max_block.0
		};

		Ok(Self { api, rpc_client, rpc, chain_id, max_block_weight })
	}

	fn block_info_from_subxt(block: &SubstrateBlock) -> BlockInfo {
		BlockInfo {
			hash: block.hash(),
			number: block.number(),
			parent_hash: block.header().parent_hash,
		}
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
	) -> Result<Option<BlockInfo>, ClientError> {
		match self.api.blocks().at(*hash).await {
			Ok(b) => Ok(Some(Self::block_info_from_subxt(&b))),
			Err(subxt::Error::Block(subxt::error::BlockError::NotFound(_))) => Ok(None),
			Err(e) => Err(e.into()),
		}
	}

	async fn block_by_number(
		&self,
		number: SubstrateBlockNumber,
	) -> Result<Option<BlockInfo>, ClientError> {
		let Some(hash) = self.rpc.chain_get_block_hash(Some(number.into())).await? else {
			return Ok(None);
		};
		self.block_by_hash(&hash).await
	}

	async fn latest_block(&self) -> Result<BlockInfo, ClientError> {
		let block = self.api.blocks().at_latest().await?;
		Ok(Self::block_info_from_subxt(&block))
	}

	async fn latest_finalized_block(&self) -> Result<BlockInfo, ClientError> {
		let hash = self.rpc.chain_get_finalized_head().await?;
		match self.api.blocks().at(hash).await {
			Ok(b) => Ok(Self::block_info_from_subxt(&b)),
			Err(e) => Err(e.into()),
		}
	}

	async fn dry_run(
		&self,
		block_hash: SubstrateBlockHash,
		tx: GenericTransaction,
		block: BlockNumberOrTagOrHash,
	) -> Result<EthTransactInfo<Balance>, ClientError> {
		use crate::client::runtime_api::RuntimeApi;
		RuntimeApi::new(self.api.runtime_api().at(block_hash)).dry_run(tx, block).await
	}

	async fn gas_price(&self, block_hash: SubstrateBlockHash) -> Result<U256, ClientError> {
		use crate::client::runtime_api::RuntimeApi;
		RuntimeApi::new(self.api.runtime_api().at(block_hash)).gas_price().await
	}

	async fn balance(
		&self,
		block_hash: SubstrateBlockHash,
		address: sp_core::H160,
	) -> Result<U256, ClientError> {
		use crate::client::runtime_api::RuntimeApi;
		RuntimeApi::new(self.api.runtime_api().at(block_hash)).balance(address).await
	}

	async fn nonce(
		&self,
		block_hash: SubstrateBlockHash,
		address: sp_core::H160,
	) -> Result<U256, ClientError> {
		use crate::client::runtime_api::RuntimeApi;
		RuntimeApi::new(self.api.runtime_api().at(block_hash)).nonce(address).await
	}

	async fn code(
		&self,
		block_hash: SubstrateBlockHash,
		address: sp_core::H160,
	) -> Result<Vec<u8>, ClientError> {
		use crate::client::runtime_api::RuntimeApi;
		RuntimeApi::new(self.api.runtime_api().at(block_hash)).code(address).await
	}

	async fn get_storage(
		&self,
		block_hash: SubstrateBlockHash,
		address: sp_core::H160,
		key: [u8; 32],
	) -> Result<Option<Vec<u8>>, ClientError> {
		use crate::client::runtime_api::RuntimeApi;
		RuntimeApi::new(self.api.runtime_api().at(block_hash))
			.get_storage(address, key)
			.await
	}

	async fn eth_block(&self, block_hash: SubstrateBlockHash) -> Result<EthBlock, ClientError> {
		use crate::client::runtime_api::RuntimeApi;
		RuntimeApi::new(self.api.runtime_api().at(block_hash)).eth_block().await
	}

	async fn eth_block_hash(
		&self,
		block_hash: SubstrateBlockHash,
		number: U256,
	) -> Result<Option<H256>, ClientError> {
		use crate::client::runtime_api::RuntimeApi;
		RuntimeApi::new(self.api.runtime_api().at(block_hash))
			.eth_block_hash(number)
			.await
	}

	async fn eth_receipt_data(
		&self,
		block_hash: SubstrateBlockHash,
	) -> Result<Vec<ReceiptGasInfo>, ClientError> {
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
	) -> Result<Vec<(u32, Trace)>, ClientError> {
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
	) -> Result<Trace, ClientError> {
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
	) -> Result<Trace, ClientError> {
		use crate::client::runtime_api::RuntimeApi;
		RuntimeApi::new(self.api.runtime_api().at(block_hash))
			.trace_call(transaction, config)
			.await
	}

	async fn submit_extrinsic(&self, payload: Vec<u8>) -> Result<SubmitResult, ClientError> {
		let call = tx().revive().eth_transact(payload);
		let ext = self.api.tx().create_unsigned(&call)?;

		let sub = self
			.rpc_client
			.subscribe(
				"author_submitAndWatchExtrinsic",
				rpc_params![to_hex(ext.encoded())],
				"author_unwatchExtrinsic",
			)
			.await?;

		let mut stream: StreamOfResults<SubxtTxStatus<SubstrateBlockHash>> =
			StreamOf::new(Box::pin(sub.map_err(|e| e.into())));

		tokio::time::timeout(std::time::Duration::from_secs(5), async {
			while let Some(status) = stream.next().await {
				let subxt_status: SubxtTxStatus<SubstrateBlockHash> =
					status.map_err(ClientError::from)?;
				let pool_status = subxt_tx_status_to_submit_result(subxt_status);

				match pool_status {
					SubmitResult::Usurped(_) | SubmitResult::Dropped | SubmitResult::Invalid => {
						return Err(ClientError::SubmitError(crate::client::SubmitError::from(
							pool_status,
						)));
					},
					other => return Ok(other),
				}
			}
			Err(ClientError::SubmitError(crate::client::SubmitError::StreamEnded))
		})
		.await
		.map_err(|_| ClientError::Timeout)?
	}

	async fn sync_state(
		&self,
	) -> Result<sc_rpc::system::SyncState<SubstrateBlockNumber>, ClientError> {
		let sync_state: sc_rpc::system::SyncState<SubstrateBlockNumber> =
			self.rpc_client.request("system_syncState", Default::default()).await?;
		Ok(sync_state)
	}

	async fn system_health(&self) -> Result<NodeHealth, ClientError> {
		Ok(system_health_from_subxt(self.rpc.system_health().await?))
	}

	async fn get_automine(&self) -> bool {
		self.rpc_client
			.request::<bool>("getAutomine", rpc_params![])
			.await
			.unwrap_or(false)
	}

	async fn subscribe_blocks<F, Fut>(
		&self,
		subscription_type: SubscriptionType,
		callback: F,
	) -> Result<(), ClientError>
	where
		F: Fn(BlockInfo) -> Fut + Send + Sync,
		Fut: Future<Output = Result<(), ClientError>> + Send,
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

			let info = Self::block_info_from_subxt(&block);

			if let Err(err) = callback(info).await {
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
		ClientError,
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
	) -> Result<Vec<RawExtrinsic>, ClientError> {
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
		use system::events::ExtrinsicSuccess;
		let block = self.api.blocks().at(block_hash).await.ok()?;
		let ext = block.extrinsics().await.ok()?.iter().nth(extrinsic_index)?;
		let event = ext.events().await.ok()?.find_first::<ExtrinsicSuccess>().ok()??;
		Some(event.dispatch_info.weight.0)
	}
}

fn to_hex(bytes: impl AsRef<[u8]>) -> String {
	format!("0x{}", hex::encode(bytes.as_ref()))
}

fn subxt_tx_status_to_submit_result(s: SubxtTxStatus<SubstrateBlockHash>) -> SubmitResult {
	use sc_transaction_pool_api::TransactionStatus as Pool;
	match s {
		SubxtTxStatus::Future => Pool::Future,
		SubxtTxStatus::Ready => Pool::Ready,
		SubxtTxStatus::Broadcast(peers) => Pool::Broadcast(peers),
		SubxtTxStatus::InBlock(hash) => Pool::InBlock((hash, 0)),
		SubxtTxStatus::Retracted(hash) => Pool::Retracted(hash),
		SubxtTxStatus::FinalityTimeout(hash) => Pool::FinalityTimeout(hash),
		SubxtTxStatus::Finalized(hash) => Pool::Finalized((hash, 0)),
		SubxtTxStatus::Usurped(hash) => Pool::Usurped(hash),
		SubxtTxStatus::Dropped => Pool::Dropped,
		SubxtTxStatus::Invalid => Pool::Invalid,
	}
}
