// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.

// Cumulus is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Cumulus is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus.  If not, see <http://www.gnu.org/licenses/>.

//! Bridge definitions used for bridging with Kusama Bridge Hub.

use crate::{
	weights,
	xcm_config::{UniversalLocation, XcmRouter},
	AccountId, Balance, Balances, BlockNumber, BridgeKusamaMessages, PolkadotXcm, Runtime,
	RuntimeEvent, RuntimeHoldReason, XcmOverBridgeHubKusama,
};

use bp_messages::LegacyLaneId;
use bp_parachains::SingleParaStoredHeaderDataBuilder;
use bp_runtime::{Chain, UnderlyingChainProvider};

use bridge_hub_common::xcm_version::XcmVersionOfDestAndRemoteBridge;
use frame_support::{parameter_types, traits::PalletInfoAccess};
use frame_system::{EnsureNever, EnsureRoot};
use pallet_bridge_messages::LaneIdOf;
use pallet_bridge_relayers::extension::{
	BridgeRelayersSignedExtension, WithMessagesExtensionConfig,
};
use pallet_xcm_bridge_hub::XcmAsPlainPayload;
use parachains_common::xcm_config::{AllSiblingSystemParachains, RelayOrOtherSystemParachains};
use polkadot_parachain_primitives::primitives::Sibling;
use polkadot_runtime_constants::{self as constants, DOLLARS};
use sp_runtime::{traits::ConstU32, RuntimeDebug};
use xcm::latest::prelude::*;
use xcm_builder::{BridgeBlobDispatcher, ParentIsPreset, SiblingParachainConvertsVia};

/// Lane identifier, used to connect Polkadot Asset Hub and Kusama Asset Hub.
pub const XCM_LANE_FOR_ASSET_HUB_POLKADOT_TO_ASSET_HUB_KUSAMA: LegacyLaneId =
	LegacyLaneId([0, 0, 0, 1]);

// Parameters that may be changed by the governance.
parameter_types! {
	/// Reward that is paid (by the Polkadot Asset Hub) to relayers for delivering a single
	/// Polkadot -> Kusama bridge message.
	///
	/// This payment is tracked by the `pallet_bridge_relayers` pallet at the Polkadot
	/// Bridge Hub.
	pub storage DeliveryRewardInBalance: Balance = constants::currency::UNITS / 2_000;

	/// Registered relayer stake.
	///
	/// Any relayer may reserve this amount on his account and get a priority boost for his
	/// message delivery transactions. In exchange, he risks losing his stake if he would
	/// submit an invalid transaction. The set of such (registered) relayers is tracked
	/// by the `pallet_bridge_relayers` pallet at the Polkadot Bridge Hub.
	pub storage RequiredStakeForStakeAndSlash: Balance = 500 * constants::currency::UNITS;
}

// Parameters, used by both XCM and bridge code.
parameter_types! {
	/// Kusama Network identifier.
	pub KusamaGlobalConsensusNetwork: NetworkId = NetworkId::Kusama;
	/// Kusama Network as `Location`.
	pub KusamaGlobalConsensusNetworkLocation: Location = Location {
		parents: 2,
		interior: [GlobalConsensus(KusamaGlobalConsensusNetwork::get())].into()
	};
	/// Interior location (relative to this runtime) of the with-Kusama messages pallet.
	pub BridgePolkadotToKusamaMessagesPalletInstance: InteriorLocation = PalletInstance(<BridgeKusamaMessages as PalletInfoAccess>::index() as u8).into();

	/// Identifier of the sibling Polkadot Asset Hub parachain.
	pub AssetHubPolkadotParaId: cumulus_primitives_core::ParaId = polkadot_runtime_constants::system_parachain::ASSET_HUB_ID.into();
	/// Identifier of the bridged Kusama Asset Hub parachain.
	pub AssetHubKusamaParaId: cumulus_primitives_core::ParaId = kusama_runtime_constants::system_parachain::ASSET_HUB_ID.into();
	/// Location of the bridged Kusama Bridge Hub parachain.
	pub BridgeHubKusamaLocation: Location = Location {
		parents: 2,
		interior: [
			GlobalConsensus(KusamaGlobalConsensusNetwork::get()),
			Parachain(<bp_bridge_hub_kusama::BridgeHubKusama as bp_runtime::Parachain>::PARACHAIN_ID)
		].into()
	};
	/// Lane identifier, used to connect Polkadot Asset Hub and Kusama Asset Hub.
	pub const AssetHubPolkadotToAssetHubKusamaMessagesLane: bp_messages::LegacyLaneId
		= XCM_LANE_FOR_ASSET_HUB_POLKADOT_TO_ASSET_HUB_KUSAMA;
}

