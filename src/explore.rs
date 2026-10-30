//! The exploration layer that sits on top of the deterministic machine:
//! campaigns, guided branching, replay, shrinking and causality analysis.
//!
//! Because a timeline is a pure function of (experiment, tape), "snapshot the
//! VM at moment t" is equivalent to "keep the tape prefix consumed up to t".
//! Antithesis restores a memory snapshot instead of re-executing the prefix;
//! we re-execute it, which costs CPU but gives the same state.

use crate::apps;
#[cfg(test)]
use crate::bank;
use crate::faults::{FaultConfig, FaultKind};
use crate::sdk::{Kind, PropRun};
use crate::sim::{self, RunConfig, RunResult, Time, MS};
use crate::tape::{Rng, Tape};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

/// Everything that identifies an experiment (besides the tape).
#[derive(Clone, Debug)]
pub struct Experiment {
    pub app: String,
    pub bugs: BTreeSet<String>,
    pub profile: String,
    pub intensity: f64,
    pub disabled: BTreeSet<String>,
    pub enabled: BTreeSet<String>,
    pub swarm: bool,
    pub buggify: bool,
}

impl Experiment {
    pub fn faults(&self) -> FaultConfig {
        let mut f = FaultConfig::profile(&self.profile).unwrap_or_else(|| panic!("unknown fault profile {}", self.profile));
        f.scale(self.intensity);
        for e in &self.enabled {
            f.disabled.remove(&FaultKind::parse(e).unwrap_or_else(|| panic!("unknown fault kind {e}")));
        }
        for d in &self.disabled {
            f.disabled.insert(FaultKind::parse(d).unwrap_or_else(|| panic!("unknown fault kind {d}")));
        }
        f
    }

    pub fn run(&self, tape: Tape, stop_at: Option<Time>, trace: bool) -> RunResult {
        let app = apps::get(&self.app);
        let spec = (app.machine)(&self.bugs, self.faults(), self.swarm, self.buggify);
        sim::run(&spec, tape, RunConfig { max_time: app.max_time, stop_at, trace })
    }

    pub fn header(&self) -> String {
        let j = |s: &BTreeSet<String>| s.iter().cloned().collect::<Vec<_>>().join(",");
        format!(
            "# detvisor tape v1\napp={}\nbugs={}\nfaults={}\nintensity={}\ndisabled={}\nenabled={}\nswarm={}\nbuggify={}\n",
            self.app,
            j(&self.bugs),
            self.profile,
            self.intensity,
            j(&self.disabled),
            j(&self.enabled),
            self.swarm,
            self.buggify
        )
    }
}

pub fn save_tape(path: &str, exp: &Experiment, tape: &[u64], note: &str) {
    let mut s = exp.header();
    let _ = writeln!(s, "# {note}");
    s.push_str("tape=");
    for (i, v) in tape.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        let _ = write!(s, "{v:x}");
    }
    s.push('\n');
    std::fs::write(path, s).unwrap_or_else(|e| panic!("write {path}: {e}"));
}

pub fn load_tape(path: &str) -> (Experiment, Vec<u64>) {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let mut exp = Experiment {
        app: "bank".into(),
        bugs: BTreeSet::new(),
        profile: "default".into(),
        intensity: 1.0,
        disabled: BTreeSet::new(),
        enabled: BTreeSet::new(),
        swarm: false,
        buggify: false,
    };
    let mut tape = vec![];
    let set = |v: &str| v.split(',').filter(|s| !s.is_empty()).map(String::from).collect::<BTreeSet<_>>();
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        match k {
            "app" => exp.app = v.into(),
            "bugs" => exp.bugs = set(v),
            "faults" => exp.profile = v.into(),
            "intensity" => exp.intensity = v.parse().unwrap(),
            "disabled" => exp.disabled = set(v),
            "enabled" => exp.enabled = set(v),
            "swarm" => exp.swarm = v == "true",
            "buggify" => exp.buggify = v == "true",
            "tape" => tape = v.split(' ').filter(|s| !s.is_empty()).map(|x| u64::from_str_radix(x, 16).unwrap()).collect(),
            _ => {}
        }
    }
    (exp, tape)
}

// ------------------------------------------------------------------ campaign

#[derive(Default)]
pub struct PropAgg {
    pub runs_reached: u32,
    pub runs_true: u32,
    pub runs_false: u32,
    pub failing_examples: Vec<String>,
}

