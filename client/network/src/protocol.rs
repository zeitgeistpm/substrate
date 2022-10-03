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

use crate::config;

use bytes::Bytes;
use codec::DecodeAll;
use futures::stream::Stream;
use libp2p::{
	core::{connection::ConnectionId, transport::ListenerId, ConnectedPoint},
	swarm::{
		ConnectionHandler, IntoConnectionHandler, NetworkBehaviour, NetworkBehaviourAction,
		PollParameters,
	},
	Multiaddr, PeerId,
};
use log::{debug, error, warn};
use notifications::{Notifications, NotificationsOut};
use prometheus_endpoint::Registry;
use sc_client_api::HeaderBackend;
use sc_network_common::{
	config::NonReservedPeerMode, error, notifications::ProtocolConfig as NotifProtocolConfig,
	protocol::ProtocolName, service::PeerValidationResult, sync::message::Roles, utils::interval,
};
use sp_runtime::traits::Block as BlockT;
use std::{
	collections::{
		hash_map::Entry::{Occupied, Vacant},
		HashMap, HashSet, VecDeque,
	},
	io, iter,
	pin::Pin,
	sync::Arc,
	task::Poll,
	time,
};

pub mod notifications;

pub mod message;

pub use notifications::{NotificationsSink, NotifsHandlerError, Ready};

/// Interval at which we perform time based maintenance
const TICK_TIMEOUT: time::Duration = time::Duration::from_millis(1100);

/// Identifier of the peerset for the block announces protocol.
const HARDCODED_PEERSETS_SYNC: sc_peerset::SetId = sc_peerset::SetId::from(0);
/// Number of hardcoded peersets (the constants right above). Any set whose identifier is equal or
/// superior to this value corresponds to a user-defined protocol.
const NUM_HARDCODED_PEERSETS: usize = 1;

mod rep {
	use sc_peerset::ReputationChange as Rep;
	/// We received a message that failed to decode.
	pub const BAD_MESSAGE: Rep = Rep::new(-(1 << 12), "Bad message");
}

#[derive(Debug, PartialEq, Eq)]
enum PeerStatus {
	OnProbation,
	Accepted,
	Disconnecting,
}

struct NewPeerInfo {
	status: PeerStatus,
	roles: Option<Roles>,
	pending_notifs: VecDeque<(PeerId, Bytes)>,
}

