import SwiftUI
import WebKit

struct WarRoomWebView: UIViewRepresentable {
    @ObservedObject var model: WarRoomModel

    func makeCoordinator() -> Coordinator {
        Coordinator(model: model)
    }

    func makeUIView(context: Context) -> WKWebView {
        // Persisted and bootstrap values are untrusted input. Validate before
        // constructing a WebView configuration that could carry the bearer.
        let selection = WarRoomRuntimeSelection.validate(model.config)
        let configuration = WKWebViewConfiguration()
        configuration.websiteDataStore = .nonPersistent()
        configuration.defaultWebpagePreferences.allowsContentJavaScript = true
        if !selection.isBlocked,
           let script = runtimeScript(for: selection.config) {
            configuration.userContentController.addUserScript(script)
        }

        let webView = WKWebView(frame: .zero, configuration: configuration)
        webView.navigationDelegate = context.coordinator
        webView.allowsBackForwardNavigationGestures = true
#if DEBUG
        if #available(iOS 16.4, *) {
            webView.isInspectable = true
        }
#endif
        context.coordinator.loadedReloadID = model.reloadID
        context.coordinator.loadedConfig = selection.config
        if selection.isBlocked {
            model.markWebBlocked("Invalid configuration. \(selection.blockingMessage)")
        } else {
            loadHome(in: webView, config: selection.config, model: model)
        }
        return webView
    }

    func updateUIView(_ webView: WKWebView, context: Context) {
        guard context.coordinator.loadedReloadID != model.reloadID ||
                context.coordinator.loadedConfig != model.config else {
            return
        }

        // Revalidate at the final runtime boundary. If state is ever corrupted
        // after launch, remove all scripts and replace the previous document.
        let selection = WarRoomRuntimeSelection.validate(model.config)
        webView.stopLoading()
        webView.configuration.userContentController.removeAllUserScripts()
        context.coordinator.loadedReloadID = model.reloadID
        context.coordinator.loadedConfig = selection.config
        guard !selection.isBlocked,
              let script = runtimeScript(for: selection.config) else {
            webView.loadHTMLString("", baseURL: nil)
            model.markWebBlocked("Invalid configuration. \(selection.blockingMessage)")
            return
        }

        webView.configuration.userContentController.addUserScript(script)
        loadHome(in: webView, config: selection.config, model: model)
    }

    private func loadHome(in webView: WKWebView, config: WarRoomConfig, model: WarRoomModel) {
        // Navigation is a second trust boundary after script construction.
        let selection = WarRoomRuntimeSelection.validate(config)
        guard !selection.isBlocked else {
            model.markWebBlocked("Invalid configuration. \(selection.blockingMessage)")
            return
        }
        guard let url = selection.config.webURL else {
            model.markWebFailed("Invalid War Room web URL")
            return
        }
        DebugLog.write("web load \(url.absoluteString)")
        var request = URLRequest(url: url, cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: 20)
        request.setValue("WarRoomiOS/0.1", forHTTPHeaderField: "User-Agent")
        webView.load(request)
    }

    /// Returns no script unless the exact configuration being serialized still
    /// passes validation. This check immediately precedes bearer serialization.
    private func runtimeScript(for config: WarRoomConfig) -> WKUserScript? {
        let selection = WarRoomRuntimeSelection.validate(config)
        guard !selection.isBlocked else {
            return nil
        }
        let payload = selection.config.runtimePayload
        let data = (try? JSONSerialization.data(withJSONObject: payload, options: [])) ?? Data("{}".utf8)
        let json = String(data: data, encoding: .utf8) ?? "{}"
        let source = """
        (() => {
          const nativeCfg = \(json);
          const durableCfg = { ...nativeCfg, authToken: "" };
          try {
            window.__WARROOM_NATIVE_CONFIG__ = nativeCfg;
            window.localStorage.setItem("warroom.runtime-config.v1", JSON.stringify(durableCfg));
            window.__WARROOM_IOS_NATIVE__ = { bridge: "launch-injection", version: "0.1.0" };
            window.dispatchEvent(new CustomEvent("warroom-native-config-ready"));
            window.dispatchEvent(new CustomEvent("warroom-config-changed"));
          } catch (error) {
            console.warn("War Room iOS config injection failed", error);
          }
        })();
        """
        return WKUserScript(source: source, injectionTime: .atDocumentStart, forMainFrameOnly: true)
    }

    final class Coordinator: NSObject, WKNavigationDelegate {
        let model: WarRoomModel
        var loadedReloadID: UUID?
        var loadedConfig: WarRoomConfig?

        init(model: WarRoomModel) {
            self.model = model
        }

        func webView(
            _ webView: WKWebView,
            decidePolicyFor navigationAction: WKNavigationAction,
            decisionHandler: @escaping (WKNavigationActionPolicy) -> Void
        ) {
            guard let url = navigationAction.request.url else {
                decisionHandler(.cancel)
                return
            }
            let selection = WarRoomRuntimeSelection.validate(model.config)
            guard !selection.isBlocked else {
                Task { @MainActor in
                    self.model.markWebBlocked("Invalid configuration. \(selection.blockingMessage)")
                }
                decisionHandler(.cancel)
                return
            }
            if selection.config.allowsNavigation(to: url) {
                decisionHandler(.allow)
            } else {
                Task { @MainActor in
                    self.model.markWebBlocked(url.absoluteString)
                }
                decisionHandler(.cancel)
            }
        }

        func webView(_ webView: WKWebView, didStartProvisionalNavigation navigation: WKNavigation!) {
            Task { @MainActor in
                model.markWebLoading()
            }
        }

        func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
            Task { @MainActor in
                DebugLog.write("web loaded")
                model.markWebLoaded()
            }
        }

        func webView(_ webView: WKWebView, didFail navigation: WKNavigation!, withError error: Error) {
            Task { @MainActor in
                DebugLog.write("web failed \(error.localizedDescription)")
                model.markWebFailed(error.localizedDescription)
            }
        }

        func webView(_ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!, withError error: Error) {
            Task { @MainActor in
                DebugLog.write("web provisional failed \(error.localizedDescription)")
                model.markWebFailed(error.localizedDescription)
            }
        }

        func webView(
            _ webView: WKWebView,
            didReceive challenge: URLAuthenticationChallenge,
            completionHandler: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void
        ) {
            let protection = challenge.protectionSpace
            guard protection.authenticationMethod == NSURLAuthenticationMethodServerTrust else {
                completionHandler(.performDefaultHandling, nil)
                return
            }

            let selection = WarRoomRuntimeSelection.validate(model.config)
            guard !selection.isBlocked else {
                DebugLog.write("web tls rejected host=\(protection.host) reason=invalid-config")
                completionHandler(.cancelAuthenticationChallenge, nil)
                return
            }

            switch selection.config.tlsPinDecision(
                for: protection.host,
                serverTrust: protection.serverTrust
            ) {
            case .platformDefault:
                DebugLog.write("web tls platform-default host=\(protection.host)")
                completionHandler(.performDefaultHandling, nil)
            case .usePinnedTrust:
                guard let trust = protection.serverTrust else {
                    DebugLog.write("web tls pin rejected host=\(protection.host) reason=missing-trust")
                    completionHandler(.cancelAuthenticationChallenge, nil)
                    return
                }
                DebugLog.write("web tls pinned host=\(protection.host)")
                completionHandler(.useCredential, URLCredential(trust: trust))
            case .reject:
                DebugLog.write("web tls pin rejected host=\(protection.host)")
                completionHandler(.cancelAuthenticationChallenge, nil)
            }
        }
    }
}
