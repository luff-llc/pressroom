import PressroomKit
import SwiftUI

/// Whether the machine thinks its person is here, and what the person can say:
/// hold it ("I need the whole machine"), give it ("take it all, I am away"), or go back
/// to what is measured. Rules 19 to 21 of the local contract.
struct PersonView: View {
    let status: MachineStatus
    let model: StatusModel

    private static let holdFor: [(String, Duration)] = [
        ("30 minutes", .seconds(30 * 60)), ("1 hour", .seconds(3600)),
        ("2 hours", .seconds(2 * 3600)), ("4 hours", .seconds(4 * 3600)),
    ]
    private static let giveFor: [(String, Duration)] = [
        ("1 hour", .seconds(3600)), ("4 hours", .seconds(4 * 3600)),
        ("8 hours", .seconds(8 * 3600)), ("12 hours", .seconds(12 * 3600)),
    ]

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Label(presence, systemImage: symbol)
                .font(.subheadline)
                .accessibilityIdentifier("pressroom.presence")
            HStack(spacing: 8) {
                if status.personSays == nil {
                    Menu("Hold the machine") {
                        ForEach(Self.holdFor, id: \.0) { title, duration in
                            Button("For \(title)") { Task { await model.say(.hold, for: duration) } }
                        }
                    }
                    .help("Nothing new starts. Work already running carries on.")
                    .accessibilityIdentifier("pressroom.hold")
                    Menu("Take it all") {
                        ForEach(Self.giveFor, id: \.0) { title, duration in
                            Button("For \(title)") { Task { await model.say(.give, for: duration) } }
                        }
                    }
                    .help("Every thread is offered until then, or until you are back at the machine.")
                    .accessibilityIdentifier("pressroom.give")
                } else {
                    Button("Back to automatic", systemImage: "arrow.uturn.backward") {
                        Task { await model.say(.auto) }
                    }
                    .accessibilityIdentifier("pressroom.auto")
                }
            }
            .controlSize(.small)
            .fixedSize()
            .disabled(model.saying)
            if let error = model.actionError {
                Text(error)
                    .font(.caption)
                    .foregroundStyle(.red)
            }
        }
    }

    private var presence: String {
        if let says = status.personSays {
            let until = Date(timeIntervalSince1970: TimeInterval(says.until))
                .formatted(.dateTime.hour().minute())
            return switch says.mode {
            case .hold: "Held for you until \(until): nothing new starts"
            case .give: "Given away until \(until): every thread offered"
            case .auto: "Automatic"
            }
        }
        if status.kind != "workstation" {
            return "A worker: no one at the keyboard is counted"
        }
        return status.personPresent ? "You are using this machine" : "You are away from this machine"
    }

    private var symbol: String {
        switch status.personSays?.mode {
        case .hold: "hand.raised.fill"
        case .give: "moon.fill"
        case .auto, nil: status.personPresent ? "person.fill" : "person.slash"
        }
    }
}