// Lock must always be taken in order declared here.
pub struct Protocol<B: BlockT, Client> {
	/// Interval at which we call `tick`.
	_tick_timeout: Pin<Box<dyn Stream<Item = ()> + Send>>,
	/// Pending list of messages to return from `poll` as a priority.
	pending_messages: VecDeque<CustomMessageOutcome>,
	/// Assigned roles.
	_roles: Roles,
	/// All connected peers. Contains both full and light node peers.
	peers: HashMap<PeerId, NewPeerInfo>,
	_chain: Arc<Client>,
	/// List of nodes for which we perform additional logging because they are important for the
	/// user.
	_important_peers: HashSet<PeerId>,
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
	/// The `PeerId`'s of all boot nodes.
	_boot_node_ids: HashSet<PeerId>,
	block_announces_protocol_name: ProtocolName,
	_marker: std::marker::PhantomData<B>, // TODO: remove
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
		network_config: &config::NetworkConfiguration,
		notifications_protocols_handshakes: Vec<Vec<u8>>,
		_metrics_registry: Option<&Registry>,
		sync_protocol_config: NotifProtocolConfig,
	) -> error::Result<(Self, sc_peerset::PeersetHandle, Vec<(PeerId, Multiaddr)>)> {
		let _boot_node_ids = {
			let mut list = HashSet::new();
			for node in &network_config.boot_nodes {
				list.insert(node.peer_id);
			}
			list.shrink_to_fit();
			list
		};

		// TODO: syncing
		let _important_peers = {
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

		// TODO: move to sync?
		let _default_peers_set_no_slot_peers = {
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

		let block_announces_protocol_name = sync_protocol_config.name.clone();
		let behaviour = {
			Notifications::new(
				peerset,
				iter::once(sync_protocol_config).chain(
					network_config.extra_sets.iter().zip(notifications_protocols_handshakes).map(
						|(s, hs)| NotifProtocolConfig {
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
			_tick_timeout: Box::pin(interval(TICK_TIMEOUT)),
			pending_messages: VecDeque::new(),
			_roles: roles,
			_chain: chain,
			_important_peers,
			peerset_handle: peerset_handle.clone(),
			behaviour,
			notification_protocols: network_config
				.extra_sets
				.iter()
				.map(|s| s.notifications_protocol.clone())
				.collect(),
			bad_handshake_substreams: Default::default(),
			_boot_node_ids,
			peers: Default::default(),
			block_announces_protocol_name,
			_marker: Default::default(),
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

	pub fn set_sync_handshake(&mut self, handshake_message: impl Into<Vec<u8>>) {
		self.behaviour
			.set_notif_protocol_handshake(HARDCODED_PEERSETS_SYNC, handshake_message);
	}

	pub fn disconnect_sync_peer(&mut self, who: PeerId) {
		match self.peers.remove(&who) {
			Some(_) => self.pending_messages.push_back(CustomMessageOutcome::SyncDisconnected(who)),
			None => panic!("trying to disconnect peers who doesn't exist"),
		}
	}

	pub fn write_sync_notification(&mut self, who: PeerId, message: Vec<u8>) {
		self.behaviour.write_notification(&who, HARDCODED_PEERSETS_SYNC, message);
	}

	pub fn write_batch_sync_notification(&mut self, peers: Vec<PeerId>, message: Vec<u8>) {
		for peer in peers {
			self.behaviour
				.write_notification(&peer, HARDCODED_PEERSETS_SYNC, message.clone());
		}
	}

	pub fn report_peer_validation_result(&mut self, peer: PeerId, result: PeerValidationResult) {
		match result {
			PeerValidationResult::Accepted(roles) =>
				if let Some(peer_info) = self.peers.get_mut(&peer) {
					peer_info.status = PeerStatus::Accepted;
					peer_info.roles = Some(roles);

					self.pending_messages.push_back(CustomMessageOutcome::SyncConnected(peer));

					// TODO: optimize
					for (peer_id, message) in
						std::mem::take(&mut peer_info.pending_notifs).into_iter()
					{
						self.pending_messages.push_back(
							CustomMessageOutcome::NotificationsReceived {
								remote: peer_id,
								messages: vec![(
									self.block_announces_protocol_name.clone(),
									message,
								)],
							},
						);
					}
				},
			PeerValidationResult::Rejected => {
				self.peers.remove(&peer);
			},
		}
	}

	/// Adjusts the reputation of a node.
	pub fn report_peer(&self, who: PeerId, reputation: sc_peerset::ReputationChange) {
		self.peerset_handle.report_peer(who, reputation)
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
}

/// Outcome of an incoming custom message.
#[derive(Debug)]
#[must_use]
pub enum CustomMessageOutcome {
	PeerConnected {
		peer_id: PeerId,
		received_handshake: Vec<u8>,
		negotiated_fallback: Option<ProtocolName>,
	},
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
	type OutEvent = CustomMessageOutcome;

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
					match self.peers.entry(peer_id) {
						Vacant(entry) => entry.insert(NewPeerInfo {
							status: PeerStatus::OnProbation,
							roles: None,
							pending_notifs: Default::default(),
						}),
						Occupied(_) => panic!("peer already exists"),
					};

					CustomMessageOutcome::PeerConnected {
						peer_id,
						received_handshake,
						negotiated_fallback,
					}
				} else {
					match (
						Roles::decode_all(&mut &received_handshake[..]),
						self.peers.iter().find(|(id, info)| {
							*id == &peer_id && info.status == PeerStatus::Accepted
						}),
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
						(Err(_), Some((_id, peer))) if received_handshake.is_empty() => {
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
								roles: peer.roles.expect("roles to exist"),
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
					match self.peers.get_mut(&peer_id) {
						Some(peer) => {
							peer.status = PeerStatus::Disconnecting;
							CustomMessageOutcome::NotificationStreamClosed {
								remote: peer_id,
								protocol: self.block_announces_protocol_name.clone(),
							}
						},
						None => panic!("tried to disconnect peer who doesn't not exist"),
					}
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
				HARDCODED_PEERSETS_SYNC => match self.peers.get_mut(&peer_id) {
					Some(peer) =>
						if peer.status == PeerStatus::OnProbation {
							peer.pending_notifs.push_back((peer_id, message.freeze()));
							CustomMessageOutcome::None
						} else {
							CustomMessageOutcome::NotificationsReceived {
								remote: peer_id,
								messages: vec![(
									self.block_announces_protocol_name.clone(),
									message.freeze(),
								)],
							}
						},
					None => panic!("unknown peer"),
				},
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

		if !matches!(outcome, CustomMessageOutcome::None) {
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
