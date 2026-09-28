// bite-crawl — detached worker process.
//
// Spawned by the Rust control plane (`bite index rebuild`, bulk tools).
// Modes:
//   --crawl-worker              30-day Mail window + 10-day backfill + mirrors
//   --bulk-worker --bulk-op …   one-shot bulk Mail operation
//   --probe-mail                doctor diagnostic (one-shot Mail OSA probe)
//
// ALL Mail access happens in THIS process through the OSA transport
// (NSAppleScript on a serialized executor — raw Apple Event sending returns
// silent null replies on macOS 27). The parent `bite` process never touches
// Mail.

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

func fatalArg(_ msg: String) -> Never {
    FileHandle.standardError.write(Data("[worker] \(msg)\n".utf8))
    exit(2)
}

// job_id stays a var so the SIGTERM handler (registered before arg parsing)
// can always name a state file even if a signal lands mid-parse.
var jobID = "crawl-\(Int(Date().timeIntervalSince1970))"

// ── signal handling: register BEFORE parsing/probing so a SIGTERM during
// arg parsing or --probe-mail hits our handler, not the default disposition ──
var termSource: DispatchSourceSignal?
let signalQueue = DispatchQueue.global()
termSource = DispatchSource.makeSignalSource(signal: SIGTERM, queue: signalQueue)
termSource?.setEventHandler {
    CrawlState.shared.stop()
    // flush any partial batch before reporting the cancel
    CrawlState.shared.runFlushPending()
    MailCrawler.writeState(jobID: jobID, state: "cancelled",
                           processed: CrawlState.shared.snapshot.processed,
                           found: CrawlState.shared.snapshot.found,
                           window: CrawlState.shared.currentWindow)
    exit(0)
}
termSource?.resume()
signal(SIGTERM, SIG_IGN)

// ── parse ──
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
    case "--unread":
        // explicit true|false: false scopes to already-read messages
        guard let raw = it.next(), let v = Bool(raw.lowercased()) else {
            fatalArg("--unread expects true|false")
        }
        bulkSelection.unread = v
    case "--older-than-days":
        guard let raw = it.next(), let v = Int(raw), v > 0 else {
            fatalArg("--older-than-days expects a positive integer")
        }
        bulkSelection.olderThanDays = v
    case "--set-read":
        guard let raw = it.next(), let v = Bool(raw.lowercased()) else {
            fatalArg("--set-read expects true|false")
        }
        setRead = v
    case "--set-flagged":
        guard let raw = it.next(), let v = Bool(raw.lowercased()) else {
            fatalArg("--set-flagged expects true|false")
        }
        setFlagged = v
    case "--set-junk":
        guard let raw = it.next(), let v = Bool(raw.lowercased()) else {
            fatalArg("--set-junk expects true|false")
        }
        setJunk = v
    case "--probe-mail":
        // Doctor diagnostic: can THIS binary read Mail data through OSA?
        // 0 accounts is a configuration state, not a TCC denial — report
        // it distinctly so doctor doesn't send users permission-chasing.
        let n = MailAE.probeMailAccounts()
        let verdict: String
        switch n {
        case .some(let c) where c > 0: verdict = "authorized"
        case .some(let c) where c == 0: verdict = "no_accounts"
        default: verdict = "denied_or_empty"
        }
        print("{\"mail_automation\": \"\(verdict)\", \"accounts\": \(n ?? -1)}")
        exit(0)
    default: break
    }
}
if workerMode == "bulk" {
    jobID = "bulk-\(Int(Date().timeIntervalSince1970))"
}

let progress: (String, Int, Int) -> Void = { state, processed, found in
    // Re-publish the last activity label instead of nil: coarse progress
    // reports must not clobber the wait-loop's attempt/diagnostic detail.
    MailCrawler.writeState(jobID: jobID, state: state, processed: processed, found: found,
                           window: CrawlState.shared.currentWindow)
    FileHandle.standardError.write(Data("[worker] \(state) processed=\(processed) found=\(found)\n".utf8))
}

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
    let crawl = MailCrawler.runCrawlWorker(windowDays: windowDays, storeBody: storeBody,
                                           mailboxFilter: mailboxFilter, progress: progress)
    if crawl.state == "done" {
        if mirrors {
            let staging = MailCrawler.stagingDir()
            // continue the mail job's batch numbering: restarting at 0 would
            // collide with (and drop) mail batches already staged
            Mirrors.calendar(staging: staging, jobID: jobID, seq: crawl.seq, progress: progress)
            Mirrors.reminders(staging: staging, jobID: jobID, seq: crawl.seq, progress: progress)
            Mirrors.contacts(staging: staging, jobID: jobID, seq: crawl.seq, progress: progress)
        }
        // only a genuinely done crawl reports done — never clobber a
        // terminal failed/cancelled state written by the walk
        progress("done", CrawlState.shared.snapshot.processed, CrawlState.shared.snapshot.found)
    }
default:
    break
}
exit(0)
