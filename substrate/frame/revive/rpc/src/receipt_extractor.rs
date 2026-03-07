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
use crate::{
	ClientError, H160, LOG_TARGET,
	client::{SubstrateBlock, SubstrateBlockNumber, runtime_api::RuntimeApi},
	substrate_client::RawExtrinsic,
	subxt_client::SrcChainConfig,
};

use pallet_revive::{
	ReviveApi, create1,
	evm::{GenericTransaction, H256, Log, ReceiptGasInfo, ReceiptInfo, TransactionSigned, U256},
};
use sp_core::keccak_256;
use std::{future::Future, pin::Pin, sync::Arc};
use subxt::OnlineClient;
const PALLET_REVIVE_INDEX: u8 = 253;
const ETH_TRANSACT_CALL_INDEX: u8 = 0;

type FetchReceiptDataFn = Arc<
	dyn Fn(H256) -> Pin<Box<dyn Future<Output = Option<Vec<ReceiptGasInfo>>> + Send>> + Send + Sync,
>;

type FetchEthBlockHashFn =
	Arc<dyn Fn(H256, u64) -> Pin<Box<dyn Future<Output = Option<H256>> + Send>> + Send + Sync>;

type RecoverEthAddressFn = Arc<dyn Fn(&TransactionSigned) -> Result<H160, ()> + Send + Sync>;

type FetchBlockEventsFn =
	Arc<dyn Fn(H256) -> Pin<Box<dyn Future<Output = Option<BlockEvents>> + Send>> + Send + Sync>;

#[derive(Clone, Debug, Default)]
pub struct ExtrinsicEvents {
	pub success: bool,
	pub logs: Vec<(H160, Vec<H256>, Option<Vec<u8>>)>,
}

/// All extrinsic events for a block.
pub type BlockEvents = Vec<ExtrinsicEvents>;

/// Utility to extract receipts from extrinsics.
#[derive(Clone)]
pub struct ReceiptExtractor {
	/// Fetch the receipt data info.
	fetch_receipt_data: FetchReceiptDataFn,

	/// Fetch ethereum block hash.
	fetch_eth_block_hash: FetchEthBlockHashFn,
	/// Fetch decoded events for every extrinsic in a block.
	fetch_block_events: Option<FetchBlockEventsFn>,
	earliest_receipt_block: Option<SubstrateBlockNumber>,

	/// Recover the ethereum address from a transaction signature.
	recover_eth_address: RecoverEthAddressFn,
}

impl ReceiptExtractor {
	/// Check if the block is before the earliest block.
	pub fn is_before_earliest_block(&self, block_number: SubstrateBlockNumber) -> bool {
		block_number < self.earliest_receipt_block.unwrap_or_default()
	}

	/// Create a new `ReceiptExtractor` backed by a **subxt** `OnlineClient`.
	pub async fn new(
		api: OnlineClient<SrcChainConfig>,
		earliest_receipt_block: Option<SubstrateBlockNumber>,
	) -> Result<Self, ClientError> {
		Self::new_with_custom_address_recovery(
			api,
			earliest_receipt_block,
			Arc::new(|signed_tx: &TransactionSigned| signed_tx.recover_eth_address()),
		)
		.await
	}

