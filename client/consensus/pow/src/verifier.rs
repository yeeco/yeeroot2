// Copyright (C) 2019 Yee Foundation.
//
// This file is part of YeeChain.
//
// YeeChain is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// YeeChain is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with YeeChain.  If not, see <https://www.gnu.org/licenses/>.

//! Import Queue Verifier for POW chain

use ansi_term::Colour;
use log::{error, warn};
use merkle_light::proof::Proof as MLProof;
use sp_core::H256;

use {
	std::{marker::PhantomData, sync::Arc},
};
use {
	sp_api::ProvideRuntimeApi,
	sp_blockchain::{
		HeaderBackend,
		well_known_cache_keys::Id as CacheKeyId,
	},
	sp_consensus::{
		BlockImportParams, BlockOrigin,
		ForkChoiceStrategy,
		import_queue::Verifier,
	},
	sp_inherents::InherentDataProviders,
	sp_runtime::{
		codec::{Decode, Encode},
		generic,
		Justification,
		Proof,
		traits::{
			BlakeTwo256,
			Block as BlockT, DigestItemFor,
			Header,
			NumberFor,
		},
	},
	yc_util::relay_decode::RelayTransfer,
	yp_consensus_pow::YeePOWApi,
	// foreign_chain::{ForeignChain, ForeignChainConfig},
	yp_sharding::ShardingAPI,
};
use yc_sharding::{ScaleOutPhase, ScaleOutPhaseDigestItem, ShardingDigestItem};
use yc_util_merkle::{MultiLayerProof, ProofAlgorithm, ProofHash};
use yp_context::Context;
use yp_sharding::utils::shard_num_for;

use crate::pow::{calc_pow_target, check_work_proof, gen_extrinsic_proof, PowSeal};
use crate::ShardExtra;

use super::CompatibleDigestItem;

/// Verifier for POW blocks.
pub struct PowVerifier<B, C, AccountId, AuthorityId> where
	B: BlockT
{
	pub client: Arc<C>,
	pub inherent_data_providers: InherentDataProviders,
	// pub foreign_chains: Arc<RwLock<Option<ForeignChain<F>>>>,
	pub phantom: PhantomData<AuthorityId>,
	pub shard_extra: ShardExtra<AccountId>,
	pub context: Context<B>,
}

#[forbid(deprecated)]
impl<B, C, AccountId, AuthorityId> Verifier<B> for PowVerifier<B, C, AccountId, AuthorityId> where
	B: BlockT,
	DigestItemFor<B>: CompatibleDigestItem<B, AuthorityId> + ShardingDigestItem<u16> + ScaleOutPhaseDigestItem<NumberFor<B>, u16>,
	C: Send + Sync,
	AccountId: Decode + Encode + Clone + Send + Sync + Default,
	AuthorityId: Decode + Encode + Clone + Send + Sync,
	C: HeaderBackend<B> + ProvideRuntimeApi<B>,
	C::Api: ShardingAPI<B> + YeePOWApi<B>,
	H256: From<B::Hash>,
{
	fn verify(
		&mut self,
		origin: BlockOrigin,
		header: B::Header,
		justification: Option<Justification>,
		// TODO proof: Option<Proof>,
		body: Option<Vec<B::Extrinsic>>,
	) -> Result<(BlockImportParams<B, ()>, Option<Vec<(CacheKeyId, Vec<u8>)>>), String> {
		let number = header.number().clone();
		let hash = header.hash();

		// check if header has a valid work proof
		let (pre_header, seal) = self.check_header(header, hash.clone())
			.map_err(|e| {
				error!("{}: {}", Colour::Red.paint("check header failed"), e);
				e
			})?;
		let proof_root = seal.as_pow_seal().ok_or_else(|| {
			let e = format!("Header {:?} not sealed", hash);
			error!("{}: {}", Colour::Red.paint("get proof root failed"), e);
			e
		})?.relay_proof;
		// check proof.
		// TODO
		// self.check_relay_merkle_proof(proof.clone(), proof_root)
		//     .map_err(|e| {
		//         error!("{}, number:{}, hash:{}", Colour::Red.paint("Proof validate failed"), number, hash.clone());
		//         e
		//     })?;

		// let mut res_proof = proof;
		// check body if not none
		// match self.check_body(&body, &pre_header, proof_root).map_err(|e| {
		//     error!("{}: {}", Colour::Red.paint("check body failed"), e);
		//     e
		// })? {
		//     Some(p) => res_proof = Some(p),
		//     None => {}
		// }

		let mut import_block = BlockImportParams::new(origin, pre_header);
		import_block.post_digests.push(seal);
		import_block.body = body;
		import_block.justification = justification;
		import_block.fork_choice = Some(ForkChoiceStrategy::LongestChain);
		import_block.post_hash = Some(hash);

		Ok((import_block, None))
	}
}