pub struct Campaign {
    pub runs: u32,
    pub branches: u32,
    pub props: BTreeMap<&'static str, PropAgg>,
    pub faults: BTreeMap<&'static str, u32>,
    /// Virtual time simulated, counting a branch only from its branch point
    /// (what a snapshotting hypervisor would pay).
    pub virtual_cost: Time,
    pub first_failure: Option<(u32, Time, String)>,
    pub failures: u32,
    pub corpus_size: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Strategy {
    /// Independent timelines from fresh seeds (classic DST).
    Random,
    /// Branch from tape prefixes where rare sometimes/reachable assertions fired.
    Guided,
}

pub struct CampaignOpts {
    pub strategy: Strategy,
    pub runs: u32,
    pub seed: u64,
    pub budget: Option<Time>,
    pub stop_on_fail: bool,
    pub horizon: Time,
    pub out_dir: Option<String>,
    pub quiet: bool,
}

struct BranchPoint {
    prefix: Vec<u64>,
    time: Time,
    label: String,
    picks: u32,
}

pub fn campaign(exp: &Experiment, o: &CampaignOpts) -> Campaign {
    let catalog = apps::get(&exp.app).full_catalog();
    let mut c = Campaign {
        runs: 0,
        branches: 0,
        props: catalog.iter().map(|(n, _)| (*n, PropAgg::default())).collect(),
        faults: BTreeMap::new(),
        virtual_cost: 0,
        first_failure: None,
        failures: 0,
        corpus_size: 0,
    };
    let mut rng = Rng::new(o.seed);
    let mut corpus: Vec<BranchPoint> = vec![];
    for i in 0..o.runs {
        if o.budget.is_some_and(|b| c.virtual_cost >= b) {
            break;
        }
        let seed = rng.next_u64();
        // Guided: most timelines restart from an interesting moment. Energy is
        // spread evenly across assertion labels (like SOMETIMES_EACH buckets),
        // then across that label's branch points, least-picked first.
        let pick = (o.strategy == Strategy::Guided && !corpus.is_empty() && rng.next_u64() % 20 != 0).then(|| {
            let labels: BTreeSet<&str> = corpus.iter().map(|b| b.label.as_str()).collect();
            let labels: Vec<&str> = labels.into_iter().collect();
            let label = labels[(rng.next_u64() % labels.len() as u64) as usize];
            let pts: Vec<usize> = (0..corpus.len()).filter(|&j| corpus[j].label == label).collect();
            let min = pts.iter().map(|&j| corpus[j].picks).min().unwrap();
            let least: Vec<usize> = pts.into_iter().filter(|&j| corpus[j].picks == min).collect();
            least[(rng.next_u64() % least.len() as u64) as usize]
        });
        let (tape, stop_at, start) = match pick {
            Some(j) => {
                let b = &mut corpus[j];
                b.picks += 1;
                (Tape::branch(b.prefix.clone(), seed), Some(b.time + o.horizon), b.time)
            }
            None => (Tape::random(seed), None, 0),
        };
        let r = exp.run(tape, stop_at, false);
        c.runs += 1;
        if pick.is_some() {
            c.branches += 1;
        }
        c.virtual_cost += r.end_time.saturating_sub(start);
        for (k, v) in &r.fault_counts {
            *c.faults.entry(k).or_default() += v;
        }
        for (name, pr) in &r.props {
            let a = c.props.entry(name).or_default();
            let PropRun { hits, trues, falses } = *pr;
            if hits > 0 {
                a.runs_reached += 1;
            }
            if trues > 0 {
                a.runs_true += 1;
            }
            if falses > 0 || r.failure.as_ref().is_some_and(|f| f.property == *name) {
                a.runs_false += 1;
            }
        }
        // add new branch points: the first time each label fires in this run
        if o.strategy == Strategy::Guided {
            for (pos, t, label) in &r.interesting {
                let n = corpus.iter().filter(|b| &b.label == label).count();
                if n < 16 && r.failure.as_ref().is_none_or(|f| f.time > *t) {
                    corpus.push(BranchPoint { prefix: r.tape[..*pos].to_vec(), time: *t, label: label.clone(), picks: 0 });
                }
            }
        }
        if let Some(f) = &r.failure {
            c.failures += 1;
            let name = catalog.iter().map(|(n, _)| *n).find(|n| *n == f.property).unwrap_or(sim::NO_PANIC);
            let agg = c.props.entry(name).or_default();
            if agg.failing_examples.len() < 3 {
                if let Some(dir) = &o.out_dir {
                    let path = format!("{dir}/{}-{}.tape", slug(&f.property), agg.failing_examples.len() + 1);
                    save_tape(&path, exp, &r.tape, &format!("fails \"{}\" at {:.3}ms: {}", f.property, f.time as f64 / MS as f64, f.detail));
                    agg.failing_examples.push(path);
                }
            }
            if c.first_failure.is_none() {
                c.first_failure = Some((c.runs, c.virtual_cost, f.property.clone()));
                if !o.quiet {
                    println!(
                        "  timeline {:>4}{}: FAIL \"{}\" at {:.1}ms  ({})",
                        i + 1,
                        if pick.is_some() { " (branch)" } else { "" },
                        f.property,
                        f.time as f64 / MS as f64,
                        f.detail
                    );
                }
            }
            if o.stop_on_fail {
                break;
            }
        }
    }
    c.corpus_size = corpus.len();
    c
}

pub fn slug(s: &str) -> String {
    s.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect::<String>().trim_matches('-').to_string()
}

pub fn prop_status(kind: Kind, a: &PropAgg) -> (&'static str, String) {
    let ex = format!("{} passing / {} failing timelines", a.runs_reached.saturating_sub(a.runs_false), a.runs_false);
    match kind {
        Kind::Always if a.runs_false > 0 => ("FAIL", ex),
        Kind::Always if a.runs_reached == 0 => ("FAIL", "never reached".into()),
        Kind::Always => ("pass", ex),
        Kind::AlwaysOrUnreachable if a.runs_false > 0 => ("FAIL", ex),
        Kind::AlwaysOrUnreachable => ("pass", if a.runs_reached == 0 { "never reached (ok)".into() } else { ex }),
        Kind::Sometimes if a.runs_true > 0 => ("pass", format!("true in {} timelines", a.runs_true)),
        Kind::Sometimes => ("FAIL", "never true: this situation was never tested".into()),
        Kind::Reachable if a.runs_reached > 0 => ("pass", format!("reached in {} timelines", a.runs_reached)),
        Kind::Reachable => ("FAIL", "never reached".into()),
        Kind::Unreachable if a.runs_reached > 0 => ("FAIL", format!("reached in {} timelines", a.runs_reached)),
        Kind::Unreachable => ("pass", "never reached".into()),
    }
}

pub fn print_report(exp: &Experiment, c: &Campaign) {
    println!("\n=== Triage report ===");
    println!(
        "experiment: bugs={:?} faults={} intensity={} swarm={} buggify={}",
        exp.bugs, exp.profile, exp.intensity, exp.swarm, exp.buggify
    );
    println!(
        "timelines: {} ({} branched from an interesting moment), virtual time explored: {:.1}s, corpus: {} branch points",
        c.runs,
        c.branches,
        c.virtual_cost as f64 / 1e9,
        c.corpus_size
    );
    println!("faults injected: {:?}", c.faults);
    let catalog = apps::get(&exp.app).full_catalog();
    println!("app: {} ({})", exp.app, apps::get(&exp.app).about);
    for group in ["Always", "Sometimes", "Reachability"] {
        println!("\n  {group} properties");
        for (name, kind) in catalog.iter().filter(|(_, k)| k.group() == group) {
            let a = &c.props[name];
            let (st, why) = prop_status(*kind, a);
            let mark = if st == "FAIL" { "✗" } else { "✓" };
            println!("   {mark} {st:<4} {:<58} {why}", format!("[{:?}] {name}", kind));
            for e in &a.failing_examples {
                println!("              example: {e}");
            }
        }
    }
}

// ------------------------------------------------------------------ shrink

/// Hypothesis-style shrinking on the tape: shorter, then with more zeros
/// (0 = the most boring decision), keeping changes that still fail the same
/// property.
pub fn shrink(exp: &Experiment, tape: &[u64], property: &str, max_runs: u32) -> (Vec<u64>, u32) {
    let mut runs = 0;
    let fails = |t: &[u64], runs: &mut u32| -> bool {
        *runs += 1;
        let r = exp.run(Tape::replay(t.to_vec()), None, false);
        r.failure.is_some_and(|f| f.property == property)
    };
    let mut best = tape.to_vec();
    // 1. truncate: everything past the end reads as 0
    let (mut lo, mut hi) = (0usize, best.len());
    while lo < hi && runs < max_runs {
        let mid = (lo + hi) / 2;
        if fails(&best[..mid], &mut runs) {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    best.truncate(hi);
    // 2. delete chunks, then zero chunks, halving the chunk size each pass
    for delete in [true, false] {
        let mut chunk = (best.len() / 2).max(1);
        while runs < max_runs {
            let mut i = 0;
            while i < best.len() && runs < max_runs {
                let end = (i + chunk).min(best.len());
                if best[i..end].iter().any(|v| *v != 0) {
                    let mut cand = best.clone();
                    if delete {
                        cand.drain(i..end);
                    } else {
                        cand[i..end].iter_mut().for_each(|v| *v = 0);
                    }
                    if fails(&cand, &mut runs) {
                        best = cand;
                        if delete {
                            continue; // same i now points at the next chunk
                        }
                    }
                }
                i = end;
            }
            if chunk == 1 {
                break;
            }
            chunk /= 2;
        }
    }
    // trailing zeros are implicit
    while best.last() == Some(&0) {
        best.pop();
    }
    (best, runs)
}

// ------------------------------------------------------------------ causality

pub fn moment_prefix(r: &RunResult, t: Time) -> (Vec<u64>, Time) {
    let mut pos = 0;
    let mut at = 0;
    for (p, mt) in &r.moments {
        if *mt > t {
            break;
        }
        pos = *p;
        at = *mt;
    }
    (r.tape[..pos].to_vec(), at)
}

/// For moments along the failing timeline, run `n` alternate futures and
/// measure how often the same property still fails.
pub fn causality(exp: &Experiment, tape: &[u64], points: usize, n: u32, seed: u64) -> (String, Vec<(Time, f64)>) {
    let base = exp.run(Tape::replay(tape.to_vec()), None, true);
    let Some(f) = base.failure.clone() else { panic!("this tape does not fail") };
    let mut rng = Rng::new(seed);
    let mut curve = vec![];
    for k in 0..=points {
        let t = f.time * k as u64 / points as u64;
        let (prefix, _) = moment_prefix(&base, t);
        let mut bad = 0;
        for _ in 0..n {
            let r = exp.run(Tape::branch(prefix.clone(), rng.next_u64()), None, false);
            if r.failure.is_some_and(|g| g.property == f.property) {
                bad += 1;
            }
        }
        curve.push((t, bad as f64 / n as f64));
    }
    (f.property, curve)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exp(bugs: &[&str]) -> Experiment {
        Experiment {
            app: "bank".into(),
            bugs: bugs.iter().map(|s| s.to_string()).collect(),
            profile: "default".into(),
            intensity: 1.0,
            disabled: BTreeSet::new(),
            enabled: BTreeSet::new(),
            swarm: false,
            buggify: false,
        }
    }

    #[test]
    fn same_tape_same_timeline() {
        let e = exp(&["ack"]);
        for seed in 0..20 {
            let a = e.run(Tape::random(seed), None, false);
            let b = e.run(Tape::replay(a.tape.clone()), None, false);
            assert_eq!(a.trace_hash, b.trace_hash, "seed {seed}");
            assert_eq!(a.failure.map(|f| f.time), b.failure.map(|f| f.time));
        }
    }

    #[test]
    fn correct_app_survives_faults_and_bugs_are_found() {
        let opts = |runs| CampaignOpts {
            strategy: Strategy::Random,
            runs,
            seed: 3,
            budget: None,
            stop_on_fail: true,
            horizon: 60 * MS,
            out_dir: None,
            quiet: true,
        };
        assert_eq!(campaign(&exp(&[]), &opts(200)).failures, 0);
        for (bug, prop) in [("race", bank::CONSERVED), ("dup", bank::AT_MOST_ONCE), ("ack", bank::DURABLE)] {
            let c = campaign(&exp(&[bug]), &opts(2000));
            assert_eq!(c.first_failure.map(|f| f.2), Some(prop.to_string()), "bug {bug}");
        }
    }
}
