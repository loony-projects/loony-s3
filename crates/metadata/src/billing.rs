//! Paid projects: plans, subscriptions, credit and gift codes, and the rules for how a
//! subscription moves between states. The rules are plain functions of their inputs, with no
//! clock or storage, so every node reaches the same answer and they can be tested directly.
//!
//! Money is always integer paise. Durations come in from the caller, so tests can shrink a
//! month to seconds without the rules changing.

use loony_core::{CheckoutId, GiftCodeId, LedgerEntryId, ProjectId, UserId};
use time::{Duration, OffsetDateTime};

/// How long one paid period lasts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingPeriod {
    /// Calendar months. A period starting on the 31st ends on the last day of a shorter
    /// month, keeping the time of day.
    Months(u32),
    /// A fixed length, for tests that can't wait a month.
    Seconds(u64),
}

/// What can be bought. Plans come from settings, and a subscription keeps its own copy of the
/// price and storage it was bought at, so changing a plan never changes a subscription.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Plan {
    pub id: String,
    pub name: String,
    pub price_paise: u64,
    pub storage_bytes: u64,
    pub period: BillingPeriod,
}

/// Where a project's subscription is. A project with no subscription record is `Unpaid`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionState {
    /// Never paid. The project can be opened, but holds nothing.
    Unpaid,
    /// Paid for the current period.
    Active,
    /// A renewal couldn't be paid. Everything still works, for a limited time.
    Grace,
    /// Lapsed or cancelled. Reading works; writing doesn't.
    ReadOnly,
    /// Lapsed for too long. Nothing works, and nothing is deleted.
    Suspended,
}

impl SubscriptionState {
    /// Whether the project may take on data: new buckets, uploads and access keys.
    pub fn writable(self) -> bool {
        matches!(self, Self::Active | Self::Grace)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unpaid => "unpaid",
            Self::Active => "active",
            Self::Grace => "grace",
            Self::ReadOnly => "read_only",
            Self::Suspended => "suspended",
        }
    }
}

impl std::fmt::Display for SubscriptionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Subscription {
    pub project_id: ProjectId,
    pub plan: Plan,
    /// Whose credit pays for renewals: whoever last paid.
    pub payer: UserId,
    pub state: SubscriptionState,
    /// When `state` began, which is what grace and suspension are counted from.
    #[serde(with = "time::serde::rfc3339")]
    pub state_since: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub period_start: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub period_end: OffsetDateTime,
    /// The Owner cancelled: the project becomes read-only when the period ends, and nothing
    /// is charged automatically from then on.
    pub cancel_at_period_end: bool,
}

/// How long a lapsed project stays in each state. Carried in each command, so every node
/// applies the same values even if settings change in between.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BillingTimings {
    pub grace_secs: u64,
    pub suspend_after_secs: u64,
}

impl Default for BillingTimings {
    fn default() -> Self {
        Self {
            grace_secs: 7 * 24 * 60 * 60,
            suspend_after_secs: 30 * 24 * 60 * 60,
        }
    }
}

/// What one tick of the background task does to a subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Step {
    /// Charge the payer and start a new period.
    Renew {
        #[serde(with = "time::serde::rfc3339")]
        start: OffsetDateTime,
        #[serde(with = "time::serde::rfc3339")]
        end: OffsetDateTime,
    },
    /// The period ended and the renewal couldn't be paid.
    EnterGrace {
        #[serde(with = "time::serde::rfc3339")]
        since: OffsetDateTime,
    },
    /// Grace ran out, or a cancelled period ended.
    EnterReadOnly {
        #[serde(with = "time::serde::rfc3339")]
        since: OffsetDateTime,
    },
    Suspend {
        #[serde(with = "time::serde::rfc3339")]
        since: OffsetDateTime,
    },
}

impl Step {
    pub fn action(self) -> &'static str {
        match self {
            Step::Renew { .. } => "subscription.renew",
            Step::EnterGrace { .. } => "subscription.grace",
            Step::EnterReadOnly { .. } => "subscription.read_only",
            Step::Suspend { .. } => "subscription.suspend",
        }
    }
}

