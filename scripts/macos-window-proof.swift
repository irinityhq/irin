import AppKit
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

func largestWindow(for pid: pid_t) -> CGWindowID? {
    guard let list = CGWindowListCopyWindowInfo(
        [.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID
    ) as? [[String: Any]] else { return nil }

    return list.compactMap { item -> (CGWindowID, Double)? in
        guard let owner = item[kCGWindowOwnerPID as String] as? Int,
              owner == Int(pid),
              let number = item[kCGWindowNumber as String] as? CGWindowID,
              let boundsDict = item[kCGWindowBounds as String] as? [String: Any],
              let bounds = CGRect(dictionaryRepresentation: boundsDict as CFDictionary)
        else { return nil }
        return (number, Double(bounds.width * bounds.height))
    }.max(by: { $0.1 < $1.1 })?.0
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
    guard let window = largestWindow(for: options.pid) else {
        throw NSError(domain: "window-proof", code: 5,
                      userInfo: [NSLocalizedDescriptionKey: "no on-screen window for pid \(options.pid)"])
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
