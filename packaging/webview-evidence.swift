#!/usr/bin/env swift
// Deterministic packaged War Room webview evidence:
//   capture  — locate the host PID's Council War Room window, screenshot only that
//              window, OCR it, require app-specific markers (fail closed).
//   verify   — OCR an existing PNG and apply the same marker predicate.
//   selftest — unit-test the predicate + reject a known-bad image if provided.
//
// Never dumps free-form OCR of unrelated windows. Only required-marker hits/misses
// are reported so a false capture cannot leak foreign desktop text into receipts.

import AppKit
import CoreGraphics
import Foundation
import Vision

// MARK: - Marker predicate

/// App-specific War Room chrome that must appear in a truthful screenshot.
/// OCR is noisy; accept common spacing variants.
let requiredMarkers: [String] = [
  "COUNCIL",
  "WAR ROOM",
  "DELIBERATE",
  "DIRECT FIRE",
]

/// Minimum distinct required markers that must match. 3 of 4 resists partial OCR
/// noise while rejecting unrelated desktops (e.g. a Kimi terminal pane).
let minRequiredHits = 3

func normalizeOCR(_ text: String) -> String {
  let upper = text.uppercased()
  // Collapse whitespace and common OCR punctuation between words.
  let scalars = upper.unicodeScalars.map { s -> Character in
    if CharacterSet.alphanumerics.contains(s) { return Character(s) }
    return " "
  }
  let joined = String(scalars)
  return joined.split(whereSeparator: { $0.isWhitespace }).joined(separator: " ")
}

struct MarkerResult {
  let hits: [String]
  let misses: [String]
  var ok: Bool { hits.count >= minRequiredHits }
}

func evaluateMarkers(in ocrText: String) -> MarkerResult {
  let norm = normalizeOCR(ocrText)
  var hits: [String] = []
  var misses: [String] = []
  for m in requiredMarkers {
    let needle = normalizeOCR(m)
    if norm.contains(needle) {
      hits.append(m)
    } else {
      misses.append(m)
    }
  }
  return MarkerResult(hits: hits, misses: misses)
}

// MARK: - OCR

func ocrImage(at path: String) throws -> String {
  let url = URL(fileURLWithPath: path)
  guard let img = NSImage(contentsOf: url) else {
    throw EvidenceError.loadFailed(path)
  }
  guard let tiff = img.tiffRepresentation,
        let rep = NSBitmapImageRep(data: tiff),
        let cg = rep.cgImage
  else {
    throw EvidenceError.decodeFailed(path)
  }
  let request = VNRecognizeTextRequest()
  request.recognitionLevel = .accurate
  request.usesLanguageCorrection = false
  request.recognitionLanguages = ["en-US"]
  let handler = VNImageRequestHandler(cgImage: cg, options: [:])
  try handler.perform([request])
  let lines = (request.results ?? []).compactMap { $0.topCandidates(1).first?.string }
  return lines.joined(separator: "\n")
}

// MARK: - Window targeting

struct WindowHit {
  let windowID: CGWindowID
  let ownerName: String
  let title: String
  let pid: pid_t
  let width: Int
  let height: Int
}

func listCandidateWindows(ownerPid: pid_t) -> [WindowHit] {
  let opts = CGWindowListOption(arrayLiteral: .optionOnScreenOnly, .excludeDesktopElements)
  guard let info = CGWindowListCopyWindowInfo(opts, kCGNullWindowID) as? [[String: Any]] else {
    return []
  }
  var hits: [WindowHit] = []
  for w in info {
    let pid = (w[kCGWindowOwnerPID as String] as? NSNumber)?.int32Value ?? 0
    guard pid == ownerPid else { continue }
    let num = (w[kCGWindowNumber as String] as? NSNumber)?.uint32Value ?? 0
    guard num > 0 else { continue }
    let owner = (w[kCGWindowOwnerName as String] as? String) ?? ""
    let title = (w[kCGWindowName as String] as? String) ?? ""
    var width = 0
    var height = 0
    if let bounds = w[kCGWindowBounds as String] as? [String: Any] {
      width = Int((bounds["Width"] as? NSNumber)?.doubleValue ?? 0)
      height = Int((bounds["Height"] as? NSNumber)?.doubleValue ?? 0)
    }
    // Ignore tiny utility surfaces.
    if width > 0 && height > 0 && (width < 200 || height < 150) { continue }
    hits.append(WindowHit(
      windowID: num,
      ownerName: owner,
      title: title,
      pid: pid,
      width: width,
      height: height
    ))
  }
  return hits
}

func isWarRoomIdentity(owner: String, title: String) -> Bool {
  let o = owner.lowercased()
  let t = title.lowercased()
  if o.contains("council war room") { return true }
  if t.contains("council war room") { return true }
  if o.contains("council-warroom-tauri") && (t.contains("war room") || t.contains("council")) {
    return true
  }
  // Empty title is common for some WKWebView layers; owner name still binds us.
  if o == "council war room" { return true }
  if o.contains("council-warroom") { return true }
  return false
}

