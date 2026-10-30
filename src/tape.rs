//! The control tape: the single source of every nondeterministic decision.
//!
//! Antithesis describes its hypervisor as replacing "all non-deterministic
//! operations ... with deterministic versions controlled by a stream of control
//! signals". This is that stream. The scheduler, the network, the fault
//! injector, BUGGIFY sites and the guest's own `random()` calls all draw from
//! it, so a run is a pure function of (experiment config, tape).
//!
//! Encoding rule that makes shrinking work: a draw of 0 always means the most
//! boring outcome (run the first runnable task, minimum latency, no fault).

/// SplitMix64: tiny, fast, and identical on every platform.
#[derive(Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed ^ 0x5DEE_CE66_D1CE_4E5B)
    }
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

#[derive(Clone)]
enum Suffix {
    /// Fresh entropy after the prefix (a new timeline / a new branch).
    Random(Rng),
    /// Zeros after the prefix (exact replay, shrinking).
    Zeros,
}

#[derive(Clone)]
pub struct Tape {
    prefix: Vec<u64>,
    suffix: Suffix,
    /// Every value actually consumed, in order. Saving this is saving the run.
    pub recorded: Vec<u64>,
}

impl Tape {
    /// A brand-new timeline.
    pub fn random(seed: u64) -> Self {
        Self::branch(Vec::new(), seed)
    }
    /// Replay `prefix` exactly, then continue with fresh entropy from `seed`.
    pub fn branch(prefix: Vec<u64>, seed: u64) -> Self {
        Tape { prefix, suffix: Suffix::Random(Rng::new(seed)), recorded: Vec::new() }
    }
    /// Replay a recorded tape exactly (anything past its end reads as 0).
    pub fn replay(values: Vec<u64>) -> Self {
        Tape { prefix: values, suffix: Suffix::Zeros, recorded: Vec::new() }
    }

    pub fn pos(&self) -> usize {
        self.recorded.len()
    }

    pub fn draw(&mut self) -> u64 {
        let i = self.recorded.len();
        let v = if i < self.prefix.len() {
            self.prefix[i]
        } else {
            match &mut self.suffix {
                Suffix::Random(r) => r.next_u64(),
                Suffix::Zeros => 0,
            }
        };
        self.recorded.push(v);
        v
    }

    /// Uniform choice in `0..n`. Does not consume the tape when there is no choice.
    pub fn choice(&mut self, n: usize) -> usize {
        if n <= 1 {
            return 0;
        }
        (self.draw() % n as u64) as usize
    }

    /// True with probability `p`. A draw of 0 is always false.
    pub fn chance(&mut self, p: f64) -> bool {
        if p <= 0.0 {
            return false;
        }
        let v = self.draw();
        v != 0 && (v as f64) >= (1.0 - p) * (u64::MAX as f64)
    }

    /// Uniform in `lo..=hi`; a draw of 0 gives `lo`.
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        if hi <= lo {
            return lo;
        }
        lo + self.draw() % (hi - lo + 1)
    }
}
