#![allow(unused)]
use crate::{config, protocol::*};

use bytes::Bytes;
use codec::{Decode, DecodeAll, Encode};
use futures::{
	channel::oneshot,
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
pub struct SyncingHelper<B: BlockT> {
	pub pending_responses:
		FuturesUnordered<Pin<Box<dyn Future<Output = PendingResponse<B>> + Send>>>,
	/// State machine that handles the list of in-progress requests. Only full node peers are
	/// registered.
	pub chain_sync: Box<dyn ChainSync<B>>,
}

impl<B: BlockT> SyncingHelper<B> {
	pub fn new(chain_sync: Box<dyn ChainSync<B>>) -> Self {
		Self { chain_sync, pending_responses: Default::default() }
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
				// TODO: report peer
				// self.peerset_handle.report_peer(peer_id, rep::BAD_MESSAGE);
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
			match self
				.chain_sync
				.on_block_data(&peer_id, Some(request), block_response)
			{
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

	// TODO: implement
	fn disconnect_and_report_peer(&mut self, _id: PeerId, _score_diff: ReputationChange) {
		todo!();
	}
}
