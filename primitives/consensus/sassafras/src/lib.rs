// This file is part of Substrate.

// Copyright (C) 2022 Parity Technologies (UK) Ltd.
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

//! Primitives for Sassafras
//! TODO-SASS-P2 : write proper docs

#![deny(warnings)]
#![forbid(unsafe_code, missing_docs, unused_variables, unused_imports)]
#![cfg_attr(not(feature = "std"), no_std)]

use scale_codec::{Decode, Encode, MaxEncodedLen};
use scale_info::TypeInfo;
#[cfg(feature = "std")]
use serde::{Deserialize, Serialize};
use sp_core::{crypto, U256};
use sp_runtime::{ConsensusEngineId, RuntimeDebug};
use sp_std::vec::Vec;

pub use sp_consensus_slots::{Slot, SlotDuration};
pub use sp_consensus_vrf::schnorrkel::{
	PublicKey, Randomness, VRFOutput, VRFProof, RANDOMNESS_LENGTH, VRF_OUTPUT_LENGTH,
	VRF_PROOF_LENGTH,
};

pub mod digests;
pub mod inherents;
pub mod vrf;

mod app {
	use sp_application_crypto::{app_crypto, key_types::SASSAFRAS, sr25519};
	app_crypto!(sr25519, SASSAFRAS);
}

/// Key type for Sassafras protocol.
pub const KEY_TYPE: crypto::KeyTypeId = sp_application_crypto::key_types::SASSAFRAS;

/// The index of an authority.
pub type AuthorityIndex = u32;

/// Sassafras authority keypair. Necessarily equivalent to the schnorrkel public key used in
/// the main Sassafras module. If that ever changes, then this must, too.
#[cfg(feature = "std")]
pub type AuthorityPair = app::Pair;

/// Sassafras authority signature.
pub type AuthoritySignature = app::Signature;

/// Sassafras authority identifier. Necessarily equivalent to the schnorrkel public key used in
/// the main Sassafras module. If that ever changes, then this must, too.
pub type AuthorityId = app::Public;

/// The `ConsensusEngineId` of BABE.
pub const SASSAFRAS_ENGINE_ID: ConsensusEngineId = *b"SASS";

/// The length of the public key
pub const PUBLIC_KEY_LENGTH: usize = 32;

/// The weight of an authority.
// NOTE: we use a unique name for the weight to avoid conflicts with other
// `Weight` types, since the metadata isn't able to disambiguate.
pub type SassafrasAuthorityWeight = u64;

/// Weight of a Sassafras block.
/// Primary blocks have a weight of 1 whereas secondary blocks have a weight of 0.
pub type SassafrasBlockWeight = u32;

/// Configuration data used by the Sassafras consensus engine.
#[derive(Clone, Encode, Decode, RuntimeDebug, PartialEq, Eq)]
pub struct SassafrasConfiguration {
	/// The slot duration in milliseconds.
	pub slot_duration: u64,
	/// The duration of epochs in slots.
	pub epoch_duration: u64,
	/// The authorities for the epoch.
	pub authorities: Vec<(AuthorityId, SassafrasAuthorityWeight)>,
	/// The randomness for the epoch.
	pub randomness: Randomness,
	/// Tickets threshold parameters.
	pub threshold_params: SassafrasEpochConfiguration,
}

impl SassafrasConfiguration {
	/// Get the slot duration defined in the genesis configuration.
	pub fn slot_duration(&self) -> SlotDuration {
		SlotDuration::from_millis(self.slot_duration)
	}
}

/// Configuration data used by the Sassafras consensus engine that can be modified on epoch change.
#[derive(Clone, PartialEq, Eq, Encode, Decode, RuntimeDebug, MaxEncodedLen, TypeInfo, Default)]
#[cfg_attr(feature = "std", derive(Serialize, Deserialize))]
pub struct SassafrasEpochConfiguration {
	/// Redundancy factor.
	pub redundancy_factor: u32,
	/// Number of attempts for tickets generation.
	pub attempts_number: u32,
}

/// Ticket type.
pub type Ticket = VRFOutput;

/// Ticket information.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode, MaxEncodedLen, TypeInfo)]
pub struct TicketInfo {
	/// Authority index.
	pub authority_index: u32,
	/// Attempt number.
	pub attempt: u32,
	/// Ticket proof.
	pub proof: VRFProof,
}

/// Computes the threshold for a given epoch as T = (x*s)/(a*v), where:
/// - x: redundancy factor;
/// - s: number of slots in epoch;
/// - a: max number of attempts;
/// - v: number of validator in epoch.
/// The parameters should be chosen such that T <= 1.
/// If `attempts * validators` is zero then we fallback to T = 0
// TODO-SASS-P3: this formula must be double-checked...
#[inline]
pub fn compute_threshold(redundancy: u32, slots: u32, attempts: u32, validators: u32) -> U256 {
	let den = attempts as u64 * validators as u64;
	let num = redundancy as u64 * slots as u64;
	U256::max_value()
		.checked_div(den.into())
		.unwrap_or(U256::zero())
		.saturating_mul(num.into())
}

/// Returns true if the given VRF output is lower than the given threshold, false otherwise.
#[inline]
pub fn check_threshold(ticket: &Ticket, threshold: U256) -> bool {
	U256::from(ticket.as_bytes()) < threshold
}

// Runtime API.
sp_api::decl_runtime_apis! {
	/// API necessary for block authorship with Sassafras.
	pub trait SassafrasApi {
		 /// Return the genesis configuration for Sassafras. The configuration is only read on genesis.
		fn configuration() -> SassafrasConfiguration;

		/// Submit next epoch validator tickets via an unsigned extrinsic.
		/// This method returns `false` when creation of the extrinsics fails.
		fn submit_tickets_unsigned_extrinsic(tickets: Vec<Ticket>) -> bool;

		/// Get expected ticket for the given slot.
		fn slot_ticket(slot: Slot) -> Option<Ticket>;
	}
}