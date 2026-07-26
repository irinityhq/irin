# War Room iOS Shell

This is the first iOS remote-operator spike for Council War Room. It is a thin
SwiftUI + WKWebView app that loads a trusted machine-hosted War Room URL and
connects to the host's Council backend. The iPhone is not a Council runtime:
providers, sessions, Gateway, Librarian, and sidecars stay on the Mac or trusted
host.

## Current Choice

Lead path: Swift WKWebView.

PWA remains the fastest browser-only check, but it cannot provide native
Keychain storage or a controlled native settings surface. Capacitor stays as a
bounded comparator if the Swift shell stalls; it should be judged on simulator
time, physical-device smoke time, origin clarity, Keychain/config-sync quality,
and whether plugins reduce code without weakening the trust boundary.

## Trust Boundary

- Hosted War Room URL is configured in native Settings.
- API, WS, Gateway, and Librarian bases are configured in native Settings.
- `COUNCIL_AUTH_TOKEN` is stored in Keychain, not app code.
- Non-sensitive endpoints are stored in `UserDefaults`.
- Optional TLS certificate SHA-256 pinning is stored in `UserDefaults` for
  trusted private/self-signed HTTPS smoke endpoints.
- The WKWebView receives runtime config through launch injection. The token is
  exposed only as the in-memory `window.__WARROOM_NATIVE_CONFIG__` override used
  by `warroom/web/lib/runtime-config.ts`; the `warroom.runtime-config.v1`
  localStorage copy is redacted with `authToken: ""`.
- The WKWebView uses a non-persistent data store. Native settings and Keychain
  are the durable source of truth; WebKit cache/localStorage should not be.
- Top-level navigation is limited to the configured War Room web origin. API,
  WS, Gateway, and Librarian origins are connection targets only.
- No native web message handlers are exposed in this spike. The bridge is
  one-way launch injection only.

## Remote Shell Limits

- The iPhone is UI only. Council, provider CLIs, sessions, runs, Gateway,
  Librarian, and sidecars stay on the trusted host.
- The host must stay awake and reachable on the private network or tailnet.
- Keep device access private; do not expose War Room to the public internet.
- Backgrounding the app, locking the phone, or changing networks can break the
  WebSocket.
- A dropped iOS/WKWebView connection can leave the host-side live run
  continuing. Reconnect starts a fresh proceeding from the last start payload;
  it does not resume the in-flight run.
- Live providers may keep spending while the UI is disconnected. Treat
  reconnect during live-provider mode as possible extra spend.
- This shell does not execute providers natively, store local session
  transcripts, or expose a native JavaScript bridge.

## Backend Exposure

### Product path: IRIN.app + Tailscale Serve

The normal real-device path is the installed IRIN.app on the Mac:

1. Start IRIN and wait for Council to report ready.
2. In Settings, enable private phone access. IRIN configures **Tailscale
   Serve** on dedicated HTTPS port **8443** (never Funnel, never claims 443)
   and shows the copyable `https://<host>.ts.net:8443` URL. Paste the full
   origin including the port — the shell preserves it for API/WSS bases.
3. In War Room on iPhone, enter that one HTTPS URL under **IRIN on your
   tailnet**. The app derives the matching API, WSS, and Gateway bases.
4. Enter the Council auth token in the native Keychain-backed field if the
   host requires it, then run **Test Connection**.

Council serves the packaged War Room export and `/api/*` + `/ws/*` from the
same loopback origin. Tailscale privately publishes that origin over HTTPS;
when Gateway Pack is enabled, IRIN also owns the `/watch` and `/health` proxy
paths. The Mac remains the runtime and must stay awake. Keep Funnel off and do
not publish the URL outside the tailnet.

The remaining proxy instructions below are development/fallback material. They
are not required for the installed all-in-one product.

Best first deployment shape is single-origin:

- `https://warroom.<tailnet>` serves the War Room frontend.
- `https://warroom.<tailnet>/api/*` proxies to `127.0.0.1:8765/api/*`.
- `wss://warroom.<tailnet>/ws/*` proxies to `127.0.0.1:8765/ws/*`.

Single-origin avoids extra browser CSP and CORS openings. If API/WS use a
separate origin, update the War Room CSP served by the frontend and set
`COUNCIL_CORS_ORIGINS` to exactly the War Room frontend origin.

