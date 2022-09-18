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
}
