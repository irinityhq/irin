import SwiftUI

struct ContentView: View {
    @StateObject private var model = WarRoomModel()
    @State private var showingSettings = false

    var body: some View {
        NavigationStack {
            ZStack(alignment: .top) {
                if model.config.webBase.isEmpty {
                    ContentUnavailableView {
                        Label("Connect to IRIN", systemImage: "network")
                    } description: {
                        Text("Enter the private HTTPS address shown by IRIN on your Mac.")
                    } actions: {
                        Button("Open Connection Settings") {
                            showingSettings = true
                        }
                        .buttonStyle(.borderedProminent)
                    }
                } else {
                    WarRoomWebView(model: model)
                        .ignoresSafeArea(edges: .bottom)
                }

                VStack(spacing: 8) {
                    if case .blocked(let url) = model.webState {
                        Banner(text: "Blocked untrusted navigation: \(url)", systemImage: "hand.raised.fill", tone: .warning)
                    }
                    if case .failed(let message) = model.webState {
                        Banner(text: "Web load failed: \(message)", systemImage: "exclamationmark.triangle.fill", tone: .danger)
                    }
                }
                .padding(.horizontal, 12)
                .padding(.top, 8)
            }
            .navigationTitle("War Room")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .topBarLeading) {
                    StatusPill(state: model.probeState)
                }
                ToolbarItemGroup(placement: .topBarTrailing) {
                    Button {
                        Task { await model.testConnection() }
                    } label: {
                        Image(systemName: "waveform.path.ecg")
                    }
                    .accessibilityLabel("Test connection")

                    Button {
                        model.reload()
                    } label: {
                        Image(systemName: "arrow.clockwise")
                    }
                    .accessibilityLabel("Reload War Room")

                    Button {
                        showingSettings = true
                    } label: {
                        Image(systemName: "slider.horizontal.3")
                    }
                    .accessibilityLabel("Settings")
                }
            }
            .sheet(isPresented: $showingSettings) {
                SettingsView(model: model)
            }
            .task {
                if model.config.webBase.isEmpty {
                    showingSettings = true
                } else {
                    await model.testConnection()
                }
            }
        }
    }
}

private struct StatusPill: View {
    var state: ProbeState

    var body: some View {
        HStack(spacing: 6) {
            Circle()
                .fill(color)
                .frame(width: 9, height: 9)
            Text(label)
                .font(.caption2.monospaced())
                .foregroundStyle(.secondary)
                .lineLimit(1)
                .fixedSize(horizontal: true, vertical: false)
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 5)
        .frame(minWidth: 66, alignment: .center)
        .background(.thinMaterial, in: Capsule())
        .accessibilityLabel("Connection status \(label)")
    }

    private var color: Color {
        switch state {
        case .idle:
            return .gray
        case .checking:
            return .cyan
        case .ok:
            return .green
        case .warning:
            return .orange
        case .failed:
            return .red
        }
    }

    private var label: String {
        switch state {
        case .idle:
            return "Idle"
        case .checking:
            return "Check"
        case .ok:
            return "Online"
        case .warning:
            return "API"
        case .failed:
            return "Offline"
        }
    }
}

private enum BannerTone {
    case warning
    case danger
}

private struct Banner: View {
    var text: String
    var systemImage: String
    var tone: BannerTone

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            Image(systemName: systemImage)
            Text(text)
                .lineLimit(3)
        }
        .font(.caption.monospaced())
        .foregroundStyle(tone == .danger ? .red : .orange)
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 8))
    }
}