/// The end of a period starting at `start`.
pub fn period_end(start: OffsetDateTime, period: BillingPeriod) -> OffsetDateTime {
    match period {
        BillingPeriod::Seconds(secs) => start + Duration::seconds(secs as i64),
        BillingPeriod::Months(months) => {
            let date = start.date();
            let index = date.year() as i64 * 12 + (date.month() as i64 - 1) + months as i64;
            let year = index.div_euclid(12) as i32;
            let month = time::Month::try_from((index.rem_euclid(12) + 1) as u8)
                .expect("a month index modulo 12 is a month");
            let day = date.day().min(month.length(year));
            let date = time::Date::from_calendar_date(year, month, day)
                .expect("the day is clamped to the month's length");
            date.with_time(start.time()).assume_offset(start.offset())
        }
    }
}

/// What the background task should do to `sub` at `at`, given the payer's `balance`. `None`
/// means nothing is due. Applying the step it returns and asking again moves the subscription
/// on, one step at a time, so a long outage is caught up with one period per call.
pub fn next_step(
    sub: &Subscription,
    balance: u64,
    at: OffsetDateTime,
    timings: BillingTimings,
) -> Option<Step> {
    let can_pay = !sub.cancel_at_period_end && balance >= sub.plan.price_paise;
    let after = |since: OffsetDateTime, secs: u64| since + Duration::seconds(secs as i64);
    match sub.state {
        SubscriptionState::Unpaid => None,
        SubscriptionState::Active => {
            if at < sub.period_end {
                None
            } else if sub.cancel_at_period_end {
                Some(Step::EnterReadOnly {
                    since: sub.period_end,
                })
            } else if can_pay {
                Some(Step::Renew {
                    start: sub.period_end,
                    end: period_end(sub.period_end, sub.plan.period),
                })
            } else {
                Some(Step::EnterGrace {
                    since: sub.period_end,
                })
            }
        }
        // Paid during grace, the new period carries on from where the old one ended, so
        // paying late doesn't earn free days.
        SubscriptionState::Grace => {
            if can_pay {
                Some(Step::Renew {
                    start: sub.period_end,
                    end: period_end(sub.period_end, sub.plan.period),
                })
            } else if at >= after(sub.state_since, timings.grace_secs) {
                Some(Step::EnterReadOnly {
                    since: after(sub.state_since, timings.grace_secs),
                })
            } else {
                None
            }
        }
        SubscriptionState::ReadOnly => {
            if can_pay {
                Some(Step::Renew {
                    start: at,
                    end: period_end(at, sub.plan.period),
                })
            } else if at >= after(sub.state_since, timings.suspend_after_secs) {
                Some(Step::Suspend {
                    since: after(sub.state_since, timings.suspend_after_secs),
                })
            } else {
                None
            }
        }
        SubscriptionState::Suspended => can_pay.then(|| Step::Renew {
            start: at,
            end: period_end(at, sub.plan.period),
        }),
    }
}

/// When paying by hand, from the console, the new period. `None` if the project is already
/// paid for.
pub fn manual_payment_period(
    current: Option<&Subscription>,
    period: BillingPeriod,
    at: OffsetDateTime,
) -> Option<(OffsetDateTime, OffsetDateTime)> {
    match current.map(|sub| (sub.state, sub.period_end)) {
        Some((SubscriptionState::Active, _)) => None,
        Some((SubscriptionState::Grace, old_end)) => Some((old_end, period_end(old_end, period))),
        _ => Some((at, period_end(at, period))),
    }
}

/// How much a project stores. Bytes are logical object sizes, before redundancy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OwnerUsage {
    pub bytes: u64,
    pub objects: u64,
    /// Parts of multipart uploads still in progress. They count against the limit too, or
    /// several large uploads could start within it and finish well over it.
    pub pending_bytes: u64,
}

impl OwnerUsage {
    pub fn counted(&self) -> u64 {
        self.bytes.saturating_add(self.pending_bytes)
    }

