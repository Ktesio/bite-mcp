import XCTest
import BiteCrawlCore

// Pure helpers of the OSA Mail transport (MailAE/MailCrawler/MailBulk) —
// script building, walk planning, reply mapping, terminal states. No Mail
// interaction happens here.
final class MailOSATests: XCTestCase {
    // ── script building ──

    func testQuotedAppleStringEscapes() {
        XCTAssertEqual(MailAE.quotedAppleString("plain"), "\"plain\"")
        XCTAssertEqual(MailAE.quotedAppleString("he said \"hi\""), "\"he said \\\"hi\\\"\"")
        XCTAssertEqual(MailAE.quotedAppleString("back\\slash"), "\"back\\\\slash\"")
        XCTAssertEqual(MailAE.quotedAppleString("Funnel 📥 INBOX"), "\"Funnel 📥 INBOX\"")
    }

    func testQuotedAppleStringControlCharacters() {
        XCTAssertEqual(MailAE.quotedAppleString("line1\nline2"), "\"line1\\nline2\"")
        XCTAssertEqual(MailAE.quotedAppleString("cr\rend"), "\"cr\\rend\"")
        XCTAssertEqual(MailAE.quotedAppleString("col\tvalue"), "\"col\\tvalue\"")
        // other scalars below 0x20 are stripped (no AppleScript escape exists)
        XCTAssertEqual(MailAE.quotedAppleString("a\u{01}b\u{1F}c"), "\"abc\"")
    }

    func testWhoseClauseEmptySelection() {
        XCTAssertEqual(MailAE.whoseClause(BulkSelection()), "")
    }

    func testWhoseClauseUnreadAndOlder() {
        let clause = MailAE.whoseClause(BulkSelection(unread: true, olderThanDays: 30))
        XCTAssertEqual(
            clause,
            " whose read status is false and date sent is less than or equal to ((current date) - 30 * days)"
        )
    }

    func testWhoseClauseReadSelection() {
        let clause = MailAE.whoseClause(BulkSelection(unread: false))
        XCTAssertEqual(clause, " whose read status is true")
    }

    func testIntStringVariants() {
        XCTAssertNil(MailAE.intString(nil))
        XCTAssertEqual(MailAE.intString(NSAppleEventDescriptor(string: "12345")), "12345")
        XCTAssertEqual(MailAE.intString(NSAppleEventDescriptor(int32: 542874)), "542874")
        XCTAssertEqual(MailAE.intString(NSAppleEventDescriptor(int32: 0)), "0")
    }

    func testIntStringWideAndRealDescriptors() {
        // 'comp' — 64-bit big-endian integer beyond Int32
        var compBits = UInt64(bitPattern: 3_500_000_000).bigEndian
        let comp = NSAppleEventDescriptor(descriptorType: DescType(0x636f6d70), bytes: &compBits, length: 8)!
        XCTAssertEqual(MailAE.intString(comp), "3500000000")
        // 'doub' — AppleScript widens overflowing integers to real
        var doubBits = (3_600_000_000.0 as Double).bitPattern.bigEndian
        let doub = NSAppleEventDescriptor(descriptorType: DescType(0x646f7562), bytes: &doubBits, length: 8)!
        XCTAssertEqual(MailAE.intString(doub), "3600000000")
    }

    // ── walk planning ──

    func testWalkPlanNewestFirstStartsWithProbeRange() {
        let (plan, probe, processProbeFirst) = MailAE.walkPlan(total: 105, walkLimit: 105, chunkSize: 40, newestFirst: true)
        XCTAssertEqual(plan.map { "\($0.lowerBound)-\($0.upperBound)" }, ["1-40", "41-80", "81-105"])
        XCTAssertTrue(processProbeFirst)
        // probe reads exactly plan[0]
        XCTAssertEqual(probe, 1...40)
        XCTAssertEqual(probe, plan[0])
    }

    func testWalkPlanOldestFirstCoversNewestChunkAndFullPlan() {
        let (plan, probe, processProbeFirst) = MailAE.walkPlan(total: 105, walkLimit: 105, chunkSize: 40, newestFirst: false)
        // walk starts at the far (newest) end…
        XCTAssertEqual(plan.first?.upperBound, 105)
        // …and the plan is NOT truncated: the tail (oldest) rows stay covered
        XCTAssertEqual(plan.map { "\($0.lowerBound)-\($0.upperBound)" }, ["81-105", "41-80", "1-40"])
        XCTAssertFalse(processProbeFirst)
        // probe is detection-only: the mailbox head
        XCTAssertEqual(probe, 1...40)
    }

    func testWalkPlanOldestFirstClampsToWalkLimit() {
        let (plan, _, _) = MailAE.walkPlan(total: 105, walkLimit: 50, chunkSize: 40, newestFirst: false)
        // walkLimit 50 → positions 56…105 (walk starts at the newest row),
        // chunks in descending walk order
        XCTAssertEqual(plan.map { "\($0.lowerBound)-\($0.upperBound)" }, ["96-105", "56-95"])
    }

    func testWalkPlanTotalWithinSingleChunk() {
        for newestFirst in [true, false] {
            let (plan, probe, processProbeFirst) = MailAE.walkPlan(total: 3, walkLimit: 20_000, chunkSize: 12, newestFirst: newestFirst)
            XCTAssertEqual(plan.map { "\($0.lowerBound)-\($0.upperBound)" }, ["1-3"], "newestFirst=\(newestFirst)")
            XCTAssertEqual(processProbeFirst, newestFirst)
            XCTAssertEqual(probe, 1...3)
        }
    }

    func testWalkPlanDegenerateDomainDoesNotCrash() {
        // total<=0 || walkLimit<=0 must yield an empty plan and a harmless
        // probe range (public API — callers guard, the planner must not
        // crash), and must NOT report the probe as processable (nothing
        // exists to process)
        for newestFirst in [true, false] {
            let (plan, probe, processProbeFirst) = MailAE.walkPlan(total: 0, walkLimit: 30, chunkSize: 12, newestFirst: newestFirst, avoidWidth: 7)
            XCTAssertTrue(plan.isEmpty)
            XCTAssertEqual(probe, 1...1)
            XCTAssertFalse(processProbeFirst)
            let (plan2, probe2, processProbeFirst2) = MailAE.walkPlan(total: 10, walkLimit: 0, chunkSize: 12, newestFirst: newestFirst, avoidWidth: 7)
            XCTAssertTrue(plan2.isEmpty)
            XCTAssertEqual(probe2, 1...1)
            XCTAssertFalse(processProbeFirst2)
        }
    }

    // ── order resolution (resample clears the defaulted flag) ──

