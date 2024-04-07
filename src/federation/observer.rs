use std::time::{Duration, SystemTime};

use anyhow::ensure;
use fedimint_core::api::{DynGlobalApi, InviteCode};
use fedimint_core::config::{ClientConfig, FederationId};
use fedimint_core::encoding::Encodable;
use fedimint_core::epoch::ConsensusItem;
use fedimint_core::session_outcome::SessionOutcome;
use fedimint_core::task::TaskGroup;
use fedimint_core::util::retry;
use fedimint_core::Amount;
use fedimint_ln_common::contracts::Contract;
use fedimint_ln_common::{LightningInput, LightningOutput, LightningOutputV0};
use fedimint_mint_common::{MintInput, MintOutput};
use fedimint_wallet_common::{WalletInput, WalletOutput};
use futures::StreamExt;
use hex::ToHex;
use serde::Serialize;
use sqlx::any::install_default_drivers;
use sqlx::pool::PoolConnection;
use sqlx::postgres::any::AnyTypeInfoKind;
use sqlx::{query, query_as, Any, AnyPool, Column, Connection, Database, Row, Transaction};
use tokio::time::sleep;
use tracing::log::info;
use tracing::{debug, error, warn};

use crate::federation::db::Federation;
use crate::federation::{db, decoders_from_config, instance_to_kind};

#[derive(Debug, Clone)]
pub struct FederationObserver {
    connection_pool: AnyPool,
    admin_auth: String,
    task_group: TaskGroup,
}

impl FederationObserver {
    pub async fn new(database: &str, admin_auth: &str) -> anyhow::Result<FederationObserver> {
        install_default_drivers();
        let connection_pool = sqlx::AnyPool::connect(database).await?;

        let slf = FederationObserver {
            connection_pool,
            admin_auth: admin_auth.to_owned(),
            task_group: Default::default(),
        };

        slf.setup_schema().await?;

        for federation in slf.list_federations().await? {
            slf.spawn_observer(federation).await;
        }

        slf.task_group
            .spawn_cancellable("fetch block times", Self::fetch_block_times(slf.clone()));

        Ok(slf)
    }

    async fn spawn_observer(&self, federation: Federation) {
        let slf = self.clone();
        self.task_group.spawn_cancellable(
            format!("Observer for {}", federation.federation_id),
            async move {
                if let Err(e) = slf
                    .observe_federation(federation.federation_id, federation.config)
                    .await
                {
                    error!("Observer errored: {e:?}");
                }
            },
        );
    }

