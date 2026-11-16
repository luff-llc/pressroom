import Foundation
import Observation
import PressroomKit
import ServiceManagement

/// Whether the daemon bundled in the app starts at login, as a LaunchAgent registered with
/// SMAppService. A setting with a default: on, applied once when an installed app first finds
/// no daemon answering. A Mac whose daemon was started another way keeps that one, and the
/// app never starts a second.
@MainActor
@Observable
final class DaemonAgent {
    static let plistName = "works.luff.pressroom.daemon.plist"
    private static let defaultApplied = "daemonAgentDefaultApplied"
    private static let registeredVersion = "daemonAgentRegisteredVersion"

    private(set) var status: SMAppService.Status
    private(set) var error: String?

    private let service = SMAppService.agent(plistName: DaemonAgent.plistName)
    private let client: PressroomClient
    private let defaults: UserDefaults

    init(client: PressroomClient, defaults: UserDefaults = .standard) {
        self.client = client
        self.defaults = defaults
        status = service.status
    }

    /// Only an installed app that carries the daemon can register it.
    var isAvailable: Bool { LoginItem.isInstalledApp && service.status != .notFound }
    var isOn: Bool { status == .enabled || status == .requiresApproval }
    var needsApproval: Bool { status == .requiresApproval }

    /// The app's version: an update replaces the agent's program, which must then be
    /// registered again or launchd may stop starting it.
    private var appVersion: String {
        Bundle.main.object(forInfoDictionaryKey: "CFBundleVersion") as? String ?? "0"
    }

    func applyDefault() async {
        guard isAvailable else { return }
        if !defaults.bool(forKey: Self.defaultApplied) {
            defaults.set(true, forKey: Self.defaultApplied)
            if await !daemonAnswers() { await set(true) }
            return
        }
        if isOn, defaults.string(forKey: Self.registeredVersion) != appVersion {
            do {
                try await service.unregister()
                try service.register()
                defaults.set(appVersion, forKey: Self.registeredVersion)
                error = nil
            } catch {
                self.error = error.localizedDescription
            }
            status = service.status
        }
    }

    func set(_ on: Bool) async {
        do {
            if on {
                if !isOn, await daemonAnswers() {
                    error = "A Pressroom daemon is already running on this Mac, started another way. Stop it first."
                    return
                }
                try service.register()
                defaults.set(appVersion, forKey: Self.registeredVersion)
            } else {
                try await service.unregister()
                defaults.removeObject(forKey: Self.registeredVersion)
            }
            error = nil
        } catch {
            self.error = error.localizedDescription
        }
        status = service.status
    }

    func openSettings() {
        SMAppService.openSystemSettingsLoginItems()
    }

    private func daemonAnswers() async -> Bool {
        do {
            _ = try await client.status()
            return true
        } catch PressroomError.notRunning {
            return false
        } catch {
            return true
        }
    }
}
