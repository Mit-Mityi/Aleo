// Copyright (C) 2019-2022 Aleo Systems Inc.
// This file is part of the snarkOS library.

// The snarkOS library is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// The snarkOS library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with the snarkOS library. If not, see <https://www.gnu.org/licenses/>.

mod cache;
mod circular_map;
mod handshake;
mod router;

use cache::Cache;
pub use router::{PeerMeta, Router};

use snarkos_account::Account;
use snarkos_node_consensus::Consensus;
use snarkos_node_executor::{NodeType, RawStatus};
use snarkos_node_ledger::Ledger;
use snarkos_node_messages::{Message, UnconfirmedBlock};
use snarkos_node_network::Network;
use snarkos_node_rest::Rest;
use snarkvm::prelude::{Block, ConsensusStorage, Network as CurrentNetwork, PrivateKey};

use anyhow::Result;
use core::time::Duration;
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicU64},
        Arc,
    },
};

#[derive(Clone)]
pub struct Beacon<N: CurrentNetwork, C: ConsensusStorage<N>> {
    /// The account of the .
    account: Account<N>,
    /// The consensus module of the node.
    consensus: Consensus<N, C>,
    /// The ledger of the node.
    ledger: Ledger<N, C>,
    /// The router of the node.
    router: Router,
    /// The cache of network data seen by the node.
    cache: Cache<N>,
    /// The REST server of the node.
    rest: Option<Arc<Rest<N, C>>>,
    /// The time it to generate a block.
    block_generation_time: Arc<AtomicU64>,
    /// The node's current state.
    status: RawStatus,
    /// The shutdown signal.
    shutdown: Arc<AtomicBool>,
}

impl<N: CurrentNetwork, C: ConsensusStorage<N>> Beacon<N, C> {
    const NODE_TYPE: NodeType = NodeType::Beacon;

    pub async fn new(
        node_ip: SocketAddr,
        rest_ip: Option<SocketAddr>,
        private_key: PrivateKey<N>,
        trusted_peers: &[SocketAddr],
        genesis: Option<Block<N>>,
        dev: Option<u16>,
    ) -> Result<Self> {
        // Initialize the node account.
        let account = Account::from(private_key)?;
        // Initialize the ledger.
        let ledger = Ledger::load(genesis, dev)?;
        // Initialize the consensus.
        let consensus = Consensus::new(ledger.clone())?;
        // Initialize the block generation time.
        let block_generation_time = Arc::new(AtomicU64::new(2));
        // Initialize the node.
        let node = Self {
            account,
            consensus,
            ledger,
            router: Router::new().await,
            cache: Cache::new(),
            rest: None,
            block_generation_time,
            status: RawStatus::new(),
            shutdown: Default::default(),
        };

        // Enable the node's protocols.
        node.enable_handshake().await;
        node.enable_reading().await;
        node.enable_writing().await;
        node.enable_disconnect().await;

        // Initialize the block production.
        // node.initialize_block_production().await;
        // Initialize the signal handler.
        // node.handle_signals();
        // Return the node.
        Ok(node)
    }

    /// Returns the ledger.
    pub fn ledger(&self) -> &Ledger<N, C> {
        &self.ledger
    }

    /// Returns the REST server.
    pub fn rest(&self) -> &Option<Arc<Rest<N, C>>> {
        &self.rest
    }

    pub fn router(&self) -> &Router {
        &self.router
    }

    pub fn cache(&self) -> &Cache<N> {
        &self.cache
    }

    fn status(&self) -> &RawStatus {
        &self.status
    }
}

/* Network traits */

// use snarkos_node_messages::{MessageOrBytes, NoiseCodec, NoiseState, PeerRequest};
use snarkos_node_messages::{MessageCodec, PeerRequest};
use snarkos_node_network::{
    protocols::{Disconnect, Handshake as Handshaking, Reading, Writing},
    ConnectionSide,
    P2P,
};

use std::io;

use rand::{
    prelude::{IteratorRandom, SliceRandom},
    rngs::OsRng,
};

const HEARTBEAT_IN_SECS: u64 = 9;
const MAXIMUM_NUMBER_OF_PEERS: usize = 21;
const MINIMUM_NUMBER_OF_PEERS: usize = 1;

impl<N: CurrentNetwork, C: ConsensusStorage<N>> Beacon<N, C> {
    pub async fn start_periodic_tasks(&self) {
        let node = self.clone();
        // TODO(nkls): task accounting.
        tokio::spawn(async move {
            loop {
                node.heartbeat().await;
                // Sleep for `Self::HEARTBEAT_IN_SECS` seconds.
                tokio::time::sleep(Duration::from_secs(HEARTBEAT_IN_SECS)).await;
            }
        });
    }

