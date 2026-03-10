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
//! The Ethereum JSON-RPC server.
use crate::{
	BlockInfoProvider, DebugRpcServer, DebugRpcServerImpl, EthRpcServer, EthRpcServerImpl,
	LOG_TARGET, PolkadotRpcServer, PolkadotRpcServerImpl, ReceiptExtractor, ReceiptProvider,
	SubstrateClientT, SystemHealthRpcServer, SystemHealthRpcServerImpl,
	client::{Client, SubscriptionType, SubstrateBlockNumber},
};
use clap::Parser;
use futures::{FutureExt, future::BoxFuture, pin_mut};
use jsonrpsee::server::RpcModule;
use sc_cli::{PrometheusParams, RpcParams, SharedParams, Signals};
use sc_service::{
	TaskManager,
	config::{PrometheusConfig, RpcConfiguration},
	start_rpc_servers,
};
use sqlx::sqlite::SqlitePoolOptions;

// Default port if --prometheus-port is not specified
const DEFAULT_PROMETHEUS_PORT: u16 = 9616;

// Default port if --rpc-port is not specified
const DEFAULT_RPC_PORT: u16 = 8545;

const IN_MEMORY_DB: &str = "sqlite::memory:";

// Parsed command instructions from the command line
#[derive(Parser, Debug)]
#[clap(author, about, version)]
pub struct CliCommand {
	/// The maximum number of blocks to cache in memory.
	#[clap(long, default_value = "256")]
	pub cache_size: usize,

	/// Earliest block number to consider when searching for transaction receipts.
	#[clap(long)]
	pub earliest_receipt_block: Option<SubstrateBlockNumber>,

	/// The database used to store Ethereum transaction hashes.
	/// This is only useful if the node needs to act as an archive node and respond to Ethereum RPC
	/// queries for transactions that are not in the in memory cache.
	#[clap(long, env = "DATABASE_URL", default_value = IN_MEMORY_DB)]
	pub database_url: String,

	/// If provided, index the last n blocks
	#[clap(long)]
	pub index_last_n_blocks: Option<SubstrateBlockNumber>,

	#[allow(missing_docs)]
	#[clap(flatten)]
	pub shared_params: SharedParams,

	#[allow(missing_docs)]
	#[clap(flatten)]
	pub rpc_params: RpcParams,

	#[allow(missing_docs)]
	#[clap(flatten)]
	pub prometheus_params: PrometheusParams,

	/// By default, the node rejects any transaction that's unprotected (i.e., that doesn't have a
	/// chain-id). If the user wishes to submit such a transaction they can use this flag to
	/// instruct the RPC to ignore this check.
	#[arg(long)]
	pub allow_unprotected_txs: bool,
}

/// Initialize the logger
#[cfg(not(test))]
fn init_logger(params: &SharedParams) -> anyhow::Result<()> {
	let mut logger = sc_cli::LoggerBuilder::new(params.log_filters().join(","));
	logger
		.with_log_reloading(params.enable_log_reloading)
		.with_detailed_output(params.detailed_log_output);

	if let Some(tracing_targets) = &params.tracing_targets {
		let tracing_receiver = params.tracing_receiver.into();
		logger.with_profiling(tracing_receiver, tracing_targets);
	}

	if params.disable_log_color {
		logger.with_colors(false);
	}

	logger.init()?;
	Ok(())
}

/// Build the list of dev accounts to inject when `--dev` is passed.
fn dev_accounts() -> Vec<crate::Account> {
	#[cfg(feature = "subxt")]
	{
		vec![
			crate::Account::from(subxt_signer::eth::dev::alith()),
			crate::Account::from(subxt_signer::eth::dev::baltathar()),
			crate::Account::from(subxt_signer::eth::dev::charleth()),
			crate::Account::from(subxt_signer::eth::dev::dorothy()),
			crate::Account::from(subxt_signer::eth::dev::ethan()),
		]
	}
	#[cfg(not(feature = "subxt"))]
	{
		log::warn!(
			target: LOG_TARGET,
			"⚠️  --dev mode: the `subxt` feature is disabled, no dev accounts are pre-loaded. \
			 Enable `subxt` or inject accounts manually via EthRpcServerImpl::with_accounts."
		);
		vec![]
	}
}

