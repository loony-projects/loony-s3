//! Admin-only changes to who is who: re-linking a user to a new identity after the identity
//! provider was reset, and making someone a project's Owner. Deterministic, like all apply
//! code: see `types.rs`.

use redb::{Database, ReadableTable};

use crate::billing::{Checkout, GiftCodeRedemption, LedgerReason, Subscription};
use crate::commands::{
    Credential, CredentialKind, Project, ProjectInvite, ProjectMember, ProjectRole, User,
};
use crate::error::MetaError;
use crate::raft::applier::{
    CREDENTIALS, PROJECT_INVITES, PROJECT_MEMBERS, PROJECTS, USER_SUBJECTS, USERS, db_err,
};
use crate::raft::billing_apply::{self, CHECKOUTS, CREDITS, GIFT_REDEMPTIONS, SUBSCRIPTIONS};
use crate::raft::projects::{Rows, load_project, read_row, write_row};
use crate::raft::types::{ResolvedMakeOwner, ResolvedRelinkUser};

fn subject_key(user: &User) -> String {
    format!("{}\0{}", user.issuer, user.subject)
}

fn same_verified_email(a: &User, b: &User) -> bool {
    match (&a.email, &b.email) {
        (Some(x), Some(y)) => a.email_verified && b.email_verified && x.eq_ignore_ascii_case(y),
        _ => false,
    }
}

/// Why `from` can't be merged into `into`, if it can't. Checked when an admin previews the
/// pairs and again inside the write, so nothing changes between the two.
pub fn relink_refusal(from: &User, into: &User) -> Option<&'static str> {
    if from.user_id == into.user_id {
        return Some("they're the same user");
    }
    if !same_verified_email(from, into) {
        return Some("they don't share a verified email address");
    }
    if from.issuer == into.issuer && from.subject == into.subject {
        return Some("they already have the same identity");
    }
    // The new identity must have appeared after the old one was last used, or these could be
    // two live accounts that merely share an address.
    if from.created_at <= into.last_sign_in_at {
        return Some("the old user signed in after the new one appeared");
    }
    None
}

/// Every (new, old) pair an admin could re-link: exactly two users with the same verified
/// email, the newer one created after the older one last signed in. Anything ambiguous,
/// such as three users with one address, is left out for a person to sort out.
pub fn relink_candidates(users: &[User]) -> Vec<(User, User)> {
    let mut by_email: std::collections::BTreeMap<String, Vec<&User>> = Default::default();
    for user in users.iter().filter(|u| u.email_verified) {
        if let Some(email) = &user.email {
            by_email
                .entry(email.to_ascii_lowercase())
                .or_default()
                .push(user);
        }
    }
    by_email
        .into_values()
        .filter(|group| group.len() == 2)
        .filter_map(|mut group| {
            group.sort_by_key(|u| (u.created_at, u.user_id));
            let (old, new) = (group[0], group[1]);
            relink_refusal(new, old)
                .is_none()
                .then(|| (new.clone(), old.clone()))
        })
        .collect()
}

