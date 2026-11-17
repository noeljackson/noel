// The app's state, fed by noel-ffi's events on the main actor.
import Foundation
import SwiftUI

/// Where the link to the server is.
public enum ConnectionState: Equatable, Sendable {
    case disconnected
    case connecting
    case connected
    case reconnecting
    case failed(String)
}

@MainActor
public final class NoelModel: ObservableObject {
    @Published public var connection = ConnectionState.disconnected
    @Published public var status = ""
    @Published public var sessions: [SessionItem] = []
    @Published public var selected: String?
    @Published public var entries: [EntryItem] = []
    @Published public var streaming: String?
    @Published public var plugins: [PluginItem] = []

    private var noel: Noel?

    public init() {}

    public var connected: Bool { connection == .connected }

    /// Connects to the server at `endpoint` over SSH; does nothing while a
    /// connection is up or being made.
    public func connect(_ endpoint: SSHEndpoint) {
        switch connection {
        case .connecting, .connected, .reconnecting:
            return
        case .disconnected, .failed:
            break
        }
        connection = .connecting
        status = "connecting"
        noel = Noel.connect(opener: SSHChannelOpener(endpoint: endpoint), listener: Listener(model: self))
    }

    public func select(_ session: String) {
        selected = session
        entries = []
        streaming = nil
        noel?.select(session: session)
    }

    public func send(_ text: String) { noel?.send(text: text) }
    public func newSession(cwd: String) { noel?.newSession(cwd: cwd) }
    public func abort() { noel?.abort() }
    public func run(plugin: String, command: String) { noel?.runPluginCommand(plugin: plugin, name: command) }
    public func act(plugin: String, panel: String, action: String) {
        noel?.panelAction(plugin: plugin, panel: panel, action: action)
    }

    func apply(_ event: NoelEvent) {
        switch event {
        case .connected:
            connection = .connected
            status = "connected"
        case .reconnecting:
            connection = .reconnecting
            status = "reconnecting"
        case let .failed(message):
            // This Noel is done; connect makes a new one.
            noel = nil
            connection = .failed(message)
            status = message
        case let .status(text):
            status = text
        case let .sessions(sessions):
            self.sessions = sessions
        case let .view(session, entries, streaming):
            // A view of a session no longer selected came late: ignored.
            guard selected == nil || selected == session else { return }
            selected = session
            self.entries = entries
            self.streaming = streaming
        case let .plugins(plugins):
            self.plugins = plugins
        }
    }
}

/// noel-ffi's listener: events arrive on its threads and go to the main actor.
final class Listener: NoelListener, @unchecked Sendable {
    weak var model: NoelModel?

    init(model: NoelModel) {
        self.model = model
    }

    func onEvent(event: NoelEvent) {
        Task { @MainActor [weak model] in model?.apply(event) }
    }
}
