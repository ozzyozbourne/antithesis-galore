mod apps;
mod bank;
mod cluster;
#[cfg(feature = "turso")]
mod turso_app;
mod composer;
mod explore;
mod faults;
mod sdk;
mod sim;
mod tape;

use explore::{campaign, load_tape, print_report, save_tape, CampaignOpts, Experiment, Strategy};
use sim::{Time, MS};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;
use tape::Tape;

const USAGE: &str = "\
detvisor: a teaching prototype of an Antithesis-style deterministic simulation hypervisor

USAGE: detvisor <command> [options]

COMMANDS
  run                 run a test campaign and print the triage report
  compare             random vs guided exploration: cost to find the first failure
  replay <tape>       re-execute one timeline exactly, print its trace
  check-determinism   run each seed twice and compare traces (find leaks)
  shrink <tape>       minimise a failing tape
  causality <tape>    bug probability over time (multiverse: rewind + alternate futures)
  inspect <tape>      stop a timeline at a moment and look inside (+ what-if futures)
  knobs               list every property, fault and test command

EXPERIMENT OPTIONS (also stored in saved tapes)
  --app bank|cluster  the system under test (default bank)
  --bugs b1,b2        bugs to switch on (see `detvisor knobs --app X`)
  --faults default|none|network|aggressive
  --intensity X       multiply every fault rate (default 1)
  --disable k1,k2     turn fault kinds off (latency,partition,clog,pause,kill,stop,thread_pause,clock_skip,torn_write)
  --enable k1,k2      turn opt-in fault kinds on (corrupt, lying_fsync)
  --swarm             each timeline enables a random subset of fault kinds
  --buggify           enable BUGGIFY sites inside the app

CAMPAIGN OPTIONS
  --runs N  --seed S  --strategy random|guided  --horizon-ms H (branch length, guided)
  --stop-on-fail  --out DIR (save failing tapes, default out/)

OTHER
  replay:     --grep TEXT  --tail N
  causality:  --points N --futures N
  inspect:    --at-ms T (negative = before the failure) | --event TEXT (before/after the last matching event)  --futures N
  compare:    --trials N --budget-s S
  check-determinism: --runs N
";

struct Args {
    pos: Vec<String>,
    kv: BTreeMap<String, String>,
}

impl Args {
    fn parse() -> Args {
        let mut pos = vec![];
        let mut kv = BTreeMap::new();
        let mut it = std::env::args().skip(1).peekable();
        while let Some(a) = it.next() {
            if let Some(k) = a.strip_prefix("--") {
                let flag = matches!(k, "swarm" | "buggify" | "stop-on-fail");
                let v = if flag { "true".to_string() } else { it.next().unwrap_or_else(|| die(&format!("--{k} needs a value"))) };
                kv.insert(k.to_string(), v);
            } else {
                pos.push(a);
            }
        }
        Args { pos, kv }
    }
    fn get<T: std::str::FromStr>(&self, k: &str, d: T) -> T {
        self.kv.get(k).map(|v| v.parse().unwrap_or_else(|_| die(&format!("bad value for --{k}")))).unwrap_or(d)
    }
    fn set(&self, k: &str) -> BTreeSet<String> {
        self.kv.get(k).map(|v| v.split(',').filter(|s| !s.is_empty()).map(String::from).collect()).unwrap_or_default()
    }
    fn experiment(&self) -> Experiment {
        let bugs = self.set("bugs");
        let app_name = self.get("app", "bank".to_string());
        let app = apps::get(&app_name);
        for b in &bugs {
            if !app.bugs.iter().any(|(n, _)| n == b) {
                die(&format!("unknown bug '{b}' for app {app_name}, choose from {:?}", app.bugs.iter().map(|b| b.0).collect::<Vec<_>>()));
            }
        }
        Experiment {
            app: app_name,
            bugs,
            profile: self.get("faults", "default".to_string()),
            intensity: self.get("intensity", 1.0),
            disabled: self.set("disable"),
            enabled: self.set("enable"),
            swarm: self.kv.contains_key("swarm"),
            buggify: self.kv.contains_key("buggify"),
        }
    }
    fn tape_arg(&self) -> (Experiment, Vec<u64>, String) {
        let p = self.pos.get(1).cloned().unwrap_or_else(|| die("missing <tape> file"));
        let (e, t) = load_tape(&p);
        (e, t, p)
    }
}