// Parameters, used by bridge transport code.
parameter_types! {
	/// Number of Kusama headers to keep in the runtime storage.
	///
	/// Note that we are keeping only required header information, not the whole header itself. Roughly, it
	/// is the 2 hours of real time (assuming that every header is submitted).
	pub const RelayChainHeadersToKeep: u32 = 1_200;
	/// Number of Kusama Bridge Hub headers to keep in the runtime storage.
	///
	/// Note that we are keeping only required header information, not the whole header itself. Roughly, it
	/// is the 2 hours of real time (assuming that every header is submitted).
	pub const ParachainHeadsToKeep: u32 = 600;
	/// Maximal size of Kusama Bridge Hub header **part** that we are storing in the runtime storage.
	pub const MaxParaHeadDataSize: u32 = bp_kusama::MAX_NESTED_PARACHAIN_HEAD_DATA_SIZE;

	/// Bridge specific chain (network) identifier of the Kusama Bridge Hub.
	pub const BridgeHubKusamaChainId: bp_runtime::ChainId = bp_bridge_hub_kusama::BridgeHubKusama::ID;
	/// Name of the `paras` pallet at Kusama that tracks all parachain heads.
	pub const ParachainPalletNameAtKusama: &'static str = bp_kusama::PARAS_PALLET_NAME;

	/// Maximal number of entries in the unrewarded relayers vector at the Polkadot Bridge Hub. It matches the
	/// maximal number of unrewarded relayers that the single confirmation transaction at Kusama Bridge
	/// Hub may process.
	pub const MaxUnrewardedRelayerEntriesAtInboundLane: bp_messages::MessageNonce =
		bp_bridge_hub_kusama::MAX_UNREWARDED_RELAYERS_IN_CONFIRMATION_TX;
	/// Maximal number of unconfirmed messages at the Polkadot Bridge Hub. It matches the maximal number of
	/// uncinfirmed messages that the single confirmation transaction at Kusama Bridge Hub may process.
	pub const MaxUnconfirmedMessagesAtInboundLane: bp_messages::MessageNonce =
		bp_bridge_hub_kusama::MAX_UNCONFIRMED_MESSAGES_IN_CONFIRMATION_TX;

	/// Reserve identifier, used by the `pallet_bridge_relayers` to hold funds of registered relayer.
	pub const RelayerStakeReserveId: [u8; 8] = *b"brdgrlrs";
	/// Minimal period of relayer registration. Roughly, it is the 1 hour of real time.
	pub const RelayerStakeLease: u32 = 300;

	// see the `FEE_BOOST_PER_RELAY_HEADER` constant get the meaning of this value
	pub PriorityBoostPerRelayHeader: u64 = 220_053_724_053;
	// see the `FEE_BOOST_PER_PARACHAIN_HEADER` constant get the meaning of this value
	pub PriorityBoostPerParachainHeader: u64 = 9_182_241_758_241;
	// see the `FEE_BOOST_PER_MESSAGE` constant to get the meaning of this value
	pub PriorityBoostPerMessage: u64 = 1_820_444_444_444;
	// TODO: @acatangiu, is there any specs about the deposit cost?
	pub storage BridgeDeposit: Balance = 10 * DOLLARS;
}

/// Dispatches received XCM messages from other bridge
pub type FromKusamaMessageBlobDispatcher = BridgeBlobDispatcher<
	XcmRouter,
	UniversalLocation,
	BridgePolkadotToKusamaMessagesPalletInstance,
>;

/// Add GRANDPA bridge pallet to track Kusama relay chain.
pub type BridgeGrandpaKusamaInstance = pallet_bridge_grandpa::Instance1;
impl pallet_bridge_grandpa::Config<BridgeGrandpaKusamaInstance> for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type BridgedChain = bp_kusama::Kusama;
	type HeadersToKeep = RelayChainHeadersToKeep;
	type MaxFreeHeadersPerBlock = ConstU32<4>;
	type FreeHeadersInterval = ConstU32<5>;
	type WeightInfo = weights::pallet_bridge_grandpa::WeightInfo<Runtime>;
}

/// Add parachain bridge pallet to track Kusama BridgeHub parachain.
pub type BridgeParachainKusamaInstance = pallet_bridge_parachains::Instance1;
impl pallet_bridge_parachains::Config<BridgeParachainKusamaInstance> for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type WeightInfo = weights::pallet_bridge_parachains::WeightInfo<Runtime>;
	type BridgesGrandpaPalletInstance = BridgeGrandpaKusamaInstance;
	type ParasPalletName = ParachainPalletNameAtKusama;
	type ParaStoredHeaderDataBuilder =
		SingleParaStoredHeaderDataBuilder<bp_bridge_hub_kusama::BridgeHubKusama>;
	type HeadsToKeep = ParachainHeadsToKeep;
	type MaxParaHeadDataSize = MaxParaHeadDataSize;
}

