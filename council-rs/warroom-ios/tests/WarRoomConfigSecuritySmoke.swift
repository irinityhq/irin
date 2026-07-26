import Foundation
import Security

@main
struct WarRoomConfigSecuritySmoke {
    private enum InjectedFailure: Error {
        case keychainWrite
    }

    private enum Keys {
        static let webBase = "warroom.ios.webBase"
        static let apiBase = "warroom.ios.apiBase"
        static let wsBase = "warroom.ios.wsBase"
        static let gatewayBase = "warroom.ios.gatewayBase"
        static let librarianBase = "warroom.ios.librarianBase"
        static let tlsCertSha256 = "warroom.ios.tlsCertSha256"
    }

    static func main() {
        validatesCompleteProductOrigin()
        rejectsInvalidRuntimeCandidatesWithoutTheirBearer()
        rejectsEmptyAndMixedBootstrapCandidates()
        rejectsRetainedInvalidDefaultsWithoutTheirKeychainBearer()
        rejectsEmptyAndMixedPersistedCandidates()
        rollsBackDefaultsWhenKeychainWriteFails()
        rollsBackDefaultsWhenKeychainDeletionFails()
        enforcesFailClosedTLSPinPolicy()
        print("PASS: iOS config persistence, origin, and TLS pin policies fail closed")
    }

    private static func validatesCompleteProductOrigin() {
        let privateTailnet = WarRoomConfig.defaults
            .usingUnifiedOrigin("https://irin.example.ts.net:8443")
        precondition(
            privateTailnet.blockingSecurityProblems().isEmpty,
            "IRIN's complete generated private tailnet origin must be accepted"
        )

        let arbitraryHTTPS = WarRoomConfig.defaults
            .usingUnifiedOrigin("https://example.com:8443")
        precondition(
            arbitraryHTTPS.blockingSecurityProblems().contains {
                $0.contains("private Tailscale .ts.net address")
            },
            "arbitrary HTTPS origins must be rejected before bearer injection"
        )

        let loopback = WarRoomConfig.defaults
            .usingUnifiedOrigin("https://127.0.0.1:8443")
        precondition(
            !loopback.blockingSecurityProblems().isEmpty,
            "iPhone loopback must not be admitted as the Mac origin"
        )

        var mixed = privateTailnet
        mixed.gatewayBase = "https://other.example.ts.net:8443"
        mixed.wsBase = "wss://irin.example.ts.net:9443"
        mixed.librarianBase = "https://irin.example.ts.net:8443"
        let detail = mixed.blockingSecurityProblems().joined(separator: " ")
        precondition(detail.contains("same HTTPS tailnet origin and port"))
        precondition(detail.contains("same tailnet host and port"))
        precondition(detail.contains("Librarian base must be empty"))
    }

    private static func rejectsInvalidRuntimeCandidatesWithoutTheirBearer() {
        var invalidBootstrap = WarRoomConfig.defaults
            .usingUnifiedOrigin("https://example.com")
        invalidBootstrap.authToken = "retained-bootstrap-bearer"
        assertBlockedTokenFree(
            WarRoomRuntimeSelection.validate(invalidBootstrap),
            bearer: invalidBootstrap.authToken,
            context: "invalid bootstrap config"
        )
    }

    private static func rejectsEmptyAndMixedBootstrapCandidates() {
        var empty = WarRoomConfig.defaults
        empty.authToken = "empty-bootstrap-bearer"
        assertBlockedTokenFree(
            WarRoomRuntimeSelection.validate(empty),
            bearer: empty.authToken,
            context: "empty bootstrap config"
        )

        var mixed = WarRoomConfig.defaults
            .usingUnifiedOrigin("https://irin.example.ts.net:8443")
        mixed.apiBase = "https://other.example.ts.net:8443"
        mixed.authToken = "mixed-bootstrap-bearer"
        assertBlockedTokenFree(
            WarRoomRuntimeSelection.validate(mixed),
            bearer: mixed.authToken,
            context: "mixed bootstrap config"
        )
    }

    private static func rejectsRetainedInvalidDefaultsWithoutTheirKeychainBearer() {
        withIsolatedDefaults { defaults in
            let invalidOrigin = "https://example.com"
            defaults.set(invalidOrigin, forKey: Keys.webBase)
            defaults.set(invalidOrigin, forKey: Keys.apiBase)
            defaults.set("wss://example.com", forKey: Keys.wsBase)
            defaults.set(invalidOrigin, forKey: Keys.gatewayBase)
            let retainedToken = "retained-keychain-bearer"
            let store = WarRoomSettingsStore(
                defaults: defaults,
                readToken: { retainedToken },
                saveToken: { _ in preconditionFailure("load must not write the token") }
            )

            assertBlockedTokenFree(
                store.load(),
                bearer: retainedToken,
                context: "invalid persisted origin"
            )
            precondition(
                defaults.string(forKey: Keys.webBase) == invalidOrigin,
                "the regression must exercise retained legacy UserDefaults"
            )
        }
    }