/// Create the JSON-RPC module from a `Client`.
pub fn build_eth_rpc_module<C, BP>(
	is_dev: bool,
	client: Client<C, BP>,
	allow_unprotected_txs: bool,
) -> Result<RpcModule<()>, sc_service::Error>
where
	C: SubstrateClientT,
	BP: BlockInfoProvider,
{
	let eth_api = EthRpcServerImpl::new(client.clone())
		.with_accounts(if is_dev { dev_accounts() } else { vec![] })
		.with_allow_unprotected_txs(allow_unprotected_txs)
		.with_use_pending_for_estimate_gas(is_dev)
		.into_rpc();

	let health_api = SystemHealthRpcServerImpl::new(client.clone()).into_rpc();
	let debug_api = DebugRpcServerImpl::new(client.clone()).into_rpc();
	let polkadot_api = PolkadotRpcServerImpl::new(client).into_rpc();

	let mut module = RpcModule::new(());
	module.merge(eth_api).map_err(|e| sc_service::Error::Application(e.into()))?;
	module.merge(health_api).map_err(|e| sc_service::Error::Application(e.into()))?;
	module.merge(debug_api).map_err(|e| sc_service::Error::Application(e.into()))?;
	module
		.merge(polkadot_api)
		.map_err(|e| sc_service::Error::Application(e.into()))?;
	Ok(module)
}

/// Build an in-memory `Client` from a [`SubstrateClientT`] and [`BlockInfoProvider`].
pub async fn build_client_from_backend<C, BP>(
	backend: C,
	block_provider: BP,
	database_url: &str,
	cache_size: usize,
	earliest_receipt_block: Option<SubstrateBlockNumber>,
	automine: bool,
) -> anyhow::Result<Client<C, BP>>
where
	C: SubstrateClientT,
	BP: BlockInfoProvider,
{
	let receipt_extractor =
		ReceiptExtractor::new_from_substrate_client(backend.clone(), earliest_receipt_block);

	let (pool, keep_latest_n_blocks) = if database_url == IN_MEMORY_DB {
		log::warn!(
			target: LOG_TARGET,
			"💾 Using in-memory database, keeping only {cache_size} blocks in memory"
		);
		let pool = SqlitePoolOptions::new()
			.max_connections(1)
			.idle_timeout(None)
			.max_lifetime(None)
			.connect(database_url)
			.await?;
		(pool, Some(cache_size))
	} else {
		(SqlitePoolOptions::new().connect(database_url).await?, None)
	};

	let receipt_provider =
		ReceiptProvider::new(pool, block_provider.clone(), receipt_extractor, keep_latest_n_blocks)
			.await?;

	let client = Client::from_backend(backend, block_provider, receipt_provider, automine)?;
	Ok(client)
}