    func testResolveOrderRecomputesDefaultedAfterResample() {
        let old = Date(timeIntervalSince1970: 500)
        let early = Date(timeIntervalSince1970: 1_000)
        let late = Date(timeIntervalSince1970: 2_000)
        // both head samples ambiguous, far end resolves → defaulted CLEARED
        // (position 1 older than the far end → oldest-first mailbox)
        let resampled = MailCrawler.resolveOrder(firstSample: early, secondSample: early, farEndSample: late)
        XCTAssertEqual(resampled.newestFirst, false)
        XCTAssertFalse(resampled.defaulted, "a successful resample must clear the defaulted flag")
        // far end also ambiguous → defaulted stays true
        let stuck = MailCrawler.resolveOrder(firstSample: nil, secondSample: nil, farEndSample: nil)
        XCTAssertEqual(stuck.newestFirst, true)
        XCTAssertTrue(stuck.defaulted)
        // head pair resolves directly → defaulted false, no resample needed
        let direct = MailCrawler.resolveOrder(firstSample: late, secondSample: early, farEndSample: nil)
        XCTAssertEqual(direct.newestFirst, true)
        XCTAssertFalse(direct.defaulted)
        // resample can also resolve to oldest-first
        let ascending = MailCrawler.resolveOrder(firstSample: old, secondSample: old, farEndSample: early)
        XCTAssertEqual(ascending.newestFirst, false)
        XCTAssertFalse(ascending.defaulted)
    }

    // ── blind pagination (count-timeout fallback) ──

    func testCountAttemptDecisionBranches() {
        // success → counted
        XCTAssertEqual(MailCrawler.countAttemptDecision(tryNumber: 1, total: 124_284), .counted(total: 124_284))
        XCTAssertEqual(MailCrawler.countAttemptDecision(tryNumber: 2, total: 0), .counted(total: 0))
        // first failure → one bounded retry
        XCTAssertEqual(MailCrawler.countAttemptDecision(tryNumber: 1, total: nil), .retryAfterBackoff)
        // second failure → blind walk, NOT a job failure
        XCTAssertEqual(MailCrawler.countAttemptDecision(tryNumber: 2, total: nil), .enterBlindWalk)
        XCTAssertEqual(MailCrawler.countAttemptDecision(tryNumber: 3, total: nil), .enterBlindWalk)
    }

    func testIsShortReadOnlyAppliesInBlindMode() {
        XCTAssertTrue(MailCrawler.isShortRead(rows: 5, requested: 12, blind: true))
        XCTAssertFalse(MailCrawler.isShortRead(rows: 12, requested: 12, blind: true))
        // boundaries: 0 rows (unreachable via one AE but guarded), 1 row,
        // and requested-1 are all short reads
        XCTAssertTrue(MailCrawler.isShortRead(rows: 0, requested: 12, blind: true))
        XCTAssertTrue(MailCrawler.isShortRead(rows: 1, requested: 12, blind: true))
        XCTAssertTrue(MailCrawler.isShortRead(rows: 11, requested: 12, blind: true))
        // counted mode: a short read is just data, never an end marker
        XCTAssertFalse(MailCrawler.isShortRead(rows: 5, requested: 12, blind: false))
    }

    // ── blind chunk halving + error classification ──

    func testHalveSplitsAndStopsAtWidthOne() {
        let h12 = MailCrawler.halve(1...12)
        XCTAssertEqual(h12?.lower, 1...6)
        XCTAssertEqual(h12?.upper, 7...12)
        XCTAssertNil(MailCrawler.halve(1...1), "width 1 cannot be halved")
        // full depth: 12 → 6+6 → 3+3 → 1+2 → 1+1 → stop; no width-7 piece
        // at ANY depth (7-wide bundle replies are structurally ambiguous)
        var widths: [Int] = [12]
        var queue: [ClosedRange<Int>] = [1...12]
        while let range = queue.first {
            queue.removeFirst()
            guard let halves = MailCrawler.halve(range) else { continue }
            queue.append(contentsOf: [halves.lower, halves.upper])
            widths.append(contentsOf: [halves.lower.count, halves.upper.count])
        }
        XCTAssertFalse(widths.contains(7), "no halved piece may be 7 wide: \(widths)")
        XCTAssertTrue(widths.allSatisfy { $0 >= 1 })
    }

    func testBlindReadDecisionClassifiesByErrorAndWidth() {
        // -1719 (Invalid index) at the frontier: halve while wide…
        XCTAssertEqual(MailCrawler.blindReadDecision(errorNumber: -1719, width: 12), .halve)
        XCTAssertEqual(MailCrawler.blindReadDecision(errorNumber: -1719, width: 2), .halve)
        // …and end-of-mailbox at the frontier's last position
        XCTAssertEqual(MailCrawler.blindReadDecision(errorNumber: -1719, width: 1), .endOfMailbox)
        // everything else is transient: -1712 timeouts, executor busy
        // (nil number), unusable replies — strike budget, never halving
        XCTAssertEqual(MailCrawler.blindReadDecision(errorNumber: -1712, width: 12), .transientStrike)
        XCTAssertEqual(MailCrawler.blindReadDecision(errorNumber: nil, width: 6), .transientStrike)
    }

    // ── direction-aware window edge ──

    func testRowDecisionDescendingEndsAtFromMs() {
        let from: Int64 = 1_000, to: Int64 = 2_000
        // newest-first walk (edge below fromMs): in-window collects…
        XCTAssertEqual(MailCrawler.rowDecision(ms: 1_500, fromMs: from, toMs: to, edgeAscending: false, staleSoFar: 0).decision, .collect)
        // …newer-than-window skips…
        XCTAssertEqual(MailCrawler.rowDecision(ms: 2_500, fromMs: from, toMs: to, edgeAscending: false, staleSoFar: 0).decision, .skip)
        // …and crossing below fromMs trips the edge after tolerance
        XCTAssertEqual(MailCrawler.rowDecision(ms: 500, fromMs: from, toMs: to, edgeAscending: false, staleSoFar: 0).decision, .skip)
        XCTAssertEqual(MailCrawler.rowDecision(ms: 500, fromMs: from, toMs: to, edgeAscending: false, staleSoFar: 2).decision, .edge)
    }

    func testRowDecisionAscendingEndsAtToMs() {
        let from: Int64 = 1_000, to: Int64 = 2_000
        // oldest-first walk (edge above toMs): pre-window rows are ordinary
        // SKIPS — the stale edge must NOT fire on them…
        XCTAssertEqual(MailCrawler.rowDecision(ms: 500, fromMs: from, toMs: to, edgeAscending: true, staleSoFar: 0).decision, .skip)
        XCTAssertEqual(MailCrawler.rowDecision(ms: 500, fromMs: from, toMs: to, edgeAscending: true, staleSoFar: 5).decision, .skip)
        // …in-window collects, and crossing ABOVE toMs trips the edge
        XCTAssertEqual(MailCrawler.rowDecision(ms: 1_500, fromMs: from, toMs: to, edgeAscending: true, staleSoFar: 0).decision, .collect)
        XCTAssertEqual(MailCrawler.rowDecision(ms: 2_500, fromMs: from, toMs: to, edgeAscending: true, staleSoFar: 0).decision, .skip)
        XCTAssertEqual(MailCrawler.rowDecision(ms: 2_500, fromMs: from, toMs: to, edgeAscending: true, staleSoFar: 2).decision, .edge)
    }

