// Copyright 2019-2022 ChainSafe Systems
// SPDX-License-Identifier: Apache-2.0, MIT

use super::cli::set_sigint_handler;
use anyhow::Context;
use dialoguer::{theme::ColorfulTheme, Confirm};
use forest_auth::{create_token, generate_priv_key, ADMIN, JWT_IDENTIFIER};
use forest_chain::ChainStore;
use forest_chain_sync::consensus::SyncGossipSubmitter;
use forest_chain_sync::ChainMuxer;
use forest_cli_shared::cli::{
    db_path, default_snapshot_dir, is_aria2_installed, snapshot_fetch, Client, Config,
    FOREST_VERSION_STRING,
};
use forest_db::Store;
use forest_genesis::{get_network_name_from_genesis, import_chain, read_genesis_header};
use forest_key_management::ENCRYPTED_KEYSTORE_NAME;
use forest_key_management::{KeyStore, KeyStoreConfig};
use forest_libp2p::PeerId;
use forest_libp2p::{ed25519, get_keypair, Keypair, Libp2pConfig, Libp2pService};
use forest_message_pool::{MessagePool, MpoolConfig, MpoolRpcProvider};
use forest_rpc::start_rpc;
use forest_rpc_api::data_types::RPCState;
use forest_state_manager::StateManager;
use forest_utils::io::write_to_file;
use futures::{select, FutureExt};
use fvm_ipld_blockstore::Blockstore;
use fvm_shared::version::NetworkVersion;
use log::{debug, error, info, warn};
use raw_sync::events::{Event, EventInit, EventState};
use rpassword::read_password;
use std::net::TcpListener;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::task::JoinSet;

use std::io::prelude::*;
use std::path::PathBuf;
use std::sync::Arc;
use std::time;

// Initialize Consensus
#[cfg(not(any(feature = "forest_fil_cns", feature = "forest_deleg_cns")))]
compile_error!("No consensus feature enabled; use e.g. `--feature forest_fil_cns` to pick one.");

// Default consensus
#[cfg(all(feature = "forest_fil_cns", not(any(feature = "forest_deleg_cns"))))]
use forest_fil_cns::composition as cns;
// Custom consensus.
#[cfg(feature = "forest_deleg_cns")]
use forest_deleg_cns::composition as cns;

fn unblock_parent_process() -> anyhow::Result<()> {
    let shmem = super::ipc_shmem_conf().open()?;
    let (event, _) =
        unsafe { Event::from_existing(shmem.as_ptr()).map_err(|err| anyhow::anyhow!("{err}")) }?;

    event
        .set(EventState::Signaled)
        .map_err(|err| anyhow::anyhow!("{err}"))
}

