//! # backup — G19 backup-nudge policy (self-custody protection)
//!
//! A self-custody offline wallet has no "forgot password" — if the user loses their
//! device without having written down their recovery words, the funds are gone forever.
//! The single most important long-term protection is to **persistently, proportionately
//! nudge** the user to back up. This module owns that decision as a small, pure, fully
//! tested policy: given the account's public state, how hard should we push the backup
//! prompt right now?
//!
//! It is deliberately **clock-free and key-free** (non-money-path): the host passes in
//! the inputs (is it backed up, the balance, how long the wallet has held funds) and gets
//! back a [`BackupUrgency`] level the UI maps to the vendored `backup-banner` component's
//! escalating treatment. No seed, no signing, no network — only the public account facts.
//! The same audited decision then runs byte-identically on iOS and Android via UniFFI.
//!
//! ## The rule
//!
//! - **Backed up, or no funds yet → [`BackupUrgency::None`].** There is nothing at stake
//!   before the wallet holds value (Nimiq's own flow shows the reminder once you have a
//!   balance), and once the recovery words are written down a single HD seed protects all
//!   future funds — re-nagging a backed-up wallet is noise.
//! - Otherwise the wallet holds value and is **not** backed up, so the nudge is at least
//!   [`BackupUrgency::Gentle`] and escalates with **both** how much is at stake and how
//!   long it has been unprotected — we take the higher of the two sub-scores. More money
//!   or more elapsed time both raise the stakes of losing the device.

/// How insistent the backup prompt should be, given the account's public state.
///
/// Ordered by severity (`None` < `Gentle` < `Important` < `Critical`) so the policy can
/// take the *max* of independent sub-scores. FFI-visible: the native UI maps each level to
/// the `backup-banner` component's escalating treatment (hidden → subtle orange words →
/// solid-orange insistence).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, uniffi::Enum)]
pub enum BackupUrgency {
    /// Don't prompt: already backed up, or no funds are at stake yet.
    None,
    /// A soft reminder — funds are present but small and recently received.
    Gentle,
    /// A firm reminder — a meaningful balance, or the wallet has been unprotected a while.
    Important,
    /// An insistent warning — a large balance and/or a long time unprotected.
    Critical,
}

/// The public account facts the nudge policy reads. **No key/seed material** — only
/// whether a backup exists, the current balance, and how long the wallet has held funds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct BackupState {
    /// Has the user completed a recovery-words / login-file backup? `true` → no nudge.
    pub backed_up: bool,
    /// Current balance in **luna** (1 NIM = 100 000 luna). `0` → nothing at stake yet.
    pub balance_luna: u64,
    /// Days since the wallet first held funds. Longer unprotected → higher urgency. The
    /// host computes this from the first-funds timestamp, keeping this module clock-free.
    pub days_since_first_funds: u32,
}

/// luna per NIM (Albatross: 1 NIM = 10^5 luna).
const LUNA_PER_NIM: u64 = 100_000;

/// At/above this balance the nudge is at least [`BackupUrgency::Important`] (100 NIM).
const BALANCE_IMPORTANT_LUNA: u64 = 100 * LUNA_PER_NIM;
/// At/above this balance the nudge is [`BackupUrgency::Critical`] (1 000 NIM).
const BALANCE_CRITICAL_LUNA: u64 = 1_000 * LUNA_PER_NIM;

/// At/above this many days unprotected the nudge is at least [`BackupUrgency::Important`].
const AGE_IMPORTANT_DAYS: u32 = 7;
/// At/above this many days unprotected the nudge is [`BackupUrgency::Critical`].
const AGE_CRITICAL_DAYS: u32 = 30;

