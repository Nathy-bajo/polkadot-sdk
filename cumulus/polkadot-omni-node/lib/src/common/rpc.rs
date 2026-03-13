// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
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

//! Parachain-specific RPCs implementation.

#![warn(missing_docs)]

use crate::common::{
	types::{AccountId, Balance, Nonce, ParachainBackend, ParachainClient},
	ConstructNodeRuntimeApi,
};
use pallet_transaction_payment_rpc::{TransactionPayment, TransactionPaymentApiServer};
use sc_rpc::{
	dev::{Dev, DevApiServer},
	statement::{StatementApiServer, StatementStore},
};
use sp_runtime::traits::Block as BlockT;
use std::{marker::PhantomData, sync::Arc};
use substrate_frame_rpc_system::{System, SystemApiServer};
use substrate_state_trie_migration_rpc::{StateMigration, StateMigrationApiServer};

/// A type representing all RPC extensions.
pub type RpcExtension = jsonrpsee::RpcModule<()>;

pub(crate) trait BuildRpcExtensions<Client, Backend, Pool, StatementStore> {
	fn build_rpc_extensions(
		client: Arc<Client>,
		backend: Arc<Backend>,
		pool: Arc<Pool>,
		statement_store: Option<Arc<StatementStore>>,
		spawn_handle: Arc<dyn sp_core::traits::SpawnNamed>,
	) -> sc_service::error::Result<RpcExtension>;
}

pub(crate) struct BuildParachainRpcExtensions<Block, RuntimeApi>(PhantomData<(Block, RuntimeApi)>);

impl<Block: BlockT, RuntimeApi>
	BuildRpcExtensions<
		ParachainClient<Block, RuntimeApi>,
		ParachainBackend<Block>,
		sc_transaction_pool::TransactionPoolHandle<Block, ParachainClient<Block, RuntimeApi>>,
		sc_statement_store::Store,
	> for BuildParachainRpcExtensions<Block, RuntimeApi>
where
	RuntimeApi:
		ConstructNodeRuntimeApi<Block, ParachainClient<Block, RuntimeApi>> + Send + Sync + 'static,
	RuntimeApi::RuntimeApi: pallet_transaction_payment_rpc::TransactionPaymentRuntimeApi<Block, Balance>
		+ substrate_frame_rpc_system::AccountNonceApi<Block, AccountId, Nonce>,
{
	fn build_rpc_extensions(
		client: Arc<ParachainClient<Block, RuntimeApi>>,
		backend: Arc<ParachainBackend<Block>>,
		pool: Arc<
			sc_transaction_pool::TransactionPoolHandle<Block, ParachainClient<Block, RuntimeApi>>,
		>,
		statement_store: Option<Arc<sc_statement_store::Store>>,
		spawn_handle: Arc<dyn sp_core::traits::SpawnNamed>,
	) -> sc_service::error::Result<RpcExtension> {
		let build = || -> Result<RpcExtension, Box<dyn std::error::Error + Send + Sync>> {
			let mut module = RpcExtension::new(());

			module.merge(System::new(client.clone(), pool).into_rpc())?;
			module.merge(TransactionPayment::new(client.clone()).into_rpc())?;
			module.merge(StateMigration::new(client.clone(), backend).into_rpc())?;
			if let Some(statement_store) = statement_store {
				module.merge(StatementStore::new(statement_store, spawn_handle).into_rpc())?;
			}
			module.merge(Dev::new(client).into_rpc())?;

			Ok(module)
		};
		build().map_err(Into::into)
	}
}

/// RPC extensions for the Asset Hub node
pub struct BuildAssetHubRpcExtensions<Block, RuntimeApi>(PhantomData<(Block, RuntimeApi)>);

/// Configuration for the embedded ETH RPC server.
#[derive(Clone, Debug)]
pub struct EthRpcConfig {
	/// Number of blocks to keep in the receipt cache.
	pub cache_size: usize,
	/// Whether to allow unprotected (chain-id-less) transactions.
	pub allow_unprotected_txs: bool,
	/// SQLite database URL for receipt storage.
	pub database_url: Option<String>,
	/// EVM chain ID.
	pub chain_id: u64,
}

