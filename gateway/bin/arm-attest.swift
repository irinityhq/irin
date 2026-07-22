// arm-attest — dual-custody-local-attest host helper (spec §3, B7).
// Owns the Secure Enclave keypair + Touch ID prompt + ES256 signing.
// The sidecar never needs macOS APIs; this tool never talks to the network.
//
// Subcommands:
//   arm-attest enroll [--label <label>]
//       Create the biometry-gated SE key (.privateKeyUsage|.biometryCurrentSet),
//       persist the SE-wrapped blob at ~/.config/gateway/arm-attest.key (0600),
//       run a local test sign+verify (Touch ID prompt proves the gate works),
//       and print the registry credential record JSON on stdout.
//   arm-attest sign --challenge <b64> [--stage-id <id>]
//       Sign the VERBATIM decoded challenge bytes (never parsed, never
//       re-canonicalized — spec §5 verbatim-bytes invariant) and print the
//       confirm-body fragment JSON on stdout. --stage-id is DISPLAY ONLY
//       (the Touch ID prompt includes the short stage ID).
//
// Rate limit (prompt-rate invariant — phish-a-touch defense is
// mechanical, not operator vigilance): max 3 sign attempts per 300s window,
// tracked in the helper's OWN timestamp file, independent of the sidecar.
// The attempt is recorded BEFORE the Touch ID prompt fires, so canceled or
// failed prompts spend the budget too.
//
// Build: swiftc -O -o bin/arm-attest bin/arm-attest.swift   (bin/arm does
// this automatically when the binary is missing or older than this source).

import CryptoKit
import Foundation
import LocalAuthentication

let CONFIG_DIR = FileManager.default.homeDirectoryForCurrentUser
    .appendingPathComponent(".config/gateway", isDirectory: true)
let KEY_PATH = CONFIG_DIR.appendingPathComponent("arm-attest.key")
let RATELIMIT_PATH = CONFIG_DIR.appendingPathComponent("arm-attest.ratelimit")
let RATE_WINDOW_S: TimeInterval = 300
let RATE_MAX_ATTEMPTS = 3

func die(_ msg: String, code: Int32 = 1) -> Never {
    FileHandle.standardError.write(("arm-attest: " + msg + "\n").data(using: .utf8)!)
    exit(code)
}

func ensureConfigDir() {
    do {
        try FileManager.default.createDirectory(
            at: CONFIG_DIR, withIntermediateDirectories: true,
            attributes: [.posixPermissions: 0o700])
    } catch {
        die("failed to create config directory for rate-limit state: \(error)", code: 75)
    }
}

/// Token bucket on a flat timestamp file: prune entries older than the
/// window, refuse when the bucket is full, append the new attempt. Returns
/// only when the attempt is allowed (and already recorded).
func spendRateLimitToken() {
    ensureConfigDir()
    let now = Date().timeIntervalSince1970
    var stamps: [TimeInterval] = []
    if let raw = try? String(contentsOf: RATELIMIT_PATH, encoding: .utf8) {
        stamps = raw.split(separator: "\n").compactMap { TimeInterval($0) }
    }
    stamps = stamps.filter { now - $0 < RATE_WINDOW_S }
    if stamps.count >= RATE_MAX_ATTEMPTS {
        let retry = Int(RATE_WINDOW_S - (now - stamps.min()!)) + 1
        die("rate limit: \(RATE_MAX_ATTEMPTS) sign attempts per \(Int(RATE_WINDOW_S))s window exhausted — retry in ~\(retry)s. An attempt you did not initiate is an ALARM (phish-a-touch).", code: 75)
    }
    stamps.append(now)
    let out = stamps.map { String($0) }.joined(separator: "\n") + "\n"
    do {
        try out.data(using: .utf8)!.write(to: RATELIMIT_PATH, options: .atomic)
        try FileManager.default.setAttributes(
            [.posixPermissions: 0o600], ofItemAtPath: RATELIMIT_PATH.path)
    } catch {
        die("failed to persist rate-limit token (fail-closed): \(error)", code: 75)
    }
}

func accessControl() -> SecAccessControl {
    var err: Unmanaged<CFError>?
    guard let ac = SecAccessControlCreateWithFlags(
        nil, kSecAttrAccessibleWhenUnlockedThisDeviceOnly,
        [.privateKeyUsage, .biometryCurrentSet], &err) else {
        die("access control creation failed: \(err!.takeRetainedValue())")
    }
    return ac
}

