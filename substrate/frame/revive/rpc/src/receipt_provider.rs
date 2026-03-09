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
	Address, AddressOrAddresses, BlockInfoProvider, BlockNumberOrTag, BlockTag, Bytes, ClientError,
	FilterTopic, ReceiptExtractor, block_info_provider::BlockInfo, client::SubstrateBlockNumber,
};
use pallet_revive::evm::{Filter, Log, ReceiptInfo, TransactionSigned};
use sp_core::{H256, U256};
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool, query};
use std::{
	collections::{BTreeMap, HashMap},
	sync::Arc,
};
use tokio::sync::Mutex;

const LOG_TARGET: &str = "eth-rpc::receipt_provider";

/// ReceiptProvider stores transaction receipts and logs in a SQLite database.
#[derive(Clone)]
pub struct ReceiptProvider<BP: BlockInfoProvider = crate::SubxtBlockInfoProvider> {
	/// The database pool.
	pool: SqlitePool,
	/// The block provider used to fetch blocks and reconstruct receipts.
	block_provider: BP,
	/// A means to extract receipts from extrinsics.
	receipt_extractor: ReceiptExtractor,
	/// When `Some`, old blocks will be pruned.
	keep_latest_n_blocks: Option<usize>,
	/// A map of the latest block numbers to block hashes.
	block_number_to_hashes: Arc<Mutex<BTreeMap<SubstrateBlockNumber, BlockHashMap>>>,
}

/// Substrate block to Ethereum block mapping
#[derive(Clone, Debug, PartialEq, Eq)]
struct BlockHashMap {
	substrate_hash: H256,
	ethereum_hash: H256,
}

impl BlockHashMap {
	fn new(substrate_hash: H256, ethereum_hash: H256) -> Self {
		Self { substrate_hash, ethereum_hash }
	}
}

/// Maximum number of entries kept in the block-to-hash map (prevents unbounded growth in
/// persistent-DB mode).
const MAX_CACHED_BLOCKS: usize = 256;

impl<BP: BlockInfoProvider> ReceiptProvider<BP> {
	/// Create a new `ReceiptProvider`.
	pub async fn new(
		pool: SqlitePool,
		block_provider: BP,
		receipt_extractor: ReceiptExtractor,
		keep_latest_n_blocks: Option<usize>,
	) -> Result<Self, sqlx::Error> {
		sqlx::migrate!().run(&pool).await?;
		Ok(Self {
			pool,
			block_provider,
			receipt_extractor,
			keep_latest_n_blocks,
			block_number_to_hashes: Default::default(),
		})
	}

	// Get block hash and  transaction index by transaction hash
	pub async fn find_transaction(&self, transaction_hash: &H256) -> Option<(H256, usize)> {
		let transaction_hash = transaction_hash.as_ref();
		let result = query!(
			r#"
			SELECT block_hash, transaction_index
			FROM transaction_hashes
			WHERE transaction_hash = $1
			"#,
			transaction_hash
		)
		.fetch_optional(&self.pool)
		.await
		.ok()??;

		let block_hash = H256::from_slice(&result.block_hash[..]);
		let transaction_index = result.transaction_index.try_into().ok()?;
		Some((block_hash, transaction_index))
	}

	/// Get the Substrate block hash for the given Ethereum block hash.
	pub async fn get_substrate_hash(&self, ethereum_block_hash: &H256) -> Option<H256> {
		let ethereum_hash = ethereum_block_hash.as_ref();
		let result = query!(
			r#"
			SELECT substrate_block_hash
			FROM eth_to_substrate_blocks
			WHERE ethereum_block_hash = $1
			"#,
			ethereum_hash
		)
		.fetch_optional(&self.pool)
		.await
		.inspect_err(|e| {
			log::error!(target: LOG_TARGET, "failed to get block mapping for ethereum block {ethereum_block_hash:?}, err: {e:?}");
		})
		.ok()?
		.or_else(|| {
			log::trace!(target: LOG_TARGET, "No block mapping found for ethereum block: {ethereum_block_hash:?}");
			None
		})?;

		log::trace!(
			target: LOG_TARGET,
			"Get block mapping ethereum block: {:?} -> substrate block: {ethereum_block_hash:?}",
			H256::from_slice(&result.substrate_block_hash[..])
		);

		Some(H256::from_slice(&result.substrate_block_hash[..]))
	}

