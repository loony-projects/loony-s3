//! Everything a signed-in user does with projects. A non-member gets the same 404 as for a
//! project that doesn't exist, so these endpoints never confirm that a project exists.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use loony_admin::AuditEntry;
use loony_core::{InviteId, ProjectId, UserId};
use loony_metadata::{
    CreateInvite, CreateProject, Credential, CredentialKind, Project, ProjectInvite, ProjectMember,
    ProjectRole, SubscriptionState,
};
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::ConsoleState;
use crate::auth::SignedIn;
use crate::error::ConsoleError;
use crate::handlers::random_hex;

type Result<T> = std::result::Result<T, ConsoleError>;

/// How many projects one person can create.
pub const MAX_PROJECTS_PER_USER: u32 = 10;
pub const MAX_ACCESS_KEYS_PER_PROJECT: u32 = 100;
const INVITE_LIFETIME: Duration = Duration::days(7);
const MAX_NAME_CHARS: usize = 100;
const MAX_EMAIL_CHARS: usize = 254;

fn not_found() -> ConsoleError {
    ConsoleError::new(
        StatusCode::NOT_FOUND,
        "not_found",
        "That doesn't exist, or you don't have access to it.",
    )
}

fn clean_name(name: &str, what: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
        return Err(ConsoleError::invalid_request(format!(
            "{what} must be 1 to {MAX_NAME_CHARS} characters."
        )));
    }
    if name.chars().any(char::is_control) {
        return Err(ConsoleError::invalid_request(format!(
            "{what} can't contain control characters."
        )));
    }
    Ok(name.to_string())
}

fn clean_email(email: &str) -> Result<String> {
    let email = email.trim().to_ascii_lowercase();
    let valid = email.len() <= MAX_EMAIL_CHARS
        && !email.chars().any(|c| c.is_whitespace() || c.is_control())
        && email
            .split_once('@')
            .is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.'));
    if !valid {
        return Err(ConsoleError::invalid_request(
            "That doesn't look like an email address.",
        ));
    }
    Ok(email)
}

fn parse_body<T>(body: std::result::Result<Json<T>, JsonRejection>) -> Result<T> {
    body.map(|Json(value)| value)
        .map_err(|_| ConsoleError::invalid_request("The request is malformed."))
}

