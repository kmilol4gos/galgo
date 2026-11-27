// Dibuja el icono de galgo (PNG 1024×1024) con los colores del tema: fondo BG y el prompt "›_" en ACCENT.
// Uso: swift scripts/make-icon.swift SALIDA.png
import AppKit

let out = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "icon-1024.png"
let size: CGFloat = 1024
let img = NSImage(size: NSSize(width: size, height: size))
img.lockFocus()
let rect = NSRect(x: 0, y: 0, width: size, height: size)
// Fondo redondeado al estilo de macOS (margen de 100 px alrededor, como los iconos del sistema).
let body = NSRect(x: 100, y: 100, width: size - 200, height: size - 200)
let path = NSBezierPath(roundedRect: body, xRadius: 180, yRadius: 180)
NSColor(calibratedRed: 0.208, green: 0.192, blue: 0.173, alpha: 1).setFill()
path.fill()
let borde = NSBezierPath(roundedRect: body.insetBy(dx: 6, dy: 6), xRadius: 174, yRadius: 174)
NSColor(calibratedRed: 0.275, green: 0.259, blue: 0.231, alpha: 1).setStroke()
borde.lineWidth = 8
borde.stroke()
let acento = NSColor(calibratedRed: 0.435, green: 0.608, blue: 0.820, alpha: 1)
let fuente = NSFont.monospacedSystemFont(ofSize: 430, weight: .bold)
let texto = NSAttributedString(string: "\u{203A}_", attributes: [.font: fuente, .foregroundColor: acento])
let t = texto.size()
texto.draw(at: NSPoint(x: (rect.width - t.width) / 2, y: (rect.height - t.height) / 2 - 40))
img.unlockFocus()
guard let tiff = img.tiffRepresentation, let rep = NSBitmapImageRep(data: tiff), let png = rep.representation(using: .png, properties: [:]) else {
    fputs("no se pudo generar el PNG\n", stderr); exit(1)
}
try png.write(to: URL(fileURLWithPath: out))