func selectWarRoomWindow(ownerPid: pid_t) throws -> WindowHit {
  let candidates = listCandidateWindows(ownerPid: ownerPid)
  if candidates.isEmpty {
    throw EvidenceError.noWindow(ownerPid)
  }
  let identified = candidates.filter { isWarRoomIdentity(owner: $0.ownerName, title: $0.title) }
  // Prefer titled main window; fall back to largest owner-matched surface.
  let pool = identified.isEmpty ? candidates : identified
  let ranked = pool.sorted { ($0.width * $0.height) > ($1.width * $1.height) }
  guard let best = ranked.first else {
    throw EvidenceError.noWindow(ownerPid)
  }
  // Fail closed if nothing looks like War Room identity and we only have
  // anonymous layers — still allow when owner PID is our launched host and
  // owner name matches executable/bundle branding.
  if identified.isEmpty {
    let owner = best.ownerName.lowercased()
    let okOwner =
      owner.contains("council")
      || owner.contains("warroom")
      || owner.contains("war room")
    if !okOwner {
      throw EvidenceError.wrongWindow(
        "pid=\(ownerPid) owner=\(best.ownerName) title=\(best.title) (not War Room identity)"
      )
    }
  }
  return best
}

// MARK: - Capture

func captureWindow(id: CGWindowID, to path: String) throws {
  let fm = FileManager.default
  if fm.fileExists(atPath: path) {
    try fm.removeItem(atPath: path)
  }
  let proc = Process()
  proc.executableURL = URL(fileURLWithPath: "/usr/sbin/screencapture")
  // -x silent, -l window id, -o no shadow so bounds match app chrome
  proc.arguments = ["-x", "-o", "-l\(id)", path]
  try proc.run()
  proc.waitUntilExit()
  guard proc.terminationStatus == 0 else {
    throw EvidenceError.captureFailed("screencapture exit \(proc.terminationStatus) window=\(id)")
  }
  guard fm.fileExists(atPath: path),
        let attrs = try? fm.attributesOfItem(atPath: path),
        let size = attrs[.size] as? NSNumber,
        size.intValue > 1024
  else {
    throw EvidenceError.captureFailed("screenshot missing or too small at \(path)")
  }
}

// MARK: - Errors / CLI

enum EvidenceError: Error, CustomStringConvertible {
  case loadFailed(String)
  case decodeFailed(String)
  case noWindow(pid_t)
  case wrongWindow(String)
  case captureFailed(String)
  case markersFailed(hits: [String], misses: [String])
  case usage(String)

  var description: String {
    switch self {
    case .loadFailed(let p): return "failed to load image: \(p)"
    case .decodeFailed(let p): return "failed to decode image: \(p)"
    case .noWindow(let pid): return "no on-screen window for packaged host pid=\(pid)"
    case .wrongWindow(let d): return "window is not Council War Room: \(d)"
    case .captureFailed(let d): return "window capture failed: \(d)"
    case .markersFailed(let hits, let misses):
      return "webview markers insufficient: hits=\(hits.joined(separator: ",")) misses=\(misses.joined(separator: ",")) need>=\(minRequiredHits)"
    case .usage(let d): return d
    }
  }
}

func printUsage() {
  fputs(
    """
    usage:
      webview-evidence.swift capture --pid <host_pid> --out <png>
      webview-evidence.swift verify --image <png>
      webview-evidence.swift selftest [--reject <bad_png>]

    """,
    stderr
  )
}

func parseArgs(_ args: [String]) -> [String: String] {
  var out: [String: String] = [:]
  var i = 0
  while i < args.count {
    let a = args[i]
    if a.hasPrefix("--"), i + 1 < args.count {
      out[String(a.dropFirst(2))] = args[i + 1]
      i += 2
    } else {
      i += 1
    }
  }
  return out
}

func reportVerify(path: String, result: MarkerResult, dims: (Int, Int)?) {
  if let d = dims {
    print("webview_image=\(path)")
    print("webview_pixels=\(d.0)x\(d.1)")
  } else {
    print("webview_image=\(path)")
  }
  print("webview_markers_hits=\(result.hits.joined(separator: ","))")
  print("webview_markers_misses=\(result.misses.joined(separator: ","))")
  print("webview_markers_ok=\(result.ok)")
  // Deliberately do not print free-form OCR text.
}

func imageDimensions(path: String) -> (Int, Int)? {
  guard let img = NSImage(contentsOfFile: path),
        let rep = img.representations.first as? NSBitmapImageRep
  else {
    // Fallback via sips-less NSImage size (may be points).
    if let img = NSImage(contentsOfFile: path) {
      return (Int(img.size.width), Int(img.size.height))
    }
    return nil
  }
  return (rep.pixelsWide, rep.pixelsHigh)
}

