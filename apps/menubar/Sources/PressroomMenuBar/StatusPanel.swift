import AppKit
import PressroomKit
import SwiftUI

/// The window under the menu bar icon.
struct StatusPanel: View {
    let model: StatusModel
    let loginItem: LoginItem

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            switch model.phase {
            case .starting:
                ProgressView("Asking the daemon…")
                    .controlSize(.small)
            case .notRunning:
                NotRunningView()
            case .failed(let why):
                Label(why, systemImage: "exclamationmark.triangle")
                    .foregroundStyle(.orange)
            case .live(let status):
                LiveStatusView(status: status, model: model)
            }
            Divider()
            FooterView(updated: model.updated, loginItem: loginItem)
        }
        .padding(14)
        .frame(width: 360)
    }
}

/// No daemon: say so, and how to start one. The item never starts or stands in for it.
struct NotRunningView: View {
    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Label("The Pressroom daemon is not running", systemImage: "square.stack.3d.up.slash")
                .font(.headline)
            Text("Nothing started through pressroom runs until it is back. Start it with:")
                .font(.callout)
                .foregroundStyle(.secondary)
            Text("pressroom daemon")
                .font(.callout.monospaced())
                .textSelection(.enabled)
        }
        .accessibilityIdentifier("pressroom.notRunning")
    }
}

struct LiveStatusView: View {
    let status: MachineStatus
    let model: StatusModel

    var body: some View {
        let state = MachineState(status)
        HeaderView(status: status, state: state)
        if let why = status.needsYouWhy {
            Label(why, systemImage: "exclamationmark.triangle.fill")
                .font(.callout)
                .foregroundStyle(.red)
        }
        CapacityView(status: status)
        PersonView(status: status, model: model)
        Divider()
        JobListsView(status: status)
    }
}

struct HeaderView: View {
    let status: MachineStatus
    let state: MachineState

    var body: some View {
        HStack(alignment: .firstTextBaseline) {
            VStack(alignment: .leading, spacing: 2) {
                Text(status.name)
                    .font(.headline)
                Text("\(status.kind) · \(status.os) · \(status.arch)")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Spacer()
            Text(state.rawValue)
                .font(.caption.weight(.semibold))
                .padding(.horizontal, 8)
                .padding(.vertical, 3)
                .foregroundStyle(stateColor)
                .background(stateColor.opacity(0.15), in: .capsule)
                .accessibilityIdentifier("pressroom.state")
        }
    }

    private var stateColor: Color {
        switch state {
        case .needsYou: .red
        case .held: .orange
        case .working: .green
        case .given: .indigo
        case .inUse, .idle: .secondary
        }
    }
}

struct FooterView: View {
    let updated: Date?
    let loginItem: LoginItem

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            if loginItem.isInstalled {
                Toggle(
                    "Start at login",
                    isOn: Binding(get: { loginItem.isOn }, set: { loginItem.set($0) })
                )
                .toggleStyle(.checkbox)
                .accessibilityIdentifier("pressroom.startAtLogin")
                if loginItem.needsApproval {
                    Button("Allow it in Login Items…") { loginItem.openSettings() }
                        .buttonStyle(.link)
                        .font(.caption)
                }
                if let error = loginItem.error {
                    Text(error).font(.caption).foregroundStyle(.red)
                }
            }
            HStack {
                if let updated {
                    Text("Updated \(updated, format: .dateTime.hour().minute().second())")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer()
                Button("Quit", systemImage: "power") { NSApplication.shared.terminate(nil) }
                    .buttonStyle(.borderless)
                    .labelStyle(.titleAndIcon)
                    .keyboardShortcut("q")
                    .accessibilityIdentifier("pressroom.quit")
            }
        }
    }
}
