//! Test Composer: turns a set of named test commands into a different random
//! test on every timeline, following Antithesis' command prefixes
//! (https://antithesis.com/docs/test_templates/test_composer_reference/).

use crate::sdk;
use crate::sim::{os, with, MainFn, Time, MS};
use std::rc::Rc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmdKind {
    /// Exactly one runs per timeline, before any driver, without faults.
    First,
    /// The workload. Runs concurrently with other drivers, including itself.
    ParallelDriver,
    /// Runs only when no other driver is running.
    SerialDriver,
    /// If chosen, it is the only driver of that timeline.
    SingletonDriver,
    /// May run at any point after first_, alongside anything.
    Anytime,
    /// Kills all drivers, stops all faults, then checks the system recovers.
    Eventually,
    /// Runs only after every driver that started has completed; no faults.
    Finally,
}

impl CmdKind {
    pub fn from_name(name: &str) -> CmdKind {
        for (p, k) in [
            ("first_", CmdKind::First),
            ("parallel_driver_", CmdKind::ParallelDriver),
            ("serial_driver_", CmdKind::SerialDriver),
            ("singleton_driver_", CmdKind::SingletonDriver),
            ("anytime_", CmdKind::Anytime),
            ("eventually_", CmdKind::Eventually),
            ("finally_", CmdKind::Finally),
        ] {
            if name.starts_with(p) {
                return k;
            }
        }
        panic!("test command '{name}' has no recognised prefix")
    }
    fn is_driver(self) -> bool {
        matches!(self, CmdKind::ParallelDriver | CmdKind::SerialDriver | CmdKind::SingletonDriver)
    }
}

pub struct Command {
    pub name: &'static str,
    pub kind: CmdKind,
    pub run: MainFn,
}

pub fn cmd(name: &'static str, run: MainFn) -> Command {
    Command { name, kind: CmdKind::from_name(name), run }
}

pub struct Template {
    pub commands: Vec<Command>,
    /// How long the driver phase lasts (virtual time).
    pub driver_time: Time,
    /// Max concurrently running drivers.
    pub max_parallel: usize,
}

fn start(c: &Command) -> os::JoinHandle {
    os::log(format!("composer: start {}", c.name));
    os::spawn(c.name, (c.run)())
}

pub async fn compose(t: Rc<Template>) {
    let of = |k: CmdKind| t.commands.iter().filter(move |c| c.kind == k).collect::<Vec<_>>();

    // Wait for the system under test to call setup_complete().
    while !with(|w| w.setup_done()) {
        os::sleep(MS).await;
    }
    let firsts = of(CmdKind::First);
    if !firsts.is_empty() {
        let c = firsts[sdk::random_choice(firsts.len())];
        start(c).join().await;
    }
    with(|w| w.start_faults());

    // Shape of this timeline: singleton or a mix of drivers, and whether it
    // ends in an eventually_ branch (and when).
    let singletons = of(CmdKind::SingletonDriver);
    let eventuallys = of(CmdKind::Eventually);
    let singleton_mode = !singletons.is_empty() && sdk::random_choice(4) == 0;
    let deadline = os::now() + t.driver_time;
    let eventually_at = if !eventuallys.is_empty() && sdk::random_choice(3) == 0 {
        Some(os::now() + sdk::random_range(t.driver_time / 4, t.driver_time))
    } else {
        None
    };

    let mut drivers: Vec<(os::JoinHandle, CmdKind)> = vec![];
    let mut others: Vec<os::JoinHandle> = vec![];
    if singleton_mode {
        let c = singletons[sdk::random_choice(singletons.len())];
        drivers.push((start(c), c.kind));
    }
    let mut started_driver = singleton_mode;
    while os::now() < deadline {
        drivers.retain(|(h, _)| !h.is_done());
        others.retain(|h| !h.is_done());
        if let Some(at) = eventually_at {
            if os::now() >= at && started_driver {
                // eventually_: kill every driver, stop all faults, check recovery.
                for (h, _) in &drivers {
                    os::kill(h);
                }
                for h in &others {
                    os::kill(h);
                }
                os::log("composer: eventually_ branch -> drivers killed, faults stopped");
                with(|w| w.calm(None));
                let c = eventuallys[sdk::random_choice(eventuallys.len())];
                start(c).join().await;
                with(|w| w.finish());
                return;
            }
        }
        let serial_running = drivers.iter().any(|(_, k)| *k == CmdKind::SerialDriver);
        let mut options: Vec<&Command> = of(CmdKind::Anytime);
        if !singleton_mode && !serial_running && drivers.len() < t.max_parallel {
            options.extend(of(CmdKind::ParallelDriver));
            if drivers.is_empty() {
                options.extend(of(CmdKind::SerialDriver));
            }
        }
        // option index == len means "launch nothing this time"
        let i = sdk::random_choice(options.len() + 1);
        if let Some(c) = options.get(i) {
            let h = start(c);
            if c.kind.is_driver() {
                started_driver = true;
                drivers.push((h, c.kind));
            } else {
                others.push(h);
            }
        }
        os::sleep(sdk::random_range(MS, 8 * MS)).await;
    }

    // finally_: wait for every started driver to complete, stop faults, check.
    for (h, _) in &drivers {
        h.join().await;
    }
    for h in &others {
        h.join().await;
    }
    os::log("composer: all drivers completed -> faults stopped, running finally_ commands");
    with(|w| w.calm(None));
    for c in of(CmdKind::Finally) {
        start(c).join().await;
    }
    with(|w| w.finish());
}
