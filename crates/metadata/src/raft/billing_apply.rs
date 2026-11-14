//! Applying billing and usage changes. Every rule that money or the storage limit depends on
//! is checked in the same write transaction as the change it guards, and Raft applies one
//! command at a time, so two requests at once can't both spend the last of a balance or the
//! last of a project's space. Everything here must be deterministic: see `types.rs`.

use loony_core::{Bucket, BucketId, GiftCodeId, LedgerEntryId, OwnerId, ProjectId, UserId};
use redb::{Database, ReadableTable, TableDefinition, WriteTransaction};
use time::OffsetDateTime;

use crate::billing::{
    Checkout, CheckoutStatus, GiftCode, GiftCodeRedemption, LedgerEntry, LedgerReason, OwnerUsage,
    Step, Subscription, SubscriptionState, manual_payment_period, next_step,
};
use crate::commands::{MultipartUploadState, Project, ProjectMember, ProjectRole};
use crate::error::MetaError;
use crate::raft::applier::{
    BUCKETS, CLUSTER, MULTIPART, OBJECTS, PROJECT_MEMBERS, PROJECTS, db_err,
};
use crate::raft::projects::{read_row, read_rows, write_row};
use crate::raft::types::{
    ResolvedAdvance, ResolvedCancel, ResolvedCompleteCheckout, ResolvedNewCheckout,
    ResolvedNewGiftCode, ResolvedPay, ResolvedRedeem,
};
use loony_core::ObjectManifest;

/// Stored bytes, object count and in-progress multipart bytes per owner.
pub(crate) const OWNER_USAGE: TableDefinition<&str, &[u8]> = TableDefinition::new("owner_usage");
/// Which owner each bucket belongs to, so a write can find its project without a scan.
pub(crate) const BUCKET_OWNERS: TableDefinition<&str, &[u8]> =
    TableDefinition::new("bucket_owners");
/// Which project an owner is, if any. Owners outside projects have no limits.
pub(crate) const OWNER_PROJECTS: TableDefinition<&str, &[u8]> =
    TableDefinition::new("owner_projects");
pub(crate) const SUBSCRIPTIONS: TableDefinition<&str, &[u8]> =
    TableDefinition::new("subscriptions");
pub(crate) const CREDITS: TableDefinition<&str, &[u8]> = TableDefinition::new("credits");
/// Keyed by `user\0time\0entry`, so one user's entries come back in order.
pub(crate) const LEDGER: TableDefinition<&str, &[u8]> = TableDefinition::new("ledger");
pub(crate) const GIFT_CODES: TableDefinition<&str, &[u8]> = TableDefinition::new("gift_codes");
pub(crate) const GIFT_CODE_HASHES: TableDefinition<&str, &[u8]> =
    TableDefinition::new("gift_code_hashes");
/// Keyed by `code\0user`: one redemption per person per code.
pub(crate) const GIFT_REDEMPTIONS: TableDefinition<&str, &[u8]> =
    TableDefinition::new("gift_redemptions");
pub(crate) const CHECKOUTS: TableDefinition<&str, &[u8]> = TableDefinition::new("checkouts");

/// Set once the indexes above have been built from the data that existed before them.
const DERIVED_INDEXES_KEY: &str = "derived_indexes_v1";

// ---------------------------------------------------------------------------
// Lookups that work inside any transaction
// ---------------------------------------------------------------------------

fn owner_of_bucket_in(
    owners: &impl ReadableTable<&'static str, &'static [u8]>,
    buckets: &impl ReadableTable<&'static str, &'static [u8]>,
    bucket_id: BucketId,
) -> Result<Option<OwnerId>, MetaError> {
    if let Some(owner) = read_row::<OwnerId>(owners, &bucket_id.to_string())? {
        return Ok(Some(owner));
    }
    // Not indexed yet, as for a bucket made before the index existed.
    let all: Vec<Bucket> = read_rows(buckets)?;
    Ok(all
        .into_iter()
        .find(|b| b.bucket_id == bucket_id)
        .map(|b| b.owner_id))
}

