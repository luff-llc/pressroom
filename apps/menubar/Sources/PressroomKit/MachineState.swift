import Foundation

/// One word for the machine, the first that holds. The same order as Galley's Machines
/// view (`contracts/machine/README.md` in Galley), with what the person said in between.
public enum MachineState: String, Sendable, Equatable {
    /// Disk below its floor, or a lease file that cannot be read.
    case needsYou = "needs you"
    /// The person asked for the whole machine.
    case held
    /// Something is running.
    case working
    /// The person gave the machine away.
    case given
    /// A workstation whose person is present.
    case inUse = "in use"
    case idle

    public init(_ status: MachineStatus) {
        if status.diskBelowFloor || !status.unreadableLeases.isEmpty {
            self = .needsYou
        } else if status.personSays?.mode == .hold {
            self = .held
        } else if !status.running.isEmpty {
            self = .working
        } else if status.personSays?.mode == .give {
            self = .given
        } else if status.kind == "workstation" && status.personPresent {
            self = .inUse
        } else {
            self = .idle
        }
    }
}

extension MachineStatus {
    public var diskBelowFloor: Bool { diskFreeGb < diskFloorGb }

    /// Threads free for new work now: the offer less what is admitted and outside load.
    public var freeThreads: Int { max(0, offeredThreads - usedThreads - outsideLoad) }

    /// Why nothing more would start, in plain words, or nil.
    public var needsYouWhy: String? {
        if diskBelowFloor {
            let free = diskFreeGb.formatted(.number.precision(.fractionLength(1)))
            let floor = diskFloorGb.formatted(.number.precision(.fractionLength(0)))
            return "Disk is below its floor: \(free) GB free, floor \(floor) GB."
        }
        if let first = unreadableLeases.first {
            return "A lease file cannot be read (\(first)); cleanup is refused until it is fixed."
        }
        return nil
    }
}