    private static func rejectsEmptyAndMixedPersistedCandidates() {
        withIsolatedDefaults { defaults in
            let retainedToken = "retained-empty-persisted-bearer"
            let store = WarRoomSettingsStore(
                defaults: defaults,
                readToken: { retainedToken },
                saveToken: { _ in preconditionFailure("load must not write the token") }
            )
            assertBlockedTokenFree(
                store.load(),
                bearer: retainedToken,
                context: "empty persisted config"
            )

            var mixed = WarRoomConfig.defaults
                .usingUnifiedOrigin("https://irin.example.ts.net:8443")
            mixed.gatewayBase = "https://other.example.ts.net:8443"
            persist(mixed, to: defaults)
            assertBlockedTokenFree(
                store.load(),
                bearer: retainedToken,
                context: "mixed persisted config"
            )
        }
    }

    private static func rollsBackDefaultsWhenKeychainWriteFails() {
        withIsolatedDefaults { defaults in
            var previous = WarRoomConfig.defaults
                .usingUnifiedOrigin("https://old.example.ts.net")
            previous.authToken = "old-keychain-bearer"
            persist(previous, to: defaults, includeFingerprint: false)

            let store = WarRoomSettingsStore(
                defaults: defaults,
                readToken: { previous.authToken },
                saveToken: { _ in throw InjectedFailure.keychainWrite }
            )
            var replacement = WarRoomConfig.defaults
                .usingUnifiedOrigin("https://new.example.ts.net:8443")
            replacement.tlsCertSha256 = String(repeating: "a", count: 64)
            replacement.authToken = "new-keychain-bearer"

            var didFail = false
            do {
                try store.save(replacement)
            } catch InjectedFailure.keychainWrite {
                didFail = true
            } catch {
                preconditionFailure("unexpected persistence error: \(error)")
            }
            precondition(didFail, "injected Keychain failure must escape save")
            precondition(defaults.string(forKey: Keys.webBase) == previous.webBase)
            precondition(defaults.string(forKey: Keys.apiBase) == previous.apiBase)
            precondition(defaults.string(forKey: Keys.wsBase) == previous.wsBase)
            precondition(defaults.string(forKey: Keys.gatewayBase) == previous.gatewayBase)
            precondition(defaults.string(forKey: Keys.librarianBase) == previous.librarianBase)
            precondition(
                defaults.object(forKey: Keys.tlsCertSha256) == nil,
                "rollback must restore an absent prior fingerprint, not an empty replacement"
            )
            let restored = store.load()
            precondition(!restored.isBlocked, "the prior valid configuration must remain usable")
            precondition(restored.config == previous.normalized)
        }
    }

    private static func rollsBackDefaultsWhenKeychainDeletionFails() {
        withIsolatedDefaults { defaults in
            var previous = WarRoomConfig.defaults
                .usingUnifiedOrigin("https://old.example.ts.net")
            previous.authToken = "old-keychain-bearer"
            persist(previous, to: defaults)

            let store = WarRoomSettingsStore(
                defaults: defaults,
                readToken: { previous.authToken },
                saveToken: { token in
                    try KeychainStore.saveToken(
                        token,
                        deleteItem: { _ in errSecAuthFailed }
                    )
                }
            )
            var replacement = WarRoomConfig.defaults
                .usingUnifiedOrigin("https://new.example.ts.net:8443")
            replacement.authToken = ""

            var didFail = false
            do {
                try store.save(replacement)
            } catch let error as KeychainError {
                precondition(error.status == errSecAuthFailed)
                didFail = true
            } catch {
                preconditionFailure("unexpected persistence error: \(error)")
            }
            precondition(didFail, "failed Keychain deletion must escape save")
            precondition(defaults.string(forKey: Keys.webBase) == previous.webBase)
            precondition(defaults.string(forKey: Keys.apiBase) == previous.apiBase)
            precondition(defaults.string(forKey: Keys.wsBase) == previous.wsBase)
            precondition(defaults.string(forKey: Keys.gatewayBase) == previous.gatewayBase)
            precondition(defaults.string(forKey: Keys.librarianBase) == previous.librarianBase)
            let restored = store.load()
            precondition(!restored.isBlocked, "the prior valid configuration must remain usable")
            precondition(restored.config == previous.normalized)

            do {
                try KeychainStore.saveToken(
                    "",
                    deleteItem: { _ in errSecItemNotFound }
                )
            } catch {
                preconditionFailure("deleting an absent token must succeed: \(error)")
            }
        }
    }