fn project_of_owner_in(
    index: &impl ReadableTable<&'static str, &'static [u8]>,
    projects: &impl ReadableTable<&'static str, &'static [u8]>,
    owner: OwnerId,
) -> Result<Option<Project>, MetaError> {
    if let Some(project_id) = read_row::<ProjectId>(index, &owner.to_string())? {
        return read_row(projects, &project_id.to_string());
    }
    let all: Vec<Project> = read_rows(projects)?;
    Ok(all.into_iter().find(|p| p.owner_id == owner))
}

fn subscription_state(sub: Option<&Subscription>) -> SubscriptionState {
    sub.map_or(SubscriptionState::Unpaid, |sub| sub.state)
}

/// What `owner` may store: `Ok(None)` for no limit, `Ok(Some(bytes))` for a paid project,
/// and an error for a project that can't take on data at all.
fn write_limit_in(
    index: &impl ReadableTable<&'static str, &'static [u8]>,
    projects: &impl ReadableTable<&'static str, &'static [u8]>,
    subscriptions: &impl ReadableTable<&'static str, &'static [u8]>,
    owner: OwnerId,
) -> Result<Option<u64>, MetaError> {
    let Some(project) = project_of_owner_in(index, projects, owner)? else {
        return Ok(None);
    };
    if !project.enabled {
        return Err(MetaError::ProjectDisabled(project.project_id.to_string()));
    }
    let sub: Option<Subscription> = read_row(subscriptions, &project.project_id.to_string())?;
    let state = subscription_state(sub.as_ref());
    match sub {
        Some(sub) if state.writable() => Ok(Some(sub.plan.storage_bytes)),
        _ => Err(MetaError::ProjectNotWritable(state)),
    }
}

// ---------------------------------------------------------------------------
// Pieces the object and project commands use inside their own transactions
// ---------------------------------------------------------------------------

pub(crate) fn owner_of_bucket(
    txn: &WriteTransaction,
    bucket_id: BucketId,
) -> Result<Option<OwnerId>, MetaError> {
    let owners = txn.open_table(BUCKET_OWNERS).map_err(db_err)?;
    let buckets = txn.open_table(BUCKETS).map_err(db_err)?;
    owner_of_bucket_in(&owners, &buckets, bucket_id)
}

pub(crate) fn write_limit(
    txn: &WriteTransaction,
    owner: OwnerId,
) -> Result<Option<u64>, MetaError> {
    let index = txn.open_table(OWNER_PROJECTS).map_err(db_err)?;
    let projects = txn.open_table(PROJECTS).map_err(db_err)?;
    let subscriptions = txn.open_table(SUBSCRIPTIONS).map_err(db_err)?;
    write_limit_in(&index, &projects, &subscriptions, owner)
}

/// Refuses any change at all to an owner whose project can't take on data, deletes
/// included: read-only means read-only.
pub(crate) fn require_writable(txn: &WriteTransaction, owner: OwnerId) -> Result<(), MetaError> {
    write_limit(txn, owner).map(|_| ())
}

pub(crate) fn usage(txn: &WriteTransaction, owner: OwnerId) -> Result<OwnerUsage, MetaError> {
    let table = txn.open_table(OWNER_USAGE).map_err(db_err)?;
    Ok(read_row(&table, &owner.to_string())?.unwrap_or_default())
}

/// Applies a change in usage, refusing growth past the owner's limit. Shrinking is always
/// allowed, so someone over their limit can still delete their way back under it.
pub(crate) fn change_usage(
    txn: &WriteTransaction,
    owner: OwnerId,
    bytes: i64,
    objects: i64,
    pending: i64,
) -> Result<(), MetaError> {
    let limit = write_limit(txn, owner)?;
    let current = usage(txn, owner)?;
    if let Some(limit) = limit
        && current.exceeds(bytes + pending, limit)
    {
        return Err(MetaError::QuotaExceeded {
            limit,
            used: current.counted(),
            requested: (bytes + pending).max(0) as u64,
        });
    }
    let apply = |value: u64, delta: i64| -> u64 {
        if delta >= 0 {
            value.saturating_add(delta as u64)
        } else {
            value.saturating_sub(delta.unsigned_abs())
        }
    };
    let updated = OwnerUsage {
        bytes: apply(current.bytes, bytes),
        objects: apply(current.objects, objects),
        pending_bytes: apply(current.pending_bytes, pending),
    };
    let mut table = txn.open_table(OWNER_USAGE).map_err(db_err)?;
    write_row(&mut table, &owner.to_string(), &updated)
}

