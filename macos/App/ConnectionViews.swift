import RustFuseKit
import SwiftUI

/// One saved connection: its hub and bucket, and the volumes its token
/// opens, each with a button to add it here.
struct ConnectionDetail: View {
    @ObservedObject var model: AppModel
    let connection: Connection
    let add: (String) -> Void
    let edit: () -> Void
    @State private var names: [String]?
    @State private var error: String?
    @State private var confirmRemove = false

    var body: some View {
        Form {
            Section {
                LabeledContent("Hub", value: connection.hub)
                if let ca = connection.caFile {
                    LabeledContent("Hub CA", value: URL(fileURLWithPath: ca).lastPathComponent)
                }
                if let s3 = connection.s3 {
                    LabeledContent("Bucket", value: s3.bucket)
                    LabeledContent("Endpoint", value: s3.endpoint?.isEmpty == false ? s3.endpoint! : "AWS")
                    if let p = s3.prefix, !p.isEmpty {
                        LabeledContent("Prefix", value: p)
                    }
                } else {
                    LabeledContent("Bucket", value: "through the hub")
                }
            }
            Section("Volumes") {
                if let names {
                    if names.isEmpty {
                        Text("This token opens no volumes yet.").foregroundStyle(.secondary)
                    }
                    ForEach(names, id: \.self) { n in
                        HStack {
                            Text(n)
                            Spacer()
                            let here = model.library.volumes.filter { $0.connection == connection.id && $0.name == n }
                            if !here.isEmpty {
                                Text(here.map { describe($0.mode) }.joined(separator: ", "))
                                    .font(.caption).foregroundStyle(.secondary)
                            }
                            Button(here.isEmpty ? "Add…" : "Add Again…") { add(n) }
                        }
                    }
                } else if error == nil {
                    ProgressView().controlSize(.small)
                }
                ErrorText(text: error)
            }
            Section {
                HStack {
                    Button("Edit…", action: edit)
                    Button("Refresh") { Task { await load() } }
                    Spacer()
                    Button("Remove…", role: .destructive) { confirmRemove = true }
                }
            }
        }
        .formStyle(.grouped)
        .navigationTitle(connection.name)
        .task(id: connection) { await load() }
        .confirmationDialog("Remove the connection \(connection.name)?", isPresented: $confirmRemove) {
            Button("Remove", role: .destructive) { model.remove(connection: connection.id) }
        } message: {
            Text("Its token is deleted from the keychain. Volumes in the bucket are not touched.")
        }
    }

    private func load() async {
        error = nil
        do {
            names = try await model.volumes(of: connection)
        } catch {
            names = nil
            self.error = error.localizedDescription
        }
    }
}