	/// Get the Ethereum block hash for the given Substrate block hash.
	pub async fn get_ethereum_hash(&self, substrate_block_hash: &H256) -> Option<H256> {
		let substrate_hash = substrate_block_hash.as_ref();
		let result = query!(
			r#"
			SELECT ethereum_block_hash
			FROM eth_to_substrate_blocks
			WHERE substrate_block_hash = $1
			"#,
			substrate_hash
		)
		.fetch_optional(&self.pool)
		.await
		.inspect_err(|e| {
			log::error!(target: LOG_TARGET, "failed to get block mapping for substrate block {substrate_block_hash:?}, err: {e:?}");
		})
		.ok()?
		.or_else(|| {
			log::trace!(target: LOG_TARGET, "No block mapping found for substrate block: {substrate_block_hash:?}");
			None
		})?;

		log::trace!(
			target: LOG_TARGET,
			"Get block mapping substrate block: {substrate_block_hash:?} -> ethereum block: {:?}",
			H256::from_slice(&result.ethereum_block_hash[..])
		);

		Some(H256::from_slice(&result.ethereum_block_hash[..]))
	}

	/// Check if the block is before the earliest indexed block.
	pub fn is_before_earliest_block(&self, at: &BlockNumberOrTag) -> bool {
		match at {
			BlockNumberOrTag::U256(block_number) => {
				self.receipt_extractor.is_before_earliest_block(block_number.as_u32())
			},
			BlockNumberOrTag::BlockTag(_) => false,
		}
	}

	/// Get the number of receipts for a given substrate block hash.
	pub async fn receipts_count_per_block(&self, block_hash: &H256) -> Option<usize> {
		let block_hash_ref = block_hash.as_ref();
		// Use query_unchecked! to avoid sqlx offline-cache issues with COUNT(*) aliases.
		let count: i64 =
			sqlx::query_scalar("SELECT COUNT(*) FROM transaction_hashes WHERE block_hash = ?")
				.bind(block_hash_ref)
				.fetch_one(&self.pool)
				.await
				.ok()?;

		Some(count as usize)
	}

	/// Return all transaction hashes for the given (substrate) block hash.
	pub async fn block_transaction_hashes(
		&self,
		block_hash: &H256,
	) -> Option<HashMap<usize, H256>> {
		let block_hash_ref = block_hash.as_ref();
		let rows: Vec<(i64, Vec<u8>)> = sqlx::query_as(
			"SELECT transaction_index, transaction_hash FROM transaction_hashes WHERE block_hash = ?",
		)
		.bind(block_hash_ref)
		.fetch_all(&self.pool)
		.await
		.ok()?;

		Some(
			rows.into_iter()
				.map(|(idx, hash)| (idx as usize, H256::from_slice(&hash)))
				.collect(),
		)
	}

	/// Extract receipts from a block identified by its hash and number.
	pub async fn receipts_from_block_by_hash(
		&self,
		block_hash: H256,
		block_number: SubstrateBlockNumber,
	) -> Result<Vec<(TransactionSigned, ReceiptInfo)>, ClientError> {
		self.receipt_extractor
			.extract_from_block_by_hash(block_hash, block_number)
			.await
	}

	/// Extract and insert receipts from the given block (identified by hash + number).
	pub async fn insert_block_receipts_by_hash(
		&self,
		block_hash: H256,
		block_number: SubstrateBlockNumber,
		ethereum_hash: &H256,
	) -> Result<Vec<(TransactionSigned, ReceiptInfo)>, ClientError> {
		let receipts = self.receipts_from_block_by_hash(block_hash, block_number).await?;
		self.insert_with_hashes(block_hash, block_number, &receipts, ethereum_hash)
			.await?;
		Ok(receipts)
	}

