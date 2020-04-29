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

use {
    std::{
        fmt::Debug,
        marker::PhantomData,
        sync::{Arc, RwLock},
        time::{Duration, Instant},
    },
    futures::{
        future::{self, Either, Loop},
        Future, IntoFuture,
    },
    log::{warn, info},
    tokio::timer::Delay,
};
use codec::{Decode, Encode, Codec};
use sp_consensus::{
    SyncOracle, BlockImportParams,
    BlockImport, BlockOrigin, ForkChoiceStrategy,
};
use sp_inherents::InherentDataProviders;
use sp_runtime::{
    generic::Digest,
    traits::{Block, Header, DigestFor, DigestItemFor, NumberFor}
};
use super::{
    CompatibleDigestItem, WorkProof, ProofNonce,
};
use crate::job::{JobManager, DefaultJob};
use crate::pow::check_work_proof;
use yee_sharding::{ShardingDigestItem, ScaleOutPhaseDigestItem};
use crate::ShardExtra;
use crate::verifier::check_scale;
use sp_core::H256;
use ansi_term::Colour;
use std::collections::HashMap;

pub trait PowWorker<JM: JobManager> {
    type Error: Debug + Send;
    type OnJob: IntoFuture<Item=JM::Job, Error=Self::Error>;
    type OnWork: IntoFuture<Item=(), Error=Self::Error>;

    fn stop_sign(&self) -> Arc<RwLock<bool>>;

    fn on_start(&self) -> Result<(), Self::Error>;

    fn on_job(&self) -> Self::OnJob;

    fn on_work(&self, iter: u64) -> Self::OnWork;
}

pub struct DefaultWorker<B, C, I, JM, AccountId, AuthorityId> {
    job_manager: Arc<JM>,
    block_import: Arc<I>,
    inherent_data_providers: InherentDataProviders,
    stop_sign: Arc<RwLock<bool>>,
    shard_extra: ShardExtra<AccountId>,
    phantom: PhantomData<(B, C, AuthorityId)>,
}

impl<B, C, I, JM, AccountId, AuthorityId> DefaultWorker<B, C, I, JM, AccountId, AuthorityId> where
    B: Block,
    JM: JobManager,
{
    pub fn new(
        job_manager: Arc<JM>,
        block_import: Arc<I>,
        inherent_data_providers: InherentDataProviders,
        shard_extra: ShardExtra<AccountId>,
    ) -> Self {
        DefaultWorker {
            job_manager,
            block_import,
            inherent_data_providers,
            stop_sign: Default::default(),
            shard_extra,
            phantom: PhantomData,
        }
    }
}

