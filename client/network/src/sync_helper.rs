use codec::{Decode, DecodeAll, Encode};
use futures::{
	channel::{mpsc, oneshot},
	prelude::*,
	stream::FuturesUnordered,
};
use libp2p::{request_response::OutboundFailure, PeerId};
use log::{debug, error, info, log, trace, warn, Level};
use sc_client_api::HeaderBackend;
use sc_consensus::{
	import_queue::{BlockImportError, BlockImportStatus},
	ImportQueue, Link,
};
use sc_network_common::{
	config::ProtocolId,
	notifications::ProtocolConfig as NotifProtocolConfig,
	protocol::{event::Event, ProtocolName},
	request_responses::{IfDisconnected, RequestFailure},
	service::{
		NetworkEventStream, NetworkNotification, NetworkPeers, NetworkRequest, PeerValidationResult,
	},
	sync::{
		message::{
			generic::Message as GenericMessage, BlockAnnounce, BlockAttributes, BlockData,
			BlockRequest, BlockResponse, BlockState, Message, Roles,
		},
		warp::{EncodedProof, WarpProofRequest},
		BadPeer, ChainSync, OnBlockData, OnBlockJustification, OnStateData, OpaqueBlockRequest,
		OpaqueBlockResponse, OpaqueStateRequest, OpaqueStateResponse, PollBlockAnnounceValidation,
		SyncStatus,
	},
	utils::LruHashSet,
};
use sc_peerset::ReputationChange;
use sp_arithmetic::traits::SaturatedConversion;
use sp_runtime::{
	generic::BlockId,
	traits::{Block as BlockT, CheckedSub, Header as HeaderT, NumberFor, Zero},
};
use std::{
	collections::{HashMap, HashSet},
	iter,
	num::NonZeroUsize,
	pin::Pin,
	sync::Arc,
	task::Poll,
};

/// Maximum number of known block hashes to keep for a peer.
pub const MAX_KNOWN_BLOCKS: usize = 1024; // ~32kb per peer + LruHashSet overhead

/// Maximum allowed size for a block announce.
pub const MAX_BLOCK_ANNOUNCE_SIZE: u64 = 1024 * 1024;

/// Maximum size used for notifications in the block announce and transaction protocols.
// Must be equal to `max(MAX_BLOCK_ANNOUNCE_SIZE, MAX_TRANSACTIONS_SIZE)`.
pub(crate) const BLOCK_ANNOUNCES_TRANSACTIONS_SUBSTREAM_SIZE: u64 = 16 * 1024 * 1024;

/// When light node connects to the full node and the full node is behind light node
/// for at least `LIGHT_MAXIMAL_BLOCKS_DIFFERENCE` blocks, we consider it not useful
/// and disconnect to free connection slot.
pub const LIGHT_MAXIMAL_BLOCKS_DIFFERENCE: u64 = 8192;

mod rep {
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
	pub fn build(
		roles: Roles,
		best_number: NumberFor<B>,
		best_hash: B::Hash,
		genesis_hash: B::Hash,
	) -> Self {
		Self { genesis_hash, roles, best_number, best_hash }
	}
}

// TODO: zzz
pub type PendingResponse<B> =
	(PeerId, PeerRequest<B>, Result<Result<Vec<u8>, RequestFailure>, oneshot::Canceled>);

// TODO: move chainsync here
pub struct SyncingHelper<B: BlockT, Client, N> {
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

	rx: mpsc::UnboundedReceiver<SyncEvent<B>>,
	sync_handle: SyncingHandle<B>,

	block_request_protocol_name: ProtocolName,
	state_request_protocol_name: ProtocolName,
	warp_sync_protocol_name: Option<ProtocolName>,
	block_announces_protocol_name: ProtocolName,

	/// The import queue that was passed at initialization.
	import_queue: Box<dyn ImportQueue<B>>,

	service: Option<Arc<N>>,
}

