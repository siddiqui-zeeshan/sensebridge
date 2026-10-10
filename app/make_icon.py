"""Draw the app icon (a haptic pulse around a dot) to app-icon.png.

Needs PySide6. Then:  npx tauri icon app-icon.png   (writes src-tauri/icons)
"""
import pathlib
import sys

from PySide6.QtCore import QPointF, QRectF, Qt
from PySide6.QtGui import QColor, QGuiApplication, QImage, QLinearGradient, QPainter, QPen

SIZE = 1024


def main():
    app = QGuiApplication(sys.argv)  # noqa: F841 - Qt needs one to paint
    image = QImage(SIZE, SIZE, QImage.Format_ARGB32)
    image.fill(Qt.transparent)
    p = QPainter(image)
    p.setRenderHint(QPainter.Antialiasing)

    background = QLinearGradient(0, 0, SIZE, SIZE)
    background.setColorAt(0, QColor("#6f9bff"))
    background.setColorAt(1, QColor("#3b5bdb"))
    p.setBrush(background)
    p.setPen(Qt.NoPen)
    margin = SIZE * 0.06
    p.drawRoundedRect(QRectF(margin, margin, SIZE - 2 * margin, SIZE - 2 * margin),
                      SIZE * 0.22, SIZE * 0.22)

    centre = QPointF(SIZE / 2, SIZE / 2)
    p.setBrush(QColor("white"))
    p.drawEllipse(centre, SIZE * 0.085, SIZE * 0.085)

    p.setBrush(Qt.NoBrush)
    for radius, alpha in ((0.2, 255), (0.32, 200)):
        pen = QPen(QColor(255, 255, 255, alpha), SIZE * 0.062, Qt.SolidLine, Qt.RoundCap)
        p.setPen(pen)
        r = SIZE * radius
        box = QRectF(centre.x() - r, centre.y() - r, 2 * r, 2 * r)
        p.drawArc(box, -48 * 16, 96 * 16)    # right
        p.drawArc(box, 132 * 16, 96 * 16)    # left
    p.end()

    out = pathlib.Path(__file__).with_name("app-icon.png")
    image.save(str(out))
    print("wrote", out)


if __name__ == "__main__":
    main()
