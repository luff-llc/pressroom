import Foundation
import Observation
import PressroomKit

/// Asks the daemon for its status every few seconds, and passes on what the person says.
@MainActor
@Observable
final class StatusModel {
    enum Phase: Equatable {
        case starting
        case live(MachineStatus)
        case notRunning
        case failed(String)
    }

    private(set) var phase: Phase = .starting
    private(set) var updated: Date?
    /// The last thing that went wrong when the person said something, in words.
    private(set) var actionError: String?
    private(set) var saying = false

    private let client: PressroomClient
    private let holder: String
    private let every: Duration
    private var loop: Task<Void, Never>?

    init(client: PressroomClient, holder: String, every: Duration = .seconds(3)) {
        self.client = client
        self.holder = holder
        self.every = every
    }

    func start() {
        guard loop == nil else { return }
        let every = every
        loop = Task { [weak self] in
            while !Task.isCancelled {
                await self?.refresh()
                try? await Task.sleep(for: every)
            }
        }
    }

    func refresh() async {
        do {
            phase = .live(try await client.status())
            updated = .now
        } catch PressroomError.notRunning {
            phase = .notRunning
            updated = .now
        } catch {
            phase = .failed(error.localizedDescription)
            updated = .now
        }
    }

    func say(_ mode: PersonMode, for duration: Duration? = nil) async {
        saying = true
        defer { saying = false }
        do {
            try await client.personSays(mode, for: duration, holder: holder)
            actionError = nil
        } catch {
            actionError = error.localizedDescription
        }
        await refresh()
    }
}