impl<B, C, AccountId, AuthorityId> PowVerifier<B, C, AccountId, AuthorityId> where
	B: BlockT,
	DigestItemFor<B>: CompatibleDigestItem<B, AuthorityId> + ShardingDigestItem<u16> + ScaleOutPhaseDigestItem<NumberFor<B>, u16>,
	C: Send + Sync,
	AccountId: Decode + Encode + Clone + Send + Sync + Default,
	AuthorityId: Decode + Encode + Clone + Send + Sync,
	C: HeaderBackend<B>,
	H256: From<B::Hash>,
{
	/// check body
	fn check_body(&self, body: &Option<Vec<B::Extrinsic>>, pre_header: &B::Header, proof_root: H256) -> Result<Option<Proof>, String> {
		match body.as_ref() {
			Some(exs) => {
				// check proof root
				let (root, proof) = gen_extrinsic_proof::<B>(&pre_header, &exs);
				if root != proof_root {
					return Err("Proof is invalid.".to_string());
				}
				// check relay extrinsic.
				self.check_relay_transfer(pre_header.digest().logs(), exs)?;
				return Ok(Some(proof));
			}
			None => {}
		}
		Ok(None)
	}

	/// check relay transfer merkle proof
	fn check_relay_merkle_proof(&self, proof: Option<Proof>, p_h: H256) -> Result<(), String> {
		if let Some(proof) = proof.as_ref() {
			let mut validate_proof = false;
			if let Ok(mlp) = MultiLayerProof::from_bytes(proof) {
				// check proof root.
				let checked = match mlp.layer2_root() {
					Some(root) => root == p_h,
					None => false
				};
				if !checked {
					return Err("Proof is invalid.".to_string());
				}
				// check proof self.
				if let Ok(mt_proof) = MLProof::from_bytes(mlp.layer2_proof.as_ref().unwrap().as_slice()) {
					let mt_proof: MLProof<ProofHash<BlakeTwo256>> = mt_proof;
					if mt_proof.validate::<ProofAlgorithm<BlakeTwo256>>() {
						validate_proof = true;
					}
				}
			}
			if !validate_proof {
				return Err("Proof validate failed.".to_string());
			}
		}
		Ok(())
	}

	/// check relay transfer
	fn check_relay_transfer(&self, logs: &[DigestItemFor<B>], exs: &[B::Extrinsic]) -> Result<(), String> {
		let err_str = "Block contains invalid extrinsic.";
		let shard_info: Option<(u16, u16)> = logs.iter().rev()
			.filter_map(ShardingDigestItem::as_sharding_info)
			.next();
		let (tc, _cs) = match shard_info {
			Some(info) => info,
			None => { return Err("Can't get shard info in header".to_string()); }
		};
		for tx in exs {
			match RelayTransfer::decode(tx.encode().as_slice()) {
				Some(rt) => {
					let rt: RelayTransfer<AccountId, u128, B::Hash> = rt;
					let h = rt.hash();
					let id = generic::BlockId::hash(h);
					let ds = yp_sharding::utils::shard_num_for(&rt.transfer.sender(), tc as u16)
						.expect("Internal error. Get shard num failed.");
					if let Some(lc) = self.foreign_chains.read().as_ref().unwrap().get_shard_component(ds) {
						match lc.client().proof(&id).map_err(|_| err_str)? {
							Some(proof) => {
								let proof = MultiLayerProof::from_bytes(proof.as_slice()).map_err(|_| err_str)?;
								if proof.contains(ds, h) {
									continue;
								}
							}
							None => { return Err(err_str.to_string()); }
						}
					}
					panic!("Internal error. Can't get shard component");
				}
				None => continue,
			}
		}
		Ok(())
	}

	/// Check if block header has a valid POW target
	fn check_header(&self, mut header: B::Header, hash: B::Hash) -> Result<(B::Header, DigestItemFor<B>), String> {
		// pow work proof MUST be last digest item
		let digest_item = match header.digest_mut().pop() {
			Some(x) => x,
			None => return Err(" get digest item failed.".to_string()),
		};
		let seal = digest_item.as_pow_seal().ok_or_else(|| {
			format!("Header {:?} not sealed", hash)
		})?;

		self.check_pow_target(&header, &seal)?;

		self.check_shard_info(&header)?;

		self.check_other_logs(&header)?;

		check_work_proof(&header, &seal)?;

		Ok((header, digest_item))
	}

	/// check pow_target in seal
	fn check_pow_target(&self, header: &B::Header, seal: &PowSeal<B, AuthorityId>) -> Result<(), String> {
		let pow_target = calc_pow_target(self.client.clone(), header, seal.timestamp, &self.context).map_err(|e| format!("{:?}", e))?;
		if seal.pow_target != pow_target {
			return Err("check_pow_target failed, pow target not match.".to_string());
		}
		Ok(())
	}

	/// check shard info
	fn check_shard_info(&self, header: &B::Header) -> Result<(), String> {
		// actual shard num in this node util status to Committed
		let (digest_shard_num, digest_shard_count): (u16, u16) = header.digest().logs().iter().rev()
			.filter_map(ShardingDigestItem::as_sharding_info).next()
			.expect("shard info must exist");
		let parent = self.client.header(generic::BlockId::hash(*header.parent_hash()))
			.expect("parent header must exist.")
			.expect("parent header must exist.");
		let number = *header.number();
		let observe_blocks = self.context.genesis_scale_out_observe_blocks;
		let ok = match parent.digest().logs().iter().rev().filter_map(ScaleOutPhaseDigestItem::as_scale_out_phase).next() {
			Some(ScaleOutPhase::Started { observe_util: p_observe_util, shard_num: _p_shard_num }) => {
				match header.digest().logs().iter().rev().filter_map(ScaleOutPhaseDigestItem::as_scale_out_phase).next() {
					Some(ScaleOutPhase::Started { observe_util, shard_num }) => {
						let ok = p_observe_util == observe_util
							&& (shard_num == digest_shard_num || shard_num == digest_shard_num + digest_shard_count)
							&& number < observe_util;
						if !ok {
							error!("parent status: {}, current status: {}", Colour::Red.paint("Started"), Colour::Red.paint("Started"));
						}
						ok
					}
					Some(ScaleOutPhase::NativeReady { observe_util, shard_num }) => {
						let ok = p_observe_util == number
							&& (shard_num == digest_shard_num || shard_num == digest_shard_num + digest_shard_count)
							&& number + observe_blocks == observe_util;
						if !ok {
							error!("parent status: {}, current status: {}", Colour::Red.paint("Started"), Colour::Red.paint("NativeReady"));
						}
						ok
					}
					None => true,
					_ => {
						error!("parent status: {}", Colour::Red.paint("Started"));
						false
					}
				}
			}
			Some(ScaleOutPhase::NativeReady { observe_util: p_observe_util, shard_num: _p_shard_num }) => {
				match header.digest().logs().iter().rev().filter_map(ScaleOutPhaseDigestItem::as_scale_out_phase).next() {
					Some(ScaleOutPhase::NativeReady { observe_util, shard_num }) => {
						let ok = p_observe_util == observe_util
							&& (shard_num == digest_shard_num || shard_num == digest_shard_num + digest_shard_count)
							&& number < observe_util;
						if !ok {
							error!("parent status: {}, current status: {}", Colour::Red.paint("NativeReady"), Colour::Red.paint("NativeReady"));
						}
						ok
					}
					Some(ScaleOutPhase::Ready { observe_util, shard_num }) => {
						let ok = p_observe_util == number
							&& (shard_num == digest_shard_num || shard_num == digest_shard_num + digest_shard_count)
							&& number + observe_blocks == observe_util;
						if !ok {
							error!("parent status: {}, current status: {}", Colour::Red.paint("NativeReady"), Colour::Red.paint("Ready"));
						}
						ok
					}
					None => true,
					_ => {
						error!("parent status: {}", Colour::Red.paint("NativeReady"));
						false
					}
				}
			}
			Some(ScaleOutPhase::Ready { observe_util: p_observe_util, shard_num: _p_shard_num }) => {
				match header.digest().logs().iter().rev().filter_map(ScaleOutPhaseDigestItem::as_scale_out_phase).next() {
					Some(ScaleOutPhase::Ready { observe_util, shard_num }) => {
						let ok = p_observe_util == observe_util
							&& (shard_num == digest_shard_num || shard_num == digest_shard_num + digest_shard_count)
							&& number < observe_util;
						if !ok {
							error!("parent status: {}, current status: {}", Colour::Red.paint("Ready"), Colour::Red.paint("Ready"));
						}
						ok
					}
					Some(ScaleOutPhase::Committing { shard_count }) => {
						let ok = p_observe_util == number
							&& shard_count == digest_shard_count * 2;
						if !ok {
							error!("parent status: {}, current status: {}", Colour::Red.paint("Ready"), Colour::Red.paint("Committing"));
						}
						ok
					}
					_ => {
						error!("parent status: {}", Colour::Red.paint("Ready"));
						false
					}
				}
			}
			Some(ScaleOutPhase::Committing { shard_count: p_shard_count }) => {
				match header.digest().logs().iter().rev().filter_map(ScaleOutPhaseDigestItem::as_scale_out_phase).next() {
					Some(ScaleOutPhase::Committed { shard_num, shard_count }) => {
						let ok = shard_num == digest_shard_num
							&& shard_count == p_shard_count
							&& digest_shard_count == shard_count;
						if !ok {
							error!("parent status: {}, current status: {}, shard_num:{}, digest_shard_count:{}, digest_shard_num:{}, shard_count:{}, p_shard_count:{}"
								   , Colour::Red.paint("Committing"), Colour::Red.paint("Committed"), shard_num, digest_shard_count, digest_shard_num, shard_count, p_shard_count
							);
						}
						ok
					}
					_ => {
						error!("parent status: {}", Colour::Red.paint("Committing"));
						false
					}
				}
			}
			Some(ScaleOutPhase::Committed { shard_num: _p_shard_num, shard_count: _p_shard_count }) => {
				match header.digest().logs().iter().rev().filter_map(ScaleOutPhaseDigestItem::as_scale_out_phase).next() {
					Some(ScaleOutPhase::Started { observe_util, shard_num }) => {
						let ok = observe_util == number + observe_blocks
							&& (shard_num == digest_shard_num || shard_num == digest_shard_num + digest_shard_count);
						if !ok {
							error!("parent status: {}, current status: {}", Colour::Red.paint("Committed"), Colour::Red.paint("Started"));
						}
						ok
					}
					None => true,
					_ => {
						error!("parent status: {}", Colour::Red.paint("Committed"));
						false
					}
				}
			}
			None => {
				match header.digest().logs().iter().rev().filter_map(ScaleOutPhaseDigestItem::as_scale_out_phase).next() {
					Some(ScaleOutPhase::Started { observe_util, shard_num }) => {
						let ok = number + observe_blocks == observe_util
							&& (shard_num == digest_shard_num || shard_num == digest_shard_num + digest_shard_count);
						if !ok {
							error!("parent status: {}, current status: {}", Colour::Red.paint("None"), Colour::Red.paint("Started"));
						}
						ok
					}
					Some(_) => {
						error!("parent status: {}", Colour::Red.paint("None"));
						false
					}
					_ => true,
				}
			}
		};
		if !ok {
			return Err("ScaleOutPhase checked failed.".to_string());
		}

		// check scale
		check_scale::<B, AccountId>(header, self.shard_extra.clone())?;

		//check header shard info (normal or scaling)
		let (header_shard_num, header_shard_count): (u16, u16) = header.digest().logs().iter().rev()
			.filter_map(ShardingDigestItem::as_sharding_info)
			.next().expect("non-genesis block always has shard info");

		let original_shard_num = get_original_shard_num(self.shard_extra.shard_num, self.shard_extra.shard_count, header_shard_count)?;
		if header_shard_num != original_shard_num {
			return Err(format!("Invalid header shard info"));
		}
		Ok(())
	}

	/// check other digest
	fn check_other_logs(&self, header: &B::Header) -> Result<(), String> {
		Ok(())
	}
}

