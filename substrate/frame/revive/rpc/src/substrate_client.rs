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

//! Substrate client trait and shared types for the ETH RPC server.
//!
//! This module provides a [`SubstrateClientT`] trait that abstracts over the
//! Substrate node client, allowing the ETH RPC server to be integrated directly
//! into a parachain node (e.g. Asset Hub) without requiring a separate `subxt`
//! connection.
use crate::{
	block_info_provider::BlockInfo,
	client::{Balance, SubscriptionType, SubstrateBlockHash, SubstrateBlockNumber},
};
use jsonrpsee::core::async_trait;
use pallet_revive::{
	EthTransactInfo,
	evm::{
		Block as EthBlock, BlockNumberOrTagOrHash, GenericTransaction, ReceiptGasInfo, Trace,
		TracerType, U256,
	},
};
use sc_transaction_pool_api::TransactionStatus;
use sp_core::H256;
use sp_weights::Weight;
use std::future::Future;

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

/// The result of submitting a transaction to the pool.
pub type SubmitResult = TransactionStatus<SubstrateBlockHash, SubstrateBlockHash>;

/// The core trait that the ETH RPC server requires from the underlying Substrate node.
#[async_trait]
pub trait SubstrateClientT: Send + Sync + Clone + 'static {
	/// The concrete block-info type returned by this client.
	type BlockInfo: BlockInfo + Clone + Send + Sync + 'static;

	/// Return the EVM chain-id constant.
	fn chain_id(&self) -> u64;

	/// Return the maximum block weight (used to compute the block gas limit).
	fn max_block_weight(&self) -> Weight;

	/// Fetch a block by its Substrate hash.
	async fn block_by_hash(
		&self,
		hash: &SubstrateBlockHash,
	) -> Result<Option<Self::BlockInfo>, crate::client::ClientError>;

	/// Fetch a block by its block number.
	async fn block_by_number(
		&self,
		number: SubstrateBlockNumber,
	) -> Result<Option<Self::BlockInfo>, crate::client::ClientError>;

	/// Return metadata for the current best (latest) block.
	async fn latest_block(&self) -> Result<Self::BlockInfo, crate::client::ClientError>;

	/// Return metadata for the current finalized block.
	async fn latest_finalized_block(&self) -> Result<Self::BlockInfo, crate::client::ClientError>;

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

	/// Submit an unsigned `eth_transact` extrinsic and return the first
	/// transaction status received from the pool.
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
		F: Fn(Self::BlockInfo) -> Fut + Send + Sync,
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

	/// Scan block events for the given extrinsic index and return its
	/// post-dispatch weight (if available).
	async fn extrinsic_post_dispatch_weight(
		&self,
		_block_hash: SubstrateBlockHash,
		_extrinsic_index: usize,
	) -> Option<sp_weights::Weight> {
		None
	}

	/// Return the post-dispatch weight for a given transaction hash.
	async fn post_dispatch_weight(
		&self,
		_tx_hash: &SubstrateBlockHash,
	) -> Option<sp_weights::Weight> {
		None
	}
}
