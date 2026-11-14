//! Paying for projects: credit, gift codes, checkouts and subscriptions, as signed-in users
//! see them, and the background task that renews and moves subscriptions along.
//!
//! Payments only ever add credit, and subscriptions are only ever paid from credit. A
//! payment provider is therefore just a way to add credit: swapping the test provider for a
//! real one changes nothing about subscriptions, credit or storage limits.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{ConnectInfo, Extension, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use loony_admin::{AuditEntry, AuditLog};
use loony_core::{CheckoutId, ProjectId};
use loony_metadata::billing::{
    BillingPeriod, BillingTimings, Checkout, CheckoutStatus, LedgerEntry, LedgerReason,
    NewCheckout, Plan, Step, Subscription, SubscriptionState, gift_code_hash, next_step,
    normalize_gift_code,
};
use loony_metadata::{MetaError, MetadataStore, ProjectRole};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::ConsoleState;
use crate::auth::SignedIn;
use crate::error::ConsoleError;

type Result<T> = std::result::Result<T, ConsoleError>;

/// Smallest and largest amount one checkout can add: ₹1 to ₹1,00,000.
const MIN_CHECKOUT_PAISE: u64 = 100;
const MAX_CHECKOUT_PAISE: u64 = 10_000_000;
const LEDGER_PAGE: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BillingMode {
    /// No checkout. Credit only comes from gift codes.
    Off,
    /// The test checkout, which never charges anything.
    Test,
}

#[derive(Debug, Clone)]
pub struct BillingConfig {
    pub plans: Vec<Plan>,
    pub mode: BillingMode,
    pub timings: BillingTimings,
    /// How often the background task looks for renewals and state changes.
    pub tick: Duration,
}

/// ₹1,000 a month, including GST, for 15 GB.
pub fn default_plans() -> Vec<Plan> {
    vec![Plan {
        id: "standard".into(),
        name: "Standard".into(),
        price_paise: 100_000,
        storage_bytes: 15 * 1024 * 1024 * 1024,
        period: BillingPeriod::Months(1),
    }]
}

impl Default for BillingConfig {
    fn default() -> Self {
        Self {
            plans: default_plans(),
            mode: BillingMode::Off,
            timings: BillingTimings::default(),
            tick: Duration::from_secs(60),
        }
    }
}

impl BillingConfig {
    fn plan(&self, id: &str) -> Option<&Plan> {
        self.plans.iter().find(|plan| plan.id == id)
    }
}

// ---------------------------------------------------------------------------
// Payment providers
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum PaymentError {
    /// The request can't be paid this way, such as a webhook this provider doesn't use.
    Unsupported,
    Invalid(String),
    Unavailable(String),
}

/// What a provider needs to start taking a payment.
pub struct CheckoutRequest<'a> {
    pub checkout_id: CheckoutId,
    pub amount_paise: u64,
    pub description: &'a str,
}

/// What the browser does next. A real provider would usually send it to its own page.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CheckoutAction {
    /// Shown on this site's own checkout page.
    ConfirmHere,
    Redirect {
        url: String,
    },
}

pub struct StartedCheckout {
    pub provider_ref: Option<String>,
    pub action: CheckoutAction,
}

pub struct PaymentOutcome {
    pub succeeded: bool,
    pub provider_ref: Option<String>,
}

/// A payment reported by the provider itself, out of band.
pub struct WebhookEvent {
    pub checkout_id: CheckoutId,
    pub outcome: PaymentOutcome,
}

/// Everything that varies between ways of taking money. The rest of billing only sees a
/// checkout succeed or fail.
#[async_trait]
pub trait PaymentProvider: Send + Sync {
    fn name(&self) -> &'static str;

    async fn start_checkout(
        &self,
        request: CheckoutRequest<'_>,
    ) -> std::result::Result<StartedCheckout, PaymentError>;

    /// Finishes a payment the browser was part of. What `details` holds is up to the
    /// provider, such as a signed confirmation from its own page.
    async fn confirm(
        &self,
        checkout: &Checkout,
        details: &serde_json::Value,
    ) -> std::result::Result<PaymentOutcome, PaymentError>;

    /// Reads a notification the provider sent, after checking it really came from them.
    async fn handle_webhook(
        &self,
        headers: &HeaderMap,
        body: &[u8],
    ) -> std::result::Result<WebhookEvent, PaymentError>;
}