fn die(msg: &str) -> ! {
    eprintln!("error: {msg}\n\n{USAGE}");
    std::process::exit(2)
}

fn ms(t: Time) -> f64 {
    t as f64 / MS as f64
}

fn main() {
    let a = Args::parse();
    let cmd = a.pos.first().cloned().unwrap_or_default();
    match cmd.as_str() {
        "run" => cmd_run(&a),
        "compare" => cmd_compare(&a),
        "replay" => cmd_replay(&a),
        "check-determinism" => cmd_determinism(&a),
        "shrink" => cmd_shrink(&a),
        "causality" => cmd_causality(&a),
        "inspect" => cmd_inspect(&a),
        "knobs" => cmd_knobs(&a),
        _ => {
            print!("{USAGE}");
        }
    }
}

fn strategy(a: &Args) -> Strategy {
    match a.get("strategy", "random".to_string()).as_str() {
        "random" => Strategy::Random,
        "guided" => Strategy::Guided,
        s => die(&format!("unknown strategy {s}")),
    }
}

fn cmd_run(a: &Args) {
    let exp = a.experiment();
    let out = a.get("out", "out".to_string());
    std::fs::create_dir_all(&out).unwrap();
    let o = CampaignOpts {
        strategy: strategy(a),
        runs: a.get("runs", 200),
        seed: a.get("seed", 1),
        budget: None,
        stop_on_fail: a.kv.contains_key("stop-on-fail"),
        horizon: a.get::<u64>("horizon-ms", 60) * MS,
        out_dir: Some(out),
        quiet: false,
    };
    println!("running {} timelines ({:?} strategy)...", o.runs, o.strategy);
    let t0 = Instant::now();
    let c = campaign(&exp, &o);
    print_report(&exp, &c);
    println!("\n(wall clock {:.2}s)", t0.elapsed().as_secs_f64());
}

fn cmd_compare(a: &Args) {
    let exp = a.experiment();
    let trials: u64 = a.get("trials", 10);
    let budget = (a.get::<f64>("budget-s", 120.0) * 1e9) as Time;
    println!("Virtual time each strategy needs before the first failure (budget {:.0}s per trial):", budget as f64 / 1e9);
    println!("{:<8} {:>22} {:>22}", "trial", "random", "guided");
    let mut tot = [(0u32, 0f64), (0u32, 0f64)];
    for t in 0..trials {
        let mut cells = vec![];
        for (i, s) in [Strategy::Random, Strategy::Guided].into_iter().enumerate() {
            let o = CampaignOpts {
                strategy: s,
                runs: u32::MAX,
                seed: 1000 + t,
                budget: Some(budget),
                stop_on_fail: true,
                horizon: a.get::<u64>("horizon-ms", 60) * MS,
                out_dir: None,
                quiet: true,
            };
            let c = campaign(&exp, &o);
            match c.first_failure {
                Some((runs, cost, _)) => {
                    tot[i].0 += 1;
                    tot[i].1 += cost as f64 / 1e9;
                    cells.push(format!("{:.1}s ({} tl)", cost as f64 / 1e9, runs));
                }
                None => cells.push("not found".into()),
            }
        }
        println!("{:<8} {:>22} {:>22}", t + 1, cells[0], cells[1]);
    }
    for (i, n) in ["random", "guided"].iter().enumerate() {
        let (found, sum) = tot[i];
        println!(
            "{n}: found the bug in {found}/{trials} trials, mean virtual time to first failure {:.1}s",
            if found > 0 { sum / found as f64 } else { f64::NAN }
        );
    }
}

fn cmd_replay(a: &Args) {
    let (exp, tape, path) = a.tape_arg();
    let r1 = exp.run(Tape::replay(tape.clone()), None, true);
    let r2 = exp.run(Tape::replay(tape.clone()), None, true);
    let grep: Option<String> = a.kv.get("grep").cloned();
    let lines: Vec<&String> = r1.trace.iter().filter(|l| grep.as_ref().is_none_or(|g| l.contains(g.as_str()))).collect();
    let tail: usize = a.get("tail", lines.len());
    for l in &lines[lines.len().saturating_sub(tail)..] {
        println!("{l}");
    }
    println!("\nreplayed {path}: {} tape values, {} scheduling steps, {:.3}ms virtual time", tape.len(), r1.steps, ms(r1.end_time));
    match &r1.failure {
        Some(f) => println!("FAILED \"{}\" at {:.3}ms: {}", f.property, ms(f.time), f.detail),
        None => println!("no property violated"),
    }
    println!(
        "determinism check: trace hash {:016x} vs {:016x} -> {}",
        r1.trace_hash,
        r2.trace_hash,
        if r1.trace_hash == r2.trace_hash { "IDENTICAL" } else { "DIVERGED (nondeterminism leak!)" }
    );
}

