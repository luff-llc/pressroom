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
    /// How many jobs that run an emulator or simulator may run at once (rule 27).
    pub device_slots: u32,
}

/// What was measured.
#[derive(Clone, Debug, Default)]
pub struct Measured {
    pub threads: u32,
    pub memory: u64,
    pub disk_free: u64,
    pub load1: f64,
    /// Threads' worth of processor in use over the last few seconds, by anything. `None`
    /// when it can't be measured; the load average stands in then.
    pub busy: Option<f64>,
    /// Seconds since the person last touched the machine. `None` when it can't be told.
    pub idle_secs: Option<u64>,
}

/// What the machine's person said about it, which stands in for what is measured until
/// its end (contracts/local-v1.md, rules 19 to 21).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Said {
    /// Nothing, or it has ended: the measured presence applies.
    #[default]
    Nothing,
    /// "I need the whole machine": nothing new is admitted.
    Hold,
    /// "Take it all, I am away": every thread is offered.
    Give,
}

impl Said {
    pub fn parse(text: &str) -> Option<Said> {
        match text {
            "auto" => Some(Said::Nothing),
            "hold" => Some(Said::Hold),
            "give" => Some(Said::Give),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Said::Nothing => "auto",
            Said::Hold => "hold",
            Said::Give => "give",
        }
    }
}

/// Input seen this long after the person said "give" means they are back (rule 21).
pub const BACK_AFTER_SECS: u64 = 60;

/// Whether what the person said at `said_at`, lasting until `until`, still stands at `at`.
/// "Give" also ends once the machine sees input more than a minute after it was said:
/// the person is back, and their own use comes first.
pub fn still_stands(said: Said, said_at: u64, until: u64, at: u64, idle_secs: Option<u64>) -> bool {
    if said == Said::Nothing || at >= until {
        return false;
    }
    if said == Said::Give {
        if let Some(idle) = idle_secs {
            let last_input = at.saturating_sub(idle);
            if last_input > said_at + BACK_AFTER_SECS {
                return false;
            }
        }
    }
    true
}

#[derive(Clone, Debug, PartialEq)]
pub struct Budget {
    pub threads: u32,
    pub memory: u64,
    pub person_present: bool,
}