/// Succeeds or fails on request and never takes money. Only offered with
/// `LS3_BILLING_MODE=test`.
pub struct TestProvider;

#[async_trait]
impl PaymentProvider for TestProvider {
    fn name(&self) -> &'static str {
        "test"
    }

    async fn start_checkout(
        &self,
        request: CheckoutRequest<'_>,
    ) -> std::result::Result<StartedCheckout, PaymentError> {
        Ok(StartedCheckout {
            provider_ref: Some(format!("test_{}", request.checkout_id.as_uuid().simple())),
            action: CheckoutAction::ConfirmHere,
        })
    }

    async fn confirm(
        &self,
        checkout: &Checkout,
        details: &serde_json::Value,
    ) -> std::result::Result<PaymentOutcome, PaymentError> {
        let succeeded = match details.get("outcome").and_then(|o| o.as_str()) {
            Some("succeed") => true,
            Some("fail") => false,
            _ => {
                return Err(PaymentError::Invalid(
                    "outcome must be succeed or fail".into(),
                ));
            }
        };
        Ok(PaymentOutcome {
            succeeded,
            provider_ref: checkout.provider_ref.clone(),
        })
    }

    async fn handle_webhook(
        &self,
        _headers: &HeaderMap,
        _body: &[u8],
    ) -> std::result::Result<WebhookEvent, PaymentError> {
        Err(PaymentError::Unsupported)
    }
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct PlanView {
    id: String,
    name: String,
    price_paise: u64,
    storage_bytes: u64,
    period: BillingPeriod,
}

impl From<&Plan> for PlanView {
    fn from(plan: &Plan) -> Self {
        Self {
            id: plan.id.clone(),
            name: plan.name.clone(),
            price_paise: plan.price_paise,
            storage_bytes: plan.storage_bytes,
            period: plan.period,
        }
    }
}

#[derive(Serialize)]
pub struct UsageView {
    bytes: u64,
    objects: u64,
    pending_bytes: u64,
    limit_bytes: Option<u64>,
}

#[derive(Serialize)]
pub struct SubscriptionView {
    state: SubscriptionState,
    plan: Option<PlanView>,
    #[serde(with = "time::serde::rfc3339::option")]
    period_start: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    period_end: Option<OffsetDateTime>,
    cancel_at_period_end: bool,
    /// When grace ends, during grace.
    #[serde(with = "time::serde::rfc3339::option")]
    read_only_at: Option<OffsetDateTime>,
    /// When a read-only project is suspended.
    #[serde(with = "time::serde::rfc3339::option")]
    suspends_at: Option<OffsetDateTime>,
    payer_is_me: bool,
    payer_email: Option<String>,
    usage: UsageView,
}

// `limit_bytes` is the plan's storage, shown whatever the state. Whether anything can be
// stored at all depends on `state`.

pub(crate) async fn subscription_view(
    state: &ConsoleState,
    project_id: ProjectId,
    owner: loony_core::OwnerId,
    me: loony_core::UserId,
) -> Result<SubscriptionView> {
    let sub = state.metadata.get_subscription(project_id).await?;
    let usage = state.metadata.get_usage(owner).await?;
    let timings = state.billing.timings;
    let after = |since: OffsetDateTime, secs: u64| since + time::Duration::seconds(secs as i64);
    let payer_email = match &sub {
        Some(sub) => state
            .metadata
            .get_user(sub.payer)
            .await?
            .and_then(|u| u.email),
        None => None,
    };
    Ok(SubscriptionView {
        state: sub.as_ref().map_or(SubscriptionState::Unpaid, |s| s.state),
        plan: sub.as_ref().map(|s| (&s.plan).into()),
        period_start: sub.as_ref().map(|s| s.period_start),
        period_end: sub.as_ref().map(|s| s.period_end),
        cancel_at_period_end: sub.as_ref().is_some_and(|s| s.cancel_at_period_end),
        read_only_at: sub
            .as_ref()
            .filter(|s| s.state == SubscriptionState::Grace)
            .map(|s| after(s.state_since, timings.grace_secs)),
        suspends_at: sub
            .as_ref()
            .filter(|s| s.state == SubscriptionState::ReadOnly)
            .map(|s| after(s.state_since, timings.suspend_after_secs)),
        payer_is_me: sub.as_ref().is_some_and(|s| s.payer == me),
        payer_email,
        usage: UsageView {
            bytes: usage.bytes,
            objects: usage.objects,
            pending_bytes: usage.pending_bytes,
            limit_bytes: sub.as_ref().map(|s| s.plan.storage_bytes),
        },
    })
}

