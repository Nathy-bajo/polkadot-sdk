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
//! The client connects to the source substrate chain
//! and is used by the rpc server to query and send transactions to the substrate chain.

pub(crate) mod runtime_api;
pub(crate) mod storage_api;

use crate::{
	BlockInfoProvider, BlockNumberOrTag, BlockTag, FeeHistoryProvider, ReceiptProvider, TracerType,
	TransactionInfo,
	block_info_provider::BlockInfo,
	substrate_client::{NodeHealth, SubmitResult, SubstrateClientT},
};
use jsonrpsee::types::{ErrorObjectOwned, error::CALL_EXECUTION_FAILED_CODE};
use pallet_revive::{
	EthTransactError,
	evm::{
		Block, BlockNumberOrTagOrHash, FeeHistoryResult, Filter, GenericTransaction, H256,
		HashesOrTransactionInfos, Log, ReceiptInfo, SyncingProgress, SyncingStatus, Trace,
		TransactionSigned, TransactionTrace, decode_revert_reason,
	},
};
use sp_weights::Weight;
use std::{ops::Range, sync::Arc};
use thiserror::Error;
use tokio::sync::Mutex;

/// The substrate block number type.
pub type SubstrateBlockNumber = u32;

/// The substrate block hash type.
pub type SubstrateBlockHash = sp_core::H256;

/// The runtime balance type.
pub type Balance = u128;

/// The subscription type used to listen to new blocks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SubscriptionType {
	/// Subscribe to best blocks.
	BestBlocks,
	/// Subscribe to finalized blocks.
	FinalizedBlocks,
}

/// Submit Error reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SubmitError {
	/// Transaction was usurped by another with the same nonce.
	#[error("Transaction was usurped by another with the same nonce")]
	Usurped,
	/// Transaction was dropped from the pool.
	#[error("Transaction was dropped")]
	Dropped,
	/// Transaction is invalid (e.g. bad nonce, signature, etc).
	#[error("Transaction is invalid (e.g. bad nonce, signature, etc)")]
	Invalid,
	/// Transaction stream ended without a terminal status.
	#[error("Transaction stream ended without status")]
	StreamEnded,
	/// Unknown transaction status.
	#[error("Unknown transaction status")]
	Unknown,
}

impl From<sc_transaction_pool_api::TransactionStatus<SubstrateBlockHash, SubstrateBlockHash>>
	for SubmitError
{
	fn from(
		status: sc_transaction_pool_api::TransactionStatus<SubstrateBlockHash, SubstrateBlockHash>,
	) -> Self {
		use sc_transaction_pool_api::TransactionStatus;
		match status {
			TransactionStatus::Usurped(_) => SubmitError::Usurped,
			TransactionStatus::Dropped => SubmitError::Dropped,
			TransactionStatus::Invalid => SubmitError::Invalid,
			_ => SubmitError::Unknown,
		}
	}
}