/// Releases usage without any check, for changes that only ever free space.
pub(crate) fn release_usage(
    txn: &WriteTransaction,
    owner: OwnerId,
    bytes: u64,
    objects: u64,
    pending: u64,
) -> Result<(), MetaError> {
    let current = usage(txn, owner)?;
    let updated = OwnerUsage {
        bytes: current.bytes.saturating_sub(bytes),
        objects: current.objects.saturating_sub(objects),
        pending_bytes: current.pending_bytes.saturating_sub(pending),
    };
    let mut table = txn.open_table(OWNER_USAGE).map_err(db_err)?;
    write_row(&mut table, &owner.to_string(), &updated)
}

pub(crate) fn index_bucket(
    txn: &WriteTransaction,
    bucket_id: BucketId,
    owner: Option<OwnerId>,
) -> Result<(), MetaError> {
    let mut table = txn.open_table(BUCKET_OWNERS).map_err(db_err)?;
    match owner {
        Some(owner) => write_row(&mut table, &bucket_id.to_string(), &owner),
        None => {
            table
                .remove(bucket_id.to_string().as_str())
                .map_err(db_err)?;
            Ok(())
        }
    }
}

pub(crate) fn index_project(
    txn: &WriteTransaction,
    owner: OwnerId,
    project: Option<ProjectId>,
) -> Result<(), MetaError> {
    let mut table = txn.open_table(OWNER_PROJECTS).map_err(db_err)?;
    match project {
        Some(project) => write_row(&mut table, &owner.to_string(), &project),
        None => {
            table.remove(owner.to_string().as_str()).map_err(db_err)?;
            Ok(())
        }
    }
}

pub(crate) fn forget_subscription(
    txn: &WriteTransaction,
    project_id: ProjectId,
) -> Result<(), MetaError> {
    let mut table = txn.open_table(SUBSCRIPTIONS).map_err(db_err)?;
    table
        .remove(project_id.to_string().as_str())
        .map_err(db_err)?;
    Ok(())
}

pub(crate) fn project_subscription_state(
    txn: &WriteTransaction,
    project_id: ProjectId,
) -> Result<SubscriptionState, MetaError> {
    let table = txn.open_table(SUBSCRIPTIONS).map_err(db_err)?;
    let sub: Option<Subscription> = read_row(&table, &project_id.to_string())?;
    Ok(subscription_state(sub.as_ref()))
}

// ---------------------------------------------------------------------------
// Credit
// ---------------------------------------------------------------------------

fn ledger_key(user: UserId, at: OffsetDateTime, entry_id: LedgerEntryId) -> String {
    format!(
        "{user}\0{:039}\0{entry_id}",
        at.unix_timestamp_nanos().max(0)
    )
}

/// Changes a balance and records why, in one step. A debit larger than the balance is
/// refused, so a balance never goes below zero.
fn change_balance(
    txn: &WriteTransaction,
    user: UserId,
    amount: i64,
    reason: LedgerReason,
    actor: String,
    at: OffsetDateTime,
    entry_id: LedgerEntryId,
) -> Result<LedgerEntry, MetaError> {
    let mut credits = txn.open_table(CREDITS).map_err(db_err)?;
    let balance: u64 = read_row(&credits, &user.to_string())?.unwrap_or(0);
    let after = if amount >= 0 {
        balance.checked_add(amount as u64)
    } else {
        balance.checked_sub(amount.unsigned_abs())
    }
    .ok_or(MetaError::InsufficientCredit {
        balance,
        needed: amount.unsigned_abs(),
    })?;
    write_row(&mut credits, &user.to_string(), &after)?;

    let entry = LedgerEntry {
        entry_id,
        user_id: user,
        amount_paise: amount,
        balance_after_paise: after,
        reason,
        actor,
        at,
    };
    let mut ledger = txn.open_table(LEDGER).map_err(db_err)?;
    write_row(&mut ledger, &ledger_key(user, at, entry_id), &entry)?;
    Ok(entry)
}

fn balance_in(txn: &WriteTransaction, user: UserId) -> Result<u64, MetaError> {
    let credits = txn.open_table(CREDITS).map_err(db_err)?;
    Ok(read_row(&credits, &user.to_string())?.unwrap_or(0))
}