	/// Get the receipt for a given block hash and transaction index.
	pub async fn receipt_by_block_hash_and_index(
		&self,
		block_hash: &H256,
		transaction_index: usize,
	) -> Option<ReceiptInfo> {
		let block = self.block_provider.block_by_hash(block_hash).await.ok()??;
		let (_, receipt) = self
			.receipt_extractor
			.extract_from_transaction_by_hash(block.hash(), block.number(), transaction_index)
			.await
			.ok()?;
		Some(receipt)
	}

	/// Get the receipt for the given transaction hash.
	pub async fn receipt_by_hash(&self, transaction_hash: &H256) -> Option<ReceiptInfo> {
		let (block_hash, transaction_index) = self.find_transaction(transaction_hash).await?;
		let block = self.block_provider.block_by_hash(&block_hash).await.ok()??;
		let (_, receipt) = self
			.receipt_extractor
			.extract_from_transaction_by_hash(block.hash(), block.number(), transaction_index)
			.await
			.ok()?;
		Some(receipt)
	}

	/// Get the signed transaction for the given transaction hash.
	pub async fn signed_tx_by_hash(&self, transaction_hash: &H256) -> Option<TransactionSigned> {
		let (block_hash, transaction_index) = self.find_transaction(transaction_hash).await?;
		let block = self.block_provider.block_by_hash(&block_hash).await.ok()??;
		let (signed_tx, _) = self
			.receipt_extractor
			.extract_from_transaction_by_hash(block.hash(), block.number(), transaction_index)
			.await
			.ok()?;
		Some(signed_tx)
	}

	/// Extract receipts from a subxt `SubstrateBlock` (legacy helper).
	pub async fn receipts_from_block(
		&self,
		block: &crate::client::SubstrateBlock,
	) -> Result<Vec<(TransactionSigned, ReceiptInfo)>, ClientError> {
		self.receipt_extractor.extract_from_block(block).await
	}

	/// Extract and insert receipts from a subxt `SubstrateBlock` (legacy helper).
	pub async fn insert_block_receipts(
		&self,
		block: &crate::client::SubstrateBlock,
		ethereum_hash: &H256,
	) -> Result<Vec<(TransactionSigned, ReceiptInfo)>, ClientError> {
		let receipts = self.receipts_from_block(block).await?;
		self.insert_with_hashes(block.hash(), block.number(), &receipts, ethereum_hash)
			.await?;
		Ok(receipts)
	}

	/// Insert a block mapping from Ethereum block hash to Substrate block hash.
	async fn insert_block_mapping(&self, block_map: &BlockHashMap) -> Result<(), ClientError> {
		let ethereum_hash_ref = block_map.ethereum_hash.as_ref();
		let substrate_hash_ref = block_map.substrate_hash.as_ref();

		query!(
			r#"
			INSERT OR REPLACE INTO eth_to_substrate_blocks (ethereum_block_hash, substrate_block_hash)
			VALUES ($1, $2)
			"#,
			ethereum_hash_ref,
			substrate_hash_ref,
		)
		.execute(&self.pool)
		.await?;

		log::trace!(
			target: LOG_TARGET,
			"Insert block mapping ethereum block: {:?} -> substrate block: {:?}",
			block_map.ethereum_hash,
			block_map.substrate_hash
		);
		Ok(())
	}