/// Decide how hard to nudge a backup, from the account's public [`BackupState`].
///
/// Returns [`BackupUrgency::None`] when there is nothing to do (backed up, or no funds);
/// otherwise the higher of the balance-driven and age-driven urgency, never below
/// [`BackupUrgency::Gentle`] (there *are* unprotected funds). Pure, total, and key-free.
#[uniffi::export]
pub fn backup_urgency(state: BackupState) -> BackupUrgency {
    if state.backed_up || state.balance_luna == 0 {
        return BackupUrgency::None;
    }
    let by_balance = if state.balance_luna >= BALANCE_CRITICAL_LUNA {
        BackupUrgency::Critical
    } else if state.balance_luna >= BALANCE_IMPORTANT_LUNA {
        BackupUrgency::Important
    } else {
        BackupUrgency::Gentle
    };
    let by_age = if state.days_since_first_funds >= AGE_CRITICAL_DAYS {
        BackupUrgency::Critical
    } else if state.days_since_first_funds >= AGE_IMPORTANT_DAYS {
        BackupUrgency::Important
    } else {
        BackupUrgency::Gentle
    };
    by_balance.max(by_age)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(backed_up: bool, balance_luna: u64, days: u32) -> BackupState {
        BackupState {
            backed_up,
            balance_luna,
            days_since_first_funds: days,
        }
    }

    #[test]
    fn backed_up_never_nudges() {
        // Even a huge, long-held balance: once backed up, no nag.
        assert_eq!(
            backup_urgency(state(true, 10_000 * LUNA_PER_NIM, 365)),
            BackupUrgency::None
        );
        assert_eq!(backup_urgency(state(true, 0, 0)), BackupUrgency::None);
    }

    #[test]
    fn no_funds_never_nudges() {
        // Nothing at stake yet — don't nag a brand-new empty wallet (matches Nimiq's flow).
        assert_eq!(backup_urgency(state(false, 0, 0)), BackupUrgency::None);
        assert_eq!(backup_urgency(state(false, 0, 999)), BackupUrgency::None);
    }

    #[test]
    fn small_recent_balance_is_gentle() {
        // Just under 100 NIM, fresh → the soft reminder, never None (funds are unprotected).
        assert_eq!(backup_urgency(state(false, 1, 0)), BackupUrgency::Gentle);
        assert_eq!(
            backup_urgency(state(false, 50 * LUNA_PER_NIM, 1)),
            BackupUrgency::Gentle
        );
    }

    #[test]
    fn balance_alone_escalates() {
        // Fresh wallet (age 0) but the balance carries the urgency.
        assert_eq!(
            backup_urgency(state(false, 100 * LUNA_PER_NIM, 0)),
            BackupUrgency::Important
        );
        assert_eq!(
            backup_urgency(state(false, 1_000 * LUNA_PER_NIM, 0)),
            BackupUrgency::Critical
        );
    }

    #[test]
    fn age_alone_escalates() {
        // Tiny balance but it has been unprotected a long time → the time carries it.
        assert_eq!(
            backup_urgency(state(false, 1, AGE_IMPORTANT_DAYS)),
            BackupUrgency::Important
        );
        assert_eq!(
            backup_urgency(state(false, 1, AGE_CRITICAL_DAYS)),
            BackupUrgency::Critical
        );
    }

    #[test]
    fn takes_the_higher_of_balance_and_age() {
        // Big balance (Critical) + fresh (Gentle) → Critical.
        assert_eq!(
            backup_urgency(state(false, 5_000 * LUNA_PER_NIM, 0)),
            BackupUrgency::Critical
        );
        // Small balance (Gentle) + long unprotected (Critical) → Critical.
        assert_eq!(
            backup_urgency(state(false, 5 * LUNA_PER_NIM, 90)),
            BackupUrgency::Critical
        );
        // Important balance + Important age → still Important (not double-counted).
        assert_eq!(
            backup_urgency(state(false, 100 * LUNA_PER_NIM, AGE_IMPORTANT_DAYS)),
            BackupUrgency::Important
        );
    }

    #[test]
    fn urgency_is_monotonic_in_balance() {
        // Holding age fixed, more money never lowers the urgency.
        let mut prev = BackupUrgency::None;
        for nim in [0u64, 1, 50, 100, 500, 1_000, 10_000] {
            let u = backup_urgency(state(false, nim * LUNA_PER_NIM, 0));
            assert!(
                u >= prev,
                "urgency dropped as balance rose: {u:?} < {prev:?}"
            );
            prev = u;
        }
    }

    #[test]
    fn urgency_is_monotonic_in_age() {
        // Holding a (small, non-zero) balance fixed, more elapsed time never lowers urgency.
        let mut prev = BackupUrgency::None;
        for days in [0u32, 1, 7, 14, 30, 60, 365] {
            let u = backup_urgency(state(false, LUNA_PER_NIM, days));
            assert!(u >= prev, "urgency dropped as age rose: {u:?} < {prev:?}");
            prev = u;
        }
    }
}
