// noel for iOS (roadmap phase 10): sessions, chat and plugin panels on a
// noel server reached over SSH (`noel bridge`), through NoelKit.
import NoelKit
import SwiftUI

@main
struct NoelApp: App {
    @StateObject private var model = NoelModel()

    var body: some Scene {
        WindowGroup {
            RootView().environmentObject(model)
        }
    }
}

struct RootView: View {
    @EnvironmentObject var model: NoelModel
    @AppStorage("host") private var host = ""
    @AppStorage("user") private var user = ""
    @State private var password = ""

    var body: some View {
        if model.connected || !model.sessions.isEmpty {
            TabView {
                NavigationStack { SessionsView() }.tabItem { Label("Sessions", systemImage: "bubble.left.and.bubble.right") }
                NavigationStack { PluginsView() }.tabItem { Label("Plugins", systemImage: "puzzlepiece") }
            }
        } else {
            Form {
                Section("Server over SSH") {
                    TextField("Host", text: $host).textInputAutocapitalization(.never)
                    TextField("User", text: $user).textInputAutocapitalization(.never)
                    SecureField("Password", text: $password)
                }
                Button("Connect") {
                    model.connect(SSHEndpoint(host: host, user: user, password: password))
                }
                Text(model.status).foregroundStyle(.secondary)
            }
        }
    }
}

struct SessionsView: View {
    @EnvironmentObject var model: NoelModel

    var body: some View {
        List(model.sessions, id: \.id) { session in
            NavigationLink(value: session.id) {
                VStack(alignment: .leading) {
                    Text(session.title)
                    Text("\(session.messageCount) messages\(session.busy ? ", running" : "")")
                        .font(.caption).foregroundStyle(.secondary)
                }
            }
        }
        .navigationTitle("Sessions")
        .navigationDestination(for: String.self) { id in ChatView(session: id) }
    }
}

struct ChatView: View {
    @EnvironmentObject var model: NoelModel
    let session: String
    @State private var draft = ""

    var body: some View {
        VStack(spacing: 0) {
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 12) {
                    ForEach(model.entries, id: \.id) { entry in
                        VStack(alignment: .leading, spacing: 4) {
                            Text(entry.label).font(.caption).foregroundStyle(.secondary)
                            Text(entry.text)
                                .font(entry.kind == "pi.tool-result" ? .system(.footnote, design: .monospaced) : .body)
                                .foregroundStyle(entry.isError ? .red : .primary)
                        }
                    }
                    if let streaming = model.streaming {
                        Text(streaming).foregroundStyle(.secondary)
                    }
                }
                .padding()
            }
            HStack {
                TextField("Message", text: $draft, axis: .vertical)
                    .textFieldStyle(.roundedBorder)
                Button("Send") {
                    model.send(draft)
                    draft = ""
                }
                .disabled(draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }
            .padding()
        }
        .navigationTitle(model.sessions.first { $0.id == session }?.title ?? session)
        .toolbar { Button("Abort") { model.abort() } }
        .onAppear { model.select(session) }
    }
}

struct PluginsView: View {
    @EnvironmentObject var model: NoelModel

    var body: some View {
        List(model.plugins, id: \.name) { plugin in
            Section("\(plugin.name)\(plugin.running ? "" : " (stopped)")") {
                ForEach(plugin.commands, id: \.self) { command in
                    Button(command) { model.run(plugin: plugin.name, command: command) }
                }
                ForEach(plugin.panels, id: \.id) { panel in
                    VStack(alignment: .leading) {
                        Text(panel.title).font(.caption).foregroundStyle(.secondary)
                        PanelView(view: PanelNode.parse(panel.viewJson)) { action in
                            model.act(plugin: plugin.name, panel: panel.id, action: action)
                        }
                    }
                }
            }
        }
        .navigationTitle("Plugins")
    }
}

/// Spec §13's widgets: text, stack, list and button.
indirect enum PanelNode {
    case text(String)
    case button(String, String)
    case list([PanelNode])
    case stack([PanelNode])
    case other(String)

    static func parse(_ json: String) -> PanelNode {
        guard let data = json.data(using: .utf8),
              let value = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else { return .other(json) }
        return node(value)
    }

    static func node(_ value: [String: Any]) -> PanelNode {
        let children = { (key: String) in
            ((value[key] as? [[String: Any]]) ?? []).map(node)
        }
        switch value["type"] as? String {
        case "text": return .text(value["text"] as? String ?? "")
        case "button": return .button(value["label"] as? String ?? "", value["action"] as? String ?? "")
        case "list": return .list(children("items"))
        case "stack": return .stack(children("children"))
        default: return .other("\(value)")
        }
    }
}

struct PanelView: View {
    let view: PanelNode
    let act: (String) -> Void

    var body: some View {
        switch view {
        case let .text(text): Text(text)
        case let .button(label, action): Button(label) { act(action) }
        case let .list(items):
            VStack(alignment: .leading) {
                ForEach(items.indices, id: \.self) { index in
                    HStack(alignment: .top) { Text("•"); PanelView(view: items[index], act: act) }
                }
            }
        case let .stack(children):
            VStack(alignment: .leading) {
                ForEach(children.indices, id: \.self) { index in PanelView(view: children[index], act: act) }
            }
        case let .other(text): Text(text).font(.caption).foregroundStyle(.secondary)
        }
    }
}