Simulator-only local smoke can use loopback HTTP. The verified lane used
`8767` so it would not collide with a local `8765` canary:

```bash
cargo build --release
mkdir -p /tmp/council-ios-smoke-sessions /tmp/council-ios-smoke-runs
COUNCIL_DEV_NO_AUTH=1 \
COUNCIL_WS_SMOKE_ONLY=1 \
COUNCIL_SESSIONS_DIR=/tmp/council-ios-smoke-sessions \
COUNCIL_RUNS_DIR=/tmp/council-ios-smoke-runs \
COUNCIL_CORS_ORIGINS="http://127.0.0.1:3010" \
./target/release/council --serve --port 8767
```

In another shell:

```bash
cd warroom/web
NEXT_PUBLIC_API_BASE=http://127.0.0.1:8767 \
NEXT_PUBLIC_WS_BASE=ws://127.0.0.1:8767 \
NEXT_PUBLIC_GATEWAY_BASE=http://127.0.0.1:18080 \
npm run build:hosted
npm run start -- --hostname 127.0.0.1 --port 3010
```

Configure the app:

- War Room URL: `http://127.0.0.1:3010`
- API base: `http://127.0.0.1:8767`
- WS base: `ws://127.0.0.1:8767`
- Gateway base: `http://127.0.0.1:18080`
- Librarian base: `http://127.0.0.1:11435`

For a physical device, use HTTPS/WSS on a trusted private network or tailnet.
The shell rejects non-loopback HTTP/WS endpoints. Set `COUNCIL_AUTH_TOKEN`,
enter the same token in native Settings, and set `COUNCIL_CORS_ORIGINS` to the
exact HTTPS War Room frontend origin.

Portable physical-device origin pattern (product Tailscale Serve default):

- Frontend/API/WS origin:
  `https://irin.example.ts.net:8443`
- WebSocket origin:
  `wss://irin.example.ts.net:8443`
- Backend target: Council loopback behind Tailscale Serve
- CORS allowlist (when required):
  `COUNCIL_CORS_ORIGINS=https://irin.example.ts.net:8443`

Development proxy lane may still use another non-443 port (for example
`3443`) with an operator-local TLS pin; that path is not required for the
installed IRIN.app product Serve origin.

These are placeholders. The committed app keeps operator-specific tailnet
hostnames and IPs out of source; put real values in native Settings,
environment variables, local Xcode settings, or one-shot bootstrap config.

**ATS for self-signed private HTTPS:** `Info.plist` ships with only
`NSAllowsLocalNetworking`. For physical-device self-signed or tailnet
endpoints you will normally add an `NSExceptionDomains` entry (via Xcode
project settings or a local build script) for your specific host during
development. Never broaden ATS for production or public use.

## Build And Smoke

```bash
xcodebuild -downloadPlatform iOS
xcodebuild \
  -project warroom-ios/WarRoomiOS.xcodeproj \
  -scheme WarRoomiOS \
  -configuration Debug \
  -sdk iphonesimulator \
  -destination 'generic/platform=iOS Simulator' \
  -derivedDataPath /tmp/warroom-ios-build \
  CODE_SIGNING_ALLOWED=NO \
  build
```

If the simulator is booted:

```bash
xcrun simctl boot "iPhone 17 Pro" || true
xcodebuild \
  -project warroom-ios/WarRoomiOS.xcodeproj \
  -scheme WarRoomiOS \
  -configuration Debug \
  -sdk iphonesimulator \
  -destination 'platform=iOS Simulator,name=iPhone 17 Pro' \
  -derivedDataPath /tmp/warroom-ios-build \
  CODE_SIGNING_ALLOWED=NO \
  build
xcrun simctl install booted /tmp/warroom-ios-build/Build/Products/Debug-iphonesimulator/WarRoomiOS.app
xcrun simctl spawn booted defaults write com.innerway.warroom.ios warroom.ios.webBase http://127.0.0.1:3010
xcrun simctl spawn booted defaults write com.innerway.warroom.ios warroom.ios.apiBase http://127.0.0.1:8767
xcrun simctl spawn booted defaults write com.innerway.warroom.ios warroom.ios.wsBase ws://127.0.0.1:8767
xcrun simctl spawn booted defaults write com.innerway.warroom.ios warroom.ios.gatewayBase http://127.0.0.1:18080
xcrun simctl spawn booted defaults write com.innerway.warroom.ios warroom.ios.librarianBase http://127.0.0.1:11435
xcrun simctl launch booted com.innerway.warroom.ios
```