fn cmd_determinism(a: &Args) {
    let exp = a.experiment();
    let runs: u64 = a.get("runs", 20);
    let mut bad = 0;
    for s in 0..runs {
        let r1 = exp.run(Tape::random(s), None, true);
        let r2 = exp.run(Tape::random(s), None, true);
        if r1.trace_hash != r2.trace_hash {
            bad += 1;
            let i = r1.trace.iter().zip(&r2.trace).position(|(x, y)| x != y).unwrap_or(0);
            println!("seed {s}: DIVERGED at trace line {i}");
            println!("   run 1: {}", r1.trace.get(i).map(String::as_str).unwrap_or("<end>"));
            println!("   run 2: {}", r2.trace.get(i).map(String::as_str).unwrap_or("<end>"));
        }
    }
    println!("{}/{} seeds reproduced bit-for-bit", runs - bad, runs);
}

fn cmd_shrink(a: &Args) {
    let (exp, tape, path) = a.tape_arg();
    let r = exp.run(Tape::replay(tape.clone()), None, true);
    let Some(f) = r.failure.clone() else { die("this tape does not fail") };
    let t0 = Instant::now();
    let (min, runs) = explore::shrink(&exp, &tape, &f.property, a.get("max-runs", 3000));
    let rm = exp.run(Tape::replay(min.clone()), None, true);
    let fm = rm.failure.clone().unwrap();
    let faults = |r: &sim::RunResult| r.trace.iter().filter(|l| l.contains("FAULT")).count();
    println!("shrinking \"{}\" with {} re-executions ({:.1}s wall clock)", f.property, runs, t0.elapsed().as_secs_f64());
    println!("{:<28} {:>12} {:>12}", "", "original", "shrunk");
    println!("{:<28} {:>12} {:>12}", "tape length", tape.len(), min.len());
    println!("{:<28} {:>12} {:>12}", "non-zero decisions", tape.iter().filter(|v| **v != 0).count(), min.iter().filter(|v| **v != 0).count());
    println!("{:<28} {:>12} {:>12}", "faults injected", faults(&r), faults(&rm));
    println!("{:<28} {:>11.1}ms {:>11.1}ms", "time of failure", ms(f.time), ms(fm.time));
    println!("{:<28} {:>12} {:>12}", "trace lines", r.trace.len(), rm.trace.len());
    let out = path.replace(".tape", ".min.tape");
    save_tape(&out, &exp, &min, &format!("shrunk from {path}"));
    println!("\nsaved {out}. Key events of the minimal timeline:");
    for l in rm.trace.iter().filter(|l| l.contains("FAULT") || l.contains("VIOLATED") || l.contains("lost") || l.contains("recovered") || l.contains("compaction")) {
        println!("  {l}");
    }
}

fn cmd_causality(a: &Args) {
    let (exp, tape, _) = a.tape_arg();
    let points: usize = a.get("points", 20);
    let n: u32 = a.get("futures", 30);
    let t0 = Instant::now();
    let (prop, curve) = explore::causality(&exp, &tape, points, n, a.get("seed", 7));
    println!("Causality analysis for \"{prop}\": rewind to each moment, run {n} alternate futures, count failures\n");
    for (t, p) in &curve {
        println!("{:>9.2}ms |{:<40}| {:>5.1}%", ms(*t), "#".repeat((p * 40.0).round() as usize), p * 100.0);
    }
    // show what happened around the biggest jump
    let mut best = (0, 0.0);
    for i in 1..curve.len() {
        let d = curve[i].1 - curve[i - 1].1;
        if d > best.1 {
            best = (i, d);
        }
    }
    if best.1 > 0.0 {
        let (t_lo, t_hi) = (curve[best.0 - 1].0, curve[best.0].0);
        println!("\nbiggest jump (+{:.0}%) between {:.2}ms and {:.2}ms. What happened there:", best.1 * 100.0, ms(t_lo), ms(t_hi));
        let r = exp.run(Tape::replay(tape), None, true);
        for l in r.trace.iter().filter(|l| {
            let t: f64 = l[1..10].trim().parse().unwrap_or(-1.0);
            t >= ms(t_lo) && t <= ms(t_hi)
        }) {
            if !l.contains("send ->") {
                println!("  {l}");
            }
        }
    }
    println!("\n(wall clock {:.1}s)", t0.elapsed().as_secs_f64());
}

