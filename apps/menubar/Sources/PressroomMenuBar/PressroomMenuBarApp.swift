import AppKit
import PressroomKit
import SwiftUI

/// Pressroom in the menu bar: what this machine offers, what runs, what waits and why.
/// A client of the local daemon only; the daemon stays headless and does all the work.
@main
struct PressroomMenuBarApp: App {
    @State private var model: StatusModel
    @State private var loginItem: LoginItem
    @State private var daemonAgent: DaemonAgent

    init() {
        // No Dock icon, also when run outside the app bundle.
        NSApplication.shared.setActivationPolicy(.accessory)
        let client = PressroomClient()
        let model = StatusModel(client: client, holder: "user:\(NSUserName())")
        model.start()
        _model = State(initialValue: model)
        let loginItem = LoginItem()
        _loginItem = State(initialValue: loginItem)
        let daemonAgent = DaemonAgent(client: client)
        _daemonAgent = State(initialValue: daemonAgent)
        Task { await daemonAgent.applyDefault() }
        if CommandLine.arguments.contains("--window") {
            PanelWindow.open(model: model, loginItem: loginItem, daemonAgent: daemonAgent)
        }
    }

    var body: some Scene {
        MenuBarExtra {
            StatusPanel(model: model, loginItem: loginItem, daemonAgent: daemonAgent)
        } label: {
            MenuBarLabel(phase: model.phase)
        }
        .menuBarExtraStyle(.window)
    }
}