No-spend backend smoke:

```bash
curl -s -H 'Origin: http://127.0.0.1:3010' \
  -D - http://127.0.0.1:8767/api/health

node -e 'const ws=new WebSocket("ws://127.0.0.1:8767/ws/deliberate",["council"]);const t=setTimeout(()=>{console.error("timeout");process.exit(1)},5000);ws.addEventListener("open",()=>ws.send(JSON.stringify({type:"start",payload:{topic:"iOS doc smoke",cabinet_name:"quick",frame_check:false,smoke_only:true}})));ws.addEventListener("message",ev=>{console.log(String(ev.data));clearTimeout(t);ws.close();process.exit(0)});ws.addEventListener("error",err=>{console.error(err.message||err);process.exit(1)});'
```

The verified response included `Access-Control-Allow-Origin:
http://127.0.0.1:3010`, `/api/health` reported `ws_smoke_only: true`, and the
WebSocket emitted `session_started` for `smoke-session` without provider spend.

## Physical Device Smoke

A real iPhone build requires an Apple Development signing identity and a
development team. Keep the team id out of source; pass it at build time or keep
it in local Xcode signing settings.

```bash
export WARROOM_IOS_TEAM_ID="<APPLE_DEVELOPMENT_TEAM_ID>"
export WARROOM_IOS_DEVICE_ID="<IPHONE_UDID>"

xcodebuild \
  -project warroom-ios/WarRoomiOS.xcodeproj \
  -scheme WarRoomiOS \
  -configuration Debug \
  -destination "id=$WARROOM_IOS_DEVICE_ID" \
  -derivedDataPath /tmp/warroom-ios-device-build \
  -allowProvisioningUpdates \
  DEVELOPMENT_TEAM="$WARROOM_IOS_TEAM_ID" \
  build

xcrun devicectl device install app \
  --device "$WARROOM_IOS_DEVICE_ID" \
  /tmp/warroom-ios-device-build/Build/Products/Debug-iphoneos/WarRoomiOS.app
```

Before launching on a phone, expose the trusted host through HTTPS/WSS on the
private network or tailnet, start `council --serve` with `COUNCIL_AUTH_TOKEN`,
`COUNCIL_WS_SMOKE_ONLY=1`, and an exact `COUNCIL_CORS_ORIGINS` value for the War
Room frontend origin, then enter those HTTPS/WSS bases plus the token in native
Settings.

For live-provider device runs, leave `COUNCIL_WS_SMOKE_ONLY` unset and launch
the backend from an environment that contains the providers needed by the chosen
cabinet. In launchd jobs, source the operator shell env or export keys directly;
otherwise `/api/health` can correctly mark cabinets as missing even though the
key exists in an interactive shell.

The private-tailnet smoke pattern uses a single-origin self-signed HTTPS/WSS
proxy with certificate pinning. Replace the host, IP, token, team id, and
device id with local values.

```bash
export WARROOM_IOS_HOST="irin.example.ts.net"
export WARROOM_IOS_TAILNET_IP="<your-tailnet-ip>"
export WARROOM_IOS_ORIGIN="https://$WARROOM_IOS_HOST:3443"
export WARROOM_IOS_TOKEN="<local smoke token>"
export WARROOM_IOS_TEAM_ID="<APPLE_DEVELOPMENT_TEAM_ID>"
export WARROOM_IOS_DEVICE_ID="<IPHONE_UDID>"

openssl req -x509 -newkey rsa:2048 -sha256 -days 7 -nodes \
  -keyout /tmp/warroom-ios-tailnet-selfsigned.key \
  -out /tmp/warroom-ios-tailnet-selfsigned.crt \
  -subj "/CN=$WARROOM_IOS_HOST" \
  -addext "subjectAltName=DNS:$WARROOM_IOS_HOST,IP:$WARROOM_IOS_TAILNET_IP" \
  -addext "basicConstraints=critical,CA:FALSE" \
  -addext "keyUsage=critical,digitalSignature,keyEncipherment" \
  -addext "extendedKeyUsage=serverAuth"

openssl x509 -in /tmp/warroom-ios-tailnet-selfsigned.crt -outform DER \
  | shasum -a 256 \
  | awk '{print $1}' > /tmp/warroom-ios-tailnet-pin.txt
```