pub(crate) fn get_balance(db: &Database, user: UserId) -> Result<u64, MetaError> {
    let txn = db.begin_read().map_err(db_err)?;
    let credits = txn.open_table(CREDITS).map_err(db_err)?;
    Ok(read_row(&credits, &user.to_string())?.unwrap_or(0))
}

/// The newest `limit` entries for `user`, newest first.
pub(crate) fn list_ledger(
    db: &Database,
    user: UserId,
    limit: usize,
) -> Result<Vec<LedgerEntry>, MetaError> {
    let txn = db.begin_read().map_err(db_err)?;
    let table = txn.open_table(LEDGER).map_err(db_err)?;
    let start = format!("{user}\0");
    let end = format!("{user}\u{1}");
    let mut entries = Vec::new();
    for row in table
        .range(start.as_str()..end.as_str())
        .map_err(db_err)?
        .rev()
    {
        let (_key, value) = row.map_err(db_err)?;
        entries.push(serde_json::from_slice(value.value())?);
        if entries.len() >= limit {
            break;
        }
    }
    Ok(entries)
}

// ---------------------------------------------------------------------------
// Subscriptions
// ---------------------------------------------------------------------------

fn require_owner(
    txn: &WriteTransaction,
    project_id: ProjectId,
    user: UserId,
) -> Result<Project, MetaError> {
    let projects = txn.open_table(PROJECTS).map_err(db_err)?;
    let project: Project = read_row(&projects, &project_id.to_string())?
        .ok_or_else(|| MetaError::NoSuchProject(project_id.to_string()))?;
    let members = txn.open_table(PROJECT_MEMBERS).map_err(db_err)?;
    let member: Option<ProjectMember> = read_row(&members, &format!("{project_id}\0{user}"))?;
    match member {
        Some(member) if member.role == ProjectRole::Owner => Ok(project),
        Some(_) => Err(MetaError::NotProjectOwner),
        None => Err(MetaError::NoSuchMember),
    }
}

pub(crate) fn get_subscription(
    db: &Database,
    project_id: ProjectId,
) -> Result<Option<Subscription>, MetaError> {
    let txn = db.begin_read().map_err(db_err)?;
    let table = txn.open_table(SUBSCRIPTIONS).map_err(db_err)?;
    read_row(&table, &project_id.to_string())
}

pub(crate) fn list_subscriptions(db: &Database) -> Result<Vec<Subscription>, MetaError> {
    let txn = db.begin_read().map_err(db_err)?;
    let table = txn.open_table(SUBSCRIPTIONS).map_err(db_err)?;
    read_rows(&table)
}

/// Pays for a project from the payer's credit: a first subscription, or bringing a lapsed one
/// back. The payer becomes whoever renewals are charged to.
pub(crate) fn pay_for_project(
    db: &Database,
    pay: ResolvedPay,
) -> Result<(Subscription, LedgerEntry), MetaError> {
    let ResolvedPay {
        project_id,
        payer,
        plan,
        at,
        entry_id,
    } = pay;
    let txn = db.begin_write().map_err(db_err)?;
    let result = {
        let project = require_owner(&txn, project_id, payer)?;
        if !project.enabled {
            return Err(MetaError::ProjectDisabled(project_id.to_string()));
        }
        let current: Option<Subscription> = {
            let table = txn.open_table(SUBSCRIPTIONS).map_err(db_err)?;
            read_row(&table, &project_id.to_string())?
        };
        let (start, end) = manual_payment_period(current.as_ref(), plan.period, at)
            .ok_or(MetaError::AlreadyPaid)?;
        let entry = change_balance(
            &txn,
            payer,
            -(plan.price_paise as i64),
            LedgerReason::Subscription {
                project_id,
                plan_id: plan.id.clone(),
                period_start: start,
                period_end: end,
                automatic: false,
            },
            format!("user:{payer}"),
            at,
            entry_id,
        )?;
        let sub = Subscription {
            project_id,
            plan,
            payer,
            state: SubscriptionState::Active,
            state_since: at,
            period_start: start,
            period_end: end,
            cancel_at_period_end: false,
        };
        let mut table = txn.open_table(SUBSCRIPTIONS).map_err(db_err)?;
        write_row(&mut table, &project_id.to_string(), &sub)?;
        (sub, entry)
    };
    txn.commit().map_err(db_err)?;
    Ok(result)
}

