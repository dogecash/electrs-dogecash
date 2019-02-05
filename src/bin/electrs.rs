extern crate bitcoin;
extern crate error_chain;
#[macro_use]
extern crate log;

extern crate electrs_syscoin;

use error_chain::ChainedError;
use std::process;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use electrs_syscoin::{
    config::Config,
    daemon::Daemon,
    errors::*,
    metrics::Metrics,
    new_index::{ChainQuery, FetchFrom, Indexer, Mempool, Query, Store},
    rest,
    signal::Waiter,
};

fn fetch_from(config: &Config, store: &Store) -> FetchFrom {
    let mut jsonrpc_import = config.jsonrpc_import;
    if !jsonrpc_import {
        jsonrpc_import = !store.is_empty();
    }
    match jsonrpc_import {
        true => FetchFrom::Bitcoind, // slower, uses JSONRPC (good for incremental updates)
        false => FetchFrom::BlkFiles, // faster, uses blk*.dat files (good for initial indexing)
    }
}

fn finish_verification(daemon: &Daemon, signal: &Waiter) -> Result<()> {
    loop {
        let progress = daemon.getblockchaininfo()?.verificationprogress;
        if progress > 0.9999 {
            return Ok(());
        }
        warn!(
            "waiting for verification to finish: {:.3}%",
            progress * 100.0
        );
        signal.wait(Duration::from_secs(5))?;
    }
}

fn run_server(config: Arc<Config>) -> Result<()> {
    let signal = Waiter::new();
    let metrics = Metrics::new(config.monitoring_addr);
    metrics.start();

    let daemon = Arc::new(Daemon::new(
        &config.daemon_dir,
        config.daemon_rpc_addr,
        config.cookie_getter(),
        config.network_type,
        signal.clone(),
        &metrics,
    )?);
    finish_verification(&daemon, &signal)?;
    let store = Arc::new(Store::open(&config.db_path.join("newindex")));
    let mut indexer = Indexer::open(Arc::clone(&store), fetch_from(&config, &store), &metrics);
    let mut tip = indexer.update(&daemon)?;

    let chain = Arc::new(ChainQuery::new(Arc::clone(&store), &metrics));
    let mempool = Arc::new(RwLock::new(Mempool::new(Arc::clone(&chain), &metrics)));
    mempool.write().unwrap().update(&daemon)?;
    let query = Arc::new(Query::new(Arc::clone(&chain), Arc::clone(&mempool)));

    let server = rest::run_server(config, query, Arc::clone(&daemon));

    loop {
        if let Err(err) = signal.wait(Duration::from_secs(5)) {
            info!("stopping server: {}", err);
            server.stop();
            break;
        }
        let current_tip = daemon.getbestblockhash()?;
        if current_tip != tip {
            indexer.update(&daemon)?;
            tip = current_tip;
        };
        mempool.write().unwrap().update(&daemon)?;
    }
    info!("server stopped");
    Ok(())
}

fn main() {
    let config = Arc::new(Config::from_args());
    if let Err(e) = run_server(config) {
        error!("server failed: {}", e.display_chain());
        process::exit(1);
    }
}
