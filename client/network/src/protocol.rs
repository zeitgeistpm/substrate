// This file is part of Substrate.

// Copyright (C) 2017-2022 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

#![allow(unused)]
use crate::{config, sync_helper};

use bytes::Bytes;
use codec::{Decode, DecodeAll, Encode};
use futures::{channel::oneshot, prelude::*, stream::Stream};
use futures_lite::stream::StreamExt;
use libp2p::{
	core::{connection::ConnectionId, transport::ListenerId, ConnectedPoint},
	request_response::OutboundFailure,
	swarm::{
		ConnectionHandler, IntoConnectionHandler, NetworkBehaviour, NetworkBehaviourAction,
		PollParameters,
	},
	Multiaddr, PeerId,
};
use log::{debug, error, info, log, trace, warn, Level};
use message::{
	generic::{Message as GenericMessage, Roles},
	Message,
};
use notifications::{Notifications, NotificationsOut};
use prometheus_endpoint::{register, Gauge, GaugeVec, Opts, PrometheusError, Registry, U64};
use sc_client_api::HeaderBackend;
use sc_consensus::import_queue::{
	BlockImportError, BlockImportStatus, IncomingBlock, RuntimeOrigin,
};
use sc_network_common::{
	config::{NonReservedPeerMode, ProtocolId},
	error,
	protocol::ProtocolName,
	request_responses::RequestFailure,
	sync::{
		message::{
			BlockAnnounce, BlockAttributes, BlockData, BlockRequest, BlockResponse, BlockState,
		},
		warp::{EncodedProof, WarpProofRequest},
		BadPeer, ChainSync, OnBlockData, OnBlockJustification, OnStateData, OpaqueBlockRequest,
		OpaqueBlockResponse, OpaqueStateRequest, OpaqueStateResponse, PollBlockAnnounceValidation,
		SyncStatus,
	},
	utils::{interval, LruHashSet},
};
use sp_arithmetic::traits::SaturatedConversion;
use sp_consensus::BlockOrigin;
use sp_runtime::{
	generic::BlockId,
	traits::{Block as BlockT, CheckedSub, Header as HeaderT, NumberFor, Zero},
	Justifications,
};
use std::{
	collections::{HashMap, HashSet, VecDeque},
	io, iter,
	num::NonZeroUsize,
	pin::Pin,
	sync::Arc,
	task::Poll,
	time,
};

mod notifications;

pub mod message;

pub use notifications::{NotificationsSink, NotifsHandlerError, Ready};

/// Interval at which we perform time based maintenance
const TICK_TIMEOUT: time::Duration = time::Duration::from_millis(1100);

/// Maximum number of known block hashes to keep for a peer.
pub const MAX_KNOWN_BLOCKS: usize = 1024; // ~32kb per peer + LruHashSet overhead
/// Maximum allowed size for a block announce.
pub const MAX_BLOCK_ANNOUNCE_SIZE: u64 = 1024 * 1024;

/// Maximum size used for notifications in the block announce and transaction protocols.
// Must be equal to `max(MAX_BLOCK_ANNOUNCE_SIZE, MAX_TRANSACTIONS_SIZE)`.
pub(crate) const BLOCK_ANNOUNCES_TRANSACTIONS_SUBSTREAM_SIZE: u64 = 16 * 1024 * 1024;

/// Identifier of the peerset for the block announces protocol.
const HARDCODED_PEERSETS_SYNC: sc_peerset::SetId = sc_peerset::SetId::from(0);
/// Number of hardcoded peersets (the constants right above). Any set whose identifier is equal or
/// superior to this value corresponds to a user-defined protocol.
const NUM_HARDCODED_PEERSETS: usize = 1;

/// When light node connects to the full node and the full node is behind light node
/// for at least `LIGHT_MAXIMAL_BLOCKS_DIFFERENCE` blocks, we consider it not useful
/// and disconnect to free connection slot.
pub const LIGHT_MAXIMAL_BLOCKS_DIFFERENCE: u64 = 8192;

pub mod rep {
	use sc_peerset::ReputationChange as Rep;
	/// Reputation change when a peer doesn't respond in time to our messages.
	pub const TIMEOUT: Rep = Rep::new(-(1 << 10), "Request timeout");
	/// Reputation change when a peer refuses a request.
	pub const REFUSED: Rep = Rep::new(-(1 << 10), "Request refused");
	/// Reputation change when we are a light client and a peer is behind us.
	pub const PEER_BEHIND_US_LIGHT: Rep = Rep::new(-(1 << 8), "Useless for a light peer");
	/// We received a message that failed to decode.
	pub const BAD_MESSAGE: Rep = Rep::new(-(1 << 12), "Bad message");
	/// Peer has different genesis.
	pub const GENESIS_MISMATCH: Rep = Rep::new_fatal("Genesis mismatch");
	/// Peer is on unsupported protocol version.
	pub const BAD_PROTOCOL: Rep = Rep::new_fatal("Unsupported protocol");
	/// Peer role does not match (e.g. light peer connecting to another light peer).
	pub const BAD_ROLE: Rep = Rep::new_fatal("Unsupported role");
	/// Peer send us a block announcement that failed at validation.
	pub const BAD_BLOCK_ANNOUNCEMENT: Rep = Rep::new(-(1 << 12), "Bad block announcement");
}