/// Cancelling lets an active period run out and charges nothing after. Cancelling during
/// grace, which is past the paid period, makes the project read-only straight away.
/// Resuming undoes a cancel while the period is still running.
pub(crate) fn set_cancel(db: &Database, cancel: ResolvedCancel) -> Result<Subscription, MetaError> {
    let ResolvedCancel {
        project_id,
        user_id,
        cancel,
        at,
    } = cancel;
    let txn = db.begin_write().map_err(db_err)?;
    let sub = {
        require_owner(&txn, project_id, user_id)?;
        let mut table = txn.open_table(SUBSCRIPTIONS).map_err(db_err)?;
        let mut sub: Subscription =
            read_row(&table, &project_id.to_string())?.ok_or(MetaError::NoSubscription)?;
        match (sub.state, cancel) {
            (SubscriptionState::Active, _) => sub.cancel_at_period_end = cancel,
            (SubscriptionState::Grace, true) => {
                sub.cancel_at_period_end = true;
                sub.state = SubscriptionState::ReadOnly;
                sub.state_since = at;
            }
            (SubscriptionState::ReadOnly | SubscriptionState::Suspended, true) => {
                sub.cancel_at_period_end = true;
            }
            _ => return Err(MetaError::NoSubscription),
        }
        write_row(&mut table, &project_id.to_string(), &sub)?;
        sub
    };
    txn.commit().map_err(db_err)?;
    Ok(sub)
}

/// One step of the background task. Does nothing unless the subscription is still in the
/// state the task saw, so running the task twice, or on two nodes, can't charge twice.
pub(crate) fn advance(
    db: &Database,
    advance: ResolvedAdvance,
) -> Result<Option<(Subscription, Step)>, MetaError> {
    let ResolvedAdvance {
        project_id,
        expected_state,
        expected_period_end,
        timings,
        at,
        entry_id,
    } = advance;
    let txn = db.begin_write().map_err(db_err)?;
    let result = {
        let sub: Option<Subscription> = {
            let table = txn.open_table(SUBSCRIPTIONS).map_err(db_err)?;
            read_row(&table, &project_id.to_string())?
        };
        let Some(mut sub) = sub else {
            return Ok(None);
        };
        if sub.state != expected_state || sub.period_end != expected_period_end {
            return Ok(None);
        }
        let balance = balance_in(&txn, sub.payer)?;
        let Some(step) = next_step(&sub, balance, at, timings) else {
            return Ok(None);
        };
        match step {
            Step::Renew { start, end } => {
                change_balance(
                    &txn,
                    sub.payer,
                    -(sub.plan.price_paise as i64),
                    LedgerReason::Subscription {
                        project_id,
                        plan_id: sub.plan.id.clone(),
                        period_start: start,
                        period_end: end,
                        automatic: true,
                    },
                    "system".into(),
                    at,
                    entry_id,
                )?;
                sub.state = SubscriptionState::Active;
                sub.state_since = at;
                sub.period_start = start;
                sub.period_end = end;
            }
            Step::EnterGrace { since } => {
                sub.state = SubscriptionState::Grace;
                sub.state_since = since;
            }
            Step::EnterReadOnly { since } => {
                sub.state = SubscriptionState::ReadOnly;
                sub.state_since = since;
            }
            Step::Suspend { since } => {
                sub.state = SubscriptionState::Suspended;
                sub.state_since = since;
            }
        }
        let mut table = txn.open_table(SUBSCRIPTIONS).map_err(db_err)?;
        write_row(&mut table, &project_id.to_string(), &sub)?;
        Some((sub, step))
    };
    txn.commit().map_err(db_err)?;
    Ok(result)
}

// ---------------------------------------------------------------------------
// Gift codes
// ---------------------------------------------------------------------------

