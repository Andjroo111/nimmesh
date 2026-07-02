import AVFoundation
import UIKit

/// The native QR scanner behind the bridge's `scanQr` method (the Send bar's scan button).
/// AVFoundation metadata scanning, full-screen camera preview, a Cancel pill. Returns the
/// first decoded QR payload, or `nil` on cancel / no camera / no permission. The WEB layer
/// owns the parsing (nimiq: URI or bare NQ address) — this only captures a string.
final class QrScannerViewController: UIViewController, AVCaptureMetadataOutputObjectsDelegate {
    private let session = AVCaptureSession()
    private var completion: ((String?) -> Void)?
    private var finished = false

    /// Check/request camera permission, then present the scanner from the top-most VC.
    /// Always calls `completion` exactly once (nil on denial/cancel/no-camera).
    static func scan(from presenter: UIViewController, completion: @escaping (String?) -> Void) {
        func present() {
            let vc = QrScannerViewController()
            vc.completion = completion
            vc.modalPresentationStyle = .fullScreen
            presenter.present(vc, animated: true)
        }
        switch AVCaptureDevice.authorizationStatus(for: .video) {
        case .authorized:
            present()
        case .notDetermined:
            AVCaptureDevice.requestAccess(for: .video) { granted in
                DispatchQueue.main.async { granted ? present() : completion(nil) }
            }
        default:
            completion(nil)
        }
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .black

        guard let device = AVCaptureDevice.default(for: .video),
              let input = try? AVCaptureDeviceInput(device: device),
              session.canAddInput(input)
        else { finish(nil); return }
        session.addInput(input)

        let output = AVCaptureMetadataOutput()
        guard session.canAddOutput(output) else { finish(nil); return }
        session.addOutput(output)
        output.setMetadataObjectsDelegate(self, queue: .main)
        output.metadataObjectTypes = [.qr]

        let preview = AVCaptureVideoPreviewLayer(session: session)
        preview.videoGravity = .resizeAspectFill
        preview.frame = view.bounds
        view.layer.addSublayer(preview)

        let cancel = UIButton(type: .system)
        cancel.setTitle("Cancel", for: .normal)
        cancel.setTitleColor(.white, for: .normal)
        cancel.titleLabel?.font = .systemFont(ofSize: 17, weight: .semibold)
        cancel.backgroundColor = UIColor(white: 0, alpha: 0.45)
        cancel.layer.cornerRadius = 20
        cancel.contentEdgeInsets = UIEdgeInsets(top: 8, left: 20, bottom: 8, right: 20)
        cancel.addTarget(self, action: #selector(cancelTapped), for: .touchUpInside)
        cancel.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(cancel)
        NSLayoutConstraint.activate([
            cancel.topAnchor.constraint(equalTo: view.safeAreaLayoutGuide.topAnchor, constant: 12),
            cancel.leadingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.leadingAnchor, constant: 20),
        ])

        // startRunning blocks — never on the main thread (Apple's own guidance).
        DispatchQueue.global(qos: .userInitiated).async { [session] in session.startRunning() }
    }

    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        (view.layer.sublayers?.first as? AVCaptureVideoPreviewLayer)?.frame = view.bounds
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