/// Records what happened, including attempts that were refused, and passes the result on.
async fn audited<T>(
    state: &ConsoleState,
    signed_in: &SignedIn,
    action: &str,
    target: String,
    result: Result<T>,
) -> Result<T> {
    let (outcome, detail) = match &result {
        Ok(_) => ("ok", None),
        Err(err) if err.status.is_client_error() => ("denied", Some(err.message.clone())),
        Err(err) => ("failed", Some(err.message.clone())),
    };
    state
        .audit
        .record(AuditEntry {
            at: OffsetDateTime::now_utc(),
            actor: format!("user:{}", signed_in.user.user_id),
            action: action.to_string(),
            target,
            outcome: outcome.to_string(),
            detail,
        })
        .await;
    result
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Need {
    /// Any member, even while an admin has the project disabled.
    Read,
    /// Any member, on an enabled project.
    Member,
    /// An Owner, on an enabled project.
    Owner,
}

async fn access(
    state: &ConsoleState,
    user_id: UserId,
    project_id: ProjectId,
    need: Need,
) -> Result<(Project, ProjectMember)> {
    let member = state
        .metadata
        .get_member(project_id, user_id)
        .await?
        .ok_or_else(not_found)?;
    let project = state
        .metadata
        .get_project(project_id)
        .await?
        .ok_or_else(not_found)?;

    if need != Need::Read && !project.enabled {
        return Err(ConsoleError::new(
            StatusCode::FORBIDDEN,
            "project_disabled",
            "An administrator has disabled this project.",
        ));
    }
    if need == Need::Owner && member.role != ProjectRole::Owner {
        return Err(ConsoleError::new(
            StatusCode::FORBIDDEN,
            "owner_only",
            "Only a project Owner can do that.",
        ));
    }
    Ok((project, member))
}

#[derive(Serialize)]
pub struct ProjectView {
    id: ProjectId,
    name: String,
    enabled: bool,
    role: ProjectRole,
    created_by_me: bool,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    subscription: SubscriptionState,
}

async fn project_view(
    state: &ConsoleState,
    project: Project,
    role: ProjectRole,
    user_id: UserId,
) -> Result<ProjectView> {
    let subscription = state
        .metadata
        .get_subscription(project.project_id)
        .await?
        .map_or(SubscriptionState::Unpaid, |sub| sub.state);
    Ok(ProjectView {
        id: project.project_id,
        name: project.name,
        enabled: project.enabled,
        role,
        created_by_me: project.created_by == user_id,
        created_at: project.created_at,
        subscription,
    })
}

#[derive(Serialize)]
pub struct InviteView {
    id: InviteId,
    project_id: ProjectId,
    email: String,
    role: ProjectRole,
    invited_by: UserId,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
}

impl From<ProjectInvite> for InviteView {
    fn from(invite: ProjectInvite) -> Self {
        Self {
            id: invite.invite_id,
            project_id: invite.project_id,
            email: invite.email,
            role: invite.role,
            invited_by: invite.invited_by,
            created_at: invite.created_at,
            expires_at: invite.expires_at,
        }
    }
}

/// An invite as the person invited sees it, naming the project they can't otherwise see.
#[derive(Serialize)]
pub struct IncomingInvite {
    id: InviteId,
    project_name: String,
    role: ProjectRole,
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
}

#[derive(Serialize)]
pub struct ProjectList {
    created: Vec<ProjectView>,
    joined: Vec<ProjectView>,
    invites: Vec<IncomingInvite>,
}

pub async fn list_projects(
    State(state): State<ConsoleState>,
    Extension(signed_in): Extension<SignedIn>,
) -> Result<Json<ProjectList>> {
    let user = &signed_in.user;
    let mut created = Vec::new();
    let mut joined = Vec::new();
    for member in state.metadata.list_user_memberships(user.user_id).await? {
        let Some(project) = state.metadata.get_project(member.project_id).await? else {
            continue;
        };
        let view = project_view(&state, project, member.role, user.user_id).await?;
        if view.created_by_me {
            created.push(view);
        } else {
            joined.push(view);
        }
    }
    created.sort_by_key(|p| std::cmp::Reverse(p.created_at));
    joined.sort_by_key(|p| std::cmp::Reverse(p.created_at));

    // An unverified email proves nothing, so it doesn't reveal any invites.
    let now = OffsetDateTime::now_utc();
    let mut invites = Vec::new();
    if user.email_verified
        && let Some(email) = user.email.as_deref()
    {
        for invite in state.metadata.list_invites().await? {
            if invite.expires_at > now && invite.email.eq_ignore_ascii_case(email) {
                let Some(project) = state.metadata.get_project(invite.project_id).await? else {
                    continue;
                };
                invites.push(IncomingInvite {
                    id: invite.invite_id,
                    project_name: project.name,
                    role: invite.role,
                    expires_at: invite.expires_at,
                });
            }
        }
    }

    Ok(Json(ProjectList {
        created,
        joined,
        invites,
    }))
}

#[derive(Deserialize)]
pub struct NameRequest {
    name: String,
}

pub async fn create_project(
    State(state): State<ConsoleState>,
    Extension(signed_in): Extension<SignedIn>,
    body: std::result::Result<Json<NameRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ProjectView>)> {
    let user_id = signed_in.user.user_id;
    let result = async {
        let name = clean_name(&parse_body(body)?.name, "A project name")?;
        Ok(state
            .metadata
            .create_project(CreateProject {
                name,
                created_by: user_id,
                max_created: MAX_PROJECTS_PER_USER,
            })
            .await?)
    }
    .await;
    let target = match &result {
        Ok(project) => format!("project:{}", project.project_id),
        Err(_) => "project:new".to_string(),
    };
    let project = audited(&state, &signed_in, "project.create", target, result).await?;
    Ok((
        StatusCode::CREATED,
        Json(project_view(&state, project, ProjectRole::Owner, user_id).await?),
    ))
}

pub async fn get_project(
    State(state): State<ConsoleState>,
    Extension(signed_in): Extension<SignedIn>,
    Path(project_id): Path<ProjectId>,
) -> Result<Json<ProjectView>> {
    let user_id = signed_in.user.user_id;
    let (project, member) = access(&state, user_id, project_id, Need::Read).await?;
    Ok(Json(
        project_view(&state, project, member.role, user_id).await?,
    ))
}

pub async fn rename_project(
    State(state): State<ConsoleState>,
    Extension(signed_in): Extension<SignedIn>,
    Path(project_id): Path<ProjectId>,
    body: std::result::Result<Json<NameRequest>, JsonRejection>,
) -> Result<Json<ProjectView>> {
    let user_id = signed_in.user.user_id;
    let result = async {
        access(&state, user_id, project_id, Need::Owner).await?;
        let name = clean_name(&parse_body(body)?.name, "A project name")?;
        Ok(state.metadata.rename_project(project_id, name).await?)
    }
    .await;
    let target = format!("project:{project_id}");
    let project = audited(&state, &signed_in, "project.rename", target, result).await?;
    Ok(Json(
        project_view(&state, project, ProjectRole::Owner, user_id).await?,
    ))
}

pub async fn delete_project(
    State(state): State<ConsoleState>,
    Extension(signed_in): Extension<SignedIn>,
    Path(project_id): Path<ProjectId>,
) -> Result<StatusCode> {
    let result = async {
        access(&state, signed_in.user.user_id, project_id, Need::Owner).await?;
        Ok(state.metadata.delete_project(project_id).await?)
    }
    .await;
    let target = format!("project:{project_id}");
    audited(&state, &signed_in, "project.delete", target, result).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub struct ProjectSession {
    access_key: String,
    secret_key: String,
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
}

/// The key the web console uses to work in this project. It ends when the sign-in does, or
/// sooner if this person loses access. Opening a project again returns the same key, so
/// reopening one neither piles up keys nor cuts off an upload using the current one.
pub async fn open_project_session(
    State(state): State<ConsoleState>,
    Extension(signed_in): Extension<SignedIn>,
    Path(project_id): Path<ProjectId>,
) -> Result<Json<ProjectSession>> {
    let user_id = signed_in.user.user_id;
    let (project, _) = access(&state, user_id, project_id, Need::Member).await?;

    let parent = state
        .metadata
        .get_credential(&signed_in.access_key)
        .await?
        .ok_or_else(not_found)?;
    let expires_at = parent.expires_at.ok_or_else(not_found)?;

    let now = OffsetDateTime::now_utc();
    let existing = state
        .metadata
        .list_credentials()
        .await?
        .into_iter()
        .find(|c| {
            c.kind == CredentialKind::ProjectSession
                && c.project_id == Some(project_id)
                && c.session_of.as_deref() == Some(signed_in.access_key.as_str())
                && !c.is_expired(now)
        });
    if let Some(existing) = existing {
        return Ok(Json(ProjectSession {
            access_key: existing.access_key,
            secret_key: existing.secret_key,
            expires_at,
        }));
    }

    let credential = Credential {
        access_key: format!("LS3T{}", random_hex(1)[..16].to_uppercase()),
        secret_key: random_hex(2),
        owner_id: project.owner_id,
        enabled: true,
        admin: false,
        kind: CredentialKind::ProjectSession,
        user_id: Some(user_id),
        expires_at: Some(expires_at),
        project_id: Some(project_id),
        name: None,
        session_of: Some(signed_in.access_key.clone()),
        last_used_at: None,
        created_at: now,
    };
    state
        .metadata
        .put_project_credential(credential.clone(), MAX_ACCESS_KEYS_PER_PROJECT)
        .await?;
    Ok(Json(ProjectSession {
        access_key: credential.access_key,
        secret_key: credential.secret_key,
        expires_at,
    }))
}

#[derive(Serialize)]
pub struct AccessKeyView {
    access_key: String,
    name: Option<String>,
    enabled: bool,
    created_by: Option<UserId>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    last_used_at: Option<OffsetDateTime>,
}

impl From<Credential> for AccessKeyView {
    fn from(key: Credential) -> Self {
        Self {
            access_key: key.access_key,
            name: key.name,
            enabled: key.enabled,
            created_by: key.user_id,
            created_at: key.created_at,
            last_used_at: key.last_used_at,
        }
    }
}

pub async fn list_access_keys(
    State(state): State<ConsoleState>,
    Extension(signed_in): Extension<SignedIn>,
    Path(project_id): Path<ProjectId>,
) -> Result<Json<Vec<AccessKeyView>>> {
    access(&state, signed_in.user.user_id, project_id, Need::Read).await?;
    let mut keys: Vec<Credential> = state.metadata.list_credentials().await?;
    keys.retain(|c| c.kind == CredentialKind::ProjectAccessKey && c.project_id == Some(project_id));
    keys.sort_by_key(|c| std::cmp::Reverse(c.created_at));
    Ok(Json(keys.into_iter().map(Into::into).collect()))
}

#[derive(Serialize)]
pub struct CreatedAccessKey {
    #[serde(flatten)]
    key: AccessKeyView,
    /// Only ever returned here, when the key is created.
    secret_key: String,
}

pub async fn create_access_key(
    State(state): State<ConsoleState>,
    Extension(signed_in): Extension<SignedIn>,
    Path(project_id): Path<ProjectId>,
    body: std::result::Result<Json<NameRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<CreatedAccessKey>)> {
    let access_key = format!("LS3P{}", random_hex(1)[..16].to_uppercase());
    let result = async {
        let user_id = signed_in.user.user_id;
        let (project, _) = access(&state, user_id, project_id, Need::Member).await?;
        let name = clean_name(&parse_body(body)?.name, "A key name")?;
        let credential = Credential {
            access_key: access_key.clone(),
            secret_key: random_hex(2),
            owner_id: project.owner_id,
            enabled: true,
            admin: false,
            kind: CredentialKind::ProjectAccessKey,
            user_id: Some(user_id),
            expires_at: None,
            project_id: Some(project_id),
            name: Some(name),
            session_of: None,
            last_used_at: None,
            created_at: OffsetDateTime::now_utc(),
        };
        state
            .metadata
            .put_project_credential(credential.clone(), MAX_ACCESS_KEYS_PER_PROJECT)
            .await?;
        Ok(credential)
    }
    .await;
    let target = format!("project:{project_id}/key:{access_key}");
    let credential = audited(&state, &signed_in, "key.create", target, result).await?;
    let secret_key = credential.secret_key.clone();
    Ok((
        StatusCode::CREATED,
        Json(CreatedAccessKey {
            key: credential.into(),
            secret_key,
        }),
    ))
}

async fn project_key(
    state: &ConsoleState,
    project_id: ProjectId,
    access_key: &str,
) -> Result<Credential> {
    state
        .metadata
        .get_credential(access_key)
        .await?
        .filter(|c| c.kind == CredentialKind::ProjectAccessKey && c.project_id == Some(project_id))
        .ok_or_else(not_found)
}

#[derive(Deserialize)]
pub struct UpdateKeyRequest {
    enabled: bool,
}

pub async fn update_access_key(
    State(state): State<ConsoleState>,
    Extension(signed_in): Extension<SignedIn>,
    Path((project_id, access_key)): Path<(ProjectId, String)>,
    body: std::result::Result<Json<UpdateKeyRequest>, JsonRejection>,
) -> Result<Json<AccessKeyView>> {
    let result = async {
        access(&state, signed_in.user.user_id, project_id, Need::Member).await?;
        project_key(&state, project_id, &access_key).await?;
        let enabled = parse_body(body)?.enabled;
        state
            .metadata
            .set_credential_enabled(&access_key, enabled)
            .await?;
        project_key(&state, project_id, &access_key).await
    }
    .await;
    let action = match &result {
        Ok(key) if key.enabled => "key.enable",
        Ok(_) => "key.disable",
        Err(_) => "key.update",
    };
    let target = format!("project:{project_id}/key:{access_key}");
    let key = audited(&state, &signed_in, action, target, result).await?;
    Ok(Json(key.into()))
}

pub async fn delete_access_key(
    State(state): State<ConsoleState>,
    Extension(signed_in): Extension<SignedIn>,
    Path((project_id, access_key)): Path<(ProjectId, String)>,
) -> Result<StatusCode> {
    let result = async {
        access(&state, signed_in.user.user_id, project_id, Need::Member).await?;
        project_key(&state, project_id, &access_key).await?;
        Ok(state.metadata.delete_credential(&access_key).await?)
    }
    .await;
    let target = format!("project:{project_id}/key:{access_key}");
    audited(&state, &signed_in, "key.delete", target, result).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub struct MemberView {
    user_id: UserId,
    email: Option<String>,
    name: Option<String>,
    role: ProjectRole,
    #[serde(with = "time::serde::rfc3339")]
    added_at: OffsetDateTime,
}

pub async fn list_members(
    State(state): State<ConsoleState>,
    Extension(signed_in): Extension<SignedIn>,
    Path(project_id): Path<ProjectId>,
) -> Result<Json<Vec<MemberView>>> {
    access(&state, signed_in.user.user_id, project_id, Need::Read).await?;
    let mut members = Vec::new();
    for member in state.metadata.list_members(project_id).await? {
        let user = state.metadata.get_user(member.user_id).await?;
        members.push(MemberView {
            user_id: member.user_id,
            email: user.as_ref().and_then(|u| u.email.clone()),
            name: user.and_then(|u| u.name),
            role: member.role,
            added_at: member.added_at,
        });
    }
    members.sort_by_key(|m| m.added_at);
    Ok(Json(members))
}

#[derive(Deserialize)]
pub struct UpdateMemberRequest {
    role: ProjectRole,
}

pub async fn update_member(
    State(state): State<ConsoleState>,
    Extension(signed_in): Extension<SignedIn>,
    Path((project_id, user_id)): Path<(ProjectId, UserId)>,
    body: std::result::Result<Json<UpdateMemberRequest>, JsonRejection>,
) -> Result<Json<MemberView>> {
    let result = async {
        access(&state, signed_in.user.user_id, project_id, Need::Owner).await?;
        let role = parse_body(body)?.role;
        Ok(state
            .metadata
            .set_member_role(project_id, user_id, role)
            .await?)
    }
    .await;
    let target = format!("project:{project_id}/member:{user_id}");
    let member = audited(&state, &signed_in, "member.update", target, result).await?;
    let user = state.metadata.get_user(user_id).await?;
    Ok(Json(MemberView {
        user_id,
        email: user.as_ref().and_then(|u| u.email.clone()),
        name: user.and_then(|u| u.name),
        role: member.role,
        added_at: member.added_at,
    }))
}

pub async fn remove_member(
    State(state): State<ConsoleState>,
    Extension(signed_in): Extension<SignedIn>,
    Path((project_id, user_id)): Path<(ProjectId, UserId)>,
) -> Result<StatusCode> {
    let result = async {
        access(&state, signed_in.user.user_id, project_id, Need::Owner).await?;
        Ok(state.metadata.remove_member(project_id, user_id).await?)
    }
    .await;
    let target = format!("project:{project_id}/member:{user_id}");
    audited(&state, &signed_in, "member.remove", target, result).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn leave_project(
    State(state): State<ConsoleState>,
    Extension(signed_in): Extension<SignedIn>,
    Path(project_id): Path<ProjectId>,
) -> Result<StatusCode> {
    let user_id = signed_in.user.user_id;
    let result = async {
        access(&state, user_id, project_id, Need::Read).await?;
        Ok(state.metadata.remove_member(project_id, user_id).await?)
    }
    .await;
    let target = format!("project:{project_id}/member:{user_id}");
    audited(&state, &signed_in, "member.leave", target, result).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_invites(
    State(state): State<ConsoleState>,
    Extension(signed_in): Extension<SignedIn>,
    Path(project_id): Path<ProjectId>,
) -> Result<Json<Vec<InviteView>>> {
    access(&state, signed_in.user.user_id, project_id, Need::Owner).await?;
    let now = OffsetDateTime::now_utc();
    let mut invites = state.metadata.list_project_invites(project_id).await?;
    invites.retain(|invite| invite.expires_at > now);
    invites.sort_by_key(|invite| invite.created_at);
    Ok(Json(invites.into_iter().map(Into::into).collect()))
}

#[derive(Deserialize)]
pub struct CreateInviteRequest {
    email: String,
    role: ProjectRole,
}

pub async fn create_invite(
    State(state): State<ConsoleState>,
    Extension(signed_in): Extension<SignedIn>,
    Path(project_id): Path<ProjectId>,
    body: std::result::Result<Json<CreateInviteRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<InviteView>)> {
    let user_id = signed_in.user.user_id;
    let result = async {
        access(&state, user_id, project_id, Need::Owner).await?;
        let request = parse_body(body)?;
        Ok(state
            .metadata
            .create_invite(CreateInvite {
                project_id,
                email: clean_email(&request.email)?,
                role: request.role,
                invited_by: user_id,
                expires_at: OffsetDateTime::now_utc() + INVITE_LIFETIME,
            })
            .await?)
    }
    .await;
    let target = match &result {
        Ok(invite) => format!("project:{project_id}/invite:{}", invite.invite_id),
        Err(_) => format!("project:{project_id}/invite:new"),
    };
    let invite = audited(&state, &signed_in, "invite.create", target, result).await?;
    Ok((StatusCode::CREATED, Json(invite.into())))
}

pub async fn cancel_invite(
    State(state): State<ConsoleState>,
    Extension(signed_in): Extension<SignedIn>,
    Path((project_id, invite_id)): Path<(ProjectId, InviteId)>,
) -> Result<StatusCode> {
    let result = async {
        access(&state, signed_in.user.user_id, project_id, Need::Owner).await?;
        state
            .metadata
            .get_invite(invite_id)
            .await?
            .filter(|invite| invite.project_id == project_id)
            .ok_or_else(not_found)?;
        Ok(state.metadata.delete_invite(invite_id).await?)
    }
    .await;
    let target = format!("project:{project_id}/invite:{invite_id}");
    audited(&state, &signed_in, "invite.cancel", target, result).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn accept_invite(
    State(state): State<ConsoleState>,
    Extension(signed_in): Extension<SignedIn>,
    Path(invite_id): Path<InviteId>,
) -> Result<Json<ProjectView>> {
    let user_id = signed_in.user.user_id;
    let result = async {
        let member = state.metadata.accept_invite(invite_id, user_id).await?;
        let project = state
            .metadata
            .get_project(member.project_id)
            .await?
            .ok_or_else(not_found)?;
        project_view(&state, project, member.role, user_id).await
    }
    .await;
    let target = format!("invite:{invite_id}");
    let view = audited(&state, &signed_in, "invite.accept", target, result).await?;
    Ok(Json(view))
}

pub async fn decline_invite(
    State(state): State<ConsoleState>,
    Extension(signed_in): Extension<SignedIn>,
    Path(invite_id): Path<InviteId>,
) -> Result<StatusCode> {
    let result = state
        .metadata
        .decline_invite(invite_id, signed_in.user.user_id)
        .await
        .map_err(Into::into);
    let target = format!("invite:{invite_id}");
    audited(&state, &signed_in, "invite.decline", target, result).await?;
    Ok(StatusCode::NO_CONTENT)
}