    async fn setup_schema(&self) -> anyhow::Result<()> {
        query(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/schema/v0.sql"
        )))
        .execute(self.connection().await?.as_mut())
        .await?;
        Ok(())
    }

    async fn connection(&self) -> anyhow::Result<PoolConnection<Any>> {
        Ok(self.connection_pool.acquire().await?)
    }

    pub async fn list_federations(&self) -> anyhow::Result<Vec<db::Federation>> {
        Ok(query_as::<_, db::Federation>("SELECT * FROM federations")
            .fetch_all(self.connection().await?.as_mut())
            .await?)
    }

    pub async fn get_federation(
        &self,
        federation_id: FederationId,
    ) -> anyhow::Result<Option<Federation>> {
        Ok(
            query_as::<_, db::Federation>("SELECT * FROM federations WHERE federation_id = $1")
                .bind(federation_id.consensus_encode_to_vec())
                .fetch_optional(self.connection().await?.as_mut())
                .await?,
        )
    }

    pub async fn add_federation(&self, invite: &InviteCode) -> anyhow::Result<FederationId> {
        let federation_id = invite.federation_id();

        if self.get_federation(federation_id).await?.is_some() {
            return Ok(federation_id);
        }

        let config = ClientConfig::download_from_invite_code(invite).await?;

        query("INSERT INTO federations VALUES ($1, $2)")
            .bind(federation_id.consensus_encode_to_vec())
            .bind(config.consensus_encode_to_vec())
            .execute(self.connection().await?.as_mut())
            .await?;

        self.spawn_observer(Federation {
            federation_id,
            config,
        })
        .await;

        Ok(federation_id)
    }

    // FIXME: use middleware for auth and get it out of here
    pub fn check_auth(&self, bearer_token: &str) -> anyhow::Result<()> {
        ensure!(self.admin_auth == bearer_token, "Invalid bearer token");
        Ok(())
    }

    async fn fetch_block_times(self) {
        const SLEEP_SECS: u64 = 10;
        loop {
            if let Err(e) = self.fetch_block_times_inner().await {
                warn!("Error while fetching block times: {e:?}");
            }
            info!("Block sync finished, waiting {SLEEP_SECS} seconds");
            sleep(Duration::from_secs(SLEEP_SECS)).await;
        }
    }

    async fn fetch_block_times_inner(&self) -> anyhow::Result<()> {
        let builder = esplora_client::Builder::new("https://blockstream.info/api");
        let esplora_client = builder.build_async()?;

        // TODO: find a better way to pre-seed the DB so we don't have to bother
        // blockstream.info Block 820k was mined Dec 2023, afaik there are no
        // compatible federations older than that
        let next_block_height =
            (self.last_fetched_block_height().await?.unwrap_or(820_000) + 1) as u32;
        let current_block_height = esplora_client.get_height().await?;

        info!("Fetching block times for block {next_block_height} to {current_block_height}");

        let mut block_stream = futures::stream::iter(next_block_height..=current_block_height)
            .map(move |block_height| {
                let esplora_client_inner = esplora_client.clone();
                async move {
                    let block_hash = esplora_client_inner.get_block_hash(block_height).await?;
                    let block = esplora_client_inner.get_header_by_hash(&block_hash).await?;

                    Result::<_, anyhow::Error>::Ok((block_height, block))
                }
            })
            .buffered(4);

        let mut timer = SystemTime::now();
        let mut last_log_height = next_block_height;
        while let Some((block_height, block)) = block_stream.next().await.transpose()? {
            query("INSERT INTO block_times VALUES ($1, $2)")
                .bind(block_height as i64)
                .bind(block.time as i64)
                .execute(self.connection().await?.as_mut())
                .await?;

            // TODO: write abstraction
            let elapsed = timer.elapsed().unwrap_or_default();
            if elapsed >= Duration::from_secs(5) {
                let blocks_synced = block_height - last_log_height;
                let rate = (blocks_synced as f64) / elapsed.as_secs_f64();
                info!("Synced up to block {block_height}, processed {blocks_synced} blocks at a rate of {rate:.2} blocks/s");
                timer = SystemTime::now();
                last_log_height = block_height;
            }
        }

        Ok(())
    }

    async fn last_fetched_block_height(&self) -> anyhow::Result<Option<u64>> {
        let row = query("SELECT MAX(block_height) AS max_height FROM block_times")
            .fetch_one(self.connection().await?.as_mut())
            .await?;

        Ok(row
            .try_get::<i64, _>("max_height")
            .ok()
            .map(|max_height| max_height as u64))
    }

    async fn observe_federation(
        self,
        federation_id: FederationId,
        config: ClientConfig,
    ) -> anyhow::Result<()> {
        let api = DynGlobalApi::from_config(&config);
        let decoders = decoders_from_config(&config);

        info!("Starting background job for {federation_id}");
        let next_session = self.federation_session_count(federation_id).await?;
        debug!("Next session {next_session}");
        let api_fetch = api.clone();
        let mut session_stream = futures::stream::iter(next_session..)
            .map(move |session_index| {
                debug!("Starting fetch job for session {session_index}");
                let api_fetch_single = api_fetch.clone();
                let decoders_single = decoders.clone();
                async move {
                    let signed_session_outcome = retry(
                        format!("Waiting for session {session_index}"),
                        || async {
                            api_fetch_single
                                .await_block(session_index, &decoders_single)
                                .await
                        },
                        Duration::from_secs(1),
                        u32::MAX,
                    )
                    .await
                    .expect("Will fail after 136 years");
                    debug!("Finished fetch job for session {session_index}");
                    (session_index, signed_session_outcome)
                }
            })
            .buffered(32);

        let mut timer = SystemTime::now();
        let mut last_session = next_session;
        while let Some((session_index, signed_session_outcome)) = session_stream.next().await {
            self.process_session(
                federation_id,
                config.clone(),
                session_index,
                signed_session_outcome,
            )
            .await?;

            let elapsed = timer.elapsed().unwrap_or_default();
            if elapsed >= Duration::from_secs(5) {
                let sessions_synced = session_index - last_session;
                let rate = (sessions_synced as f64) / elapsed.as_secs_f64();
                info!("Synced up to session {session_index}, processed {sessions_synced} sessions at a rate of {rate:.2} sessions/s");
                timer = SystemTime::now();
                last_session = session_index;
            }
        }

        unreachable!("Session stream should never end")
    }

    async fn process_session(
        &self,
        federation_id: FederationId,
        config: ClientConfig,
        session_index: u64,
        signed_session_outcome: SessionOutcome,
    ) -> anyhow::Result<()> {
        self.connection()
            .await?
            .transaction(|dbtx: &mut Transaction<Any>| {
                Box::pin(async move {
                    query("INSERT INTO sessions VALUES ($1, $2, $3)")
                        .bind(federation_id.consensus_encode_to_vec())
                        .bind(session_index as i64)
                        .bind(signed_session_outcome.consensus_encode_to_vec())
                        .execute(dbtx.as_mut())
                        .await?;

                    for (item_idx, item) in signed_session_outcome.items.into_iter().enumerate() {
                        match item.item {
                            ConsensusItem::Transaction(transaction) => {
                                Self::process_transaction(
                                    dbtx,
                                    federation_id,
                                    &config,
                                    session_index,
                                    item_idx as u64,
                                    transaction,
                                )
                                .await?;
                            }
                            _ => {
                                // FIXME: process module CIs
                            }
                        }
                    }

                    Result::<(), sqlx::Error>::Ok(())
                })
            })
            .await?;

        debug!("Processed session {session_index} of federation {federation_id}");
        Ok(())
    }

    async fn process_transaction(
        dbtx: &mut Transaction<'_, Any>,
        federation_id: FederationId,
        config: &ClientConfig,
        session_index: u64,
        item_index: u64,
        transaction: fedimint_core::transaction::Transaction,
    ) -> sqlx::Result<()> {
        let txid = transaction.tx_hash();

        query("INSERT INTO transactions VALUES ($1, $2, $3, $4, $5)")
            .bind(txid.consensus_encode_to_vec())
            .bind(federation_id.consensus_encode_to_vec())
            .bind(session_index as i64)
            .bind(item_index as i64)
            .bind(transaction.consensus_encode_to_vec())
            .execute(dbtx.as_mut())
            .await?;

        for (in_idx, input) in transaction.inputs.into_iter().enumerate() {
            let kind = instance_to_kind(config, input.module_instance_id());
            let maybe_amount_msat = match kind.as_str() {
                "ln" => Some(
                    input
                        .as_any()
                        .downcast_ref::<LightningInput>()
                        .expect("Not LN input")
                        .maybe_v0_ref()
                        .expect("Not v0")
                        .amount
                        .msats,
                ),
                "mint" => Some(
                    input
                        .as_any()
                        .downcast_ref::<MintInput>()
                        .expect("Not Mint input")
                        .maybe_v0_ref()
                        .expect("Not v0")
                        .amount
                        .msats,
                ),
                "wallet" => Some(
                    input
                        .as_any()
                        .downcast_ref::<WalletInput>()
                        .expect("Not Wallet input")
                        .maybe_v0_ref()
                        .expect("Not v0")
                        .0
                        .tx_output()
                        .value
                        * 1000,
                ),
                _ => None,
            };

            // TODO: use for LN input, but needs ability to query previously created
            // contracts
            let subtype = Option::<String>::None;

            query("INSERT INTO transaction_inputs VALUES ($1, $2, $3, $4, $5, $6)")
                .bind(federation_id.consensus_encode_to_vec())
                .bind(txid.consensus_encode_to_vec())
                .bind(in_idx as i64)
                .bind(kind.as_str())
                .bind(subtype)
                .bind(maybe_amount_msat.map(|amt| amt as i64))
                .execute(dbtx.as_mut())
                .await?;
        }

        for (out_idx, output) in transaction.outputs.into_iter().enumerate() {
            let kind = instance_to_kind(config, output.module_instance_id());
            let (maybe_amount_msat, maybe_subtype) = match kind.as_str() {
                "ln" => {
                    let ln_output = output
                        .as_any()
                        .downcast_ref::<LightningOutput>()
                        .expect("Not LN input")
                        .maybe_v0_ref()
                        .expect("Not v0");
                    let (amount_msat, maybe_subtype) = match ln_output {
                        LightningOutputV0::Contract(contract) => {
                            let subtype = match contract.contract {
                                Contract::Incoming(_) => "incoming",
                                Contract::Outgoing(_) => "outgoing",
                            };
                            (contract.amount.msats, Some(subtype))
                        }
                        // TODO: handle separately
                        LightningOutputV0::Offer(_) => (0, None),
                        LightningOutputV0::CancelOutgoing { .. } => (0, None),
                    };

                    (Some(amount_msat), maybe_subtype)
                }
                "mint" => {
                    let amount_msat = output
                        .as_any()
                        .downcast_ref::<MintOutput>()
                        .expect("Not Mint input")
                        .maybe_v0_ref()
                        .expect("Not v0")
                        .amount
                        .msats;
                    (Some(amount_msat), None)
                }
                "wallet" => {
                    let amount_msat = output
                        .as_any()
                        .downcast_ref::<WalletOutput>()
                        .expect("Not Wallet input")
                        .maybe_v0_ref()
                        .expect("Not v0")
                        .amount()
                        .to_sat()
                        * 1000;
                    (Some(amount_msat), None)
                }
                _ => (None, None),
            };

            query("INSERT INTO transaction_outputs VALUES ($1, $2, $3, $4, $5, $6)")
                .bind(federation_id.consensus_encode_to_vec())
                .bind(txid.consensus_encode_to_vec())
                .bind(out_idx as i64)
                .bind(kind.as_str())
                .bind(maybe_subtype)
                .bind(maybe_amount_msat.map(|amt| amt as i64))
                .execute(dbtx.as_mut())
                .await?;
        }

        Ok(())
    }

    pub(crate) async fn federation_session_count(
        &self,
        federation_id: FederationId,
    ) -> anyhow::Result<u64> {
        let last_session =
            query_as::<_, (i64,)>("SELECT COALESCE(MAX(session_index), -1) as max_session_index FROM sessions WHERE federation_id = $1")
                .bind(federation_id.consensus_encode_to_vec())
                .fetch_one(self.connection().await?.as_mut())
                .await?.0;
        Ok((last_session + 1) as u64)
    }

    #[allow(dead_code)]
    pub async fn list_federation_transactions(
        &self,
        federation_id: FederationId,
    ) -> anyhow::Result<Vec<db::Transaction>> {
        Ok(query_as::<_, db::Transaction>("SELECT txid, session_index, item_index, data FROM transactions WHERE federation_id = $1")
            .bind(federation_id.consensus_encode_to_vec())
            .fetch_all(self.connection().await?.as_mut())
            .await?)
    }

    pub async fn get_federation_assets(
        &self,
        federation_id: FederationId,
    ) -> anyhow::Result<Amount> {
        // Unfortunately SQLx has a bug where the integer parsing logic of the Any DB
        // type always uses signed 32bit integer decoding when receiving integer values
        // from SQLite. This is probably due to SQLite lacking the distinction between
        // integer types and just calling everything INTEGER and always using 64bit
        // representations while any other DBMS will call 64bit integers BIGINT or
        // something similar. That's why we serialize the number to a string and the
        // deserialize again in rust.
        let total_assets_msat = query_as::<_, (String,)>(
            "
        SELECT
            CAST((SELECT COALESCE(SUM(amount_msat), 0)
             FROM transaction_inputs
             WHERE kind = 'wallet' AND federation_id = $1) -
            (SELECT COALESCE(SUM(amount_msat), 0)
             FROM transaction_outputs
             WHERE kind = 'wallet' AND federation_id = $1) AS TEXT) AS net_amount_msat
        ",
        )
        .bind(federation_id.consensus_encode_to_vec())
        .fetch_one(self.connection().await?.as_mut())
        .await?
        .0;

        Ok(Amount::from_msats(
            total_assets_msat.parse().expect("DB returns valid number"),
        ))
    }

    /// Runs a SQL query against the database and outputs thew result as a JSON
    /// encodable `QueryResult`.
    pub async fn run_qery(&self, sql: &str) -> anyhow::Result<QueryResult> {
        let result: Vec<<Any as Database>::Row> = query(sql)
            .fetch_all(self.connection().await?.as_mut())
            .await?;

        let Some(first_row) = result.first() else {
            return Ok(QueryResult {
                cols: vec![],
                rows: vec![],
            });
        };

        let cols = first_row
            .columns()
            .iter()
            .map(|col| col.name().to_owned())
            .collect();

        info!("cols: {cols:?}");

        let rows = result
            .into_iter()
            .map(|row| {
                row.columns()
                    .iter()
                    .map(|col| {
                        let col_type = col.type_info();

                        match col_type.kind() {
                            AnyTypeInfoKind::Null => row
                                .try_get::<bool, _>(col.ordinal())
                                .ok()
                                .map(Into::<serde_json::Value>::into)
                                .or_else(|| {
                                    row.try_get::<String, _>(col.ordinal()).ok().map(Into::into)
                                })
                                .or_else(|| {
                                    row.try_get::<i64, _>(col.ordinal()).ok().map(Into::into)
                                })
                                .or_else(|| {
                                    row.try_get::<Vec<u8>, _>(col.ordinal())
                                        .ok()
                                        .map(|bytes| bytes.encode_hex::<String>().into())
                                })
                                .into(),
                            AnyTypeInfoKind::Bool => {
                                row.try_get::<bool, _>(col.ordinal()).ok().into()
                            }
                            AnyTypeInfoKind::SmallInt
                            | AnyTypeInfoKind::Integer
                            | AnyTypeInfoKind::BigInt => {
                                row.try_get::<i64, _>(col.ordinal()).ok().into()
                            }
                            AnyTypeInfoKind::Real | AnyTypeInfoKind::Double => {
                                row.try_get::<f64, _>(col.ordinal()).ok().into()
                            }
                            AnyTypeInfoKind::Text => {
                                row.try_get::<String, _>(col.ordinal()).ok().into()
                            }
                            AnyTypeInfoKind::Blob => row
                                .try_get::<Vec<u8>, _>(col.ordinal())
                                .ok()
                                .map(|bytes| bytes.encode_hex::<String>())
                                .into(),
                        }
                    })
                    .collect()
            })
            .collect();

        Ok(QueryResult { cols, rows })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct QueryResult {
    cols: Vec<String>,
    rows: Vec<Vec<serde_json::Value>>,
}