fn rewrite_rows<T, F>(table: &mut Rows<'_>, mut change: F) -> Result<(), MetaError>
where
    T: serde::de::DeserializeOwned + serde::Serialize,
    F: FnMut(&mut T) -> bool,
{
    let mut changed = Vec::new();
    for entry in table.iter().map_err(db_err)? {
        let (key, value) = entry.map_err(db_err)?;
        let mut row: T = serde_json::from_slice(value.value())?;
        if change(&mut row) {
            changed.push((key.value().to_string(), row));
        }
    }
    for (key, row) in changed {
        write_row(table, &key, &row)?;
    }
    Ok(())
}

/// Makes `into`, the user from before a reset, sign in as the identity `from` now has, and
/// folds everything `from` did meanwhile into it: memberships (keeping the higher role),
/// credit, gift code redemptions, checkouts, payments and the projects and invites it made.
/// `from`'s browser sessions end, so they sign in again as `into`. `from` is then deleted.
pub(crate) fn relink_user(db: &Database, resolved: ResolvedRelinkUser) -> Result<User, MetaError> {
    let ResolvedRelinkUser {
        from,
        into,
        actor,
        entry_id,
        at,
    } = resolved;
    let txn = db.begin_write().map_err(db_err)?;
    let merged = {
        let mut users = txn.open_table(USERS).map_err(db_err)?;
        let load = |users: &Rows<'_>, id: loony_core::UserId| -> Result<User, MetaError> {
            read_row(users, &id.to_string())?.ok_or_else(|| MetaError::NoSuchUser(id.to_string()))
        };
        let new = load(&users, from)?;
        let old = load(&users, into)?;
        if let Some(reason) = relink_refusal(&new, &old) {
            return Err(MetaError::CannotRelink(reason.to_string()));
        }

        let mut subjects = txn.open_table(USER_SUBJECTS).map_err(db_err)?;
        subjects
            .remove(subject_key(&old).as_str())
            .map_err(db_err)?;
        let id_bytes = serde_json::to_vec(&into)?;
        subjects
            .insert(subject_key(&new).as_str(), id_bytes.as_slice())
            .map_err(db_err)?;

        let merged = User {
            user_id: into,
            issuer: new.issuer.clone(),
            subject: new.subject.clone(),
            email: new.email.clone(),
            email_verified: new.email_verified,
            name: new.name.clone(),
            // A block on either stays.
            enabled: old.enabled && new.enabled,
            created_at: old.created_at,
            last_sign_in_at: new.last_sign_in_at.max(old.last_sign_in_at),
        };
        write_row(&mut users, &into.to_string(), &merged)?;
        users.remove(from.to_string().as_str()).map_err(db_err)?;
        merged
    };

    {
        let mut members = txn.open_table(PROJECT_MEMBERS).map_err(db_err)?;
        let mut moved = Vec::new();
        for entry in members.iter().map_err(db_err)? {
            let (key, value) = entry.map_err(db_err)?;
            let member: ProjectMember = serde_json::from_slice(value.value())?;
            if member.user_id == from {
                moved.push((key.value().to_string(), member));
            }
        }
        for (key, member) in moved {
            members.remove(key.as_str()).map_err(db_err)?;
            let target = format!("{}\0{into}", member.project_id);
            let existing: Option<ProjectMember> = read_row(&members, &target)?;
            let row = match existing {
                Some(mut kept) => {
                    if member.role == ProjectRole::Owner {
                        kept.role = ProjectRole::Owner;
                    }
                    kept
                }
                None => ProjectMember {
                    user_id: into,
                    ..member
                },
            };
            write_row(&mut members, &target, &row)?;
        }
    }
    {
        let mut projects = txn.open_table(PROJECTS).map_err(db_err)?;
        rewrite_rows(&mut projects, |p: &mut Project| {
            (p.created_by == from)
                .then(|| p.created_by = into)
                .is_some()
        })?;
        let mut invites = txn.open_table(PROJECT_INVITES).map_err(db_err)?;
        rewrite_rows(&mut invites, |i: &mut ProjectInvite| {
            (i.invited_by == from)
                .then(|| i.invited_by = into)
                .is_some()
        })?;
    }
    {
        let mut credentials = txn.open_table(CREDENTIALS).map_err(db_err)?;
        // Browser sessions end; keys made for apps keep working and now show the old user.
        crate::raft::projects::remove_credentials(&mut credentials, |c| {
            c.user_id == Some(from)
                && matches!(
                    c.kind,
                    CredentialKind::UserSession | CredentialKind::ProjectSession
                )
        })?;
        rewrite_rows(&mut credentials, |c: &mut Credential| {
            (c.user_id == Some(from))
                .then(|| c.user_id = Some(into))
                .is_some()
        })?;
    }
    {
        let mut subscriptions = txn.open_table(SUBSCRIPTIONS).map_err(db_err)?;
        rewrite_rows(&mut subscriptions, |s: &mut Subscription| {
            (s.payer == from).then(|| s.payer = into).is_some()
        })?;
        let mut checkouts = txn.open_table(CHECKOUTS).map_err(db_err)?;
        rewrite_rows(&mut checkouts, |c: &mut Checkout| {
            (c.user_id == from).then(|| c.user_id = into).is_some()
        })?;
    }
    {
        let mut redemptions = txn.open_table(GIFT_REDEMPTIONS).map_err(db_err)?;
        let mut moved = Vec::new();
        for entry in redemptions.iter().map_err(db_err)? {
            let (key, value) = entry.map_err(db_err)?;
            let redemption: GiftCodeRedemption = serde_json::from_slice(value.value())?;
            if redemption.user_id == from {
                moved.push((key.value().to_string(), redemption));
            }
        }
        for (key, redemption) in moved {
            redemptions.remove(key.as_str()).map_err(db_err)?;
            let target = format!("{}\0{into}", redemption.code_id);
            // Both redeemed the same code: the credit is merged below, and one record of
            // the code being used by this person is enough.
            if read_row::<GiftCodeRedemption>(&redemptions, &target)?.is_none() {
                write_row(
                    &mut redemptions,
                    &target,
                    &GiftCodeRedemption {
                        user_id: into,
                        ..redemption
                    },
                )?;
            }
        }
    }
    let balance: u64 = {
        let mut credits = txn.open_table(CREDITS).map_err(db_err)?;
        let balance = read_row(&credits, &from.to_string())?.unwrap_or(0);
        credits.remove(from.to_string().as_str()).map_err(db_err)?;
        balance
    };
    if balance > 0 {
        let amount = i64::try_from(balance)
            .map_err(|_| MetaError::CannotRelink("the credit is too large to move".into()))?;
        billing_apply::change_balance(
            &txn,
            into,
            amount,
            LedgerReason::AccountMerged { from_user: from },
            actor,
            at,
            entry_id,
        )?;
    }
    txn.commit().map_err(db_err)?;
    Ok(merged)
}

/// Makes `user_id` an Owner of the project, adding them if they aren't a member, for when
/// a project's only Owner can't act any more, such as after deleting their Loony account.
pub(crate) fn make_project_owner(
    db: &Database,
    resolved: ResolvedMakeOwner,
) -> Result<ProjectMember, MetaError> {
    let ResolvedMakeOwner {
        project_id,
        user_id,
        at,
    } = resolved;
    let txn = db.begin_write().map_err(db_err)?;
    let member = {
        let projects = txn.open_table(PROJECTS).map_err(db_err)?;
        load_project(&projects, project_id)?;
        let users = txn.open_table(USERS).map_err(db_err)?;
        read_row::<User>(&users, &user_id.to_string())?
            .ok_or_else(|| MetaError::NoSuchUser(user_id.to_string()))?;

        let mut members = txn.open_table(PROJECT_MEMBERS).map_err(db_err)?;
        let key = format!("{project_id}\0{user_id}");
        let member = match read_row::<ProjectMember>(&members, &key)? {
            Some(member) => ProjectMember {
                role: ProjectRole::Owner,
                ..member
            },
            None => ProjectMember {
                project_id,
                user_id,
                role: ProjectRole::Owner,
                added_at: at,
            },
        };
        write_row(&mut members, &key, &member)?;
        member
    };
    txn.commit().map_err(db_err)?;
    Ok(member)
}
