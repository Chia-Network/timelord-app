import Foundation
import TimelordPresentation

actor LiveStatusClient {
    let endpoint: URL
    init(endpoint: URL) { self.endpoint = endpoint }

    func fetch(expectedPID: Int32?) async throws -> ServiceStatus {
        var request = URLRequest(url: endpoint)
        request.timeoutInterval = 2
        request.cachePolicy = .reloadIgnoringLocalCacheData
        let (data, response) = try await URLSession.shared.data(for: request)
        guard (response as? HTTPURLResponse)?.statusCode == 200 else { throw StatusError.invalidResponse }
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let status = try decoder.decode(ServiceStatus.self, from: data)
        guard status.apiVersion == 1 else { throw StatusError.unsupportedVersion }
        if let expectedPID, status.servicePid != Int(expectedPID) { throw StatusError.differentOwner }
        let age = Date().timeIntervalSince1970 - status.timestampUnix
        guard age >= -5 && age < 10 else { throw StatusError.stale }
        return status
    }
    func history(expectedPID: Int) async throws -> HistoryPayload {
        let url = endpoint.deletingLastPathComponent().deletingLastPathComponent().appendingPathComponent("v3/history")
        var request = URLRequest(url: url); request.timeoutInterval = 5; request.cachePolicy = .reloadIgnoringLocalCacheData
        let (data, response) = try await URLSession.shared.data(for: request)
        guard (response as? HTTPURLResponse)?.statusCode == 200 else { throw StatusError.invalidResponse }
        let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
        let history = try decoder.decode(HistoryPayload.self, from: data)
        guard history.apiVersion == 3 else { throw StatusError.unsupportedVersion }
        guard history.servicePid == expectedPID else { throw StatusError.differentOwner }
        return history
    }

}

enum StatusError: LocalizedError {
    case invalidResponse, unsupportedVersion, differentOwner, stale
    var errorDescription: String? {
        switch self {
        case .invalidResponse: return "The local service did not return its status."
        case .unsupportedVersion: return "The app and service versions do not match."
        case .differentOwner: return "Another service is using the status address."
        case .stale: return "The service status is out of date."
        }
    }
}
