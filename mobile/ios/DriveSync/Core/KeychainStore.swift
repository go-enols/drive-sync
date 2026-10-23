//
//  KeychainStore.swift
//  drive-sync iOS
//
//  access token 属于敏感凭据，放 Keychain（不写 UserDefaults 明文）。
//  relay 地址/设备名等非敏感配置走 UserDefaults（见 AppModel）。
//

import Foundation
import Security

enum KeychainStore {
    private static let service = "dev.drivesync.mobile"
    private static let tokenAccount = "relay-access-token"

    /// 保存/覆盖访问令牌；空串表示删除。
    static func saveToken(_ token: String) {
        if token.isEmpty {
            deleteToken()
            return
        }
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: tokenAccount
        ]
        _ = SecItemDelete(query as CFDictionary)

        var attributes = query
        attributes[kSecValueData as String] = Data(token.utf8)
        attributes[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlock
        _ = SecItemAdd(attributes as CFDictionary, nil)
    }

    /// 读取访问令牌；不存在时返回 nil。
    static func loadToken() -> String? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: tokenAccount,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne
        ]
        var item: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &item)
        guard status == errSecSuccess, let data = item as? Data else { return nil }
        return String(data: data, encoding: .utf8)
    }

    static func deleteToken() {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: tokenAccount
        ]
        _ = SecItemDelete(query as CFDictionary)
    }
}
