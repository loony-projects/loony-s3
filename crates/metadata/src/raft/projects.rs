//! Applying project changes. Each rule that must hold against concurrent requests, such
//! as a project always keeping an Owner, is checked in the same write transaction as the
//! change it guards. Everything here must be deterministic: see `types.rs`.

use loony_core::{Bucket, InviteId, ProjectId, UserId};
use redb::{Database, ReadableTable, Table};
use serde::de::DeserializeOwned;
use time::OffsetDateTime;

use crate::billing::SubscriptionState;
use crate::commands::{
    Credential, CredentialKind, Project, ProjectInvite, ProjectMember, ProjectRole, User,
};
use crate::error::MetaError;
use crate::raft::applier::{
    BUCKETS, CREDENTIALS, PROJECT_INVITES, PROJECT_MEMBERS, PROJECTS, USERS, db_err,
};
use crate::raft::billing_apply;
use crate::raft::types::{
    ResolvedAdoptOwner, ResolvedCreateInvite, ResolvedCreateProject, ResolvedInviteAnswer,
    ResolvedTouchCredential,
};

pub(crate) type Rows<'txn> = Table<'txn, &'static str, &'static [u8]>;

fn member_key(project_id: ProjectId, user_id: UserId) -> String {
    format!("{project_id}\0{user_id}")
}

pub(crate) fn read_row<T: DeserializeOwned>(
    table: &impl ReadableTable<&'static str, &'static [u8]>,
    key: &str,
) -> Result<Option<T>, MetaError> {
    match table.get(key).map_err(db_err)? {
        Some(guard) => Ok(Some(serde_json::from_slice(guard.value())?)),
        None => Ok(None),
    }
}

pub(crate) fn read_rows<T: DeserializeOwned>(
    table: &impl ReadableTable<&'static str, &'static [u8]>,
) -> Result<Vec<T>, MetaError> {
    let mut rows = Vec::new();
    for entry in table.iter().map_err(db_err)? {
        let (_key, value) = entry.map_err(db_err)?;
        rows.push(serde_json::from_slice(value.value())?);
    }
    Ok(rows)
}

pub(crate) fn write_row<T: serde::Serialize>(
    table: &mut Rows<'_>,
    key: &str,
    row: &T,
) -> Result<(), MetaError> {
    let bytes = serde_json::to_vec(row)?;
    table.insert(key, bytes.as_slice()).map_err(db_err)?;
    Ok(())
}

/// Deletes the rows of type `T` matching `remove`.
pub(crate) fn remove_rows<T: DeserializeOwned>(
    table: &mut Rows<'_>,
    remove: impl Fn(&T) -> bool,
) -> Result<(), MetaError> {
    let mut keys = Vec::new();
    for entry in table.iter().map_err(db_err)? {
        let (key, value) = entry.map_err(db_err)?;
        if remove(&serde_json::from_slice(value.value())?) {
            keys.push(key.value().to_string());
        }
    }
    for key in keys {
        table.remove(key.as_str()).map_err(db_err)?;
    }
    Ok(())
}

pub(crate) fn remove_credentials(
    credentials: &mut Rows<'_>,
    remove: impl Fn(&Credential) -> bool,
) -> Result<(), MetaError> {
    remove_rows(credentials, remove)
}

pub(crate) fn load_project(table: &Rows<'_>, project_id: ProjectId) -> Result<Project, MetaError> {
    read_row(table, &project_id.to_string())?
        .ok_or_else(|| MetaError::NoSuchProject(project_id.to_string()))
}

fn members_of(table: &Rows<'_>, project_id: ProjectId) -> Result<Vec<ProjectMember>, MetaError> {
    let mut members: Vec<ProjectMember> = read_rows(table)?;
    members.retain(|member| member.project_id == project_id);
    Ok(members)
}