    func testBlindPlanCutsFromTheCap() {
        // blind walks plan from the walk cap alone (no total): the plan must
        // cover 1…cap in chunkSize pieces, still avoiding the 7-wide ambiguity
        let cap = 30
        let (plan, probe, processProbeFirst) = MailAE.walkPlan(total: cap, walkLimit: cap, chunkSize: 12, newestFirst: true, avoidWidth: 7)
        XCTAssertEqual(plan.map { "\($0.lowerBound)-\($0.upperBound)" }, ["1-12", "13-24", "25-30"])
        XCTAssertEqual(plan.reduce(0) { $0 + $1.count }, cap)
        XCTAssertEqual(probe, plan[0])  // probe range = first blind chunk
        XCTAssertTrue(processProbeFirst)
    }

    func testLastErrorNumberRoundtrip() {
        // plumbing for blind-mode error classification: the number must be
        // settable/clearable and independent of the message
        lastErrorNumber = -1719
        XCTAssertEqual(lastErrorNumber, -1719)
        lastError = "unrelated"
        XCTAssertEqual(lastErrorNumber, -1719)
        lastErrorNumber = nil
        XCTAssertNil(lastErrorNumber)
        lastError = nil
    }

    // ── transport diagnostics (abort-reason formatting) ──

    func testTransportDiagnostic() {
        // message already carries the PAREN-delimited number → not duplicated
        lastError = "Mail got an error (-1712)"
        lastErrorNumber = -1712
        XCTAssertEqual(MailCrawler.transportDiagnostic("fallback"), "Mail got an error (-1712)")
        // a BARE substring never suppresses: "-17120" must not hide "-1712"
        // (and "error -1712:" without parens still gets the number appended)
        lastError = "AppleScript error -17120: Mail got an error"
        lastErrorNumber = -1712
        XCTAssertEqual(
            MailCrawler.transportDiagnostic("fallback"),
            "AppleScript error -17120: Mail got an error (-1712)"
        )
        // production's executeScript format carries the number without
        // parens → paren-appended once (honest, never suppressible)
        lastError = "AppleScript error -1712: Mail got an error: AppleEvent timed out."
        lastErrorNumber = -1712
        XCTAssertEqual(
            MailCrawler.transportDiagnostic("fallback"),
            "AppleScript error -1712: Mail got an error: AppleEvent timed out. (-1712)"
        )
        // message without a number, number set → appended once
        lastError = "Mail got an error"
        lastErrorNumber = -1719
        XCTAssertEqual(MailCrawler.transportDiagnostic("fallback"), "Mail got an error (-1719)")
        // number cleared → message only
        lastError = "some failure"
        lastErrorNumber = nil
        XCTAssertEqual(MailCrawler.transportDiagnostic("fallback"), "some failure")
        // nothing recorded → the caller's fallback
        lastError = nil
        lastErrorNumber = nil
        XCTAssertEqual(MailCrawler.transportDiagnostic("unknown error"), "unknown error")
    }

    func testTransportDiagnosticSnapshotIsAtomic() {
        // (message, number) must be captured under ONE lock acquisition so
        // the pair always comes from the same execution: write the number
        // via the snapshot-visible path and verify a read sees a consistent
        // pair (message cleared + number set would be a mixed pair).
        lastError = "paired failure"
        lastErrorNumber = -42
        let snap = lastTransportError()
        XCTAssertEqual(snap.message, "paired failure")
        XCTAssertEqual(snap.number, -42)
        lastError = nil
        lastErrorNumber = nil
        let cleared = lastTransportError()
        XCTAssertNil(cleared.message)
        XCTAssertNil(cleared.number)
    }

    // ── terminal counters (failures/skipped persistence) ──

    func testTerminalCounters() {
        // failed bumps the failure streak and accumulates skips
        let failed = MailCrawler.terminalCounters(state: "failed", previousFailures: 2, previousSkipped: 1, newSkips: 2)
        XCTAssertEqual(failed.failures, 3)
        XCTAssertEqual(failed.skipped, 3)
        // done zeroes failures, keeps accumulating skips
        let done = MailCrawler.terminalCounters(state: "done", previousFailures: 2, previousSkipped: 0, newSkips: 1)
        XCTAssertEqual(done.failures, 0)
        XCTAssertEqual(done.skipped, 1)
        // cancelled preserves the failure streak
        let cancelled = MailCrawler.terminalCounters(state: "cancelled", previousFailures: 4, previousSkipped: 0, newSkips: 0)
        XCTAssertNil(cancelled.failures)
        XCTAssertEqual(cancelled.skipped, 0)
    }

    func testWalkPlanNeverIssuesAvoidWidthChunks() {
        // greedy cut: a would-be 7-wide piece shrinks by one and the
        // leftover becomes its own tiny piece
        let (plan, probe, _) = MailAE.walkPlan(total: 19, walkLimit: 19, chunkSize: 12, newestFirst: true, avoidWidth: 7)
        XCTAssertFalse(plan.contains { $0.count == 7 })
        XCTAssertEqual(plan.map { "\($0.lowerBound)-\($0.upperBound)" }, ["1-12", "13-18", "19-19"])
        XCTAssertEqual(probe, plan[0])  // newest-first probe stays plan[0]
        XCTAssertEqual(plan.reduce(0) { $0 + $1.count }, 19)

        // whole walk exactly 7 wide → shrunk piece + tiny remainder
        let (sPlan, sProbe, _) = MailAE.walkPlan(total: 7, walkLimit: 7, chunkSize: 12, newestFirst: true, avoidWidth: 7)
        XCTAssertEqual(sPlan.map { "\($0.lowerBound)-\($0.upperBound)" }, ["1-6", "7-7"])
        XCTAssertEqual(sProbe, sPlan[0])
    }

    func testWalkPlanDescendingSplitEmitsFarPieceFirst() {
        // a 7-message mailbox walked oldest-first (descending) must visit
        // the NEWEST rows first — ascending piece order would process the
        // oldest rows, trip the edge tolerance, and never index the newest
        let (plan, _, processProbeFirst) = MailAE.walkPlan(total: 7, walkLimit: 7, chunkSize: 12, newestFirst: false, avoidWidth: 7)
        XCTAssertEqual(plan.map { "\($0.lowerBound)-\($0.upperBound)" }, ["7-7", "1-6"])
        XCTAssertFalse(processProbeFirst)
        XCTAssertEqual(plan.first?.upperBound, 7)  // far end first
    }

    func testWalkPlanChunkSizeAvoidPlusOneKeepsInvariant() {
        // chunkSize == avoidWidth + 1 (8): every cut lands on 8s until the
        // remainder forces a shrink — the invariant must hold throughout
        for total in [15, 23, 31] {
            for newestFirst in [true, false] {
                let (plan, _, _) = MailAE.walkPlan(total: total, walkLimit: total, chunkSize: 8, newestFirst: newestFirst, avoidWidth: 7)
                XCTAssertFalse(plan.contains { $0.count == 7 }, "total=\(total) newestFirst=\(newestFirst): \(plan)")
                XCTAssertEqual(plan.reduce(0) { $0 + $1.count }, total, "coverage broken: \(plan)")
                for (a, b) in zip(plan, plan.dropFirst()) {
                    if newestFirst {
                        XCTAssertEqual(b.lowerBound, a.upperBound + 1, "ascending order broken: \(plan)")
                    } else {
                        XCTAssertEqual(a.lowerBound, b.upperBound + 1, "descending order broken: \(plan)")
                    }
                }
            }
        }
        // concrete shape for total=15, chunk=8: 8 + 6 + 1
        let (plan, _, _) = MailAE.walkPlan(total: 15, walkLimit: 15, chunkSize: 8, newestFirst: true, avoidWidth: 7)
        XCTAssertEqual(plan.map { "\($0.lowerBound)-\($0.upperBound)" }, ["1-8", "9-14", "15-15"])
    }

