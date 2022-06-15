use crate::models;
use crate::Sqlite;
use anyhow::Result;
use bdk::bitcoin::hashes::hex::FromHex;
use bdk::bitcoin::secp256k1;
use bdk::bitcoin::Address;
use bdk::bitcoin::Amount;
use bdk::bitcoin::Script;
use bdk::descriptor::Descriptor;
use model::olivia::BitMexPriceEventId;
use model::Cet;
use model::Dlc;
use model::FundingFee;
use model::RevokedCommit;
use sqlx::Transaction;
use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::str::FromStr;

/// Load RolloverCompleted event data
///
/// Returns Ok(Some(..)) if one was found or Ok(None) if none was found.
/// In error case, it returns Err(..)
pub async fn load(
    transaction: &mut Transaction<'_, Sqlite>,
    cfd_row_id: i64,
    event_row_id: i64,
) -> Result<Option<(Dlc, FundingFee)>> {
    let revoked_commit = load_revoked_commit_transactions(transaction, cfd_row_id).await?;
    let cets = load_cets(transaction, cfd_row_id).await?;

    let row = sqlx::query!(
        r#"
            SELECT
                settlement_event_id as "settlement_event_id: model::olivia::BitMexPriceEventId",
                refund_timelock as "refund_timelock: i64",
                funding_fee as "funding_fee: i64",
                rate as "rate: model::FundingRate",
                identity as "identity: models::SecretKey",
                identity_counterparty as "identity_counterparty: model::PublicKey",
                maker_address,
                taker_address,
                maker_lock_amount as "maker_lock_amount: i64",
                taker_lock_amount as "taker_lock_amount: i64",
                publish_sk as "publish_sk: models::SecretKey",
                publish_pk_counterparty as "publish_pk_counterparty: model::PublicKey",
                revocation_secret as "revocation_secret: models::SecretKey",
                revocation_pk_counterparty as "revocation_pk_counterparty: model::PublicKey",
                lock_tx as "lock_tx: model::Transaction",
                lock_tx_descriptor,
                commit_tx as "commit_tx: model::Transaction",
                commit_adaptor_signature as "commit_adaptor_signature: model::AdaptorSignature",
                commit_descriptor,
                refund_tx as "refund_tx: model::Transaction",
                refund_signature
            FROM
                rollover_completed_event_data
            WHERE 
                cfd_id = $1 and 
                event_id = $2
            "#,
        cfd_row_id,
        event_row_id,
    )
    .fetch_optional(transaction)
    .await?;

    let row = match row {
        Some(row) => row,
        None => return Ok(None),
    };

    let dlc = Dlc {
        identity: row.identity.into(),
        identity_counterparty: row.identity_counterparty,
        revocation: row.revocation_secret.into(),
        revocation_pk_counterparty: row.revocation_pk_counterparty,
        publish: row.publish_sk.into(),
        publish_pk_counterparty: row.publish_pk_counterparty,
        maker_address: Address::from_str(row.maker_address.as_str())?,
        taker_address: Address::from_str(row.taker_address.as_str())?,
        lock: (
            row.lock_tx,
            Descriptor::from_str(row.lock_tx_descriptor.as_str())?,
        ),
        commit: (
            row.commit_tx,
            row.commit_adaptor_signature,
            Descriptor::from_str(row.commit_descriptor.as_str())?,
        ),
        refund: (
            row.refund_tx,
            secp256k1::Signature::from_str(row.refund_signature.as_str())?,
        ),
        cets,
        maker_lock_amount: Amount::from_sat(row.maker_lock_amount as u64),
        taker_lock_amount: Amount::from_sat(row.taker_lock_amount as u64),
        revoked_commit,
        settlement_event_id: row.settlement_event_id,
        refund_timelock: row.refund_timelock as u32,
    };
    let funding_fee = FundingFee {
        fee: Amount::from_sat(row.funding_fee as u64),
        rate: row.rate,
    };

    Ok(Some((dlc, funding_fee)))
}

async fn load_revoked_commit_transactions(
    db_transaction: &mut Transaction<'_, Sqlite>,
    cfd_row_id: i64,
) -> Result<Vec<RevokedCommit>> {
    let revoked_commit = sqlx::query!(
        r#"
            SELECT
                encsig_ours as "encsig_ours: model::AdaptorSignature",
                publication_pk_theirs as "publication_pk_theirs: model::PublicKey",
                revocation_sk_theirs as "revocation_sk_theirs: models::SecretKey",
                script_pubkey,
                txid as "txid: model::Txid"
            FROM
                revoked_commit_transactions
            WHERE
                cfd_id = $1
            "#,
        cfd_row_id,
    )
    .fetch_all(db_transaction)
    .await?
    .into_iter()
    .map(|row| {
        Ok(RevokedCommit {
            encsig_ours: row.encsig_ours,
            revocation_sk_theirs: row.revocation_sk_theirs.into(),
            publication_pk_theirs: row.publication_pk_theirs,
            script_pubkey: Script::from_hex(row.script_pubkey.as_str())?,
            txid: row.txid,
        })
    })
    .collect::<Result<Vec<_>>>()?;
    Ok(revoked_commit)
}

async fn load_cets(
    db_transaction: &mut Transaction<'_, Sqlite>,
    cfd_row_id: i64,
) -> Result<HashMap<BitMexPriceEventId, Vec<Cet>>> {
    let revoked_commit_per_event: Vec<(BitMexPriceEventId, Cet)> = sqlx::query!(
        r#"
            SELECT
                oracle_event_id as "oracle_event_id: model::olivia::BitMexPriceEventId",
                adaptor_sig as "adaptor_sig: model::AdaptorSignature",
                maker_amount as "maker_amount: i64",
                taker_amount as "taker_amount: i64",
                n_bits as "n_bits: i64",
                range_end as "range_end: i64",
                range_start as "range_start: i64",
                txid as "txid: model::Txid"
            FROM
                open_cets
            WHERE
                cfd_id = $1
            "#,
        cfd_row_id,
    )
    .fetch_all(db_transaction)
    .await?
    .into_iter()
    .map(|row| {
        (
            row.oracle_event_id,
            Cet {
                maker_amount: Amount::from_sat(row.maker_amount as u64),
                taker_amount: Amount::from_sat(row.taker_amount as u64),
                adaptor_sig: row.adaptor_sig,
                range: RangeInclusive::new(row.range_start as u64, row.range_end as u64),
                n_bits: row.n_bits as usize,
                txid: row.txid,
            },
        )
    })
    .collect::<Vec<(_, _)>>();

    let mut revoked_commit: HashMap<BitMexPriceEventId, Vec<Cet>> = HashMap::new();
    for (event, cet) in revoked_commit_per_event {
        match revoked_commit.get_mut(&event) {
            Some(a) => {
                a.push(cet);
            }
            None => {
                revoked_commit.insert(event, vec![cet]);
            }
        }
    }

    Ok(revoked_commit)
}