/// The error type for the client.
#[derive(Error, Debug)]
pub enum ClientError {
	/// A [`jsonrpsee::core::ClientError`] wrapper error.
	#[error(transparent)]
	Jsonrpsee(#[from] jsonrpsee::core::ClientError),
	/// An error originating from the native in-process Substrate client
	#[error("Native client error: {0}")]
	NativeClientError(String),
	/// A [`sqlx::Error`] wrapper error.
	#[error(transparent)]
	SqlxError(#[from] sqlx::Error),
	/// A [`codec::Error`] wrapper error.
	#[error(transparent)]
	CodecError(#[from] codec::Error),
	/// author_submitExtrinsic failed.
	#[error("Invalid transaction: {0}")]
	SubmitError(SubmitError),
	/// Transact call failed.
	#[error("contract reverted: {0:?}")]
	TransactError(EthTransactError),
	/// A decimal conversion failed.
	#[error("conversion failed")]
	ConversionFailed,
	/// The block hash was not found.
	#[error("hash not found")]
	BlockNotFound,
	/// The contract was not found.
	#[error("Contract not found")]
	ContractNotFound,
	#[error("No Ethereum extrinsic found")]
	EthExtrinsicNotFound,
	/// The transaction fee could not be found.
	#[error("transactionFeePaid event not found")]
	TxFeeNotFound,
	/// Failed to decode a raw payload into a signed transaction.
	#[error("Failed to decode a raw payload into a signed transaction")]
	TxDecodingFailed,
	/// Failed to recover eth address.
	#[error("failed to recover eth address")]
	RecoverEthAddressFailed,
	/// Failed to filter logs.
	#[error("Failed to filter logs")]
	LogFilterFailed(#[from] anyhow::Error),
	/// Receipt storage was not found.
	#[error("Receipt storage not found")]
	ReceiptDataNotFound,
	/// Ethereum block was not found.
	#[error("Ethereum block not found")]
	EthereumBlockNotFound,
	/// Receipt data length mismatch.
	#[error("Receipt data length mismatch")]
	ReceiptDataLengthMismatch,
	/// Transaction submission timeout.
	#[error("Transaction submission timeout")]
	Timeout,
}
const LOG_TARGET: &str = "eth-rpc::client";

const REVERT_CODE: i32 = 3;

const NOTIFIER_CAPACITY: usize = 16;
impl From<ClientError> for ErrorObjectOwned {
	fn from(err: ClientError) -> Self {
		match err {
			ClientError::TransactError(EthTransactError::Data(data)) => {
				let msg = match decode_revert_reason(&data) {
					Some(reason) => format!("execution reverted: {reason}"),
					None => "execution reverted".to_string(),
				};
				let data = format!("0x{}", hex::encode(data));
				ErrorObjectOwned::owned::<String>(REVERT_CODE, msg, Some(data))
			},
			ClientError::TransactError(EthTransactError::Message(msg)) => {
				ErrorObjectOwned::owned::<String>(CALL_EXECUTION_FAILED_CODE, msg, None)
			},
			_ => {
				ErrorObjectOwned::owned::<String>(CALL_EXECUTION_FAILED_CODE, err.to_string(), None)
			},
		}
	}
}

/// A client that connects to a Substrate node and maintains a receipt/block cache.
#[derive(Clone)]
pub struct Client<C: SubstrateClientT, BP: BlockInfoProvider> {
	/// The underlying substrate client backend.
	pub(crate) backend: C,
	/// Block info provider (caches latest best/finalized blocks).
	block_provider: BP,
	/// Receipt storage / cache.
	receipt_provider: ReceiptProvider<BP>,
	/// Fee history cache.
	fee_history_provider: FeeHistoryProvider,
	/// Whether the node has automine enabled.
	automine: bool,
	/// Block notifier for automine.
	block_notifier: Option<tokio::sync::broadcast::Sender<H256>>,
	/// Serialises write operations across concurrent subscriptions.
	subscription_lock: Arc<Mutex<()>>,
}

impl<C: SubstrateClientT, BP: BlockInfoProvider> Client<C, BP> {
	/// Create a `Client` from an arbitrary backend and block-info-provider.
	pub fn from_backend(
		backend: C,
		block_provider: BP,
		receipt_provider: ReceiptProvider<BP>,
		automine: bool,
	) -> Result<Self, ClientError> {
		let block_notifier =
			automine.then(|| tokio::sync::broadcast::channel::<H256>(NOTIFIER_CAPACITY).0);

		Ok(Self {
			backend,
			block_provider,
			receipt_provider,
			fee_history_provider: FeeHistoryProvider::default(),
			automine,
			block_notifier,
			subscription_lock: Arc::new(Mutex::new(())),
		})
	}

	pub fn from_native_backend(
		backend: C,
		block_provider: BP,
		receipt_provider: ReceiptProvider<BP>,
		automine: bool,
	) -> Result<Self, ClientError> {
		Self::from_backend(backend, block_provider, receipt_provider, automine)
	}

	/// Creates a block notifier instance.
	pub fn create_block_notifier(&mut self) {
		self.block_notifier = Some(tokio::sync::broadcast::channel::<H256>(NOTIFIER_CAPACITY).0);
	}

	/// Sets a block notifier.
	pub fn set_block_notifier(&mut self, notifier: Option<tokio::sync::broadcast::Sender<H256>>) {
		self.block_notifier = notifier;
	}

	/// Subscribe to past blocks, executing the callback for each block in `range`.
	async fn subscribe_past_blocks<F, Fut>(
		&self,
		range: Range<SubstrateBlockNumber>,
		callback: F,
	) -> Result<(), ClientError>
	where
		F: Fn(Arc<BP::Block>) -> Fut + Send + Sync,
		Fut: std::future::Future<Output = Result<(), ClientError>> + Send,
	{
		let mut block = self
			.block_provider
			.block_by_number(range.end)
			.await?
			.ok_or(ClientError::BlockNotFound)?;

		loop {
			let block_number = block.number();
			log::trace!(
				target: "eth-rpc::subscription",
				"Processing past block #{block_number}"
			);

			let parent_hash = block.parent_hash();
			callback(block.clone()).await.inspect_err(|err| {
				log::error!(
					target: "eth-rpc::subscription",
					"Failed to process past block #{block_number}: {err:?}"
				);
			})?;

			if range.start < block_number {
				block = self
					.block_provider
					.block_by_hash(&parent_hash)
					.await?
					.ok_or(ClientError::BlockNotFound)?;
			} else {
				return Ok(());
			}
		}
	}

	/// Subscribe to new blocks, and execute the async closure for each block.
	async fn subscribe_new_blocks<F, Fut>(
		&self,
		subscription_type: SubscriptionType,
		callback: F,
	) -> Result<(), ClientError>
	where
		F: Fn(Arc<BP::Block>) -> Fut + Send + Sync + 'static,
		Fut: std::future::Future<Output = Result<(), ClientError>> + Send,
	{
		let callback = Arc::new(callback);
		let lock = self.subscription_lock.clone();
		let block_provider = self.block_provider.clone();

		self.backend
			.subscribe_blocks(subscription_type, move |info: C::BlockInfo| {
				let callback = Arc::clone(&callback);
				let lock_ref = Arc::clone(&lock);
				let block_provider = block_provider.clone();

				async move {
					let block = block_provider
						.block_by_hash(&info.hash())
						.await?
						.ok_or(ClientError::BlockNotFound)?;

					let _guard = lock_ref.lock().await;
					callback(block).await
				}
			})
			.await
	}

	/// Start the block subscription, and populate the block cache.
	pub async fn subscribe_and_cache_new_blocks(
		&self,
		subscription_type: SubscriptionType,
	) -> Result<(), ClientError> {
		log::info!(
			target: LOG_TARGET,
			"🔌 Subscribing to new blocks ({subscription_type:?})"
		);

		let backend = self.backend.clone();
		let receipt_provider = self.receipt_provider.clone();
		let block_provider_update = self.block_provider.clone();
		let fee_history_provider = self.fee_history_provider.clone();
		let block_notifier = self.block_notifier.clone();

		self.subscribe_new_blocks(subscription_type, move |block: Arc<BP::Block>| {
			let backend = backend.clone();
			let receipt_provider = receipt_provider.clone();
			let block_provider_update = block_provider_update.clone();
			let fee_history_provider = fee_history_provider.clone();
			let block_notifier = block_notifier.clone();

			async move {
				let hash = block.hash();
				let number = block.number();
				let evm_block = backend.eth_block(hash).await?;

				let (_, receipts): (Vec<_>, Vec<_>) = receipt_provider
					.insert_block_receipts_by_hash(hash, number, &evm_block.hash)
					.await?
					.into_iter()
					.unzip();

				block_provider_update.update_latest(Arc::clone(&block), subscription_type).await;
				fee_history_provider.update_fee_history(&evm_block, &receipts).await;

				match (subscription_type, &block_notifier) {
					(SubscriptionType::BestBlocks, Some(sender)) if sender.receiver_count() > 0 => {
						let _ = sender.send(hash);
					},
					_ => {},
				}
				Ok(())
			}
		})
		.await
	}

	/// Cache old blocks up to the given block number.
	pub async fn subscribe_and_cache_blocks(
		&self,
		index_last_n_blocks: SubstrateBlockNumber,
	) -> Result<(), ClientError> {
		let last = self.latest_block().await.number().saturating_sub(1);
		let range = last.saturating_sub(index_last_n_blocks)..last;
		log::info!(target: LOG_TARGET, "🗄️ Indexing past blocks in range {range:?}");

		self.subscribe_past_blocks(range, |block: Arc<BP::Block>| async move {
			let ethereum_hash = self
				.backend
				.eth_block_hash(block.hash(), pallet_revive::evm::U256::from(block.number()))
				.await?
				.ok_or(ClientError::EthereumBlockNotFound)?;
			self.receipt_provider
				.insert_block_receipts_by_hash(block.hash(), block.number(), &ethereum_hash)
				.await?;
			Ok(())
		})
		.await?;

		log::info!(target: LOG_TARGET, "🗄️ Finished indexing past blocks");
		Ok(())
	}

	/// Get the block hash for the given block number or tag.
	pub async fn block_hash_for_tag(
		&self,
		at: BlockNumberOrTagOrHash,
	) -> Result<SubstrateBlockHash, ClientError> {
		match at {
			BlockNumberOrTagOrHash::BlockHash(hash) => self
				.resolve_substrate_hash(&hash)
				.await
				.ok_or(ClientError::EthereumBlockNotFound),
			BlockNumberOrTagOrHash::BlockNumber(block_number) => {
				let n: SubstrateBlockNumber =
					block_number.try_into().map_err(|_| ClientError::ConversionFailed)?;
				let hash = self.get_block_hash(n).await?.ok_or(ClientError::BlockNotFound)?;
				Ok(hash)
			},
			BlockNumberOrTagOrHash::BlockTag(BlockTag::Finalized | BlockTag::Safe) => {
				Ok(self.latest_finalized_block().await.hash())
			},
			BlockNumberOrTagOrHash::BlockTag(_) => Ok(self.latest_block().await.hash()),
		}
	}

	/// Get the latest finalized block.
	pub async fn latest_finalized_block(&self) -> Arc<BP::Block> {
		self.block_provider.latest_finalized_block().await
	}

	/// Get the latest best block.
	pub async fn latest_block(&self) -> Arc<BP::Block> {
		self.block_provider.latest_block().await
	}

	/// Get the block number of the latest block.
	pub async fn block_number(&self) -> Result<SubstrateBlockNumber, ClientError> {
		Ok(self.block_provider.latest_block().await.number())
	}

	/// Get a block hash for the given block number.
	pub async fn get_block_hash(
		&self,
		block_number: SubstrateBlockNumber,
	) -> Result<Option<SubstrateBlockHash>, ClientError> {
		Ok(self.block_provider.block_by_number(block_number).await?.map(|b| b.hash()))
	}

	/// Get a block for the specified hash or number.
	pub async fn block_by_number_or_tag(
		&self,
		block: &BlockNumberOrTag,
	) -> Result<Option<Arc<BP::Block>>, ClientError> {
		match block {
			BlockNumberOrTag::U256(n) => {
				let n = (*n).try_into().map_err(|_| ClientError::ConversionFailed)?;
				self.block_by_number(n).await
			},
			BlockNumberOrTag::BlockTag(BlockTag::Finalized | BlockTag::Safe) => {
				Ok(Some(self.block_provider.latest_finalized_block().await))
			},
			BlockNumberOrTag::BlockTag(_) => Ok(Some(self.block_provider.latest_block().await)),
		}
	}

	/// Get a block by hash.
	pub async fn block_by_hash(
		&self,
		hash: &SubstrateBlockHash,
	) -> Result<Option<Arc<BP::Block>>, ClientError> {
		self.block_provider.block_by_hash(hash).await
	}

	/// Resolve Ethereum block hash to Substrate block hash.
	pub async fn resolve_substrate_hash(&self, ethereum_hash: &H256) -> Option<H256> {
		self.receipt_provider.get_substrate_hash(ethereum_hash).await
	}

	/// Resolve Substrate block hash to Ethereum block hash.
	pub async fn resolve_ethereum_hash(&self, substrate_hash: &H256) -> Option<H256> {
		self.receipt_provider.get_ethereum_hash(substrate_hash).await
	}

	/// Get a block by Ethereum hash, falling back to treating it as a Substrate hash.
	pub async fn block_by_ethereum_hash(
		&self,
		ethereum_hash: &H256,
	) -> Result<Option<Arc<BP::Block>>, ClientError> {
		if let Some(substrate_hash) = self.resolve_substrate_hash(ethereum_hash).await {
			return self.block_by_hash(&substrate_hash).await;
		}
		self.block_by_hash(ethereum_hash).await
	}

	/// Get a block by number.
	pub async fn block_by_number(
		&self,
		block_number: SubstrateBlockNumber,
	) -> Result<Option<Arc<BP::Block>>, ClientError> {
		self.block_provider.block_by_number(block_number).await
	}

	/// Submit an ethereum transaction payload.
	pub async fn submit(&self, payload: Vec<u8>) -> Result<SubmitResult, ClientError> {
		self.backend.submit_extrinsic(payload).await
	}

	/// Get an EVM transaction receipt by hash.
	pub async fn receipt(&self, tx_hash: &H256) -> Option<ReceiptInfo> {
		self.receipt_provider.receipt_by_hash(tx_hash).await
	}

	/// Get an EVM transaction receipt by block hash and index.
	pub async fn receipt_by_hash_and_index(
		&self,
		block_hash: &H256,
		transaction_index: usize,
	) -> Option<ReceiptInfo> {
		self.receipt_provider
			.receipt_by_block_hash_and_index(block_hash, transaction_index)
			.await
	}

	pub async fn signed_tx_by_hash(&self, tx_hash: &H256) -> Option<TransactionSigned> {
		self.receipt_provider.signed_tx_by_hash(tx_hash).await
	}

	/// Get receipts count per block.
	pub async fn receipts_count_per_block(&self, block_hash: &SubstrateBlockHash) -> Option<usize> {
		self.receipt_provider.receipts_count_per_block(block_hash).await
	}

	pub async fn sync_state(
		&self,
	) -> Result<sc_rpc::system::SyncState<SubstrateBlockNumber>, ClientError> {
		self.backend.sync_state().await
	}

	/// Get the syncing status of the chain.
	pub async fn syncing(&self) -> Result<SyncingStatus, ClientError> {
		let health = self.backend.system_health().await?;
		let status = if health.is_syncing {
			let sync_state = self.sync_state().await?;
			SyncingProgress {
				current_block: Some(sync_state.current_block.into()),
				highest_block: Some(sync_state.highest_block.into()),
				starting_block: Some(sync_state.starting_block.into()),
			}
			.into()
		} else {
			SyncingStatus::Bool(false)
		};

		Ok(status)
	}

	/// Get the system health.
	pub async fn system_health(&self) -> Result<NodeHealth, ClientError> {
		self.backend.system_health().await
	}

	/// Get the chain ID.
	pub fn chain_id(&self) -> u64 {
		self.backend.chain_id()
	}

	/// Get the max block weight.
	pub fn max_block_weight(&self) -> Weight {
		self.backend.max_block_weight()
	}

	/// Get the block notifier.
	pub fn block_notifier(&self) -> Option<tokio::sync::broadcast::Sender<H256>> {
		self.block_notifier.clone()
	}

	/// Check if automine is enabled.
	pub fn is_automine(&self) -> bool {
		self.automine
	}

	/// Get the automine status from the node.
	pub async fn get_automine(&self) -> bool {
		self.backend.get_automine().await
	}

	/// Get the EVM block for the given block.
	pub async fn evm_block(
		&self,
		block: Arc<BP::Block>,
		hydrated_transactions: bool,
	) -> Option<Block> {
		log::trace!(
			target: LOG_TARGET,
			"Get Ethereum block for hash {:?}",
			block.hash()
		);
		match self.backend.eth_block(block.hash()).await {
			Ok(mut eth_block) => {
				log::trace!(
					target: LOG_TARGET,
					"Ethereum block from runtime API hash {:?}",
					eth_block.hash
				);

				if hydrated_transactions {
					// Hydrate the block.
					let tx_infos = self
						.receipt_provider
						.receipts_from_block_by_hash(block.hash(), block.number())
						.await
						.unwrap_or_default()
						.into_iter()
						.map(|(signed_tx, receipt)| TransactionInfo::new(&receipt, signed_tx))
						.collect::<Vec<_>>();

					eth_block.transactions = HashesOrTransactionInfos::TransactionInfos(tx_infos);
				}

				Some(eth_block)
			},
			Err(err) => {
				log::error!(
					target: LOG_TARGET,
					"Failed to get Ethereum block for hash {:?}: {err:?}",
					block.hash()
				);
				None
			},
		}
	}

	/// Get the logs matching the given filter.
	pub async fn logs(&self, filter: Option<Filter>) -> Result<Vec<Log>, ClientError> {
		self.receipt_provider.logs(filter).await.map_err(ClientError::LogFilterFailed)
	}

	pub async fn fee_history(
		&self,
		block_count: u32,
		latest_block: BlockNumberOrTag,
		reward_percentiles: Option<Vec<f64>>,
	) -> Result<FeeHistoryResult, ClientError> {
		let Some(latest_block) = self.block_by_number_or_tag(&latest_block).await? else {
			return Err(ClientError::BlockNotFound);
		};

		self.fee_history_provider
			.fee_history(block_count, latest_block.number(), reward_percentiles)
			.await
	}

	/// Get the gas price at the given block.
	pub async fn gas_price_at(
		&self,
		block_hash: SubstrateBlockHash,
	) -> Result<sp_core::U256, ClientError> {
		self.backend.gas_price(block_hash).await
	}

	/// Get the balance at the given block.
	pub async fn balance_at(
		&self,
		block_hash: SubstrateBlockHash,
		address: sp_core::H160,
	) -> Result<sp_core::U256, ClientError> {
		self.backend.balance(block_hash, address).await
	}

	/// Get the nonce at the given block.
	pub async fn nonce_at(
		&self,
		block_hash: SubstrateBlockHash,
		address: sp_core::H160,
	) -> Result<sp_core::U256, ClientError> {
		self.backend.nonce(block_hash, address).await
	}

	/// Get the code at the given block.
	pub async fn code_at(
		&self,
		block_hash: SubstrateBlockHash,
		address: sp_core::H160,
	) -> Result<Vec<u8>, ClientError> {
		self.backend.code(block_hash, address).await
	}

	/// Get the storage at the given block.
	pub async fn storage_at(
		&self,
		block_hash: SubstrateBlockHash,
		address: sp_core::H160,
		key: [u8; 32],
	) -> Result<Option<Vec<u8>>, ClientError> {
		self.backend.get_storage(block_hash, address, key).await
	}

	/// Dry-run a transaction and return the estimated gas as `U256`.
	pub async fn dry_run(
		&self,
		block_hash: SubstrateBlockHash,
		tx: GenericTransaction,
		block: BlockNumberOrTagOrHash,
	) -> Result<pallet_revive::evm::U256, ClientError> {
		self.backend.dry_run(block_hash, tx, block).await.map(|info| info.eth_gas)
	}

	/// Fetch the raw signed block (used for tracing).
	async fn tracing_block(
		&self,
		block_hash: H256,
	) -> Result<
		sp_runtime::generic::Block<
			sp_runtime::generic::Header<u32, sp_runtime::traits::BlakeTwo256>,
			sp_runtime::OpaqueExtrinsic,
		>,
		ClientError,
	> {
		self.backend.signed_block(block_hash).await
	}

	/// Get the transaction traces for the given block.
	pub async fn trace_block_by_number(
		&self,
		at: BlockNumberOrTag,
		config: TracerType,
	) -> Result<Vec<TransactionTrace>, ClientError> {
		if self.receipt_provider.is_before_earliest_block(&at) {
			return Ok(vec![]);
		}

		let block_hash = self.block_hash_for_tag(at.into()).await?;
		let block = self.tracing_block(block_hash).await?;
		let traces = self.backend.trace_block(block_hash, block, config).await?;

		let mut hashes = self
			.receipt_provider
			.block_transaction_hashes(&block_hash)
			.await
			.ok_or(ClientError::EthExtrinsicNotFound)?;

		let traces = traces.into_iter().filter_map(|(index, trace)| {
			Some(TransactionTrace { tx_hash: hashes.remove(&(index as usize))?, trace })
		});

		Ok(traces.collect())
	}

	/// Get the transaction traces for the given transaction.
	pub async fn trace_transaction(
		&self,
		transaction_hash: H256,
		config: TracerType,
	) -> Result<Trace, ClientError> {
		let (block_hash, transaction_index) = self
			.receipt_provider
			.find_transaction(&transaction_hash)
			.await
			.ok_or(ClientError::EthExtrinsicNotFound)?;

		let block = self.tracing_block(block_hash).await?;
		self.backend.trace_tx(block_hash, block, transaction_index as u32, config).await
	}

	/// Dry-run a call and return its trace.
	pub async fn trace_call(
		&self,
		transaction: GenericTransaction,
		block: BlockNumberOrTagOrHash,
		config: TracerType,
	) -> Result<Trace, ClientError> {
		let block_hash = self.block_hash_for_tag(block).await?;
		self.backend.trace_call(block_hash, transaction, config).await
	}

	/// Get the post-dispatch weight associated with this Ethereum transaction hash.
	pub async fn post_dispatch_weight(&self, tx_hash: &H256) -> Option<Weight> {
		let receipt = self.receipt(tx_hash).await?;
		let block_hash = self.resolve_substrate_hash(&receipt.block_hash).await?;
		self.backend
			.extrinsic_post_dispatch_weight(block_hash, receipt.transaction_index.as_usize())
			.await
	}
}