    // ── chunk failure policy ──

    func testFailureDecisionRetriesSameRange() {
        let range = 13...24
        // no refreshed count → treat as transient, retry the SAME range
        XCTAssertEqual(MailAE.failureDecision(range: range, refreshedTotal: nil, consecutiveFailures: 0), .retrySame)
        XCTAssertEqual(MailAE.failureDecision(range: range, refreshedTotal: 105, consecutiveFailures: 1), .retrySame)
    }

    func testFailureDecisionSkipsEvaporatedRangeWithoutStrike() {
        // mailbox shrank below the range start → advance, no failure counted
        XCTAssertEqual(MailAE.failureDecision(range: 96...105, refreshedTotal: 50, consecutiveFailures: 0), .advance)
    }

    func testFailureDecisionAbortsAfterThreeStrikes() {
        XCTAssertEqual(MailAE.failureDecision(range: 13...24, refreshedTotal: nil, consecutiveFailures: 3), .abort)
        XCTAssertEqual(MailAE.failureDecision(range: 13...24, refreshedTotal: 105, consecutiveFailures: 4), .abort)
    }

    // ── mailbox identity re-validation ──

    func testIdentityDecisionVerifiedAndMismatch() {
        // same name (case-insensitive) → verified
        XCTAssertEqual(
            MailAE.identityDecision(currentName: "INBOX", expected: "inbox", mailboxAt: 9,
                                    account: "iCloud", consecutiveFailures: 2),
            .verified
        )
        // different name → definitive abort, no strike budget
        if case .abort = MailAE.identityDecision(currentName: "Archive", expected: "INBOX", mailboxAt: 9,
                                                 account: "iCloud", consecutiveFailures: 0) {
        } else {
            XCTFail("mismatch must abort")
        }
    }

    func testIdentityDecisionNilIsTransientWithStrikeBudget() {
        // nil (executor busy / watchdog) retries with strikes…
        XCTAssertEqual(
            MailAE.identityDecision(currentName: nil, expected: "INBOX", mailboxAt: 9,
                                    account: "iCloud", consecutiveFailures: 0),
            .retrySame
        )
        XCTAssertEqual(
            MailAE.identityDecision(currentName: nil, expected: "INBOX", mailboxAt: 9,
                                    account: "iCloud", consecutiveFailures: 1),
            .retrySame
        )
        // …and aborts on the third strike
        if case .abort = MailAE.identityDecision(currentName: nil, expected: "INBOX", mailboxAt: 9,
                                                 account: "iCloud", consecutiveFailures: 2) {
        } else {
            XCTFail("third nil must abort")
        }
    }

    func testIdentityDecisionEmptyNameIsTransientNotDefinitive() {
        // a null reply coerces to "" — that is TRANSIENT (a retry succeeds),
        // not a definitive mismatch
        XCTAssertEqual(
            MailAE.identityDecision(currentName: "", expected: "INBOX", mailboxAt: 9,
                                    account: "iCloud", consecutiveFailures: 0),
            .retrySame
        )
        XCTAssertEqual(
            MailAE.identityDecision(currentName: "   ", expected: "INBOX", mailboxAt: 9,
                                    account: "iCloud", consecutiveFailures: 1),
            .retrySame
        )
        // strikes still budget it
        if case .abort = MailAE.identityDecision(currentName: "", expected: "INBOX", mailboxAt: 9,
                                                 account: "iCloud", consecutiveFailures: 2) {
        } else {
            XCTFail("third empty must abort")
        }
    }

    // ── zero-yield breaker (skip, not fail) ──

    func testZeroYieldSkipReason() {
        // mostly-unreadable rows with nothing indexed → skip diagnostic
        XCTAssertEqual(MailCrawler.zeroYieldSkipReason(unreadable: 501, outOfWindow: 0, indexed: 0),
                       "skipped: zero yield after 501 rows (unreadable 501, out-of-window 0)")
        // out-of-window dated skips feed the breaker: 300 unreadable + 250
        // pre-window rows with nothing indexed is a zero-yield walk too
        XCTAssertEqual(MailCrawler.zeroYieldSkipReason(unreadable: 300, outOfWindow: 250, indexed: 0),
                       "skipped: zero yield after 550 rows (unreadable 300, out-of-window 250)")
        // under the combined threshold → keep walking
        XCTAssertNil(MailCrawler.zeroYieldSkipReason(unreadable: 300, outOfWindow: 100, indexed: 0))
        // indexing something → keep walking
        XCTAssertNil(MailCrawler.zeroYieldSkipReason(unreadable: 900, outOfWindow: 0, indexed: 1))
        XCTAssertNil(MailCrawler.zeroYieldSkipReason(unreadable: 0, outOfWindow: 0, indexed: 0))
    }

    // ── scanChunk: the four mode × direction combinations (round-2 critical regression) ──

    /// Build a dated row for a synthetic mailbox.
    private func datedRow(id: Int, daysBeforeNow: Int) -> NSAppleEventDescriptor {
        let row = NSAppleEventDescriptor.record()
        row.setDescriptor(NSAppleEventDescriptor(int32: Int32(id)), forKeyword: MailAE.kwMessageID)
        row.setDescriptor(NSAppleEventDescriptor(date: Date(timeIntervalSince1970: 1_790_000_000 - Double(daysBeforeNow) * 86_400)),
                          forKeyword: MailAE.kwDateSent)
        row.setDescriptor(stringDesc("subject \(id)"), forKeyword: MailAE.kwSubject)
        return row
    }

    /// 40 dated rows. newest-first: position 1 = now (newest), position 40
    /// = now-39d. oldest-first: position 1 = now-39d, position 40 = now.
    private func syntheticMailbox(newestFirst: Bool, count: Int = 40) -> [NSAppleEventDescriptor?] {
        (1...count).map { i in
            let age = newestFirst ? (i - 1) : (count - i)
            return datedRow(id: 100 + i, daysBeforeNow: age)
        }
    }