#[derive(Serialize)]
pub struct LedgerView {
    id: String,
    amount_paise: i64,
    balance_after_paise: u64,
    description: String,
    project_id: Option<ProjectId>,
    #[serde(with = "time::serde::rfc3339")]
    at: OffsetDateTime,
}

fn describe(
    entry: &LedgerEntry,
    project_names: &std::collections::HashMap<ProjectId, String>,
) -> (String, Option<ProjectId>) {
    match &entry.reason {
        LedgerReason::GiftCode { hint, .. } => (format!("Gift code …{hint}"), None),
        LedgerReason::Payment { provider, .. } if provider == "test" => {
            ("Test payment".into(), None)
        }
        LedgerReason::Payment { .. } => ("Payment".into(), None),
        LedgerReason::Subscription {
            project_id,
            automatic,
            ..
        } => {
            let name = project_names
                .get(project_id)
                .cloned()
                .unwrap_or_else(|| "a deleted project".into());
            let what = if *automatic {
                "Renewal"
            } else {
                "Subscription"
            };
            (format!("{what}: {name}"), Some(*project_id))
        }
    }
}

#[derive(Serialize)]
pub struct CheckoutView {
    id: CheckoutId,
    amount_paise: u64,
    status: CheckoutStatus,
    provider: String,
    project_id: Option<ProjectId>,
    plan_id: Option<String>,
    action: Option<CheckoutAction>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

fn checkout_view(checkout: Checkout, action: Option<CheckoutAction>) -> CheckoutView {
    CheckoutView {
        id: checkout.checkout_id,
        amount_paise: checkout.amount_paise,
        status: checkout.status,
        provider: checkout.provider,
        project_id: checkout.project_id,
        plan_id: checkout.plan_id,
        action,
        created_at: checkout.created_at,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn audit(
    state: &ConsoleState,
    actor: String,
    action: &str,
    target: String,
    outcome: std::result::Result<(), &ConsoleError>,
) {
    let (outcome, detail) = match outcome {
        Ok(()) => ("ok", None),
        Err(err) if err.status.is_client_error() => ("denied", Some(err.message.clone())),
        Err(err) => ("failed", Some(err.message.clone())),
    };
    state
        .audit
        .record(AuditEntry {
            at: OffsetDateTime::now_utc(),
            actor,
            action: action.to_string(),
            target,
            outcome: outcome.to_string(),
            detail,
        })
        .await;
}

async fn audited<T>(
    state: &ConsoleState,
    signed_in: &SignedIn,
    action: &str,
    target: String,
    result: Result<T>,
) -> Result<T> {
    audit(
        state,
        format!("user:{}", signed_in.user.user_id),
        action,
        target,
        result.as_ref().map(|_| ()),
    )
    .await;
    result
}

fn parse<T>(body: std::result::Result<Json<T>, JsonRejection>) -> Result<T> {
    body.map(|Json(value)| value)
        .map_err(|_| ConsoleError::invalid_request("The request is malformed."))
}

fn not_found() -> ConsoleError {
    ConsoleError::new(
        StatusCode::NOT_FOUND,
        "not_found",
        "That doesn't exist, or you don't have access to it.",
    )
}

/// Any member may look; only an Owner may pay or cancel.
async fn project_for(
    state: &ConsoleState,
    signed_in: &SignedIn,
    project_id: ProjectId,
    owner_only: bool,
) -> Result<loony_metadata::Project> {
    let member = state
        .metadata
        .get_member(project_id, signed_in.user.user_id)
        .await?
        .ok_or_else(not_found)?;
    let project = state
        .metadata
        .get_project(project_id)
        .await?
        .ok_or_else(not_found)?;
    if owner_only && member.role != ProjectRole::Owner {
        return Err(ConsoleError::new(
            StatusCode::FORBIDDEN,
            "owner_only",
            "Only a project Owner can do that.",
        ));
    }
    Ok(project)
}

pub(crate) fn rupees(paise: u64) -> String {
    let rupees = paise / 100;
    let paise = paise % 100;
    // Indian grouping: the last three digits, then pairs.
    let digits = rupees.to_string();
    let grouped = if digits.len() <= 3 {
        digits
    } else {
        let (head, tail) = digits.split_at(digits.len() - 3);
        let mut parts: Vec<String> = Vec::new();
        let mut head = head.to_string();
        while head.len() > 2 {
            parts.insert(0, head.split_off(head.len() - 2));
        }
        if !head.is_empty() {
            parts.insert(0, head);
        }
        format!("{},{tail}", parts.join(","))
    };
    if paise == 0 {
        format!("₹{grouped}")
    } else {
        format!("₹{grouped}.{paise:02}")
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct BillingOverview {
    balance_paise: u64,
    plans: Vec<PlanView>,
    checkout: Option<&'static str>,
}

pub async fn overview(
    State(state): State<ConsoleState>,
    Extension(signed_in): Extension<SignedIn>,
) -> Result<Json<BillingOverview>> {
    Ok(Json(BillingOverview {
        balance_paise: state.metadata.get_balance(signed_in.user.user_id).await?,
        plans: state.billing.plans.iter().map(Into::into).collect(),
        checkout: state.payments.as_ref().map(|p| p.name()),
    }))
}

pub async fn ledger(
    State(state): State<ConsoleState>,
    Extension(signed_in): Extension<SignedIn>,
) -> Result<Json<Vec<LedgerView>>> {
    let entries = state
        .metadata
        .list_ledger(signed_in.user.user_id, LEDGER_PAGE)
        .await?;
    let mut names = std::collections::HashMap::new();
    for entry in &entries {
        if let LedgerReason::Subscription { project_id, .. } = &entry.reason
            && !names.contains_key(project_id)
            && let Some(project) = state.metadata.get_project(*project_id).await?
        {
            names.insert(*project_id, project.name);
        }
    }
    Ok(Json(
        entries
            .iter()
            .map(|entry| {
                let (description, project_id) = describe(entry, &names);
                LedgerView {
                    id: entry.entry_id.to_string(),
                    amount_paise: entry.amount_paise,
                    balance_after_paise: entry.balance_after_paise,
                    description,
                    project_id,
                    at: entry.at,
                }
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
pub struct RedeemRequest {
    code: String,
}

#[derive(Serialize)]
pub struct Redeemed {
    amount_paise: u64,
    balance_paise: u64,
}

pub async fn redeem(
    State(state): State<ConsoleState>,
    Extension(signed_in): Extension<SignedIn>,
    peer: Option<ConnectInfo<std::net::SocketAddr>>,
    headers: HeaderMap,
    body: std::result::Result<Json<RedeemRequest>, JsonRejection>,
) -> Result<Response> {
    let user_key = format!("user:{}", signed_in.user.user_id);
    let ip_key = format!("ip:{}", crate::handlers::client_key(&state, &headers, peer));
    if !state.redeem_limiter.allow(&user_key) || !state.redeem_limiter.allow(&ip_key) {
        let error = ConsoleError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "too_many_requests",
            "Too many gift code attempts. Wait a minute and try again.",
        );
        audit(
            &state,
            user_key,
            "gift_code.redeem",
            "gift_code:unknown".into(),
            Err(&error),
        )
        .await;
        return Ok(([(header::RETRY_AFTER, "60")], error).into_response());
    }

    let mut target = "gift_code:unknown".to_string();
    let result = async {
        let code = parse(body)?.code;
        // A malformed code is treated like a wrong one, without telling which.
        let normalized = normalize_gift_code(&code).ok_or(MetaError::NoSuchGiftCode)?;
        let (gift, entry) = state
            .metadata
            .redeem_gift_code(gift_code_hash(&normalized), signed_in.user.user_id)
            .await?;
        target = format!("gift_code:{}", gift.code_id);
        Ok(Redeemed {
            amount_paise: gift.amount_paise,
            balance_paise: entry.balance_after_paise,
        })
    }
    .await;
    let redeemed = audited(&state, &signed_in, "gift_code.redeem", target, result).await?;
    Ok(Json(redeemed).into_response())
}

#[derive(Deserialize)]
pub struct NewCheckoutRequest {
    amount_paise: u64,
    project_id: Option<ProjectId>,
    plan_id: Option<String>,
}

fn payments(state: &ConsoleState) -> Result<Arc<dyn PaymentProvider>> {
    state.payments.clone().ok_or_else(|| {
        ConsoleError::new(
            StatusCode::NOT_FOUND,
            "checkout_unavailable",
            "Paying by card or UPI isn't available yet. Use a gift code.",
        )
    })
}

pub async fn create_checkout(
    State(state): State<ConsoleState>,
    Extension(signed_in): Extension<SignedIn>,
    body: std::result::Result<Json<NewCheckoutRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<CheckoutView>)> {
    let result = async {
        let provider = payments(&state)?;
        let request = parse(body)?;
        if !(MIN_CHECKOUT_PAISE..=MAX_CHECKOUT_PAISE).contains(&request.amount_paise) {
            return Err(ConsoleError::invalid_request(format!(
                "An amount must be between {} and {}.",
                rupees(MIN_CHECKOUT_PAISE),
                rupees(MAX_CHECKOUT_PAISE)
            )));
        }
        if let Some(project_id) = request.project_id {
            project_for(&state, &signed_in, project_id, true).await?;
        }
        if let Some(plan_id) = &request.plan_id
            && state.billing.plan(plan_id).is_none()
        {
            return Err(ConsoleError::invalid_request("That plan doesn't exist."));
        }
        let checkout = state
            .metadata
            .create_checkout(NewCheckout {
                user_id: signed_in.user.user_id,
                amount_paise: request.amount_paise,
                project_id: request.project_id,
                plan_id: request.plan_id,
                provider: provider.name().into(),
                provider_ref: None,
            })
            .await?;
        let started = provider
            .start_checkout(CheckoutRequest {
                checkout_id: checkout.checkout_id,
                amount_paise: checkout.amount_paise,
                description: "LS3 credit",
            })
            .await
            .map_err(payment_error)?;
        let mut checkout = checkout;
        checkout.provider_ref = started.provider_ref;
        Ok(checkout_view(checkout, Some(started.action)))
    }
    .await;
    let target = match &result {
        Ok(view) => format!("checkout:{}", view.id),
        Err(_) => "checkout:new".into(),
    };
    Ok((
        StatusCode::CREATED,
        Json(audited(&state, &signed_in, "checkout.create", target, result).await?),
    ))
}

fn payment_error(err: PaymentError) -> ConsoleError {
    match err {
        PaymentError::Unsupported => not_found(),
        PaymentError::Invalid(message) => ConsoleError::invalid_request(message),
        PaymentError::Unavailable(reason) => {
            tracing::warn!(%reason, "payment provider unavailable");
            ConsoleError::new(
                StatusCode::BAD_GATEWAY,
                "payment_unavailable",
                "Payments are unavailable right now. Try again in a moment.",
            )
        }
    }
}

async fn own_checkout(
    state: &ConsoleState,
    signed_in: &SignedIn,
    checkout_id: CheckoutId,
) -> Result<Checkout> {
    state
        .metadata
        .get_checkout(checkout_id)
        .await?
        .filter(|c| c.user_id == signed_in.user.user_id)
        .ok_or_else(not_found)
}

pub async fn get_checkout(
    State(state): State<ConsoleState>,
    Extension(signed_in): Extension<SignedIn>,
    Path(checkout_id): Path<CheckoutId>,
) -> Result<Json<CheckoutView>> {
    let checkout = own_checkout(&state, &signed_in, checkout_id).await?;
    let action =
        (checkout.status == CheckoutStatus::Pending).then_some(CheckoutAction::ConfirmHere);
    Ok(Json(checkout_view(checkout, action)))
}

#[derive(Serialize)]
pub struct ConfirmedCheckout {
    checkout: CheckoutView,
    balance_paise: u64,
    /// When the checkout was to pay for a project, whether that went through.
    subscription: Option<SubscriptionView>,
    subscription_error: Option<String>,
}

pub async fn confirm_checkout(
    State(state): State<ConsoleState>,
    Extension(signed_in): Extension<SignedIn>,
    Path(checkout_id): Path<CheckoutId>,
    body: std::result::Result<Json<serde_json::Value>, JsonRejection>,
) -> Result<Json<ConfirmedCheckout>> {
    let result = async {
        let provider = payments(&state)?;
        let details = parse(body)?;
        let checkout = own_checkout(&state, &signed_in, checkout_id).await?;
        if checkout.provider != provider.name() {
            return Err(not_found());
        }
        let outcome = provider
            .confirm(&checkout, &details)
            .await
            .map_err(payment_error)?;
        let checkout = state
            .metadata
            .complete_checkout(checkout_id, outcome.succeeded, outcome.provider_ref)
            .await?;
        Ok(checkout)
    }
    .await;
    let action = match &result {
        Ok(c) if c.status == CheckoutStatus::Succeeded => "checkout.succeeded",
        Ok(_) => "checkout.failed",
        Err(_) => "checkout.confirm",
    };
    let checkout = audited(
        &state,
        &signed_in,
        action,
        format!("checkout:{checkout_id}"),
        result,
    )
    .await?;

    // The payment added credit. If it was for a project, use it for that now.
    let mut subscription = None;
    let mut subscription_error = None;
    if checkout.status == CheckoutStatus::Succeeded
        && let Some(project_id) = checkout.project_id
    {
        match subscribe(&state, &signed_in, project_id, checkout.plan_id.clone()).await {
            Ok(view) => subscription = Some(view),
            Err(err) => subscription_error = Some(err.message),
        }
    }
    Ok(Json(ConfirmedCheckout {
        balance_paise: state.metadata.get_balance(signed_in.user.user_id).await?,
        checkout: checkout_view(checkout, None),
        subscription,
        subscription_error,
    }))
}

/// Unsigned, as providers call it. The provider checks the payload is genuinely theirs.
pub async fn webhook(
    State(state): State<ConsoleState>,
    Path(provider_name): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<StatusCode> {
    let provider = state
        .payments
        .clone()
        .filter(|p| p.name() == provider_name)
        .ok_or_else(not_found)?;
    let event = provider
        .handle_webhook(&headers, &body)
        .await
        .map_err(payment_error)?;
    let checkout = state
        .metadata
        .complete_checkout(
            event.checkout_id,
            event.outcome.succeeded,
            event.outcome.provider_ref,
        )
        .await?;
    audit(
        &state,
        format!("provider:{provider_name}"),
        "checkout.webhook",
        format!("checkout:{}", checkout.checkout_id),
        Ok(()),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn get_subscription(
    State(state): State<ConsoleState>,
    Extension(signed_in): Extension<SignedIn>,
    Path(project_id): Path<ProjectId>,
) -> Result<Json<SubscriptionView>> {
    let project = project_for(&state, &signed_in, project_id, false).await?;
    Ok(Json(
        subscription_view(&state, project_id, project.owner_id, signed_in.user.user_id).await?,
    ))
}

#[derive(Deserialize)]
pub struct SubscribeRequest {
    plan_id: Option<String>,
}

/// Pays for the project from the caller's credit: a first subscription, or bringing one
/// back. With no plan given, a lapsed project keeps its own plan.
async fn subscribe(
    state: &ConsoleState,
    signed_in: &SignedIn,
    project_id: ProjectId,
    plan_id: Option<String>,
) -> Result<SubscriptionView> {
    let project = project_for(state, signed_in, project_id, true).await?;
    let current = state.metadata.get_subscription(project_id).await?;
    let plan =
        match (plan_id, &current) {
            (Some(id), _) => state
                .billing
                .plan(&id)
                .cloned()
                .ok_or_else(|| ConsoleError::invalid_request("That plan doesn't exist."))?,
            (None, Some(sub)) => sub.plan.clone(),
            (None, None) => state.billing.plans.first().cloned().ok_or_else(|| {
                ConsoleError::invalid_request("There are no plans to subscribe to.")
            })?,
        };
    state
        .metadata
        .pay_for_project(project_id, signed_in.user.user_id, plan)
        .await?;
    subscription_view(state, project_id, project.owner_id, signed_in.user.user_id).await
}

pub async fn create_subscription(
    State(state): State<ConsoleState>,
    Extension(signed_in): Extension<SignedIn>,
    Path(project_id): Path<ProjectId>,
    body: std::result::Result<Json<SubscribeRequest>, JsonRejection>,
) -> Result<Json<SubscriptionView>> {
    let result =
        async { subscribe(&state, &signed_in, project_id, parse(body)?.plan_id).await }.await;
    Ok(Json(
        audited(
            &state,
            &signed_in,
            "subscription.subscribe",
            format!("project:{project_id}"),
            result,
        )
        .await?,
    ))
}

pub async fn renew_subscription(
    State(state): State<ConsoleState>,
    Extension(signed_in): Extension<SignedIn>,
    Path(project_id): Path<ProjectId>,
) -> Result<Json<SubscriptionView>> {
    let result = subscribe(&state, &signed_in, project_id, None).await;
    Ok(Json(
        audited(
            &state,
            &signed_in,
            "subscription.renew_now",
            format!("project:{project_id}"),
            result,
        )
        .await?,
    ))
}

async fn set_cancelled(
    state: &ConsoleState,
    signed_in: &SignedIn,
    project_id: ProjectId,
    cancel: bool,
) -> Result<SubscriptionView> {
    let project = project_for(state, signed_in, project_id, true).await?;
    state
        .metadata
        .set_subscription_cancelled(project_id, signed_in.user.user_id, cancel)
        .await?;
    subscription_view(state, project_id, project.owner_id, signed_in.user.user_id).await
}

pub async fn cancel_subscription(
    State(state): State<ConsoleState>,
    Extension(signed_in): Extension<SignedIn>,
    Path(project_id): Path<ProjectId>,
) -> Result<Json<SubscriptionView>> {
    let result = set_cancelled(&state, &signed_in, project_id, true).await;
    Ok(Json(
        audited(
            &state,
            &signed_in,
            "subscription.cancel",
            format!("project:{project_id}"),
            result,
        )
        .await?,
    ))
}

pub async fn resume_subscription(
    State(state): State<ConsoleState>,
    Extension(signed_in): Extension<SignedIn>,
    Path(project_id): Path<ProjectId>,
) -> Result<Json<SubscriptionView>> {
    let result = set_cancelled(&state, &signed_in, project_id, false).await;
    Ok(Json(
        audited(
            &state,
            &signed_in,
            "subscription.resume",
            format!("project:{project_id}"),
            result,
        )
        .await?,
    ))
}

// ---------------------------------------------------------------------------
// Background task
// ---------------------------------------------------------------------------

/// Renews subscriptions and moves lapsed ones along. Safe on every node: only the metadata
/// leader can write, and each step only applies if the subscription is still as this node
/// saw it, so nothing is charged twice. Returns how many steps it took.
pub async fn run_billing_once(
    metadata: &dyn MetadataStore,
    audit: &AuditLog,
    timings: BillingTimings,
    at: OffsetDateTime,
) -> std::result::Result<usize, MetaError> {
    let mut steps = 0;
    for mut sub in metadata.list_subscriptions().await? {
        // Each step may make another due, as after an outage: catch up a bounded amount.
        for _ in 0..24 {
            let balance = metadata.get_balance(sub.payer).await?;
            if next_step(&sub, balance, at, timings).is_none() {
                break;
            }
            let Some((updated, step)) = metadata.advance_subscription(&sub, timings, at).await?
            else {
                break;
            };
            steps += 1;
            record_step(audit, &updated, step).await;
            sub = updated;
        }
    }
    Ok(steps)
}

async fn record_step(audit: &AuditLog, sub: &Subscription, step: Step) {
    let detail = match step {
        Step::Renew { end, .. } => Some(format!("renewed until {end}")),
        _ => None,
    };
    audit
        .record(AuditEntry {
            at: OffsetDateTime::now_utc(),
            actor: "system".into(),
            action: step.action().into(),
            target: format!("project:{}", sub.project_id),
            outcome: "ok".into(),
            detail,
        })
        .await;
}

pub async fn run_billing_loop(
    metadata: Arc<dyn MetadataStore>,
    audit: Arc<AuditLog>,
    config: BillingConfig,
) {
    let mut interval = tokio::time::interval(config.tick);
    loop {
        interval.tick().await;
        match run_billing_once(
            metadata.as_ref(),
            &audit,
            config.timings,
            OffsetDateTime::now_utc(),
        )
        .await
        {
            Ok(0) => {}
            Ok(steps) => tracing::info!(steps, "billing: renewed or moved subscriptions"),
            Err(MetaError::RaftUnavailable(reason)) => {
                tracing::debug!(%reason, "billing: left for the metadata leader");
            }
            Err(err) => tracing::warn!(%err, "billing: failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rupees_are_shown_the_indian_way() {
        assert_eq!(rupees(0), "₹0");
        assert_eq!(rupees(100_000), "₹1,000");
        assert_eq!(rupees(2_000_000), "₹20,000");
        assert_eq!(rupees(10_000_000), "₹1,00,000");
        assert_eq!(rupees(123_456_789), "₹12,34,567.89");
        assert_eq!(rupees(150), "₹1.50");
    }
}
