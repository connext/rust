#![allow(clippy::enum_variant_names)]
#![allow(missing_docs)]

use async_trait::async_trait;
use color_eyre::Result;
use ethers::{
    core::types::{Signature, H256, U256},
    providers::Middleware,
};
use futures_util::future::join_all;
use nomad_core::{
    ChainCommunicationError, Common, CommonIndexer, ContractLocator, DoubleUpdate, Home,
    HomeIndexer, Message, RawCommittedMessage, SignedUpdate, SignedUpdateWithMeta, State,
    TxOutcome, Update, UpdateMeta,
};
use nomad_xyz_configuration::HomeGasLimits;
use std::{convert::TryFrom, error::Error as StdError, sync::Arc};
use tracing::instrument;

use crate::{bindings::home::Home as EthereumHomeInternal, report_tx};

impl<M> std::fmt::Display for EthereumHomeInternal<M>
where
    M: ethers::providers::Middleware,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[derive(Debug)]
/// Struct that retrieves event data for an Ethereum home
pub struct EthereumHomeIndexer<R>
where
    R: ethers::providers::Middleware + 'static,
{
    contract: Arc<EthereumHomeInternal<R>>,
    provider: Arc<R>,
    from_height: u32,
    chunk_size: u32,
}

impl<R> EthereumHomeIndexer<R>
where
    R: ethers::providers::Middleware + 'static,
{
    /// Create new EthereumHomeIndexer
    pub fn new(
        provider: Arc<R>,
        ContractLocator {
            name: _,
            domain: _,
            address,
        }: &ContractLocator,
        from_height: u32,
        chunk_size: u32,
    ) -> Self {
        Self {
            contract: Arc::new(EthereumHomeInternal::new(
                address.as_ethereum_address().expect("!eth address"),
                provider.clone(),
            )),
            provider,
            from_height,
            chunk_size,
        }
    }
}

#[async_trait]
impl<R> CommonIndexer for EthereumHomeIndexer<R>
where
    R: ethers::providers::Middleware + 'static,
{
    #[instrument(err, skip(self))]
    async fn get_block_number(&self) -> Result<u32> {
        Ok(self.provider.get_block_number().await?.as_u32())
    }

    #[instrument(err, skip(self))]
    async fn fetch_sorted_updates(&self, from: u32, to: u32) -> Result<Vec<SignedUpdateWithMeta>> {
        let mut events = self
            .contract
            .update_filter()
            .from_block(from)
            .to_block(to)
            .query_with_meta()
            .await?;

        events.sort_by(|a, b| {
            let mut ordering = a.1.block_number.cmp(&b.1.block_number);
            if ordering == std::cmp::Ordering::Equal {
                ordering = a.1.transaction_index.cmp(&b.1.transaction_index);
            }

            ordering
        });

        let update_futs: Vec<_> = events
            .iter()
            .map(|event| async {
                let signature = Signature::try_from(event.0.signature.as_ref())
                    .expect("chain accepted invalid signature");

                let update = Update {
                    home_domain: event.0.home_domain,
                    previous_root: event.0.old_root.into(),
                    new_root: event.0.new_root.into(),
                };

                let block_number = event.1.block_number.as_u64();
                let timestamp = self
                    .provider
                    .get_block(block_number)
                    .await
                    .ok()
                    .flatten()
                    .map(|b| b.timestamp.as_u64());

                SignedUpdateWithMeta {
                    signed_update: SignedUpdate { update, signature },
                    metadata: UpdateMeta {
                        block_number,
                        timestamp,
                    },
                }
            })
            .collect();

        Ok(join_all(update_futs).await)
    }
}

#[async_trait]
impl<R> HomeIndexer for EthereumHomeIndexer<R>
where
    R: ethers::providers::Middleware + 'static,
{
    #[instrument(err, skip(self))]
    async fn fetch_sorted_messages(&self, from: u32, to: u32) -> Result<Vec<RawCommittedMessage>> {
        let mut events = self
            .contract
            .dispatch_filter()
            .from_block(from)
            .to_block(to)
            .query()
            .await?;

        events.sort_by(|a, b| a.leaf_index.cmp(&b.leaf_index));

        Ok(events
            .into_iter()
            .map(|f| RawCommittedMessage {
                leaf_index: f.leaf_index.as_u32(),
                committed_root: f.committed_root.into(),
                message: f.message.to_vec(),
            })
            .collect())
    }
}

/// A reference to a Home contract on some Ethereum chain
#[derive(Debug)]
pub struct EthereumHome<W, R>
where
    W: ethers::providers::Middleware + 'static,
    R: ethers::providers::Middleware + 'static,
{
    write_contract: Arc<EthereumHomeInternal<W>>,
    read_contract: Arc<EthereumHomeInternal<R>>,
    domain: u32,
    name: String,
    gas: Option<HomeGasLimits>,
}

impl<W, R> EthereumHome<W, R>
where
    W: ethers::providers::Middleware + 'static,
    R: ethers::providers::Middleware + 'static,
{
    /// Create a reference to a Home at a specific Ethereum address on some
    /// chain
    pub fn new(
        write_provider: Arc<W>,
        read_provider: Arc<R>,
        ContractLocator {
            name,
            domain,
            address,
        }: &ContractLocator,
        gas: Option<HomeGasLimits>,
    ) -> Self {
        Self {
            write_contract: Arc::new(EthereumHomeInternal::new(
                address.as_ethereum_address().expect("!eth address"),
                write_provider,
            )),
            read_contract: Arc::new(EthereumHomeInternal::new(
                address.as_ethereum_address().expect("!eth address"),
                read_provider,
            )),
            domain: *domain,
            name: name.to_owned(),
            gas,
        }
    }
}