    func testScanChunkFourCombinationTable() {
        // window 1 = last 30 days; backfill = the preceding 30 days (half-
        // open [from, to)). The 40 rows span now-39d…now with boundary rows
        // landing exactly on fromMs (included).
        let now: Double = 1_790_000_000
        let day: Double = 86_400
        let w1 = CrawlWindow(fromMs: Int64((now - 30 * day) * 1000), toMs: Int64(now * 1000))
        let w2 = CrawlWindow(fromMs: Int64((now - 60 * day) * 1000), toMs: Int64((now - 30 * day) * 1000))

        // counted newest-first: rows as-is (dates fall), below-fromMs edge.
        // W1: ages 1-30 in window, age 0 == toMs skips, ages 31-33 trip the
        // edge (the round-2 regression collected 0 here — the wrong polarity
        // tripped on ages 0-2, which are ABOVE toMs).
        // W2 (backfill): ages 0-29 are newer than toMs → skipped (no false
        // edge — that was the regression); age 30 sits exactly ON toMs →
        // excluded (half-open window); ages 31-39 are the slice → 9.
        // Out-of-window counters are FAR-SIDE gated: only skips beyond the
        // window's far edge (below fromMs for descending, above toMs for
        // ascending) feed the breaker — a large APPROACH-side prefix
        // (previous windows' volume) must never false-trip it.
        let expected: [(mode: String, order: String, window: String, collected: Int, scanned: Int, edge: Bool, outOfWindow: Int)] = [
            ("counted", "NF", "W1", 30, 34, true, 2),
            ("counted", "NF", "W2", 9, 40, false, 0),
            ("counted", "OF", "W1", 30, 34, true, 2),
            ("counted", "OF", "W2", 9, 40, false, 0),
            ("blind", "NF", "W1", 30, 34, true, 2),
            ("blind", "NF", "W2", 9, 40, false, 0),
            ("blind", "OF", "W1", 30, 40, false, 10),
            ("blind", "OF", "W2", 9, 12, true, 2),
        ]
        for (mode, order, windowName, wantCollected, wantScanned, wantEdge, wantOOW) in expected {
            let blind = mode == "blind"
            let newestFirst = order == "NF"
            let flags = MailCrawler.flagsForWalk(blind: blind, newestFirstResolved: newestFirst)
            XCTAssertEqual(flags.reverseRows, !blind && !newestFirst)
            XCTAssertEqual(flags.edgeAscending, blind && !newestFirst)
            let window = windowName == "W1" ? w1 : w2
            let scan = MailCrawler.scanChunk(syntheticMailbox(newestFirst: newestFirst), window: window,
                                             reverseRows: flags.reverseRows, edgeAscending: flags.edgeAscending,
                                             account: "a", mailbox: "m", includeContent: false)
            XCTAssertEqual(scan.records.count, wantCollected,
                           "\(mode)/\(order)/\(windowName): collected")
            XCTAssertEqual(scan.scanned, wantScanned, "\(mode)/\(order)/\(windowName): scanned")
            XCTAssertEqual(scan.edgeCrossed, wantEdge, "\(mode)/\(order)/\(windowName): edge")
            XCTAssertEqual(scan.skippedOutOfWindow, wantOOW, "\(mode)/\(order)/\(windowName): far-side skips")
        }
    }

    func testScanChunkDescendingBackfillApproachPrefixDoesNotTripBreaker() {
        // counted NF backfill with a LARGE above-toMs approach prefix (570
        // rows newer than the backfill window — previous windows' volume):
        // the far-side-gated breaker must NOT trip, and the deep in-window
        // rows must be collected.
        let now: Double = 1_790_000_000
        let w2 = CrawlWindow(fromMs: Int64((now - 60 * 86_400) * 1000), toMs: Int64((now - 30 * 86_400) * 1000))
        var rows: [NSAppleEventDescriptor?] = []
        for i in 0..<600 {
            rows.append(datedRow(id: 300 + i, daysBeforeNow: i))  // ages 0…599 days
        }
        let scan = MailCrawler.scanChunk(rows, window: w2, reverseRows: false, edgeAscending: false,
                                         account: "a", mailbox: "m", includeContent: false)
        let ids = scan.records.map { $0.id }
        // Half-open [fromMs, toMs): the collected slice is ages 31…60 (30
        // rows) — age 30 sits ON toMs (out), age 60 ON fromMs (in).
        XCTAssertEqual(scan.records.count, 30)
        XCTAssertEqual(ids.first, "331")
        XCTAssertEqual(ids.last, "360")
        // FAR-SIDE GATING: only the 2 rows beyond the far edge (ages 61,62)
        // feed the breaker — the 568-row approach prefix does not.
        XCTAssertEqual(scan.skippedOutOfWindow, 2)
        XCTAssertEqual(scan.skipped, 33)
        XCTAssertEqual(scan.scanned, 64)
        XCTAssertTrue(scan.edgeCrossed)          // stops below fromMs as designed
        XCTAssertNil(MailCrawler.zeroYieldSkipReason(unreadable: scan.unreadable,
                                                     outOfWindow: scan.skippedOutOfWindow,
                                                     indexed: scan.records.count),
                     "a 570-row approach prefix must not trip the zero-yield breaker")
    }

    func testScanChunkOrderMisDetectedTripwire() {
        // defaulted-order DESCENDING walk over an actually-ASCENDING
        // mailbox whose early rows sit INSIDE the backfill window: the
        // in-window block is collected (arming the tripwire), then every
        // later chunk lies entirely ABOVE toMs — those skips accumulate in
        // the WALK-LEVEL tail until it passes 3 chunks' worth (36) and
        // flags "order mis-detected". The tail must thread across scanChunk
        // calls: with a per-call local it resets every 12-row chunk and
        // never trips (the wiring this test pins).
        let now: Double = 1_790_000_000
        let w = CrawlWindow(fromMs: Int64((now - 60 * 86_400) * 1000), toMs: Int64((now - 30 * 86_400) * 1000))

        // ascending dates: chunk 1 fully in-window (ages 45…34 days),
        // later chunks climb above toMs into future-dated rows
        var allRows: [[NSAppleEventDescriptor?]] = []
        for chunk in 0..<5 {
            var rows: [NSAppleEventDescriptor?] = []
            for j in 0..<12 {
                let age = 45 - chunk * 12 - j  // 45…-14 days before now
                rows.append(datedRow(id: 500 + chunk * 12 + j, daysBeforeNow: age))
            }
            allRows.append(rows)
        }

        var tail = 0
        var totalRecords = 0
        var scan = MailCrawler.scanChunk(allRows[0], window: w, reverseRows: false, edgeAscending: false,
                                         account: "a", mailbox: "m", includeContent: false,
                                         orderMisDetectTail: 36, hasCollected: false,
                                         aboveWindowTail: tail)
        totalRecords += scan.records.count
        XCTAssertEqual(totalRecords, 12, "chunk 1 fully in-window")
        XCTAssertEqual(scan.aboveWindowTail, 0)
        XCTAssertFalse(scan.orderMisDetected)

        for chunk in 1..<5 {
            scan = MailCrawler.scanChunk(allRows[chunk], window: w, reverseRows: false, edgeAscending: false,
                                         account: "a", mailbox: "m", includeContent: false,
                                         orderMisDetectTail: 36, hasCollected: true,
                                         aboveWindowTail: scan.aboveWindowTail)
            totalRecords += scan.records.count
            tail = scan.aboveWindowTail
            // chunk 2: ages 33…22 (3 in-window at ages 33-31, then the
            // above-toMs run begins at age 30 == toMs); chunks 3-5 add 12
            // above rows each
            let wantTail = [9, 21, 33, 45][chunk - 1]
            XCTAssertEqual(tail, wantTail, "chunk \(chunk + 1): walk-level tail must thread")
            if chunk < 4 {
                XCTAssertFalse(scan.orderMisDetected, "chunk \(chunk + 1): tail \(tail) is at — not past — the 36 budget")
            } else {
                XCTAssertTrue(scan.orderMisDetected, "chunk 5: 42 consecutive above-toMs rows past the collected block must flag mis-detection")
            }
        }
        XCTAssertEqual(totalRecords, 15, "12 in-window + 3 in-window from chunk 2; chunks 3-5 are above-toMs skips (not collected)")

        // WIDE-CHUNK case (production never produces an 80-row chunk, but
        // this pins scanChunk's internal counting): 20 in-window + 60
        // above-toMs in ONE call trips in a single invocation.
        var rows: [NSAppleEventDescriptor?] = []
        for i in 0..<20 { rows.append(datedRow(id: 500 + i, daysBeforeNow: 55 - i)) }  // in-window
        for i in 0..<60 { rows.append(datedRow(id: 600 + i, daysBeforeNow: max(0, 20 - i))) }  // above-toMs tail
        let wide = MailCrawler.scanChunk(rows, window: w, reverseRows: false, edgeAscending: false,
                                         account: "a", mailbox: "m", includeContent: false,
                                         orderMisDetectTail: 36)
        XCTAssertTrue(wide.orderMisDetected, "60 above-toMs rows > 36 must flag mis-detection")
        XCTAssertEqual(wide.records.count, 20)
        XCTAssertFalse(wide.edgeCrossed)

        // the mirror shape (above-toMs block FIRST, nothing collected yet):
        // the tripwire stays disarmed — no collected row means the walk
        // hasn't proven the order wrong yet; pre-window/undated skips
        // govern instead.
        var aboveFirst: [NSAppleEventDescriptor?] = []
        for i in 0..<60 { aboveFirst.append(datedRow(id: 700 + i, daysBeforeNow: max(0, 20 - i))) }
        for i in 0..<20 { aboveFirst.append(datedRow(id: 800 + i, daysBeforeNow: 5)) }
        let scan2 = MailCrawler.scanChunk(aboveFirst, window: w, reverseRows: false, edgeAscending: false,
                                          account: "a", mailbox: "m", includeContent: false,
                                          orderMisDetectTail: 36)
        XCTAssertFalse(scan2.orderMisDetected)
    }