pub(crate) fn create_gift_code(
    db: &Database,
    new: ResolvedNewGiftCode,
) -> Result<GiftCode, MetaError> {
    let ResolvedNewGiftCode { code, code_id, at } = new;
    let txn = db.begin_write().map_err(db_err)?;
    let gift = {
        let mut hashes = txn.open_table(GIFT_CODE_HASHES).map_err(db_err)?;
        if read_row::<GiftCodeId>(&hashes, &code.code_hash)?.is_some() {
            return Err(MetaError::GiftCodeExists);
        }
        write_row(&mut hashes, &code.code_hash, &code_id)?;
        let gift = GiftCode {
            code_id,
            code_hash: code.code_hash,
            hint: code.hint,
            amount_paise: code.amount_paise,
            expires_at: code.expires_at,
            max_uses: code.max_uses,
            uses: 0,
            disabled: false,
            note: code.note,
            created_by: code.created_by,
            created_at: at,
        };
        let mut codes = txn.open_table(GIFT_CODES).map_err(db_err)?;
        write_row(&mut codes, &code_id.to_string(), &gift)?;
        gift
    };
    txn.commit().map_err(db_err)?;
    Ok(gift)
}

pub(crate) fn list_gift_codes(db: &Database) -> Result<Vec<GiftCode>, MetaError> {
    let txn = db.begin_read().map_err(db_err)?;
    let table = txn.open_table(GIFT_CODES).map_err(db_err)?;
    read_rows(&table)
}

pub(crate) fn set_gift_code_disabled(
    db: &Database,
    code_id: GiftCodeId,
    disabled: bool,
) -> Result<GiftCode, MetaError> {
    let txn = db.begin_write().map_err(db_err)?;
    let gift = {
        let mut table = txn.open_table(GIFT_CODES).map_err(db_err)?;
        let mut gift: GiftCode =
            read_row(&table, &code_id.to_string())?.ok_or(MetaError::NoSuchGiftCode)?;
        gift.disabled = disabled;
        write_row(&mut table, &code_id.to_string(), &gift)?;
        gift
    };
    txn.commit().map_err(db_err)?;
    Ok(gift)
}

pub(crate) fn list_redemptions(
    db: &Database,
    code_id: GiftCodeId,
) -> Result<Vec<GiftCodeRedemption>, MetaError> {
    let txn = db.begin_read().map_err(db_err)?;
    let table = txn.open_table(GIFT_REDEMPTIONS).map_err(db_err)?;
    let start = format!("{code_id}\0");
    let end = format!("{code_id}\u{1}");
    let mut redemptions = Vec::new();
    for row in table.range(start.as_str()..end.as_str()).map_err(db_err)? {
        let (_key, value) = row.map_err(db_err)?;
        redemptions.push(serde_json::from_slice(value.value())?);
    }
    Ok(redemptions)
}

/// Adds a code's amount to the user's credit. Every reason a code doesn't work is checked
/// here, with the use counted in the same step, so a single-use code can't be redeemed twice
/// by two requests at once.
pub(crate) fn redeem(
    db: &Database,
    redeem: ResolvedRedeem,
) -> Result<(GiftCode, LedgerEntry), MetaError> {
    let ResolvedRedeem {
        code_hash,
        user_id,
        at,
        entry_id,
    } = redeem;
    let txn = db.begin_write().map_err(db_err)?;
    let result = {
        let code_id: GiftCodeId = {
            let hashes = txn.open_table(GIFT_CODE_HASHES).map_err(db_err)?;
            read_row(&hashes, &code_hash)?.ok_or(MetaError::NoSuchGiftCode)?
        };
        let mut gift: GiftCode = {
            let codes = txn.open_table(GIFT_CODES).map_err(db_err)?;
            read_row(&codes, &code_id.to_string())?.ok_or(MetaError::NoSuchGiftCode)?
        };
        if gift.disabled {
            return Err(MetaError::NoSuchGiftCode);
        }
        if gift.expires_at.is_some_and(|expires| at >= expires) {
            return Err(MetaError::GiftCodeExpired);
        }
        let redemption_key = format!("{code_id}\0{user_id}");
        {
            let redemptions = txn.open_table(GIFT_REDEMPTIONS).map_err(db_err)?;
            if read_row::<GiftCodeRedemption>(&redemptions, &redemption_key)?.is_some() {
                return Err(MetaError::GiftCodeAlreadyRedeemed);
            }
        }
        if gift.uses >= gift.max_uses {
            return Err(MetaError::GiftCodeUsedUp);
        }

        gift.uses += 1;
        {
            let mut codes = txn.open_table(GIFT_CODES).map_err(db_err)?;
            write_row(&mut codes, &code_id.to_string(), &gift)?;
            let mut redemptions = txn.open_table(GIFT_REDEMPTIONS).map_err(db_err)?;
            write_row(
                &mut redemptions,
                &redemption_key,
                &GiftCodeRedemption {
                    code_id,
                    user_id,
                    amount_paise: gift.amount_paise,
                    at,
                },
            )?;
        }
        let entry = change_balance(
            &txn,
            user_id,
            gift.amount_paise as i64,
            LedgerReason::GiftCode {
                code_id,
                hint: gift.hint.clone(),
            },
            format!("user:{user_id}"),
            at,
            entry_id,
        )?;
        (gift, entry)
    };
    txn.commit().map_err(db_err)?;
    Ok(result)
}