	/// Deletes older records from the database.
	async fn remove(&self, block_mappings: &[BlockHashMap]) -> Result<(), ClientError> {
		if block_mappings.is_empty() {
			return Ok(());
		}
		log::debug!(target: LOG_TARGET, "Removing block hashes: {block_mappings:?}");

		let placeholders = vec!["?"; block_mappings.len()].join(", ");
		let sql = format!("DELETE FROM transaction_hashes WHERE block_hash in ({placeholders})");
		let mut delete_tx_query = sqlx::query(&sql);

		let sql = format!(
			"DELETE FROM eth_to_substrate_blocks WHERE substrate_block_hash in ({placeholders})"
		);
		let mut delete_mappings_query = sqlx::query(&sql);

		let sql = format!("DELETE FROM logs WHERE block_hash in ({placeholders})");
		let mut delete_logs_query = sqlx::query(&sql);

		for block_map in block_mappings {
			delete_tx_query = delete_tx_query.bind(block_map.substrate_hash.as_ref());
			delete_mappings_query = delete_mappings_query.bind(block_map.substrate_hash.as_ref());
			// logs table uses ethereum block hash
			delete_logs_query = delete_logs_query.bind(block_map.ethereum_hash.as_ref());
		}

		let delete_transaction_hashes = delete_tx_query.execute(&self.pool);
		let delete_logs = delete_logs_query.execute(&self.pool);
		let delete_mappings = delete_mappings_query.execute(&self.pool);
		tokio::try_join!(delete_transaction_hashes, delete_logs, delete_mappings)?;
		Ok(())
	}

	/// Prune old blocks when the cache is full or a fork is detected.
	async fn prune_blocks(
		&self,
		block_number: SubstrateBlockNumber,
		block_map: &BlockHashMap,
	) -> Result<(), ClientError> {
		let mut to_remove = Vec::new();
		let mut block_number_to_hash = self.block_number_to_hashes.lock().await;

		// Fork? - If inserting the same block number with a different hash, remove the old ones.
		match block_number_to_hash.insert(block_number, block_map.clone()) {
			Some(old_block_map) if &old_block_map != block_map => {
				to_remove.push(old_block_map);

				let mut next = block_number.saturating_add(1);
				while let Some(old) = block_number_to_hash.remove(&next) {
					to_remove.push(old);
					next = next.saturating_add(1);
				}
			},
			_ => {},
		}

		if let Some(keep_latest_n_blocks) = self.keep_latest_n_blocks {
			// If we have more blocks than we should keep, remove the oldest ones by count
			// (not by block number range, to handle gaps correctly)
			while block_number_to_hash.len() > keep_latest_n_blocks {
				// Remove the block with the smallest number (first in BTreeMap)
				if let Some((_, block_map)) = block_number_to_hash.pop_first() {
					to_remove.push(block_map);
				}
			}
		} else {
			// Evict oldest entries to prevent unbounded growth.
			// Forks deeper than MAX_CACHED_BLOCKS(256) are unlikely.
			while block_number_to_hash.len() > MAX_CACHED_BLOCKS {
				block_number_to_hash.pop_first();
			}
		}

		// Release the lock.
		drop(block_number_to_hash);

		if !to_remove.is_empty() {
			log::trace!(target: LOG_TARGET, "Pruning old blocks: {to_remove:?}");
			self.remove(&to_remove).await?;
		}

		Ok(())
	}