/// Starts daemon process
pub(super) async fn start(config: Config, detached: bool) -> anyhow::Result<Db> {
    let mut ctrlc_oneshot = set_sigint_handler();

    info!(
        "Starting Forest daemon, version {}",
        FOREST_VERSION_STRING.as_str()
    );

    let path: PathBuf = config.client.data_dir.join("libp2p");
    let net_keypair = match get_keypair(&path.join("keypair")) {
        Some(keypair) => Ok::<forest_libp2p::Keypair, std::io::Error>(keypair),
        None => {
            let gen_keypair = ed25519::Keypair::generate();
            // Save Ed25519 keypair to file
            // TODO rename old file to keypair.old(?)
            let file = write_to_file(&gen_keypair.encode(), &path, "keypair")?;
            // Restrict permissions on files containing private keys
            forest_utils::io::set_user_perm(&file)?;
            Ok(Keypair::Ed25519(gen_keypair))
        }
    }?;

    // Hint at the multihash which has to go in the `/p2p/<multihash>` part of the peer's multiaddress.
    // Useful if others want to use this node to bootstrap from.
    info!("PeerId: {}", PeerId::from(net_keypair.public()));

    let mut ks = if config.client.encrypt_keystore {
        loop {
            print!("Enter the keystore passphrase: ");
            std::io::stdout().flush()?;

            let passphrase = read_password()?;

            let data_dir = PathBuf::from(&config.client.data_dir).join(ENCRYPTED_KEYSTORE_NAME);
            if !data_dir.exists() {
                print!("Confirm passphrase: ");
                std::io::stdout().flush()?;

                if passphrase != read_password()? {
                    error!("Passphrases do not match. Please retry.");
                    continue;
                }
            }

            let key_store_init_result = KeyStore::new(KeyStoreConfig::Encrypted(
                PathBuf::from(&config.client.data_dir),
                passphrase,
            ));

            match key_store_init_result {
                Ok(ks) => break ks,
                Err(_) => {
                    error!("Incorrect passphrase entered. Please try again.")
                }
            };
        }
    } else {
        warn!("Warning: Keystore encryption disabled!");
        KeyStore::new(KeyStoreConfig::Persistent(PathBuf::from(
            &config.client.data_dir,
        )))?
    };

    if ks.get(JWT_IDENTIFIER).is_err() {
        ks.put(JWT_IDENTIFIER.to_owned(), generate_priv_key())?;
    }

    // Print admin token
    let ki = ks.get(JWT_IDENTIFIER)?;
    let token_exp = config.client.token_exp;
    let token = create_token(ADMIN.to_owned(), ki.private_key(), token_exp)?;
    info!("Admin token: {}", token);

    let keystore = Arc::new(RwLock::new(ks));

    let db = open_db(&config)?;

    let mut services = JoinSet::new();

    {
        // Start Prometheus server port
        let prometheus_listener = TcpListener::bind(config.client.metrics_address).context(
            format!("could not bind to {}", config.client.metrics_address),
        )?;
        info!(
            "Prometheus server started at {}",
            config.client.metrics_address
        );
        let db_directory = db_path(&config);
        let db = db.clone();
        services.spawn(async {
            forest_metrics::init_prometheus(prometheus_listener, db_directory, db)
                .await
                .context("Failed to initiate prometheus server")
        });
    }

    // Initialize ChainStore
    let chain_store = Arc::new(ChainStore::new(db.clone()).await);

    let publisher = chain_store.publisher();

    // Read Genesis file
    // * When snapshot command implemented, this genesis does not need to be initialized
    let genesis = read_genesis_header(
        config.client.genesis_file.as_ref(),
        config.chain.genesis_bytes(),
        &chain_store,
    )
    .await?;
    let genesis_header = &genesis.blocks()[0];
    chain_store.set_genesis(genesis_header)?;

    // XXX: This code has to be run before starting the background services.
    //      If it isn't, several threads will be competing for access to stdout.
    // Terminate if no snapshot is provided or DB isn't recent enough
    let should_fetch_snapshot = match chain_store.heaviest_tipset().await {
        None => prompt_snapshot_or_die(&config).await?,
        Some(tipset) => {
            let epoch = tipset.epoch();
            let nv = config.chain.network_version(epoch);
            if nv < NetworkVersion::V16 {
                prompt_snapshot_or_die(&config).await?
            } else {
                false
            }
        }
    };

    // Reward calculation is needed by the VM to calculate state, which can happen essentially anywhere the `StateManager` is called.
    // It is consensus specific, but threading it through the type system would be a nightmare, which is why dynamic dispatch is used.
    let reward_calc = cns::reward_calc();

    // Initialize StateManager
    let sm = StateManager::new(
        Arc::clone(&chain_store),
        Arc::clone(&config.chain),
        reward_calc,
    )
    .await?;

    let state_manager = Arc::new(sm);

    let network_name = get_network_name_from_genesis(&genesis, &state_manager).await?;

    info!("Using network :: {}", get_actual_chain_name(&network_name));

    let (tipset_sink, tipset_stream) = flume::bounded(20);

    // if bootstrap peers are not set, set them
    let config = if config.network.bootstrap_peers.is_empty() {
        let bootstrap_peers = config
            .chain
            .bootstrap_peers
            .iter()
            .map(|node| node.parse())
            .collect::<Result<_, _>>()?;
        Config {
            network: Libp2pConfig {
                bootstrap_peers,
                ..config.network
            },
            ..config
        }
    } else {
        config
    };

    let genesis_cid = *genesis_header.cid();
    // Libp2p service setup
    let p2p_service = Libp2pService::new(
        config.network.clone(),
        Arc::clone(&chain_store),
        net_keypair,
        &network_name,
        genesis_cid,
    )
    .await;

    let network_rx = p2p_service.network_receiver();
    let network_send = p2p_service.network_sender();

    // Initialize mpool
    let provider = MpoolRpcProvider::new(publisher.clone(), Arc::clone(&state_manager));
    let mpool = MessagePool::new(
        provider,
        network_name.clone(),
        network_send.clone(),
        MpoolConfig::load_config(&db)?,
        Arc::clone(state_manager.chain_config()),
        &mut services,
    )
    .await?;

    let mpool = Arc::new(mpool);

    // For consensus types that do mining, create a component to submit their proposals.
    let submitter = SyncGossipSubmitter::new(
        network_name.clone(),
        network_send.clone(),
        tipset_sink.clone(),
    );

    // Initialize Consensus. Mining may or may not happen, depending on type.
    let consensus =
        cns::consensus(&state_manager, &keystore, &mpool, submitter, &mut services).await?;

    // Initialize ChainMuxer
    let chain_muxer_tipset_sink = tipset_sink.clone();
    let chain_muxer = ChainMuxer::new(
        Arc::new(consensus),
        Arc::clone(&state_manager),
        Arc::clone(&mpool),
        network_send.clone(),
        network_rx,
        Arc::new(genesis),
        chain_muxer_tipset_sink,
        tipset_stream,
        config.sync.clone(),
    )?;
    let bad_blocks = chain_muxer.bad_blocks_cloned();
    let sync_state = chain_muxer.sync_state_cloned();
    services.spawn(async { Err(anyhow::anyhow!("{}", chain_muxer.await)) });

    // Start services
    if config.client.enable_rpc {
        let keystore_rpc = Arc::clone(&keystore);
        let rpc_listen =
            std::net::TcpListener::bind(config.client.rpc_address).context(format!(
                "could not bind to rpc address {}",
                config.client.rpc_address
            ))?;

        let rpc_state_manager = Arc::clone(&state_manager);
        let rpc_chain_store = Arc::clone(&chain_store);

        services.spawn(async move {
            info!("JSON-RPC endpoint started at {}", config.client.rpc_address);
            // XXX: The JSON error message are a nightmare to print.
            start_rpc::<_, _, cns::FullConsensus>(
                Arc::new(RPCState {
                    state_manager: Arc::clone(&rpc_state_manager),
                    keystore: keystore_rpc,
                    mpool,
                    bad_blocks,
                    sync_state,
                    network_send,
                    network_name,
                    beacon: rpc_state_manager.beacon_schedule(), // TODO: the RPCState can fetch this itself from the StateManager
                    chain_store: rpc_chain_store,
                    new_mined_block_tx: tipset_sink,
                }),
                rpc_listen,
                FOREST_VERSION_STRING.as_str(),
            )
            .await
            .map_err(|err| anyhow::anyhow!("{:?}", serde_json::to_string(&err)))
        });
    } else {
        debug!("RPC disabled.");
    };
    if detached {
        unblock_parent_process()?;
    }

    // Fetch and ensure verification keys are downloaded
    if cns::FETCH_PARAMS {
        use forest_paramfetch::{
            get_params_default, set_proofs_parameter_cache_dir_env, SectorSizeOpt,
        };
        set_proofs_parameter_cache_dir_env(&config.client.data_dir);

        get_params_default(&config.client.data_dir, SectorSizeOpt::Keys).await?;
    }

    let config = maybe_fetch_snapshot(should_fetch_snapshot, config).await?;

    select! {
        () = sync_from_snapshot(&config, &state_manager).fuse() => {},
        _ = ctrlc_oneshot => {
            // Cancel all async services
            services.shutdown().await;
            return Ok(db);
        },
    }

    // Halt
    if config.client.halt_after_import {
        // Cancel all async services
        services.shutdown().await;
        return Ok(db);
    }

    services.spawn(p2p_service.run());

    // blocking until any of the services returns an error,
    // or CTRL-C is pressed
    select! {
        err = propagate_error(&mut services).fuse() => error!("services failure: {}", err),
        _ = ctrlc_oneshot => {}
    }

    // Cancel all async services
    services.shutdown().await;

    Ok(db)
}

