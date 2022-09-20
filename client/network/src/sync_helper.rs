#![allow(unused)]
use crate::{config, protocol::*};

use bytes::Bytes;
use codec::{Decode, DecodeAll, Encode};
use futures::{
	channel::{mpsc, oneshot},
	prelude::*,
	stream::{FuturesUnordered, Stream},
};
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
use prometheus_endpoint::{register, Gauge, GaugeVec, Opts, PrometheusError, Registry, U64};
use sc_client_api::HeaderBackend;
use sc_consensus::import_queue::{BlockImportError, BlockImportStatus, IncomingBlock};
use sc_network_common::{
	config::ProtocolId,
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
};
use sc_peerset::ReputationChange;
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

// TODO: zzz
pub type PendingResponse<B> =
	(PeerId, PeerRequest<B>, Result<Result<Vec<u8>, RequestFailure>, oneshot::Canceled>);

// TODO: move chainsync here
pub struct SyncingHelper<B: BlockT, Client> {
	pub pending_responses:
		FuturesUnordered<Pin<Box<dyn Future<Output = PendingResponse<B>> + Send>>>,

	/// State machine that handles the list of in-progress requests. Only full node peers are
	/// registered.
	chain_sync: Box<dyn ChainSync<B>>,

	/// A cache for the data that was associated to a block announcement.
	pub block_announce_data_cache: lru::LruCache<B::Hash, Vec<u8>>,

	/// Genesis hash
	pub genesis_hash: B::Hash,

	/// Blockchain client
	pub chain: Arc<Client>,

	/// Set of all peers
	pub peers: HashMap<PeerId, Peer<B>>,

	pub roles: Roles,

	/// Value that was passed as part of the configuration. Used to cap the number of full nodes.
	default_peers_set_num_full: usize,

	/// List of nodes that should never occupy peer slots.
	default_peers_set_no_slot_peers: HashSet<PeerId>,

	/// Actual list of connected no-slot nodes.
	default_peers_set_no_slot_connected_peers: HashSet<PeerId>,

	/// Number of slots to allocate to light nodes.
	default_peers_set_num_light: usize,

	rx: mpsc::Receiver<SyncEvent<B>>,
}

pub enum SyncEvent<B: BlockT> {
	NumConnectedPeers,
	SyncState(oneshot::Sender<SyncStatus<B>>),
	BestSeenBlock(oneshot::Sender<Option<NumberFor<B>>>),
	NumSyncPeers(oneshot::Sender<u32>),
	NumQueuedBlocks(oneshot::Sender<u32>),
	NumDownloadedBlocks(oneshot::Sender<usize>),
	NumSyncRequests(oneshot::Sender<usize>),
	UpdateChainInfo,
	PeersInfo(oneshot::Sender<Vec<Peer<B>>>),
	GetBlockAnnounceData(B::Hash, oneshot::Sender<Vec<u8>>),
	OnBlockFinalized(B::Hash, B::Header),
	RequestJustification(B::Hash, NumberFor<B>),
	ClearJustificationRequests,
	SetSyncForkRequest(Vec<PeerId>, B::Hash, NumberFor<B>),
	OnBlocksProcessed(
		usize,
		usize,
		Vec<(Result<BlockImportStatus<NumberFor<B>>, BlockImportError>, B::Hash)>,
		Vec<CustomMessageOutcome<B>>,
	),
	EncodeBlockRequest(OpaqueBlockRequest, oneshot::Sender<Result<Vec<u8>, String>>),
	EncodeStateRequest(OpaqueStateRequest, oneshot::Sender<Result<Vec<u8>, String>>),
}

#[derive(Clone)]
pub struct SyncingHandle<B: BlockT> {
	tx: mpsc::Sender<SyncEvent<B>>,
}

impl<B: BlockT> SyncingHandle<B> {
	pub fn new(tx: mpsc::Sender<SyncEvent<B>>) -> Self {
		Self { tx }
	}
}