pub(crate) fn create_project(
    db: &Database,
    resolved: ResolvedCreateProject,
) -> Result<Project, MetaError> {
    let ResolvedCreateProject {
        cmd,
        project_id,
        owner_id,
        at,
    } = resolved;
    let write_txn = db.begin_write().map_err(db_err)?;
    let project = {
        let mut projects = write_txn.open_table(PROJECTS).map_err(db_err)?;
        let existing: Vec<Project> = read_rows(&projects)?;
        let created = existing
            .iter()
            .filter(|p| p.created_by == cmd.created_by)
            .count();
        if created >= cmd.max_created as usize {
            return Err(MetaError::ProjectLimitReached);
        }

        let project = Project {
            project_id,
            name: cmd.name,
            owner_id,
            enabled: true,
            created_by: cmd.created_by,
            created_at: at,
        };
        write_row(&mut projects, &project_id.to_string(), &project)?;
        drop(projects);
        billing_apply::index_project(&write_txn, owner_id, Some(project_id))?;

        let mut members = write_txn.open_table(PROJECT_MEMBERS).map_err(db_err)?;
        write_row(
            &mut members,
            &member_key(project_id, cmd.created_by),
            &ProjectMember {
                project_id,
                user_id: cmd.created_by,
                role: ProjectRole::Owner,
                added_at: at,
            },
        )?;
        project
    };
    write_txn.commit().map_err(db_err)?;
    Ok(project)
}

pub(crate) fn adopt_owner(
    db: &Database,
    resolved: ResolvedAdoptOwner,
) -> Result<Project, MetaError> {
    let ResolvedAdoptOwner {
        cmd,
        project_id,
        at,
    } = resolved;
    let owner = cmd.owner_id.to_string();

    let write_txn = db.begin_write().map_err(db_err)?;
    let project = {
        let users = write_txn.open_table(USERS).map_err(db_err)?;
        if read_row::<User>(&users, &cmd.user_id.to_string())?.is_none() {
            return Err(MetaError::NoSuchUser(cmd.user_id.to_string()));
        }

        let mut credentials = write_txn.open_table(CREDENTIALS).map_err(db_err)?;
        let mut keys: Vec<Credential> = read_rows(&credentials)?;
        keys.retain(|key| key.owner_id == cmd.owner_id);
        if keys.iter().any(|key| key.admin) {
            return Err(MetaError::OwnerIsAdmin(owner));
        }
        if keys.iter().any(|key| key.project_id.is_some()) {
            return Err(MetaError::OwnerAlreadyInProject(owner));
        }

        let mut projects = write_txn.open_table(PROJECTS).map_err(db_err)?;
        let existing: Vec<Project> = read_rows(&projects)?;
        if existing.iter().any(|p| p.owner_id == cmd.owner_id) {
            return Err(MetaError::OwnerAlreadyInProject(owner));
        }

        let buckets = write_txn.open_table(BUCKETS).map_err(db_err)?;
        let buckets: Vec<Bucket> = read_rows(&buckets)?;
        if keys.is_empty() && !buckets.iter().any(|b| b.owner_id == cmd.owner_id) {
            return Err(MetaError::NoSuchOwner(owner));
        }

        let project = Project {
            project_id,
            name: cmd.name,
            owner_id: cmd.owner_id,
            enabled: true,
            created_by: cmd.user_id,
            created_at: at,
        };
        write_row(&mut projects, &project_id.to_string(), &project)?;
        drop(projects);
        billing_apply::index_project(&write_txn, cmd.owner_id, Some(project_id))?;

        for mut key in keys {
            key.project_id = Some(project_id);
            key.kind = CredentialKind::ProjectAccessKey;
            key.name.get_or_insert_with(|| "Imported key".to_string());
            let access_key = key.access_key.clone();
            write_row(&mut credentials, &access_key, &key)?;
        }

        let mut members = write_txn.open_table(PROJECT_MEMBERS).map_err(db_err)?;
        write_row(
            &mut members,
            &member_key(project_id, cmd.user_id),
            &ProjectMember {
                project_id,
                user_id: cmd.user_id,
                role: ProjectRole::Owner,
                added_at: at,
            },
        )?;
        project
    };
    write_txn.commit().map_err(db_err)?;
    Ok(project)
}

pub(crate) fn get_project(
    db: &Database,
    project_id: ProjectId,
) -> Result<Option<Project>, MetaError> {
    let read_txn = db.begin_read().map_err(db_err)?;
    let table = read_txn.open_table(PROJECTS).map_err(db_err)?;
    read_row(&table, &project_id.to_string())
}