    /// Whether adding `delta` bytes, which may be negative, would go over `limit`. Freeing
    /// space is always allowed, even while over the limit, as after a plan change.
    pub fn exceeds(&self, delta: i64, limit: u64) -> bool {
        if delta <= 0 {
            return false;
        }
        self.bytes
            .checked_add(self.pending_bytes)
            .and_then(|counted| counted.checked_add(delta as u64))
            .is_none_or(|total| total > limit)
    }
}

/// Why a balance changed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LedgerReason {
    GiftCode {
        code_id: GiftCodeId,
        /// The code's last characters, enough to recognise it without revealing it.
        hint: String,
    },
    Payment {
        checkout_id: CheckoutId,
        provider: String,
    },
    Subscription {
        project_id: ProjectId,
        plan_id: String,
        #[serde(with = "time::serde::rfc3339")]
        period_start: OffsetDateTime,
        #[serde(with = "time::serde::rfc3339")]
        period_end: OffsetDateTime,
        /// Charged by the background task rather than by someone pressing a button.
        automatic: bool,
    },
}

/// One change to a balance. Balances only ever change together with one of these.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LedgerEntry {
    pub entry_id: LedgerEntryId,
    pub user_id: UserId,
    /// Positive adds credit, negative spends it.
    pub amount_paise: i64,
    pub balance_after_paise: u64,
    pub reason: LedgerReason,
    /// Who caused it, when not the account holder: `system` for automatic renewals.
    pub actor: String,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