/// credential_id = first 16 bytes of SHA-256(compressed SEC1 public key),
/// hex — deterministic, no registration server (spec §7.1), and the same
/// 32-hex shape the sidecar's stage_ids use.
func credentialId(_ publicKeyCompressed: Data) -> String {
    let digest = SHA256.hash(data: publicKeyCompressed)
    return digest.prefix(16).map { String(format: "%02x", $0) }.joined()
}

func jsonString(_ s: String) -> String {
    let data = try! JSONSerialization.data(withJSONObject: [s])
    let arr = String(data: data, encoding: .utf8)!
    return String(arr.dropFirst().dropLast())
}

func cmdEnroll(label: String) {
    guard SecureEnclave.isAvailable else { die("Secure Enclave unavailable on this host") }
    if FileManager.default.fileExists(atPath: KEY_PATH.path) {
        die("key blob already exists at \(KEY_PATH.path) — re-enrollment replaces the key; move the old blob aside first (archive, never delete)")
    }
    ensureConfigDir()
    let ctx = LAContext()
    ctx.localizedReason = "Enroll arm-attest signing key"
    let key: SecureEnclave.P256.Signing.PrivateKey
    do {
        key = try SecureEnclave.P256.Signing.PrivateKey(
            accessControl: accessControl(), authenticationContext: ctx)
    } catch {
        die("SE key creation failed: \(error)")
    }
    // Local test sign+verify — the Touch ID prompt here proves the biometric
    // gate works from THIS invocation context before anything is enrolled.
    let probe = "arm-attest enroll self-test".data(using: .utf8)!
    do {
        let sig = try key.signature(for: probe)
        guard key.publicKey.isValidSignature(sig, for: probe) else {
            die("self-test signature did not verify")
        }
    } catch {
        die("self-test sign failed (biometric gate?): \(error)")
    }
    do {
        try key.dataRepresentation.write(to: KEY_PATH, options: .atomic)
        try FileManager.default.setAttributes(
            [.posixPermissions: 0o600], ofItemAtPath: KEY_PATH.path)
    } catch {
        die("key blob write failed: \(error)")
    }
    let pub = key.publicKey.compressedRepresentation
    let iso = ISO8601DateFormatter().string(from: Date())
    print("""
    {"credential_id": \(jsonString(credentialId(pub))), "credential_type": "se-p256", "public_key": \(jsonString(pub.base64EncodedString())), "label": \(jsonString(label)), "enrolled_at": \(jsonString(iso))}
    """)
}

func cmdSign(challengeB64: String, stageId: String) {
    guard let challenge = Data(base64Encoded: challengeB64) else {
        die("--challenge is not valid base64")
    }
    guard let blob = try? Data(contentsOf: KEY_PATH) else {
        die("no key blob at \(KEY_PATH.path) — run 'arm-attest enroll' first")
    }
    spendRateLimitToken()
    let ctx = LAContext()
    let shortId = String(stageId.prefix(8))
    ctx.localizedReason = "Confirm arm stage \(shortId.isEmpty ? "(unknown)" : shortId)"
    let key: SecureEnclave.P256.Signing.PrivateKey
    do {
        key = try SecureEnclave.P256.Signing.PrivateKey(
            dataRepresentation: blob, authenticationContext: ctx)
    } catch {
        die("key blob load failed (enclave/biometry change invalidates the key — recover with the FIDO2 backup credential, then re-enroll): \(error)")
    }
    do {
        let sig = try key.signature(for: challenge) // Touch ID fires here
        let pub = key.publicKey.compressedRepresentation
        print("""
        {"credential_id": \(jsonString(credentialId(pub))), "credential_type": "se-p256", "signature": \(jsonString(sig.derRepresentation.base64EncodedString())), "authenticator_data": null}
        """)
    } catch {
        die("signing failed (canceled / wrong finger / -25308 detached context): \(error)")
    }
}

// ---------------------------------------------------------------------------

var args = Array(CommandLine.arguments.dropFirst())
guard let cmd = args.first else {
    die("usage: arm-attest enroll [--label <label>] | arm-attest sign --challenge <b64> [--stage-id <id>]")
}
args.removeFirst()

func flag(_ name: String) -> String? {
    guard let i = args.firstIndex(of: name), i + 1 < args.count else { return nil }
    return args[i + 1]
}

switch cmd {
case "enroll":
    cmdEnroll(label: flag("--label") ?? "sovereign-mac-touchid")
case "sign":
    guard let ch = flag("--challenge") else { die("sign requires --challenge <b64>") }
    cmdSign(challengeB64: ch, stageId: flag("--stage-id") ?? "")
default:
    die("unknown subcommand '\(cmd)'")
}