#[async_trait]
impl<W, R> Common for EthereumHome<W, R>
where
    W: ethers::providers::Middleware + 'static,
    R: ethers::providers::Middleware + 'static,
{
    fn name(&self) -> &str {
        &self.name
    }

    #[tracing::instrument(err, skip(self))]
    async fn status(&self, txid: H256) -> Result<Option<TxOutcome>, ChainCommunicationError> {
        self.read_contract
            .client()
            .get_transaction_receipt(txid)
            .await
            .map_err(|e| Box::new(e) as Box<dyn StdError + Send + Sync>)?
            .map(|receipt| receipt.try_into())
            .transpose()
    }

    #[tracing::instrument(err, skip(self))]
    async fn updater(&self) -> Result<H256, ChainCommunicationError> {
        Ok(self.read_contract.updater().call().await?.into())
    }

    #[tracing::instrument(err, skip(self))]
    async fn state(&self) -> Result<State, ChainCommunicationError> {
        let state = self.read_contract.state().call().await?;
        match state {
            0 => Ok(State::Uninitialized),
            1 => Ok(State::Active),
            2 => Ok(State::Failed),
            _ => unreachable!(),
        }
    }

    #[tracing::instrument(err, skip(self))]
    async fn committed_root(&self) -> Result<H256, ChainCommunicationError> {
        Ok(self.read_contract.committed_root().call().await?.into())
    }

    #[tracing::instrument(err, skip(self, update), fields(update = %update))]
    async fn update(&self, update: &SignedUpdate) -> Result<TxOutcome, ChainCommunicationError> {
        let mut tx = self.write_contract.update(
            update.update.previous_root.to_fixed_bytes(),
            update.update.new_root.to_fixed_bytes(),
            update.signature.to_vec().into(),
        );

        if let Some(limits) = &self.gas {
            let queue_length = self.queue_length().await?;
            tx.tx.set_gas(
                U256::from(limits.update.base)
                    + (U256::from(limits.update.per_message) * queue_length),
            );
        }

        report_tx!(tx, &self.provider).try_into()
    }

    #[tracing::instrument(err, skip(self, double), fields(double = %double))]
    async fn double_update(
        &self,
        double: &DoubleUpdate,
    ) -> Result<TxOutcome, ChainCommunicationError> {
        let mut tx = self.write_contract.double_update(
            double.0.update.previous_root.to_fixed_bytes(),
            [
                double.0.update.new_root.to_fixed_bytes(),
                double.1.update.new_root.to_fixed_bytes(),
            ],
            double.0.signature.to_vec().into(),
            double.1.signature.to_vec().into(),
        );

        if let Some(limits) = &self.gas {
            tx.tx.set_gas(U256::from(limits.double_update));
        }

        report_tx!(tx, &self.provider).try_into()
    }
}

#[async_trait]
impl<W, R> Home for EthereumHome<W, R>
where
    W: ethers::providers::Middleware + 'static,
    R: ethers::providers::Middleware + 'static,
{
    fn local_domain(&self) -> u32 {
        self.domain
    }

    #[tracing::instrument(err, skip(self))]
    async fn nonces(&self, destination: u32) -> Result<u32, ChainCommunicationError> {
        Ok(self.read_contract.nonces(destination).call().await?)
    }

    #[tracing::instrument(err, skip(self))]
    async fn dispatch(&self, message: &Message) -> Result<TxOutcome, ChainCommunicationError> {
        let tx = self.write_contract.dispatch(
            message.destination,
            message.recipient.to_fixed_bytes(),
            message.body.clone().into(),
        );

        report_tx!(tx, &self.provider).try_into()
    }

    async fn queue_length(&self) -> Result<U256, ChainCommunicationError> {
        Ok(self.read_contract.queue_length().call().await?)
    }

    async fn queue_contains(&self, root: H256) -> Result<bool, ChainCommunicationError> {
        Ok(self
            .read_contract
            .queue_contains(root.into())
            .call()
            .await?)
    }

    #[tracing::instrument(err, skip(self), fields(hex_signature = %format!("0x{}", hex::encode(update.signature.to_vec()))))]
    async fn improper_update(
        &self,
        update: &SignedUpdate,
    ) -> Result<TxOutcome, ChainCommunicationError> {
        let mut tx = self.write_contract.improper_update(
            update.update.previous_root.to_fixed_bytes(),
            update.update.new_root.to_fixed_bytes(),
            update.signature.to_vec().into(),
        );

        if let Some(limits) = &self.gas {
            let queue_length = self.queue_length().await?;
            tx.tx.set_gas(
                U256::from(limits.improper_update.base)
                    + U256::from(limits.improper_update.per_message) * queue_length,
            );
        }

        report_tx!(tx, &self.provider).try_into()
    }

    #[tracing::instrument(err, skip(self))]
    async fn produce_update(&self) -> Result<Option<Update>, ChainCommunicationError> {
        let (a, b) = self.read_contract.suggest_update().call().await?;

        let previous_root: H256 = a.into();
        let new_root: H256 = b.into();

        if new_root.is_zero() {
            return Ok(None);
        }

        Ok(Some(Update {
            home_domain: self.local_domain(),
            previous_root,
            new_root,
        }))
    }
}