// ---------------------------------------------------------------------------
// Checkouts
// ---------------------------------------------------------------------------

pub(crate) fn create_checkout(
    db: &Database,
    new: ResolvedNewCheckout,
) -> Result<Checkout, MetaError> {
    let ResolvedNewCheckout {
        checkout: new,
        checkout_id,
        at,
    } = new;
    let txn = db.begin_write().map_err(db_err)?;
    let checkout = Checkout {
        checkout_id,
        user_id: new.user_id,
        amount_paise: new.amount_paise,
        project_id: new.project_id,
        plan_id: new.plan_id,
        provider: new.provider,
        provider_ref: new.provider_ref,
        status: CheckoutStatus::Pending,
        created_at: at,
        completed_at: None,
    };
    {
        let mut table = txn.open_table(CHECKOUTS).map_err(db_err)?;
        write_row(&mut table, &checkout_id.to_string(), &checkout)?;
    }
    txn.commit().map_err(db_err)?;
    Ok(checkout)
}

pub(crate) fn get_checkout(
    db: &Database,
    checkout_id: loony_core::CheckoutId,
) -> Result<Option<Checkout>, MetaError> {
    let txn = db.begin_read().map_err(db_err)?;
    let table = txn.open_table(CHECKOUTS).map_err(db_err)?;
    read_row(&table, &checkout_id.to_string())
}

/// Records a checkout's outcome, crediting the user if it succeeded. A checkout that already
/// has an outcome is returned unchanged, because a provider can report the same payment
/// more than once.
pub(crate) fn complete_checkout(
    db: &Database,
    done: ResolvedCompleteCheckout,
) -> Result<Checkout, MetaError> {
    let ResolvedCompleteCheckout {
        checkout_id,
        succeeded,
        provider_ref,
        at,
        entry_id,
    } = done;
    let txn = db.begin_write().map_err(db_err)?;
    let checkout = {
        let mut checkout: Checkout = {
            let table = txn.open_table(CHECKOUTS).map_err(db_err)?;
            read_row(&table, &checkout_id.to_string())?.ok_or(MetaError::NoSuchCheckout)?
        };
        if checkout.status != CheckoutStatus::Pending {
            return Ok(checkout);
        }
        checkout.status = if succeeded {
            CheckoutStatus::Succeeded
        } else {
            CheckoutStatus::Failed
        };
        checkout.completed_at = Some(at);
        if provider_ref.is_some() {
            checkout.provider_ref = provider_ref;
        }
        if succeeded {
            change_balance(
                &txn,
                checkout.user_id,
                checkout.amount_paise as i64,
                LedgerReason::Payment {
                    checkout_id,
                    provider: checkout.provider.clone(),
                },
                format!("user:{}", checkout.user_id),
                at,
                entry_id,
            )?;
        }
        let mut table = txn.open_table(CHECKOUTS).map_err(db_err)?;
        write_row(&mut table, &checkout_id.to_string(), &checkout)?;
        checkout
    };
    txn.commit().map_err(db_err)?;
    Ok(checkout)
}

// ---------------------------------------------------------------------------
// Usage reads and rebuilding
// ---------------------------------------------------------------------------

pub(crate) fn get_usage(db: &Database, owner: OwnerId) -> Result<OwnerUsage, MetaError> {
    let txn = db.begin_read().map_err(db_err)?;
    let table = txn.open_table(OWNER_USAGE).map_err(db_err)?;
    Ok(read_row(&table, &owner.to_string())?.unwrap_or_default())
}