/// check scale
pub fn check_scale<B, AccountId>(
	header: &B::Header,
	shard_extra: ShardExtra<AccountId>,
) -> Result<(), String> where
	B: BlockT,
	DigestItemFor<B>: ShardingDigestItem<u16> + ScaleOutPhaseDigestItem<NumberFor<B>, u16>,
	AccountId: Encode + Decode + Clone,
{
	let coinbase = shard_extra.coinbase.clone();
	let shard_num = shard_extra.shard_num;
	let shard_count = shard_extra.shard_count;
	let scale_out = shard_extra.scale_out;
	let trigger_exit = shard_extra.trigger_exit;

	//check arg shard info and coinbase when scale out phase committed
	if let Some(ScaleOutPhase::Committed { shard_num: _scale_shard_num, shard_count: scale_shard_count }) = header.digest().logs().iter().rev()
		.filter_map(ScaleOutPhaseDigestItem::as_scale_out_phase)
		.next() {
		let target_shard_num = match scale_out {
			Some(scale_out) => scale_out.shard_num,
			None => shard_num,
		};

		if shard_count != scale_shard_count {
			let coinbase_shard_num = shard_num_for(&coinbase, scale_shard_count).expect("qed");
			if target_shard_num != coinbase_shard_num {
				warn!("Stop service for invalid arg coinbase");
				trigger_exit.trigger_stop();
				return Err(format!("Invalid arg coinbase"));
			}

			warn!("Restart service for invalid arg shard info");
			trigger_exit.trigger_restart();

			return Err(format!("Invalid arg shard info"));
		}
	}

	Ok(())
}

