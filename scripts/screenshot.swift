// Helper for scripts/screenshot.sh: finds Kuana's window, sizes it and brings it to the front.
// Uses only system frameworks so taking screenshots needs no Homebrew packages.
//
//   screenshot-helper window <pid>          print the CGWindowID of the pid's main window
//   screenshot-helper resize <pid> <w> <h>  set that window's size in points (Accessibility)
//   screenshot-helper activate <pid>        bring the app to the front

import AppKit
import ApplicationServices

func fail(_ message: String) -> Never {
    FileHandle.standardError.write((message + "\n").data(using: .utf8)!)
    exit(1)
}

/// The largest normal-layer, on-screen window of `pid` (Kuana may also have the About window open).
func mainWindow(pid: pid_t) -> CGWindowID? {
    let info = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID)
        as? [[String: Any]] ?? []
    return info.compactMap { window -> (CGWindowID, CGFloat)? in
        guard (window[kCGWindowOwnerPID as String] as? pid_t) == pid,
              (window[kCGWindowLayer as String] as? Int) == 0,
              let id = window[kCGWindowNumber as String] as? CGWindowID,
              let bounds = window[kCGWindowBounds as String] as? NSDictionary,
              let rect = CGRect(dictionaryRepresentation: bounds),
              rect.width > 100, rect.height > 100
        else { return nil }
        return (id, rect.width * rect.height)
    }.max { $0.1 < $1.1 }?.0
}

func resize(pid: pid_t, width: CGFloat, height: CGFloat) {
    guard AXIsProcessTrusted() else {
        fail("can't resize: give your terminal Accessibility access (System Settings → Privacy & Security)")
    }
    let app = AXUIElementCreateApplication(pid)
    var value: CFTypeRef?
    guard AXUIElementCopyAttributeValue(app, kAXMainWindowAttribute as CFString, &value) == .success,
          let window = value
    else { fail("can't find the window to resize") }
    var size = CGSize(width: width, height: height)
    let axSize = AXValueCreate(.cgSize, &size)!
    AXUIElementSetAttributeValue(window as! AXUIElement, kAXSizeAttribute as CFString, axSize)
}

let args = CommandLine.arguments
func pidArg() -> pid_t {
    guard args.count > 2, let pid = pid_t(args[2]) else { fail("expected a pid") }
    return pid
}

switch args.count > 1 ? args[1] : "" {
case "window":
    guard let window = mainWindow(pid: pidArg()) else { exit(1) }
    print(window)
case "resize":
    guard args.count == 5, let w = Double(args[3]), let h = Double(args[4]) else { fail("usage: resize <pid> <w> <h>") }
    resize(pid: pidArg(), width: w, height: h)
case "activate":
    let pid = pidArg()
    // Since macOS 14, NSRunningApplication.activate is only a request that the frontmost app
    // (the terminal) may ignore; Accessibility's frontmost attribute is honored.
    if AXIsProcessTrusted() {
        AXUIElementSetAttributeValue(AXUIElementCreateApplication(pid), kAXFrontmostAttribute as CFString, kCFBooleanTrue)
    } else {
        guard let app = NSRunningApplication(processIdentifier: pid) else { fail("no such process") }
        app.activate()
    }
default:
    fail("usage: screenshot-helper window|resize|activate …")
}
