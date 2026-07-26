import Foundation
import CryptoKit
import Security

enum TLSPinDecision: Equatable {
    case platformDefault
    case usePinnedTrust
    case reject
}

struct WarRoomConfig: Codable, Equatable {
    var webBase: String
    var apiBase: String
    var wsBase: String
    var gatewayBase: String
    var librarianBase: String
    var tlsCertSha256: String
    var authToken: String

    static let defaults = WarRoomConfig(
        webBase: "",
        apiBase: "",
        wsBase: "",
        gatewayBase: "",
        librarianBase: "",
        tlsCertSha256: "",
        authToken: ""
    )

    var normalized: WarRoomConfig {
        WarRoomConfig(
            webBase: Self.trimBase(webBase),
            apiBase: Self.trimBase(apiBase),
            wsBase: Self.trimBase(wsBase),
            gatewayBase: Self.trimBase(gatewayBase),
            librarianBase: Self.trimBase(librarianBase),
            tlsCertSha256: Self.normalizeFingerprint(tlsCertSha256),
            authToken: authToken.trimmingCharacters(in: .whitespacesAndNewlines)
        )
    }

    var webURL: URL? {
        URL(string: normalized.webBase)
    }

    /// The installed IRIN product publishes one Tailscale HTTPS origin. War
    /// Room, Council API, and the optional Gateway routes all live behind it;
    /// only WebSocket changes scheme.
    func usingUnifiedOrigin(_ value: String) -> WarRoomConfig {
        let origin = Self.trimBase(value)
        var websocket = ""
        if var components = URLComponents(string: origin) {
            components.scheme = components.scheme?.lowercased() == "https" ? "wss" : "ws"
            websocket = Self.trimBase(components.string ?? "")
        }
        return WarRoomConfig(
            webBase: origin,
            apiBase: origin,
            wsBase: websocket,
            gatewayBase: origin,
            librarianBase: "",
            tlsCertSha256: tlsCertSha256,
            authToken: authToken
        )
    }

    var unifiedOrigin: String {
        let cfg = normalized
        guard !cfg.webBase.isEmpty,
              cfg.apiBase == cfg.webBase,
              cfg.gatewayBase == cfg.webBase,
              cfg.librarianBase.isEmpty,
              let web = URLComponents(string: cfg.webBase),
              let ws = URLComponents(string: cfg.wsBase),
              web.host == ws.host,
              web.port == ws.port,
              web.scheme?.lowercased() == "https",
              ws.scheme?.lowercased() == "wss" else {
            return cfg.webBase
        }
        return cfg.webBase
    }

    var runtimePayload: [String: String] {
        let cfg = normalized
        return [
            "apiBase": cfg.apiBase,
            "wsBase": cfg.wsBase,
            "gatewayBase": cfg.gatewayBase,
            "authToken": cfg.authToken,
            "councilPath": "",
            "councilRoot": "",
            "librarianBase": cfg.librarianBase,
        ]
    }

    func blockingSecurityProblems() -> [String] {
        let rawFingerprint = tlsCertSha256.trimmingCharacters(in: .whitespacesAndNewlines)
        let cfg = normalized
        var problems: [String] = []
        problems += Self.requireURL(cfg.webBase, label: "War Room web URL", allowedSchemes: ["http", "https"], secureRemoteScheme: "https")
        problems += Self.requireURL(cfg.apiBase, label: "API base", allowedSchemes: ["http", "https"], secureRemoteScheme: "https")
        problems += Self.requireURL(cfg.wsBase, label: "WebSocket base", allowedSchemes: ["ws", "wss"], secureRemoteScheme: "wss")
        problems += Self.requireURL(cfg.gatewayBase, label: "Gateway base", allowedSchemes: ["http", "https"], secureRemoteScheme: "https")
        if !cfg.librarianBase.isEmpty {
            problems.append("Librarian base must be empty for the installed iPhone product.")
        }
        if !rawFingerprint.isEmpty && !Self.isRawSha256Fingerprint(rawFingerprint) {
            problems.append("TLS certificate SHA-256 must be exactly 64 hex characters.")
        }

        if let web = URLOrigin(urlString: cfg.webBase),
           let api = URLOrigin(urlString: cfg.apiBase),
           let gateway = URLOrigin(urlString: cfg.gatewayBase),
           let websocket = URLOrigin(urlString: cfg.wsBase) {
            if web.scheme != "https" || api != web || gateway != web {
                problems.append("War Room, API, and Gateway must use the same HTTPS tailnet origin and port.")
            }
            if websocket.scheme != "wss" ||
                websocket.host != web.host ||
                websocket.port != web.port {
                problems.append("WebSocket must use WSS on the same tailnet host and port.")
            }
        }
        return problems
    }

    func allowsNavigation(to url: URL) -> Bool {
        if url.scheme == "about" {
            return true
        }
        guard let trusted = URLOrigin(urlString: normalized.webBase),
              let target = URLOrigin(url: url) else {
            return false
        }
        return trusted == target
    }

    /// Pure TLS policy shared by WKWebView and URLSession delegates. A pin is
    /// an override contract: once configured, any missing/mismatched evidence
    /// rejects the challenge instead of falling back to platform trust.
    func tlsPinDecision(for host: String, presentedFingerprint: String?) -> TLSPinDecision {
        let cfg = normalized
        guard !cfg.tlsCertSha256.isEmpty else {
            return .platformDefault
        }
        guard let configuredHost = URL(string: cfg.webBase)?.host?.lowercased(),
              configuredHost == host.lowercased(),
              let presentedFingerprint else {
            return .reject
        }
        return Self.normalizeFingerprint(presentedFingerprint) == cfg.tlsCertSha256
            ? .usePinnedTrust
            : .reject
    }

