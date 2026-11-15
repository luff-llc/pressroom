//! Admission by weight. Pure: no clock, no files, no sockets.
//!
//! A machine offers a budget in threads and memory. Work is admitted only while the sum
//! of what is running fits. Work that does not fit waits, in order, with the reason.

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    /// Someone is waiting on the result.
    Waiting = 0,
    /// A check that gates a release.
    Gate = 1,
    /// Everything else.
    Batch = 2,
}

impl Class {
    pub fn parse(text: &str) -> Option<Class> {
        match text {
            "waiting" => Some(Class::Waiting),
            "gate" => Some(Class::Gate),
            "batch" => Some(Class::Batch),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Class::Waiting => "waiting",
            Class::Gate => "gate",
            Class::Batch => "batch",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A person's own machine: agents get a share that depends on whether the person
    /// is using it.
    Workstation,
    /// A machine that exists to run work, or an always-on server.
    Worker,
}

/// What the owner declared.
#[derive(Clone, Debug)]
pub struct Limits {
    pub kind: Kind,
    /// Share of threads agents may use while the person is using the machine.
    pub share_present: f64,
    /// Share while the person is away. Also the share on a worker.
    pub share_away: f64,
    /// Memory kept back for the person and the system, in bytes.
    pub memory_reserve: u64,
    /// Work is not admitted when free disk is below this, in bytes.
    pub disk_floor: u64,
    /// Seconds without input after which the person counts as away.
    pub away_after_secs: u64,
}

/// What was measured.
#[derive(Clone, Debug, Default)]
pub struct Measured {
    pub threads: u32,
    pub memory: u64,
    pub disk_free: u64,
    pub load1: f64,
    /// Seconds since the person last touched the machine. `None` when it can't be told.
    pub idle_secs: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Budget {
    pub threads: u32,
    pub memory: u64,
    pub person_present: bool,
}

pub fn budget(limits: &Limits, m: &Measured) -> Budget {
    // When presence can't be told on a workstation, assume the person is there.
    let person_present = match limits.kind {
        Kind::Worker => false,
        Kind::Workstation => m.idle_secs.is_none_or(|idle| idle < limits.away_after_secs),
    };
    let share = if person_present {
        limits.share_present
    } else {
        limits.share_away
    };
    let share = share.clamp(0.0, 1.0);
    let threads = ((m.threads as f64 * share).floor() as u32).max(1);
    Budget {
        threads,
        memory: m.memory.saturating_sub(limits.memory_reserve),
        person_present,
    }
}

#[derive(Clone, Debug)]
pub struct Request {
    pub seq: u64,
    pub class: Class,
    pub threads: u32,
    pub memory: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Decision {
    /// Start now, with this many threads.
    Admit { threads: u32 },
    /// Wait, and say why.
    Wait { reason: String },
    /// This machine can never run it.
    Refuse { reason: String },
}

/// Threads and memory already admitted.
#[derive(Clone, Copy, Debug, Default)]
pub struct InUse {
    pub threads: u32,
    pub memory: u64,
}

/// Load on the machine that pressroom did not admit: a build someone started outside it,
/// or the person's own work. Rounded to whole threads.
pub fn outside_load(m: &Measured, in_use: InUse) -> u32 {
    let load = m.load1.max(0.0).round() as u32;
    load.saturating_sub(in_use.threads)
}

/// Decide for one request. `ahead` is how many waiting requests come before it in order
/// (class first, then arrival); only the head of the line may be admitted, so a large
/// job is never starved by small ones slipping past.
pub fn decide(
    limits: &Limits,
    m: &Measured,
    in_use: InUse,
    request: &Request,
    ahead: usize,
) -> Decision {
    let b = budget(limits, m);
    if request.memory > b.memory {
        return Decision::Refuse {
            reason: format!(
                "needs {} of memory; this machine offers {}",
                bytes(request.memory),
                bytes(b.memory)
            ),
        };
    }
    if ahead > 0 {
        return Decision::Wait {
            reason: format!("{ahead} ahead in the queue"),
        };
    }
    if m.disk_free < limits.disk_floor {
        return Decision::Wait {
            reason: format!(
                "disk has {} free, below this machine's floor of {}",
                bytes(m.disk_free),
                bytes(limits.disk_floor)
            ),
        };
    }
    // A request for more threads than the budget is given the budget, not refused. And a
    // request is given what is free when that is at least half of what it asked, so a
    // little load from elsewhere does not hold a large job back for ever. What is
    // granted never takes the machine past its budget.
    let want = request.threads.clamp(1, b.threads);
    let outside = outside_load(m, in_use);
    let free = b.threads.saturating_sub(in_use.threads + outside);
    if free < want.div_ceil(2) {
        let who = if b.person_present {
            " while its person is using it"
        } else {
            ""
        };
        let reason = if b.threads.saturating_sub(in_use.threads) >= want.div_ceil(2) {
            format!(
                "machine is busy with work not started through pressroom \
                 (load {:.1}; {} of {} threads offered{who})",
                m.load1, b.threads, m.threads
            )
        } else {
            format!(
                "{} of {} offered threads in use{who}; this needs {want}",
                in_use.threads, b.threads
            )
        };
        return Decision::Wait { reason };
    }
    let granted = want.min(free);
    if in_use.memory + request.memory > b.memory {
        return Decision::Wait {
            reason: format!(
                "{} of {} offered memory in use; this needs {}",
                bytes(in_use.memory),
                bytes(b.memory),
                bytes(request.memory)
            ),
        };
    }
    Decision::Admit { threads: granted }
}

/// How many waiting requests come before `seq`.
pub fn ahead_of(waiting: &[Request], seq: u64) -> usize {
    let Some(me) = waiting.iter().find(|r| r.seq == seq) else {
        return 0;
    };
    waiting
        .iter()
        .filter(|r| (r.class, r.seq) < (me.class, me.seq))
        .count()
}

pub const GB: u64 = 1 << 30;

pub fn bytes(n: u64) -> String {
    if n >= GB {
        format!("{:.1} GB", n as f64 / GB as f64)
    } else {
        format!("{} MB", n >> 20)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(kind: Kind) -> Limits {
        Limits {
            kind,
            share_present: 0.5,
            share_away: 0.75,
            memory_reserve: 16 * GB,
            disk_floor: 20 * GB,
            away_after_secs: 300,
        }
    }
    fn macbook(idle: Option<u64>, load1: f64) -> Measured {
        Measured {
            threads: 16,
            memory: 64 * GB,
            disk_free: 100 * GB,
            load1,
            idle_secs: idle,
        }
    }
    fn req(seq: u64, class: Class, threads: u32, gb: u64) -> Request {
        Request {
            seq,
            class,
            threads,
            memory: gb * GB,
        }
    }

    #[test]
    fn budget_follows_the_person() {
        let l = limits(Kind::Workstation);
        assert_eq!(budget(&l, &macbook(Some(10), 0.0)).threads, 8);
        assert_eq!(budget(&l, &macbook(Some(900), 0.0)).threads, 12);
        // Unknown presence on a workstation counts as present.
        assert_eq!(budget(&l, &macbook(None, 0.0)).threads, 8);
        // A worker has no person.
        assert_eq!(
            budget(&limits(Kind::Worker), &macbook(Some(1), 0.0)).threads,
            12
        );
        assert_eq!(budget(&l, &macbook(Some(10), 0.0)).memory, 48 * GB);
    }

    #[test]
    fn four_builds_do_not_all_start() {
        // The night of 14 November: four builds, each wanting every core.
        let l = limits(Kind::Workstation);
        let m = macbook(Some(10), 1.0);
        let mut in_use = InUse::default();
        let mut started = 0;
        for seq in 0..4 {
            match decide(&l, &m, in_use, &req(seq, Class::Batch, 16, 4), 0) {
                Decision::Admit { threads } => {
                    in_use.threads += threads;
                    in_use.memory += 4 * GB;
                    started += 1;
                }
                Decision::Wait { .. } => {}
                Decision::Refuse { reason } => panic!("{reason}"),
            }
        }
        assert_eq!(started, 1);
        assert_eq!(
            in_use.threads, 7,
            "the offered half, less the load already there"
        );
    }

    #[test]
    fn small_jobs_share_the_budget() {
        let l = limits(Kind::Workstation);
        let m = macbook(Some(10), 0.0);
        let a = decide(&l, &m, InUse::default(), &req(0, Class::Batch, 4, 4), 0);
        assert_eq!(a, Decision::Admit { threads: 4 });
        let used = InUse {
            threads: 4,
            memory: 4 * GB,
        };
        assert_eq!(
            decide(&l, &m, used, &req(1, Class::Batch, 4, 4), 0),
            Decision::Admit { threads: 4 }
        );
        let used = InUse {
            threads: 8,
            memory: 8 * GB,
        };
        assert!(matches!(
            decide(&l, &m, used, &req(2, Class::Batch, 1, 1), 0),
            Decision::Wait { .. }
        ));
    }

    #[test]
    fn a_job_takes_what_is_free_if_that_is_half_its_ask() {
        let l = limits(Kind::Workstation);
        let m = macbook(Some(10), 0.0);
        let used = InUse {
            threads: 4,
            memory: GB,
        };
        assert_eq!(
            decide(&l, &m, used, &req(1, Class::Batch, 8, 1), 0),
            Decision::Admit { threads: 4 }
        );
        let used = InUse {
            threads: 5,
            memory: GB,
        };
        assert!(matches!(
            decide(&l, &m, used, &req(1, Class::Batch, 8, 1), 0),
            Decision::Wait { .. }
        ));
        // Never past the budget, whatever is asked.
        assert_eq!(
            decide(&l, &m, InUse::default(), &req(1, Class::Batch, 500, 1), 0),
            Decision::Admit { threads: 8 }
        );
    }

    #[test]
    fn outside_load_counts() {
        let l = limits(Kind::Workstation);
        // Load 30 from builds nobody admitted: nothing new starts.
        let d = decide(
            &l,
            &macbook(Some(10), 30.0),
            InUse::default(),
            &req(0, Class::Batch, 2, 1),
            0,
        );
        match d {
            Decision::Wait { reason } => assert!(reason.contains("not started through pressroom")),
            other => panic!("{other:?}"),
        }
        // Our own admitted work is not counted twice.
        let used = InUse {
            threads: 4,
            memory: GB,
        };
        assert_eq!(outside_load(&macbook(Some(10), 4.4), used), 0);
        assert_eq!(outside_load(&macbook(Some(10), 9.0), used), 5);
    }

    #[test]
    fn memory_and_disk() {
        let l = limits(Kind::Workstation);
        let m = macbook(Some(10), 0.0);
        assert!(matches!(
            decide(&l, &m, InUse::default(), &req(0, Class::Batch, 2, 60), 0),
            Decision::Refuse { .. }
        ));
        let used = InUse {
            threads: 2,
            memory: 40 * GB,
        };
        assert!(matches!(
            decide(&l, &m, used, &req(1, Class::Batch, 2, 10), 0),
            Decision::Wait { .. }
        ));
        let mut low = m.clone();
        low.disk_free = 5 * GB;
        match decide(&l, &low, InUse::default(), &req(2, Class::Batch, 2, 1), 0) {
            Decision::Wait { reason } => assert!(reason.contains("floor")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn order_is_class_then_arrival() {
        let waiting = vec![
            req(1, Class::Batch, 8, 1),
            req(2, Class::Batch, 1, 1),
            req(3, Class::Waiting, 1, 1),
            req(4, Class::Gate, 1, 1),
        ];
        assert_eq!(ahead_of(&waiting, 3), 0);
        assert_eq!(ahead_of(&waiting, 4), 1);
        assert_eq!(ahead_of(&waiting, 1), 2);
        assert_eq!(ahead_of(&waiting, 2), 3);
        // Only the head is admitted, even if a later one would fit.
        let l = limits(Kind::Workstation);
        let m = macbook(Some(10), 0.0);
        assert!(matches!(
            decide(&l, &m, InUse::default(), &waiting[1], ahead_of(&waiting, 2)),
            Decision::Wait { .. }
        ));
    }
}