// returns the first error with which any of the services end
// in case all services finished without an error sleeps for more than 2 years
// and then returns with an error
async fn propagate_error(services: &mut JoinSet<Result<(), anyhow::Error>>) -> anyhow::Error {
    while !services.is_empty() {
        select! {
            option = services.join_next().fuse() => {
                if let Some(Ok(Err(error_message))) = option {
                    return error_message
                }
            },
        }
    }
    // In case all services are down without errors we are still willing
    // to wait indefinitely for CTRL-C signal. As `tokio::time::sleep` has
    // a limit of approximately 2.2 years we have to loop
    loop {
        tokio::time::sleep(Duration::new(64000000, 0)).await;
    }
}

/// Optionally fetches the snapshot. Returns the configuration (modified accordingly if a snapshot was fetched).
async fn maybe_fetch_snapshot(
    should_fetch_snapshot: bool,
    config: Config,
) -> anyhow::Result<Config> {
    if should_fetch_snapshot {
        let snapshot_path = default_snapshot_dir(&config);
        let path = snapshot_fetch(&snapshot_path, &config, &None, is_aria2_installed()).await?;
        Ok(Config {
            client: Client {
                snapshot_path: Some(path),
                snapshot: true,
                ..config.client
            },
            ..config
        })
    } else {
        Ok(config)
    }
}

/// Last resort in case a snapshot is needed. If it is not to be downloaded, this method fails and
/// exits the process.
async fn prompt_snapshot_or_die(config: &Config) -> anyhow::Result<bool> {
    if config.client.snapshot_path.is_some() {
        return Ok(false);
    }
    let should_download = if !config.client.download_snapshot && atty::is(atty::Stream::Stdin) {
        Confirm::with_theme(&ColorfulTheme::default())
                .with_prompt(
                    "Forest needs a snapshot to sync with the network. Would you like to download one now?",
                )
                .default(false)
                .interact()
                .unwrap_or_default()
    } else {
        config.client.download_snapshot
    };

    if should_download {
        Ok(true)
    } else {
        anyhow::bail!("Forest cannot sync without a snapshot. Download a snapshot from a trusted source and import with --import-snapshot=[file] or --download-snapshot to download one automatically");
    }
}

