import Foundation

public enum PressroomError: Error, Equatable, Sendable {
    /// No socket, or nothing listening on it: the daemon is not running.
    case notRunning(path: String)
    /// The socket path does not fit in a Unix socket address.
    case pathTooLong(String)
    /// The daemon said no, in words.
    case refused(String)
    /// The daemon did not answer in time.
    case timedOut
    /// The daemon closed the connection before a whole line.
    case closed
    /// A reply that is not what the contract says.
    case badReply(String)
    /// A daemon speaking another version of the contract.
    case unsupportedProtocol(String)
    /// A system call failed.
    case system(call: String, errno: Int32)
}

extension PressroomError: LocalizedError {
    public var errorDescription: String? {
        switch self {
        case .notRunning:
            "The Pressroom daemon is not running."
        case .pathTooLong(let path):
            "The socket path is too long: \(path)"
        case .refused(let reason):
            "Pressroom refused: \(reason)"
        case .timedOut:
            "The daemon did not answer in time."
        case .closed:
            "The daemon closed the connection."
        case .badReply(let why):
            "The daemon's reply could not be read: \(why)"
        case .unsupportedProtocol(let version):
            "The daemon speaks \(version); this item speaks \(MachineStatus.protocolVersion)."
        case .system(let call, let errno):
            "\(call) failed: \(String(cString: strerror(errno)))"
        }
    }
}