impl<B, Client, N> SyncingHelper<B, Client, N>
where
	B: BlockT,
	Client: HeaderBackend<B> + 'static,
	N: NetworkRequest + NetworkEventStream + NetworkPeers + NetworkNotification,
{
	pub fn new(
		chain_sync: Box<dyn ChainSync<B>>,
		import_queue: Box<dyn ImportQueue<B>>,
		cache_size: usize,
		protocol_id: ProtocolId,
		fork_id: &Option<String>,
		chain: Arc<Client>,
		roles: Roles,
		default_peers_set_num_full: usize,
		default_peers_set_num_light: usize,
		block_request_protocol_name: ProtocolName,
		state_request_protocol_name: ProtocolName,
		warp_sync_protocol_name: Option<ProtocolName>,
	) -> (Self, SyncingHandle<B>, NotifProtocolConfig) {
		let (tx, rx) = mpsc::unbounded();

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

		let best_number = chain.info().best_number;
		let best_hash = chain.info().best_hash;
		let genesis_hash = chain.info().genesis_hash;

		let block_announces_handshake =
			BlockAnnouncesHandshake::<B>::build(roles, best_number, best_hash, genesis_hash)
				.encode();

		let block_announces_protocol_name = block_announces_protocol.clone().into();
		let sync_protocol_config = NotifProtocolConfig {
			name: block_announces_protocol.into(),
			fallback_names: iter::once(legacy_ba_protocol_name.into()).collect(),
			handshake: block_announces_handshake,
			max_notification_size: MAX_BLOCK_ANNOUNCE_SIZE,
		};

		(
			Self {
				chain_sync,
				import_queue,
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
				service: None,
				block_request_protocol_name,
				state_request_protocol_name,
				warp_sync_protocol_name,
				sync_handle: SyncingHandle::new(tx.clone()),
				block_announces_protocol_name,
			},
			SyncingHandle::new(tx),
			sync_protocol_config,
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
	) {
		// println!("here, imported {imported} count {count}");
		for result in self.chain_sync.on_blocks_processed(imported, count, results) {
			match result {
				Ok((id, req)) => {
					self.prepare_block_request(id, req);
				},
				Err(BadPeer(id, repu)) => {
					self.disconnect_peer(id);
					self.report_peer(id, repu)
				},
			}
		}
	}

	pub fn announce_block(&mut self, hash: B::Hash, data: Option<Vec<u8>>) -> () {
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
			.or_else(|| self.block_announce_data_cache.get(&hash).cloned())
			.unwrap_or_default();

		let message = BlockAnnounce {
			header: header.clone(),
			state: if is_best { Some(BlockState::Best) } else { Some(BlockState::Normal) },
			data: Some(data.clone()),
		}
		.encode();

		let mut peers = Vec::new();

		for (who, peer) in self.peers.iter_mut() {
			let inserted = peer.known_blocks.insert(hash);
			// futures::executor::block_on(self.sync_handle.insert_known_block(who, hash));
			if inserted {
				trace!(target: "sync", "Announcing block {:?} to {}", hash, who);
				peers.push(*who);
				// if let Some(service) = &self.service {
				// 				service.write_sync_notification(*who, message.clone());
				// }
			}
		}

		if peers.len() > 0 {
			if let Some(service) = &self.service {
				service.write_batch_sync_notification(peers, message);
			}
		}
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
	) -> Result<(), ()> {
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
			self.prepare_block_request(who, req);
			Ok(())
		} else {
			Ok(())
		}
	}

	/// Called by peer when it is disconnecting.
	///
	/// Returns a result if the handshake of this peer was indeed accepted.
	pub fn on_sync_peer_disconnected(&mut self, peer: PeerId) -> Result<(), ()> {
		if let Some(_peer_data) = self.peers.remove(&peer) {
			if let Some(OnBlockData::Import(origin, blocks)) =
				self.chain_sync.peer_disconnected(&peer)
			{
				self.import_queue.import_blocks(origin, blocks);
			}

			self.default_peers_set_no_slot_connected_peers.remove(&peer);
			Ok(())
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

	pub async fn notification(&mut self, peer: PeerId, message: bytes::Bytes) {
		if self.peers.contains_key(&peer) {
			if let Ok(announce) = BlockAnnounce::decode(&mut message.as_ref()) {
				self.push_block_announce_validation(peer, announce);

				futures::future::poll_fn(|cx| {
					if let Poll::Ready(res) = self.chain_sync.poll_block_announce_validation(cx) {
						self.process_block_announce_validation_result(res)
					}
					std::task::Poll::Ready(())
				})
				.await;
			} else {
				warn!(target: "sub-libp2p", "Failed to decode block announce");
			}
		} else {
			trace!(
				target: "sync",
				"Received sync for peer earlier refused by sync layer: {peer}",
			);
		}
	}

	pub fn custom_protocol_close(&mut self, peer: PeerId) {
		if self.on_sync_peer_disconnected(peer).is_ok() {
			self.service.as_ref().unwrap().disconnect_sync_peer(peer);
		} else {
			log::trace!(
				target: "sync",
				"Disconnected peer which had earlier been refused by on_sync_peer_connected {peer}",
			);
		}
	}

	pub fn custom_protocol_open(
		&mut self,
		peer_id: PeerId,
		received_handshake: Vec<u8>,
		_negotiated_fallback: Option<ProtocolName>,
	) {
		match <Message<B> as DecodeAll>::decode_all(&mut &received_handshake[..]) {
			Ok(GenericMessage::Status(handshake)) => {
				let handshake = BlockAnnouncesHandshake {
					roles: handshake.roles,
					best_number: handshake.best_number,
					best_hash: handshake.best_hash,
					genesis_hash: handshake.genesis_hash,
				};

				let roles = handshake.roles;
				match self.on_sync_peer_connected(peer_id, handshake) {
					Ok(()) =>
						if let Some(service) = &self.service {
							service.report_peer_validation_result(
								peer_id,
								PeerValidationResult::Accepted(roles),
							);
						},
					Err(_) => {},
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
			},
			Err(err) => {
				match <BlockAnnouncesHandshake<B> as DecodeAll>::decode_all(
					&mut &received_handshake[..],
				) {
					Ok(handshake) => {
						let roles = handshake.roles;
						match self.on_sync_peer_connected(peer_id, handshake) {
							Ok(()) =>
								if let Some(service) = &self.service {
									service.report_peer_validation_result(
										peer_id,
										PeerValidationResult::Accepted(roles),
									);
								},
							Err(_) => {},
						}
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
					},
				}
			},
		}
	}

	pub fn prepare_block_request(&mut self, who: PeerId, request: BlockRequest<B>) {
		let (tx, rx) = oneshot::channel();

		let new_request = self.chain_sync.create_opaque_block_request(&request);

		self.pending_responses
			.push(Box::pin(async move { (who, PeerRequest::Block(request), rx.await) }));

		match self.service {
			Some(ref service) => {
				service.start_request(
					who,
					self.block_request_protocol_name.clone(),
					self.encode_block_request(&new_request).unwrap(), // TODO: fix
					tx,
					IfDisconnected::ImmediateError,
				);
			},
			None => {},
		}
	}

	pub fn prepare_state_request(&mut self, who: PeerId, request: OpaqueStateRequest) {
		let (tx, rx) = oneshot::channel();

		self.pending_responses
			.push(Box::pin(async move { (who, PeerRequest::State, rx.await) }));

		match self.service {
			Some(ref service) => {
				service.start_request(
					who,
					self.state_request_protocol_name.clone(),
					self.encode_state_request(&request).unwrap(), // TODO: fix
					tx,
					IfDisconnected::ImmediateError,
				);
			},
			None => {},
		}
	}

	pub fn prepare_warp_sync_request(&mut self, who: PeerId, request: WarpProofRequest<B>) {
		let (tx, rx) = oneshot::channel();

		self.pending_responses
			.push(Box::pin(async move { (who, PeerRequest::WarpProof, rx.await) }));

		match self.service {
			Some(ref service) => {
				service.start_request(
					who,
					self.warp_sync_protocol_name
						.as_ref()
						.expect("warp sync protocol to be available")
						.clone(),
					request.encode(),
					tx,
					IfDisconnected::ImmediateError,
				);
			},
			None => {},
		}
	}

	/// Must be called in response to a [`CustomMessageOutcome::BlockRequest`] being emitted.
	/// Must contain the same `PeerId` and request that have been emitted.
	pub fn on_block_response(
		&mut self,
		peer_id: PeerId,
		request: BlockRequest<B>,
		response: OpaqueBlockResponse,
	) {
		let blocks = match self.chain_sync.block_response_into_blocks(&request, response) {
			Ok(blocks) => blocks,
			Err(err) => {
				debug!(target: "sync", "Failed to decode block response from {}: {}", peer_id, err);
				self.report_peer(peer_id, rep::BAD_MESSAGE);
				return
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
				Ok(OnBlockJustification::Nothing) => {},
				Ok(OnBlockJustification::Import { peer, hash, number, justifications }) => {
					self.import_queue.import_justifications(peer, hash, number, justifications);
					// CustomMessageOutcome::JustificationImport(peer, hash, number,
					// justifications), CustomMessageOutcome::None
				},
				Err(BadPeer(id, repu)) => {
					self.disconnect_and_report_peer(id, repu);
					// CustomMessageOutcome::None
				},
			}
		} else {
			match self.chain_sync.on_block_data(&peer_id, Some(request), block_response) {
				Ok(OnBlockData::Import(origin, blocks)) => {
					self.import_queue.import_blocks(origin, blocks);
					// CustomMessageOutcome::BlockImport(origin, blocks)
					// CustomMessageOutcome::None
				},
				Ok(OnBlockData::Request(peer, req)) => {
					self.prepare_block_request(peer, req);
					// CustomMessageOutcome::None
				},
				Ok(OnBlockData::Continue) => {},
				Err(BadPeer(id, repu)) => {
					self.disconnect_and_report_peer(id, repu);
					// CustomMessageOutcome::None
				},
			}
		}
	}

	/// Must be called in response to a [`CustomMessageOutcome::StateRequest`] being emitted.
	/// Must contain the same `PeerId` and request that have been emitted.
	pub fn on_state_response(&mut self, peer_id: PeerId, response: OpaqueStateResponse) {
		match self.chain_sync.on_state_data(&peer_id, response) {
			Ok(OnStateData::Import(origin, block)) => {
				self.import_queue.import_blocks(origin, vec![block]);
			},
			Ok(OnStateData::Continue) => {},
			Err(BadPeer(id, repu)) => {
				self.disconnect_and_report_peer(id, repu);
			},
		}
	}

	/// Must be called in response to a [`CustomMessageOutcome::WarpSyncRequest`] being emitted.
	/// Must contain the same `PeerId` and request that have been emitted.
	pub fn on_warp_sync_response(&mut self, peer_id: PeerId, response: EncodedProof) {
		match self.chain_sync.on_warp_sync_data(&peer_id, response) {
			Ok(()) => {},
			Err(BadPeer(id, repu)) => {
				self.disconnect_and_report_peer(id, repu);
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
	) {
		let (header, who) = match validation_result {
			PollBlockAnnounceValidation::Skip => return,
			PollBlockAnnounceValidation::Nothing { is_best: _, who: _, announce } => {
				if let Some(data) = announce.data {
					if !data.is_empty() {
						self.block_announce_data_cache.put(announce.header.hash(), data);
					}
				}

				return
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
				return
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
			Ok(OnBlockData::Import(origin, blocks)) => {
				self.import_queue.import_blocks(origin, blocks);
			},
			Ok(OnBlockData::Request(peer, req)) => {
				self.prepare_block_request(peer, req);
			},
			Ok(OnBlockData::Continue) => {},
			Err(BadPeer(id, repu)) => {
				self.disconnect_and_report_peer(id, repu);
			},
		}
	}

	pub fn register_network_service(&mut self, service: Arc<N>) {
		self.service = Some(service);
	}

	// TODO: zzz
	fn handle_pending_response(
		&mut self,
		id: PeerId,
		request: PeerRequest<B>,
		response: Result<Result<Vec<u8>, RequestFailure>, oneshot::Canceled>,
	) {
		// Check for finished outgoing requests.
		let mut finished_block_requests = Vec::new();
		let mut finished_state_requests = Vec::new();
		let mut finished_warp_sync_requests = Vec::new();

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
							return
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
							return
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

		for (id, req, response) in finished_block_requests {
			self.on_block_response(id, req, response);
		}

		for (id, response) in finished_state_requests {
			self.on_state_response(id, response);
		}

		for (id, response) in finished_warp_sync_requests {
			self.on_warp_sync_response(id, EncodedProof(response));
		}
	}

	// TODO: hideous, fix
	fn handle_command(&mut self, event: SyncEvent<B>) {
		match event {
			SyncEvent::NumConnectedPeers(channel_response) => {
				let _ = channel_response.send(self.peers.len());
			},
			SyncEvent::SyncState(channel_response) => {
				let _ = channel_response.send(self.chain_sync.status());
			},
			SyncEvent::BestSeenBlock(channel_response) => {
				let _ = channel_response.send(self.chain_sync.status().best_seen_block);
			},
			SyncEvent::NumSyncPeers(channel_response) => {
				let _ = channel_response.send(self.chain_sync.status().num_peers);
			},
			SyncEvent::NumQueuedBlocks(channel_response) => {
				let _ = channel_response.send(self.chain_sync.status().queued_blocks);
			},
			SyncEvent::NumDownloadedBlocks(channel_response) => {
				let _ = channel_response.send(self.num_downloaded_blocks());
			},
			SyncEvent::NumSyncRequests(channel_response) => {
				let _ = channel_response.send(self.num_sync_requests());
			},
			SyncEvent::UpdateChainInfo(hash, number) => {
				self.update_chain_info(hash, number);
			},
			SyncEvent::OnBlockFinalized(hash, header) => {
				self.on_block_finalized(hash, header);
			},
			SyncEvent::RequestJustification(hash, number) => {
				self.request_justification(&hash, number);
			},
			SyncEvent::ClearJustificationRequests => {
				self.clear_justification_requests();
			},
			SyncEvent::SetSyncForkRequest(peers, hash, number) => {
				self.set_sync_fork_request(peers, &hash, number);
			},
			SyncEvent::JustificationImportResult(peer_id, hash, number, success) => {
				self.justification_import_result(peer_id, hash, number, success);
			},
			SyncEvent::OnBlocksProcessed(imported, count, results) => {
				self.on_blocks_processed(imported, count, results);
			},
			SyncEvent::GetPeers(channel_response) => {
				// TODO: remove clone if possible
				let _ = channel_response
					.send(self.peers.iter().map(|(id, peer)| (*id, (*peer).clone())).collect());
			},
			SyncEvent::GetHandshake(hash, number, channel_response) => {
				let _ = channel_response.send(
					BlockAnnouncesHandshake::<B>::build(
						self.roles,
						number,
						hash,
						self.genesis_hash,
					)
					.encode(),
				);
			},
			SyncEvent::AnnounceBlock(hash, data, channel_response) => {
				let _ = channel_response.send(self.announce_block(hash, data));
			},
		}
	}

	fn call_chain_sync(&mut self) {
		for (id, request) in self
			.chain_sync
			.block_requests()
			.map(|(peer_id, request)| (*peer_id, request))
			.collect::<Vec<_>>()
		{
			self.prepare_block_request(id, request);
		}

		if let Some((id, request)) = self.chain_sync.state_request() {
			self.prepare_state_request(id, request);
		}

		for (id, request) in self.chain_sync.justification_requests().collect::<Vec<_>>() {
			self.prepare_block_request(id, request);
		}

		if let Some((id, request)) = self.chain_sync.warp_sync_request() {
			self.prepare_warp_sync_request(id, request);
		}
	}

	pub async fn run(mut self) {
		let mut event_stream = self.service.as_ref().unwrap().event_stream("sync-stuff");

		loop {
			futures::select! {
				command = futures::StreamExt::next(&mut self.rx).fuse() => match command {
					Some(command) => {
						self.handle_command(command);
						self.call_chain_sync();
					}
					None => {}
				},
				network_event = futures::StreamExt::next(&mut event_stream).fuse() => {
					if let Some(network_event) = network_event {
						match network_event {
							Event::PeerConnected { peer_id, received_handshake, negotiated_fallback, } => {
								self.custom_protocol_open(peer_id, received_handshake, negotiated_fallback);
							}
							Event::NotificationStreamClosed { remote, protocol } => {
								if protocol != self.block_announces_protocol_name {
									continue
								}

								self.custom_protocol_close(remote);
							}
							Event::NotificationsReceived { remote, messages } => {
								for (protocol, message) in messages {
									if protocol != self.block_announces_protocol_name {
										continue
									}

									self.notification(remote, message).await;
								}
							}
							_ => {},
						}
					} else {
						// Networking has seemingly closed. Closing as well.
						return;
					}
				}
				request = self.pending_responses.select_next_some() => {
					self.handle_pending_response(request.0, request.1, request.2);
					self.call_chain_sync();
				}
				_ = async_std::task::sleep(std::time::Duration::from_millis(500)).fuse() => {
					self.call_chain_sync();
				}
				result = futures::future::poll_fn(|cx| self.chain_sync.poll_block_announce_validation(cx)).fuse() => {
					self.process_block_announce_validation_result(result);
					self.call_chain_sync();
				}
				_ = futures::future::poll_fn(|cx| {
					self.import_queue.poll_actions(cx, &mut NetworkLink { sync_handle: &self.sync_handle });
					std::task::Poll::Pending::<()>
				}).fuse() => {}
			}
		}
	}
}

// Implementation of `import_queue::Link` trait using the available local variables.
struct NetworkLink<'a, B: BlockT> {
	sync_handle: &'a SyncingHandle<B>,
}

impl<'a, B: BlockT> Link<B> for NetworkLink<'a, B> {
	fn blocks_processed(
		&mut self,
		imported: usize,
		count: usize,
		results: Vec<(Result<BlockImportStatus<NumberFor<B>>, BlockImportError>, B::Hash)>,
	) {
		self.sync_handle.on_blocks_processed(imported, count, results);
	}

	fn justification_imported(
		&mut self,
		who: PeerId,
		hash: &B::Hash,
		number: NumberFor<B>,
		success: bool,
	) {
		self.sync_handle.justification_import_result(who, *hash, number, success)
	}

	fn request_justification(&mut self, hash: &B::Hash, number: NumberFor<B>) {
		self.sync_handle.request_justification(*hash, number)
	}
}

#[derive(Debug)]
pub enum SyncEvent<B: BlockT> {
	NumConnectedPeers(oneshot::Sender<usize>),
	SyncState(oneshot::Sender<SyncStatus<B>>),
	BestSeenBlock(oneshot::Sender<Option<NumberFor<B>>>),
	NumSyncPeers(oneshot::Sender<u32>),
	NumQueuedBlocks(oneshot::Sender<u32>),
	NumDownloadedBlocks(oneshot::Sender<usize>),
	NumSyncRequests(oneshot::Sender<usize>),
	UpdateChainInfo(B::Hash, NumberFor<B>),
	OnBlockFinalized(B::Hash, B::Header),
	RequestJustification(B::Hash, NumberFor<B>),
	ClearJustificationRequests,
	SetSyncForkRequest(Vec<PeerId>, B::Hash, NumberFor<B>),
	JustificationImportResult(PeerId, B::Hash, NumberFor<B>, bool),
	OnBlocksProcessed(
		usize,
		usize,
		Vec<(Result<BlockImportStatus<NumberFor<B>>, BlockImportError>, B::Hash)>,
	),
	GetPeers(oneshot::Sender<Vec<(PeerId, Peer<B>)>>),
	GetHandshake(B::Hash, NumberFor<B>, oneshot::Sender<Vec<u8>>),
	AnnounceBlock(B::Hash, Option<Vec<u8>>, oneshot::Sender<()>),
}

#[derive(Clone)]
pub struct SyncingHandle<B: BlockT> {
	tx: mpsc::UnboundedSender<SyncEvent<B>>,
}

impl<B: BlockT> SyncingHandle<B> {
	pub fn new(tx: mpsc::UnboundedSender<SyncEvent<B>>) -> Self {
		Self { tx }
	}

	pub async fn announce_block(&self, hash: B::Hash, data: Option<Vec<u8>>) -> () {
		let (tx, rx) = oneshot::channel();

		self.tx
			.unbounded_send(SyncEvent::AnnounceBlock(hash, data, tx))
			.expect("channel to stay open");
		rx.await.expect("channel to stay open")
	}

	pub fn on_block_finalized(&self, hash: B::Hash, header: B::Header) {
		self.tx
			.unbounded_send(SyncEvent::OnBlockFinalized(hash, header))
			.expect("channel to stay open");
	}

	pub fn request_justification(&self, hash: B::Hash, number: NumberFor<B>) {
		self.tx
			.unbounded_send(SyncEvent::RequestJustification(hash, number))
			.expect("channel to stay open");
	}

	pub fn clear_justification_requests(&self) {
		self.tx
			.unbounded_send(SyncEvent::ClearJustificationRequests)
			.expect("channel to stay open");
	}

	pub fn set_sync_fork_request(&self, peers: Vec<PeerId>, hash: B::Hash, number: NumberFor<B>) {
		self.tx
			.unbounded_send(SyncEvent::SetSyncForkRequest(peers, hash, number))
			.expect("channel to stay open");
	}

	pub async fn get_handshake(&self, hash: B::Hash, number: NumberFor<B>) -> Vec<u8> {
		let (tx, rx) = oneshot::channel();

		self.tx
			.unbounded_send(SyncEvent::GetHandshake(hash, number, tx))
			.expect("channel to stay open");
		rx.await.expect("channel to stay open")
	}

	pub fn on_blocks_processed(
		&self,
		imported: usize,
		count: usize,
		results: Vec<(Result<BlockImportStatus<NumberFor<B>>, BlockImportError>, B::Hash)>,
	) {
		self.tx
			.unbounded_send(SyncEvent::OnBlocksProcessed(imported, count, results))
			.expect("channel to stay open");
	}

	pub fn justification_import_result(
		&self,
		who: PeerId,
		hash: B::Hash,
		number: NumberFor<B>,
		success: bool,
	) {
		self.tx
			.unbounded_send(SyncEvent::JustificationImportResult(who, hash, number, success))
			.expect("channel to stay open");
	}

	pub async fn num_connected_peers(&self) -> usize {
		let (tx, rx) = oneshot::channel();

		self.tx
			.unbounded_send(SyncEvent::NumConnectedPeers(tx))
			.expect("channel to stay open");
		rx.await.expect("channel to stay open")
	}

	pub async fn num_sync_peers(&self) -> u32 {
		let (tx, rx) = oneshot::channel();

		self.tx
			.unbounded_send(SyncEvent::NumSyncPeers(tx))
			.expect("channel to stay open");
		rx.await.expect("channel to stay open")
	}

	pub async fn num_queued_blocks(&self) -> u32 {
		let (tx, rx) = oneshot::channel();

		self.tx
			.unbounded_send(SyncEvent::NumQueuedBlocks(tx))
			.expect("channel to stay open");
		rx.await.expect("channel to stay open")
	}

	pub async fn status(&self) -> SyncStatus<B> {
		let (tx, rx) = oneshot::channel();

		self.tx.unbounded_send(SyncEvent::SyncState(tx)).expect("channel to stay open");
		rx.await.expect("channel to stay open")
	}

	pub async fn num_downloaded_blocks(&self) -> usize {
		let (tx, rx) = oneshot::channel();

		self.tx
			.unbounded_send(SyncEvent::NumDownloadedBlocks(tx))
			.expect("channel to stay open");
		rx.await.expect("channel to stay open")
	}

	pub async fn num_sync_requests(&self) -> usize {
		let (tx, rx) = oneshot::channel();

		self.tx
			.unbounded_send(SyncEvent::NumSyncRequests(tx))
			.expect("channel to stay open");
		rx.await.expect("channel to stay open")
	}

	pub async fn best_seen_block(&self) -> Option<NumberFor<B>> {
		let (tx, rx) = oneshot::channel();

		self.tx
			.unbounded_send(SyncEvent::BestSeenBlock(tx))
			.expect("channel to stay open");
		rx.await.expect("channel to stay open")
	}

	pub fn update_chain_info(&self, hash: B::Hash, number: NumberFor<B>) {
		self.tx
			.unbounded_send(SyncEvent::UpdateChainInfo(hash, number))
			.expect("channel to stay open");
	}

	pub async fn get_peers(&self) -> Vec<(PeerId, Peer<B>)> {
		let (tx, rx) = oneshot::channel();

		self.tx.unbounded_send(SyncEvent::GetPeers(tx)).expect("channel to stay open");
		rx.await.expect("channel to stay open")
	}
}