impl Default for EthRpcConfig {
	fn default() -> Self {
		Self {
			cache_size: 256,
			allow_unprotected_txs: false,
			database_url: None,
			chain_id: 420420421,
		}
	}
}

/// The SQLite in-memory connection string.
const IN_MEMORY_DB: &str = "sqlite::memory:";

/// Resolve the effective database URL and whether we're in-memory.
fn resolve_db(database_url: &Option<String>) -> (&str, bool) {
	match database_url.as_deref() {
		None | Some("") => (IN_MEMORY_DB, true),
		Some(url) if url == IN_MEMORY_DB => (IN_MEMORY_DB, true),
		Some(url) => (url, false),
	}
}

/// Build the ETH RPC [`RpcExtension`] using the native in-process Substrate client.
pub fn build_revive_eth_rpc_module_native<Block, RuntimeApi, Pool>(
	config: EthRpcConfig,
	native_client: Arc<ParachainClient<Block, RuntimeApi>>,
	pool: Arc<Pool>,
	is_dev: bool,
	spawn_handle: Arc<dyn sp_core::traits::SpawnNamed>,
) -> sc_service::error::Result<RpcExtension>
where
	Block: sp_runtime::traits::Block<Hash = sp_core::H256, Extrinsic = sp_runtime::OpaqueExtrinsic>
		+ Send
		+ Sync
		+ 'static,
	Block::Header: sp_runtime::traits::Header<Number = u32, Hash = sp_core::H256> + Send + Sync,
	RuntimeApi:
		ConstructNodeRuntimeApi<Block, ParachainClient<Block, RuntimeApi>> + Send + Sync + 'static,
	RuntimeApi::RuntimeApi: pallet_revive_eth_rpc::native_client::ReviveRuntimeApiT<Block, u64>
		+ sp_api::Core<Block>
		+ sp_api::Metadata<Block>,
	Pool: sc_transaction_pool_api::TransactionPool<Block = Block> + Send + Sync + 'static,
{
	use pallet_revive_eth_rpc::{
		cli::build_eth_rpc_module,
		client::{Client, SubscriptionType},
		native_block_info_provider::NativeClientBlockInfoProvider,
		native_client::NativeSubstrateClient,
		ReceiptExtractor, ReceiptProvider,
	};
	use sqlx::sqlite::SqlitePoolOptions;

	let tokio_handle = tokio::runtime::Handle::current();

	let eth_client = tokio_handle
		.block_on(async {
			let block_provider = NativeClientBlockInfoProvider::new(native_client.clone())
				.map_err(|e| {
					pallet_revive_eth_rpc::client::ClientError::NativeClientError(e.to_string())
				})?;

			let backend =
				NativeSubstrateClient::new(native_client.clone(), pool, config.chain_id, false)
					.map_err(|e| {
						pallet_revive_eth_rpc::client::ClientError::NativeClientError(e.to_string())
					})?;

			let receipt_extractor = ReceiptExtractor::new_from_substrate_client(
				backend.clone(),
				None, // earliest_receipt_block
			);

			// Resolve whether we use an in-memory or file-backed database.
			let (db_url, is_in_memory) = resolve_db(&config.database_url);

			let (pool_db, keep_latest_n_blocks) = if is_in_memory {
				log::warn!(
					target: pallet_revive_eth_rpc::LOG_TARGET,
					"💾 Using in-memory receipt DB, keeping only {} blocks",
					config.cache_size,
				);
				let p = SqlitePoolOptions::new()
					.max_connections(1)
					.idle_timeout(None)
					.max_lifetime(None)
					.connect(db_url)
					.await
					.map_err(pallet_revive_eth_rpc::client::ClientError::SqlxError)?;
				(p, Some(config.cache_size))
			} else {
				log::info!(
					target: pallet_revive_eth_rpc::LOG_TARGET,
					"💾 Using persistent receipt DB at: {db_url}",
				);
				(
					SqlitePoolOptions::new()
						.connect(db_url)
						.await
						.map_err(pallet_revive_eth_rpc::client::ClientError::SqlxError)?,
					None,
				)
			};

			let receipt_provider = ReceiptProvider::new(
				pool_db,
				block_provider.clone(),
				receipt_extractor,
				keep_latest_n_blocks,
			)
			.await
			.map_err(pallet_revive_eth_rpc::client::ClientError::SqlxError)?;

			Client::from_native_backend(
				backend,
				block_provider,
				receipt_provider,
				false, // automine
			)
		})
		.map_err(|e: pallet_revive_eth_rpc::client::ClientError| {
			sc_service::Error::Application(Box::new(e) as _)
		})?;

	// Spawn the background block-subscription tasks.
	let client_for_sub = eth_client.clone();
	spawn_handle.spawn(
		"eth-rpc-block-subscription",
		Some("eth-rpc"),
		Box::pin(async move {
			use futures::FutureExt;
			let result = futures::future::try_join_all(vec![
				client_for_sub
					.subscribe_and_cache_new_blocks(SubscriptionType::BestBlocks)
					.boxed(),
				client_for_sub
					.subscribe_and_cache_new_blocks(SubscriptionType::FinalizedBlocks)
					.boxed(),
			])
			.await;

			if let Err(err) = result {
				log::error!(
					target: pallet_revive_eth_rpc::LOG_TARGET,
					"ETH RPC block subscription task failed: {err:?}"
				);
			}
		}),
	);

	build_eth_rpc_module(is_dev, eth_client, config.allow_unprotected_txs)
		.map_err(|e| sc_service::Error::Application(Box::new(e) as _))
}