	/// Insert receipts for a block identified by its hashes.
	async fn insert_with_hashes(
		&self,
		substrate_block_hash: H256,
		block_number: SubstrateBlockNumber,
		receipts: &[(TransactionSigned, ReceiptInfo)],
		ethereum_hash: &H256,
	) -> Result<(), ClientError> {
		let substrate_hash_ref = substrate_block_hash.as_ref();
		let block_number_i64 = block_number as i64;
		let ethereum_hash_ref = ethereum_hash.as_ref();
		let block_map = BlockHashMap::new(substrate_block_hash, *ethereum_hash);

		log::trace!(
			target: LOG_TARGET,
			"Insert receipts for substrate block #{block_number_i64} {:?}",
			substrate_block_hash
		);

		self.prune_blocks(block_number, &block_map).await?;

		let exists: bool = sqlx::query_scalar(
			"SELECT EXISTS(SELECT 1 FROM eth_to_substrate_blocks WHERE substrate_block_hash = ?)",
		)
		.bind(substrate_hash_ref)
		.fetch_one(&self.pool)
		.await?;

		// Assuming that if no mapping exists then no relevant entries in transaction_hashes and
		// logs exist
		if !exists {
			for (_, receipt) in receipts {
				let transaction_hash: &[u8] = receipt.transaction_hash.as_ref();
				let transaction_index = receipt.transaction_index.as_u32() as i32;

				query!(
					r#"
					INSERT OR REPLACE INTO transaction_hashes (transaction_hash, block_hash, transaction_index)
					VALUES ($1, $2, $3)
					"#,
					transaction_hash,
					substrate_hash_ref,
					transaction_index
				)
				.execute(&self.pool)
				.await?;

				for log in &receipt.logs {
					let log_index = log.log_index.as_u32() as i32;
					let address: &[u8] = log.address.as_ref();

					let topic_0 = log.topics.first().as_ref().map(|v| &v[..]);
					let topic_1 = log.topics.get(1).as_ref().map(|v| &v[..]);
					let topic_2 = log.topics.get(2).as_ref().map(|v| &v[..]);
					let topic_3 = log.topics.get(3).as_ref().map(|v| &v[..]);
					let data = log.data.as_ref().map(|v| &v.0[..]);

					query!(
						r#"
						INSERT INTO logs(
							block_hash,
							transaction_index,
							log_index,
							address,
							block_number,
							transaction_hash,
							topic_0, topic_1, topic_2, topic_3,
							data)
						VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
						"#,
						ethereum_hash_ref,
						transaction_index,
						log_index,
						address,
						block_number_i64,
						transaction_hash,
						topic_0,
						topic_1,
						topic_2,
						topic_3,
						data
					)
					.execute(&self.pool)
					.await?;
				}
			}

			self.insert_block_mapping(&block_map).await?;
		}

		Ok(())
	}