	pub async fn new_with_custom_address_recovery(
		api: OnlineClient<SrcChainConfig>,
		earliest_receipt_block: Option<SubstrateBlockNumber>,
		recover_eth_address_fn: RecoverEthAddressFn,
	) -> Result<Self, ClientError> {
		use crate::subxt_client::revive::events::{ContractEmitted, EthExtrinsicRevert};

		let api_for_hash = api.clone();
		let fetch_eth_block_hash = Arc::new(move |block_hash, block_number| {
			let api = api_for_hash.clone();
			let fut = async move {
				let runtime_api = RuntimeApi::new(api.runtime_api().at(block_hash));
				runtime_api.eth_block_hash(U256::from(block_number)).await.ok().flatten()
			};

			Box::pin(fut) as Pin<Box<_>>
		});

		let api_for_data = api.clone();
		let fetch_receipt_data = Arc::new(move |block_hash| {
			let api = api_for_data.clone();
			let fut = async move {
				let runtime_api = RuntimeApi::new(api.runtime_api().at(block_hash));
				runtime_api.eth_receipt_data().await.ok()
			};
			Box::pin(fut) as Pin<Box<_>>
		});

		let api_for_events = api.clone();
		let fetch_block_events: FetchBlockEventsFn = Arc::new(move |block_hash| {
			let api = api_for_events.clone();
			let fut = async move {
				let block = api.blocks().at(block_hash).await.ok()?;
				let extrinsics = block.extrinsics().await.ok()?;

				let mut result = Vec::new();
				for ext in extrinsics.iter() {
					let events = ext.events().await.ok()?;
					let success = !events.has::<EthExtrinsicRevert>().unwrap_or(false);
					let logs = events
						.iter()
						.filter_map(|ev| {
							let ev = ev.ok()?;
							let ce = ev.as_event::<ContractEmitted>().ok()??;
							Some((ce.contract, ce.topics, Some(ce.data)))
						})
						.collect();
					result.push(ExtrinsicEvents { success, logs });
				}
				Some(result)
			};
			Box::pin(fut) as Pin<Box<_>>
		});

		Ok(Self {
			fetch_receipt_data,
			fetch_eth_block_hash,
			fetch_block_events: Some(fetch_block_events),
			earliest_receipt_block,
			recover_eth_address: recover_eth_address_fn,
		})
	}

	/// Create a `ReceiptExtractor` for the **native in-process** path.
	pub fn new_native(
		fetch_receipt_data_fn: impl Fn(
			H256,
		) -> Pin<
			Box<dyn Future<Output = Option<Vec<ReceiptGasInfo>>> + Send>,
		> + Send
		+ Sync
		+ 'static,
		fetch_eth_block_hash_fn: impl Fn(
			H256,
			u64,
		) -> Pin<Box<dyn Future<Output = Option<H256>> + Send>>
		+ Send
		+ Sync
		+ 'static,
		earliest_receipt_block: Option<SubstrateBlockNumber>,
		fetch_block_events_fn: Option<
			impl Fn(H256) -> Pin<Box<dyn Future<Output = Option<BlockEvents>> + Send>>
			+ Send
			+ Sync
			+ 'static,
		>,
	) -> Self {
		Self {
			fetch_receipt_data: Arc::new(fetch_receipt_data_fn),
			fetch_eth_block_hash: Arc::new(fetch_eth_block_hash_fn),
			fetch_block_events: fetch_block_events_fn.map(|f| Arc::new(f) as FetchBlockEventsFn),
			earliest_receipt_block,
			recover_eth_address: Arc::new(|signed_tx: &TransactionSigned| {
				signed_tx.recover_eth_address()
			}),
		}
	}

	/// Convenience wrapper: build a native `ReceiptExtractor` directly from a
	/// Substrate `client` that implements `ProvideRuntimeApi` + `HeaderBackend`.
	pub fn new_native_from_client<Client, Block, Moment>(
		client: Arc<Client>,
		earliest_receipt_block: Option<SubstrateBlockNumber>,
	) -> Self
	where
		Block: sp_runtime::traits::Block<Hash = sp_core::H256> + Send + Sync + 'static,
		Moment: codec::Codec + Clone + Send + Sync + 'static,
		Client: sp_api::ProvideRuntimeApi<Block>
			+ sc_client_api::HeaderBackend<Block>
			+ Send
			+ Sync
			+ 'static,
		Client::Api: crate::native_client::ReviveRuntimeApiT<Block, Moment>,
	{
		// ── receipt-data closure ─────────────────────────────────────────────
		let client_for_data = client.clone();
		let fetch_receipt_data_fn = move |block_hash: H256| {
			let client = client_for_data.clone();
			let fut = async move { client.runtime_api().eth_receipt_data(block_hash).ok() };
			Box::pin(fut) as Pin<Box<dyn Future<Output = Option<Vec<ReceiptGasInfo>>> + Send>>
		};

		let client_for_hash = client.clone();
		let fetch_eth_block_hash_fn = move |block_hash: H256, block_number: u64| {
			let client = client_for_hash.clone();
			let fut = async move {
				client
					.runtime_api()
					.eth_block_hash(block_hash, U256::from(block_number))
					.ok()
					.flatten()
			};
			Box::pin(fut) as Pin<Box<dyn Future<Output = Option<H256>> + Send>>
		};

		Self::new_native(
			fetch_receipt_data_fn,
			fetch_eth_block_hash_fn,
			earliest_receipt_block,
			// No block-event scanning in the native path.
			None::<fn(H256) -> Pin<Box<dyn Future<Output = Option<BlockEvents>> + Send>>>,
		)
	}