    func testScanChunkUnreadableRowsFeedBreaker() {
        // DESCENDING walk: pre-window rows trip the edge after tolerance —
        // they still feed skippedOutOfWindow on the way (2 stale skips here).
        let now: Double = 1_790_000_000
        let w = CrawlWindow(fromMs: Int64((now - 30 * 86_400) * 1000), toMs: Int64(now * 1000))
        var rows: [NSAppleEventDescriptor?] = []
        rows.append(nil)  // unreadable
        rows.append(NSAppleEventDescriptor.list())  // record-less → unreadable
        for i in 0..<40 {
            rows.append(datedRow(id: 200 + i, daysBeforeNow: 100 + i))  // all pre-window
        }
        let scan = MailCrawler.scanChunk(rows, window: w, reverseRows: false, edgeAscending: false,
                                         account: "a", mailbox: "m", includeContent: false)
        XCTAssertEqual(scan.records.count, 0)       // all pre-window → nothing indexed
        XCTAssertEqual(scan.unreadable, 2)
        XCTAssertEqual(scan.skippedOutOfWindow, 2)  // ages 100,101 before the edge
        XCTAssertEqual(scan.scanned, 5)             // 2 unreadable + 3 pre-window
        XCTAssertTrue(scan.edgeCrossed)

        // ASCENDING walk (the no-tripwire case — e.g. a blind-memoized old
        // mailbox): every pre-window row is a plain skip, ALL of them feed
        // the breaker counter, and the breaker then fires on the totals.
        let asc = MailCrawler.scanChunk(rows, window: w, reverseRows: false, edgeAscending: true,
                                        account: "a", mailbox: "m", includeContent: false)
        XCTAssertEqual(asc.records.count, 0)
        XCTAssertEqual(asc.unreadable, 2)
        XCTAssertEqual(asc.skippedOutOfWindow, 40)
        XCTAssertFalse(asc.edgeCrossed)
        XCTAssertNotNil(MailCrawler.zeroYieldSkipReason(unreadable: asc.unreadable,
                                                        outOfWindow: asc.skippedOutOfWindow,
                                                        indexed: asc.records.count,
                                                        threshold: 40))
    }

    // ── walk order detection ──

    func testOrderFromSamples() {
        let early = Date(timeIntervalSince1970: 1_000)
        let late = Date(timeIntervalSince1970: 2_000)
        XCTAssertEqual(MailAE.orderFromSamples(late, early), true)   // newest-first
        XCTAssertEqual(MailAE.orderFromSamples(early, late), false)  // oldest-first
        XCTAssertNil(MailAE.orderFromSamples(nil, late))             // missing sample
        XCTAssertNil(MailAE.orderFromSamples(late, nil))
        XCTAssertNil(MailAE.orderFromSamples(late, late))            // equal → ambiguous
    }

    // ── reply mapping ──

    private func stringDesc(_ s: String) -> NSAppleEventDescriptor { NSAppleEventDescriptor(string: s) }

    func testRowsFromReplyBareRecordIsSingleRow() {
        // `messages S thru S` answers a bare record, not a list
        let rec = NSAppleEventDescriptor.record()
        let rows = MailAE.rowsFromReply(rec, expectedProps: 7)
        XCTAssertEqual(rows.count, 1)
        XCTAssertEqual(rows[0]?.descriptorType, rec.descriptorType)
    }

    func testRowsFromReplyListOfRecords() {
        let list = NSAppleEventDescriptor.list()
        for i in 1...2 {
            let rec = NSAppleEventDescriptor.record()
            rec.setDescriptor(NSAppleEventDescriptor(int32: Int32(i)), forKeyword: MailAE.kwMessageID)
            list.insert(rec, at: Int(list.numberOfItems) + 1)
        }
        let rows = MailAE.rowsFromReply(list, expectedProps: 0)
        XCTAssertEqual(rows.count, 2)
    }

    func testRowsFromReplyMapsMssgTypedRows() {
        // LIVE REGRESSION: Mail types `get properties` rows 'mssg' (the
        // message class code), not 'reco' — a type-code gate returned no
        // rows for every with-body read ("probe read failed … unknown
        // error"). Rows must be classified and carried through.
        let mssgType = DescType(0x6d737367)  // 'mssg' — observed live
        let list = NSAppleEventDescriptor.list()
        for i in 1...3 {
            let rec = NSAppleEventDescriptor(descriptorType: mssgType, data: nil)!
            rec.setDescriptor(NSAppleEventDescriptor(int32: 542_000 + Int32(i)), forKeyword: MailAE.kwMessageID)
            rec.setDescriptor(stringDesc("subject \(i)"), forKeyword: MailAE.kwSubject)
            rec.setDescriptor(NSAppleEventDescriptor(date: Date(timeIntervalSince1970: 1_790_000_000)), forKeyword: MailAE.kwDateSent)
            list.insert(rec, at: Int(list.numberOfItems) + 1)
        }
        let rows = MailAE.rowsFromReply(list, expectedProps: 8)
        XCTAssertEqual(rows.count, 3, "mssg-typed rows must survive reply mapping")
        XCTAssertNotNil(rows[0])
    }

