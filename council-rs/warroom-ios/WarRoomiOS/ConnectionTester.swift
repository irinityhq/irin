import Foundation

struct ConnectionProbeResult: Equatable {
    var apiOK: Bool
    var wsOK: Bool
    var detail: String
    var endpoints: [ConnectionEndpointResult]
}

struct ConnectionEndpointResult: Equatable, Identifiable {
    var id: String
    var name: String
    var required: Bool
    var configured: Bool
    var ok: Bool?
    var detail: String
}

private struct HealthPayload: Decodable {
    var council_version: String?
    var stream_version: String?
    var ws_smoke_only: Bool?
    var base_dir: String?
}

private struct HTTPProbeResult {
    var ok: Bool
    var statusCode: Int?
    var data: Data?
    var detail: String
}

enum ConnectionTester {
    static func run(config: WarRoomConfig) async -> ConnectionProbeResult {
        let cfg = config.normalized
        DebugLog.write("probe start web=\(cfg.webBase) api=\(cfg.apiBase) ws=\(cfg.wsBase) gateway=\(cfg.gatewayBase) librarian=\(cfg.librarianBase)")

        async let web = probeHTTPEndpoint(
            id: "web",
            name: "Web UI",
            base: cfg.webBase,
            path: "",
            required: false,
            config: cfg,
            includeAuth: false
        )
        async let api = probeCouncilAPI(config: cfg)
        async let ws = probeWebSocket(config: cfg)
        async let gateway = probeHTTPEndpoint(
            id: "gateway",
            name: "Gateway",
            base: cfg.gatewayBase,
            path: "/health",
            required: false,
            config: cfg,
            includeAuth: false
        )
        async let librarian = probeHTTPEndpoint(
            id: "librarian",
            name: "Librarian",
            base: cfg.librarianBase,
            path: "/health",
            required: false,
            config: cfg,
            includeAuth: false
        )

        let results = await [web, api, ws, gateway, librarian]
        for result in results {
            let state = result.ok.map { $0 ? "ok" : "failed" } ?? "skipped"
            DebugLog.write("probe \(result.id) \(state) \(result.detail)")
        }

        let apiOK = results.first { $0.id == "api" }?.ok == true
        let wsOK = results.first { $0.id == "ws" }?.ok == true
        let optionalFailures = results.filter { !$0.required && $0.ok == false }.count
        let summary: String
        if apiOK && wsOK {
            summary = optionalFailures > 0
                ? "Council API and WebSocket OK · \(optionalFailures) optional endpoint failed"
                : "Council API and WebSocket OK"
        } else {
            let failedRequired = results
                .filter { $0.required && $0.ok != true }
                .map(\.name)
                .joined(separator: ", ")
            summary = failedRequired.isEmpty
                ? "Required connection check failed"
                : "Required connection failed: \(failedRequired)"
        }

        return ConnectionProbeResult(apiOK: apiOK, wsOK: wsOK, detail: summary, endpoints: results)
    }

    private static func probeCouncilAPI(config: WarRoomConfig) async -> ConnectionEndpointResult {
        guard let url = endpointURL(base: config.apiBase, path: "/api/health") else {
            return ConnectionEndpointResult(
                id: "api",
                name: "Council API",
                required: true,
                configured: !WarRoomConfig.trimBase(config.apiBase).isEmpty,
                ok: false,
                detail: "Invalid URL"
            )
        }

        let response = await requestHTTP(url: url, config: config, includeAuth: true)
        guard response.ok else {
            return ConnectionEndpointResult(
                id: "api",
                name: "Council API",
                required: true,
                configured: true,
                ok: false,
                detail: response.detail
            )
        }

        do {
            let payload = try JSONDecoder().decode(HealthPayload.self, from: response.data ?? Data())
            let version = payload.council_version ?? "unknown"
            let stream = payload.stream_version ?? "unknown"
            let smoke = payload.ws_smoke_only == true ? "true" : "false"
            return ConnectionEndpointResult(
                id: "api",
                name: "Council API",
                required: true,
                configured: true,
                ok: true,
                detail: "HTTP \(response.statusCode ?? 200) · council \(version), stream \(stream), ws_smoke_only=\(smoke)"
            )
        } catch {
            return ConnectionEndpointResult(
                id: "api",
                name: "Council API",
                required: true,
                configured: true,
                ok: false,
                detail: "HTTP \(response.statusCode ?? 200) · Bad health JSON: \(describeError(error))"
            )
        }
    }