/// Start the JSON-RPC server using a pre-built native client.
pub fn run_with_native_client<C, BP>(
	cmd: CliCommand,
	backend: C,
	block_provider: BP,
) -> anyhow::Result<()>
where
	C: SubstrateClientT,
	BP: BlockInfoProvider,
{
	let CliCommand {
		rpc_params,
		prometheus_params,
		cache_size,
		database_url,
		earliest_receipt_block,
		index_last_n_blocks,
		shared_params,
		allow_unprotected_txs,
		..
	} = cmd;

	#[cfg(not(test))]
	init_logger(&shared_params)?;

	let is_dev = shared_params.dev;
	let rpc_addrs: Option<Vec<sc_service::config::RpcEndpoint>> = rpc_params
		.rpc_addr(is_dev, false, 8545)?
		.map(|addrs| addrs.into_iter().map(Into::into).collect());

	let rpc_config = RpcConfiguration {
		addr: rpc_addrs,
		methods: rpc_params.rpc_methods.into(),
		max_connections: rpc_params.rpc_max_connections,
		cors: rpc_params.rpc_cors(is_dev)?,
		max_request_size: rpc_params.rpc_max_request_size,
		max_response_size: rpc_params.rpc_max_response_size,
		id_provider: None,
		max_subs_per_conn: rpc_params.rpc_max_subscriptions_per_connection,
		port: rpc_params.rpc_port.unwrap_or(DEFAULT_RPC_PORT),
		message_buffer_capacity: rpc_params.rpc_message_buffer_capacity_per_connection,
		batch_config: rpc_params.rpc_batch_config()?,
		rate_limit: rpc_params.rpc_rate_limit,
		rate_limit_whitelisted_ips: rpc_params.rpc_rate_limit_whitelisted_ips,
		rate_limit_trust_proxy_headers: rpc_params.rpc_rate_limit_trust_proxy_headers,
		request_logger_limit: if is_dev { 1024 * 1024 } else { 1024 },
	};

	let prometheus_config =
		prometheus_params.prometheus_config(DEFAULT_PROMETHEUS_PORT, "eth-rpc".into());
	let prometheus_registry = prometheus_config.as_ref().map(|config| &config.registry);

	let tokio_runtime = sc_cli::build_runtime()?;
	let tokio_handle = tokio_runtime.handle();
	let mut task_manager = TaskManager::new(tokio_handle.clone(), prometheus_registry)?;

	// Build the client synchronously inside the tokio runtime.
	let client = {
		let fut = build_client_from_backend(
			backend,
			block_provider,
			&database_url,
			cache_size,
			earliest_receipt_block,
			false, // automine
		)
		.fuse();
		pin_mut!(fut);

		let abort_signal = tokio_runtime.block_on(async { Signals::capture() })?;
		match tokio_handle.block_on(abort_signal.try_until_signal(fut)) {
			Ok(Ok(c)) => c,
			Ok(Err(e)) => return Err(e),
			Err(_) => anyhow::bail!("Process interrupted"),
		}
	};

	// Prometheus metrics.
	if let Some(PrometheusConfig { port, registry }) = prometheus_config.clone() {
		task_manager.spawn_handle().spawn(
			"prometheus-endpoint",
			None,
			prometheus_endpoint::init_prometheus(port, registry).map(drop),
		);
	}

	let rpc_server_handle = start_rpc_servers(
		&rpc_config,
		prometheus_registry,
		tokio_handle,
		|| build_eth_rpc_module(is_dev, client.clone(), allow_unprotected_txs),
		None,
	)?;

	task_manager
		.spawn_essential_handle()
		.spawn("block-subscription", None, async move {
			let mut futures: Vec<BoxFuture<'_, Result<(), crate::client::ClientError>>> = vec![
				Box::pin(client.subscribe_and_cache_new_blocks(SubscriptionType::BestBlocks)),
				Box::pin(client.subscribe_and_cache_new_blocks(SubscriptionType::FinalizedBlocks)),
			];

			if let Some(index_last_n_blocks) = index_last_n_blocks {
				futures.push(Box::pin(client.subscribe_and_cache_blocks(index_last_n_blocks)));
			}

			if let Err(err) = futures::future::try_join_all(futures).await {
				panic!("Block subscription task failed: {err:?}")
			}
		});

	task_manager.keep_alive(rpc_server_handle);
	let signals = tokio_runtime.block_on(async { Signals::capture() })?;
	tokio_runtime.block_on(signals.run_until_signal(task_manager.future().fuse()))?;
	Ok(())
}