fn get_original_shard_num(shard_num: u16, shard_count: u16, original_shard_count: u16) -> Result<u16, String> {
	let mut shard_num = shard_num;
	let mut shard_count = shard_count;
	while shard_count > original_shard_count {
		shard_count = shard_count / 2;
		shard_num = if shard_num >= shard_count { shard_num - shard_count } else { shard_num };
	}

	if shard_count != original_shard_count {
		return Err(format!("Invalid header shard info"));
	}

	Ok(shard_num)
}

#[cfg(test)]
mod tests {
	use crate::verifier::get_original_shard_num;

	#[test]
	fn test_get_original_shard_num() {
		assert_eq!(Ok(0), get_original_shard_num(0u16, 8u16, 8u16));
		assert_eq!(Ok(0), get_original_shard_num(0u16, 8u16, 4u16));
		assert_eq!(Ok(1), get_original_shard_num(1u16, 8u16, 4u16));
		assert_eq!(Ok(2), get_original_shard_num(2u16, 8u16, 4u16));
		assert_eq!(Ok(0), get_original_shard_num(4u16, 8u16, 4u16));
		assert_eq!(Ok(1), get_original_shard_num(13u16, 16u16, 4u16));
		assert_eq!(Err(format!("Invalid header shard info")), get_original_shard_num(5u16, 8u16, 16u16));
	}
}
