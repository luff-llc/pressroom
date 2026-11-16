import PressroomKit
import SwiftUI

/// Threads and memory in use against what is offered, and disk against its floor.
struct CapacityView: View {
    let status: MachineStatus

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            MeterRow(
                title: "Threads",
                value: Double(status.usedThreads + status.outsideLoad),
                total: Double(status.offeredThreads),
                detail: threadsDetail,
                note: outsideNote
            )
            .accessibilityIdentifier("pressroom.threads")
            MeterRow(
                title: "Memory",
                value: status.usedMemoryGb,
                total: status.offeredMemoryGb,
                detail: "\(gb(status.usedMemoryGb)) of \(gb(status.offeredMemoryGb)) GB offered · \(gb(status.memoryGb)) GB on the machine",
                note: nil
            )
            .accessibilityIdentifier("pressroom.memory")
            HStack(spacing: 6) {
                Image(systemName: status.diskBelowFloor ? "externaldrive.trianglebadge.exclamationmark" : "internaldrive")
                    .foregroundStyle(status.diskBelowFloor ? .red : .secondary)
                Text("Disk")
                    .font(.subheadline.weight(.medium))
                Spacer()
                Text("\(gb(status.diskFreeGb)) GB free · floor \(gb(status.diskFloorGb)) GB")
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(status.diskBelowFloor ? .red : .secondary)
            }
            .accessibilityElement(children: .combine)
            .accessibilityIdentifier("pressroom.disk")
        }
    }

    private var threadsDetail: String {
        if status.offeredThreads == 0 {
            return "none offered · \(status.threads) on the machine"
        }
        return "\(status.usedThreads) of \(status.offeredThreads) offered · \(status.threads) on the machine"
    }

    private var outsideNote: String? {
        guard status.outsideLoad > 0 else { return nil }
        let load = status.load1.formatted(.number.precision(.fractionLength(1)))
        return "Load \(load): about \(status.outsideLoad) threads not started through pressroom"
    }

    private func gb(_ value: Double) -> String {
        value.formatted(.number.precision(.fractionLength(value < 10 && value != value.rounded() ? 1 : 0)))
    }
}

struct MeterRow: View {
    let title: String
    let value: Double
    let total: Double
    let detail: String
    let note: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            HStack(alignment: .firstTextBaseline) {
                Text(title)
                    .font(.subheadline.weight(.medium))
                Spacer()
                Text(detail)
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(.secondary)
                    .contentTransition(.numericText())
            }
            ProgressView(value: min(value, max(total, 0.0001)), total: max(total, 0.0001))
                .progressViewStyle(.linear)
                .tint(value > total ? .orange : .accentColor)
                .accessibilityLabel(Text(title))
            if let note {
                Text(note)
                    .font(.caption)
                    .foregroundStyle(.orange)
            }
        }
        .accessibilityElement(children: .combine)
    }
}
