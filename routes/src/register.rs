// This file is part of RegionX.
//
// RegionX is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// RegionX is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with RegionX.  If not, see <https://www.gnu.org/licenses/>.

use crate::{
	register::polkadot::runtime_types::{
		frame_system::pallet::Call as SystemCall, pallet_balances::pallet::Call as BalancesCall,
		pallet_utility::pallet::Call as UtilityCall,
	},
	*,
};
use parity_scale_codec::Encode;
use polkadot_core_primitives::BlockNumber;
use rocket::{post, serde::json::Json};
use shared::{
	config::{config, PaymentInfo},
	registry::{registered_para, registered_paras, update_registry},
};
use subxt::{
	backend::rpc::{rpc_params, RpcClient},
	blocks::Block,
	utils::H256,
	OnlineClient, PolkadotConfig,
};
use types::Parachain;

#[subxt::subxt(runtime_metadata_path = "../artifacts/metadata.scale")]
mod polkadot {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "rocket::serde")]
pub struct RegistrationData {
	/// The parachain getting registered.
	pub para: Parachain,
	/// The block in which the payment occurred for the specific parachain.
	///
	/// In free mode (where payment is not required), this is ignored and can be `None`.
	/// Otherwise, it should contain a valid block number.
	pub payment_block_number: Option<BlockNumber>,
}

/// Register a parachain for resource utilization tracking.
#[post("/register_para", data = "<registration_data>")]
pub async fn register_para(registration_data: Json<RegistrationData>) -> Result<(), Error> {
	let para = registration_data.para.clone();

	log::info!(
		target: LOG_TARGET,
		"Attempting to register para: {}:{}",
		para.relay_chain, para.para_id
	);

	let mut paras = registered_paras();

	if registered_para(para.relay_chain.clone(), para.para_id).is_some() {
		return Err(Error::AlreadyRegistered);
	}

	if let Some(payment_info) = config().payment_info {
		let payment_block_number =
			registration_data.payment_block_number.ok_or(Error::PaymentRequired)?;

		validate_registration_payment(para.clone(), payment_info, payment_block_number).await?;
	}

	paras.push(para);

	if let Err(err) = update_registry(paras) {
		log::error!(
			target: LOG_TARGET,
			"Failed to register para: {:?}",
			err
		);
	}

	Ok(())
}

async fn validate_registration_payment(
	para: Parachain,
	payment_info: PaymentInfo,
	payment_block_number: BlockNumber,
) -> Result<(), Error> {
	// TODO: Could this code be improved so that we don't have to instantiate both clients?
	let rpc_client = RpcClient::from_url(&payment_info.rpc_url.clone())
		.await
		.map_err(|_| Error::PaymentValidationFailed)?;

	let online_client = OnlineClient::<PolkadotConfig>::from_url(payment_info.rpc_url.clone())
		.await
		.map_err(|_| Error::PaymentValidationFailed)?;

	// Ensure that the `payment_block_number` is from a finalized block.
	let last_finalized =
		get_last_finalized_block(rpc_client.clone(), online_client.clone()).await?;
	if payment_block_number > last_finalized {
		return Err(Error::UnfinalizedPayment)
	}

	let block_hash = get_block_hash(rpc_client, payment_block_number).await?;
	let block = get_block(online_client, block_hash).await?;

	ensure_contains_payment(para, payment_info, block).await
}

async fn ensure_contains_payment(
	para: Parachain,
	payment_info: PaymentInfo,
	block: Block<PolkadotConfig, OnlineClient<PolkadotConfig>>,
) -> Result<(), Error> {
	let payment = opaque_payment_extrinsic(para, payment_info).await?;

	let extrinsics = block.extrinsics().await.map_err(|_| Error::PaymentValidationFailed)?;
	let extrinsics: Vec<Vec<u8>> = extrinsics
		.iter()
		.filter_map(|ext| {
			ext.as_ref().ok().and_then(|e| e.as_root_extrinsic::<polkadot::Call>().ok())
		})
		.map(|ext| ext.encode())
		.collect();

	if extrinsics.contains(&payment.encode()) {
		Ok(())
	} else {
		Err(Error::PaymentNotFound)
	}
}

async fn opaque_payment_extrinsic(
	para: Parachain,
	payment_info: PaymentInfo,
) -> Result<polkadot::Call, Error> {
	if let Ok(cost) = payment_info.cost.parse::<u128>() {
		let transfer_call = polkadot::Call::Balances(BalancesCall::transfer_keep_alive {
			dest: payment_info.receiver.into(),
			value: cost,
		});

		let remark = format!("{}:{}", para.relay_chain, para.para_id).as_bytes().to_vec();
		let remark_call = polkadot::Call::System(SystemCall::remark { remark });

		let batch_call = polkadot::Call::Utility(UtilityCall::batch_all {
			calls: vec![transfer_call, remark_call],
		});

		Ok(batch_call)
	} else {
		log::error!(
			target: LOG_TARGET,
			"Failed to parse cost",
		);
		Err(Error::PaymentValidationFailed)
	}
}

async fn get_last_finalized_block(
	rpc_client: RpcClient,
	online_client: OnlineClient<PolkadotConfig>,
) -> Result<BlockNumber, Error> {
	let params = rpc_params![];
	let block_hash: H256 = rpc_client
		.request("chain_getFinalizedHead", params)
		.await
		.map_err(|_| Error::PaymentValidationFailed)?;

	let block = get_block(online_client, block_hash).await?;

	Ok(block.number())
}

async fn get_block(
	api: OnlineClient<PolkadotConfig>,
	block_hash: H256,
) -> Result<Block<PolkadotConfig, OnlineClient<PolkadotConfig>>, Error> {
	api.blocks().at(block_hash).await.map_err(|_| Error::PaymentValidationFailed)
}

async fn get_block_hash(rpc_client: RpcClient, block_number: BlockNumber) -> Result<H256, Error> {
	let params = rpc_params![Some(block_number)];
	let block_hash: H256 = rpc_client
		.request("chain_getBlockHash", params)
		.await
		.map_err(|_| Error::PaymentValidationFailed)?;

	Ok(block_hash)
}
