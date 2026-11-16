import Foundation

/// The ops the menu bar item uses: `status` and `person-says`.
public struct PressroomClient: Sendable {
    public let socket: DaemonSocket

    public init(socket: DaemonSocket = DaemonSocket(path: DaemonSocket.defaultPath())) {
        self.socket = socket
    }

    public func status() async throws -> MachineStatus {
        let reply = try await ask(Request(op: "status"))
        return try MachineStatus.decode(reply)
    }

    /// Hold the machine, give it all, or go back to what is measured. `duration` is
    /// required for `hold` and `give` and ignored for `auto`.
    @discardableResult
    public func personSays(_ mode: PersonMode, for duration: Duration?, holder: String)
        async throws -> PersonSaysReply
    {
        let seconds = mode == .auto ? nil : duration.map { Int($0.components.seconds) }
        let request = PersonSaysRequest(mode: mode, forSecs: seconds, holder: holder)
        let reply = try await ask(request)
        do {
            return try JSONDecoder().decode(PersonSaysReply.self, from: reply)
        } catch {
            throw PressroomError.badReply(String(describing: error))
        }
    }

    /// The request as one line of JSON, keys sorted.
    static func encode(_ request: some Encodable) throws -> Data {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys, .withoutEscapingSlashes]
        return try encoder.encode(request)
    }

    private func ask(_ request: some Encodable) async throws -> Data {
        let reply = try await socket.exchange(Self.encode(request))
        if let refusal = try? JSONDecoder().decode(Refusal.self, from: reply), refusal.ok == false {
            throw PressroomError.refused(refusal.reason ?? "refused")
        }
        return reply
    }

    struct Request: Encodable {
        var op: String
    }

    struct PersonSaysRequest: Encodable {
        var op = "person-says"
        var mode: PersonMode
        var forSecs: Int?
        var holder: String
    }

    struct Refusal: Decodable {
        var ok: Bool?
        var reason: String?
    }
}

public struct PersonSaysReply: Decodable, Sendable, Equatable {
    public var mode: PersonMode
    /// Unix seconds; nil for `auto`.
    public var until: Int?
}
