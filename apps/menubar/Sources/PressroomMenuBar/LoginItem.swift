import Foundation
import Observation
import ServiceManagement

/// Whether the item starts at login. A setting with a default: on, applied once, the first
/// time the app runs from an Applications folder. A copy run from a build folder is never
/// registered, so a test run leaves no login item behind.
@MainActor
@Observable
final class LoginItem {
    private static let defaultApplied = "startAtLoginDefaultApplied"

    private(set) var status: SMAppService.Status
    private(set) var error: String?

    init(defaults: UserDefaults = .standard) {
        status = SMAppService.mainApp.status
        if isInstalled, !defaults.bool(forKey: Self.defaultApplied) {
            defaults.set(true, forKey: Self.defaultApplied)
            set(true)
        }
    }

    /// Only an installed app can be a login item.
    var isInstalled: Bool {
        let folder = Bundle.main.bundleURL.deletingLastPathComponent().path
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        return Bundle.main.bundleURL.pathExtension == "app"
            && (folder == "/Applications" || folder == home + "/Applications")
    }

    var isOn: Bool { status == .enabled || status == .requiresApproval }
    var needsApproval: Bool { status == .requiresApproval }

    func set(_ on: Bool) {
        do {
            if on {
                try SMAppService.mainApp.register()
            } else {
                try SMAppService.mainApp.unregister()
            }
            error = nil
        } catch {
            self.error = error.localizedDescription
        }
        status = SMAppService.mainApp.status
    }

    func openSettings() {
        SMAppService.openSystemSettingsLoginItems()
    }
}