async fn sync_from_snapshot<DB>(config: &Config, state_manager: &Arc<StateManager<DB>>)
where
    DB: Store + Send + Clone + Sync + Blockstore + 'static,
{
    if let Some(path) = &config.client.snapshot_path {
        let stopwatch = time::Instant::now();
        let validate_height = if config.client.snapshot {
            config.client.snapshot_height
        } else {
            Some(0)
        };

        match import_chain::<_>(
            state_manager,
            &path.display().to_string(),
            validate_height,
            config.client.skip_load,
        )
        .await
        {
            Ok(_) => {
                info!("Imported snapshot in: {}s", stopwatch.elapsed().as_secs());
            }
            Err(err) => {
                error!(
                    "Failed miserably while importing chain from snapshot {}: {err}",
                    path.display()
                )
            }
        }
    }
}

fn get_actual_chain_name(internal_network_name: &str) -> &str {
    match internal_network_name {
        "testnetnet" => "mainnet",
        "calibrationnet" => "calibnet",
        _ => internal_network_name,
    }
}

#[cfg(feature = "rocksdb")]
fn open_db(config: &Config) -> anyhow::Result<forest_db::rocks::RocksDb> {
    forest_db::rocks::RocksDb::open(db_path(config), &config.rocks_db).map_err(Into::into)
}

#[cfg(feature = "paritydb")]
fn open_db(config: &Config) -> anyhow::Result<forest_db::parity_db::ParityDb> {
    use forest_db::parity_db::*;
    ParityDb::open(db_path(config), &config.parity_db)
}

#[cfg(feature = "rocksdb")]
type Db = forest_db::rocks::RocksDb;

#[cfg(feature = "paritydb")]
type Db = forest_db::parity_db::ParityDb;

#[cfg(test)]
mod test {
    use super::*;
    use forest_blocks::BlockHeader;
    use forest_db::MemoryDB;
    use forest_networks::ChainConfig;
    use fvm_shared::address::Address;

    #[tokio::test]
    async fn import_snapshot_from_file_valid() -> anyhow::Result<()> {
        anyhow::ensure!(import_snapshot_from_file("test_files/chain4.car")
            .await
            .is_ok());
        Ok(())
    }

    #[tokio::test]
    async fn import_snapshot_from_file_invalid() -> anyhow::Result<()> {
        anyhow::ensure!(import_snapshot_from_file("Cargo.toml").await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn import_snapshot_from_file_not_found() -> anyhow::Result<()> {
        anyhow::ensure!(import_snapshot_from_file("dummy.car").await.is_err());
        Ok(())
    }

    #[cfg(feature = "slow_tests")]
    #[tokio::test]
    async fn import_snapshot_from_url_not_found() -> anyhow::Result<()> {
        anyhow::ensure!(import_snapshot_from_file("https://dummy.com/dummy.car")
            .await
            .is_err());
        Ok(())
    }

    async fn import_snapshot_from_file(file_path: &str) -> anyhow::Result<()> {
        let db = MemoryDB::default();
        let cs = Arc::new(ChainStore::new(db).await);
        let genesis_header = BlockHeader::builder()
            .miner_address(Address::new_id(0))
            .timestamp(7777)
            .build()?;
        cs.set_genesis(&genesis_header)?;
        let chain_config = Arc::new(ChainConfig::default());
        let sm = Arc::new(
            StateManager::new(
                cs,
                chain_config,
                Arc::new(forest_interpreter::RewardActorMessageCalc),
            )
            .await?,
        );
        import_chain::<_>(&sm, file_path, None, false).await?;
        Ok(())
    }

    // FIXME: This car file refers to actors that are not available in FVM yet.
    //        See issue: https://github.com/ChainSafe/forest/issues/1452
    // #[async_std::test]
    // async fn import_chain_from_file() {
    //     let db = Arc::new(MemoryDB::default());
    //     let cs = Arc::new(ChainStore::new(db));
    //     let genesis_header = BlockHeader::builder()
    //         .miner_address(Address::new_id(0))
    //         .timestamp(7777)
    //         .build()
    //         .unwrap();
    //     cs.set_genesis(&genesis_header).unwrap();
    //     let sm = Arc::new(StateManager::new(cs).await.unwrap());
    //     import_chain::<FullVerifier, _>(&sm, "test_files/chain4.car", Some(0), false)
    //         .await
    //         .expect("Failed to import chain");
    // }
}
