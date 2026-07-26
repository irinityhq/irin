import Foundation
import SwiftUI

enum ProbeState: Equatable {
    case idle
    case checking
    case ok
    case warning
    case failed
}

enum WebLoadState: Equatable {
    case idle
    case loading
    case loaded
    case blocked(String)
    case failed(String)
}

@MainActor
final class WarRoomModel: ObservableObject {
    @Published private(set) var config: WarRoomConfig
    @Published var probeState: ProbeState = .idle
    @Published var probeDetail = "Not tested"
    @Published var probeResults: [ConnectionEndpointResult] = []
    @Published var webState: WebLoadState = .idle
    @Published var reloadID = UUID()

    private let store: WarRoomSettingsStore
    private var probeGeneration = 0

    init(store: WarRoomSettingsStore = WarRoomSettingsStore()) {
        self.store = store

        #if DEBUG
        if let bootstrap = BootstrapConfig.loadFromEnvironmentIfPresent() {
            self.config = bootstrap.config
            if bootstrap.isBlocked {
                self.webState = .blocked("Invalid bootstrap configuration. \(bootstrap.blockingMessage)")
                DebugLog.write("config source=bootstrap blocked")
            } else {
                DebugLog.write("config source=bootstrap web=\(bootstrap.config.webBase) api=\(bootstrap.config.apiBase) ws=\(bootstrap.config.wsBase) pin=\(bootstrap.config.tlsCertSha256.isEmpty ? "empty" : "set")")
                do {
                    try store.save(bootstrap.config)
                    DebugLog.write("bootstrap persisted")
                } catch {
                    DebugLog.write("bootstrap persist failed \(error.localizedDescription)")
                }
            }
        } else {
            let persisted = store.load()
            self.config = persisted.config
            if persisted.isBlocked {
                self.webState = .blocked("Invalid saved configuration. \(persisted.blockingMessage)")
                DebugLog.write("config source=store blocked")
            } else {
                DebugLog.write("config source=store web=\(config.webBase) api=\(config.apiBase) ws=\(config.wsBase) pin=\(config.tlsCertSha256.isEmpty ? "empty" : "set")")
            }
        }
        #else
        let persisted = store.load()
        self.config = persisted.config
        if persisted.isBlocked {
            self.webState = .blocked("Invalid saved configuration. \(persisted.blockingMessage)")
        }
        #endif
    }

    func save(_ next: WarRoomConfig) -> Bool {
        let problems = next.blockingSecurityProblems()
        let cfg = next.normalized
        guard problems.isEmpty else {
            probeState = .failed
            probeDetail = problems.joined(separator: " ")
            probeResults = []
            return false
        }
        do {
            try store.save(cfg)
            config = cfg
            reload()
            return true
        } catch {
            probeState = .failed
            probeDetail = error.localizedDescription
            return false
        }
    }

    func clearProbe() {
        probeGeneration += 1
        probeState = .idle
        probeDetail = "Not tested"
        probeResults = []
    }

    func reload() {
        reloadID = UUID()
    }

    func testConnection() async {
        probeGeneration += 1
        let generation = probeGeneration
        probeState = .checking
        probeDetail = "Checking connection"
        probeResults = []
        let result = await ConnectionTester.run(config: config)
        guard generation == probeGeneration else {
            return
        }
        if result.apiOK && result.wsOK {
            probeState = .ok
        } else {
            probeState = .failed
        }
        probeDetail = result.detail
        probeResults = result.endpoints
    }

    func markWebLoading() {
        webState = .loading
    }

    func markWebLoaded() {
        webState = .loaded
    }

    func markWebBlocked(_ url: String) {
        webState = .blocked(url)
    }

    func markWebFailed(_ message: String) {
        webState = .failed(message)
    }
}