    func tlsPinDecision(for host: String, serverTrust: SecTrust?) -> TLSPinDecision {
        tlsPinDecision(
            for: host,
            presentedFingerprint: serverTrust.flatMap(Self.certificateFingerprint)
        )
    }

    func allowsPinnedTrust(for host: String, serverTrust: SecTrust) -> Bool {
        tlsPinDecision(for: host, serverTrust: serverTrust) == .usePinnedTrust
    }

    static func pointsAtLoopback(_ value: String) -> Bool {
        guard let host = URL(string: trimBase(value))?.host else {
            return false
        }
        return isLoopback(host)
    }

    static func trimBase(_ value: String) -> String {
        var trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
        while trimmed.count > 1 && trimmed.hasSuffix("/") {
            trimmed.removeLast()
        }
        return trimmed
    }

    static func normalizeFingerprint(_ value: String) -> String {
        value
            .trimmingCharacters(in: .whitespacesAndNewlines)
            .lowercased()
            .filter { $0.isHexDigit }
    }

    private static func requireURL(
        _ value: String,
        label: String,
        allowedSchemes: Set<String>,
        secureRemoteScheme: String
    ) -> [String] {
        if value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            return ["\(label) is required."]
        }
        return optionalURL(value, label: label, allowedSchemes: allowedSchemes, secureRemoteScheme: secureRemoteScheme)
    }

    private static func optionalURL(
        _ value: String,
        label: String,
        allowedSchemes: Set<String>,
        secureRemoteScheme: String
    ) -> [String] {
        let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.isEmpty {
            return []
        }
        guard let url = URL(string: trimmed),
              let scheme = url.scheme?.lowercased(),
              let host = url.host?.lowercased(),
              allowedSchemes.contains(scheme) else {
            return ["\(label) must be a valid \(allowedSchemes.sorted().joined(separator: "/")) URL."]
        }
        if scheme != secureRemoteScheme {
            return ["\(label) must use \(secureRemoteScheme.uppercased())."]
        }
        if !Self.isTailnetHost(host) {
            return ["\(label) must use the private Tailscale .ts.net address shown by IRIN."]
        }
        return []
    }

    static func isTailnetHost(_ host: String) -> Bool {
        let normalized = host.lowercased().trimmingCharacters(in: CharacterSet(charactersIn: "."))
        return normalized.hasSuffix(".ts.net") && normalized.count > ".ts.net".count
    }

    private static func isLoopback(_ host: String) -> Bool {
        let normalized = host
            .trimmingCharacters(in: CharacterSet(charactersIn: "[]"))
            .lowercased()
        return normalized == "127.0.0.1" || normalized == "localhost" || normalized == "::1"
    }

    private static func isRawSha256Fingerprint(_ value: String) -> Bool {
        let bytes = Array(value.utf8)
        return bytes.count == 64 && bytes.allSatisfy { byte in
            (byte >= 48 && byte <= 57) ||
                (byte >= 65 && byte <= 70) ||
                (byte >= 97 && byte <= 102)
        }
    }

    private static func isSha256Fingerprint(_ value: String) -> Bool {
        value.count == 64 && value.allSatisfy(\.isHexDigit)
    }

    private static func certificateFingerprint(serverTrust: SecTrust) -> String? {
        guard let chain = SecTrustCopyCertificateChain(serverTrust) as? [SecCertificate],
              let certificate = chain.first else {
            return nil
        }
        let data = SecCertificateCopyData(certificate) as Data
        return SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
    }
}

/// The only configuration shape allowed to cross into live iOS runtime state.
/// Invalid persisted or bootstrap input is replaced with the token-free defaults
/// while its non-secret validation failures remain available for the blocked UI.
struct WarRoomRuntimeSelection: Equatable {
    let config: WarRoomConfig
    let blockingProblems: [String]

    var isBlocked: Bool {
        !blockingProblems.isEmpty
    }

    var blockingMessage: String {
        blockingProblems.joined(separator: " ")
    }

    static func validate(_ candidate: WarRoomConfig) -> WarRoomRuntimeSelection {
        let problems = candidate.blockingSecurityProblems()
        let normalized = candidate.normalized
        guard problems.isEmpty else {
            return WarRoomRuntimeSelection(
                config: .defaults,
                blockingProblems: problems
            )
        }
        return WarRoomRuntimeSelection(config: normalized, blockingProblems: [])
    }
}

struct URLOrigin: Hashable {
    let scheme: String
    let host: String
    let port: Int?

    init?(urlString: String) {
        guard let url = URL(string: urlString) else {
            return nil
        }
        self.init(url: url)
    }

    init?(url: URL) {
        guard let scheme = url.scheme?.lowercased(),
              let host = url.host?.lowercased() else {
            return nil
        }
        self.scheme = scheme
        self.host = host
        self.port = url.port ?? Self.defaultPort(for: scheme)
    }

    private static func defaultPort(for scheme: String) -> Int? {
        switch scheme {
        case "http", "ws":
            return 80
        case "https", "wss":
            return 443
        default:
            return nil
        }
    }
}
