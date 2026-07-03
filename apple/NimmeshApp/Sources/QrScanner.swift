import AVFoundation
import UIKit

/// The native QR scanner behind the bridge's `scanQr` method — matched to the REAL wallet's
/// scanner: four corner brackets framing the scan area (grey while waiting for permission,
/// NIMIQ GOLD over the live feed), a white bottom-center Cancel pill, and the wallet's navy
/// "Unblock the camera" screen when access is missing (tapping the hint opens Settings).
/// Always presents — a denied permission shows the unblock screen instead of a silent no-op.
/// The WEB layer owns parsing; this only captures a string. Completion fires exactly once.
final class QrScannerViewController: UIViewController, AVCaptureMetadataOutputObjectsDelegate {
    private let session = AVCaptureSession()
    private var completion: ((String?) -> Void)?
    private var finished = false

    private let bracketsLayer = CAShapeLayer()
    private let gradientLayer = CAGradientLayer()
    private var previewLayer: AVCaptureVideoPreviewLayer?
    private let titleLabel = UILabel()
    private let hintLabel = UILabel()

    /// The scanner's few native strings, in the app language (UserDefaults "nimmesh.lang").
    private static let strings: [String: [String: String]] = [
        "cancel": ["en": "Cancel", "es": "Cancelar", "de": "Abbrechen", "fr": "Annuler", "pt": "Cancelar"],
        "unblock": [
            "en": "Unblock the camera for nimmesh to scan QR codes.",
            "es": "Desbloquea la cámara para que nimmesh escanee códigos QR.",
            "de": "Gib die Kamera frei, damit nimmesh QR-Codes scannen kann.",
            "fr": "Débloque la caméra pour que nimmesh scanne les codes QR.",
            "pt": "Desbloqueie a câmera para o nimmesh escanear códigos QR.",
        ],
        "grant": [
            "en": "Grant camera access when asked.",
            "es": "Concede acceso a la cámara cuando se te pida.",
            "de": "Erlaube den Kamerazugriff, wenn du gefragt wirst.",
            "fr": "Autorise l’accès à la caméra quand on te le demande.",
            "pt": "Permita o acesso à câmera quando solicitado.",
        ],
        "settings": [
            "en": "Open Settings to allow camera access",
            "es": "Abre Ajustes para permitir la cámara",
            "de": "Öffne die Einstellungen, um die Kamera zu erlauben",
            "fr": "Ouvre Réglages pour autoriser la caméra",
            "pt": "Abra os Ajustes para permitir a câmera",
        ],
    ]

    private static func t(_ key: String) -> String {
        let lang = UserDefaults.standard.string(forKey: "nimmesh.lang") ?? "en"
        let table = strings[key] ?? [:]
        return table[lang] ?? table["en"] ?? key
    }