pub(crate) fn list_projects(db: &Database) -> Result<Vec<Project>, MetaError> {
    let read_txn = db.begin_read().map_err(db_err)?;
    let table = read_txn.open_table(PROJECTS).map_err(db_err)?;
    read_rows(&table)
}

pub(crate) fn rename_project(
    db: &Database,
    project_id: ProjectId,
    name: String,
) -> Result<Project, MetaError> {
    let write_txn = db.begin_write().map_err(db_err)?;
    let project = {
        let mut projects = write_txn.open_table(PROJECTS).map_err(db_err)?;
        let mut project = load_project(&projects, project_id)?;
        project.name = name;
        write_row(&mut projects, &project_id.to_string(), &project)?;
        project
    };
    write_txn.commit().map_err(db_err)?;
    Ok(project)
}

pub(crate) fn set_project_enabled(
    db: &Database,
    project_id: ProjectId,
    enabled: bool,
) -> Result<Project, MetaError> {
    let write_txn = db.begin_write().map_err(db_err)?;
    let project = {
        let mut projects = write_txn.open_table(PROJECTS).map_err(db_err)?;
        let mut project = load_project(&projects, project_id)?;
        project.enabled = enabled;
        write_row(&mut projects, &project_id.to_string(), &project)?;
        if !enabled {
            let mut credentials = write_txn.open_table(CREDENTIALS).map_err(db_err)?;
            remove_credentials(&mut credentials, |c| {
                c.kind == CredentialKind::ProjectSession && c.project_id == Some(project_id)
            })?;
        }
        project
    };
    write_txn.commit().map_err(db_err)?;
    Ok(project)
}

pub(crate) fn delete_project(db: &Database, project_id: ProjectId) -> Result<(), MetaError> {
    let write_txn = db.begin_write().map_err(db_err)?;
    {
        let mut projects = write_txn.open_table(PROJECTS).map_err(db_err)?;
        let project = load_project(&projects, project_id)?;

        let buckets = write_txn.open_table(BUCKETS).map_err(db_err)?;
        let buckets: Vec<Bucket> = read_rows(&buckets)?;
        if buckets.iter().any(|b| b.owner_id == project.owner_id) {
            return Err(MetaError::ProjectNotEmpty(project_id.to_string()));
        }

        let mut credentials = write_txn.open_table(CREDENTIALS).map_err(db_err)?;
        remove_credentials(&mut credentials, |c| c.project_id == Some(project_id))?;
        let mut members = write_txn.open_table(PROJECT_MEMBERS).map_err(db_err)?;
        remove_rows(&mut members, |m: &ProjectMember| m.project_id == project_id)?;
        let mut invites = write_txn.open_table(PROJECT_INVITES).map_err(db_err)?;
        remove_rows(&mut invites, |i: &ProjectInvite| i.project_id == project_id)?;
        projects
            .remove(project_id.to_string().as_str())
            .map_err(db_err)?;
        drop((projects, credentials, members, invites));
        billing_apply::index_project(&write_txn, project.owner_id, None)?;
        billing_apply::forget_subscription(&write_txn, project_id)?;
    }
    write_txn.commit().map_err(db_err)?;
    Ok(())
}

pub(crate) fn get_member(
    db: &Database,
    project_id: ProjectId,
    user_id: UserId,
) -> Result<Option<ProjectMember>, MetaError> {
    let read_txn = db.begin_read().map_err(db_err)?;
    let table = read_txn.open_table(PROJECT_MEMBERS).map_err(db_err)?;
    read_row(&table, &member_key(project_id, user_id))
}

pub(crate) fn list_members(
    db: &Database,
    project_id: ProjectId,
) -> Result<Vec<ProjectMember>, MetaError> {
    let read_txn = db.begin_read().map_err(db_err)?;
    let table = read_txn.open_table(PROJECT_MEMBERS).map_err(db_err)?;
    let mut members: Vec<ProjectMember> = read_rows(&table)?;
    members.retain(|member| member.project_id == project_id);
    Ok(members)
}

