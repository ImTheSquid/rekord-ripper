import ImageIO
import SwiftUI

/// Cover art thumbnails, decoded off the main thread and kept for reuse.
enum Artwork {
    /// NSCache is thread-safe.
    nonisolated(unsafe) private static let cache = NSCache<NSString, CGImage>()

    static func thumbnail(_ path: String, pixels: Int) async -> CGImage? {
        let key = "\(pixels)|\(path)" as NSString
        if let hit = cache.object(forKey: key) { return hit }
        let image = await Task.detached(priority: .utility) { () -> CGImage? in
            guard let source = CGImageSourceCreateWithURL(URL(fileURLWithPath: path) as CFURL, nil) else { return nil }
            let options = [
                kCGImageSourceCreateThumbnailFromImageAlways: true,
                kCGImageSourceCreateThumbnailWithTransform: true,
                kCGImageSourceThumbnailMaxPixelSize: pixels,
            ] as CFDictionary
            return CGImageSourceCreateThumbnailAtIndex(source, 0, options)
        }.value
        if let image { cache.setObject(image, forKey: key) }
        return image
    }
}

struct ArtworkView: View {
    let path: String?
    let size: CGFloat
    @State private var image: CGImage?
    @Environment(\.displayScale) private var scale

    var body: some View {
        ZStack {
            if let image {
                Image(decorative: image, scale: scale)
                    .resizable()
                    .interpolation(.high)
                    .aspectRatio(contentMode: .fill)
            } else {
                Rectangle().fill(.quaternary)
                Image(systemName: "music.note")
                    .font(.system(size: size * 0.4))
                    .foregroundStyle(.secondary)
            }
        }
        .frame(width: size, height: size)
        .clipShape(.rect(cornerRadius: size > 32 ? 6 : 3))
        .task(id: path) {
            image = nil
            guard let path else { return }
            image = await Artwork.thumbnail(path, pixels: Int(size * scale))
        }
    }
}