	#[cfg(test)]
	pub fn new_mock() -> Self {
		let fetch_receipt_data = Arc::new(|_| Box::pin(std::future::ready(None)) as Pin<Box<_>>);
		// This method is useful when testing eth - substrate mapping.
		let fetch_eth_block_hash = Arc::new(|block_hash: H256, block_number: u64| {
			// Generate hash from substrate block hash and number
			let bytes: Vec<u8> = [block_hash.as_bytes(), &block_number.to_be_bytes()].concat();
			let eth_block_hash = H256::from(keccak_256(&bytes));
			Box::pin(std::future::ready(Some(eth_block_hash))) as Pin<Box<_>>
		});

		Self {
			fetch_receipt_data,
			fetch_eth_block_hash,
			fetch_block_events: None,
			earliest_receipt_block: None,
			recover_eth_address: Arc::new(|signed_tx: &TransactionSigned| {
				signed_tx.recover_eth_address()
			}),
		}
	}

	/// Try to decode a raw extrinsic as an `eth_transact` call.
	fn decode_eth_transact(raw: &RawExtrinsic) -> Option<Vec<u8>> {
		let bytes = &raw.payload;

		let mut offset = 0usize;
		let first = *bytes.get(offset)?;
		let len_len = match first & 0b11 {
			0 => 1,
			1 => 2,
			2 => 4,
			_ => return None,
		};
		offset += len_len;

		let _version = *bytes.get(offset)?;
		offset += 1;

		let pallet_idx = *bytes.get(offset)?;
		offset += 1;
		let call_idx = *bytes.get(offset)?;
		offset += 1;

		if pallet_idx != PALLET_REVIVE_INDEX || call_idx != ETH_TRANSACT_CALL_INDEX {
			return None;
		}

		use codec::Decode;
		let mut rest = &bytes[offset..];
		let payload = Vec::<u8>::decode(&mut rest).ok()?;
		Some(payload)
	}

	/// Extract a receipt from a decoded call and its associated events.
	fn build_receipt(
		&self,
		substrate_block_number: u32,
		eth_block_hash: H256,
		ext_events: &ExtrinsicEvents,
		eth_payload: &[u8],
		receipt_gas_info: &ReceiptGasInfo,
		transaction_index: usize,
	) -> Result<(TransactionSigned, ReceiptInfo), ClientError> {
		let transaction_hash = H256(keccak_256(eth_payload));
		let signed_tx =
			TransactionSigned::decode(eth_payload).map_err(|_| ClientError::TxDecodingFailed)?;

		let from = (self.recover_eth_address)(&signed_tx).map_err(|_| {
			log::error!(target: LOG_TARGET, "Failed to recover eth address from signed tx");
			ClientError::RecoverEthAddressFailed
		})?;

		let tx_info = GenericTransaction::from_signed(
			signed_tx.clone(),
			receipt_gas_info.effective_gas_price,
			Some(from),
		);

		let block_number: U256 = substrate_block_number.into();

		let logs = ext_events
			.logs
			.iter()
			.enumerate()
			.map(|(log_idx, (address, topics, data))| Log {
				address: *address,
				topics: topics.clone(),
				data: data.as_ref().map(|d| d.clone().into()),
				block_number,
				transaction_hash,
				transaction_index: transaction_index.into(),
				block_hash: eth_block_hash,
				log_index: log_idx.into(),
				..Default::default()
			})
			.collect();

		let contract_address = if tx_info.to.is_none() {
			Some(create1(
				&from,
				tx_info
					.nonce
					.unwrap_or_default()
					.try_into()
					.map_err(|_| ClientError::ConversionFailed)?,
			))
		} else {
			None
		};

		let receipt = ReceiptInfo::new(
			eth_block_hash,
			block_number,
			contract_address,
			from,
			logs,
			tx_info.to,
			receipt_gas_info.effective_gas_price,
			U256::from(receipt_gas_info.gas_used),
			ext_events.success,
			transaction_hash,
			transaction_index.into(),
			tx_info.r#type.unwrap_or_default(),
		);
		Ok((signed_tx, receipt))
	}

