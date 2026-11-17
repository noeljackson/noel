// Opens the byte channels noel-ffi asks for: an SSH exec channel running
// `noel bridge` on the server's machine, as `noel client --remote` does.
import Citadel
import Foundation
import NIOCore
import NIOFoundationCompat

/// Where and how to reach the server.
public struct SSHEndpoint: Sendable {
    public var host: String
    public var port: Int
    public var user: String
    public var password: String
    /// The remote `noel` program.
    public var program: String

    public init(host: String, port: Int = 22, user: String, password: String, program: String = "noel") {
        self.host = host
        self.port = port
        self.user = user
        self.password = password
        self.program = program
    }
}

/// One exec channel: what noel-ffi sends goes to the command's stdin.
final class ExecChannel: ByteChannel, @unchecked Sendable {
    private let writer: (Data) -> Void
    private let closer: () -> Void

    init(writer: @escaping (Data) -> Void, closer: @escaping () -> Void) {
        self.writer = writer
        self.closer = closer
    }

    func send(bytes: Data) { writer(bytes) }
    func close() { closer() }
}

/// What `openChannel` waits for: set once, from the connecting task.
private final class Opening: @unchecked Sendable {
    private let lock = NSLock()
    private let done = DispatchSemaphore(value: 0)
    private var settled = false
    private var channel: ByteChannel?

    /// The first call settles it; later ones (an error after the channel
    /// opened) change nothing.
    func settle(_ channel: ByteChannel?) {
        lock.lock()
        defer { lock.unlock() }
        guard !settled else { return }
        settled = true
        self.channel = channel
        done.signal()
    }

    /// The channel, or nil when it failed or did not open within `timeout`.
    func wait(timeout: DispatchTime) -> ByteChannel? {
        let result: ByteChannel?
        if done.wait(timeout: timeout) == .timedOut {
            settle(nil)
        }
        lock.lock()
        result = channel
        lock.unlock()
        return result
    }
}

/// noel-ffi calls `openChannel` from a background thread and waits for it.
public final class SSHChannelOpener: ChannelOpener, @unchecked Sendable {
    private let endpoint: SSHEndpoint
    /// How long a connect may take before the attempt fails and noel-ffi
    /// tries again.
    private let timeout: DispatchTimeInterval

    public init(endpoint: SSHEndpoint, timeout: DispatchTimeInterval = .seconds(20)) {
        self.endpoint = endpoint
        self.timeout = timeout
    }

    public func openChannel(delivery: Delivery) -> ByteChannel? {
        let opening = Opening()
        let endpoint = endpoint
        let task = Task {
            var connected: SSHClient?
            do {
                let client = try await SSHClient.connect(
                    host: endpoint.host,
                    port: endpoint.port,
                    authenticationMethod: .passwordBased(username: endpoint.user, password: endpoint.password),
                    hostKeyValidator: .acceptAnything(),
                    reconnect: .never
                )
                connected = client
                let command = "'\(endpoint.program)' bridge"
                try await client.withExec(command) { inbound, outbound in
                    opening.settle(ExecChannel(
                        writer: { data in
                            Task { try? await outbound.write(ByteBuffer(data: data)) }
                        },
                        closer: {
                            Task { try? await client.close() }
                        }
                    ))
                    for try await output in inbound {
                        if case let .stdout(chunk) = output {
                            delivery.receive(bytes: Data(buffer: chunk))
                        }
                    }
                }
            } catch {
                opening.settle(nil)
            }
            // The command ended or failed: the channel is done.
            delivery.closed()
            try? await connected?.close()
        }
        let channel = opening.wait(timeout: .now() + timeout)
        if channel == nil {
            task.cancel()
        }
        return channel
    }
}