pub trait SyncingInterface {}

impl<B: BlockT> SyncingInterface for SyncingHandle<B> {}

impl<B: BlockT, Client: HeaderBackend<B> + 'static> SyncingHelper<B, Client> {
	pub fn new(
		chain_sync: Box<dyn ChainSync<B>>,
		cache_size: usize,
		genesis_hash: B::Hash,
		chain: Arc<Client>,
		roles: Roles,
		default_peers_set_num_full: usize,
		default_peers_set_num_light: usize,
	) -> (Self, SyncingHandle<B>) {
		let (tx, rx) = mpsc::channel(64);

		(
			Self {
				chain_sync,
				pending_responses: Default::default(),
				block_announce_data_cache: lru::LruCache::new(cache_size),
				genesis_hash,
				chain,
				roles,
				peers: HashMap::new(),
				default_peers_set_no_slot_peers: HashSet::new(),
				default_peers_set_no_slot_connected_peers: HashSet::new(),
				default_peers_set_num_full,
				default_peers_set_num_light,
				rx,
			},
			SyncingHandle::new(tx),
		)
	}

	pub fn justification_import_result(
		&mut self,
		who: PeerId,
		hash: B::Hash,
		number: NumberFor<B>,
		success: bool,
	) {
		self.chain_sync.on_justification_import(hash, number, success);
		if !success {
			info!("💔 Invalid justification provided by {} for #{}", who, hash);
			self.disconnect_peer(who);
			self.report_peer(who, sc_peerset::ReputationChange::new_fatal("Invalid justification"));
		}
	}

	/// Encode implementation-specific block request.
	pub fn encode_block_request(&self, request: &OpaqueBlockRequest) -> Result<Vec<u8>, String> {
		self.chain_sync.encode_block_request(request)
	}

	/// Encode implementation-specific state request.
	pub fn encode_state_request(&self, request: &OpaqueStateRequest) -> Result<Vec<u8>, String> {
		self.chain_sync.encode_state_request(request)
	}

	pub fn status(&self) -> SyncStatus<B> {
		self.chain_sync.status()
	}

	/// Target sync block number.
	pub fn best_seen_block(&self) -> Option<NumberFor<B>> {
		self.chain_sync.status().best_seen_block
	}

	/// Number of peers participating in syncing.
	pub fn num_sync_peers(&self) -> u32 {
		self.chain_sync.status().num_peers
	}

	/// Number of blocks in the import queue.
	pub fn num_queued_blocks(&self) -> u32 {
		self.chain_sync.status().queued_blocks
	}

	/// Number of downloaded blocks.
	pub fn num_downloaded_blocks(&self) -> usize {
		self.chain_sync.num_downloaded_blocks()
	}

	/// Number of active sync requests.
	pub fn num_sync_requests(&self) -> usize {
		self.chain_sync.num_sync_requests()
	}

	pub fn update_chain_info(&mut self, hash: B::Hash, number: NumberFor<B>) {
		self.chain_sync.update_chain_info(&hash, number);
	}

	pub fn on_block_finalized(&mut self, hash: B::Hash, header: B::Header) {
		self.chain_sync.on_block_finalized(&hash, *header.number())
	}

	// TODO: move to `SyncingHelper`
	/// Request a justification for the given block.
	///
	/// Uses `protocol` to queue a new justification request and tries to dispatch all pending
	/// requests.
	pub fn request_justification(&mut self, hash: &B::Hash, number: NumberFor<B>) {
		self.chain_sync.request_justification(hash, number)
	}

	// TODO: move to `SyncingHelper`
	/// Clear all pending justification requests.
	pub fn clear_justification_requests(&mut self) {
		self.chain_sync.clear_justification_requests();
	}

	// TODO: move to `SyncingHelper`
	/// Request syncing for the given block from given set of peers.
	/// Uses `protocol` to queue a new block download request and tries to dispatch all pending
	/// requests.
	pub fn set_sync_fork_request(
		&mut self,
		peers: Vec<PeerId>,
		hash: &B::Hash,
		number: NumberFor<B>,
	) {
		self.chain_sync.set_sync_fork_request(peers, hash, number)
	}

	pub fn on_blocks_processed(
		&mut self,
		imported: usize,
		count: usize,
		results: Vec<(Result<BlockImportStatus<NumberFor<B>>, BlockImportError>, B::Hash)>,
	) -> VecDeque<CustomMessageOutcome<B>> {
		let mut out = VecDeque::new();

		for result in self.chain_sync.on_blocks_processed(imported, count, results) {
			match result {
				Ok((id, req)) => out.push_back(self.prepare_block_request(id, req)),
				Err(BadPeer(id, repu)) => {
					self.disconnect_peer(id);
					self.report_peer(id, repu)
				},
			}
		}

		out
	}

	// TODO: how to fix this???
	/// Called on the first connection between two peers on the default set, after their exchange
	/// of handshake.
	///
	/// Returns `Ok` if the handshake is accepted and the peer added to the list of peers we sync
	/// from.
	fn on_sync_peer_connected(
		&mut self,
		who: PeerId,
		status: BlockAnnouncesHandshake<B>,
	) -> Result<Option<CustomMessageOutcome<B>>, ()> {
		trace!(target: "sync", "New peer {} {:?}", who, status);

		if self.peers.contains_key(&who) {
			error!(target: "sync", "Called on_sync_peer_connected with already connected peer {}", who);
			debug_assert!(false);
			return Err(())
		}

		if status.genesis_hash != self.genesis_hash {
			log!(
				target: "sync",
				Level::Warn,
				"Peer is on different chain (our genesis: {} theirs: {})",
				self.genesis_hash, status.genesis_hash
			);
			self.disconnect_and_report_peer(who, rep::GENESIS_MISMATCH);

			// if self.boot_node_ids.contains(&who) {
			// 	error!(
			// 		target: "sync",
			// 		"Bootnode with peer id `{}` is on a different chain (our genesis: {} theirs: {})",
			// 		who,
			// 		self.genesis_hash,
			// 		status.genesis_hash,
			// 	);
			// }

			return Err(())
		}

		if self.roles.is_light() {
			// we're not interested in light peers
			if status.roles.is_light() {
				debug!(target: "sync", "Peer {} is unable to serve light requests", who);
				self.disconnect_and_report_peer(who, rep::BAD_ROLE);
				return Err(())
			}

			// we don't interested in peers that are far behind us
			let self_best_block = self.chain.info().best_number;
			let blocks_difference = self_best_block
				.checked_sub(&status.best_number)
				.unwrap_or_else(Zero::zero)
				.saturated_into::<u64>();
			if blocks_difference > LIGHT_MAXIMAL_BLOCKS_DIFFERENCE {
				debug!(target: "sync", "Peer {} is far behind us and will unable to serve light requests", who);
				self.disconnect_and_report_peer(who, rep::PEER_BEHIND_US_LIGHT);
				return Err(())
			}
		}

		let no_slot_peer = self.default_peers_set_no_slot_peers.contains(&who);
		let this_peer_reserved_slot: usize = if no_slot_peer { 1 } else { 0 };

		if status.roles.is_full() &&
			self.chain_sync.num_peers() >=
				self.default_peers_set_num_full +
					self.default_peers_set_no_slot_connected_peers.len() +
					this_peer_reserved_slot
		{
			debug!(target: "sync", "Too many full nodes, rejecting {}", who);
			self.disconnect_peer(who);
			return Err(())
		}

		if status.roles.is_light() &&
			(self.peers.len() - self.chain_sync.num_peers()) >= self.default_peers_set_num_light
		{
			// Make sure that not all slots are occupied by light clients.
			debug!(target: "sync", "Too many light nodes, rejecting {}", who);
			self.disconnect_peer(who);
			return Err(())
		}

		let peer = Peer {
			info: PeerInfo {
				roles: status.roles,
				best_hash: status.best_hash,
				best_number: status.best_number,
			},
			known_blocks: LruHashSet::new(
				NonZeroUsize::new(MAX_KNOWN_BLOCKS).expect("Constant is nonzero"),
			),
		};

		let req = if peer.info.roles.is_full() {
			match self.chain_sync.new_peer(who, peer.info.best_hash, peer.info.best_number) {
				Ok(req) => req,
				Err(BadPeer(id, repu)) => {
					self.disconnect_and_report_peer(id, repu);
					return Err(())
				},
			}
		} else {
			None
		};

		debug!(target: "sync", "Connected {}", who);

		self.peers.insert(who, peer);
		if no_slot_peer {
			self.default_peers_set_no_slot_connected_peers.insert(who);
		}

		if let Some(req) = req {
			Ok(Some(self.prepare_block_request(who, req)))
		} else {
			Ok(None)
		}
	}

	/// Called by peer when it is disconnecting.
	///
	/// Returns a result if the handshake of this peer was indeed accepted.
	pub fn on_sync_peer_disconnected(
		&mut self,
		peer: PeerId,
	) -> Result<Option<CustomMessageOutcome<B>>, ()> {
		if let Some(_peer_data) = self.peers.remove(&peer) {
			let msg = if let Some(OnBlockData::Import(origin, blocks)) =
				self.chain_sync.peer_disconnected(&peer)
			{
				Some(CustomMessageOutcome::BlockImport(origin, blocks))
			} else {
				None
			};

			self.default_peers_set_no_slot_connected_peers.remove(&peer);
			Ok(msg)
		} else {
			Err(())
		}
	}

	// TODO: move to `SyncingHelper`
	/// Push a block announce validation.
	///
	/// It is required that [`ChainSync::poll_block_announce_validation`] is
	/// called later to check for finished validations. The result of the validation
	/// needs to be passed to [`Protocol::process_block_announce_validation_result`]
	/// to finish the processing.
	///
	/// # Note
	///
	/// This will internally create a future, but this future will not be registered
	/// in the task before being polled once. So, it is required to call
	/// [`ChainSync::poll_block_announce_validation`] to ensure that the future is
	/// registered properly and will wake up the task when being ready.
	fn push_block_announce_validation(&mut self, who: PeerId, announce: BlockAnnounce<B::Header>) {
		let hash = announce.header.hash();

		let peer = match self.peers.get_mut(&who) {
			Some(p) => p,
			None => {
				log::error!(target: "sync", "Received block announce from disconnected peer {}", who);
				debug_assert!(false);
				return
			},
		};

		peer.known_blocks.insert(hash);

		let is_best = match announce.state.unwrap_or(BlockState::Best) {
			BlockState::Best => true,
			BlockState::Normal => false,
		};

		if peer.info.roles.is_full() {
			self.chain_sync.push_block_announce_validation(who, hash, announce, is_best);
		}
	}

	pub fn notification(
		&mut self,
		peer: PeerId,
		message: bytes::BytesMut,
		cx: &mut std::task::Context,
	) -> CustomMessageOutcome<B> {
		if self.peers.contains_key(&peer) {
			if let Ok(announce) = BlockAnnounce::decode(&mut message.as_ref()) {
				self.push_block_announce_validation(peer, announce);

				// Make sure that the newly added block announce validation future was
				// polled once to be registered in the task.
				if let Poll::Ready(res) = self.chain_sync.poll_block_announce_validation(cx) {
					self.process_block_announce_validation_result(res)
				} else {
					CustomMessageOutcome::None
				}
			} else {
				warn!(target: "sub-libp2p", "Failed to decode block announce");
				CustomMessageOutcome::None
			}
		} else {
			trace!(
				target: "sync",
				"Received sync for peer earlier refused by sync layer: {peer}",
			);
			CustomMessageOutcome::None
		}
	}

	pub fn custom_protocol_close(&mut self, peer: PeerId) -> CustomMessageOutcome<B> {
		if self.on_sync_peer_disconnected(peer).is_ok() {
			CustomMessageOutcome::SyncDisconnected(peer)
		} else {
			log::trace!(
				target: "sync",
				"Disconnected peer which had earlier been refused by on_sync_peer_connected {peer}",
			);
			CustomMessageOutcome::None
		}
	}

	pub fn custom_protocol_open(
		&mut self,
		peer_id: PeerId,
		received_handshake: Vec<u8>,
		notifications_sink: NotificationsSink,
		negotiated_fallback: Option<ProtocolName>,
	) -> VecDeque<CustomMessageOutcome<B>> {
		match <Message<B> as DecodeAll>::decode_all(&mut &received_handshake[..]) {
			Ok(GenericMessage::Status(handshake)) => {
				let handshake = BlockAnnouncesHandshake {
					roles: handshake.roles,
					best_number: handshake.best_number,
					best_hash: handshake.best_hash,
					genesis_hash: handshake.genesis_hash,
				};

				match self.on_sync_peer_connected(peer_id, handshake) {
					Ok(msg) => match msg {
						Some(inner) =>
							VecDeque::from([inner, CustomMessageOutcome::SyncConnected(peer_id)]),
						None => VecDeque::from([CustomMessageOutcome::SyncConnected(peer_id)]),
					},
					Err(_) => VecDeque::from([CustomMessageOutcome::None]),
				}
			},
			Ok(msg) => {
				debug!(
					target: "sync",
					"Expected Status message from {}, but got {:?}",
					peer_id,
					msg,
				);
				self.report_peer(peer_id, rep::BAD_MESSAGE);
				VecDeque::from([CustomMessageOutcome::None])
			},
			Err(err) => {
				match <BlockAnnouncesHandshake<B> as DecodeAll>::decode_all(
					&mut &received_handshake[..],
				) {
					Ok(handshake) => match self.on_sync_peer_connected(peer_id, handshake) {
						Ok(msg) => match msg {
							Some(inner) => VecDeque::from([
								inner,
								CustomMessageOutcome::SyncConnected(peer_id),
							]),
							None => VecDeque::from([CustomMessageOutcome::SyncConnected(peer_id)]),
						},
						Err(_) => VecDeque::from([CustomMessageOutcome::None]),
					},
					Err(err2) => {
						debug!(
							target: "sync",
							"Couldn't decode handshake sent by {}: {:?}: {} & {}",
							peer_id,
							received_handshake,
							err,
							err2,
						);
						self.report_peer(peer_id, rep::BAD_MESSAGE);
						VecDeque::from([CustomMessageOutcome::None])
					},
				}
			},
		}
	}

	pub fn prepare_block_request(
		&mut self,
		who: PeerId,
		request: BlockRequest<B>,
	) -> CustomMessageOutcome<B> {
		let (tx, rx) = oneshot::channel();

		let new_request = self.chain_sync.create_opaque_block_request(&request);

		self.pending_responses
			.push(Box::pin(async move { (who, PeerRequest::Block(request), rx.await) }));

		CustomMessageOutcome::BlockRequest {
			target: who,
			request: new_request,
			pending_response: tx,
		}
	}

	pub fn prepare_state_request(
		&mut self,
		who: PeerId,
		request: OpaqueStateRequest,
	) -> CustomMessageOutcome<B> {
		let (tx, rx) = oneshot::channel();

		self.pending_responses
			.push(Box::pin(async move { (who, PeerRequest::State, rx.await) }));

		CustomMessageOutcome::StateRequest { target: who, request, pending_response: tx }
	}

	pub fn prepare_warp_sync_request(
		&mut self,
		who: PeerId,
		request: WarpProofRequest<B>,
	) -> CustomMessageOutcome<B> {
		let (tx, rx) = oneshot::channel();

		self.pending_responses
			.push(Box::pin(async move { (who, PeerRequest::WarpProof, rx.await) }));

		CustomMessageOutcome::WarpSyncRequest { target: who, request, pending_response: tx }
	}

	/// Must be called in response to a [`CustomMessageOutcome::BlockRequest`] being emitted.
	/// Must contain the same `PeerId` and request that have been emitted.
	pub fn on_block_response(
		&mut self,
		peer_id: PeerId,
		request: BlockRequest<B>,
		response: OpaqueBlockResponse,
	) -> CustomMessageOutcome<B> {
		let blocks = match self.chain_sync.block_response_into_blocks(&request, response) {
			Ok(blocks) => blocks,
			Err(err) => {
				debug!(target: "sync", "Failed to decode block response from {}: {}", peer_id, err);
				self.report_peer(peer_id, rep::BAD_MESSAGE);
				return CustomMessageOutcome::None
			},
		};

		let block_response = BlockResponse::<B> { id: request.id, blocks };

		let blocks_range = || match (
			block_response
				.blocks
				.first()
				.and_then(|b| b.header.as_ref().map(|h| h.number())),
			block_response.blocks.last().and_then(|b| b.header.as_ref().map(|h| h.number())),
		) {
			(Some(first), Some(last)) if first != last => format!(" ({}..{})", first, last),
			(Some(first), Some(_)) => format!(" ({})", first),
			_ => Default::default(),
		};

		trace!(target: "sync", "BlockResponse {} from {} with {} blocks {}",
			block_response.id,
			peer_id,
			block_response.blocks.len(),
			blocks_range(),
		);

		if request.fields == BlockAttributes::JUSTIFICATION {
			match self.chain_sync.on_block_justification(peer_id, block_response) {
				Ok(OnBlockJustification::Nothing) => CustomMessageOutcome::None,
				Ok(OnBlockJustification::Import { peer, hash, number, justifications }) =>
					CustomMessageOutcome::JustificationImport(peer, hash, number, justifications),
				Err(BadPeer(id, repu)) => {
					self.disconnect_and_report_peer(id, repu);
					CustomMessageOutcome::None
				},
			}
		} else {
			match self.chain_sync.on_block_data(&peer_id, Some(request), block_response) {
				Ok(OnBlockData::Import(origin, blocks)) =>
					CustomMessageOutcome::BlockImport(origin, blocks),
				Ok(OnBlockData::Request(peer, req)) => self.prepare_block_request(peer, req),
				Ok(OnBlockData::Continue) => CustomMessageOutcome::None,
				Err(BadPeer(id, repu)) => {
					self.disconnect_and_report_peer(id, repu);
					CustomMessageOutcome::None
				},
			}
		}
	}

	/// Must be called in response to a [`CustomMessageOutcome::StateRequest`] being emitted.
	/// Must contain the same `PeerId` and request that have been emitted.
	pub fn on_state_response(
		&mut self,
		peer_id: PeerId,
		response: OpaqueStateResponse,
	) -> CustomMessageOutcome<B> {
		match self.chain_sync.on_state_data(&peer_id, response) {
			Ok(OnStateData::Import(origin, block)) =>
				CustomMessageOutcome::BlockImport(origin, vec![block]),
			Ok(OnStateData::Continue) => CustomMessageOutcome::None,
			Err(BadPeer(id, repu)) => {
				self.disconnect_and_report_peer(id, repu);
				CustomMessageOutcome::None
			},
		}
	}

	/// Must be called in response to a [`CustomMessageOutcome::WarpSyncRequest`] being emitted.
	/// Must contain the same `PeerId` and request that have been emitted.
	pub fn on_warp_sync_response(
		&mut self,
		peer_id: PeerId,
		response: EncodedProof,
	) -> CustomMessageOutcome<B> {
		match self.chain_sync.on_warp_sync_data(&peer_id, response) {
			Ok(()) => CustomMessageOutcome::None,
			Err(BadPeer(id, repu)) => {
				self.disconnect_and_report_peer(id, repu);
				CustomMessageOutcome::None
			},
		}
	}

	fn disconnect_and_report_peer(&mut self, _id: PeerId, _score_diff: ReputationChange) {
		self.disconnect_peer(_id);
		self.report_peer(_id, _score_diff);
	}

	fn report_peer(&mut self, _id: PeerId, _score_diff: ReputationChange) {
		// TODO: report peer
		// todo!();
	}

	fn disconnect_peer(&mut self, _id: PeerId) {
		// TODO: disconnect peer
		// todo!();
	}

	// TODO: move to `SyncingHelper`
	/// Process the result of the block announce validation.
	pub fn process_block_announce_validation_result(
		&mut self,
		validation_result: PollBlockAnnounceValidation<B::Header>,
	) -> CustomMessageOutcome<B> {
		let (header, who) = match validation_result {
			PollBlockAnnounceValidation::Skip => return CustomMessageOutcome::None,
			PollBlockAnnounceValidation::Nothing { is_best: _, who, announce } => {
				if let Some(data) = announce.data {
					if !data.is_empty() {
						self.block_announce_data_cache.put(announce.header.hash(), data);
					}
				}

				return CustomMessageOutcome::None
			},
			PollBlockAnnounceValidation::ImportHeader { announce, is_best: _, who } => {
				if let Some(data) = announce.data {
					if !data.is_empty() {
						self.block_announce_data_cache.put(announce.header.hash(), data);
					}
				}

				(announce.header, who)
			},
			PollBlockAnnounceValidation::Failure { who, disconnect } => {
				if disconnect {
					self.disconnect_peer(who);
				}

				self.report_peer(who, rep::BAD_BLOCK_ANNOUNCEMENT);
				return CustomMessageOutcome::None
			},
		};

		// TODO: refactor this?
		// to import header from announced block let's construct response to request that normally
		// would have been sent over network (but it is not in our case)
		let blocks_to_import = self.chain_sync.on_block_data(
			&who,
			None,
			BlockResponse::<B> {
				id: 0,
				blocks: vec![BlockData::<B> {
					hash: header.hash(),
					header: Some(header),
					body: None,
					indexed_body: None,
					receipt: None,
					message_queue: None,
					justification: None,
					justifications: None,
				}],
			},
		);

		match blocks_to_import {
			Ok(OnBlockData::Import(origin, blocks)) =>
				CustomMessageOutcome::BlockImport(origin, blocks),
			Ok(OnBlockData::Request(peer, req)) => self.prepare_block_request(peer, req),
			Ok(OnBlockData::Continue) => CustomMessageOutcome::None,
			Err(BadPeer(id, repu)) => {
				self.disconnect_and_report_peer(id, repu);
				CustomMessageOutcome::None
			},
		}
	}

	pub fn poll(&mut self, cx: &mut std::task::Context) -> VecDeque<CustomMessageOutcome<B>> {
		// Check for finished outgoing requests.
		let mut finished_block_requests = Vec::new();
		let mut finished_state_requests = Vec::new();
		let mut finished_warp_sync_requests = Vec::new();

		while let Poll::Ready(Some((id, request, response))) = self.pending_responses.poll_next(cx)
		{
			match response {
				Ok(Ok(resp)) => match request {
					PeerRequest::Block(req) => {
						let response = match self.chain_sync.decode_block_response(&resp[..]) {
							Ok(proto) => proto,
							Err(e) => {
								debug!(
									target: "sync",
									"Failed to decode block response from peer {:?}: {:?}.",
									id,
									e
								);
								self.disconnect_and_report_peer(id, rep::BAD_MESSAGE);
								continue
							},
						};

						finished_block_requests.push((id, req, response));
					},
					PeerRequest::State => {
						let response = match self.chain_sync.decode_state_response(&resp[..]) {
							Ok(proto) => proto,
							Err(e) => {
								debug!(
									target: "sync",
									"Failed to decode state response from peer {:?}: {:?}.",
									id,
									e
								);
								self.disconnect_and_report_peer(id, rep::BAD_MESSAGE);
								continue
							},
						};

						finished_state_requests.push((id, response));
					},
					PeerRequest::WarpProof => {
						finished_warp_sync_requests.push((id, resp));
					},
				},
				Ok(Err(err)) => {
					debug!(target: "sync", "Request to peer {:?} failed: {:?}.", id, err);

					match err {
						RequestFailure::Network(OutboundFailure::Timeout) => {
							self.disconnect_and_report_peer(id, rep::TIMEOUT);
						},
						RequestFailure::Network(OutboundFailure::UnsupportedProtocols) => {
							self.disconnect_and_report_peer(id, rep::BAD_PROTOCOL);
						},
						RequestFailure::Network(OutboundFailure::DialFailure) => {
							self.disconnect_peer(id);
						},
						RequestFailure::Refused => {
							self.disconnect_and_report_peer(id, rep::REFUSED);
						},
						RequestFailure::Network(OutboundFailure::ConnectionClosed) |
						RequestFailure::NotConnected => {
							self.disconnect_peer(id);
						},
						RequestFailure::UnknownProtocol => {
							debug_assert!(false, "Block request protocol should always be known.");
						},
						RequestFailure::Obsolete => {
							debug_assert!(
								false,
								"Can not receive `RequestFailure::Obsolete` after dropping the \
									 response receiver.",
							);
						},
					}
				},
				Err(oneshot::Canceled) => {
					trace!(
						target: "sync",
						"Request to peer {:?} failed due to oneshot being canceled.",
						id,
					);
					self.disconnect_peer(id);
				},
			}
		}

		let mut pending_messages = VecDeque::new();

		// TODO: merge loops below with the loop above

		for (id, req, response) in finished_block_requests {
			let ev = self.on_block_response(id, req, response);
			pending_messages.push_back(ev);
		}

		for (id, response) in finished_state_requests {
			let ev = self.on_state_response(id, response);
			pending_messages.push_back(ev);
		}

		for (id, response) in finished_warp_sync_requests {
			let ev = self.on_warp_sync_response(id, EncodedProof(response));
			pending_messages.push_back(ev);
		}

		for (id, request) in self
			.chain_sync
			.block_requests()
			.map(|(peer_id, request)| (*peer_id, request))
			.collect::<Vec<_>>()
		{
			// TODO: send block request
			let event = self.prepare_block_request(id, request);
			pending_messages.push_back(event);
		}

		if let Some((id, request)) = self.chain_sync.state_request() {
			let event = self.prepare_state_request(id, request);
			pending_messages.push_back(event);
		}

		for (id, request) in self.chain_sync.justification_requests().collect::<Vec<_>>() {
			// TODO: send block request
			let event = self.prepare_block_request(id, request);
			pending_messages.push_back(event);
		}

		if let Some((id, request)) = self.chain_sync.warp_sync_request() {
			let event = self.prepare_warp_sync_request(id, request);
			pending_messages.push_back(event);
		}

		// Check if there is any block announcement validation finished.
		while let Poll::Ready(result) = self.chain_sync.poll_block_announce_validation(cx) {
			match self.process_block_announce_validation_result(result) {
				CustomMessageOutcome::None => {},
				outcome => pending_messages.push_back(outcome),
			}
		}

		pending_messages
	}

	pub async fn run(mut self) {
		todo!();
		// while let Some(event) = self.rx.next().await {
		//    	// TODO: zzz
		// }
	}
}