    pub async fn heartbeat(&self) {
        // tl;dr:
        // 1. ensure min-max peers (disconnect, peer requests to trusted peers, attempting
        //    connections).
        // 2. ensure trusted peers are connected.
        // 3. ensure only one beacon is connected.

        // Ensure the node has less than MAX PEERS. This shouldn't be necessary as this is checked
        // in the network upon connection but might as well sanity check it here.
        let num_excess_peers = self.network().num_connected().saturating_sub(MAXIMUM_NUMBER_OF_PEERS);
        if num_excess_peers > 0 {
            debug!("Exceeded maximum number of connected peers, disconnecting from {num_excess_peers} peers");

            for peer_addr in self
                .network()
                .connected_addrs()
                .into_iter()
                .filter(|peer_addr| !self.router().trusted_peers().contains(peer_addr))
                .take(num_excess_peers)
            {
                info!("Disconnecting from 'peer' {peer_addr}");

                let _disconnected = self.network().disconnect(peer_addr).await;
                debug_assert!(_disconnected);
            }
        }

        // Ensure the node is only connected to one beacon.
        let connected_beacons = self.router().connected_beacons();
        let num_excess_beacons = connected_beacons.len().saturating_sub(1);
        if num_excess_beacons > 0 {
            debug!("Exceeded maximum number of connected beacons by {num_excess_beacons}");

            for beacon_addr in connected_beacons.into_iter().choose_multiple(&mut OsRng::default(), num_excess_beacons)
            {
                info!("Disconnecting from 'beacon' {beacon_addr}");

                let _disconnected = self.network().disconnect(beacon_addr).await;
                debug_assert!(_disconnected);
            }
        }

        // Ensure the trusted peers are connected.
        for trusted_peer_addr in self.router().trusted_peers().iter() {
            if !self.network().is_connected(*trusted_peer_addr) {
                info!("Connecting to 'trusted peer' {trusted_peer_addr}");

                // Silence the error if there is any, this isn't a halting case.
                let _connected = self.network().connect(*trusted_peer_addr).await;
                debug_assert!(_connected.is_ok());
            }
        }

        // Ensure the node has more peers than MIN PEERS.
        let num_connected = self.network().num_connected();
        let num_missing_peers = MINIMUM_NUMBER_OF_PEERS.saturating_sub(num_connected);

        if num_missing_peers > 0 {
            for candidate_addr in self.router().candidate_peers().into_iter().take(num_missing_peers) {
                let connection_succesful = self.network().connect(candidate_addr).await.is_ok();
                self.router().remove_candidate_peer(candidate_addr);

                if !connection_succesful {
                    self.router().insert_restricted_peer(candidate_addr)
                }
            }

            // If we have existing peers, request more addresses from them.
            if num_connected > 0 {
                for peer_addr in self.network().connected_addrs().choose_multiple(&mut OsRng::default(), 3) {
                    // Let the error through for now.
                    let _res = self.unicast(*peer_addr, Message::PeerRequest(PeerRequest));
                    debug_assert!(_res.expect("writing protocol should be enabled").await.is_ok());
                }
            }
        }
    }

    async fn process_unconfirmed_block(&self, source: SocketAddr, message: UnconfirmedBlock<N>) -> anyhow::Result<()> {
        let message_clone = message.clone();

        // If the block has been seen before, don't deserialise or propagate the block.
        if !self.cache().insert_seen_block(message.block_hash) {
            return Ok(());
        }

        // Perform the deferred non-blocking deserialisation of the block.
        let block = message.block.deserialize().await?;

        if message.block_height != block.height() || message.block_hash != block.hash() {
            anyhow::bail!("deserialized block doesn't match the 'UnconfirmedBlock' header")
        }

        // Propagate the block to all connected peers except the source.
        for peer_addr in self.router().network().connected_addrs() {
            if peer_addr == source {
                continue;
            }

            // Block data shouldn't need to be reserialised as we're sending the serialised copy.
            // TODO(nkls): handling errors here is not crucial but would be nice to have.
            let _res = self.unicast(peer_addr, Message::UnconfirmedBlock(message_clone.clone()));
        }

        Ok(())
    }
}

impl<N: CurrentNetwork, C: ConsensusStorage<N>> P2P for Beacon<N, C> {
    fn network(&self) -> &Network {
        self.router().network()
    }
}

#[async_trait::async_trait]
impl<N: CurrentNetwork, C: ConsensusStorage<N>> Reading for Beacon<N, C> {
    type Codec = MessageCodec<N>;
    type Message = Message<N>;

    fn codec(&self, addr: SocketAddr, _side: ConnectionSide) -> Self::Codec {
        Default::default()
    }

    async fn process_message(&self, source: SocketAddr, message: Self::Message) -> io::Result<()> {
        let result = match message {
            // Protocol violation, should disconnect.
            Message::BlockRequest(_)
            | Message::BlockResponse(_)
            | Message::ChallengeRequest(_)
            | Message::ChallengeResponse(_)
            | Message::PuzzleRequest(_)
            | Message::PuzzleResponse(_) => {
                Err(anyhow::anyhow!("peer sent a message that isn't handled by the beacon"))
            }

            // Valid messages for a beacon to receive.
            Message::Ping(ping) => todo!(),
            Message::Pong(pong) => todo!(),

            Message::UnconfirmedBlock(unconfirmed_block) => {
                self.process_unconfirmed_block(source, unconfirmed_block).await
            }
            Message::UnconfirmedSolution(unconfirmed_solution) => todo!(),
            Message::UnconfirmedTransaction(unconfirmed_transaction) => todo!(),

            _ => todo!(),
        };

        if let Err(err) = result {
            warn!("disconnecting '{source}' for the following reason: {err}");

            // TODO(nkls): this can likely be unified in the router.
            if let Some(meta) = self.router().remove_peer(source) {
                self.router().insert_restricted_peer(meta.listening_addr());
            }

            let _res = self.router().network().disconnect(source).await;
            debug_assert!(_res);
        }

        Ok(())
    }
}

#[async_trait::async_trait]
impl<N: CurrentNetwork, C: ConsensusStorage<N>> Writing for Beacon<N, C> {
    type Codec = MessageCodec<N>;
    type Message = Message<N>;

    fn codec(&self, addr: SocketAddr, _side: ConnectionSide) -> Self::Codec {
        Default::default()
    }
}

#[async_trait::async_trait]
impl<N: CurrentNetwork, C: ConsensusStorage<N>> Disconnect for Beacon<N, C> {
    async fn handle_disconnect(&self, _addr: SocketAddr) {
        // TODO(nkls): update appropriate peer collections
    }
}