	/// Get logs matching the given filter.
	pub async fn logs(&self, filter: Option<Filter>) -> anyhow::Result<Vec<Log>> {
		let mut qb = QueryBuilder::<Sqlite>::new("SELECT logs.* FROM logs WHERE 1=1");
		let filter = filter.unwrap_or_default();

		let latest_block = U256::from(self.block_provider.latest_block_number().await);

		let as_block_number = |block_param| match block_param {
			None => Ok(None),
			Some(BlockNumberOrTag::U256(v)) => Ok(Some(v)),
			Some(BlockNumberOrTag::BlockTag(BlockTag::Latest)) => Ok(Some(latest_block)),
			Some(BlockNumberOrTag::BlockTag(tag)) => anyhow::bail!("Unsupported tag: {tag:?}"),
		};

		let from_block = as_block_number(filter.from_block)?;
		let to_block = as_block_number(filter.to_block)?;

		match (from_block, to_block, filter.block_hash) {
			(Some(_), _, Some(_)) | (_, Some(_), Some(_)) => {
				anyhow::bail!("block number and block hash cannot be used together");
			},

			(Some(block), _, _) | (_, Some(block), _) if block > latest_block => {
				anyhow::bail!("block number exceeds latest block");
			},
			(Some(from_block), Some(to_block), None) if from_block > to_block => {
				anyhow::bail!("invalid block range params");
			},
			(Some(from_block), Some(to_block), None) if from_block == to_block => {
				qb.push(" AND block_number = ").push_bind(from_block.as_u64() as i64);
			},
			(Some(from_block), Some(to_block), None) => {
				qb.push(" AND block_number BETWEEN ")
					.push_bind(from_block.as_u64() as i64)
					.push(" AND ")
					.push_bind(to_block.as_u64() as i64);
			},
			(Some(from_block), None, None) => {
				qb.push(" AND block_number >= ").push_bind(from_block.as_u64() as i64);
			},
			(None, Some(to_block), None) => {
				qb.push(" AND block_number <= ").push_bind(to_block.as_u64() as i64);
			},
			(None, None, Some(hash)) => {
				qb.push(" AND block_hash = ").push_bind(hash.0.to_vec());
			},
			(None, None, None) => {
				qb.push(" AND block_number = ").push_bind(latest_block.as_u64() as i64);
			},
		}

		if let Some(addresses) = filter.address {
			match addresses {
				AddressOrAddresses::Address(addr) => {
					qb.push(" AND address = ").push_bind(addr.0.to_vec());
				},
				AddressOrAddresses::Addresses(addrs) => {
					qb.push(" AND address IN (");
					let mut separated = qb.separated(", ");
					for addr in addrs {
						separated.push_bind(addr.0.to_vec());
					}
					separated.push_unseparated(")");
				},
			}
		}

		if let Some(topics) = filter.topics {
			if topics.len() > 4 {
				return Err(anyhow::anyhow!("exceed max topics"));
			}

			for (i, topic) in topics.into_iter().enumerate() {
				match topic {
					FilterTopic::Single(hash) => {
						qb.push(format_args!(" AND topic_{i} = ")).push_bind(hash.0.to_vec());
					},
					FilterTopic::Multiple(hashes) => {
						qb.push(format_args!(" AND topic_{i} IN ("));
						let mut separated = qb.separated(", ");
						for hash in hashes {
							separated.push_bind(hash.0.to_vec());
						}
						separated.push_unseparated(")");
					},
				}
			}
		}

		qb.push(" LIMIT 10000");

		let logs = qb
			.build()
			.try_map(|row| {
				let block_hash: Vec<u8> = row.try_get("block_hash")?;
				let transaction_index: i64 = row.try_get("transaction_index")?;
				let log_index: i64 = row.try_get("log_index")?;
				let address: Vec<u8> = row.try_get("address")?;
				let block_number: i64 = row.try_get("block_number")?;
				let transaction_hash: Vec<u8> = row.try_get("transaction_hash")?;
				let topic_0: Option<Vec<u8>> = row.try_get("topic_0")?;
				let topic_1: Option<Vec<u8>> = row.try_get("topic_1")?;
				let topic_2: Option<Vec<u8>> = row.try_get("topic_2")?;
				let topic_3: Option<Vec<u8>> = row.try_get("topic_3")?;
				let data: Option<Vec<u8>> = row.try_get("data")?;

				let topics = [topic_0, topic_1, topic_2, topic_3]
					.iter()
					.filter_map(|t| t.as_ref().map(|t| H256::from_slice(t)))
					.collect::<Vec<_>>();

				Ok(Log {
					address: Address::from_slice(&address),
					block_hash: H256::from_slice(&block_hash),
					block_number: U256::from(block_number as u64),
					data: data.map(Bytes::from),
					log_index: U256::from(log_index as u64),
					topics,
					transaction_hash: H256::from_slice(&transaction_hash),
					transaction_index: U256::from(transaction_index as u64),
					removed: false,
				})
			})
			.fetch_all(&self.pool)
			.await?;

		Ok(logs)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::block_info_provider::test::{MockBlockInfo, MockBlockInfoProvider};
	use pallet_revive::evm::{ReceiptInfo, TransactionSigned};
	use pretty_assertions::assert_eq;
	use sp_core::{H160, H256};
	use sqlx::SqlitePool;

	async fn count(pool: &SqlitePool, table: &str, block_hash: Option<H256>) -> usize {
		let count: i64 = match block_hash {
			None => {
				sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
					.fetch_one(pool)
					.await
			},
			Some(hash) => {
				sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table} WHERE block_hash = ?"))
					.bind(hash.as_ref())
					.fetch_one(pool)
					.await
			},
		}
		.unwrap();

		count as _
	}

	async fn setup_sqlite_provider(pool: SqlitePool) -> ReceiptProvider<MockBlockInfoProvider> {
		ReceiptProvider {
			pool,
			block_provider: MockBlockInfoProvider,
			receipt_extractor: ReceiptExtractor::new_mock(),
			keep_latest_n_blocks: Some(10),
			block_number_to_hashes: Default::default(),
		}
	}

