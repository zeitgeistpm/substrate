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

use crate::*;
use frame_support::pallet_prelude::*;

impl<T: Config<I>, I: 'static> Pallet<T, I> {
	pub fn do_set_item_metadata(
		maybe_check_owner: Option<T::AccountId>,
		collection: T::CollectionId,
		item: T::ItemId,
		data: BoundedVec<u8, T::StringLimit>,
	) -> DispatchResult {
		let mut collection_details =
			Collection::<T, I>::get(&collection).ok_or(Error::<T, I>::UnknownCollection)?;

		let collection_settings = Self::get_collection_settings(&collection)?;

		if let Some(check_owner) = &maybe_check_owner {
			ensure!(check_owner == &collection_details.owner, Error::<T, I>::NoPermission);
		}

		ItemMetadataOf::<T, I>::try_mutate_exists(collection, item, |metadata| {
			if metadata.is_none() {
				collection_details.item_metadatas.saturating_inc();
			}
			let old_deposit = metadata.take().map_or(Zero::zero(), |m| m.deposit);
			collection_details.total_deposit.saturating_reduce(old_deposit);
			let mut deposit = Zero::zero();
			if !collection_settings.contains(CollectionSetting::FreeHolding) &&
				maybe_check_owner.is_some()
			{
				deposit = T::DepositPerByte::get()
					.saturating_mul(((data.len()) as u32).into())
					.saturating_add(T::MetadataDepositBase::get());
			}
			if deposit > old_deposit {
				T::Currency::reserve(&collection_details.owner, deposit - old_deposit)?;
			} else if deposit < old_deposit {
				T::Currency::unreserve(&collection_details.owner, old_deposit - deposit);
			}
			collection_details.total_deposit.saturating_accrue(deposit);

			*metadata = Some(ItemMetadata { deposit, data: data.clone() });

			Collection::<T, I>::insert(&collection, &collection_details);
			Self::deposit_event(Event::MetadataSet { collection, item, data });
			Ok(())
		})
	}

	pub fn do_clear_item_metadata(
		maybe_check_owner: Option<T::AccountId>,
		collection: T::CollectionId,
		item: T::ItemId,
	) -> DispatchResult {
		let mut collection_details =
			Collection::<T, I>::get(&collection).ok_or(Error::<T, I>::UnknownCollection)?;

		if let Some(check_owner) = &maybe_check_owner {
			ensure!(check_owner == &collection_details.owner, Error::<T, I>::NoPermission);
		}

		ItemMetadataOf::<T, I>::try_mutate_exists(collection, item, |metadata| {
			if metadata.is_some() {
				collection_details.item_metadatas.saturating_dec();
			}
			let deposit = metadata.take().ok_or(Error::<T, I>::UnknownCollection)?.deposit;
			T::Currency::unreserve(&collection_details.owner, deposit);
			collection_details.total_deposit.saturating_reduce(deposit);

			Collection::<T, I>::insert(&collection, &collection_details);
			Self::deposit_event(Event::MetadataCleared { collection, item });
			Ok(())
		})
	}

	pub fn do_set_collection_metadata(
		maybe_check_owner: Option<T::AccountId>,
		collection: T::CollectionId,
		data: BoundedVec<u8, T::StringLimit>,
		settings: CollectionSettings,
	) -> DispatchResult {
		let mut details =
			Collection::<T, I>::get(&collection).ok_or(Error::<T, I>::UnknownCollection)?;

		if let Some(check_owner) = &maybe_check_owner {
			ensure!(check_owner == &details.owner, Error::<T, I>::NoPermission);
		}

		CollectionMetadataOf::<T, I>::try_mutate_exists(collection, |metadata| {
			let old_deposit = metadata.take().map_or(Zero::zero(), |m| m.deposit);
			details.total_deposit.saturating_reduce(old_deposit);
			let mut deposit = Zero::zero();
			if maybe_check_owner.is_some() && !settings.contains(CollectionSetting::FreeHolding) {
				deposit = T::DepositPerByte::get()
					.saturating_mul(((data.len()) as u32).into())
					.saturating_add(T::MetadataDepositBase::get());
			}
			if deposit > old_deposit {
				T::Currency::reserve(&details.owner, deposit - old_deposit)?;
			} else if deposit < old_deposit {
				T::Currency::unreserve(&details.owner, old_deposit - deposit);
			}
			details.total_deposit.saturating_accrue(deposit);

			Collection::<T, I>::insert(&collection, details);

			*metadata = Some(CollectionMetadata { deposit, data: data.clone() });

			Self::deposit_event(Event::CollectionMetadataSet { collection, data });
			Ok(())
		})
	}

	pub fn do_clear_collection_metadata(
		maybe_check_owner: Option<T::AccountId>,
		collection: T::CollectionId,
	) -> DispatchResult {
		let details =
			Collection::<T, I>::get(&collection).ok_or(Error::<T, I>::UnknownCollection)?;

		if let Some(check_owner) = &maybe_check_owner {
			ensure!(check_owner == &details.owner, Error::<T, I>::NoPermission);
		}

		CollectionMetadataOf::<T, I>::try_mutate_exists(collection, |metadata| {
			let deposit = metadata.take().ok_or(Error::<T, I>::UnknownCollection)?.deposit;
			T::Currency::unreserve(&details.owner, deposit);
			Self::deposit_event(Event::CollectionMetadataCleared { collection });
			Ok(())
		})
	}
}
