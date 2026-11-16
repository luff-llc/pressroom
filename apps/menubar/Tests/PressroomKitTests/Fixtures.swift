import Foundation

/// Synthetic `status` replies, shaped like the daemon's. Made-up names and paths only.
enum Fixtures {
    /// A workstation with one job running, two waiting and one lease, its person present.
    static let busy = #"""
        {"protocol":"pressroom.local/1","name":"studio-a","os":"Darwin 27.0.0","arch":"aarch64",
        "kind":"workstation","threads":16,"memoryGb":64,"offeredThreads":8,"offeredMemoryGb":48,
        "personPresent":true,"personSays":null,"usedThreads":4,"usedMemoryGb":6.5,"load1":5.25,
        "outsideLoad":1,"diskFreeGb":116.15,"diskFloorGb":20,
        "toolchains":[{"name":"rustc","version":"rustc 1.99.1 (0000000 2026-10-13)"}],
        "managed":["/Users/example/.pressroom/lanes"],
        "running":[{"job":"j-1763240000-7","holder":"agent:builder-a","label":"cargo test --workspace",
          "class":"gate","threads":4,"memoryGb":6.5,"lane":"/Users/example/.pressroom/lanes/demo/rust-1.99.1/0",
          "pid":4242,"forSecs":95}],
        "waiting":[{"holder":"agent:checker-b","label":"bun test","class":"waiting","threads":4,
          "reason":"4 of 8 offered threads in use while its person is using it; this needs 4","forSecs":12},
          {"holder":"user:example","label":"xcodebuild test","class":"batch","threads":6,
          "reason":"1 ahead in the queue","forSecs":3}],
        "leases":[{"version":"pressroom.lease/1","id":"j-1763240000-7-lane","holder":"agent:builder-a",
          "kind":"cache","mode":"in-use","paths":["/Users/example/.pressroom/lanes/demo/rust-1.99.1/0"],
          "created":1763240000,"expires":1763844800,"pid":4242}],
        "unreadableLeases":[],"aFieldFromALaterVersion":{"ignored":true}}
        """#

    /// A worker its person has held, from a daemon that knows `person-says`.
    static let held = #"""
        {"protocol":"pressroom.local/1","name":"rack-b","os":"Linux 6.17","arch":"x86_64",
        "kind":"worker","threads":32,"memoryGb":31,"offeredThreads":0,"offeredMemoryGb":23,
        "personPresent":true,"personSays":{"mode":"hold","holder":"user:example","since":1763240000,
        "until":1763243600},"usedThreads":0,"usedMemoryGb":0,"load1":0.4,"outsideLoad":0,
        "diskFreeGb":400,"diskFloorGb":20,"toolchains":[],"managed":[],"running":[],
        "waiting":[{"holder":"agent:a","label":"cargo build","class":"batch","threads":8,
          "reason":"its person has asked for the whole machine","forSecs":40}],
        "leases":[],"unreadableLeases":[]}
        """#

    /// A daemon from before `person-says`: no `personSays` field. Disk is below its floor.
    static let olderAndLowOnDisk = #"""
        {"protocol":"pressroom.local/1","name":"laptop-c","os":"Darwin 27.0.0","arch":"aarch64",
        "kind":"workstation","threads":8,"memoryGb":16,"offeredThreads":4,"offeredMemoryGb":12,
        "personPresent":false,"usedThreads":0,"usedMemoryGb":0,"load1":0.1,"outsideLoad":0,
        "diskFreeGb":3.3,"diskFloorGb":20,"toolchains":[],"managed":[],"running":[],"waiting":[],
        "leases":[],"unreadableLeases":["broken.json"]}
        """#

    static func data(_ text: String) -> Data { Data(text.utf8) }

    /// `text` with `field` removed or replaced, for the negative cases.
    static func replacing(_ text: String, _ old: String, with new: String) -> String {
        text.replacingOccurrences(of: old, with: new)
    }
}