	#[sqlx::test]
	async fn test_insert_remove(pool: SqlitePool) -> anyhow::Result<()> {
		let provider = setup_sqlite_provider(pool).await;
		let block = MockBlockInfo { hash: H256::default(), number: 0 };
		let receipts = vec![(
			TransactionSigned::default(),
			ReceiptInfo {
				logs: vec![Log { block_hash: block.hash, ..Default::default() }],
				..Default::default()
			},
		)];
		let ethereum_hash = H256::from([1_u8; 32]);
		let block_map = BlockHashMap::new(block.hash(), ethereum_hash);

		provider
			.insert_with_hashes(block.hash(), block.number(), &receipts, &ethereum_hash)
			.await?;
		let row = provider.find_transaction(&receipts[0].1.transaction_hash).await;
		assert_eq!(row, Some((block.hash, 0)));

		provider.remove(&[block_map]).await?;
		assert_eq!(count(&provider.pool, "transaction_hashes", Some(block.hash())).await, 0);
		assert_eq!(count(&provider.pool, "logs", Some(block.hash())).await, 0);
		Ok(())
	}

	#[sqlx::test]
	async fn test_prune(pool: SqlitePool) -> anyhow::Result<()> {
		let provider = setup_sqlite_provider(pool).await;
		let n = provider.keep_latest_n_blocks.unwrap();

		for i in 0..2 * n {
			let block = MockBlockInfo { hash: H256::from([i as u8; 32]), number: i as _ };
			let transaction_hash = H256::from([i as u8; 32]);
			let receipts = vec![(
				TransactionSigned::default(),
				ReceiptInfo {
					transaction_hash,
					logs: vec![Log {
						block_hash: block.hash,
						transaction_hash,
						..Default::default()
					}],
					..Default::default()
				},
			)];
			let ethereum_hash = H256::from([(i + 1) as u8; 32]);
			provider
				.insert_with_hashes(block.hash(), block.number(), &receipts, &ethereum_hash)
				.await?;
		}
		assert_eq!(count(&provider.pool, "transaction_hashes", None).await, n);
		assert_eq!(count(&provider.pool, "logs", None).await, n);
		assert_eq!(count(&provider.pool, "eth_to_substrate_blocks", None).await, n);
		assert_eq!(provider.block_number_to_hashes.lock().await.len(), n);

		return Ok(());
	}

	#[sqlx::test]
	async fn test_fork(pool: SqlitePool) -> anyhow::Result<()> {
		let provider = setup_sqlite_provider(pool).await;

		let build_block = |seed: u8, number: u32| {
			let block = MockBlockInfo { hash: H256::from([seed; 32]), number };
			let transaction_hash = H256::from([seed; 32]);
			let receipts = vec![(
				TransactionSigned::default(),
				ReceiptInfo {
					transaction_hash,
					logs: vec![Log {
						block_hash: block.hash,
						transaction_hash,
						..Default::default()
					}],
					..Default::default()
				},
			)];
			let ethereum_hash = H256::from([seed + 1; 32]);

			(block, receipts, ethereum_hash)
		};

		let (block0, receipts, eh0) = build_block(0, 0);
		provider
			.insert_with_hashes(block0.hash(), block0.number(), &receipts, &eh0)
			.await?;
		let (block1, receipts, eh1) = build_block(1, 1);
		provider
			.insert_with_hashes(block1.hash(), block1.number(), &receipts, &eh1)
			.await?;
		let (block2, receipts, eh2) = build_block(2, 2);
		provider
			.insert_with_hashes(block2.hash(), block2.number(), &receipts, &eh2)
			.await?;
		let (block3, receipts, eh3) = build_block(3, 3);
		provider
			.insert_with_hashes(block3.hash(), block3.number(), &receipts, &eh3)
			.await?;

		assert_eq!(count(&provider.pool, "transaction_hashes", None).await, 4);
		assert_eq!(count(&provider.pool, "logs", None).await, 4);
		assert_eq!(count(&provider.pool, "eth_to_substrate_blocks", None).await, 4);
		assert_eq!(
			provider.block_number_to_hashes.lock().await.clone(),
			[
				(0, BlockHashMap::new(block0.hash, eh0)),
				(1, BlockHashMap::new(block1.hash, eh1)),
				(2, BlockHashMap::new(block2.hash, eh2)),
				(3, BlockHashMap::new(block3.hash, eh3))
			]
			.into(),
		);

		// Fork at height 1.
		let (fork_block, receipts, eh_fork) = build_block(4, 1);
		provider
			.insert_with_hashes(fork_block.hash(), fork_block.number(), &receipts, &eh_fork)
			.await?;

		assert_eq!(count(&provider.pool, "transaction_hashes", None).await, 2);
		assert_eq!(count(&provider.pool, "logs", None).await, 2);
		assert_eq!(count(&provider.pool, "eth_to_substrate_blocks", None).await, 2);

		assert_eq!(
			provider.block_number_to_hashes.lock().await.clone(),
			[
				(0, BlockHashMap::new(block0.hash, eh0)),
				(1, BlockHashMap::new(fork_block.hash, eh_fork))
			]
			.into(),
		);

		return Ok(());
	}