struct Metrics {
	peers: Gauge<U64>,
	queued_blocks: Gauge<U64>,
	fork_targets: Gauge<U64>,
	justifications: GaugeVec<U64>,
}

impl Metrics {
	fn register(r: &Registry) -> Result<Self, PrometheusError> {
		Ok(Self {
			peers: {
				let g = Gauge::new("substrate_sync_peers", "Number of peers we sync with")?;
				register(g, r)?
			},
			queued_blocks: {
				let g =
					Gauge::new("substrate_sync_queued_blocks", "Number of blocks in import queue")?;
				register(g, r)?
			},
			fork_targets: {
				let g = Gauge::new("substrate_sync_fork_targets", "Number of fork sync targets")?;
				register(g, r)?
			},
			justifications: {
				let g = GaugeVec::new(
					Opts::new(
						"substrate_sync_extra_justifications",
						"Number of extra justifications requests",
					),
					&["status"],
				)?;
				register(g, r)?
			},
		})
	}
}

// Lock must always be taken in order declared here.
pub struct Protocol<B: BlockT, Client> {
	/// Interval at which we call `tick`.
	tick_timeout: Pin<Box<dyn Stream<Item = ()> + Send>>,
	/// Pending list of messages to return from `poll` as a priority.
	pending_messages: VecDeque<CustomMessageOutcome<B>>,
	/// Assigned roles.
	roles: Roles,
	genesis_hash: B::Hash,
	// temporary object that performs all Protocol's syncing-related functionality
	// sync_helper: sync_helper::SyncingHelper<B, Client>,
	// All connected peers. Contains both full and light node peers.
	// peers: HashMap<PeerId, Peer<B>>,
	chain: Arc<Client>,
	/// List of nodes for which we perform additional logging because they are important for the
	/// user.
	important_peers: HashSet<PeerId>,
	/// Used to report reputation changes.
	peerset_handle: sc_peerset::PeersetHandle,
	/// Handles opening the unique substream and sending and receiving raw messages.
	behaviour: Notifications,
	/// List of notifications protocols that have been registered.
	notification_protocols: Vec<ProtocolName>,
	/// If we receive a new "substream open" event that contains an invalid handshake, we ask the
	/// inner layer to force-close the substream. Force-closing the substream will generate a
	/// "substream closed" event. This is a problem: since we can't propagate the "substream open"
	/// event to the outer layers, we also shouldn't propagate this "substream closed" event. To
	/// solve this, an entry is added to this map whenever an invalid handshake is received.
	/// Entries are removed when the corresponding "substream closed" is later received.
	bad_handshake_substreams: HashSet<(PeerId, sc_peerset::SetId)>,
	/// Prometheus metrics.
	metrics: Option<Metrics>,
	/// The `PeerId`'s of all boot nodes.
	boot_node_ids: HashSet<PeerId>,
	sync_handle: sync_helper::SyncingHandle<B>,
	counter: usize,
}

#[derive(Debug)]
pub enum PeerRequest<B: BlockT> {
	Block(BlockRequest<B>),
	State,
	WarpProof,
}

/// Peer information
// TODO: remove clone when no longer needed
#[derive(Debug, Clone)]
pub struct Peer<B: BlockT> {
	pub info: PeerInfo<B>,
	/// Holds a set of blocks known to this peer.
	pub known_blocks: LruHashSet<B::Hash>,
}

/// Info about a peer's known state.
#[derive(Clone, Debug)]
pub struct PeerInfo<B: BlockT> {
	/// Roles
	pub roles: Roles,
	/// Peer best block hash
	pub best_hash: B::Hash,
	/// Peer best block number
	pub best_number: <B::Header as HeaderT>::Number,
}

/// Handshake sent when we open a block announces substream.
#[derive(Debug, PartialEq, Eq, Clone, Encode, Decode)]
pub struct BlockAnnouncesHandshake<B: BlockT> {
	/// Roles of the node.
	pub roles: Roles,
	/// Best block number.
	pub best_number: NumberFor<B>,
	/// Best block hash.
	pub best_hash: B::Hash,
	/// Genesis block hash.
	pub genesis_hash: B::Hash,
}

impl<B: BlockT> BlockAnnouncesHandshake<B> {
	fn build(
		roles: Roles,
		best_number: NumberFor<B>,
		best_hash: B::Hash,
		genesis_hash: B::Hash,
	) -> Self {
		Self { genesis_hash, roles, best_number, best_hash }
	}
}