pub(crate) fn list_user_memberships(
    db: &Database,
    user_id: UserId,
) -> Result<Vec<ProjectMember>, MetaError> {
    let read_txn = db.begin_read().map_err(db_err)?;
    let table = read_txn.open_table(PROJECT_MEMBERS).map_err(db_err)?;
    let mut members: Vec<ProjectMember> = read_rows(&table)?;
    members.retain(|member| member.user_id == user_id);
    Ok(members)
}

/// Whether removing `user_id`'s Owner role would leave no Owner.
fn is_last_owner(members: &[ProjectMember], user_id: UserId) -> bool {
    let is_owner = |m: &&ProjectMember| m.role == ProjectRole::Owner;
    members
        .iter()
        .filter(is_owner)
        .all(|m| m.user_id == user_id)
}

pub(crate) fn set_member_role(
    db: &Database,
    project_id: ProjectId,
    user_id: UserId,
    role: ProjectRole,
) -> Result<ProjectMember, MetaError> {
    let write_txn = db.begin_write().map_err(db_err)?;
    let member = {
        let mut table = write_txn.open_table(PROJECT_MEMBERS).map_err(db_err)?;
        let members = members_of(&table, project_id)?;
        let mut member = members
            .iter()
            .find(|m| m.user_id == user_id)
            .cloned()
            .ok_or(MetaError::NoSuchMember)?;
        if role != ProjectRole::Owner && is_last_owner(&members, user_id) {
            return Err(MetaError::LastOwner);
        }
        member.role = role;
        write_row(&mut table, &member_key(project_id, user_id), &member)?;
        member
    };
    write_txn.commit().map_err(db_err)?;
    Ok(member)
}

pub(crate) fn remove_member(
    db: &Database,
    project_id: ProjectId,
    user_id: UserId,
) -> Result<(), MetaError> {
    let write_txn = db.begin_write().map_err(db_err)?;
    {
        let mut table = write_txn.open_table(PROJECT_MEMBERS).map_err(db_err)?;
        let members = members_of(&table, project_id)?;
        if !members.iter().any(|m| m.user_id == user_id) {
            return Err(MetaError::NoSuchMember);
        }
        if is_last_owner(&members, user_id) {
            return Err(MetaError::LastOwner);
        }
        table
            .remove(member_key(project_id, user_id).as_str())
            .map_err(db_err)?;

        let mut credentials = write_txn.open_table(CREDENTIALS).map_err(db_err)?;
        remove_credentials(&mut credentials, |c| {
            c.kind == CredentialKind::ProjectSession
                && c.project_id == Some(project_id)
                && c.user_id == Some(user_id)
        })?;
    }
    write_txn.commit().map_err(db_err)?;
    Ok(())
}

pub(crate) fn create_invite(
    db: &Database,
    resolved: ResolvedCreateInvite,
) -> Result<ProjectInvite, MetaError> {
    let ResolvedCreateInvite { cmd, invite_id, at } = resolved;
    let write_txn = db.begin_write().map_err(db_err)?;
    let invite = {
        let projects = write_txn.open_table(PROJECTS).map_err(db_err)?;
        let project = load_project(&projects, cmd.project_id)?;
        if !project.enabled {
            return Err(MetaError::ProjectDisabled(cmd.project_id.to_string()));
        }

        let members_table = write_txn.open_table(PROJECT_MEMBERS).map_err(db_err)?;
        let users = write_txn.open_table(USERS).map_err(db_err)?;
        for member in members_of(&members_table, cmd.project_id)? {
            let user: Option<User> = read_row(&users, &member.user_id.to_string())?;
            let same_email = user
                .and_then(|user| user.email)
                .is_some_and(|email| email.eq_ignore_ascii_case(&cmd.email));
            if same_email {
                return Err(MetaError::AlreadyMember);
            }
        }

        let mut invites = write_txn.open_table(PROJECT_INVITES).map_err(db_err)?;
        let existing: Vec<ProjectInvite> = read_rows(&invites)?;
        let same = |i: &&ProjectInvite| i.project_id == cmd.project_id && i.email == cmd.email;
        if existing.iter().filter(same).any(|i| i.expires_at > at) {
            return Err(MetaError::InviteExists);
        }
        // One that has expired is simply replaced.
        for old in existing.iter().filter(same) {
            invites
                .remove(old.invite_id.to_string().as_str())
                .map_err(db_err)?;
        }

        let invite = ProjectInvite {
            invite_id,
            project_id: cmd.project_id,
            email: cmd.email,
            role: cmd.role,
            invited_by: cmd.invited_by,
            created_at: at,
            expires_at: cmd.expires_at,
        };
        write_row(&mut invites, &invite_id.to_string(), &invite)?;
        invite
    };
    write_txn.commit().map_err(db_err)?;
    Ok(invite)
}