/// Adds or edits a connection: the hub, its token and CA, and optionally
/// direct access to the bucket.
struct ConnectionSheet: View {
    @ObservedObject var model: AppModel
    let editing: Connection?
    @Environment(\.dismiss) private var dismiss
    @State private var name = ""
    @State private var hub = ""
    @State private var token = ""
    @State private var ca = ""
    @State private var direct = false
    @State private var bucket = ""
    @State private var endpoint = ""
    @State private var region = ""
    @State private var prefix = ""
    @State private var accessKey = ""
    @State private var secretKey = ""
    @State private var checking = false
    @State private var result: String?
    @State private var error: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(editing == nil ? "Add a Connection" : "Edit \(editing!.name)").font(.headline)
            Form {
                TextField("Name", text: $name, prompt: Text("Home NAS"))
                TextField("Hub", text: $hub, prompt: Text("host:7420 or tls://host"))
                SecureField("Hub token", text: $token, prompt: Text(editing == nil ? "" : "unchanged"))
                HStack {
                    TextField("Hub CA (PEM, optional)", text: $ca, prompt: Text(editing?.caFile == nil ? "system" : "unchanged"))
                    Button("Choose…") { if let f = chooseFile(prompt: "Use") { ca = f } }
                }
                Toggle("Also read the bucket directly (faster on a LAN)", isOn: $direct)
                if direct {
                    TextField("Bucket", text: $bucket)
                    TextField("Endpoint", text: $endpoint, prompt: Text("http://nas:9000; empty for AWS"))
                    TextField("Region", text: $region, prompt: Text("us-east-1"))
                    TextField("Prefix", text: $prefix, prompt: Text("none"))
                    TextField("Access key", text: $accessKey)
                    SecureField("Secret key", text: $secretKey, prompt: Text(editing?.s3 == nil ? "" : "unchanged"))
                }
            }
            if let result {
                Text(result).foregroundStyle(.secondary)
            }
            ErrorText(text: error)
            HStack {
                Button(checking ? "Checking…" : "Check") { check() }
                    .disabled(checking || hub.isEmpty)
                Spacer()
                Button("Cancel") { dismiss() }.keyboardShortcut(.cancelAction)
                Button("Save") { save() }
                    .keyboardShortcut(.defaultAction)
                    .disabled(hub.isEmpty || (direct && (bucket.isEmpty || accessKey.isEmpty)))
            }
        }
        .padding(20)
        .frame(width: 520)
        .onAppear(perform: fill)
    }

    private func fill() {
        guard let c = editing else { return }
        name = c.name
        hub = c.hub
        if let s3 = c.s3 {
            direct = true
            bucket = s3.bucket
            endpoint = s3.endpoint ?? ""
            region = s3.region ?? ""
            prefix = s3.prefix ?? ""
            accessKey = s3.accessKey
        }
    }

    private var draft: Connection {
        var c = editing ?? Connection(name: "", hub: "")
        c.name = name.isEmpty ? hub : name
        c.hub = hub.trimmingCharacters(in: .whitespaces)
        c.s3 =
            direct
            ? S3Access(
                bucket: bucket, endpoint: endpoint.isEmpty ? nil : endpoint, region: region.isEmpty ? nil : region,
                prefix: prefix.isEmpty ? nil : prefix, accessKey: accessKey)
            : nil
        return c
    }

    /// Lists the volumes with what is filled in, without saving anything.
    private func check() {
        checking = true
        error = nil
        result = nil
        var c = draft
        if !ca.isEmpty { c.caFile = (ca as NSString).expandingTildeInPath }
        let id = c.id
        Task {
            defer { checking = false }
            // The check needs the typed secrets; saved ones are used when
            // the fields are left empty.
            var env = toolEnv(for: model.library.connection(id) == nil ? nil : c)
            if !token.isEmpty { env["RUSTFUSE_HUB_TOKEN"] = token }
            if direct {
                env["RUSTFUSE_ACCESS_KEY"] = accessKey
                if !secretKey.isEmpty { env["RUSTFUSE_SECRET_KEY"] = secretKey }
            }
            do {
                let names = try await runChecked(volumesArguments(c), env: env)
                result = names.isEmpty ? "Connected. The token opens no volumes yet." : "Connected: \(names.joined(separator: ", "))"
            } catch {
                self.error = error.localizedDescription
            }
        }
    }

    private func save() {
        do {
            try model.save(connection: draft, token: token, s3Secret: secretKey, caSource: ca)
            dismiss()
        } catch {
            self.error = error.localizedDescription
        }
    }
}

/// `rustfuse volumes` with an explicit environment, for checking a
/// connection before it is saved.
private func runChecked(_ args: [String], env: [String: String]) async throws -> [String] {
    try await withCheckedThrowingContinuation { cont in
        let p = Process()
        p.executableURL = rustfuseTool()
        p.arguments = args
        p.environment = env
        let out = Pipe()
        let err = Pipe()
        p.standardOutput = out
        p.standardError = err
        p.terminationHandler = { p in
            let o = out.fileHandleForReading.readDataToEndOfFile()
            let e = String(decoding: err.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
            if p.terminationStatus == 0, let names = try? JSONDecoder().decode([String].self, from: o) {
                cont.resume(returning: names)
            } else {
                cont.resume(
                    throwing: CocoaError(
                        .executableLoad,
                        userInfo: [NSLocalizedDescriptionKey: e.trimmingCharacters(in: .whitespacesAndNewlines)]))
            }
        }
        do {
            try p.run()
        } catch {
            cont.resume(throwing: error)
        }
    }
}
