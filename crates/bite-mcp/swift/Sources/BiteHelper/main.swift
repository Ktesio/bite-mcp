import Foundation

/// bite-helper entry point.
///
/// Process model:
/// - Requests are read from stdin and executed INLINE on one serial queue, so
///   all Apple-framework work (EventKit, ScriptingBridge Apple Events) is
///   confined to a single dedicated thread.
/// - The main thread parks in dispatchMain(); we deliberately do NOT dispatch
///   work to the main queue — a main-queue hop from the reader deadlocks
///   (verified: GCD never delivers main.sync blocks in this process shape).
/// - The first line on stdout is the `hello` notification (protocol handshake).

let dispatcher = Dispatcher()
let stdout = LineWriter(FileHandle.standardOutput)

let readerQueue = DispatchQueue(label: "bite.reader")  // serial: all handlers run here

func main() {
    SysBridge.register(dispatcher)
    registerCalendarHandlers(dispatcher)
    registerRemindersHandlers(dispatcher)
    registerContactsHandlers(dispatcher)
    MailBridge.register(dispatcher)
    NotesBridge.register(dispatcher)
    MessagesBridge.register(dispatcher)

    stdout.write([
        "method": "hello",
        "params": [
            "protocol": PROTOCOL_VERSION,
            "version": HELPER_VERSION,
            "capabilities": CAPABILITIES,
        ] as [String: Any],
    ] as [String: Any])

    readerQueue.async { readLoop() }
    dispatchMain()  // park the main thread; the reader queue does the work
}

func readLoop() {
    let stdin = FileHandle.standardInput
    var buffer = Data()
    while true {
        // availableData blocks until bytes arrive and returns empty at EOF;
        // read(upToCount:) can block for the full count on pipes.
        let chunk = stdin.availableData
        if chunk.isEmpty { exit(0) }  // stdin closed: parent went away
        buffer.append(chunk)
        while let nl = buffer.firstIndex(of: 0x0A) {
            let line = buffer.subdata(in: buffer.startIndex..<nl)
            buffer.removeSubrange(buffer.startIndex...nl)
            guard !line.isEmpty else { continue }
            if let req = Envelope.parse(line) {
                let response = dispatcher.respond(req)
                if !response.isEmpty {
                    stdout.write(response)
                }
            }
        }
    }
}

main()