pub(crate) fn get_invite(
    db: &Database,
    invite_id: InviteId,
) -> Result<Option<ProjectInvite>, MetaError> {
    let read_txn = db.begin_read().map_err(db_err)?;
    let table = read_txn.open_table(PROJECT_INVITES).map_err(db_err)?;
    read_row(&table, &invite_id.to_string())
}

pub(crate) fn list_project_invites(
    db: &Database,
    project_id: ProjectId,
) -> Result<Vec<ProjectInvite>, MetaError> {
    let mut invites = list_invites(db)?;
    invites.retain(|invite| invite.project_id == project_id);
    Ok(invites)
}

pub(crate) fn list_invites(db: &Database) -> Result<Vec<ProjectInvite>, MetaError> {
    let read_txn = db.begin_read().map_err(db_err)?;
    let table = read_txn.open_table(PROJECT_INVITES).map_err(db_err)?;
    read_rows(&table)
}

pub(crate) fn delete_invite(db: &Database, invite_id: InviteId) -> Result<(), MetaError> {
    let write_txn = db.begin_write().map_err(db_err)?;
    {
        let mut table = write_txn.open_table(PROJECT_INVITES).map_err(db_err)?;
        table
            .remove(invite_id.to_string().as_str())
            .map_err(db_err)?;
    }
    write_txn.commit().map_err(db_err)?;
    Ok(())
}

/// Returns the membership when accepting, and `None` when declining.
pub(crate) fn answer_invite(
    db: &Database,
    answer: ResolvedInviteAnswer,
) -> Result<Option<ProjectMember>, MetaError> {
    let ResolvedInviteAnswer {
        invite_id,
        user_id,
        accept,
        at,
    } = answer;
    let write_txn = db.begin_write().map_err(db_err)?;
    let member = {
        let mut invites = write_txn.open_table(PROJECT_INVITES).map_err(db_err)?;
        let invite: ProjectInvite =
            read_row(&invites, &invite_id.to_string())?.ok_or(MetaError::NoSuchInvite)?;

        let users = write_txn.open_table(USERS).map_err(db_err)?;
        let user: User = read_row(&users, &user_id.to_string())?
            .ok_or_else(|| MetaError::NoSuchUser(user_id.to_string()))?;
        let matches = user
            .email
            .as_deref()
            .is_some_and(|email| email.eq_ignore_ascii_case(&invite.email));
        if !matches {
            return Err(MetaError::InviteEmailMismatch);
        }
        if !user.email_verified {
            return Err(MetaError::EmailNotVerified);
        }

        let member = if accept {
            if at >= invite.expires_at {
                return Err(MetaError::InviteExpired);
            }
            let projects = write_txn.open_table(PROJECTS).map_err(db_err)?;
            let project = load_project(&projects, invite.project_id)?;
            if !project.enabled {
                return Err(MetaError::ProjectDisabled(invite.project_id.to_string()));
            }

            let mut members = write_txn.open_table(PROJECT_MEMBERS).map_err(db_err)?;
            let key = member_key(invite.project_id, user_id);
            // Already a member, as when their email changed to the invited one: an invite
            // never changes the role they have.
            let member = match read_row::<ProjectMember>(&members, &key)? {
                Some(existing) => existing,
                None => {
                    let member = ProjectMember {
                        project_id: invite.project_id,
                        user_id,
                        role: invite.role,
                        added_at: at,
                    };
                    write_row(&mut members, &key, &member)?;
                    member
                }
            };
            Some(member)
        } else {
            None
        };
        invites
            .remove(invite_id.to_string().as_str())
            .map_err(db_err)?;
        member
    };
    write_txn.commit().map_err(db_err)?;
    Ok(member)
}