    /// Present the scanner. It handles permission itself (unblock screen on denial), so the
    /// scan button never silently no-ops. `completion(nil)` on cancel.
    static func scan(from presenter: UIViewController, completion: @escaping (String?) -> Void) {
        let vc = QrScannerViewController()
        vc.completion = completion
        vc.modalPresentationStyle = .fullScreen
        presenter.present(vc, animated: true)
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = UIColor(red: 0x1F / 255, green: 0x23 / 255, blue: 0x48 / 255, alpha: 1)

        // The wallet's navy radial background (--nimiq-blue-bg: purple at bottom right -> navy).
        gradientLayer.type = .radial
        gradientLayer.colors = [
            UIColor(red: 0x26 / 255, green: 0x01 / 255, blue: 0x33 / 255, alpha: 1).cgColor,
            UIColor(red: 0x1F / 255, green: 0x23 / 255, blue: 0x48 / 255, alpha: 1).cgColor,
        ]
        gradientLayer.startPoint = CGPoint(x: 1, y: 1)
        gradientLayer.endPoint = CGPoint(x: 0, y: 0)
        view.layer.addSublayer(gradientLayer)

        // The four corner brackets (grey until the feed runs, gold over the live camera).
        bracketsLayer.strokeColor = UIColor(white: 1, alpha: 0.45).cgColor
        bracketsLayer.fillColor = UIColor.clear.cgColor
        bracketsLayer.lineWidth = 5
        bracketsLayer.lineCap = .round
        bracketsLayer.lineJoin = .round
        view.layer.addSublayer(bracketsLayer)

        // Unblock/grant copy (hidden while the camera runs).
        titleLabel.text = QrScannerViewController.t("unblock")
        titleLabel.textColor = .white
        titleLabel.font = .systemFont(ofSize: 21, weight: .bold)
        titleLabel.numberOfLines = 0
        titleLabel.textAlignment = .center
        view.addSubview(titleLabel)

        hintLabel.text = QrScannerViewController.t("grant")
        hintLabel.textColor = UIColor(white: 1, alpha: 0.6)
        hintLabel.font = .systemFont(ofSize: 17, weight: .regular)
        hintLabel.numberOfLines = 0
        hintLabel.textAlignment = .center
        hintLabel.isUserInteractionEnabled = true
        hintLabel.addGestureRecognizer(UITapGestureRecognizer(target: self, action: #selector(hintTapped)))
        view.addSubview(hintLabel)

        // The wallet's white Cancel pill, bottom center.
        let cancel = UIButton(type: .system)
        cancel.setTitle(QrScannerViewController.t("cancel"), for: .normal)
        cancel.setTitleColor(UIColor(red: 0x1F / 255, green: 0x23 / 255, blue: 0x48 / 255, alpha: 1), for: .normal)
        cancel.titleLabel?.font = .systemFont(ofSize: 16, weight: .semibold)
        cancel.backgroundColor = .white
        cancel.layer.cornerRadius = 20
        cancel.contentEdgeInsets = UIEdgeInsets(top: 10, left: 26, bottom: 10, right: 26)
        cancel.addTarget(self, action: #selector(cancelTapped), for: .touchUpInside)
        cancel.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(cancel)

        titleLabel.translatesAutoresizingMaskIntoConstraints = false
        hintLabel.translatesAutoresizingMaskIntoConstraints = false
        NSLayoutConstraint.activate([
            titleLabel.centerXAnchor.constraint(equalTo: view.centerXAnchor),
            titleLabel.centerYAnchor.constraint(equalTo: view.centerYAnchor, constant: -20),
            titleLabel.leadingAnchor.constraint(equalTo: view.leadingAnchor, constant: 44),
            titleLabel.trailingAnchor.constraint(equalTo: view.trailingAnchor, constant: -44),
            hintLabel.centerXAnchor.constraint(equalTo: view.centerXAnchor),
            hintLabel.leadingAnchor.constraint(equalTo: view.leadingAnchor, constant: 44),
            hintLabel.trailingAnchor.constraint(equalTo: view.trailingAnchor, constant: -44),
            hintLabel.bottomAnchor.constraint(equalTo: cancel.topAnchor, constant: -28),
            cancel.centerXAnchor.constraint(equalTo: view.centerXAnchor),
            cancel.bottomAnchor.constraint(equalTo: view.safeAreaLayoutGuide.bottomAnchor, constant: -24),
        ])

        switch AVCaptureDevice.authorizationStatus(for: .video) {
        case .authorized:
            startCamera()
        case .notDetermined:
            AVCaptureDevice.requestAccess(for: .video) { granted in
                DispatchQueue.main.async { granted ? self.startCamera() : self.showDenied() }
            }
        default:
            showDenied()
        }
    }

    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        gradientLayer.frame = view.bounds
        previewLayer?.frame = view.bounds
        layoutBrackets()
    }

    /// Four L-shaped corners framing a centered square scan area (the wallet's geometry:
    /// ~62% of the width, slightly above center, ~42pt arms, rounded caps).
    private func layoutBrackets() {
        let w = view.bounds.width
        let h = view.bounds.height
        let side = min(w - 128, 280)
        let rect = CGRect(x: (w - side) / 2, y: h * 0.47 - side / 2, width: side, height: side)
        let arm: CGFloat = 42
        let r: CGFloat = 8
        let path = UIBezierPath()
        // top left
        path.move(to: CGPoint(x: rect.minX, y: rect.minY + arm))
        path.addLine(to: CGPoint(x: rect.minX, y: rect.minY + r))
        path.addArc(withCenter: CGPoint(x: rect.minX + r, y: rect.minY + r), radius: r,
                    startAngle: .pi, endAngle: 3 * .pi / 2, clockwise: true)
        path.addLine(to: CGPoint(x: rect.minX + arm, y: rect.minY))
        // top right
        path.move(to: CGPoint(x: rect.maxX - arm, y: rect.minY))
        path.addLine(to: CGPoint(x: rect.maxX - r, y: rect.minY))
        path.addArc(withCenter: CGPoint(x: rect.maxX - r, y: rect.minY + r), radius: r,
                    startAngle: 3 * .pi / 2, endAngle: 0, clockwise: true)
        path.addLine(to: CGPoint(x: rect.maxX, y: rect.minY + arm))
        // bottom right
        path.move(to: CGPoint(x: rect.maxX, y: rect.maxY - arm))
        path.addLine(to: CGPoint(x: rect.maxX, y: rect.maxY - r))
        path.addArc(withCenter: CGPoint(x: rect.maxX - r, y: rect.maxY - r), radius: r,
                    startAngle: 0, endAngle: .pi / 2, clockwise: true)
        path.addLine(to: CGPoint(x: rect.maxX - arm, y: rect.maxY))
        // bottom left
        path.move(to: CGPoint(x: rect.minX + arm, y: rect.maxY))
        path.addLine(to: CGPoint(x: rect.minX + r, y: rect.maxY))
        path.addArc(withCenter: CGPoint(x: rect.minX + r, y: rect.maxY - r), radius: r,
                    startAngle: .pi / 2, endAngle: .pi, clockwise: true)
        path.addLine(to: CGPoint(x: rect.minX, y: rect.maxY - arm))
        bracketsLayer.path = path.cgPath
        bracketsLayer.frame = view.bounds
    }

    private func startCamera() {
        guard let device = AVCaptureDevice.default(for: .video),
              let input = try? AVCaptureDeviceInput(device: device),
              session.canAddInput(input)
        else { showDenied(); return }
        session.addInput(input)

        let output = AVCaptureMetadataOutput()
        guard session.canAddOutput(output) else { showDenied(); return }
        session.addOutput(output)
        output.setMetadataObjectsDelegate(self, queue: .main)
        output.metadataObjectTypes = [.qr]

        let preview = AVCaptureVideoPreviewLayer(session: session)
        preview.videoGravity = .resizeAspectFill
        preview.frame = view.bounds
        view.layer.insertSublayer(preview, above: gradientLayer)
        previewLayer = preview

        // Live feed: hide the unblock copy, brackets go NIMIQ GOLD above the feed.
        titleLabel.isHidden = true
        hintLabel.isHidden = true
        bracketsLayer.strokeColor = UIColor(red: 0xE9 / 255, green: 0xB2 / 255, blue: 0x13 / 255, alpha: 1).cgColor
        view.layer.insertSublayer(bracketsLayer, above: preview)

        // startRunning blocks — never on the main thread (Apple's own guidance).
        DispatchQueue.global(qos: .userInitiated).async { [session] in session.startRunning() }
    }

    /// The wallet's "Unblock the camera" state; the hint deep-links to Settings.
    private func showDenied() {
        titleLabel.isHidden = false
        hintLabel.isHidden = false
        if AVCaptureDevice.authorizationStatus(for: .video) != .notDetermined {
            hintLabel.text = QrScannerViewController.t("settings")
            hintLabel.textColor = UIColor(red: 0x0C / 255, green: 0xA6 / 255, blue: 0xFE / 255, alpha: 1)
        }
    }

    @objc private func hintTapped() {
        guard AVCaptureDevice.authorizationStatus(for: .video) != .notDetermined,
              let url = URL(string: UIApplication.openSettingsURLString)
        else { return }
        UIApplication.shared.open(url)
    }

    func metadataOutput(
        _ output: AVCaptureMetadataOutput,
        didOutput metadataObjects: [AVMetadataObject],
        from connection: AVCaptureConnection
    ) {
        guard let qr = metadataObjects.compactMap({ $0 as? AVMetadataMachineReadableCodeObject }).first,
              qr.type == .qr, let text = qr.stringValue, !text.isEmpty
        else { return }
        UINotificationFeedbackGenerator().notificationOccurred(.success)
        finish(text)
    }

    @objc private func cancelTapped() { finish(nil) }

    /// Resolve exactly once: stop the session, dismiss, call completion.
    private func finish(_ text: String?) {
        guard !finished else { return }
        finished = true
        let done = completion
        completion = nil
        DispatchQueue.global(qos: .userInitiated).async { [session] in session.stopRunning() }
        DispatchQueue.main.async {
            if self.presentingViewController != nil {
                self.dismiss(animated: true) { done?(text) }
            } else {
                done?(text)
            }
        }
    }
}
