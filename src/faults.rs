//! Fault catalog and intensity knobs, modeled on Antithesis' fault types
//! (https://antithesis.com/docs/product/writing_tests/controlling_faults/fault_types/).

use crate::sim::{Time, MS, US};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FaultKind {
    /// Baseline network latency variation, packet loss, duplication.
    Latency,
    /// Nodes split into two groups; links between them Stopped or Slowed.
    Partition,
    /// One node's whole network stack Stopped or Slowed.
    Clog,
    /// Node frozen (still "on the network": packets queue up).
    Pause,
    /// Node crash: unsynced disk writes may be lost; restarts later.
    Kill,
    /// Graceful node stop: page cache flushed, then restart.
    Stop,
    /// One guest thread frozen for a while (needs instrumentation in Antithesis).
    ThreadPause,
    /// Wall clock jumps forward/backward (monotonic time unaffected).
    ClockSkip,
    /// On a crash, the first unflushed write is partially persisted.
    TornWrite,
    /// A bit flips in durable data (bit rot). Off by default.
    Corrupt,
    /// fsync returns success without persisting anything. Off by default:
    /// no storage system can be correct on such a disk.
    LyingFsync,
}

impl FaultKind {
    pub const ALL: [FaultKind; 11] = [
        FaultKind::Latency,
        FaultKind::Partition,
        FaultKind::Clog,
        FaultKind::Pause,
        FaultKind::Kill,
        FaultKind::Stop,
        FaultKind::ThreadPause,
        FaultKind::ClockSkip,
        FaultKind::TornWrite,
        FaultKind::Corrupt,
        FaultKind::LyingFsync,
    ];
    pub fn name(self) -> &'static str {
        match self {
            FaultKind::Latency => "latency",
            FaultKind::Partition => "partition",
            FaultKind::Clog => "clog",
            FaultKind::Pause => "pause",
            FaultKind::Kill => "kill",
            FaultKind::Stop => "stop",
            FaultKind::ThreadPause => "thread_pause",
            FaultKind::ClockSkip => "clock_skip",
            FaultKind::TornWrite => "torn_write",
            FaultKind::Corrupt => "corrupt",
            FaultKind::LyingFsync => "lying_fsync",
        }
    }
    pub fn parse(s: &str) -> Option<FaultKind> {
        FaultKind::ALL.iter().copied().find(|k| k.name() == s)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LinkMode {
    Normal,
    Slowed,
    Stopped,
}

/// Every dial of the fault injector. Rates are "probability per tick".
#[derive(Clone, Debug)]
pub struct FaultConfig {
    pub tick: Time,
    pub latency_min: Time,
    pub latency_max: Time,
    pub drop_rate: f64,
    pub dup_rate: f64,
    pub partition: f64,
    pub clog: f64,
    pub partition_min: Time,
    pub partition_max: Time,
    pub pause: f64,
    pub pause_min: Time,
    pub pause_max: Time,
    pub kill: f64,
    pub stop: f64,
    pub restart_min: Time,
    pub restart_max: Time,
    pub thread_pause: f64,
    pub clock_skip: f64,
    pub clock_skip_max: Time,
    /// Probability that a crash tears the in-flight write.
    pub torn_write: f64,
    /// Bit-rot probability per tick.
    pub corrupt: f64,
    /// Probability that one fsync lies.
    pub lying_fsync: f64,
    /// Fault kinds the user turned off entirely.
    pub disabled: BTreeSet<FaultKind>,
}

impl FaultConfig {
    pub fn profile(name: &str) -> Option<FaultConfig> {
        let base = FaultConfig {
            tick: MS,
            latency_min: 100 * US,
            latency_max: 3 * MS,
            drop_rate: 0.02,
            dup_rate: 0.01,
            partition: 0.002,
            clog: 0.002,
            partition_min: 5 * MS,
            partition_max: 300 * MS,
            pause: 0.002,
            pause_min: 2 * MS,
            pause_max: 40 * MS,
            kill: 0.0015,
            stop: 0.0005,
            restart_min: 5 * MS,
            restart_max: 40 * MS,
            thread_pause: 0.004,
            clock_skip: 0.0005,
            clock_skip_max: 2_000 * MS,
            torn_write: 0.5,
            corrupt: 0.0005,
            lying_fsync: 0.05,
            disabled: [FaultKind::Corrupt, FaultKind::LyingFsync].into_iter().collect(),
        };
        Some(match name {
            "default" => base,
            "none" => FaultConfig { disabled: FaultKind::ALL.into_iter().collect(), ..base },
            "network" => FaultConfig {
                disabled: FaultKind::ALL.into_iter().filter(|k| !matches!(k, FaultKind::Latency | FaultKind::Partition | FaultKind::Clog))
                    .into_iter()
                    .collect(),
                ..base
            },
            "aggressive" => FaultConfig {
                drop_rate: 0.08,
                dup_rate: 0.04,
                partition: 0.008,
                clog: 0.008,
                pause: 0.008,
                kill: 0.006,
                stop: 0.002,
                thread_pause: 0.015,
                corrupt: 0.002,
                ..base
            },
            _ => return None,
        })
    }

    pub fn kinds(&self) -> Vec<FaultKind> {
        FaultKind::ALL.into_iter().filter(|k| !self.disabled.contains(k)).collect()
    }

    pub fn rate(&self, k: FaultKind) -> f64 {
        match k {
            FaultKind::Latency => 0.0,
            FaultKind::Partition => self.partition,
            FaultKind::Clog => self.clog,
            FaultKind::Pause => self.pause,
            FaultKind::Kill => self.kill,
            FaultKind::Stop => self.stop,
            FaultKind::ThreadPause => self.thread_pause,
            FaultKind::ClockSkip => self.clock_skip,
            FaultKind::Corrupt => self.corrupt,
            FaultKind::TornWrite | FaultKind::LyingFsync => 0.0,
        }
    }

    /// Multiply every rate by `x` (the `--intensity` dial).
    pub fn scale(&mut self, x: f64) {
        self.drop_rate *= x;
        self.dup_rate *= x;
        self.partition *= x;
        self.clog *= x;
        self.pause *= x;
        self.kill *= x;
        self.stop *= x;
        self.thread_pause *= x;
        self.clock_skip *= x;
        self.corrupt *= x;
        self.lying_fsync *= x;
    }
}