	/// Extract receipts from a block using `RawExtrinsic` data.
	pub async fn extract_from_block_raw(
		&self,
		_block_hash: H256,
		block_number: u32,
		eth_block_hash: H256,
		raw_extrinsics: &[RawExtrinsic],
		receipt_data: &[ReceiptGasInfo],
		block_events: Option<&[ExtrinsicEvents]>,
	) -> Result<Vec<(TransactionSigned, ReceiptInfo)>, ClientError> {
		let eth_extrinsics: Vec<(usize, Vec<u8>)> = raw_extrinsics
			.iter()
			.filter_map(|raw| {
				let payload = Self::decode_eth_transact(raw)?;
				Some((raw.index, payload))
			})
			.collect();

		if eth_extrinsics.len() != receipt_data.len() {
			log::error!(
				target: LOG_TARGET,
				"Receipt data length ({}) != eth extrinsics ({}) for block #{block_number}",
				receipt_data.len(),
				eth_extrinsics.len()
			);
			return Err(ClientError::ReceiptDataLengthMismatch);
		}

		let mut results = Vec::with_capacity(eth_extrinsics.len());
		for (i, (ext_index, eth_payload)) in eth_extrinsics.iter().enumerate() {
			let events = if let Some(all_events) = block_events {
				all_events.get(*ext_index).cloned().unwrap_or_default()
			} else {
				// Native path: no event scanning available; mark success = true.
				ExtrinsicEvents { success: true, logs: vec![] }
			};

			let receipt = self.build_receipt(
				block_number,
				eth_block_hash,
				&events,
				eth_payload,
				&receipt_data[i],
				*ext_index,
			)?;
			results.push(receipt);
		}
		Ok(results)
	}

	/// Get the Ethereum block hash for the Substrate block with specific hash.
	pub async fn get_ethereum_block_hash(
		&self,
		block_hash: &H256,
		block_number: u64,
	) -> Option<H256> {
		(self.fetch_eth_block_hash)(*block_hash, block_number).await
	}

	/// Extract receipts from block.
	pub async fn extract_from_block(
		&self,
		block: &SubstrateBlock,
	) -> Result<Vec<(TransactionSigned, ReceiptInfo)>, ClientError> {
		if self.is_before_earliest_block(block.number()) {
			return Ok(vec![]);
		}

		let block_hash = block.hash();
		let block_number = block.number();

		let eth_block_hash = (self.fetch_eth_block_hash)(block_hash, block_number as u64)
			.await
			.unwrap_or(block_hash);

		let receipt_data = (self.fetch_receipt_data)(block_hash)
			.await
			.ok_or(ClientError::ReceiptDataNotFound)?;

		let block_events = if let Some(ref fetch_events) = self.fetch_block_events {
			fetch_events(block_hash).await
		} else {
			None
		};

		let extrinsics = block.extrinsics().await?;
		let raw: Vec<RawExtrinsic> = extrinsics
			.iter()
			.enumerate()
			.map(|(i, ext)| RawExtrinsic { payload: ext.bytes().to_vec(), index: i })
			.collect();

		self.extract_from_block_raw(
			block_hash,
			block_number,
			eth_block_hash,
			&raw,
			&receipt_data,
			block_events.as_deref(),
		)
		.await
	}

	/// Extract a single receipt from a block by transaction index.
	pub async fn extract_from_transaction(
		&self,
		block: &SubstrateBlock,
		transaction_index: usize,
	) -> Result<(TransactionSigned, ReceiptInfo), ClientError> {
		let receipts = self.extract_from_block(block).await?;
		receipts
			.into_iter()
			.find(|(_, r)| r.transaction_index.as_usize() == transaction_index)
			.ok_or(ClientError::EthExtrinsicNotFound)
	}
}
