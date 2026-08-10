import AppKit
import ApplicationServices
import CoreGraphics
import Foundation
import Vision

struct Options {
    let pid: pid_t
    let output: String
    let required: [String]
}

func parseOptions() throws -> Options {
    var pid: pid_t?
    var output: String?
    var required: [String] = []
    var index = 1
    while index < CommandLine.arguments.count {
        let arg = CommandLine.arguments[index]
        guard index + 1 < CommandLine.arguments.count else {
            throw NSError(domain: "window-proof", code: 2,
                          userInfo: [NSLocalizedDescriptionKey: "missing value for \(arg)"])
        }
        let value = CommandLine.arguments[index + 1]
        switch arg {
        case "--pid":
            guard let parsedPid = pid_t(value), parsedPid > 0 else {
                throw NSError(domain: "window-proof", code: 2,
                              userInfo: [NSLocalizedDescriptionKey: "invalid PID: \(value)"])
            }
            pid = parsedPid
        case "--output": output = value
        case "--contains": required.append(value)
        default:
            throw NSError(domain: "window-proof", code: 2,
                          userInfo: [NSLocalizedDescriptionKey: "unknown argument \(arg)"])
        }
        index += 2
    }
    guard let resolvedPid = pid, resolvedPid > 0, let resolvedOutput = output else {
        throw NSError(domain: "window-proof", code: 2,
                      userInfo: [NSLocalizedDescriptionKey: "usage: --pid PID --output PNG --contains TEXT..."])
    }
    return Options(pid: resolvedPid, output: resolvedOutput, required: required)
}

/// Ranked CG windows owned by `pid`. On-screen-only matches capture; all surfaces
/// are used only to diagnose "exists but not visible" vs "no window at all".
func rankedWindows(for pid: pid_t, onScreenOnly: Bool) -> [(CGWindowID, Double)] {
    var opts: CGWindowListOption = [.excludeDesktopElements]
    if onScreenOnly {
        opts.insert(.optionOnScreenOnly)
    }
    guard let list = CGWindowListCopyWindowInfo(opts, kCGNullWindowID) as? [[String: Any]] else {
        return []
    }

    return list.compactMap { item -> (CGWindowID, Double)? in
        guard let owner = item[kCGWindowOwnerPID as String] as? Int,
              owner == Int(pid),
              let number = item[kCGWindowNumber as String] as? CGWindowID,
              let boundsDict = item[kCGWindowBounds as String] as? [String: Any],
              let bounds = CGRect(dictionaryRepresentation: boundsDict as CFDictionary)
        else { return nil }
        let area = Double(bounds.width * bounds.height)
        // Ignore zero-size chrome that CG sometimes lists for app shells.
        guard area > 1 else { return nil }
        return (number, area)
    }.sorted { $0.1 > $1.1 }
}

func largestOnScreenWindow(for pid: pid_t) -> CGWindowID? {
    rankedWindows(for: pid, onScreenOnly: true).first?.0
}

/// Accessibility can see a standard window when CoreGraphics cannot (common when
/// the invoking host lacks Screen Recording). Used only for fail diagnostics.
/// Requires both `AXWindow` role and `AXStandardWindow` subrole so dialogs,
/// floating panels, and other AX chrome do not trigger a Screen Recording hint.
func accessibilityHasStandardWindow(pid: pid_t) -> Bool {
    let app = AXUIElementCreateApplication(pid)
    var value: CFTypeRef?
    guard AXUIElementCopyAttributeValue(app, kAXWindowsAttribute as CFString, &value) == .success,
          let windows = value as? [AXUIElement]
    else {
        return false
    }
    for window in windows {
        var role: CFTypeRef?
        guard AXUIElementCopyAttributeValue(window, kAXRoleAttribute as CFString, &role) == .success,
              let roleName = role as? String,
              roleName == (kAXWindowRole as String)
        else {
            continue
        }
        var subrole: CFTypeRef?
        guard AXUIElementCopyAttributeValue(window, kAXSubroleAttribute as CFString, &subrole) == .success,
              let subroleName = subrole as? String,
              subroleName == (kAXStandardWindowSubrole as String)
        else {
            continue
        }
        return true
    }
    return false
}

func processAlive(_ pid: pid_t) -> Bool {
    kill(pid, 0) == 0
}

func diagnoseMissingOnScreenWindow(pid: pid_t) -> String {
    if !processAlive(pid) {
        return "no on-screen window for pid \(pid) (process is not running)"
    }
    let offScreen = rankedWindows(for: pid, onScreenOnly: false)
    if !offScreen.isEmpty {
        return "no on-screen window for pid \(pid) " +
            "(CG sees \(offScreen.count) surface(s) but none on-screen — " +
            "wake the display, un-minimize, or switch to the app's Space; " +
            "see docs/troubleshooting.md native visual proof)"
    }
    if accessibilityHasStandardWindow(pid: pid) {
        return "no on-screen window for pid \(pid) " +
            "(Accessibility sees a standard window but CoreGraphics does not — " +
            "grant Screen Recording to the host terminal that runs ship-check / " +
            "smoke-macos-tauri-app.sh, then re-run)"
    }
    return "no on-screen window for pid \(pid)"
}

func capture(window: CGWindowID, output: String) throws -> CGImage {
    let process = Process()
    process.executableURL = URL(fileURLWithPath: "/usr/sbin/screencapture")
    process.arguments = ["-x", "-l\(window)", output]
    try process.run()
    process.waitUntilExit()
    guard process.terminationStatus == 0,
          let source = NSImage(contentsOfFile: output),
          let image = source.cgImage(forProposedRect: nil, context: nil, hints: nil),
          image.width > 100, image.height > 100 else {
        throw NSError(domain: "window-proof", code: 3,
                      userInfo: [NSLocalizedDescriptionKey: "unable to capture application window; grant Screen Recording access"])
    }
    return image
}

func recognizedText(in image: CGImage) throws -> String {
    let request = VNRecognizeTextRequest()
    request.recognitionLevel = .accurate
    request.usesLanguageCorrection = true
    let handler = VNImageRequestHandler(cgImage: image, options: [:])
    try handler.perform([request])
    return (request.results ?? []).compactMap { observation in
        observation.topCandidates(1).first?.string
    }.joined(separator: "\n")
}

do {
    let options = try parseOptions()
    guard let window = largestOnScreenWindow(for: options.pid) else {
        throw NSError(domain: "window-proof", code: 5,
                      userInfo: [NSLocalizedDescriptionKey: diagnoseMissingOnScreenWindow(pid: options.pid)])
    }
    let image = try capture(window: window, output: options.output)
    let text = try recognizedText(in: image).lowercased()
    for required in options.required where !text.contains(required.lowercased()) {
        throw NSError(domain: "window-proof", code: 6,
                      userInfo: [NSLocalizedDescriptionKey: "required visible text missing: \(required)"])
    }
    print("native window proof: PASS (\(options.required.count) required surfaces)")
} catch {
    fputs("native window proof: FAIL: \(error.localizedDescription)\n", stderr)
    exit(1)
}