    private static func probeHTTPEndpoint(
        id: String,
        name: String,
        base: String,
        path: String,
        required: Bool,
        config: WarRoomConfig,
        includeAuth: Bool
    ) async -> ConnectionEndpointResult {
        let trimmed = WarRoomConfig.trimBase(base)
        guard !trimmed.isEmpty else {
            return ConnectionEndpointResult(
                id: id,
                name: name,
                required: required,
                configured: false,
                ok: nil,
                detail: "Not configured"
            )
        }
        guard let url = endpointURL(base: trimmed, path: path) else {
            return ConnectionEndpointResult(
                id: id,
                name: name,
                required: required,
                configured: true,
                ok: false,
                detail: "Invalid URL"
            )
        }
        let response = await requestHTTP(url: url, config: config, includeAuth: includeAuth)
        return ConnectionEndpointResult(
            id: id,
            name: name,
            required: required,
            configured: true,
            ok: response.ok,
            detail: response.detail
        )
    }

    private static func probeWebSocket(config: WarRoomConfig) async -> ConnectionEndpointResult {
        guard let url = endpointURL(base: config.wsBase, path: "/ws/deliberate") else {
            return ConnectionEndpointResult(
                id: "ws",
                name: "Council WS",
                required: true,
                configured: !WarRoomConfig.trimBase(config.wsBase).isEmpty,
                ok: false,
                detail: "Invalid URL"
            )
        }
        DebugLog.write("ws upgrade request \(url.absoluteString)")

        let result = await withCheckedContinuation { continuation in
            let token = config.authToken.trimmingCharacters(in: .whitespacesAndNewlines)
            let protocols = token.isEmpty ? ["council"] : ["council", "token.\(token)"]
            WebSocketUpgradeProbe(
                url: url,
                config: config,
                protocols: protocols,
                continuation: continuation
            ).start()
        }

        return ConnectionEndpointResult(
            id: "ws",
            name: "Council WS",
            required: true,
            configured: true,
            ok: result.ok,
            detail: result.detail
        )
    }

    private static func requestHTTP(
        url: URL,
        config: WarRoomConfig,
        includeAuth: Bool
    ) async -> HTTPProbeResult {
        DebugLog.write("http request \(url.absoluteString)")
        var request = URLRequest(url: url, cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: 6)
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        if includeAuth {
            let token = config.authToken.trimmingCharacters(in: .whitespacesAndNewlines)
            if !token.isEmpty {
                request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
            }
        }

        do {
            let delegate = PinnedTrustDelegate(config: config)
            let session = URLSession(configuration: .ephemeral, delegate: delegate, delegateQueue: nil)
            defer { session.invalidateAndCancel() }
            let (data, response) = try await session.data(for: request)
            guard let http = response as? HTTPURLResponse else {
                return HTTPProbeResult(ok: false, statusCode: nil, data: nil, detail: "Non-HTTP response")
            }
            let status = http.statusCode
            let phrase = HTTPURLResponse.localizedString(forStatusCode: status).capitalized
            guard (200..<300).contains(status) else {
                return HTTPProbeResult(ok: false, statusCode: status, data: data, detail: "HTTP \(status) \(phrase)")
            }
            return HTTPProbeResult(ok: true, statusCode: status, data: data, detail: "HTTP \(status) \(phrase)")
        } catch {
            return HTTPProbeResult(ok: false, statusCode: nil, data: nil, detail: describeError(error))
        }
    }

    private static func endpointURL(base: String, path: String) -> URL? {
        let trimmed = WarRoomConfig.trimBase(base)
        guard !trimmed.isEmpty else {
            return nil
        }
        guard !path.isEmpty else {
            return URL(string: trimmed)
        }
        let separator = path.hasPrefix("/") ? "" : "/"
        return URL(string: "\(trimmed)\(separator)\(path)")
    }

    fileprivate static func describeError(_ error: Error) -> String {
        let nsError = error as NSError
        if let http = nsError.userInfo["NSErrorFailingURLResponseKey"] as? HTTPURLResponse {
            let phrase = HTTPURLResponse.localizedString(forStatusCode: http.statusCode).capitalized
            return "HTTP \(http.statusCode) \(phrase)"
        }
        if let urlError = error as? URLError {
            return describeURLError(urlError.code, fallback: urlError.localizedDescription)
        }
        if nsError.domain == NSURLErrorDomain {
            return describeURLError(URLError.Code(rawValue: nsError.code), fallback: nsError.localizedDescription)
        }
        return "\(nsError.domain) \(nsError.code): \(error.localizedDescription)"
    }

    private static func describeURLError(_ code: URLError.Code, fallback: String) -> String {
        switch code {
        case .timedOut:
            return "Timeout: \(fallback)"
        case .cannotFindHost:
            return "DNS: cannot find host (\(fallback))"
        case .cannotConnectToHost:
            return "Connection: cannot connect to host (\(fallback))"
        case .notConnectedToInternet:
            return "Network: not connected to the internet (\(fallback))"
        case .networkConnectionLost:
            return "Network: connection lost (\(fallback))"
        case .secureConnectionFailed,
             .serverCertificateUntrusted,
             .serverCertificateHasBadDate,
             .serverCertificateHasUnknownRoot,
             .serverCertificateNotYetValid,
             .clientCertificateRejected,
             .clientCertificateRequired:
            return "TLS: \(fallback)"
        case .appTransportSecurityRequiresSecureConnection:
            return "ATS: \(fallback)"
        case .badURL, .unsupportedURL:
            return "Invalid URL: \(fallback)"
        case .cancelled:
            return "Cancelled: \(fallback)"
        default:
            return "Network (\(code.rawValue)): \(fallback)"
        }
    }
}

