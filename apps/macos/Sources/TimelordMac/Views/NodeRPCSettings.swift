import SwiftUI
import TimelordPresentation

struct NodeRPCSettings: View {
    @Binding var rpc: NodeRPCConfiguration?
    let defaultHost: String
    let chooseFile: () -> String?

    var body: some View {
        Toggle("Enable sync status", isOn: Binding(
            get: { rpc != nil },
            set: { rpc = $0 ? NodeRPCConfiguration(host: defaultHost) : nil }
        ))
        if rpc != nil {
            TextField("RPC host", text: field(\.host))
            TextField("RPC port", value: field(\.port), format: .number.grouping(.never))
            TextField("RPC TLS server name", text: field(\.serverName))
            credential("RPC client certificate", path: field(\.certificate))
            credential("RPC private key", path: field(\.privateKey))
            credential("RPC CA certificate", path: field(\.caCertificate))
            Text("Use an address reachable from this Mac and the full node’s RPC port.")
                .font(.caption).foregroundStyle(.secondary)
        }
    }

    private func field<Value>(_ keyPath: WritableKeyPath<NodeRPCConfiguration, Value>) -> Binding<Value> {
        Binding(get: { (rpc ?? NodeRPCConfiguration(host: defaultHost))[keyPath: keyPath] },
                set: { rpc?[keyPath: keyPath] = $0 })
    }
    private func credential(_ title: String, path: Binding<String>) -> some View {
        HStack {
            VStack(alignment: .leading, spacing: 4) {
                Text(title)
                Text(path.wrappedValue.isEmpty ? "Not selected" : URL(fileURLWithPath: path.wrappedValue).lastPathComponent)
                    .font(.caption).foregroundStyle(.secondary)
            }
            Spacer()
            Button("Choose…") { if let selected = chooseFile() { path.wrappedValue = selected } }
        }
    }
}
