import PressroomKit
import SwiftUI

/// The icon in the menu bar. It changes with the machine's state so a glance is enough.
struct MenuBarLabel: View {
    let phase: StatusModel.Phase

    var body: some View {
        Image(systemName: symbol)
            .accessibilityLabel(Text("Pressroom: \(words)"))
    }

    private var symbol: String {
        switch phase {
        case .starting: "square.stack.3d.up"
        case .notRunning: "square.stack.3d.up.slash"
        case .failed: "exclamationmark.triangle"
        case .live(let status):
            switch MachineState(status) {
            case .needsYou: "exclamationmark.triangle.fill"
            case .held: "hand.raised.fill"
            case .working: "square.stack.3d.up.fill"
            case .given: "moon.fill"
            case .inUse, .idle: "square.stack.3d.up"
            }
        }
    }

    private var words: String {
        switch phase {
        case .starting: "starting"
        case .notRunning: "the daemon is not running"
        case .failed(let why): why
        case .live(let status): MachineState(status).rawValue
        }
    }
}
