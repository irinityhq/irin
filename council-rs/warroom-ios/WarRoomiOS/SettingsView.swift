import SwiftUI

struct SettingsView: View {
    @ObservedObject var model: WarRoomModel
    @Environment(\.dismiss) private var dismiss
    @State private var draft: WarRoomConfig
    @State private var irinOrigin: String

    init(model: WarRoomModel) {
        self.model = model
        _draft = State(initialValue: model.config)
        _irinOrigin = State(initialValue: model.config.unifiedOrigin)
    }

    var body: some View {
        NavigationStack {
            Form {
                Section("IRIN on your tailnet") {
                    ConfigField(
                        "HTTPS URL",
                        text: $irinOrigin,
                        placeholder: "https://irin.example.ts.net",
                        hint: loopbackHint(for: irinOrigin)
                    )
                    Text("War Room, Council, WebSocket, and optional Gateway routes use this single Tailscale origin.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    SecureConfigField("Auth token (optional)", text: $draft.authToken)
                    ConfigField(
                        "Pinned cert SHA-256 (advanced — leave empty for rotating Tailscale TLS)",
                        text: $draft.tlsCertSha256,
                        placeholder: "64 hex characters",
                        keyboardType: .asciiCapable
                    )
                }

                let effectiveDraft = draft.usingUnifiedOrigin(irinOrigin)
                let problems = effectiveDraft.blockingSecurityProblems()
                if !problems.isEmpty {
                    Section("Security") {
                        ForEach(problems, id: \.self) { problem in
                            Label(problem, systemImage: "lock.trianglebadge.exclamationmark")
                                .font(.caption)
                                .foregroundStyle(.orange)
                        }
                    }
                }

                Section("Status") {
                    Label(model.probeDetail, systemImage: statusIcon)
                        .font(.caption.monospaced())
                    ForEach(model.probeResults) { result in
                        EndpointProbeRow(result: result)
                    }
                }
            }
            .navigationTitle("Connection")
            .navigationBarTitleDisplayMode(.inline)
            .onChange(of: draft) { _, _ in
                model.clearProbe()
            }
            .onChange(of: irinOrigin) { _, _ in
                model.clearProbe()
            }
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") {
                        dismiss()
                    }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Save") {
                        saveAndReload(dismissAfterSave: true)
                    }
                }
            }
            .safeAreaInset(edge: .bottom) {
                HStack {
                    Button {
                        saveAndReload(dismissAfterSave: false)
                    } label: {
                        Label("Test", systemImage: "waveform.path.ecg")
                    }
                    .buttonStyle(.bordered)

                    Button {
                        if model.save(draft.usingUnifiedOrigin(irinOrigin)) {
                            dismiss()
                        }
                    } label: {
                        Label("Reload", systemImage: "arrow.clockwise")
                    }
                    .buttonStyle(.borderedProminent)
                }
                .padding()
                .frame(maxWidth: .infinity)
                .background(.bar)
            }
        }
    }

    private func loopbackHint(for value: String) -> String? {
        WarRoomConfig.pointsAtLoopback(value) ? "points at this iPhone, not your Mac" : nil
    }

    private var statusIcon: String {
        switch model.probeState {
        case .idle:
            return "circle"
        case .checking:
            return "hourglass"
        case .ok:
            return "checkmark.circle"
        case .warning:
            return "exclamationmark.circle"
        case .failed:
            return "xmark.circle"
        }
    }

    private func saveAndReload(dismissAfterSave: Bool) {
        guard model.save(draft.usingUnifiedOrigin(irinOrigin)) else {
            return
        }
        Task {
            await model.testConnection()
            if dismissAfterSave {
                dismiss()
            }
        }
    }
}

private struct ConfigField: View {
    var label: String
    @Binding var text: String
    var placeholder: String
    var hint: String?
    var keyboardType: UIKeyboardType

    init(
        _ label: String,
        text: Binding<String>,
        placeholder: String,
        hint: String? = nil,
        keyboardType: UIKeyboardType = .URL
    ) {
        self.label = label
        self._text = text
        self.placeholder = placeholder
        self.hint = hint
        self.keyboardType = keyboardType
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(label)
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
            TextField("", text: $text, prompt: Text(placeholder))
                .keyboardType(keyboardType)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
            if let hint {
                Label(hint, systemImage: "iphone")
                    .font(.caption2)
                    .foregroundStyle(.orange)
            }
        }
    }
}

private struct SecureConfigField: View {
    var label: String
    @Binding var text: String

    init(_ label: String, text: Binding<String>) {
        self.label = label
        self._text = text
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(label)
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
            SecureField("", text: $text)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
        }
    }
}

private struct EndpointProbeRow: View {
    var result: ConnectionEndpointResult

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            Image(systemName: icon)
                .foregroundStyle(color)
                .frame(width: 18)
            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 6) {
                    Text(result.name)
                        .font(.caption.weight(.semibold))
                    Text(result.required ? "required" : "optional")
                        .font(.caption2.monospaced())
                        .foregroundStyle(.secondary)
                }
                Text(result.detail)
                    .font(.caption2.monospaced())
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    private var icon: String {
        switch result.ok {
        case .some(true):
            return "checkmark.circle.fill"
        case .some(false):
            return "xmark.circle.fill"
        case .none:
            return "minus.circle"
        }
    }

    private var color: Color {
        switch result.ok {
        case .some(true):
            return .green
        case .some(false):
            return .red
        case .none:
            return .secondary
        }
    }
}