/// A code that adds credit. Only its hash is stored: the code itself is shown once, when it's
/// made, so a copy of the metadata doesn't hand out credit.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GiftCode {
    pub code_id: GiftCodeId,
    pub code_hash: String,
    pub hint: String,
    pub amount_paise: u64,
    #[serde(with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
    pub max_uses: u32,
    pub uses: u32,
    pub disabled: bool,
    pub note: Option<String>,
    pub created_by: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GiftCodeRedemption {
    pub code_id: GiftCodeId,
    pub user_id: UserId,
    pub amount_paise: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NewGiftCode {
    pub code_hash: String,
    pub hint: String,
    pub amount_paise: u64,
    #[serde(with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
    pub max_uses: u32,
    pub note: Option<String>,
    pub created_by: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutStatus {
    Pending,
    Succeeded,
    Failed,
}

/// One attempt to buy credit. What it was for is kept so the console can finish the
/// subscription once it's paid; the payment itself only ever adds credit.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Checkout {
    pub checkout_id: CheckoutId,
    pub user_id: UserId,
    pub amount_paise: u64,
    pub project_id: Option<ProjectId>,
    pub plan_id: Option<String>,
    pub provider: String,
    pub provider_ref: Option<String>,
    pub status: CheckoutStatus,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub completed_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NewCheckout {
    pub user_id: UserId,
    pub amount_paise: u64,
    pub project_id: Option<ProjectId>,
    pub plan_id: Option<String>,
    pub provider: String,
    pub provider_ref: Option<String>,
}

/// Gift codes are this many characters long.
pub const GIFT_CODE_LEN: usize = 17;
/// The characters a gift code is made of.
pub const GIFT_CODE_ALPHABET: &[u8; 36] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/// A gift code as typed, made canonical: uppercase, without the spaces and dashes people add
/// when copying it. `None` if it can't be a code at all.
pub fn normalize_gift_code(input: &str) -> Option<String> {
    let code: String = input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let valid =
        code.len() == GIFT_CODE_LEN && code.bytes().all(|b| GIFT_CODE_ALPHABET.contains(&b));
    valid.then_some(code)
}

/// What's stored in place of a code, so the metadata alone can't be used to redeem one.
pub fn gift_code_hash(normalized: &str) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(normalized.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const PRICE: u64 = 100_000;

    fn plan(period: BillingPeriod) -> Plan {
        Plan {
            id: "standard".into(),
            name: "Standard".into(),
            price_paise: PRICE,
            storage_bytes: 15 << 30,
            period,
        }
    }

    fn sub(state: SubscriptionState, since: OffsetDateTime, end: OffsetDateTime) -> Subscription {
        Subscription {
            project_id: ProjectId::new(),
            plan: plan(BillingPeriod::Months(1)),
            payer: UserId::new(),
            state,
            state_since: since,
            period_start: datetime!(2026-01-01 10:00 UTC),
            period_end: end,
            cancel_at_period_end: false,
        }
    }

    const T: BillingTimings = BillingTimings {
        grace_secs: 7 * 86400,
        suspend_after_secs: 30 * 86400,
    };
    const END: OffsetDateTime = datetime!(2026-02-01 10:00 UTC);

    #[test]
    fn months_keep_the_day_and_time_and_clamp_to_short_months() {
        let start = datetime!(2026-01-31 18:30 UTC);
        assert_eq!(
            period_end(start, BillingPeriod::Months(1)),
            datetime!(2026-02-28 18:30 UTC)
        );
        assert_eq!(
            period_end(datetime!(2028-01-31 00:00 UTC), BillingPeriod::Months(1)),
            datetime!(2028-02-29 00:00 UTC)
        );
        assert_eq!(
            period_end(datetime!(2026-12-15 09:00 UTC), BillingPeriod::Months(1)),
            datetime!(2027-01-15 09:00 UTC)
        );
        assert_eq!(
            period_end(datetime!(2026-03-31 00:00 UTC), BillingPeriod::Months(13)),
            datetime!(2027-04-30 00:00 UTC)
        );
        assert_eq!(
            period_end(start, BillingPeriod::Seconds(90)),
            datetime!(2026-01-31 18:31:30 UTC)
        );
    }

    #[test]
    fn nothing_happens_before_the_period_ends_or_while_unpaid() {
        let active = sub(SubscriptionState::Active, END, END);
        assert_eq!(
            next_step(&active, PRICE, END - Duration::seconds(1), T),
            None
        );
        let unpaid = sub(SubscriptionState::Unpaid, END, END);
        assert_eq!(
            next_step(&unpaid, PRICE * 10, END + Duration::days(400), T),
            None
        );
    }

    #[test]
    fn a_period_renews_from_where_it_ended_when_the_credit_is_there() {
        let active = sub(SubscriptionState::Active, END, END);
        let step = next_step(&active, PRICE, END, T);
        assert_eq!(
            step,
            Some(Step::Renew {
                start: END,
                end: datetime!(2026-03-01 10:00 UTC)
            })
        );
    }

    #[test]
    fn without_the_credit_it_goes_through_grace_read_only_and_suspension() {
        let short = PRICE - 1;
        let active = sub(SubscriptionState::Active, END, END);
        assert_eq!(
            next_step(&active, short, END, T),
            Some(Step::EnterGrace { since: END })
        );

        let grace = sub(SubscriptionState::Grace, END, END);
        let grace_ends = END + Duration::days(7);
        assert_eq!(
            next_step(&grace, short, grace_ends - Duration::seconds(1), T),
            None
        );
        assert_eq!(
            next_step(&grace, short, grace_ends, T),
            Some(Step::EnterReadOnly { since: grace_ends })
        );

        let read_only = sub(SubscriptionState::ReadOnly, grace_ends, END);
        let suspends = grace_ends + Duration::days(30);
        assert_eq!(
            next_step(&read_only, short, suspends - Duration::seconds(1), T),
            None
        );
        assert_eq!(
            next_step(&read_only, short, suspends, T),
            Some(Step::Suspend { since: suspends })
        );

        let suspended = sub(SubscriptionState::Suspended, suspends, END);
        assert_eq!(
            next_step(&suspended, short, suspends + Duration::days(999), T),
            None
        );
    }

    #[test]
    fn steps_are_dated_from_when_they_were_due_not_when_the_task_ran() {
        // A task that was down for a while still records the right dates.
        let grace = sub(SubscriptionState::Grace, END, END);
        let late = END + Duration::days(20);
        assert_eq!(
            next_step(&grace, 0, late, T),
            Some(Step::EnterReadOnly {
                since: END + Duration::days(7)
            })
        );
    }

    #[test]
    fn paying_during_grace_carries_on_from_the_old_period() {
        let grace = sub(SubscriptionState::Grace, END, END);
        let step = next_step(&grace, PRICE, END + Duration::days(3), T);
        assert_eq!(
            step,
            Some(Step::Renew {
                start: END,
                end: datetime!(2026-03-01 10:00 UTC)
            })
        );
        assert_eq!(
            manual_payment_period(
                Some(&grace),
                BillingPeriod::Months(1),
                END + Duration::days(3)
            ),
            Some((END, datetime!(2026-03-01 10:00 UTC)))
        );
    }

    #[test]
    fn credit_arriving_later_restores_a_lapsed_project_from_then() {
        let now = END + Duration::days(60);
        for state in [SubscriptionState::ReadOnly, SubscriptionState::Suspended] {
            let lapsed = sub(state, END, END);
            assert_eq!(
                next_step(&lapsed, PRICE, now, T),
                Some(Step::Renew {
                    start: now,
                    end: period_end(now, BillingPeriod::Months(1))
                }),
                "{state}"
            );
            assert_eq!(
                manual_payment_period(Some(&lapsed), BillingPeriod::Months(1), now),
                Some((now, period_end(now, BillingPeriod::Months(1))))
            );
        }
        assert_eq!(
            manual_payment_period(None, BillingPeriod::Months(1), now),
            Some((now, period_end(now, BillingPeriod::Months(1))))
        );
    }

    #[test]
    fn an_active_project_cant_be_paid_for_twice() {
        let active = sub(SubscriptionState::Active, END, END);
        assert_eq!(
            manual_payment_period(Some(&active), BillingPeriod::Months(1), END),
            None
        );
    }

    #[test]
    fn a_cancelled_subscription_is_never_charged_again() {
        let mut active = sub(SubscriptionState::Active, END, END);
        active.cancel_at_period_end = true;
        assert_eq!(
            next_step(&active, PRICE * 100, END, T),
            Some(Step::EnterReadOnly { since: END })
        );

        let mut read_only = sub(SubscriptionState::ReadOnly, END, END);
        read_only.cancel_at_period_end = true;
        assert_eq!(
            next_step(&read_only, PRICE * 100, END + Duration::days(1), T),
            None
        );
        assert_eq!(
            next_step(&read_only, PRICE * 100, END + Duration::days(30), T),
            Some(Step::Suspend {
                since: END + Duration::days(30)
            })
        );
    }

    #[test]
    fn only_paid_states_can_take_on_data() {
        use SubscriptionState::*;
        for (state, writable) in [
            (Unpaid, false),
            (Active, true),
            (Grace, true),
            (ReadOnly, false),
            (Suspended, false),
        ] {
            assert_eq!(state.writable(), writable, "{state}");
        }
    }

    #[test]
    fn quota_arithmetic() {
        let usage = OwnerUsage {
            bytes: 900,
            objects: 3,
            pending_bytes: 50,
        };
        assert_eq!(usage.counted(), 950);
        assert!(!usage.exceeds(50, 1000), "exactly full is fine");
        assert!(usage.exceeds(51, 1000));
        assert!(!usage.exceeds(0, 100), "no change is always fine");
        assert!(
            !usage.exceeds(-10, 100),
            "freeing space is fine even when over"
        );
        let huge = OwnerUsage {
            bytes: u64::MAX - 1,
            objects: 1,
            pending_bytes: 5,
        };
        assert!(huge.exceeds(1, u64::MAX), "no overflow");
    }

    #[test]
    fn gift_codes_are_read_the_way_people_type_them() {
        assert_eq!(
            normalize_gift_code("u97d9-fs8f0 s7df09s").as_deref(),
            Some("U97D9FS8F0S7DF09S")
        );
        assert_eq!(
            normalize_gift_code("U97D9FS8F0S7DF09S").as_deref(),
            Some("U97D9FS8F0S7DF09S")
        );
        for bad in [
            "",
            "U97D9FS8F0S7DF09",
            "U97D9FS8F0S7DF09SX",
            "U97D9FS8F0S7DF0!S",
            "Ü97D9FS8F0S7DF09S",
        ] {
            assert_eq!(normalize_gift_code(bad), None, "{bad:?}");
        }
        assert_eq!(gift_code_hash("A").len(), 64);
        assert_ne!(gift_code_hash("A"), gift_code_hash("B"));
    }
}