    func testIsRecordRowDetectsMailRowTypes() {
        // Mail's row type 'mssg' and the generic 'reco' are both records
        XCTAssertTrue(isRecordRow(NSAppleEventDescriptor(descriptorType: DescType(0x6d737367), data: nil)))
        XCTAssertTrue(isRecordRow(NSAppleEventDescriptor(descriptorType: DescType(0x7265636f), data: nil)))
        // a record carrying the id keyword is detected functionally too
        let keyed = NSAppleEventDescriptor.record()
        keyed.setDescriptor(NSAppleEventDescriptor(int32: 1), forKeyword: MailAE.kwMessageID)
        XCTAssertTrue(isRecordRow(keyed))
        // lists and scalars are not record rows — bundle rows (lists of
        // scalars) must classify as NON-records so column replies repack
        let bundleRow = NSAppleEventDescriptor.list()
        for _ in 0..<7 { bundleRow.insert(NSAppleEventDescriptor(int32: 1), at: Int(bundleRow.numberOfItems) + 1) }
        XCTAssertFalse(isRecordRow(bundleRow))
        XCTAssertFalse(isRecordRow(NSAppleEventDescriptor.list()))
        XCTAssertFalse(isRecordRow(NSAppleEventDescriptor(int32: 5)))
        XCTAssertFalse(isRecordRow(nil))
    }

    func testRowsFromReplyRowMajorLists() {
        let list = NSAppleEventDescriptor.list()
        list.insert(NSAppleEventDescriptor.list(), at: 1)
        list.insert(NSAppleEventDescriptor.list(), at: 2)
        let rows = MailAE.rowsFromReply(list, expectedProps: 7)
        XCTAssertEqual(rows.count, 2)
    }

    func testRowsFromReplyPropertyMajorColumns() {
        // CONFIRMED live shape for the no-body bundle: outer list of 7
        // property columns, each column a list of per-message values
        let propNames = ["id", "subject", "sender", "date", "read", "flagged", "junk"]
        let outer = NSAppleEventDescriptor.list()
        for p in 0..<7 {
            let col = NSAppleEventDescriptor.list()
            for m in 1...3 {
                col.insert(p == 0 ? NSAppleEventDescriptor(int32: Int32(m)) : stringDesc("\(propNames[p]) m\(m)"),
                           at: Int(col.numberOfItems) + 1)
            }
            outer.insert(col, at: Int(outer.numberOfItems) + 1)
        }
        let rows = MailAE.rowsFromReply(outer, expectedProps: 7)
        XCTAssertEqual(rows.count, 3)
        XCTAssertEqual(rows[0]?.atIndex(1)?.int32Value, 1)
        XCTAssertEqual(rows[0]?.atIndex(2)?.stringValue, "subject m1")
        XCTAssertEqual(rows[0]?.atIndex(7)?.stringValue, "junk m1")
        XCTAssertEqual(rows[2]?.atIndex(1)?.int32Value, 3)
        XCTAssertEqual(rows[2]?.atIndex(2)?.stringValue, "subject m3")
    }

    func testRowsFromReplyAmbiguousSevenBySevenPrefersColumns() {
        // width == property count is structurally ambiguous; chunk planning
        // avoids issuing it, and when one arrives the COLUMN reading wins
        // (bundle fetches are property-major, live-verified)
        let outer = NSAppleEventDescriptor.list()
        for p in 0..<7 {
            let col = NSAppleEventDescriptor.list()
            for m in 1...7 { col.insert(stringDesc("p\(p)m\(m)"), at: Int(col.numberOfItems) + 1) }
            outer.insert(col, at: Int(outer.numberOfItems) + 1)
        }
        let rows = MailAE.rowsFromReply(outer, expectedProps: 7)
        XCTAssertEqual(rows.count, 7)
        XCTAssertEqual(rows[0]?.atIndex(1)?.stringValue, "p0m1")
        XCTAssertEqual(rows[0]?.atIndex(2)?.stringValue, "p1m1")
        XCTAssertEqual(rows[6]?.atIndex(1)?.stringValue, "p0m7")

        // heterogeneous "row-like" 7×7 ALSO reads as columns now — a
        // homogeneity heuristic misdetects when the id column mixes
        // long/comp ids straddling 2^31
        let mixed = NSAppleEventDescriptor.list()
        for p in 0..<7 {
            let col = NSAppleEventDescriptor.list()
            for m in 1...7 {
                if p == 0 {
                    // id column: half 32-bit 'long', half wide 'comp'
                    col.insert(m <= 3 ? NSAppleEventDescriptor(int32: Int32(m))
                                      : compDesc(Int64(3_000_000_000 + m)),
                               at: Int(col.numberOfItems) + 1)
                } else {
                    col.insert(stringDesc("v\(p)m\(m)"), at: Int(col.numberOfItems) + 1)
                }
            }
            mixed.insert(col, at: Int(mixed.numberOfItems) + 1)
        }
        let rows2 = MailAE.rowsFromReply(mixed, expectedProps: 7)
        XCTAssertEqual(rows2.count, 7)
        // repacked row 4 keeps its wide id + its per-message values
        XCTAssertEqual(MailAE.intString(rows2[3]?.atIndex(1)), "3000000004")
        XCTAssertEqual(rows2[3]?.atIndex(2)?.stringValue, "v1m4")
        XCTAssertEqual(rows2[6]?.atIndex(2)?.stringValue, "v1m7")
    }

    private func compDesc(_ v: Int64) -> NSAppleEventDescriptor {
        var bits = UInt64(bitPattern: v).bigEndian
        return NSAppleEventDescriptor(descriptorType: DescType(0x636f6d70), bytes: &bits, length: 8)!
    }

    // ── row date extraction (order detection) ──

    func testDateFromRowHandlesBothShapes() {
        let date = Date(timeIntervalSince1970: 1_790_000_000)
        // record row → sdef keyword
        let rec = sampleRow()
        XCTAssertEqual(MailAE.dateFromRow(rec), date)
        // bundle row → fixed position 4 (id, subject, sender, DATE SENT, …)
        let bundle = sampleBundleRow()
        XCTAssertEqual(MailAE.dateFromRow(bundle), date)
        // a forKeyword-only read would miss the bundle row entirely
        XCTAssertNil(bundle.forKeyword(MailAE.kwDateSent))
        XCTAssertNil(MailAE.dateFromRow(nil))
    }

    func testRowsFromReplyPropertyMajorFlatBundle() {
        // get {p1, p2, p3} of messages 1 thru 2 → flat list in
        // property-major order: [p1m1, p1m2, p2m1, p2m2, p3m1, p3m2]
        let list = NSAppleEventDescriptor.list()
        let values = ["p1m1", "p1m2", "p2m1", "p2m2", "p3m1", "p3m2"]
        for v in values { list.insert(stringDesc(v), at: Int(list.numberOfItems) + 1) }
        let rows = MailAE.rowsFromReply(list, expectedProps: 3)
        XCTAssertEqual(rows.count, 2)
        XCTAssertEqual(rows[0]?.atIndex(1)?.stringValue, "p1m1")
        XCTAssertEqual(rows[0]?.atIndex(2)?.stringValue, "p2m1")
        XCTAssertEqual(rows[0]?.atIndex(3)?.stringValue, "p3m1")
        XCTAssertEqual(rows[1]?.atIndex(1)?.stringValue, "p1m2")
        XCTAssertEqual(rows[1]?.atIndex(2)?.stringValue, "p2m2")
        XCTAssertEqual(rows[1]?.atIndex(3)?.stringValue, "p3m2")
    }