Backend:

```bash
cargo build --release
mkdir -p /tmp/council-ios-device-sessions /tmp/council-ios-device-runs
COUNCIL_AUTH_TOKEN="$WARROOM_IOS_TOKEN" \
COUNCIL_WS_SMOKE_ONLY=1 \
COUNCIL_SESSIONS_DIR=/tmp/council-ios-device-sessions \
COUNCIL_RUNS_DIR=/tmp/council-ios-device-runs \
COUNCIL_CORS_ORIGINS="$WARROOM_IOS_ORIGIN" \
./target/release/council --serve --port 8767
```

Hosted web:

```bash
cd warroom/web
NEXT_PUBLIC_API_BASE="$WARROOM_IOS_ORIGIN" \
NEXT_PUBLIC_WS_BASE="wss://$WARROOM_IOS_HOST:3443" \
NEXT_PUBLIC_GATEWAY_BASE="$WARROOM_IOS_ORIGIN" \
npm run build:hosted

NEXT_PUBLIC_API_BASE="$WARROOM_IOS_ORIGIN" \
NEXT_PUBLIC_WS_BASE="wss://$WARROOM_IOS_HOST:3443" \
NEXT_PUBLIC_GATEWAY_BASE="$WARROOM_IOS_ORIGIN" \
node node_modules/next/dist/bin/next start --hostname 127.0.0.1 --port 3010
```

Private HTTPS/WSS proxy:

```bash
cd /path/to/irin/council-rs
WARROOM_IOS_PROXY_HOST="$WARROOM_IOS_TAILNET_IP" \
WARROOM_IOS_PROXY_PORT=3443 \
WARROOM_IOS_PROXY_CERT=/tmp/warroom-ios-tailnet-selfsigned.crt \
WARROOM_IOS_PROXY_KEY=/tmp/warroom-ios-tailnet-selfsigned.key \
WARROOM_IOS_WEB_TARGET=http://127.0.0.1:3010 \
WARROOM_IOS_COUNCIL_TARGET=http://127.0.0.1:8767 \
node warroom-ios/tools/tailnet-smoke-proxy.mjs
```

Host-side no-spend probes:

```bash
curl -sk -H "Origin: $WARROOM_IOS_ORIGIN" \
  -H "Authorization: Bearer $WARROOM_IOS_TOKEN" \
  -D - "$WARROOM_IOS_ORIGIN/api/health"

WS_URL="wss://$WARROOM_IOS_HOST:3443/ws/deliberate" \
NODE_TLS_REJECT_UNAUTHORIZED=0 \
node -e 'const ws=new WebSocket(process.env.WS_URL,["council",`token.${process.env.WARROOM_IOS_TOKEN}`]);const t=setTimeout(()=>{console.error("timeout");process.exit(1)},5000);ws.addEventListener("open",()=>ws.send(JSON.stringify({type:"start",payload:{topic:"iOS device smoke",cabinet_name:"quick",frame_check:false,smoke_only:true}})));ws.addEventListener("message",ev=>{console.log(String(ev.data));clearTimeout(t);ws.close();process.exit(0)});ws.addEventListener("error",err=>{console.error(err.message||err);process.exit(1)});'
```

Signed device build and launch with one-time Debug bootstrap:

