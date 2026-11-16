import Foundation
import Testing

@testable import PressroomKit

@Suite("Socket client")
struct DaemonSocketTests {
    private func client(_ daemon: FakeDaemon, timeout: Duration = .seconds(2)) -> PressroomClient {
        PressroomClient(socket: DaemonSocket(path: daemon.path, timeout: timeout))
    }

    @Test func asksForStatusInOneLineAndReadsTheReply() async throws {
        let daemon = try FakeDaemon(answer: .line(Fixtures.busy.replacingOccurrences(of: "\n", with: "")))
        let status = try await client(daemon).status()
        #expect(status.name == "studio-a")
        let request = daemon.request()
        #expect(request == #"{"op":"status"}"#)
    }

    @Test func holdSendsTheModeTheLengthAndWho() async throws {
        let daemon = try FakeDaemon(answer: .line(#"{"ok":true,"mode":"hold","until":1763243600}"#))
        let reply = try await client(daemon).personSays(.hold, for: .seconds(3600), holder: "user:example")
        #expect(reply == PersonSaysReply(mode: .hold, until: 1_763_243_600))
        let request = daemon.request()
        #expect(request == #"{"forSecs":3600,"holder":"user:example","mode":"hold","op":"person-says"}"#)
    }

    @Test func autoSendsNoLength() async throws {
        let daemon = try FakeDaemon(answer: .line(#"{"ok":true,"mode":"auto","until":null}"#))
        let reply = try await client(daemon).personSays(.auto, for: .seconds(60), holder: "user:example")
        #expect(reply.until == nil)
        let request = daemon.request()
        #expect(request == #"{"holder":"user:example","mode":"auto","op":"person-says"}"#)
    }

    @Test func aRefusalComesBackInTheDaemonsWords() async throws {
        let daemon = try FakeDaemon(
            answer: .line(#"{"ok":false,"state":"refused","reason":"forSecs must be a whole number from 60 to 604800"}"#))
        await #expect(throws: PressroomError.refused("forSecs must be a whole number from 60 to 604800")) {
            try await client(daemon).personSays(.give, for: .seconds(5), holder: "user:example")
        }
    }

    @Test func noSocketMeansNotRunning() async {
        let path = "/tmp/prmb-\(getpid())-absent.sock"
        let client = PressroomClient(socket: DaemonSocket(path: path))
        await #expect(throws: PressroomError.notRunning(path: path)) {
            try await client.status()
        }
    }

    @Test func aSilentDaemonTimesOut() async throws {
        let daemon = try FakeDaemon(answer: .nothing)
        await #expect(throws: PressroomError.timedOut) {
            try await client(daemon, timeout: .milliseconds(300)).status()
        }
    }

    @Test func aDaemonThatHangsUpIsNotAReply() async throws {
        let daemon = try FakeDaemon(answer: .close)
        await #expect(throws: PressroomError.closed) {
            try await client(daemon).status()
        }
    }

    @Test func rubbishIsABadReply() async throws {
        let daemon = try FakeDaemon(answer: .line("not json"))
        await #expect(throws: PressroomError.self) {
            try await client(daemon).status()
        }
    }

    @Test func aPathTooLongForASocketIsSaidSo() async {
        let path = "/tmp/" + String(repeating: "p", count: 200)
        let client = PressroomClient(socket: DaemonSocket(path: path))
        await #expect(throws: PressroomError.pathTooLong(path)) {
            try await client.status()
        }
    }

    @Test func theDefaultPathFollowsPressroomHome() {
        let custom = DaemonSocket.defaultPath(environment: ["PRESSROOM_HOME": "/tmp/ph"], home: "/Users/example")
        #expect(custom == "/tmp/ph/sock")
        let usual = DaemonSocket.defaultPath(environment: [:], home: "/Users/example")
        #expect(usual == "/Users/example/.pressroom/sock")
    }
}
