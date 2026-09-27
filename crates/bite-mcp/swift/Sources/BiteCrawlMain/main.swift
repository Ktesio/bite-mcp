// bite-crawl — detached worker process.
//
// Spawned by the Rust control plane (`bite index rebuild`, bulk tools).
// Modes:
//   --crawl-worker              30-day Mail window + 10-day backfill + mirrors
//   --bulk-worker --bulk-op …   one-shot bulk Mail operation
//
// ALL Mail Apple Events are sent from THIS process's helper child
// (bite-helper), whose TCC identity the user has already approved at the
// stable install path. The parent `bite` process never touches Mail.

import Foundation
import BiteCrawlCore

// ── args ──
var workerMode = "crawl"
var windowDays = 30
var mailboxFilter: String?
var storeBody = true
var mirrors = true
var bulkOp: String?
var bulkMailbox = "INBOX"
var bulkToMailbox: String?
var bulkAccount: String?
var bulkSelection = BulkSelection()
var setRead: Bool?
var setFlagged: Bool?
var setJunk: Bool?

var it = Array(CommandLine.arguments.dropFirst()).makeIterator()
while let arg = it.next() {
    switch arg {
    case "--crawl-worker": workerMode = "crawl"
    case "--bulk-worker": workerMode = "bulk"
    case "--window-days": windowDays = Int(it.next() ?? "") ?? 30
    case "--mailbox": mailboxFilter = it.next()
    case "--no-body": storeBody = false
    case "--no-mirrors": mirrors = false
    case "--bulk-op": bulkOp = it.next() ?? "mark"
    case "--bulk-mailbox": bulkMailbox = it.next() ?? "INBOX"
    case "--to-mailbox": bulkToMailbox = it.next()
    case "--bulk-account": bulkAccount = it.next()
    case "--unread": bulkSelection.unread = true
    case "--older-than-days": bulkSelection.olderThanDays = Int(it.next() ?? "")
    case "--set-read": setRead = Bool(it.next() ?? "true") ?? true
    case "--set-flagged": setFlagged = Bool(it.next() ?? "true") ?? true
    case "--set-junk": setJunk = Bool(it.next() ?? "true") ?? true
    default: break
    }
}

let jobID = workerMode + "-\(Int(Date().timeIntervalSince1970))"
let progress: (String, Int, Int) -> Void = { state, processed, found in
    MailCrawler.writeState(jobID: jobID, state: state, processed: processed, found: found, window: nil)
    FileHandle.standardError.write(Data("[worker] \(state) processed=\(processed) found=\(found)\n".utf8))
}

// ── signal handling: stop cleanly on cancel ──
var termSource: DispatchSourceSignal?
let signalQueue = DispatchQueue.global()
termSource = DispatchSource.makeSignalSource(signal: SIGTERM, queue: signalQueue)
termSource?.setEventHandler {
    CrawlState.shared.stop()
    MailCrawler.writeState(jobID: jobID, state: "cancelled", processed: CrawlState.shared.snapshot.processed, found: CrawlState.shared.snapshot.found, window: nil)
    exit(0)
}
termSource?.resume()
signal(SIGTERM, SIG_IGN)

// ── run ──
MailCrawler.writeState(jobID: jobID, state: "running", processed: 0, found: 0, window: nil)

switch workerMode {
case "bulk":
    MailBulk.runBulkWorker(
        op: bulkOp ?? "mark", accountName: bulkAccount, mailboxName: bulkMailbox,
        toMailboxName: bulkToMailbox, selection: bulkSelection,
        setRead: setRead, setFlagged: setFlagged, setJunk: setJunk
    ) { state, count in
        progress(state, count, 0)
    }
case "crawl":
    MailCrawler.runCrawlWorker(windowDays: windowDays, storeBody: storeBody, mailboxFilter: mailboxFilter, progress: progress)
    if !CrawlState.shared.isCancelled && mirrors {
        let staging = MailCrawler.stagingDir()
        let seq = SeqCounter(0)
        Mirrors.calendar(staging: staging, jobID: jobID, seq: seq, progress: progress)
        Mirrors.reminders(staging: staging, jobID: jobID, seq: seq, progress: progress)
        Mirrors.contacts(staging: staging, jobID: jobID, seq: seq, progress: progress)
    }
    progress("done", CrawlState.shared.snapshot.processed, CrawlState.shared.snapshot.found)
default:
    break
}
exit(0)