func cmdVerify(image: String) throws {
  let text = try ocrImage(at: image)
  let result = evaluateMarkers(in: text)
  reportVerify(path: image, result: result, dims: imageDimensions(path: image))
  if !result.ok {
    throw EvidenceError.markersFailed(hits: result.hits, misses: result.misses)
  }
}

func cmdCapture(pid: pid_t, out: String) throws {
  // Retry briefly: window may appear after health is already up.
  var lastError: Error?
  var hit: WindowHit?
  for _ in 0..<40 {
    do {
      hit = try selectWarRoomWindow(ownerPid: pid)
      break
    } catch {
      lastError = error
      Thread.sleep(forTimeInterval: 0.25)
    }
  }
  guard let window = hit else {
    throw lastError ?? EvidenceError.noWindow(pid)
  }
  print("webview_window_id=\(window.windowID)")
  print("webview_window_owner=\(window.ownerName)")
  // Title is app chrome only; safe to log.
  print("webview_window_title=\(window.title)")
  print("webview_window_pid=\(window.pid)")
  print("webview_window_size=\(window.width)x\(window.height)")
  try captureWindow(id: window.windowID, to: out)
  try cmdVerify(image: out)
  print("webview_capture_ok=true")
}

func renderFixturePNG(text: String, path: String) throws {
  let size = NSSize(width: 800, height: 400)
  let image = NSImage(size: size)
  image.lockFocus()
  NSColor.black.setFill()
  NSBezierPath.fill(NSRect(origin: .zero, size: size))
  let attrs: [NSAttributedString.Key: Any] = [
    .font: NSFont.systemFont(ofSize: 28, weight: .semibold),
    .foregroundColor: NSColor.white,
  ]
  let rect = NSRect(x: 40, y: 80, width: 720, height: 280)
  (text as NSString).draw(in: rect, withAttributes: attrs)
  image.unlockFocus()
  guard let tiff = image.tiffRepresentation,
        let rep = NSBitmapImageRep(data: tiff),
        let png = rep.representation(using: .png, properties: [:])
  else {
    throw EvidenceError.captureFailed("fixture render failed")
  }
  try png.write(to: URL(fileURLWithPath: path))
}

func cmdSelftest(rejectPath: String?) throws {
  // Unit: predicate only (no OCR).
  let goodOCR = """
  COUNCIL · WAR ROOM
  Deliberate
  Direct Fire
  Cabinets
  """
  let good = evaluateMarkers(in: goodOCR)
  guard good.ok, good.hits.count >= minRequiredHits else {
    throw EvidenceError.usage("selftest predicate failed on synthetic good text hits=\(good.hits)")
  }
  print("selftest_predicate_good=true hits=\(good.hits.joined(separator: ","))")

  let badOCR = """
  KIMI
  terminal
  codex
  Idle
  """
  let bad = evaluateMarkers(in: badOCR)
  guard !bad.ok else {
    throw EvidenceError.usage("selftest predicate incorrectly accepted Kimi-like text")
  }
  print("selftest_predicate_bad_rejected=true hits=\(bad.hits.count)")

  // Render a positive fixture and OCR it.
  let tmp = FileManager.default.temporaryDirectory
    .appendingPathComponent("warroom-webview-fixture-\(UUID().uuidString).png").path
  defer { try? FileManager.default.removeItem(atPath: tmp) }
  try renderFixturePNG(
    text: "COUNCIL · WAR ROOM\nDeliberate\nDirect Fire",
    path: tmp
  )
  try cmdVerify(image: tmp)
  print("selftest_fixture_ocr_ok=true")

  if let reject = rejectPath {
    let text = try ocrImage(at: reject)
    let result = evaluateMarkers(in: text)
    reportVerify(path: reject, result: result, dims: imageDimensions(path: reject))
    if result.ok {
      throw EvidenceError.usage("selftest expected reject image to fail markers: \(reject)")
    }
    print("selftest_reject_image_ok=true")
  }
  print("selftest_ok=true")
}

// MARK: - main

let args = Array(CommandLine.arguments.dropFirst())
guard let cmd = args.first else {
  printUsage()
  exit(2)
}
let flags = parseArgs(Array(args.dropFirst()))

do {
  switch cmd {
  case "capture":
    guard let pidStr = flags["pid"], let pid = pid_t(pidStr), let out = flags["out"] else {
      throw EvidenceError.usage("capture requires --pid and --out")
    }
    try cmdCapture(pid: pid, out: out)
  case "verify":
    guard let image = flags["image"] else {
      throw EvidenceError.usage("verify requires --image")
    }
    try cmdVerify(image: image)
  case "selftest":
    try cmdSelftest(rejectPath: flags["reject"])
  default:
    printUsage()
    exit(2)
  }
  exit(0)
} catch {
  fputs("ERROR: \(error)\n", stderr)
  exit(1)
}