/// Allows collect and claim rewards for relayers.
impl pallet_bridge_relayers::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type Reward = Balance;
	type PaymentProcedure = bp_relayers::PayRewardFromAccount<
		pallet_balances::Pallet<Runtime>,
		AccountId,
		Self::LaneId,
	>;
	type StakeAndSlash = pallet_bridge_relayers::StakeAndSlashNamed<
		AccountId,
		BlockNumber,
		Balances,
		RelayerStakeReserveId,
		RequiredStakeForStakeAndSlash,
		RelayerStakeLease,
	>;
	type WeightInfo = weights::pallet_bridge_relayers::WeightInfo<Runtime>;
	type LaneId = LegacyLaneId;
}

/// Add XCM messages support for exchanging messages with BridgeHubKusama.
pub type WithBridgeHubKusamaMessagesInstance = pallet_bridge_messages::Instance1;
impl pallet_bridge_messages::Config<WithBridgeHubKusamaMessagesInstance> for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type WeightInfo = weights::pallet_bridge_messages::WeightInfo<Runtime>;

	type ThisChain = bp_bridge_hub_polkadot::BridgeHubPolkadot;
	type BridgedChain = bp_bridge_hub_kusama::BridgeHubKusama;
	type BridgedHeaderChain = pallet_bridge_parachains::ParachainHeaders<
		Runtime,
		BridgeParachainKusamaInstance,
		bp_bridge_hub_kusama::BridgeHubKusama,
	>;

	type OutboundPayload = XcmAsPlainPayload;
	type InboundPayload = XcmAsPlainPayload;
	type LaneId = LegacyLaneId;

	type DeliveryPayments = ();
	type DeliveryConfirmationPayments = pallet_bridge_relayers::DeliveryConfirmationPaymentsAdapter<
		Runtime,
		WithBridgeHubKusamaMessagesInstance,
		DeliveryRewardInBalance,
	>;

	type MessageDispatch = XcmOverBridgeHubKusama;
	type OnMessagesDelivered = XcmOverBridgeHubKusama;
}

/// Add support for the export and dispatch of XCM programs.
pub type XcmOverBridgeHubKusamaInstance = pallet_xcm_bridge_hub::Instance1;
impl pallet_xcm_bridge_hub::Config<XcmOverBridgeHubKusamaInstance> for Runtime {
	type RuntimeEvent = RuntimeEvent;

	type UniversalLocation = UniversalLocation;
	type BridgedNetwork = KusamaGlobalConsensusNetworkLocation;
	type BridgeMessagesPalletInstance = WithBridgeHubKusamaMessagesInstance;
	// `MessageExportPrice` is simply propagated to the inner `xcm_builder::HaulBlobExporter`, and
	// we do not need or want to add any additional price for exporting here, as it is already
	// covered by the measured weight of the `ExportMessage` instruction.
	type MessageExportPrice = ();
	type DestinationVersion = XcmVersionOfDestAndRemoteBridge<PolkadotXcm, BridgeHubKusamaLocation>;

	type ForceOrigin = EnsureRoot<AccountId>;
	// We don't want to allow creating bridges for this instance with `LegacyLaneId`.
	type OpenBridgeOrigin = EnsureNever<Location>;
	// Converter aligned with `OpenBridgeOrigin`.
	type BridgeOriginAccountIdConverter =
		(ParentIsPreset<AccountId>, SiblingParachainConvertsVia<Sibling, AccountId>);

	type BridgeDeposit = BridgeDeposit;
	type Currency = Balances;
	type RuntimeHoldReason = RuntimeHoldReason;
	type AllowWithoutBridgeDeposit =
		RelayOrOtherSystemParachains<AllSiblingSystemParachains, Runtime>;

	// TODO: @acatangiu (bridges-v2) - add `LocalXcmChannelManager` impl - https://github.com/paritytech/parity-bridges-common/issues/3047
	type LocalXcmChannelManager = ();
	type BlobDispatcher = FromKusamaMessageBlobDispatcher;
}

/// BridgeHubKusama chain from message lane point of view.
#[derive(RuntimeDebug, Clone, Copy)]
pub struct BridgeHubKusama;

impl UnderlyingChainProvider for BridgeHubKusama {
	type Chain = bp_bridge_hub_kusama::BridgeHubKusama;
}

/// BridgeHubPolkadot chain from message lane point of view.
#[derive(RuntimeDebug, Clone, Copy)]
pub struct BridgeHubPolkadot;

impl UnderlyingChainProvider for BridgeHubPolkadot {
	type Chain = bp_bridge_hub_polkadot::BridgeHubPolkadot;
}

pub type RelayersForLegacyLaneIdsMessagesInstance = ();

