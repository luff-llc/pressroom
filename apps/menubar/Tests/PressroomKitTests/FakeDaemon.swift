import Darwin
import Foundation
import Synchronization

/// A stand-in for the daemon: listens on a scratch Unix socket, takes one connection,
/// records the request line, and answers as told. Lives on a thread of its own.
final class FakeDaemon: Sendable {
    enum Answer: Sendable {
        case line(String)
        case nothing
        case close
    }

    let path: String
    private let listener: Int32
    private let received = Mutex<String?>(nil)
    private let done = DispatchSemaphore(value: 0)

    init(answer: Answer) throws {
        let id = UUID().uuidString.prefix(8)
        let path = "/tmp/prmb-\(getpid())-\(id).sock"
        self.path = path
        unlink(path)
        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        withUnsafeMutableBytes(of: &address.sun_path) { $0.copyBytes(from: Array(path.utf8)) }
        let length = socklen_t(MemoryLayout<sockaddr_un>.size)
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        let bound = withUnsafePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { bind(fd, $0, length) }
        }
        guard fd >= 0, bound == 0, listen(fd, 4) == 0 else {
            throw POSIXError(.init(rawValue: errno) ?? .EIO)
        }
        listener = fd
        let thread = Thread { [self] in serve(answer) }
        thread.start()
    }

    private func serve(_ answer: Answer) {
        defer { done.signal() }
        let client = accept(listener, nil, nil)
        guard client >= 0 else { return }
        defer { close(client) }
        var line = [UInt8]()
        var byte: UInt8 = 0
        while read(client, &byte, 1) == 1, byte != 0x0A { line.append(byte) }
        received.withLock { $0 = String(decoding: line, as: UTF8.self) }
        switch answer {
        case .line(let text):
            let bytes = Array((text + "\n").utf8)
            _ = bytes.withUnsafeBytes { write(client, $0.baseAddress, $0.count) }
        case .nothing:
            // Say nothing until the client gives up and closes.
            while read(client, &byte, 1) == 1 {}
        case .close:
            break
        }
    }

    /// The request line, once the exchange is over.
    func request() -> String? {
        done.wait()
        return received.withLock { $0 }
    }

    deinit {
        close(listener)
        unlink(path)
    }
}