```bash
xcodebuild \
  -project warroom-ios/WarRoomiOS.xcodeproj \
  -scheme WarRoomiOS \
  -configuration Debug \
  -destination "id=$WARROOM_IOS_DEVICE_ID" \
  -derivedDataPath /tmp/warroom-ios-device-build \
  DEVELOPMENT_TEAM="$WARROOM_IOS_TEAM_ID" \
  build

xcrun devicectl device install app \
  --device "$WARROOM_IOS_DEVICE_ID" \
  /tmp/warroom-ios-device-build/Build/Products/Debug-iphoneos/WarRoomiOS.app

BOOTSTRAP_CONFIG_B64="$(node -e 'const fs=require("fs"); const cfg={webBase:process.env.WARROOM_IOS_ORIGIN,apiBase:process.env.WARROOM_IOS_ORIGIN,wsBase:`wss://${process.env.WARROOM_IOS_HOST}:3443`,gatewayBase:process.env.WARROOM_IOS_ORIGIN,librarianBase:process.env.WARROOM_IOS_ORIGIN,tlsCertSha256:fs.readFileSync("/tmp/warroom-ios-tailnet-pin.txt","utf8").trim(),authToken:process.env.WARROOM_IOS_TOKEN}; process.stdout.write(Buffer.from(JSON.stringify(cfg)).toString("base64"));')"

xcrun devicectl device process launch \
  --device "$WARROOM_IOS_DEVICE_ID" \
  --terminate-existing \
  --environment-variables "{\"WARROOM_BOOTSTRAP_CONFIG_B64\":\"$BOOTSTRAP_CONFIG_B64\"}" \
  com.innerway.warroom.ios
```

Then prove restart persistence by relaunching without bootstrap:

```bash
xcrun devicectl device process launch \
  --device "$WARROOM_IOS_DEVICE_ID" \
  --terminate-existing \
  com.innerway.warroom.ios
```

The verified restart loaded config from native storage, reused the Keychain
token, applied the TLS pin, loaded the War Room shell, and completed
`/api/health` plus `/ws/deliberate` smoke mode without provider spend.

Live-device UI notes from the same validation lane:

- Keep the phone as a thin remote shell; do not move provider execution or
  session storage into the app.
- The Deliberate screen intentionally has one **Convene the Council** action in
  the bottom sticky bar. It must remain a large tap target and account for
  `safe-area-inset-bottom` so WKWebView/iOS gesture handling does not intercept
  the tap.
- On live runs, the first provider event may take tens of seconds. The web UI
  should show an active startup heartbeat after the WebSocket opens so the
  operator does not read a cold provider start as a dead page.

Safari Web Inspector flow:

1. Build a Debug app. Debug sets `WKWebView.isInspectable = true` on iOS 16.4+.
2. Open Safari on the Mac.
3. Use Develop -> Simulator or device -> War Room.
4. Inspect localStorage and confirm `warroom.runtime-config.v1` is present.
5. Confirm `authToken` is empty in localStorage while authenticated REST/WS
   requests still succeed through the native in-memory override.

## Static Export Bundled Mode

Bundling is intentionally deferred until hosted instance smoke and exact-origin
CORS/CSP are proven on a device.
Current export command:

```bash
cd warroom/web
npm run build:tauri
test -f out/index.html
```

When promoted, copy or sync `warroom/web/out/` into the iOS app bundle and load
it through a deliberate local scheme or file URL. Re-check origin/CORS behavior
before allowing a `null` origin; avoid adding `null` to `COUNCIL_CORS_ORIGINS`
unless it is a temporary simulator-only proof.

## Verification Checklist

- `cd warroom/web && npm run lint`
- `cd warroom/web && npm run typecheck`
- `cd warroom/web && npm run build:hosted`
- `cd warroom/web && npm run build:tauri`
- `cd warroom/web && npm test`
- `xcodebuild -project warroom-ios/WarRoomiOS.xcodeproj -scheme WarRoomiOS -configuration Debug -sdk iphonesimulator -destination 'generic/platform=iOS Simulator' CODE_SIGNING_ALLOWED=NO build`
- `xcodebuild -project warroom-ios/WarRoomiOS.xcodeproj -scheme WarRoomiOS -configuration Debug -destination 'generic/platform=iOS' DEVELOPMENT_TEAM=<team> build`
- Native Settings persists API/WS/Gateway/Librarian across app restart.
- Token persists across restart via Keychain and is not hardcoded or printed.
- TLS certificate pin persists across restart for private self-signed smoke.
- Web reload receives the injected runtime config.
- Navigation to an unconfigured origin is blocked.
- Backend uses `COUNCIL_WS_SMOKE_ONLY=1` for deliberation smoke.
- No `sessions/`, `runs/`, `librarian_chats/`, or provider transcripts are staged.