private final class WebSocketUpgradeProbe: NSObject, URLSessionWebSocketDelegate {
    private let url: URL
    private let config: WarRoomConfig
    private let protocols: [String]
    private let continuation: CheckedContinuation<(ok: Bool, detail: String), Never>
    private let once = ResumeOnce()
    private var session: URLSession?
    private var task: URLSessionWebSocketTask?
    private var retainSelf: WebSocketUpgradeProbe?

    init(
        url: URL,
        config: WarRoomConfig,
        protocols: [String],
        continuation: CheckedContinuation<(ok: Bool, detail: String), Never>
    ) {
        self.url = url
        self.config = config
        self.protocols = protocols
        self.continuation = continuation
    }

    func start() {
        retainSelf = self
        let session = URLSession(configuration: .ephemeral, delegate: self, delegateQueue: nil)
        let task = session.webSocketTask(with: url, protocols: protocols)
        self.session = session
        self.task = task
        task.resume()

        DispatchQueue.global().asyncAfter(deadline: .now() + 6) { [weak self] in
            self?.finish(false, "Timeout: WebSocket upgrade timed out after 6s")
        }
    }

    func urlSession(
        _ session: URLSession,
        webSocketTask: URLSessionWebSocketTask,
        didOpenWithProtocol protocol: String?
    ) {
        finish(true, "WebSocket upgrade OK")
    }

    func urlSession(
        _ session: URLSession,
        webSocketTask: URLSessionWebSocketTask,
        didCloseWith closeCode: URLSessionWebSocketTask.CloseCode,
        reason: Data?
    ) {
        let message = reason.flatMap { String(data: $0, encoding: .utf8) }
        let suffix = message.map { ": \($0)" } ?? ""
        finish(false, "WebSocket closed before upgrade (\(closeCode))\(suffix)")
    }

    func urlSession(
        _ session: URLSession,
        task: URLSessionTask,
        didCompleteWithError error: Error?
    ) {
        guard let error else {
            return
        }
        finish(false, "WS \(ConnectionTester.describeError(error))")
    }

    func urlSession(
        _ session: URLSession,
        didReceive challenge: URLAuthenticationChallenge,
        completionHandler: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void
    ) {
        PinnedTrustDelegate(config: config).handle(challenge: challenge, completionHandler: completionHandler)
    }

    private func finish(_ ok: Bool, _ detail: String) {
        if once.claim() {
            DebugLog.write("ws finish \(ok ? "ok" : "failed") \(detail)")
            task?.cancel(with: .normalClosure, reason: nil)
            session?.invalidateAndCancel()
            continuation.resume(returning: (ok, detail))
            retainSelf = nil
        }
    }
}

private final class PinnedTrustDelegate: NSObject, URLSessionDelegate {
    private let config: WarRoomConfig

    init(config: WarRoomConfig) {
        self.config = config
    }

    func urlSession(
        _ session: URLSession,
        didReceive challenge: URLAuthenticationChallenge,
        completionHandler: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void
    ) {
        handle(challenge: challenge, completionHandler: completionHandler)
    }

    func handle(
        challenge: URLAuthenticationChallenge,
        completionHandler: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void
    ) {
        let protection = challenge.protectionSpace
        guard protection.authenticationMethod == NSURLAuthenticationMethodServerTrust else {
            completionHandler(.performDefaultHandling, nil)
            return
        }

        switch config.tlsPinDecision(
            for: protection.host,
            serverTrust: protection.serverTrust
        ) {
        case .platformDefault:
            DebugLog.write("urlsession tls platform-default host=\(protection.host)")
            completionHandler(.performDefaultHandling, nil)
        case .usePinnedTrust:
            guard let trust = protection.serverTrust else {
                DebugLog.write("urlsession tls pin rejected host=\(protection.host) reason=missing-trust")
                completionHandler(.cancelAuthenticationChallenge, nil)
                return
            }
            DebugLog.write("urlsession tls pinned host=\(protection.host)")
            completionHandler(.useCredential, URLCredential(trust: trust))
        case .reject:
            DebugLog.write("urlsession tls pin rejected host=\(protection.host)")
            completionHandler(.cancelAuthenticationChallenge, nil)
        }
    }
}

private final class ResumeOnce {
    private let lock = NSLock()
    private var didResume = false

    func claim() -> Bool {
        lock.lock()
        defer { lock.unlock() }
        if didResume {
            return false
        }
        didResume = true
        return true
    }
}
