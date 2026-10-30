//! The registry of systems under test. Each app supplies its machine (nodes,
//! property catalog, Test Composer template) and the bugs it can switch on.

use crate::composer::CmdKind;
use crate::faults::FaultConfig;
use crate::sdk::Kind;
use crate::sim::{MachineSpec, Time};
use std::collections::BTreeSet;

pub struct App {
    pub name: &'static str,
    pub about: &'static str,
    pub bugs: &'static [(&'static str, &'static str)],
    pub catalog: fn() -> Vec<(&'static str, Kind)>,
    pub commands: fn() -> Vec<(&'static str, CmdKind)>,
    pub machine: fn(&BTreeSet<String>, FaultConfig, bool, bool) -> MachineSpec,
    pub max_time: Time,
}

impl App {
    /// The catalog plus the hypervisor's built-in properties.
    pub fn full_catalog(&self) -> Vec<(&'static str, Kind)> {
        let mut c = (self.catalog)();
        c.push((crate::sim::NO_PANIC, Kind::Always));
        c
    }
}

pub fn all() -> Vec<&'static App> {
    #[allow(unused_mut)]
    let mut v: Vec<&'static App> = vec![&crate::bank::APP, &crate::cluster::APP];
    #[cfg(feature = "turso")]
    v.push(&crate::turso_app::APP);
    v
}

pub fn get(name: &str) -> &'static App {
    all().into_iter().find(|a| a.name == name).unwrap_or_else(|| {
        let names: Vec<&str> = all().iter().map(|a| a.name).collect();
        eprintln!("unknown app '{name}', choose from {names:?}");
        std::process::exit(2)
    })
}
