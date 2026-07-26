import Foundation

struct WarRoomSettingsStore {
    private enum Keys {
        static let webBase = "warroom.ios.webBase"
        static let apiBase = "warroom.ios.apiBase"
        static let wsBase = "warroom.ios.wsBase"
        static let gatewayBase = "warroom.ios.gatewayBase"
        static let librarianBase = "warroom.ios.librarianBase"
        static let tlsCertSha256 = "warroom.ios.tlsCertSha256"
        static let all = [
            webBase,
            apiBase,
            wsBase,
            gatewayBase,
            librarianBase,
            tlsCertSha256,
        ]
    }

    private let defaults: UserDefaults
    private let readToken: () -> String
    private let saveToken: (String) throws -> Void

    init(
        defaults: UserDefaults = .standard,
        readToken: @escaping () -> String = { KeychainStore.readToken() },
        saveToken: @escaping (String) throws -> Void = { try KeychainStore.saveToken($0) }
    ) {
        self.defaults = defaults
        self.readToken = readToken
        self.saveToken = saveToken
    }

    /// Loads persisted values as untrusted input. An invalid legacy or damaged
    /// record never returns its Keychain bearer in the selected runtime config.
    func load() -> WarRoomRuntimeSelection {
        let fallback = WarRoomConfig.defaults
        let candidate = WarRoomConfig(
            webBase: defaults.string(forKey: Keys.webBase) ?? fallback.webBase,
            apiBase: defaults.string(forKey: Keys.apiBase) ?? fallback.apiBase,
            wsBase: defaults.string(forKey: Keys.wsBase) ?? fallback.wsBase,
            gatewayBase: defaults.string(forKey: Keys.gatewayBase) ?? fallback.gatewayBase,
            librarianBase: defaults.string(forKey: Keys.librarianBase) ?? fallback.librarianBase,
            tlsCertSha256: defaults.string(forKey: Keys.tlsCertSha256) ?? fallback.tlsCertSha256,
            authToken: readToken()
        )
        return WarRoomRuntimeSelection.validate(candidate)
    }

    func save(_ config: WarRoomConfig) throws {
        let selection = WarRoomRuntimeSelection.validate(config)
        guard !selection.isBlocked else {
            throw WarRoomSettingsStoreError.invalidConfiguration(selection.blockingMessage)
        }
        let cfg = selection.config
        let snapshot = Keys.all.map { key in
            (key: key, value: defaults.object(forKey: key))
        }

        defaults.set(cfg.webBase, forKey: Keys.webBase)
        defaults.set(cfg.apiBase, forKey: Keys.apiBase)
        defaults.set(cfg.wsBase, forKey: Keys.wsBase)
        defaults.set(cfg.gatewayBase, forKey: Keys.gatewayBase)
        defaults.set(cfg.librarianBase, forKey: Keys.librarianBase)
        defaults.set(cfg.tlsCertSha256, forKey: Keys.tlsCertSha256)
        do {
            try saveToken(cfg.authToken)
        } catch {
            // UserDefaults and Keychain are separate stores. Restore the exact
            // endpoint snapshot so a failed bearer update cannot pair a new
            // origin with the previous Keychain token.
            for item in snapshot {
                if let value = item.value {
                    defaults.set(value, forKey: item.key)
                } else {
                    defaults.removeObject(forKey: item.key)
                }
            }
            throw error
        }
    }
}

enum WarRoomSettingsStoreError: LocalizedError {
    case invalidConfiguration(String)

    var errorDescription: String? {
        switch self {
        case .invalidConfiguration(let detail):
            return "Blocked invalid War Room configuration. \(detail)"
        }
    }
}