	#[sqlx::test]
	async fn test_receipts_count_per_block(pool: SqlitePool) -> anyhow::Result<()> {
		let provider = setup_sqlite_provider(pool).await;
		let block = MockBlockInfo { hash: H256::default(), number: 0 };
		let receipts = vec![
			(
				TransactionSigned::default(),
				ReceiptInfo { transaction_hash: H256::from([0u8; 32]), ..Default::default() },
			),
			(
				TransactionSigned::default(),
				ReceiptInfo { transaction_hash: H256::from([1u8; 32]), ..Default::default() },
			),
		];
		let ethereum_hash = H256::from([2u8; 32]);

		provider
			.insert_with_hashes(block.hash(), block.number(), &receipts, &ethereum_hash)
			.await?;
		let count = provider.receipts_count_per_block(&block.hash).await;
		assert_eq!(count, Some(2));
		Ok(())
	}

	#[sqlx::test]
	async fn persistent_mode_caps_in_memory_map(pool: SqlitePool) -> anyhow::Result<()> {
		// Persistent DB mode: keep_latest_n_blocks = None
		let provider = ReceiptProvider {
			pool,
			block_provider: MockBlockInfoProvider,
			receipt_extractor: ReceiptExtractor::new_mock(),
			keep_latest_n_blocks: None,
			block_number_to_hashes: Default::default(),
		};

		// Insert more than MAX_CACHED_BLOCKS blocks.
		let start_block: u64 = 1;
		let n = MAX_CACHED_BLOCKS + 1;
		let end_block = start_block + n as u64;
		for i in start_block..end_block {
			let block = MockBlockInfo { hash: H256::from_low_u64_be(i), number: i as _ };
			let receipts = vec![(
				TransactionSigned::default(),
				ReceiptInfo {
					transaction_hash: H256::from_low_u64_be(i),
					logs: vec![Log {
						block_hash: block.hash,
						transaction_hash: H256::from_low_u64_be(i),
						..Default::default()
					}],
					..Default::default()
				},
			)];
			let ethereum_hash = H256::from_low_u64_be(i + 1);
			provider
				.insert_with_hashes(block.hash(), block.number(), &receipts, &ethereum_hash)
				.await?;
		}

		// The map is capped at MAX_CACHED_BLOCKS.
		let map = provider.block_number_to_hashes.lock().await;
		assert_eq!(map.len(), MAX_CACHED_BLOCKS);

		// The oldest block (1) should have been evicted, keeping blocks 2..=MAX+1.
		assert!(!map.contains_key(&1));
		assert!(map.contains_key(&2));
		assert!(map.contains_key(&(MAX_CACHED_BLOCKS as u32 + 1)));
		drop(map);

		// All blocks are still in the DB.
		assert_eq!(count(&provider.pool, "eth_to_substrate_blocks", None).await, n);

		Ok(())
	}
}