fn cmd_inspect(a: &Args) {
    let (exp, tape, _) = a.tape_arg();
    let n: u32 = a.get("futures", 0);
    if let Some(ev) = a.kv.get("event") {
        // the last trace event matching TEXT before the failure: look just before and just after it
        let full = exp.run(Tape::replay(tape.clone()), None, true);
        let end = full.failure.as_ref().map(|f| f.time).unwrap_or(full.end_time);
        let hit = full
            .trace
            .iter()
            .zip(&full.trace_times)
            .filter(|(l, t)| l.contains(ev.as_str()) && **t <= end)
            .map(|(l, t)| (*t, l))
            .last();
        let Some((t, line)) = hit else { die(&format!("no trace event matching {ev:?} before the failure")) };
        println!("event: {line}\n\n--- just BEFORE it ---");
        inspect_at(&exp, &full, t.saturating_sub(1), n);
        println!("\n--- just AFTER it ---");
        inspect_at(&exp, &full, t, n);
        return;
    }
    let full = exp.run(Tape::replay(tape.clone()), None, false);
    // negative --at-ms means "this long before the failure"
    let at_ms: f64 = a.get("at-ms", 0.0);
    let at_ms = if at_ms < 0.0 { full.failure.as_ref().map(|f| ms(f.time)).unwrap_or(ms(full.end_time)) + at_ms } else { at_ms };
    inspect_at(&exp, &full, (at_ms.max(0.0) * MS as f64) as Time, n);
}

fn inspect_at(exp: &Experiment, full: &sim::RunResult, at: Time, n: u32) {
    let (prefix, t) = explore::moment_prefix(full, at);
    let r = exp.run(Tape::replay(prefix.clone()), Some(t), false);
    println!("moment {:.3}ms (tape prefix of {} values). State inside the machine:", ms(t), prefix.len());
    for (k, v) in &r.app_state {
        println!("  {k:<16} {v}");
    }
    if n > 0 {
        let mut outcomes: BTreeMap<String, u32> = BTreeMap::new();
        let mut rng = tape::Rng::new(99);
        for _ in 0..n {
            let r = exp.run(Tape::branch(prefix.clone(), rng.next_u64()), None, false);
            *outcomes.entry(r.failure.map(|f| format!("FAIL: {}", f.property)).unwrap_or("ok".into())).or_default() += 1;
        }
        println!("{n} alternate futures from this moment:");
        for (k, v) in outcomes {
            println!("  {v:>4} x {k}");
        }
    }
}

fn cmd_knobs(a: &Args) {
    let app = apps::get(&a.get("app", "bank".to_string()));
    println!("APPS: {}", apps::all().iter().map(|a| format!("{} ({})", a.name, a.about)).collect::<Vec<_>>().join("\n      "));
    println!("\nPROPERTIES of app '{}'", app.name);
    for (n, k) in app.full_catalog() {
        println!("  {:<22} {n}", format!("{k:?}"));
    }
    println!("\nFAULT KINDS (default profile; rates are per 1ms tick unless noted)");
    let f = faults::FaultConfig::profile("default").unwrap();
    for k in faults::FaultKind::ALL {
        let rate = match k {
            faults::FaultKind::Latency => format!("drop {} dup {} per packet", f.drop_rate, f.dup_rate),
            faults::FaultKind::TornWrite => format!("{} per crash with an in-flight write", f.torn_write),
            faults::FaultKind::LyingFsync => format!("{} per fsync", f.lying_fsync),
            _ => f.rate(k).to_string(),
        };
        let off = if f.disabled.contains(&k) { "  (off by default: --enable)" } else { "" };
        println!("  {:<14} {rate}{off}", k.name());
    }
    println!("\nTEST COMMANDS (Test Composer)");
    for (name, kind) in (app.commands)() {
        println!("  {name:<38} {kind:?}");
    }
    println!("\nBUGS (--bugs)");
    for (b, d) in app.bugs {
        println!("  {b:<14} {d}");
    }
}