    private static func enforcesFailClosedTLSPinPolicy() {
        var config = WarRoomConfig.defaults
            .usingUnifiedOrigin("https://irin.example.ts.net:8443")
        precondition(
            config.tlsPinDecision(for: "irin.example.ts.net", presentedFingerprint: nil) == .platformDefault,
            "platform trust is allowed only when no pin is configured"
        )

        let pin = String(repeating: "a", count: 64)
        config.tlsCertSha256 = pin
        precondition(
            config.tlsPinDecision(for: "irin.example.ts.net", presentedFingerprint: pin) == .usePinnedTrust
        )
        precondition(
            config.tlsPinDecision(
                for: "irin.example.ts.net",
                presentedFingerprint: String(repeating: "b", count: 64)
            ) == .reject,
            "configured pin mismatch must reject"
        )
        precondition(
            config.tlsPinDecision(for: "irin.example.ts.net", presentedFingerprint: nil) == .reject,
            "missing trust evidence with a configured pin must reject"
        )
        precondition(
            config.tlsPinDecision(for: "other.example.ts.net", presentedFingerprint: pin) == .reject,
            "a matching certificate fingerprint for the wrong host must reject"
        )

        var allNonHex = WarRoomConfig.defaults
            .usingUnifiedOrigin("https://irin.example.ts.net:8443")
        allNonHex.tlsCertSha256 = "not-a-fingerprint!!!"
        precondition(
            allNonHex.blockingSecurityProblems().contains {
                $0.contains("64 hex characters")
            },
            "a nonblank pin that normalizes to empty must be rejected"
        )
        assertBlockedTokenFree(
            WarRoomRuntimeSelection.validate(allNonHex),
            bearer: "unused-bearer",
            context: "all-nonhex TLS pin"
        )

        var validHexPlusJunk = WarRoomConfig.defaults
            .usingUnifiedOrigin("https://irin.example.ts.net:8443")
        validHexPlusJunk.tlsCertSha256 = String(repeating: "a", count: 64) + "!"
        precondition(
            validHexPlusJunk.blockingSecurityProblems().contains {
                $0.contains("64 hex characters")
            },
            "a valid-length hex prefix with trailing junk must be rejected"
        )
        assertBlockedTokenFree(
            WarRoomRuntimeSelection.validate(validHexPlusJunk),
            bearer: "unused-bearer",
            context: "valid hex plus junk TLS pin"
        )

        var malformed = WarRoomConfig.defaults
            .usingUnifiedOrigin("https://irin.example.ts.net:8443")
        malformed.tlsCertSha256 = "aa:bb:cc"
        precondition(
            malformed.blockingSecurityProblems().contains {
                $0.contains("64 hex characters")
            },
            "a malformed nonblank pin must be rejected"
        )
        assertBlockedTokenFree(
            WarRoomRuntimeSelection.validate(malformed),
            bearer: "unused-bearer",
            context: "malformed TLS pin"
        )

        var whitespaceOnly = WarRoomConfig.defaults
            .usingUnifiedOrigin("https://irin.example.ts.net:8443")
        whitespaceOnly.tlsCertSha256 = "   \n"
        precondition(
            WarRoomRuntimeSelection.validate(whitespaceOnly).blockingProblems.isEmpty,
            "blank pin text must preserve platform-default trust"
        )
    }

    private static func assertBlockedTokenFree(
        _ selected: WarRoomRuntimeSelection,
        bearer: String,
        context: String
    ) {
        precondition(selected.isBlocked, "\(context) must be blocked")
        precondition(selected.config == .defaults, "\(context) must select safe defaults")
        precondition(selected.config.authToken.isEmpty, "\(context) must be token-free")
        precondition(
            !selected.blockingMessage.contains(bearer),
            "\(context) diagnostics must not include the bearer"
        )
    }

    private static func persist(
        _ config: WarRoomConfig,
        to defaults: UserDefaults,
        includeFingerprint: Bool = true
    ) {
        let cfg = config.normalized
        defaults.set(cfg.webBase, forKey: Keys.webBase)
        defaults.set(cfg.apiBase, forKey: Keys.apiBase)
        defaults.set(cfg.wsBase, forKey: Keys.wsBase)
        defaults.set(cfg.gatewayBase, forKey: Keys.gatewayBase)
        defaults.set(cfg.librarianBase, forKey: Keys.librarianBase)
        if includeFingerprint {
            defaults.set(cfg.tlsCertSha256, forKey: Keys.tlsCertSha256)
        } else {
            defaults.removeObject(forKey: Keys.tlsCertSha256)
        }
    }

    private static func withIsolatedDefaults(_ body: (UserDefaults) -> Void) {
        let suiteName = "com.innerway.warroom.tests.\(UUID().uuidString)"
        guard let defaults = UserDefaults(suiteName: suiteName) else {
            preconditionFailure("could not create isolated UserDefaults suite")
        }
        defer { defaults.removePersistentDomain(forName: suiteName) }
        body(defaults)
    }
}
