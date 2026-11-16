import Darwin
import Foundation

/// One request line to the daemon's Unix socket and one reply line back
/// (contracts/local-v1.md, "The socket"). Blocking calls run on a queue of their own,
/// never on the caller's executor.
public struct DaemonSocket: Sendable {
    public let path: String
    public let timeout: Duration

    /// The most a reply may weigh. `status` with many leases is the largest.
    static let maxReply = 4 << 20

    public init(path: String, timeout: Duration = .seconds(3)) {
        self.path = path
        self.timeout = timeout
    }

    /// `$PRESSROOM_HOME/sock`, else `~/.pressroom/sock`.
    public static func defaultPath(
        environment: [String: String] = ProcessInfo.processInfo.environment,
        home: String = NSHomeDirectory()
    ) -> String {
        if let custom = environment["PRESSROOM_HOME"], !custom.isEmpty {
            return (custom as NSString).appendingPathComponent("sock")
        }
        return (home as NSString).appendingPathComponent(".pressroom/sock")
    }

    private static let queue = DispatchQueue(
        label: "works.luff.pressroom.socket", qos: .utility, attributes: .concurrent)

    /// Sends `request` and a newline, and returns the first line of the reply without it.
    public func exchange(_ request: Data) async throws -> Data {
        let path = path
        let timeout = timeout
        return try await withCheckedThrowingContinuation { continuation in
            Self.queue.async {
                continuation.resume(
                    with: Result {
                        try Self.blockingExchange(path: path, request: request, timeout: timeout)
                    })
            }
        }
    }

    static func blockingExchange(path: String, request: Data, timeout: Duration) throws -> Data {
        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        let bytes = Array(path.utf8)
        let capacity = MemoryLayout.size(ofValue: address.sun_path)
        guard !bytes.isEmpty, bytes.count < capacity else {
            throw PressroomError.pathTooLong(path)
        }
        withUnsafeMutableBytes(of: &address.sun_path) { $0.copyBytes(from: bytes) }
        let length = socklen_t(MemoryLayout<sockaddr_un>.size)
        address.sun_len = UInt8(length)

        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw PressroomError.system(call: "socket", errno: errno) }
        defer { close(fd) }
        var on: Int32 = 1
        setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &on, socklen_t(MemoryLayout<Int32>.size))
        var limit = timeval(timeout)
        let timevalSize = socklen_t(MemoryLayout<timeval>.size)
        setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &limit, timevalSize)
        setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &limit, timevalSize)

        let connected = withUnsafePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { connect(fd, $0, length) }
        }
        if connected != 0 {
            let error = errno
            switch error {
            case ENOENT, ECONNREFUSED, ENOTSOCK:
                throw PressroomError.notRunning(path: path)
            default:
                throw PressroomError.system(call: "connect", errno: error)
            }
        }

        var line = request
        line.append(0x0A)
        try line.withUnsafeBytes { raw in
            guard let base = raw.baseAddress else { return }
            var offset = 0
            while offset < raw.count {
                let written = write(fd, base + offset, raw.count - offset)
                if written < 0 {
                    let error = errno
                    if error == EINTR { continue }
                    if error == EAGAIN { throw PressroomError.timedOut }
                    throw PressroomError.system(call: "write", errno: error)
                }
                offset += written
            }
        }

        let deadline = ContinuousClock.now + timeout
        var reply = Data()
        var buffer = [UInt8](repeating: 0, count: 16 * 1024)
        while true {
            if ContinuousClock.now >= deadline { throw PressroomError.timedOut }
            let count = buffer.withUnsafeMutableBytes { read(fd, $0.baseAddress, $0.count) }
            if count < 0 {
                let error = errno
                if error == EINTR { continue }
                if error == EAGAIN { throw PressroomError.timedOut }
                throw PressroomError.system(call: "read", errno: error)
            }
            if count == 0 { throw PressroomError.closed }
            let chunk = buffer[0..<count]
            if let newline = chunk.firstIndex(of: 0x0A) {
                reply.append(contentsOf: chunk[..<newline])
                return reply
            }
            reply.append(contentsOf: chunk)
            if reply.count > maxReply { throw PressroomError.badReply("reply too long") }
        }
    }
}

extension timeval {
    init(_ duration: Duration) {
        let (seconds, attoseconds) = duration.components
        self.init(
            tv_sec: Int(max(0, seconds)),
            tv_usec: Int32(max(0, attoseconds) / 1_000_000_000_000))
    }
}