impl<Block: BlockT, RuntimeApi>
	BuildRpcExtensions<
		ParachainClient<Block, RuntimeApi>,
		ParachainBackend<Block>,
		sc_transaction_pool::TransactionPoolHandle<Block, ParachainClient<Block, RuntimeApi>>,
		sc_statement_store::Store,
	> for BuildAssetHubRpcExtensions<Block, RuntimeApi>
where
	Block: sp_runtime::traits::Block<Hash = sp_core::H256, Extrinsic = sp_runtime::OpaqueExtrinsic>
		+ Send
		+ Sync
		+ 'static,
	Block::Header: sp_runtime::traits::Header<Number = u32, Hash = sp_core::H256> + Send + Sync,
	RuntimeApi:
		ConstructNodeRuntimeApi<Block, ParachainClient<Block, RuntimeApi>> + Send + Sync + 'static,
	RuntimeApi::RuntimeApi: pallet_transaction_payment_rpc::TransactionPaymentRuntimeApi<Block, Balance>
		+ substrate_frame_rpc_system::AccountNonceApi<Block, AccountId, Nonce>
		+ pallet_revive_eth_rpc::native_client::ReviveRuntimeApiT<Block, u64>
		+ sp_api::Core<Block>
		+ sp_api::Metadata<Block>,
{
	fn build_rpc_extensions(
		client: Arc<ParachainClient<Block, RuntimeApi>>,
		backend: Arc<ParachainBackend<Block>>,
		pool: Arc<
			sc_transaction_pool::TransactionPoolHandle<Block, ParachainClient<Block, RuntimeApi>>,
		>,
		statement_store: Option<Arc<sc_statement_store::Store>>,
		spawn_handle: Arc<dyn sp_core::traits::SpawnNamed>,
	) -> sc_service::error::Result<RpcExtension> {
		let build = || -> Result<RpcExtension, Box<dyn std::error::Error + Send + Sync>> {
			let mut module = RpcExtension::new(());

			module.merge(System::new(client.clone(), pool.clone()).into_rpc())?;
			module.merge(TransactionPayment::new(client.clone()).into_rpc())?;
			module.merge(StateMigration::new(client.clone(), backend).into_rpc())?;
			if let Some(statement_store) = statement_store {
				module
					.merge(StatementStore::new(statement_store, spawn_handle.clone()).into_rpc())?;
			}
			module.merge(Dev::new(client.clone()).into_rpc())?;

			// ETH RPC is always present for Asset Hub
			let eth_module = build_revive_eth_rpc_module_native(
				EthRpcConfig::default(),
				client,
				pool,
				false, // is_dev
				spawn_handle,
			)
			.map_err(|e| format!("Failed to build ETH RPC module: {e}"))?;
			module.merge(eth_module)?;

			Ok(module)
		};
		build().map_err(Into::into)
	}
}
