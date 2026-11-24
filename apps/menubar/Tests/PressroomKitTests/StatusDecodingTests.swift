import Foundation
import Testing

@testable import PressroomKit

@Suite("Status decoding")
struct StatusDecodingTests {
    @Test func decodesABusyWorkstation() throws {
        let status = try MachineStatus.decode(Fixtures.data(Fixtures.busy))
        #expect(status.name == "studio-a")
        #expect(status.kind == "workstation")
        #expect(status.offeredThreads == 8)
        #expect(status.usedMemoryGb == 6.5)
        #expect(status.personPresent)
        #expect(status.personSays == nil)
        #expect(status.running.count == 2)
        let job = try #require(status.running.first)
        #expect(job.useThreads == nil)
        #expect(job.over == nil)
        let measured = try #require(status.running.last)
        #expect(measured.useThreads == 4.5)
        #expect(measured.over == true)
        #expect(measured.device == true)
        #expect(job.class == "gate")
        #expect(job.lane == "/Users/example/.pressroom/lanes/demo/rust-1.99.1/0")
        #expect(job.pid == 4242)
        let reasons = status.waiting.map(\.reason)
        #expect(reasons.count == 2)
        #expect(reasons.last == "1 ahead in the queue")
        let lease = try #require(status.leases.first)
        #expect(lease.kind == "cache")
        #expect(lease.expires == 1_763_844_800)
        #expect(status.freeThreads == 3)
        #expect(MachineState(status) == .working)
    }

    @Test func decodesWhatThePersonSaid() throws {
        let status = try MachineStatus.decode(Fixtures.data(Fixtures.held))
        let says = try #require(status.personSays)
        #expect(says.mode == .hold)
        #expect(says.until == 1_763_243_600)
        #expect(status.offeredThreads == 0)
        #expect(status.freeThreads == 0)
        #expect(MachineState(status) == .held)
    }

    @Test func readsAnOlderDaemonAndSaysWhatNeedsThePerson() throws {
        let status = try MachineStatus.decode(Fixtures.data(Fixtures.olderAndLowOnDisk))
        #expect(status.personSays == nil)
        #expect(status.diskBelowFloor)
        #expect(MachineState(status) == .needsYou)
        let why = try #require(status.needsYouWhy)
        #expect(why.contains("3.3 GB free"))
    }

    @Test func refusesAnotherProtocol() {
        let other = Fixtures.replacing(Fixtures.busy, "pressroom.local/1", with: "pressroom.local/2")
        #expect(throws: PressroomError.unsupportedProtocol("pressroom.local/2")) {
            try MachineStatus.decode(Fixtures.data(other))
        }
    }

    @Test func aMissingFieldIsAnErrorNotADefault() {
        let missing = Fixtures.replacing(Fixtures.busy, #""offeredThreads":8,"#, with: "")
        #expect(throws: PressroomError.self) {
            try MachineStatus.decode(Fixtures.data(missing))
        }
        let wrongType = Fixtures.replacing(Fixtures.busy, #""threads":16"#, with: #""threads":"16""#)
        #expect(throws: PressroomError.self) {
            try MachineStatus.decode(Fixtures.data(wrongType))
        }
    }

    @Test func statesFollowGalleysOrder() throws {
        var status = try MachineStatus.decode(Fixtures.data(Fixtures.busy))
        status.running = []
        #expect(MachineState(status) == .inUse)
        status.personPresent = false
        #expect(MachineState(status) == .idle)
        status.personSays = .init(mode: .give, holder: "user:example", since: 0, until: 1)
        #expect(MachineState(status) == .given)
        status.diskFreeGb = 1
        #expect(MachineState(status) == .needsYou)
    }
}