/// Signed extension that refunds relayers that are delivering messages from the Kusama parachain.
pub type OnBridgeHubPolkadotRefundBridgeHubKusamaMessages = BridgeRelayersSignedExtension<
	Runtime,
	WithMessagesExtensionConfig<
		StrOnBridgeHubPolkadotRefundBridgeHubKusamaMessages,
		Runtime,
		WithBridgeHubKusamaMessagesInstance,
		RelayersForLegacyLaneIdsMessagesInstance,
		PriorityBoostPerMessage,
	>,
	LaneIdOf<Runtime, WithBridgeHubKusamaMessagesInstance>,
>;
bp_runtime::generate_static_str_provider!(OnBridgeHubPolkadotRefundBridgeHubKusamaMessages);

#[cfg(test)]
mod tests {
	use super::*;
	use bridge_runtime_common::{
		assert_complete_bridge_types,
		integrity::{
			assert_complete_with_parachain_bridge_constants, check_message_lane_weights,
			AssertChainConstants, AssertCompleteBridgeConstants,
		},
	};

	/// Every additional message in the message delivery transaction boosts its priority.
	/// So the priority of transaction with `N+1` messages is larger than priority of
	/// transaction with `N` messages by the `PriorityBoostPerMessage`.
	///
	/// Economically, it is an equivalent of adding tip to the transaction with `N` messages.
	/// The `FEE_BOOST_PER_MESSAGE` constant is the value of this tip.
	///
	/// We want this tip to be large enough (delivery transactions with more messages = less
	/// operational costs and a faster bridge), so this value should be significant.
	const FEE_BOOST_PER_MESSAGE: Balance = 2 * constants::currency::UNITS;
	// see `FEE_BOOST_PER_MESSAGE` comment
	const FEE_BOOST_PER_RELAY_HEADER: Balance = 2 * constants::currency::UNITS;
	// see `FEE_BOOST_PER_MESSAGE` comment
	const FEE_BOOST_PER_PARACHAIN_HEADER: Balance = 2 * constants::currency::UNITS;

	#[test]
	fn ensure_bridge_hub_polkadot_message_lane_weights_are_correct() {
		check_message_lane_weights::<
			bp_bridge_hub_polkadot::BridgeHubPolkadot,
			Runtime,
			WithBridgeHubKusamaMessagesInstance,
		>(
			bp_bridge_hub_kusama::EXTRA_STORAGE_PROOF_SIZE,
			bp_bridge_hub_polkadot::MAX_UNREWARDED_RELAYERS_IN_CONFIRMATION_TX,
			bp_bridge_hub_polkadot::MAX_UNCONFIRMED_MESSAGES_IN_CONFIRMATION_TX,
			true,
		);
	}

	#[test]
	fn ensure_bridge_integrity() {
		assert_complete_bridge_types!(
			runtime: Runtime,
			with_bridged_chain_grandpa_instance: BridgeGrandpaKusamaInstance,
			with_bridged_chain_messages_instance: WithBridgeHubKusamaMessagesInstance,
			this_chain: bp_bridge_hub_polkadot::BridgeHubPolkadot,
			bridged_chain: bp_bridge_hub_kusama::BridgeHubKusama,
		);

		assert_complete_with_parachain_bridge_constants::<
			Runtime,
			BridgeGrandpaKusamaInstance,
			WithBridgeHubKusamaMessagesInstance,
			bp_kusama::Kusama,
		>(AssertCompleteBridgeConstants {
			this_chain_constants: AssertChainConstants {
				block_length: bp_bridge_hub_polkadot::BlockLength::get(),
				block_weights: bp_bridge_hub_polkadot::BlockWeights::get(),
			},
		});

		pallet_bridge_relayers::extension::per_relay_header::ensure_priority_boost_is_sane::<
			Runtime,
			BridgeGrandpaKusamaInstance,
			PriorityBoostPerRelayHeader,
		>(FEE_BOOST_PER_RELAY_HEADER);

		pallet_bridge_relayers::extension::per_parachain_header::ensure_priority_boost_is_sane::<
			Runtime,
			WithBridgeHubKusamaMessagesInstance,
			bp_bridge_hub_kusama::BridgeHubKusama,
			PriorityBoostPerParachainHeader,
		>(FEE_BOOST_PER_PARACHAIN_HEADER);

		pallet_bridge_relayers::extension::per_message::ensure_priority_boost_is_sane::<
			Runtime,
			WithBridgeHubKusamaMessagesInstance,
			PriorityBoostPerMessage,
		>(FEE_BOOST_PER_MESSAGE);

		assert_eq!(
			BridgePolkadotToKusamaMessagesPalletInstance::get(),
			Into::<InteriorLocation>::into(PalletInstance(
				bp_bridge_hub_polkadot::WITH_BRIDGE_POLKADOT_TO_KUSAMA_MESSAGES_PALLET_INDEX
			))
		);

		assert!(BridgeHubKusamaLocation::get()
			.starts_with(&KusamaGlobalConsensusNetworkLocation::get()));
	}
}