impl<B, Client> Protocol<B, Client>
where
	B: BlockT,
	Client: HeaderBackend<B> + 'static,
{
	/// Create a new instance.
	pub fn new(
		roles: Roles,
		chain: Arc<Client>,
		protocol_id: ProtocolId,
		fork_id: &Option<String>,
		network_config: &config::NetworkConfiguration,
		notifications_protocols_handshakes: Vec<Vec<u8>>,
		metrics_registry: Option<&Registry>,
		sync_handle: sync_helper::SyncingHandle<B>,
	) -> error::Result<(Self, sc_peerset::PeersetHandle, Vec<(PeerId, Multiaddr)>)> {
		let info = chain.info();

		let boot_node_ids = {
			let mut list = HashSet::new();
			for node in &network_config.boot_nodes {
				list.insert(node.peer_id);
			}
			list.shrink_to_fit();
			list
		};

		let important_peers = {
			let mut imp_p = HashSet::new();
			for reserved in &network_config.default_peers_set.reserved_nodes {
				imp_p.insert(reserved.peer_id);
			}
			for reserved in network_config
				.extra_sets
				.iter()
				.flat_map(|s| s.set_config.reserved_nodes.iter())
			{
				imp_p.insert(reserved.peer_id);
			}
			imp_p.shrink_to_fit();
			imp_p
		};

		let default_peers_set_no_slot_peers = {
			let mut no_slot_p: HashSet<PeerId> = network_config
				.default_peers_set
				.reserved_nodes
				.iter()
				.map(|reserved| reserved.peer_id)
				.collect();
			no_slot_p.shrink_to_fit();
			no_slot_p
		};

		let mut known_addresses = Vec::new();

		let (peerset, peerset_handle) = {
			let mut sets =
				Vec::with_capacity(NUM_HARDCODED_PEERSETS + network_config.extra_sets.len());

			let mut default_sets_reserved = HashSet::new();
			for reserved in network_config.default_peers_set.reserved_nodes.iter() {
				default_sets_reserved.insert(reserved.peer_id);

				if !reserved.multiaddr.is_empty() {
					known_addresses.push((reserved.peer_id, reserved.multiaddr.clone()));
				}
			}

			let mut bootnodes = Vec::with_capacity(network_config.boot_nodes.len());
			for bootnode in network_config.boot_nodes.iter() {
				bootnodes.push(bootnode.peer_id);
			}

			// Set number 0 is used for block announces.
			sets.push(sc_peerset::SetConfig {
				in_peers: network_config.default_peers_set.in_peers,
				out_peers: network_config.default_peers_set.out_peers,
				bootnodes,
				reserved_nodes: default_sets_reserved.clone(),
				reserved_only: network_config.default_peers_set.non_reserved_mode ==
					NonReservedPeerMode::Deny,
			});

			for set_cfg in &network_config.extra_sets {
				let mut reserved_nodes = HashSet::new();
				for reserved in set_cfg.set_config.reserved_nodes.iter() {
					reserved_nodes.insert(reserved.peer_id);
					known_addresses.push((reserved.peer_id, reserved.multiaddr.clone()));
				}

				let reserved_only =
					set_cfg.set_config.non_reserved_mode == NonReservedPeerMode::Deny;

				sets.push(sc_peerset::SetConfig {
					in_peers: set_cfg.set_config.in_peers,
					out_peers: set_cfg.set_config.out_peers,
					bootnodes: Vec::new(),
					reserved_nodes,
					reserved_only,
				});
			}

			sc_peerset::Peerset::from_config(sc_peerset::PeersetConfig { sets })
		};

		let block_announces_protocol = {
			let genesis_hash =
				chain.hash(0u32.into()).ok().flatten().expect("Genesis block exists; qed");
			let genesis_hash = genesis_hash.as_ref();
			if let Some(fork_id) = fork_id {
				format!(
					"/{}/{}/block-announces/1",
					array_bytes::bytes2hex("", genesis_hash),
					fork_id
				)
			} else {
				format!("/{}/block-announces/1", array_bytes::bytes2hex("", genesis_hash))
			}
		};

		let legacy_ba_protocol_name = format!("/{}/block-announces/1", protocol_id.as_ref());

		let behaviour = {
			let best_number = info.best_number;
			let best_hash = info.best_hash;
			let genesis_hash = info.genesis_hash;

			let block_announces_handshake =
				BlockAnnouncesHandshake::<B>::build(roles, best_number, best_hash, genesis_hash)
					.encode();

			let sync_protocol_config = notifications::ProtocolConfig {
				name: block_announces_protocol.into(),
				fallback_names: iter::once(legacy_ba_protocol_name.into()).collect(),
				handshake: block_announces_handshake,
				max_notification_size: MAX_BLOCK_ANNOUNCE_SIZE,
			};

			Notifications::new(
				peerset,
				iter::once(sync_protocol_config).chain(
					network_config.extra_sets.iter().zip(notifications_protocols_handshakes).map(
						|(s, hs)| notifications::ProtocolConfig {
							name: s.notifications_protocol.clone(),
							fallback_names: s.fallback_names.clone(),
							handshake: hs,
							max_notification_size: s.max_notification_size,
						},
					),
				),
			)
		};

		let protocol = Self {
			tick_timeout: Box::pin(interval(TICK_TIMEOUT)),
			pending_messages: VecDeque::new(),
			roles,
			chain,
			genesis_hash: info.genesis_hash,
			important_peers,
			peerset_handle: peerset_handle.clone(),
			behaviour,
			notification_protocols: network_config
				.extra_sets
				.iter()
				.map(|s| s.notifications_protocol.clone())
				.collect(),
			bad_handshake_substreams: Default::default(),
			metrics: if let Some(r) = metrics_registry {
				Some(Metrics::register(r)?)
			} else {
				None
			},
			boot_node_ids,
			sync_handle,
			counter: Default::default(),
		};

		Ok((protocol, peerset_handle, known_addresses))
	}

	/// Returns the list of all the peers we have an open channel to.
	pub fn open_peers(&self) -> impl Iterator<Item = &PeerId> {
		self.behaviour.open_peers()
	}

	/// Returns the number of discovered nodes that we keep in memory.
	pub fn num_discovered_peers(&self) -> usize {
		self.behaviour.num_discovered_peers()
	}

	/// Disconnects the given peer if we are connected to it.
	pub fn disconnect_peer(&mut self, peer_id: &PeerId, protocol_name: ProtocolName) {
		if let Some(position) = self.notification_protocols.iter().position(|p| *p == protocol_name)
		{
			self.behaviour.disconnect_peer(
				peer_id,
				sc_peerset::SetId::from(position + NUM_HARDCODED_PEERSETS),
			);
		} else {
			warn!(target: "sub-libp2p", "disconnect_peer() with invalid protocol name")
		}
	}

	/// Returns the state of the peerset manager, for debugging purposes.
	pub fn peerset_debug_info(&mut self) -> serde_json::Value {
		self.behaviour.peerset_debug_info()
	}

	/// Returns the number of peers we're connected to and that are being queried.
	pub fn num_active_peers(&self) -> usize {
		0usize
		// TODO: reimplement this using something
		// self.sync_helper.pending_responses.len()
		// self.peers.values().filter(|p| p.request.is_some()).count()
	}

	/// Inform sync about new best imported block.
	pub fn new_best_block_imported(&mut self, hash: B::Hash, number: NumberFor<B>) {
		debug!(target: "sync", "New best block imported {:?}/#{}", hash, number);

		self.sync_handle.update_chain_info(hash, number);

		self.behaviour.set_notif_protocol_handshake(
			HARDCODED_PEERSETS_SYNC,
			BlockAnnouncesHandshake::<B>::build(self.roles, number, hash, self.genesis_hash)
				.encode(),
		);
	}

	/// Returns information about all the peers we are connected to after the handshake message.
	pub fn peers_info(&self) -> Vec<(PeerId, PeerInfo<B>)> {
		futures::executor::block_on(self.sync_handle.get_peers())
			.iter()
			.map(|(id, peer)| (*id, peer.info.clone())) // TODO: fix
			.collect()
	}

	/// Adjusts the reputation of a node.
	pub fn report_peer(&self, who: PeerId, reputation: sc_peerset::ReputationChange) {
		self.peerset_handle.report_peer(who, reputation)
	}

	// /// Perform time based maintenance.
	// ///
	// /// > **Note**: This method normally doesn't have to be called except for testing purposes.
	// pub fn tick(&mut self) {
	// 	self.report_metrics()
	// }

	// TODO: move to `SyncingHelper`
	/// Make sure an important block is propagated to peers.
	///
	/// In chain-based consensus, we often need to make sure non-best forks are
	/// at least temporarily synced.
	pub fn announce_block(&mut self, hash: B::Hash, data: Option<Vec<u8>>) {
		let header = match self.chain.header(BlockId::Hash(hash)) {
			Ok(Some(header)) => header,
			Ok(None) => {
				warn!("Trying to announce unknown block: {}", hash);
				return
			},
			Err(e) => {
				warn!("Error reading block header {}: {}", hash, e);
				return
			},
		};

		// don't announce genesis block since it will be ignored
		if header.number().is_zero() {
			return
		}

		let is_best = self.chain.info().best_hash == hash;
		debug!(target: "sync", "Reannouncing block {:?} is_best: {}", hash, is_best);

		let data = data
			.or_else(|| futures::executor::block_on(self.sync_handle.get_annouce_data(hash)))
			.unwrap_or_default();

		for (who, peer) in futures::executor::block_on(self.sync_handle.get_peers()) {
			let inserted =
				futures::executor::block_on(self.sync_handle.insert_known_block(who, hash));
			if inserted {
				trace!(target: "sync", "Announcing block {:?} to {}", hash, who);
				let message = BlockAnnounce {
					header: header.clone(),
					state: if is_best { Some(BlockState::Best) } else { Some(BlockState::Normal) },
					data: Some(data.clone()),
				};

				// println!("send block annoucement!");

				self.behaviour
					.write_notification(&who, HARDCODED_PEERSETS_SYNC, message.encode());
			}
		}
	}

	// TODO: move to `SyncingHelper`
	/// A batch of blocks have been processed, with or without errors.
	/// Call this when a batch of blocks have been processed by the importqueue, with or without
	/// errors.
	pub fn on_blocks_processed(
		&mut self,
		imported: usize,
		count: usize,
		results: Vec<(Result<BlockImportStatus<NumberFor<B>>, BlockImportError>, B::Hash)>,
	) {
		let messages = futures::executor::block_on(
			self.sync_handle.on_blocks_processed(imported, count, results),
		);
		self.pending_messages.extend(messages);
	}

	/// Set whether the syncing peers set is in reserved-only mode.
	pub fn set_reserved_only(&self, reserved_only: bool) {
		self.peerset_handle.set_reserved_only(HARDCODED_PEERSETS_SYNC, reserved_only);
	}

	/// Removes a `PeerId` from the list of reserved peers for syncing purposes.
	pub fn remove_reserved_peer(&self, peer: PeerId) {
		self.peerset_handle.remove_reserved_peer(HARDCODED_PEERSETS_SYNC, peer);
	}

	/// Returns the list of reserved peers.
	pub fn reserved_peers(&self) -> impl Iterator<Item = &PeerId> {
		self.behaviour.reserved_peers(HARDCODED_PEERSETS_SYNC)
	}

	/// Adds a `PeerId` to the list of reserved peers for syncing purposes.
	pub fn add_reserved_peer(&self, peer: PeerId) {
		self.peerset_handle.add_reserved_peer(HARDCODED_PEERSETS_SYNC, peer);
	}

	/// Sets the list of reserved peers for syncing purposes.
	pub fn set_reserved_peers(&self, peers: HashSet<PeerId>) {
		self.peerset_handle.set_reserved_peers(HARDCODED_PEERSETS_SYNC, peers);
	}

	/// Sets the list of reserved peers for the given protocol/peerset.
	pub fn set_reserved_peerset_peers(&self, protocol: ProtocolName, peers: HashSet<PeerId>) {
		if let Some(index) = self.notification_protocols.iter().position(|p| *p == protocol) {
			self.peerset_handle
				.set_reserved_peers(sc_peerset::SetId::from(index + NUM_HARDCODED_PEERSETS), peers);
		} else {
			error!(
				target: "sub-libp2p",
				"set_reserved_peerset_peers with unknown protocol: {}",
				protocol
			);
		}
	}

	/// Removes a `PeerId` from the list of reserved peers.
	pub fn remove_set_reserved_peer(&self, protocol: ProtocolName, peer: PeerId) {
		if let Some(index) = self.notification_protocols.iter().position(|p| *p == protocol) {
			self.peerset_handle.remove_reserved_peer(
				sc_peerset::SetId::from(index + NUM_HARDCODED_PEERSETS),
				peer,
			);
		} else {
			error!(
				target: "sub-libp2p",
				"remove_set_reserved_peer with unknown protocol: {}",
				protocol
			);
		}
	}

	/// Adds a `PeerId` to the list of reserved peers.
	pub fn add_set_reserved_peer(&self, protocol: ProtocolName, peer: PeerId) {
		if let Some(index) = self.notification_protocols.iter().position(|p| *p == protocol) {
			self.peerset_handle
				.add_reserved_peer(sc_peerset::SetId::from(index + NUM_HARDCODED_PEERSETS), peer);
		} else {
			error!(
				target: "sub-libp2p",
				"add_set_reserved_peer with unknown protocol: {}",
				protocol
			);
		}
	}

	/// Notify the protocol that we have learned about the existence of nodes on the default set.
	///
	/// Can be called multiple times with the same `PeerId`s.
	pub fn add_default_set_discovered_nodes(&mut self, peer_ids: impl Iterator<Item = PeerId>) {
		for peer_id in peer_ids {
			self.peerset_handle.add_to_peers_set(HARDCODED_PEERSETS_SYNC, peer_id);
		}
	}

	/// Add a peer to a peers set.
	pub fn add_to_peers_set(&self, protocol: ProtocolName, peer: PeerId) {
		if let Some(index) = self.notification_protocols.iter().position(|p| *p == protocol) {
			self.peerset_handle
				.add_to_peers_set(sc_peerset::SetId::from(index + NUM_HARDCODED_PEERSETS), peer);
		} else {
			error!(
				target: "sub-libp2p",
				"add_to_peers_set with unknown protocol: {}",
				protocol
			);
		}
	}

	/// Remove a peer from a peers set.
	pub fn remove_from_peers_set(&self, protocol: ProtocolName, peer: PeerId) {
		if let Some(index) = self.notification_protocols.iter().position(|p| *p == protocol) {
			self.peerset_handle.remove_from_peers_set(
				sc_peerset::SetId::from(index + NUM_HARDCODED_PEERSETS),
				peer,
			);
		} else {
			error!(
				target: "sub-libp2p",
				"remove_from_peers_set with unknown protocol: {}",
				protocol
			);
		}
	}

	// /// Encode implementation-specific block request.
	// pub fn encode_block_request(&mut self, request: OpaqueBlockRequest) -> Result<Vec<u8>,
	// String> { 	futures::executor::block_on(self.sync_handle.encode_block_request(request))
	// }

	// /// Encode implementation-specific state request.
	// pub fn encode_state_request(&mut self, request: OpaqueStateRequest) -> Result<Vec<u8>,
	// String> { 	futures::executor::block_on(self.sync_handle.encode_state_request(request))
	// }

	// // TODO: move to syncing
	// fn report_metrics(&self) {
	// 	if let Some(metrics) = &self.metrics {
	// 		let n = u64::try_from(self.peers.len()).unwrap_or(std::u64::MAX);
	// 		metrics.peers.set(n);
	// 		let m = self.sync_helper.chain_sync.metrics();
	// 		metrics.fork_targets.set(m.fork_targets.into());
	// 		metrics.queued_blocks.set(m.queued_blocks.into());
	// 		metrics
	// 			.justifications
	// 			.with_label_values(&["pending"])
	// 			.set(m.justifications.pending_requests.into());
	// 		metrics
	// 			.justifications
	// 			.with_label_values(&["active"])
	// 			.set(m.justifications.active_requests.into());
	// 		metrics
	// 			.justifications
	// 			.with_label_values(&["failed"])
	// 			.set(m.justifications.failed_requests.into());
	// 		metrics
	// 			.justifications
	// 			.with_label_values(&["importing"])
	// 			.set(m.justifications.importing_requests.into());
	// 	}
	// }
}

