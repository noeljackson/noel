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

/// noel-ffi calls `openChannel` from a background thread and waits for it.
public final class SSHChannelOpener: ChannelOpener, @unchecked Sendable {
    private let endpoint: SSHEndpoint

    public init(endpoint: SSHEndpoint) {
        self.endpoint = endpoint
    }

    public func openChannel(delivery: Delivery) -> ByteChannel? {
        let opened = DispatchSemaphore(value: 0)
        var channel: ByteChannel?
        let endpoint = endpoint
        Task {
            do {
                let client = try await SSHClient.connect(
                    host: endpoint.host,
                    port: endpoint.port,
                    authenticationMethod: .passwordBased(username: endpoint.user, password: endpoint.password),
                    hostKeyValidator: .acceptAnything(),
                    reconnect: .never
                )
                let command = "'\(endpoint.program)' bridge"
                try await client.withExec(command) { inbound, outbound in
                    channel = ExecChannel(
                        writer: { data in
                            Task { try? await outbound.write(ByteBuffer(data: data)) }
                        },
                        closer: {
                            Task { try? await client.close() }
                        }
                    )
                    opened.signal()
                    for try await output in inbound {
                        if case let .stdout(chunk) = output {
                            delivery.receive(bytes: Data(buffer: chunk))
                        }
                    }
                }
                delivery.closed()
            } catch {
                delivery.closed()
                opened.signal()
            }
        }
        opened.wait()
        return channel
    }
}
