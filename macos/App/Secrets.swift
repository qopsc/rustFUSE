import Foundation
import Security

/// Hub tokens and S3 secret keys, kept in the login keychain (never on
/// disk in the clear) and handed to each daemon in its environment.
enum Secrets {
    enum Kind: String {
        case hubToken = "dev.rustfuse.hub-token"
        case s3Secret = "dev.rustfuse.s3-secret"
    }

    static func save(_ value: String, _ kind: Kind, for connection: UUID) throws {
        try save(value, kind, account: connection.uuidString)
    }

    static func get(_ kind: Kind, for connection: UUID) -> String? {
        get(kind, account: connection.uuidString)
    }

    static func delete(_ kind: Kind, for connection: UUID) {
        delete(kind, account: connection.uuidString)
    }

    /// The first version kept each volume's token under the volume's name.
    static func migrateLegacyToken(volume: String, to connection: UUID) {
        guard get(.hubToken, for: connection) == nil, let token = get(.hubToken, account: volume) else { return }
        if (try? save(token, .hubToken, for: connection)) != nil {
            delete(.hubToken, account: volume)
        }
    }

    private static func save(_ value: String, _ kind: Kind, account: String) throws {
        delete(kind, account: account)
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: kind.rawValue,
            kSecAttrAccount as String: account,
            kSecValueData as String: Data(value.utf8),
        ]
        let status = SecItemAdd(query as CFDictionary, nil)
        guard status == errSecSuccess else {
            throw NSError(domain: NSOSStatusErrorDomain, code: Int(status))
        }
    }

    private static func get(_ kind: Kind, account: String) -> String? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: kind.rawValue,
            kSecAttrAccount as String: account,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var out: AnyObject?
        guard SecItemCopyMatching(query as CFDictionary, &out) == errSecSuccess, let data = out as? Data else {
            return nil
        }
        return String(decoding: data, as: UTF8.self)
    }

    private static func delete(_ kind: Kind, account: String) {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: kind.rawValue,
            kSecAttrAccount as String: account,
        ]
        SecItemDelete(query as CFDictionary)
    }
}