impl<B, C, I, JM, AccountId, AuthorityId> PowWorker<JM> for DefaultWorker<B, C, I, JM, AccountId, AuthorityId> where
    B: Block,
    //C: ProvideRuntimeApi<B> + BlockOf + ProvideCache<B> + Sync,
    //C::Api: AuraApi<B, AuthorityId<P>>,
    //DigestFor<B>: Digest,
    I: BlockImport<B, Error=sp_consensus::Error> + Send + Sync + 'static,
    DigestItemFor<B>: CompatibleDigestItem<B, AuthorityId> + ShardingDigestItem<u16> + ScaleOutPhaseDigestItem<NumberFor<B>, u16>,
    JM: JobManager<Job=DefaultJob<B, AuthorityId>>,
    AccountId: Codec + Send + Sync + Clone + 'static,
    AuthorityId: Decode + Encode + Send + Sync + Clone + 'static,
    AuthorityId: Decode + Encode + Clone + 'static,
    B::Hash: From<H256> + Ord,
{
    type Error = sp_consensus::Error;
    type OnJob = Box<dyn Future<Item=DefaultJob<B, AuthorityId>, Error=Self::Error> + Send>;
    type OnWork = Box<dyn Future<Item=(), Error=Self::Error> + Send>;

    fn stop_sign(&self) -> Arc<RwLock<bool>> {
        self.stop_sign.clone()
    }

    fn on_start(&self) -> Result<(), sp_consensus::Error> {
        super::register_inherent_data_provider(&self.inherent_data_providers, self.shard_extra.coinbase.clone())
    }

    fn on_job(&self) -> Self::OnJob {
        self.job_manager.get_job()
    }

    fn on_work(&self,
              iter: u64,
    ) -> Self::OnWork {
        let mut block_import = self.block_import.clone();

        let job = self.on_job().into_future();

        let shard_extra = self.shard_extra.clone();

        let on_proposal_block = move |job: DefaultJob<B, AuthorityId>| -> Result<(), sp_consensus::Error> {
            let header = job.header;
            let body = job.body;
            let header_num = header.number().clone();
            let header_pre_hash = header.hash();
            let digest_item = job.digest_item;
            let pow_target = digest_item.pow_target;
            let xts_proof = job.xts_proof.clone();

            info!("block template {} @ {:?}, pow target: {:#x}", header_num, header_pre_hash, pow_target);

            // TODO: remove hardcoded
            const PREFIX: &str = "yeeroot-";

            for i in 0_u64..iter {

                let shard_extra = shard_extra.clone();
                let proof = WorkProof::Nonce(ProofNonce::get_with_prefix_len(PREFIX, 12, i));
                let mut seal = digest_item.clone();
                seal.work_proof = proof;

                if let Ok((post_digest, hash)) = check_work_proof(&header, &seal){

                    check_scale::<B, AccountId>(&header, shard_extra)?;

                    let import_block: BlockImportParams<B, sp_api::TransactionFor<C, B>> = BlockImportParams {
                        origin: BlockOrigin::Own,
                        header,
                        justification: None,
                        //proof: Some(xts_proof),
                        post_digests: vec![post_digest],
                        body: Some(body),
                        storage_changes: None,
                        finalized: false,
                        intermediates: HashMap::new(),
                        auxiliary: Vec::new(),
                        allow_missing_state: false,
                        fork_choice: Some(ForkChoiceStrategy::LongestChain),
                        import_existing: false,
                        post_hash: None,
                    };
                    block_import.import_block(import_block, Default::default())?;

                    info!("{} @ {} {:?}", Colour::Green.bold().paint("Block mined"), header_num, hash);
                    return Ok(());
                }
            }

            Ok(())
        };

        Box::new(
            job
                .map_err(|e|{warn!("job error: {:?}", e); e})
                .map_err(to_common_error)
                .map(move |job| {
                    if let Err(e) = on_proposal_block(job) {
                        warn!("block proposal failed {:?}", e);
                    }
                })
        )
    }
}

pub fn to_common_error<E: Debug>(e: E) -> sp_consensus::Error {
    sp_consensus::Error::ClientImport(format!("{:?}", e)).into()
}

pub fn start_worker<W, SO, OnExit, JM>(
    worker: Arc<W>,
    sync_oracle: &'static mut SO,
    on_exit: OnExit,
    mine: bool,
) -> Result<impl Future<Item=(), Error=()>, sp_consensus::Error> where
    W: PowWorker<JM>,
    SO: SyncOracle,
    OnExit: Future<Item=(), Error=()>,
    JM : JobManager,
{
    worker.on_start().map_err(to_common_error)?;

    let stop_sign = worker.stop_sign();

    info!("worker loop start");
    let work = future::loop_fn((), move |()| {
        let delay = Delay::new(Instant::now() + Duration::from_secs(5));
        let delayed_continue = Either::A(delay.then(|_| future::ok(Loop::Continue(()))));
        let no_delay_stop = Either::B(future::ok(Loop::Break(())));

        if !mine {
            return Either::A(no_delay_stop);
        }

        match worker.stop_sign().read() {
            Ok(stop_sign) => {
                if *stop_sign {
                    return Either::A(no_delay_stop);
                }
            }
            Err(e) => {
                warn!("work stop sign read error {:?}", e);
                return Either::A(no_delay_stop);
            }
        }

        // worker main loop
        info!("worker one loop start");

        if sync_oracle.is_major_syncing() {
            return Either::A(delayed_continue);
        }

        let task = worker.on_work(10000).into_future();
        Either::B(
            task.then(|_| Delay::new(Instant::now()))
                .then(|_| Ok(Loop::Continue(())))
        )
    });

    Ok(work.select(on_exit).then(move |_| {
        stop_sign.write()
            .map(|mut sign| { *sign = true; })
            .unwrap_or_else(|e| { warn!("write stop sign error : {:?}", e); });

        Ok(())
    }))
}