pub(crate) fn put_project_credential(
    db: &Database,
    credential: Credential,
    max_access_keys: u32,
) -> Result<(), MetaError> {
    let project_id = credential
        .project_id
        .ok_or_else(|| MetaError::Db("a project key must name its project".into()))?;

    let write_txn = db.begin_write().map_err(db_err)?;
    {
        let projects = write_txn.open_table(PROJECTS).map_err(db_err)?;
        let project = load_project(&projects, project_id)?;
        if !project.enabled {
            return Err(MetaError::ProjectDisabled(project_id.to_string()));
        }
        if project.owner_id != credential.owner_id {
            return Err(MetaError::Db(
                "a project key must use its project's owner".into(),
            ));
        }

        drop(projects);
        // A suspended project gives out no keys at all. One that can't take on data can still
        // be opened, to see what's there and to pay, but gets no new access keys.
        let state = billing_apply::project_subscription_state(&write_txn, project_id)?;
        match credential.kind {
            CredentialKind::ProjectSession if state == SubscriptionState::Suspended => {
                return Err(MetaError::ProjectNotWritable(state));
            }
            CredentialKind::ProjectAccessKey if !state.writable() => {
                return Err(MetaError::ProjectNotWritable(state));
            }
            _ => {}
        }
        let mut credentials = write_txn.open_table(CREDENTIALS).map_err(db_err)?;
        match credential.kind {
            CredentialKind::ProjectSession => {
                let user_id = credential.user_id.ok_or(MetaError::NoSuchMember)?;
                let members = write_txn.open_table(PROJECT_MEMBERS).map_err(db_err)?;
                if read_row::<ProjectMember>(&members, &member_key(project_id, user_id))?.is_none()
                {
                    return Err(MetaError::NoSuchMember);
                }
            }
            CredentialKind::ProjectAccessKey => {
                let existing: Vec<Credential> = read_rows(&credentials)?;
                let count = existing
                    .iter()
                    .filter(|c| {
                        c.kind == CredentialKind::ProjectAccessKey
                            && c.project_id == Some(project_id)
                    })
                    .count();
                if count >= max_access_keys as usize {
                    return Err(MetaError::KeyLimitReached);
                }
            }
            CredentialKind::Standard | CredentialKind::UserSession => {
                return Err(MetaError::Db("not a project key".into()));
            }
        }
        write_row(&mut credentials, &credential.access_key, &credential)?;
    }
    write_txn.commit().map_err(db_err)?;
    Ok(())
}

pub(crate) fn set_credential_enabled(
    db: &Database,
    access_key: &str,
    enabled: bool,
) -> Result<(), MetaError> {
    let write_txn = db.begin_write().map_err(db_err)?;
    {
        let mut table = write_txn.open_table(CREDENTIALS).map_err(db_err)?;
        let mut credential: Credential = read_row(&table, access_key)?
            .ok_or_else(|| MetaError::NoSuchCredential(access_key.to_string()))?;
        credential.enabled = enabled;
        write_row(&mut table, access_key, &credential)?;
    }
    write_txn.commit().map_err(db_err)?;
    Ok(())
}

pub(crate) fn touch_credential(
    db: &Database,
    touch: ResolvedTouchCredential,
) -> Result<(), MetaError> {
    let write_txn = db.begin_write().map_err(db_err)?;
    {
        let mut table = write_txn.open_table(CREDENTIALS).map_err(db_err)?;
        let credential: Option<Credential> = read_row(&table, &touch.access_key)?;
        if let Some(mut credential) = credential {
            let at: OffsetDateTime = touch.at;
            if credential.last_used_at.is_none_or(|last| last < at) {
                credential.last_used_at = Some(at);
                write_row(&mut table, &touch.access_key, &credential)?;
            }
        }
    }
    write_txn.commit().map_err(db_err)?;
    Ok(())
}

pub(crate) fn delete_session(db: &Database, access_key: &str) -> Result<(), MetaError> {
    let write_txn = db.begin_write().map_err(db_err)?;
    {
        let mut table = write_txn.open_table(CREDENTIALS).map_err(db_err)?;
        remove_credentials(&mut table, |c| {
            c.access_key == access_key || c.session_of.as_deref() == Some(access_key)
        })?;
    }
    write_txn.commit().map_err(db_err)?;
    Ok(())
}
