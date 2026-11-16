import Foundation

/// What the daemon's `status` op answers (contracts/local-v1.md, "What `status` answers").
/// Unknown fields are ignored; a missing required field is an error, never a default.
public struct MachineStatus: Decodable, Sendable, Equatable {
    public static let protocolVersion = "pressroom.local/1"

    public var name: String
    public var kind: String
    public var os: String
    public var arch: String
    public var threads: Int
    public var memoryGb: Double
    public var offeredThreads: Int
    public var offeredMemoryGb: Double
    public var personPresent: Bool
    /// Absent from daemons older than the `person-says` op.
    public var personSays: PersonSays?
    public var usedThreads: Int
    public var usedMemoryGb: Double
    public var load1: Double
    public var outsideLoad: Int
    public var diskFreeGb: Double
    public var diskFloorGb: Double
    public var toolchains: [Toolchain]
    public var running: [RunningJob]
    public var waiting: [WaitingJob]
    public var leases: [Lease]
    public var unreadableLeases: [String]

    public struct PersonSays: Decodable, Sendable, Equatable {
        public var mode: PersonMode
        public var holder: String
        public var since: Int
        public var until: Int
    }

    public struct Toolchain: Decodable, Sendable, Equatable, Identifiable {
        public var name: String
        public var version: String
        public var id: String { name }
    }

    public struct RunningJob: Decodable, Sendable, Equatable, Identifiable {
        public var job: String
        public var holder: String
        public var label: String
        public var `class`: String
        public var threads: Int
        public var memoryGb: Double
        public var lane: String?
        public var pid: Int?
        public var forSecs: Int
        public var id: String { job }
    }

    public struct WaitingJob: Decodable, Sendable, Equatable {
        public var holder: String
        public var label: String
        public var `class`: String
        public var threads: Int
        public var reason: String
        public var forSecs: Int
    }

    public struct Lease: Decodable, Sendable, Equatable, Identifiable {
        public var id: String
        public var holder: String
        public var kind: String
        public var mode: String
        public var paths: [String]
        public var created: Int
        public var expires: Int
        public var pid: Int?
    }

    private enum CodingKeys: String, CodingKey {
        case `protocol`, name, kind, os, arch, threads, memoryGb, offeredThreads,
            offeredMemoryGb, personPresent, personSays, usedThreads, usedMemoryGb, load1,
            outsideLoad, diskFreeGb, diskFloorGb, toolchains, running, waiting, leases,
            unreadableLeases
    }

    public init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let version = try c.decode(String.self, forKey: .protocol)
        guard version == Self.protocolVersion else {
            throw PressroomError.unsupportedProtocol(version)
        }
        name = try c.decode(String.self, forKey: .name)
        kind = try c.decode(String.self, forKey: .kind)
        os = try c.decode(String.self, forKey: .os)
        arch = try c.decode(String.self, forKey: .arch)
        threads = try c.decode(Int.self, forKey: .threads)
        memoryGb = try c.decode(Double.self, forKey: .memoryGb)
        offeredThreads = try c.decode(Int.self, forKey: .offeredThreads)
        offeredMemoryGb = try c.decode(Double.self, forKey: .offeredMemoryGb)
        personPresent = try c.decode(Bool.self, forKey: .personPresent)
        personSays = try c.decodeIfPresent(PersonSays.self, forKey: .personSays)
        usedThreads = try c.decode(Int.self, forKey: .usedThreads)
        usedMemoryGb = try c.decode(Double.self, forKey: .usedMemoryGb)
        load1 = try c.decode(Double.self, forKey: .load1)
        outsideLoad = try c.decode(Int.self, forKey: .outsideLoad)
        diskFreeGb = try c.decode(Double.self, forKey: .diskFreeGb)
        diskFloorGb = try c.decode(Double.self, forKey: .diskFloorGb)
        toolchains = try c.decode([Toolchain].self, forKey: .toolchains)
        running = try c.decode([RunningJob].self, forKey: .running)
        waiting = try c.decode([WaitingJob].self, forKey: .waiting)
        leases = try c.decode([Lease].self, forKey: .leases)
        unreadableLeases = try c.decode([String].self, forKey: .unreadableLeases)
    }

    public static func decode(_ data: Data) throws -> MachineStatus {
        do {
            return try JSONDecoder().decode(MachineStatus.self, from: data)
        } catch let error as PressroomError {
            throw error
        } catch {
            throw PressroomError.badReply(String(describing: error))
        }
    }
}

/// What the person can say about the machine (rules 19 to 21).
public enum PersonMode: String, Codable, Sendable, CaseIterable {
    /// "I need the whole machine": nothing new starts.
    case hold
    /// "Take it all, I am away": every thread is offered.
    case give
    /// Back to what is measured.
    case auto
}