pub fn budget(limits: &Limits, m: &Measured, said: Said) -> Budget {
    // When presence can't be told on a workstation, assume the person is there.
    let measured_present = match limits.kind {
        Kind::Worker => false,
        Kind::Workstation => m.idle_secs.is_none_or(|idle| idle < limits.away_after_secs),
    };
    let (person_present, share) = match said {
        Said::Hold => (true, 0.0),
        Said::Give => (false, 1.0),
        Said::Nothing if measured_present => (true, limits.share_present),
        Said::Nothing => (false, limits.share_away),
    };
    let share = share.clamp(0.0, 1.0);
    let threads = if said == Said::Hold {
        0
    } else {
        ((m.threads as f64 * share).floor() as u32).max(1)
    };
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
    /// It runs an emulator or simulator and needs the device slot (rule 27).
    pub device: bool,
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

/// Threads and memory charged to running work: for each job the larger of what it was
/// granted and what it was last measured to use (rule 24). And the device slots taken.
#[derive(Clone, Copy, Debug, Default)]
pub struct InUse {
    pub threads: u32,
    pub memory: u64,
    pub devices: u32,
}

/// What one running job is charged (rule 24): the larger of its grant and its measured
/// use, threads rounded to the nearest whole one.
pub fn charge(granted_threads: u32, granted_memory: u64, used: Option<(f64, u64)>) -> (u32, u64) {
    match used {
        Some((threads, memory)) => (
            granted_threads.max(threads.max(0.0).round().min(4096.0) as u32),
            granted_memory.max(memory),
        ),
        None => (granted_threads, granted_memory),
    }
}

/// Whether a job is over its grant (rule 24): more than a quarter, and at least half a
/// thread, above its threads; or above its memory.
pub fn over(granted_threads: u32, granted_memory: u64, used: Option<(f64, u64)>) -> bool {
    let Some((threads, memory)) = used else {
        return false;
    };
    let margin = (granted_threads as f64 * 0.25).max(0.5);
    threads > granted_threads as f64 + margin || memory > granted_memory
}

/// Whether a waiting request can be admitted only once a device slot frees. Such a request
/// does not hold up the line behind it (rule 27).
pub fn waits_for_device(limits: &Limits, in_use: InUse, request: &Request) -> bool {
    request.device && limits.device_slots > 0 && in_use.devices >= limits.device_slots
}

/// Load on the machine that pressroom did not admit: a build someone started outside it,
/// or the person's own work. Rounded to whole threads.
///
/// It is worked out from the processor actually in use. The load average also counts
/// threads that are waiting on something else, and on a desktop it runs several times
/// above what the processor is doing, so it is used only where use cannot be measured.
pub fn outside_load(m: &Measured, in_use: InUse) -> u32 {
    let load = m.busy.unwrap_or(m.load1).max(0.0).round() as u32;
    load.saturating_sub(in_use.threads)
}

/// The figure admission goes by, said for a person.
fn load_said(m: &Measured) -> String {
    match m.busy {
        Some(busy) => format!("{busy:.1} of {} threads busy", m.threads),
        None => format!("load {:.1}", m.load1),
    }
}

/// Threads free for new work: what is left of the offer, and never more than what is left
/// of the machine once admitted work and outside load are counted.
pub fn free_threads(machine: u32, offered: u32, in_use: u32, outside: u32) -> u32 {
    let of_offer = offered.saturating_sub(in_use);
    let of_machine = machine.saturating_sub(in_use.saturating_add(outside));
    of_offer.min(of_machine)
}

/// Decide for one request. `ahead` is how many waiting requests come before it in order
/// (class first, then arrival); only the head of the line may be admitted, so a large
/// job is never starved by small ones slipping past.
pub fn decide(
    limits: &Limits,
    m: &Measured,
    said: Said,
    in_use: InUse,
    request: &Request,
    ahead: usize,
) -> Decision {
    let b = budget(limits, m, said);
    if request.memory > b.memory {
        return Decision::Refuse {
            reason: format!(
                "needs {} of memory; this machine offers {}",
                bytes(request.memory),
                bytes(b.memory)
            ),
        };
    }
    if request.device && limits.device_slots == 0 {
        return Decision::Refuse {
            reason: "this machine runs no emulators or simulators (device_slots = 0)".into(),
        };
    }
    if said == Said::Hold {
        return Decision::Wait {
            reason: "its person has asked for the whole machine".into(),
        };
    }
    if waits_for_device(limits, in_use, request) {
        return Decision::Wait {
            reason: format!(
                "the machine's device slot{} taken; one emulator or simulator at a time",
                if limits.device_slots == 1 { " is" } else { "s are all" }
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
    //
    // Load pressroom did not admit is taken first from the threads the machine keeps back
    // (the person's share on a workstation), and only what is left over comes off the
    // offer. A desktop's ordinary load therefore never holds the queue; a build started
    // outside pressroom that fills the machine still does.
    let want = request.threads.clamp(1, b.threads);
    let outside = outside_load(m, in_use);
    let free = free_threads(m.threads, b.threads, in_use.threads, outside);
    if free < want.div_ceil(2) {
        let who = if b.person_present {
            " while its person is using it"
        } else {
            ""
        };
        let reason = if b.threads.saturating_sub(in_use.threads) >= want.div_ceil(2) {
            format!(
                "machine is busy with work not started through pressroom \
                 ({}; {} of {} threads offered{who})",
                load_said(m),
                b.threads,
                m.threads
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
            device_slots: 1,
        }
    }
    fn macbook(idle: Option<u64>, load1: f64) -> Measured {
        Measured {
            threads: 16,
            memory: 64 * GB,
            disk_free: 100 * GB,
            load1,
            busy: None,
            idle_secs: idle,
        }
    }
    fn req(seq: u64, class: Class, threads: u32, gb: u64) -> Request {
        Request {
            seq,
            class,
            threads,
            memory: gb * GB,
            device: false,
        }
    }

    #[test]
    fn budget_follows_the_person() {
        let l = limits(Kind::Workstation);
        assert_eq!(
            budget(&l, &macbook(Some(10), 0.0), Said::Nothing).threads,
            8
        );
        assert_eq!(
            budget(&l, &macbook(Some(900), 0.0), Said::Nothing).threads,
            12
        );
        // Unknown presence on a workstation counts as present.
        assert_eq!(budget(&l, &macbook(None, 0.0), Said::Nothing).threads, 8);
        // A worker has no person.
        assert_eq!(
            budget(&limits(Kind::Worker), &macbook(Some(1), 0.0), Said::Nothing).threads,
            12
        );
        assert_eq!(
            budget(&l, &macbook(Some(10), 0.0), Said::Nothing).memory,
            48 * GB
        );
    }

    #[test]
    fn one_device_at_a_time() {
        let l = limits(Kind::Worker);
        let m = macbook(Some(900), 0.0);
        let mut emulator = req(0, Class::Waiting, 2, 4);
        emulator.device = true;
        let free = InUse::default();
        assert_eq!(
            decide(&l, &m, Said::Nothing, free, &emulator, 0),
            Decision::Admit { threads: 2 }
        );
        let taken = InUse {
            devices: 1,
            ..free
        };
        assert!(matches!(
            decide(&l, &m, Said::Nothing, taken, &emulator, 0),
            Decision::Wait { reason } if reason.contains("device slot")
        ));
        // Work that needs no device is not held up by it.
        assert!(waits_for_device(&l, taken, &emulator));
        assert!(!waits_for_device(&l, taken, &req(1, Class::Batch, 2, 4)));
        let none = Limits {
            device_slots: 0,
            ..l
        };
        assert!(matches!(
            decide(&none, &m, Said::Nothing, free, &emulator, 0),
            Decision::Refuse { .. }
        ));
    }

    #[test]
    fn a_job_is_charged_what_it_uses_when_that_is_more() {
        assert_eq!(charge(2, 2 * GB, None), (2, 2 * GB));
        assert_eq!(charge(2, 2 * GB, Some((0.3, GB))), (2, 2 * GB));
        assert_eq!(charge(2, 2 * GB, Some((7.6, 5 * GB))), (8, 5 * GB));
        assert!(!over(2, 2 * GB, None));
        assert!(!over(2, 2 * GB, Some((2.5, GB))));
        assert!(over(2, 2 * GB, Some((2.6, GB))));
        assert!(over(8, 2 * GB, Some((10.1, GB))));
        assert!(!over(8, 2 * GB, Some((10.0, GB))));
        assert!(over(2, 2 * GB, Some((1.0, 3 * GB))));
    }

    #[test]
    fn four_builds_do_not_all_start() {
        // The night of 14 November: four builds, each wanting every core.
        let l = limits(Kind::Workstation);
        let m = macbook(Some(10), 1.0);
        let mut in_use = InUse::default();
        let mut started = 0;
        for seq in 0..4 {
            match decide(
                &l,
                &m,
                Said::Nothing,
                in_use,
                &req(seq, Class::Batch, 16, 4),
                0,
            ) {
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
            in_use.threads, 8,
            "the offered half; a little load elsewhere comes off the kept half"
        );
    }

    #[test]
    fn small_jobs_share_the_budget() {
        let l = limits(Kind::Workstation);
        let m = macbook(Some(10), 0.0);
        let a = decide(
            &l,
            &m,
            Said::Nothing,
            InUse::default(),
            &req(0, Class::Batch, 4, 4),
            0,
        );
        assert_eq!(a, Decision::Admit { threads: 4 });
        let used = InUse {
            threads: 4,
            memory: 4 * GB,
            ..Default::default()
        };
        assert_eq!(
            decide(&l, &m, Said::Nothing, used, &req(1, Class::Batch, 4, 4), 0),
            Decision::Admit { threads: 4 }
        );
        let used = InUse {
            threads: 8,
            memory: 8 * GB,
            ..Default::default()
        };
        assert!(matches!(
            decide(&l, &m, Said::Nothing, used, &req(2, Class::Batch, 1, 1), 0),
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
            ..Default::default()
        };
        assert_eq!(
            decide(&l, &m, Said::Nothing, used, &req(1, Class::Batch, 8, 1), 0),
            Decision::Admit { threads: 4 }
        );
        let used = InUse {
            threads: 5,
            memory: GB,
            ..Default::default()
        };
        assert!(matches!(
            decide(&l, &m, Said::Nothing, used, &req(1, Class::Batch, 8, 1), 0),
            Decision::Wait { .. }
        ));
        // Never past the budget, whatever is asked.
        assert_eq!(
            decide(
                &l,
                &m,
                Said::Nothing,
                InUse::default(),
                &req(1, Class::Batch, 500, 1),
                0
            ),
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
            Said::Nothing,
            InUse::default(),
            &req(0, Class::Batch, 2, 1),
            0,
        );
        match d {
            Decision::Wait { reason } => assert!(reason.contains("not started through pressroom")),
            other => panic!("{other:?}"),
        }
        // A desktop's own load (window server, browser) is taken from the half kept for
        // the person, so it does not hold the queue.
        assert_eq!(
            decide(
                &l,
                &macbook(Some(10), 9.4),
                Said::Nothing,
                InUse {
                    threads: 2,
                    memory: GB,
                    ..Default::default()
                },
                &req(0, Class::Waiting, 2, 1),
                0
            ),
            Decision::Admit { threads: 2 }
        );
        // Once outside load passes the kept half, it comes off the offer.
        assert_eq!(free_threads(16, 8, 0, 8), 8);
        assert_eq!(free_threads(16, 8, 0, 11), 5);
        assert_eq!(free_threads(16, 8, 2, 11), 3);
        assert_eq!(free_threads(16, 8, 0, 16), 0);
        // With every thread offered there is no kept share to absorb it.
        assert_eq!(free_threads(16, 16, 0, 5), 11);
        // Processor use is what counts where it can be measured: a load average of 20
        // with a third of the processor in use (a desktop, 16 November) holds nothing.
        let mut desktop = macbook(Some(10), 20.3);
        desktop.busy = Some(5.8);
        assert_eq!(outside_load(&desktop, InUse::default()), 6);
        assert_eq!(
            decide(
                &l,
                &desktop,
                Said::Nothing,
                InUse::default(),
                &req(0, Class::Waiting, 4, 1),
                0
            ),
            Decision::Admit { threads: 4 }
        );
        desktop.busy = Some(15.6);
        match decide(
            &l,
            &desktop,
            Said::Nothing,
            InUse::default(),
            &req(0, Class::Waiting, 4, 1),
            0,
        ) {
            Decision::Wait { reason } => assert!(reason.contains("15.6 of 16 threads busy")),
            other => panic!("{other:?}"),
        }
        // Our own admitted work is not counted twice.
        let used = InUse {
            threads: 4,
            memory: GB,
            ..Default::default()
        };
        assert_eq!(outside_load(&macbook(Some(10), 4.4), used), 0);
        assert_eq!(outside_load(&macbook(Some(10), 9.0), used), 5);
    }

    #[test]
    fn memory_and_disk() {
        let l = limits(Kind::Workstation);
        let m = macbook(Some(10), 0.0);
        assert!(matches!(
            decide(
                &l,
                &m,
                Said::Nothing,
                InUse::default(),
                &req(0, Class::Batch, 2, 60),
                0
            ),
            Decision::Refuse { .. }
        ));
        let used = InUse {
            threads: 2,
            memory: 40 * GB,
            ..Default::default()
        };
        assert!(matches!(
            decide(&l, &m, Said::Nothing, used, &req(1, Class::Batch, 2, 10), 0),
            Decision::Wait { .. }
        ));
        let mut low = m.clone();
        low.disk_free = 5 * GB;
        match decide(
            &l,
            &low,
            Said::Nothing,
            InUse::default(),
            &req(2, Class::Batch, 2, 1),
            0,
        ) {
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
            decide(
                &l,
                &m,
                Said::Nothing,
                InUse::default(),
                &waiting[1],
                ahead_of(&waiting, 2)
            ),
            Decision::Wait { .. }
        ));
    }

    #[test]
    fn the_person_can_hold_the_machine_or_give_it_all() {
        let l = limits(Kind::Workstation);
        let m = macbook(Some(10), 0.0);
        let held = budget(&l, &m, Said::Hold);
        assert_eq!(held.threads, 0);
        assert!(held.person_present);
        assert_eq!(held.memory, 48 * GB, "the memory offer does not change");
        match decide(
            &l,
            &m,
            Said::Hold,
            InUse::default(),
            &req(0, Class::Waiting, 1, 1),
            0,
        ) {
            Decision::Wait { reason } => assert!(reason.contains("whole machine")),
            other => panic!("{other:?}"),
        }
        // Work that can never fit is still refused, not held.
        assert!(matches!(
            decide(
                &l,
                &m,
                Said::Hold,
                InUse::default(),
                &req(0, Class::Batch, 1, 60),
                0
            ),
            Decision::Refuse { .. }
        ));
        // Give: every thread, even while the person was just typing.
        let given = budget(&l, &m, Said::Give);
        assert_eq!(given.threads, 16);
        assert!(!given.person_present);
        assert_eq!(
            decide(
                &l,
                &m,
                Said::Give,
                InUse::default(),
                &req(0, Class::Batch, 16, 1),
                0
            ),
            Decision::Admit { threads: 16 }
        );
        // A worker can be held too.
        assert_eq!(budget(&limits(Kind::Worker), &m, Said::Hold).threads, 0);
    }

    #[test]
    fn what_the_person_said_ends() {
        // Said at 1000 for an hour.
        let (at, until) = (1000, 4600);
        assert!(still_stands(Said::Hold, at, until, 2000, Some(0)));
        assert!(!still_stands(Said::Hold, at, until, 4600, Some(900)));
        assert!(!still_stands(Said::Nothing, at, until, 1001, None));
        // Give survives the click that said it, and input in the minute after.
        assert!(still_stands(Said::Give, at, until, 1005, Some(0)));
        assert!(still_stands(Said::Give, at, until, 1060, Some(0)));
        // Away and quiet: it stands.
        assert!(still_stands(Said::Give, at, until, 3000, Some(1990)));
        // Input more than a minute after it was said: the person is back.
        assert!(!still_stands(Said::Give, at, until, 3000, Some(100)));
        // Where presence cannot be told, only the end ends it.
        assert!(still_stands(Said::Give, at, until, 3000, None));
    }
}