    func testRowsFromReplyMalformedFlatBundleYieldsNoRows() {
        let list = NSAppleEventDescriptor.list()
        for v in ["a", "b", "c"] { list.insert(stringDesc(v), at: Int(list.numberOfItems) + 1) }
        XCTAssertTrue(MailAE.rowsFromReply(list, expectedProps: 7).isEmpty)  // 3 % 7 != 0
        XCTAssertTrue(MailAE.rowsFromReply(NSAppleEventDescriptor.list(), expectedProps: 7).isEmpty)
    }

    // ── record mapping (JSONL contract) ──

    private func sampleRow() -> NSAppleEventDescriptor {
        let row = NSAppleEventDescriptor.record()
        row.setDescriptor(NSAppleEventDescriptor(int32: 542874), forKeyword: MailAE.kwMessageID)
        row.setDescriptor(stringDesc("Test subject"), forKeyword: MailAE.kwSubject)
        row.setDescriptor(stringDesc("Ada <ada@example.com>"), forKeyword: MailAE.kwSender)
        row.setDescriptor(NSAppleEventDescriptor(date: Date(timeIntervalSince1970: 1_790_000_000)), forKeyword: MailAE.kwDateSent)
        row.setDescriptor(NSAppleEventDescriptor(boolean: true), forKeyword: MailAE.kwRead)
        row.setDescriptor(NSAppleEventDescriptor(boolean: false), forKeyword: MailAE.kwFlagged)
        row.setDescriptor(NSAppleEventDescriptor(boolean: true), forKeyword: MailAE.kwJunk)
        row.setDescriptor(stringDesc("body text"), forKeyword: MailAE.kwContent)
        return row
    }

    private func sampleBundleRow() -> NSAppleEventDescriptor {
        // bundleProps order: id, subject, sender, date sent, read, flagged, junk
        let row = NSAppleEventDescriptor.list()
        row.insert(NSAppleEventDescriptor(int32: 542874), at: 1)
        row.insert(stringDesc("Test subject"), at: 2)
        row.insert(stringDesc("Ada <ada@example.com>"), at: 3)
        row.insert(NSAppleEventDescriptor(date: Date(timeIntervalSince1970: 1_790_000_000)), at: 4)
        row.insert(NSAppleEventDescriptor(boolean: true), at: 5)
        row.insert(NSAppleEventDescriptor(boolean: false), at: 6)
        row.insert(NSAppleEventDescriptor(boolean: true), at: 7)
        return row
    }

    private func assertContractRecord(_ rec: CrawlRecord?) {
        XCTAssertNotNil(rec)
        XCTAssertEqual(rec?.app, "mail")
        XCTAssertEqual(rec?.id, "542874")
        XCTAssertEqual(rec?.account, "iCloud")
        XCTAssertEqual(rec?.container, "INBOX")
        XCTAssertEqual(rec?.title, "Test subject")
        XCTAssertEqual(rec?.participants, "Ada <ada@example.com>")
        XCTAssertEqual(rec?.read, true)
        XCTAssertEqual(rec?.flagged, false)
        XCTAssertEqual(rec?.junk, true)
        XCTAssertNotNil(rec?.start_ms)
        XCTAssertEqual(rec?.updated_ms, rec?.start_ms)
        XCTAssertNil(rec?.end_ms)
        XCTAssertNil(rec?.completed)
        XCTAssertNil(rec?.priority)
        XCTAssertNil(rec?.props)
    }

    func testRecordFromPropertiesRecordShapeMapsContractFields() {
        let rec = MailAE.recordFromRow(sampleRow(), account: "iCloud", mailbox: "INBOX", includeContent: true)
        assertContractRecord(rec)
        XCTAssertEqual(rec?.content, "body text")
    }

    func testRecordFromBundleRowMapsContractFields() {
        let rec = MailAE.recordFromRow(sampleBundleRow(), account: "iCloud", mailbox: "INBOX", includeContent: false)
        assertContractRecord(rec)
        XCTAssertNil(rec?.content)  // --no-body never carries content
    }

    func testRecordContentOmittedWhenStoreBodyOff() {
        let rec = MailAE.recordFromRow(sampleRow(), account: "a", mailbox: "m", includeContent: false)
        XCTAssertNil(rec?.content)
        XCTAssertNotNil(rec?.title)
    }

    func testRecordRejectsMissingOrZeroID() {
        XCTAssertNil(MailAE.recordFromRow(nil, account: "a", mailbox: "m", includeContent: true))
        let zero = NSAppleEventDescriptor.record()
        zero.setDescriptor(NSAppleEventDescriptor(int32: 0), forKeyword: MailAE.kwMessageID)
        XCTAssertNil(MailAE.recordFromRow(zero, account: "a", mailbox: "m", includeContent: true))
        XCTAssertNil(MailAE.recordFromRow(NSAppleEventDescriptor.list(), account: "a", mailbox: "m", includeContent: true))
    }

    func testRecordJSONLFieldNamesMatchContract() throws {
        let rec = CrawlRecord(app: "mail", id: "1", account: "a", container: "m", title: "t",
                              content: "c", participants: "p", start_ms: 1, end_ms: nil, updated_ms: 2,
                              read: true, flagged: false, junk: true, completed: nil, priority: nil, props: nil)
        let obj = try JSONSerialization.jsonObject(with: JSONEncoder().encode(rec)) as! [String: Any]
        // nil optionals are omitted by JSONEncoder; non-nil fields must keep
        // the exact contract names (bite-index::store::Record).
        for key in ["app", "id", "account", "container", "title", "content", "participants",
                    "start_ms", "updated_ms", "read", "flagged", "junk"] {
            XCTAssertTrue(obj.keys.contains(key), "missing field \(key)")
        }
        for key in ["end_ms", "completed", "priority", "props"] {
            XCTAssertFalse(obj.keys.contains(key), "nil field \(key) must be omitted")
        }
    }

    // ── bulk terminal states ──

    func testBulkTerminalStateTable() {
        XCTAssertEqual(MailBulk.terminalState(ok: true, remaining: 0), "done")
        XCTAssertEqual(MailBulk.terminalState(ok: true, remaining: 5), "partial")
        // ok + verification-count timeout → done (a succeeded destructive op
        // must not be reported failed; the caller notes the unknown remaining)
        XCTAssertEqual(MailBulk.terminalState(ok: true, remaining: nil), "done")
        XCTAssertEqual(MailBulk.terminalState(ok: false, remaining: 0), "failed")
        XCTAssertEqual(MailBulk.terminalState(ok: false, remaining: nil), "failed")
    }

    func testBulkSelectionLabelAndEmptiness() {
        XCTAssertTrue(BulkSelection().isEmpty)
        XCTAssertFalse(BulkSelection(unread: false).isEmpty)
        XCTAssertEqual(BulkSelection(unread: false).label, "read")
        XCTAssertEqual(BulkSelection(unread: true, olderThanDays: 7).label, "unread AND olderThanDays=7")
    }
}
