import FileProvider
import RustFuseKit

/// What macOS should make of a failed request.
func fileProviderError(_ error: Error) -> Error {
    guard let e = error as? ProviderError else { return error }
    switch e {
    case .daemon(.noSuchItem, _):
        return NSFileProviderError(.noSuchItem)
    case .daemon(.exists, _):
        return NSFileProviderError(.filenameCollision)
    case .daemon(.notEmpty, _):
        return NSFileProviderError(.directoryNotEmpty)
    case .daemon(.anchorExpired, _):
        return NSFileProviderError(.syncAnchorExpired)
    case .daemon(.badRequest, let message):
        return CocoaError(.fileWriteInvalidFileName, userInfo: [NSLocalizedDescriptionKey: message])
    case .daemon(.io, let message), .badAnswer(let message):
        return CocoaError(.fileWriteUnknown, userInfo: [NSLocalizedDescriptionKey: message])
    case .unreachable(let message):
        // macOS retries once the daemon is back.
        return NSFileProviderError(.serverUnreachable, userInfo: [NSLocalizedDescriptionKey: message])
    }
}