/// Outcome of an incoming custom message.
#[derive(Debug)]
#[must_use]
pub enum CustomMessageOutcome<B: BlockT> {
	BlockImport(BlockOrigin, Vec<IncomingBlock<B>>),
	JustificationImport(RuntimeOrigin, B::Hash, NumberFor<B>, Justifications),
	/// Notification protocols have been opened with a remote.
	NotificationStreamOpened {
		remote: PeerId,
		protocol: ProtocolName,
		/// See [`crate::Event::NotificationStreamOpened::negotiated_fallback`].
		negotiated_fallback: Option<ProtocolName>,
		roles: Roles,
		notifications_sink: NotificationsSink,
	},
	/// The [`NotificationsSink`] of some notification protocols need an update.
	NotificationStreamReplaced {
		remote: PeerId,
		protocol: ProtocolName,
		notifications_sink: NotificationsSink,
	},
	/// Notification protocols have been closed with a remote.
	NotificationStreamClosed {
		remote: PeerId,
		protocol: ProtocolName,
	},
	/// Messages have been received on one or more notifications protocols.
	NotificationsReceived {
		remote: PeerId,
		messages: Vec<(ProtocolName, Bytes)>,
	},
	/// A new block request must be emitted.
	BlockRequest {
		target: PeerId,
		request: OpaqueBlockRequest,
		pending_response: oneshot::Sender<Result<Vec<u8>, RequestFailure>>,
	},
	/// A new storage request must be emitted.
	StateRequest {
		target: PeerId,
		request: OpaqueStateRequest,
		pending_response: oneshot::Sender<Result<Vec<u8>, RequestFailure>>,
	},
	/// A new warp sync request must be emitted.
	WarpSyncRequest {
		target: PeerId,
		request: WarpProofRequest<B>,
		pending_response: oneshot::Sender<Result<Vec<u8>, RequestFailure>>,
	},
	/// Now connected to a new peer for syncing purposes.
	SyncConnected(PeerId),
	/// No longer connected to a peer for syncing purposes.
	SyncDisconnected(PeerId),
	None,
}