/// The same check a write makes, without writing: lets an upload that can't fit be refused
/// before any of it is stored. The write's own check is still the one that counts.
pub(crate) fn check_write(db: &Database, owner: OwnerId, additional: u64) -> Result<(), MetaError> {
    let txn = db.begin_read().map_err(db_err)?;
    let index = txn.open_table(OWNER_PROJECTS).map_err(db_err)?;
    let projects = txn.open_table(PROJECTS).map_err(db_err)?;
    let subscriptions = txn.open_table(SUBSCRIPTIONS).map_err(db_err)?;
    let Some(limit) = write_limit_in(&index, &projects, &subscriptions, owner)? else {
        return Ok(());
    };
    let usage_table = txn.open_table(OWNER_USAGE).map_err(db_err)?;
    let usage: OwnerUsage = read_row(&usage_table, &owner.to_string())?.unwrap_or_default();
    if usage.exceeds(additional.min(i64::MAX as u64) as i64, limit) {
        return Err(MetaError::QuotaExceeded {
            limit,
            used: usage.counted(),
            requested: additional,
        });
    }
    Ok(())
}

pub(crate) fn derived_indexes_built(db: &Database) -> Result<bool, MetaError> {
    let txn = db.begin_read().map_err(db_err)?;
    let table = txn.open_table(CLUSTER).map_err(db_err)?;
    Ok(table.get(DERIVED_INDEXES_KEY).map_err(db_err)?.is_some())
}

/// Rebuilds the usage counters and indexes from the data itself. Run once when upgrading,
/// for data stored before they existed, and safe to run again: it only reads the tables
/// every node already agrees on.
pub(crate) fn rebuild_derived_indexes(db: &Database) -> Result<(), MetaError> {
    let txn = db.begin_write().map_err(db_err)?;
    {
        let buckets: Vec<Bucket> = read_rows(&txn.open_table(BUCKETS).map_err(db_err)?)?;
        let owner_by_bucket: std::collections::HashMap<BucketId, OwnerId> =
            buckets.iter().map(|b| (b.bucket_id, b.owner_id)).collect();

        let mut usage: std::collections::BTreeMap<OwnerId, OwnerUsage> = Default::default();
        let manifests: Vec<ObjectManifest> = read_rows(&txn.open_table(OBJECTS).map_err(db_err)?)?;
        for manifest in manifests {
            if let Some(owner) = owner_by_bucket.get(&manifest.bucket_id) {
                let entry = usage.entry(*owner).or_default();
                entry.bytes += manifest.total_size();
                entry.objects += 1;
            }
        }
        let uploads: Vec<MultipartUploadState> =
            read_rows(&txn.open_table(MULTIPART).map_err(db_err)?)?;
        for upload in uploads {
            if let Some(owner) = owner_by_bucket.get(&upload.bucket_id) {
                usage.entry(*owner).or_default().pending_bytes +=
                    upload.parts.values().map(|p| p.size).sum::<u64>();
            }
        }
        let projects: Vec<Project> = read_rows(&txn.open_table(PROJECTS).map_err(db_err)?)?;

        let mut table = txn.open_table(OWNER_USAGE).map_err(db_err)?;
        table.retain(|_, _| false).map_err(db_err)?;
        for (owner, counted) in &usage {
            write_row(&mut table, &owner.to_string(), counted)?;
        }
        drop(table);
        let mut table = txn.open_table(BUCKET_OWNERS).map_err(db_err)?;
        table.retain(|_, _| false).map_err(db_err)?;
        for bucket in &buckets {
            write_row(&mut table, &bucket.bucket_id.to_string(), &bucket.owner_id)?;
        }
        drop(table);
        let mut table = txn.open_table(OWNER_PROJECTS).map_err(db_err)?;
        table.retain(|_, _| false).map_err(db_err)?;
        for project in &projects {
            write_row(
                &mut table,
                &project.owner_id.to_string(),
                &project.project_id,
            )?;
        }
        drop(table);
        let mut cluster = txn.open_table(CLUSTER).map_err(db_err)?;
        cluster
            .insert(DERIVED_INDEXES_KEY, &b"true"[..])
            .map_err(db_err)?;
    }
    txn.commit().map_err(db_err)?;
    Ok(())
}
