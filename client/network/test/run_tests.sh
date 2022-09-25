#!/bin/bash

cargo test --release block_import::import_single_good_known_block_is_ignored -- --nocapture
cargo test --release block_import::import_single_good_block_without_header_fails -- --nocapture &&
cargo test --release block_import::import_single_good_block_works -- --nocapture &&
cargo test --release sync::can_sync_forks_ahead_of_the_best_chain -- --nocapture &&
cargo test --release sync::can_sync_explicit_forks -- --nocapture &&
cargo test --release sync::can_sync_small_non_best_forks -- --nocapture &&
cargo test --release sync::ancestry_search_works_when_backoff_is_one -- --nocapture &&
cargo test --release sync::can_sync_to_peers_with_wrong_common_block -- --nocapture &&
cargo test --release sync::block_announce_data_is_propagated -- --nocapture &&
cargo test --release sync::ancestry_search_works_when_common_is_two -- --nocapture &&
cargo test --release sync::ancestry_search_works_when_ancestor_is_genesis -- --nocapture &&
cargo test --release sync::full_sync_requires_block_body -- --nocapture &&
cargo test --release sync::ancestry_search_works_when_common_is_one -- --nocapture &&
cargo test --release sync::ancestry_search_works_when_common_is_hundred -- --nocapture &&
cargo test --release sync::sync_blocks_when_block_announce_validator_says_it_is_new_best -- --nocapture &&
cargo test --release sync::imports_stale_once -- --nocapture &&
cargo test --release sync::does_not_sync_announced_old_best_block -- --nocapture &&
cargo test --release sync::own_blocks_are_announced -- --nocapture &&
cargo test --release sync::sync_cycle_from_offline_to_syncing_to_offline -- --nocapture &&
cargo test --release sync::sync_after_fork_works -- --nocapture &&
cargo test --release sync::sync_justifications -- --nocapture &&
cargo test --release sync::sync_peers_works -- --nocapture &&
cargo test --release sync::sync_justifications_across_forks -- --nocapture &&
cargo test --release sync::sync_from_two_peers_works -- --nocapture &&
cargo test --release sync::sync_from_two_peers_with_ancestry_search_works -- --nocapture &&
cargo test --release sync::sync_long_chain_works -- --nocapture &&
cargo test --release sync::sync_no_common_longer_chain_fails -- --nocapture &&
cargo test --release sync::sync_to_tip_requires_that_sync_protocol_is_informed_about_best_block -- --nocapture &&
cargo test --release sync::syncs_after_missing_announcement -- --nocapture &&
cargo test --release sync::syncs_header_only_forks -- --nocapture &&
cargo test --release sync::syncs_indexed_blocks -- --nocapture &&
cargo test --release sync::syncing_node_not_major_syncing_when_disconnected -- --nocapture &&
cargo test --release sync::syncs_all_forks_from_single_peer -- --nocapture &&
cargo test --release sync::wait_until_deferred_block_announce_validation_is_ready -- --nocapture &&
cargo test --release sync::syncs_all_forks -- --nocapture &&
cargo test --release sync::syncs_state -- --nocapture &&
cargo test --release sync::warp_sync -- --nocapture &&
cargo test --release sync::continue_to_sync_after_some_block_announcement_verifications_failed -- --nocapture &&
cargo test --release block_import::async_import_queue_drops -- --nocapture &&
cargo test --release sync::syncs_huge_blocks -- --nocapture &&
cargo test --release sync::multiple_requests_are_accepted_as_long_as_they_are_not_fulfilled -- --nocapture &&
cargo test --release sync::sync_to_tip_when_we_sync_together_with_multiple_peers -- --nocapture