/// Run the ETH RPC server using a `subxt` WebSocket connection to a Substrate node.
#[cfg(feature = "subxt")]
pub fn run(cmd: CliCommand) -> anyhow::Result<()> {
	use crate::{
		SubxtBlockInfoProvider,
		subxt_client::{SubxtClient, connect},
	};

	let CliCommand {
		rpc_params,
		prometheus_params,
		cache_size,
		database_url,
		earliest_receipt_block,
		index_last_n_blocks,
		shared_params,
		allow_unprotected_txs,
		..
	} = cmd.clone();

	#[cfg(not(test))]
	init_logger(&shared_params)?;

	let is_dev = shared_params.dev;

	let node_rpc_url = rpc_params
		.rpc_addr(is_dev, false, 9944)?
		.and_then(|addrs| addrs.into_iter().next())
		.map(|addr| format!("ws://{}:{}", addr.addr.ip(), addr.addr.port()))
		.unwrap_or_else(|| "ws://127.0.0.1:9944".to_string());

	let rpc_addrs: Option<Vec<sc_service::config::RpcEndpoint>> = cmd
		.rpc_params
		.rpc_addr(is_dev, false, DEFAULT_RPC_PORT)?
		.map(|addrs| addrs.into_iter().map(Into::into).collect());

	let rpc_config = RpcConfiguration {
		addr: rpc_addrs,
		methods: cmd.rpc_params.rpc_methods.into(),
		max_connections: cmd.rpc_params.rpc_max_connections,
		cors: cmd.rpc_params.rpc_cors(is_dev)?,
		max_request_size: cmd.rpc_params.rpc_max_request_size,
		max_response_size: cmd.rpc_params.rpc_max_response_size,
		id_provider: None,
		max_subs_per_conn: cmd.rpc_params.rpc_max_subscriptions_per_connection,
		port: cmd.rpc_params.rpc_port.unwrap_or(DEFAULT_RPC_PORT),
		message_buffer_capacity: cmd.rpc_params.rpc_message_buffer_capacity_per_connection,
		batch_config: cmd.rpc_params.rpc_batch_config()?,
		rate_limit: cmd.rpc_params.rpc_rate_limit,
		rate_limit_whitelisted_ips: cmd.rpc_params.rpc_rate_limit_whitelisted_ips,
		rate_limit_trust_proxy_headers: cmd.rpc_params.rpc_rate_limit_trust_proxy_headers,
		request_logger_limit: if is_dev { 1024 * 1024 } else { 1024 },
	};

	let prometheus_config =
		prometheus_params.prometheus_config(DEFAULT_PROMETHEUS_PORT, "eth-rpc".into());
	let prometheus_registry = prometheus_config.as_ref().map(|config| &config.registry);

	let tokio_runtime = sc_cli::build_runtime()?;
	let tokio_handle = tokio_runtime.handle();
	let mut task_manager = TaskManager::new(tokio_handle.clone(), prometheus_registry)?;

	let (client, block_provider) = {
		let fut = async {
			let (api, rpc_client, rpc) =
				connect(&node_rpc_url, rpc_config.max_request_size, rpc_config.max_response_size)
					.await?;

			let subxt_client = SubxtClient::new(api.clone(), rpc_client, rpc.clone()).await?;
			let block_provider = SubxtBlockInfoProvider::new(api, rpc).await?;

			let client = build_client_from_backend(
				subxt_client,
				block_provider.clone(),
				&database_url,
				cache_size,
				earliest_receipt_block,
				false, // automine not supported in standalone mode
			)
			.await?;

			Ok::<_, anyhow::Error>((client, block_provider))
		}
		.fuse();

		futures::pin_mut!(fut);
		let abort_signal = tokio_runtime.block_on(async { sc_cli::Signals::capture() })?;
		match tokio_handle.block_on(abort_signal.try_until_signal(fut)) {
			Ok(Ok(result)) => result,
			Ok(Err(e)) => return Err(e),
			Err(_) => anyhow::bail!("Process interrupted during startup"),
		}
	};

	// Prometheus metrics.
	if let Some(sc_service::config::PrometheusConfig { port, registry }) = prometheus_config.clone()
	{
		task_manager.spawn_handle().spawn(
			"prometheus-endpoint",
			None,
			prometheus_endpoint::init_prometheus(port, registry).map(drop),
		);
	}

	let rpc_server_handle = start_rpc_servers(
		&rpc_config,
		prometheus_registry,
		tokio_handle,
		|| build_eth_rpc_module(is_dev, client.clone(), allow_unprotected_txs),
		None,
	)?;

	task_manager
		.spawn_essential_handle()
		.spawn("block-subscription", None, async move {
			let mut futures: Vec<BoxFuture<'_, Result<(), crate::client::ClientError>>> = vec![
				Box::pin(client.subscribe_and_cache_new_blocks(SubscriptionType::BestBlocks)),
				Box::pin(client.subscribe_and_cache_new_blocks(SubscriptionType::FinalizedBlocks)),
			];

			if let Some(index_last_n_blocks) = index_last_n_blocks {
				futures.push(Box::pin(client.subscribe_and_cache_blocks(index_last_n_blocks)));
			}

			if let Err(err) = futures::future::try_join_all(futures).await {
				panic!("Block subscription task failed: {err:?}")
			}
		});

	task_manager.keep_alive(rpc_server_handle);
	let signals = tokio_runtime.block_on(async { sc_cli::Signals::capture() })?;
	tokio_runtime.block_on(signals.run_until_signal(task_manager.future().fuse()))?;
	Ok(())
}

/// Fallback when the `subxt` feature is disabled.
#[cfg(not(feature = "subxt"))]
pub fn run(_cmd: CliCommand) -> anyhow::Result<()> {
	anyhow::bail!(
		"The standalone `eth-rpc` binary requires the `subxt` feature. \
		 Rebuild with `--features subxt` or embed the server using `run_with_native_client`."
	)
}
