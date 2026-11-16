import AppKit
import SwiftUI

/// `--window`: the same panel in an ordinary window, for a menu bar that is hidden or full,
/// and for taking a picture of the panel alone (`screencapture -l <window number>`, which
/// it prints on standard error).
@MainActor
enum PanelWindow {
    private static var window: NSWindow?

    static func open(model: StatusModel, loginItem: LoginItem, daemonAgent: DaemonAgent) {
        Task { @MainActor in
            let window = NSWindow(
                contentViewController: NSHostingController(
                    rootView: StatusPanel(model: model, loginItem: loginItem, daemonAgent: daemonAgent)))
            window.title = "Pressroom"
            window.styleMask = [.titled, .closable]
            window.isReleasedWhenClosed = false
            window.center()
            window.makeKeyAndOrderFront(nil)
            NSApplication.shared.activate()
            Self.window = window
            FileHandle.standardError.write(Data("pressroom: panel window \(window.windowNumber)\n".utf8))
        }
    }
}
