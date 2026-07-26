import Foundation

enum BootstrapConfig {
    static func loadFromEnvironmentIfPresent() -> WarRoomRuntimeSelection? {
        guard let encoded = ProcessInfo.processInfo.environment["WARROOM_BOOTSTRAP_CONFIG_B64"],
              !encoded.isEmpty,
              let data = Data(base64Encoded: encoded),
              let config = try? JSONDecoder().decode(WarRoomConfig.self, from: data) else {
            return nil
        }
        return WarRoomRuntimeSelection.validate(config)
    }
}
