import PressroomKit
import SwiftUI

/// What runs, what waits and why, and live leases. Long lists are cut, with a count.
struct JobListsView: View {
    let status: MachineStatus
    private let shown = 5

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            ListSection(title: "Running", count: status.running.count, empty: "Nothing running") {
                ForEach(status.running.prefix(shown)) { job in
                    RunningRow(job: job)
                }
                More(count: status.running.count - shown)
            }
            ListSection(title: "Waiting", count: status.waiting.count, empty: "Nothing waiting") {
                ForEach(Array(status.waiting.prefix(shown).enumerated()), id: \.offset) { _, job in
                    WaitingRow(job: job)
                }
                More(count: status.waiting.count - shown)
            }
            ListSection(title: "Leases", count: status.leases.count, empty: "No live leases") {
                ForEach(status.leases.prefix(shown)) { lease in
                    LeaseRow(lease: lease)
                }
                More(count: status.leases.count - shown)
            }
        }
    }
}

private struct ListSection<Content: View>: View {
    let title: String
    let count: Int
    let empty: String
    @ViewBuilder let content: Content

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(count > 0 ? "\(title) · \(count)" : title)
                .font(.caption.weight(.semibold))
                .textCase(.uppercase)
                .foregroundStyle(.secondary)
            if count == 0 {
                Text(empty)
                    .font(.callout)
                    .foregroundStyle(.tertiary)
            } else {
                content
            }
        }
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("pressroom.\(title.lowercased())")
    }
}

private struct More: View {
    let count: Int
    var body: some View {
        if count > 0 {
            Text("and \(count) more · pressroom status")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
    }
}

struct RunningRow: View {
    let job: MachineStatus.RunningJob

    var body: some View {
        VStack(alignment: .leading, spacing: 1) {
            Text(job.label)
                .font(.callout)
                .lineLimit(1)
                .truncationMode(.middle)
            Text("\(job.holder) · \(threads) · \(job.class) · for \(span(job.forSecs))")
                .font(.caption)
                .foregroundStyle(.secondary)
                .lineLimit(1)
            if let flag {
                Text(flag)
                    .font(.caption)
                    .foregroundStyle(.orange)
                    .lineLimit(1)
            }
        }
        .accessibilityElement(children: .combine)
    }

    /// Use against grant, once measured: "4 threads, using 3.2".
    private var threads: String {
        let granted = job.threads == 1 ? "1 thread" : "\(job.threads) threads"
        guard let use = job.useThreads else { return granted }
        return "\(granted), using \(use.formatted(.number.precision(.fractionLength(1))))"
    }

    /// What is worth the person's eye: over its grant, or held back to it.
    private var flag: String? {
        if job.over == true { return "Over its grant" }
        if job.held == true { return "Held to its grant" }
        return nil
    }
}

struct WaitingRow: View {
    let job: MachineStatus.WaitingJob

    var body: some View {
        VStack(alignment: .leading, spacing: 1) {
            Text(job.label)
                .font(.callout)
                .lineLimit(1)
                .truncationMode(.middle)
            Text(job.reason)
                .font(.caption)
                .foregroundStyle(.orange)
                .lineLimit(2)
                .fixedSize(horizontal: false, vertical: true)
            Text("\(job.holder) · \(job.threads) threads · \(job.class) · in line \(span(job.forSecs))")
                .font(.caption)
                .foregroundStyle(.secondary)
                .lineLimit(1)
        }
        .accessibilityElement(children: .combine)
    }
}

struct LeaseRow: View {
    let lease: MachineStatus.Lease

    var body: some View {
        VStack(alignment: .leading, spacing: 1) {
            Text(paths)
                .font(.callout.monospaced())
                .lineLimit(1)
                .truncationMode(.head)
            Text("\(lease.kind) (\(lease.mode)) · \(lease.holder) · \(lasts)")
                .font(.caption)
                .foregroundStyle(.secondary)
                .lineLimit(1)
        }
        .accessibilityElement(children: .combine)
    }

    private var paths: String {
        let first = lease.paths.first.map { ($0 as NSString).abbreviatingWithTildeInPath } ?? "-"
        return lease.paths.count > 1 ? "\(first) +\(lease.paths.count - 1)" : first
    }

    private var lasts: String {
        if lease.pid != nil { return "while its process lives" }
        let left = max(0, lease.expires - Int(Date.now.timeIntervalSince1970))
        return "for \(span(left)) more"
    }
}

/// A span of seconds in at most two units: "1 min, 35 sec".
func span(_ seconds: Int) -> String {
    Duration.seconds(seconds).formatted(
        .units(allowed: [.days, .hours, .minutes, .seconds], width: .abbreviated, maximumUnitCount: 2))
}
