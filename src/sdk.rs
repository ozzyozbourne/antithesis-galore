//! The guest-side SDK, mirroring the Antithesis SDK
//! (https://antithesis.com/docs/generated/sdk/rust/antithesis_sdk/).
//!
//! * Assertions declare properties. One property per unique message. Every
//!   assertion must be declared in the app's catalog up front, because that is
//!   how "never reached" can count as a failure (Antithesis sends a
//!   declaration record at startup for the same reason).
//! * Randomness comes from the hypervisor's control tape, so the explorer can
//!   steer it. Never seed your own PRNG from it.
//! * Lifecycle: `setup_complete` turns on fault injection.

use crate::sim::{with, Time};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Fails if ever false, and fails if never reached.
    Always,
    /// Fails if ever false; passes if never reached.
    AlwaysOrUnreachable,
    /// Passes if true at least once across the campaign.
    Sometimes,
    /// Passes if reached at least once.
    Reachable,
    /// Fails if ever reached.
    Unreachable,
}

impl Kind {
    pub fn group(self) -> &'static str {
        match self {
            Kind::Always | Kind::AlwaysOrUnreachable => "Always",
            Kind::Sometimes => "Sometimes",
            Kind::Reachable | Kind::Unreachable => "Reachability",
        }
    }
}

/// What happened to one property during one run.
#[derive(Clone, Copy, Debug, Default)]
pub struct PropRun {
    pub hits: u32,
    pub trues: u32,
    pub falses: u32,
}

fn eval(kind: Kind, msg: &'static str, cond: bool, details: &str) {
    with(|w| {
        match w.catalog.get(msg) {
            Some(k) if *k == kind => {}
            other => panic!("assertion \"{msg}\" used as {kind:?} but declared as {other:?}; add it to the app catalog"),
        }
        let e = w.props.entry(msg).or_default();
        let first_true = cond && e.trues == 0;
        e.hits += 1;
        if cond {
            e.trues += 1;
        } else {
            e.falses += 1;
        }
        let violated = match kind {
            Kind::Always | Kind::AlwaysOrUnreachable => !cond,
            Kind::Unreachable => true,
            _ => false,
        };
        if violated {
            w.fail(msg, details.to_string());
        } else if first_true && matches!(kind, Kind::Sometimes | Kind::Reachable) {
            // A sometimes/reachable firing is a checkpoint the explorer may branch from.
            let n = w.current_node();
            w.log(Some(n), format!("sometimes/reachable: \"{msg}\""));
            w.mark_interesting(msg);
        }
    })
}

pub fn assert_always(cond: bool, msg: &'static str, details: &str) {
    eval(Kind::Always, msg, cond, details)
}
pub fn assert_always_or_unreachable(cond: bool, msg: &'static str, details: &str) {
    eval(Kind::AlwaysOrUnreachable, msg, cond, details)
}
pub fn assert_sometimes(cond: bool, msg: &'static str, details: &str) {
    eval(Kind::Sometimes, msg, cond, details)
}
pub fn assert_reachable(msg: &'static str, details: &str) {
    eval(Kind::Reachable, msg, true, details)
}
pub fn assert_unreachable(msg: &'static str, details: &str) {
    eval(Kind::Unreachable, msg, true, details)
}

// ---- randomness (antithesis_sdk::random)

pub fn get_random() -> u64 {
    with(|w| w.tape.draw())
}
/// Pick one of `n` options; prefer this to `get_random() % n` so the
/// explorer sees a structured choice.
pub fn random_choice(n: usize) -> usize {
    with(|w| w.tape.choice(n))
}
pub fn random_range(lo: u64, hi: u64) -> u64 {
    with(|w| w.tape.range(lo, hi))
}

// ---- lifecycle (antithesis_sdk::lifecycle)

/// The system is up: snapshot-worthy moment, and fault injection starts now.
pub fn setup_complete() {
    with(|w| w.setup_complete())
}
pub fn send_event(name: &str, details: &str) {
    with(|w| {
        let n = w.current_node();
        w.log(Some(n), format!("event {name}: {details}"))
    })
}
/// Like the ANTITHESIS_STOP_FAULTS binary: pause faults for `d` and restore
/// crashed nodes.
pub fn stop_faults(d: Time) {
    with(|w| {
        let until = w.now + d;
        w.log(None, format!("stop_faults for {:.1}ms", d as f64 / 1e6));
        w.calm(Some(until))
    })
}

/// Expose a piece of guest state to `inspect` (a stand-in for opening a bash
/// shell in a container at a past moment).
pub fn export_state(key: &str, value: String) {
    with(|w| {
        w.app_state.insert(key.to_string(), value);
    })
}

// ---- BUGGIFY (FoundationDB style)

/// The first time a site is evaluated in a run it is enabled with p=0.25 for
/// the whole run; an enabled site then fires 25% of the time.
pub fn buggify(site: &str) -> bool {
    with(|w| {
        if !w.buggify_enabled {
            return false;
        }
        let on = match w.buggify_sites.get(site) {
            Some(on) => *on,
            None => {
                let on = w.tape.chance(0.25);
                w.buggify_sites.insert(site.to_string(), on);
                on
            }
        };
        on && w.tape.chance(0.25)
    })
}