impl<B, Client> NetworkBehaviour for Protocol<B, Client>
where
	B: BlockT,
	Client: HeaderBackend<B> + 'static,
{
	type ConnectionHandler = <Notifications as NetworkBehaviour>::ConnectionHandler;
	type OutEvent = CustomMessageOutcome<B>;

	fn new_handler(&mut self) -> Self::ConnectionHandler {
		self.behaviour.new_handler()
	}

	fn addresses_of_peer(&mut self, peer_id: &PeerId) -> Vec<Multiaddr> {
		self.behaviour.addresses_of_peer(peer_id)
	}

	fn inject_connection_established(
		&mut self,
		peer_id: &PeerId,
		conn: &ConnectionId,
		endpoint: &ConnectedPoint,
		failed_addresses: Option<&Vec<Multiaddr>>,
		other_established: usize,
	) {
		self.behaviour.inject_connection_established(
			peer_id,
			conn,
			endpoint,
			failed_addresses,
			other_established,
		)
	}

	fn inject_connection_closed(
		&mut self,
		peer_id: &PeerId,
		conn: &ConnectionId,
		endpoint: &ConnectedPoint,
		handler: <Self::ConnectionHandler as IntoConnectionHandler>::Handler,
		remaining_established: usize,
	) {
		self.behaviour.inject_connection_closed(
			peer_id,
			conn,
			endpoint,
			handler,
			remaining_established,
		)
	}

	fn inject_event(
		&mut self,
		peer_id: PeerId,
		connection: ConnectionId,
		event: <<Self::ConnectionHandler as IntoConnectionHandler>::Handler as ConnectionHandler>::OutEvent,
	) {
		self.behaviour.inject_event(peer_id, connection, event)
	}

	fn poll(
		&mut self,
		cx: &mut std::task::Context,
		params: &mut impl PollParameters,
	) -> Poll<NetworkBehaviourAction<Self::OutEvent, Self::ConnectionHandler>> {
		if let Some(message) = self.pending_messages.pop_front() {
			return Poll::Ready(NetworkBehaviourAction::GenerateEvent(message))
		}

		let events = futures::executor::block_on(self.sync_handle.get_events());
		self.pending_messages.extend(events);

		// while let Poll::Ready(Some(())) = self.tick_timeout.poll_next_unpin(cx) {
		// 	self.tick();
		// }

		if let Some(message) = self.pending_messages.pop_front() {
			return Poll::Ready(NetworkBehaviourAction::GenerateEvent(message))
		}

		let event = match self.behaviour.poll(cx, params) {
			Poll::Pending => return Poll::Pending,
			Poll::Ready(NetworkBehaviourAction::GenerateEvent(ev)) => ev,
			Poll::Ready(NetworkBehaviourAction::Dial { opts, handler }) =>
				return Poll::Ready(NetworkBehaviourAction::Dial { opts, handler }),
			Poll::Ready(NetworkBehaviourAction::NotifyHandler { peer_id, handler, event }) =>
				return Poll::Ready(NetworkBehaviourAction::NotifyHandler {
					peer_id,
					handler,
					event,
				}),
			Poll::Ready(NetworkBehaviourAction::ReportObservedAddr { address, score }) =>
				return Poll::Ready(NetworkBehaviourAction::ReportObservedAddr { address, score }),
			Poll::Ready(NetworkBehaviourAction::CloseConnection { peer_id, connection }) =>
				return Poll::Ready(NetworkBehaviourAction::CloseConnection { peer_id, connection }),
		};

		let outcome = match event {
			NotificationsOut::CustomProtocolOpen {
				peer_id,
				set_id,
				received_handshake,
				notifications_sink,
				negotiated_fallback,
			} => {
				// TODO: remove hardcoded peerset entry
				// Set number 0 is hardcoded the default set of peers we sync from.
				if set_id == HARDCODED_PEERSETS_SYNC {
					let mut events =
						futures::executor::block_on(self.sync_handle.custom_protocol_open(
							peer_id,
							received_handshake,
							notifications_sink,
							negotiated_fallback,
						));

					// TODO: beyond hideous, remove
					if events.len() > 1 {
						self.pending_messages
							.push_back(events.pop_front().expect("event to exist"));
					}

					events.pop_front().expect("event to exist")
				} else {
					match (
						message::Roles::decode_all(&mut &received_handshake[..]),
						futures::executor::block_on(self.sync_handle.get_peers())
							.iter()
							.find(|(id, _)| id == &peer_id), // TODO: fix
					) {
						(Ok(roles), _) => CustomMessageOutcome::NotificationStreamOpened {
							remote: peer_id,
							protocol: self.notification_protocols
								[usize::from(set_id) - NUM_HARDCODED_PEERSETS]
								.clone(),
							negotiated_fallback,
							roles,
							notifications_sink,
						},
						(Err(_), Some((id, peer))) if received_handshake.is_empty() => {
							// As a convenience, we allow opening substreams for "external"
							// notification protocols with an empty handshake. This fetches the
							// roles from the locally-known roles.
							// TODO: remove this after https://github.com/paritytech/substrate/issues/5685
							CustomMessageOutcome::NotificationStreamOpened {
								remote: peer_id,
								protocol: self.notification_protocols
									[usize::from(set_id) - NUM_HARDCODED_PEERSETS]
									.clone(),
								negotiated_fallback,
								roles: peer.info.roles,
								notifications_sink,
							}
						},
						(Err(err), _) => {
							debug!(target: "sync", "Failed to parse remote handshake: {}", err);
							self.bad_handshake_substreams.insert((peer_id, set_id));
							self.behaviour.disconnect_peer(&peer_id, set_id);
							self.peerset_handle.report_peer(peer_id, rep::BAD_MESSAGE);
							CustomMessageOutcome::None
						},
					}
				}
			},
			NotificationsOut::CustomProtocolReplaced { peer_id, notifications_sink, set_id } =>
			// TODO: remove hardcoded peerset entry
				if set_id == HARDCODED_PEERSETS_SYNC ||
					self.bad_handshake_substreams.contains(&(peer_id, set_id))
				{
					CustomMessageOutcome::None
				} else {
					CustomMessageOutcome::NotificationStreamReplaced {
						remote: peer_id,
						protocol: self.notification_protocols
							[usize::from(set_id) - NUM_HARDCODED_PEERSETS]
							.clone(),
						notifications_sink,
					}
				},
			NotificationsOut::CustomProtocolClosed { peer_id, set_id } => {
				// Set number 0 is hardcoded the default set of peers we sync from.
				// TODO: remove hardcoded peerset entry
				if set_id == HARDCODED_PEERSETS_SYNC {
					futures::executor::block_on(self.sync_handle.custom_protocol_close(peer_id))
				} else if self.bad_handshake_substreams.remove(&(peer_id, set_id)) {
					// The substream that has just been closed had been opened with a bad
					// handshake. The outer layers have never received an opening event about this
					// substream, and consequently shouldn't receive a closing event either.
					CustomMessageOutcome::None
				} else {
					CustomMessageOutcome::NotificationStreamClosed {
						remote: peer_id,
						protocol: self.notification_protocols
							[usize::from(set_id) - NUM_HARDCODED_PEERSETS]
							.clone(),
					}
				}
			},
			NotificationsOut::Notification { peer_id, set_id, message } => match set_id {
				HARDCODED_PEERSETS_SYNC =>
					futures::executor::block_on(self.sync_handle.notification(peer_id, message)),
				_ if self.bad_handshake_substreams.contains(&(peer_id, set_id)) =>
					CustomMessageOutcome::None,
				_ => {
					let protocol_name = self.notification_protocols
						[usize::from(set_id) - NUM_HARDCODED_PEERSETS]
						.clone();
					CustomMessageOutcome::NotificationsReceived {
						remote: peer_id,
						messages: vec![(protocol_name, message.freeze())],
					}
				},
			},
		};

		if !matches!(outcome, CustomMessageOutcome::<B>::None) {
			return Poll::Ready(NetworkBehaviourAction::GenerateEvent(outcome))
		}

		if let Some(message) = self.pending_messages.pop_front() {
			return Poll::Ready(NetworkBehaviourAction::GenerateEvent(message))
		}

		// This block can only be reached if an event was pulled from the behaviour and that
		// resulted in `CustomMessageOutcome::None`. Since there might be another pending
		// message from the behaviour, the task is scheduled again.
		cx.waker().wake_by_ref();
		Poll::Pending
	}

	fn inject_dial_failure(
		&mut self,
		peer_id: Option<PeerId>,
		handler: Self::ConnectionHandler,
		error: &libp2p::swarm::DialError,
	) {
		self.behaviour.inject_dial_failure(peer_id, handler, error);
	}

	fn inject_new_listener(&mut self, id: ListenerId) {
		self.behaviour.inject_new_listener(id)
	}

	fn inject_new_listen_addr(&mut self, id: ListenerId, addr: &Multiaddr) {
		self.behaviour.inject_new_listen_addr(id, addr)
	}

	fn inject_expired_listen_addr(&mut self, id: ListenerId, addr: &Multiaddr) {
		self.behaviour.inject_expired_listen_addr(id, addr)
	}

	fn inject_new_external_addr(&mut self, addr: &Multiaddr) {
		self.behaviour.inject_new_external_addr(addr)
	}

	fn inject_expired_external_addr(&mut self, addr: &Multiaddr) {
		self.behaviour.inject_expired_external_addr(addr)
	}

	fn inject_listener_error(&mut self, id: ListenerId, err: &(dyn std::error::Error + 'static)) {
		self.behaviour.inject_listener_error(id, err);
	}

	fn inject_listener_closed(&mut self, id: ListenerId, reason: Result<(), &io::Error>) {
		self.behaviour.inject_listener_closed(id, reason);
	}
}
