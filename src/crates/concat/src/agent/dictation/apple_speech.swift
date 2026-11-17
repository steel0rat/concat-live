// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat contributors

import Foundation
@preconcurrency import AVFoundation
import Speech

public typealias DictationCallback = @convention(c) (UInt64, Int32, UnsafePointer<CChar>?, Float, Double) -> Void

// The C entry points register cancellation synchronously. A queued MainActor
// start must never open a microphone after its Rust owner has been dropped.
private final class Request: @unchecked Sendable {
    let id: UInt64
    let callback: DictationCallback
    private let lock = NSLock()
    private var cancelled = false
    private var completed = false
    private var captureFailure: String?
    private var stopping = false
    private var capture: Capture?
    private var download: Progress?
    private var frames: UInt64 = 0
    private var rate: Double = 1
    private var power: Float = 0
    private var lastAudio = Date()

    init(id: UInt64, callback: @escaping DictationCallback) {
        self.id = id
        self.callback = callback
    }

    var active: Bool {
        lock.lock(); defer { lock.unlock() }
        return !cancelled && !completed
    }

    var accepting: Bool {
        lock.lock(); defer { lock.unlock() }
        return !cancelled && !completed && !stopping
    }

    func install(_ capture: Capture) throws {
        lock.lock(); defer { lock.unlock() }
        guard !cancelled && !stopping else { throw CancellationError() }
        self.capture = capture
    }

    func setDownload(_ progress: Progress?) {
        lock.lock()
        download = progress
        let cancel = cancelled || stopping
        lock.unlock()
        if cancel { progress?.cancel() }
    }

    func stop(cancel: Bool) {
        lock.lock()
        cancelled = cancelled || cancel
        stopping = true
        let capture = capture
        self.capture = nil
        let download = download
        self.download = nil
        lock.unlock()
        capture?.stop()
        download?.cancel()
    }

    var hasCapture: Bool {
        lock.lock(); defer { lock.unlock() }
        return capture != nil
    }

    func resetMeter() {
        lock.lock(); defer { lock.unlock() }
        frames = 0
        power = 0
        captureFailure = nil
        lastAudio = Date()
    }

    func failedCapture(_ message: String) {
        lock.lock(); defer { lock.unlock() }
        captureFailure = captureFailure ?? message
    }

    var failure: String? {
        lock.lock(); defer { lock.unlock() }
        return captureFailure
    }

    func heard(_ buffer: AVAudioPCMBuffer) {
        var value: Float = 0
        if let samples = buffer.floatChannelData, buffer.frameLength > 0 {
            let count = Int(buffer.frameLength)
            let stride = buffer.stride
            for index in 0..<count { value += samples[0][index * stride] * samples[0][index * stride] }
            value = sqrt(value / Float(count))
        }
        lock.lock(); defer { lock.unlock() }
        frames += UInt64(buffer.frameLength)
        rate = buffer.format.sampleRate
        power = min(1, value)
        lastAudio = Date()
    }

    func meter() -> (Float, Double, Double) {
        lock.lock(); defer { lock.unlock() }
        return (power, Double(frames) / max(1, rate), Date().timeIntervalSince(lastAudio))
    }

    func emit(_ kind: Int32, _ text: String = "", level: Float = 0, seconds: Double = 0) {
        lock.lock()
        if cancelled || completed { lock.unlock(); return }
        if kind == 4 || kind == 5 { completed = true }
        lock.unlock()
        text.withCString { callback(id, kind, $0, level, seconds) }
    }
}

private enum Requests {
    static let lock = NSLock()
    nonisolated(unsafe) private static var values: [UInt64: Request] = [:]

    static func insert(_ request: Request) -> Bool {
        lock.lock(); defer { lock.unlock() }
        guard values.isEmpty else { return false }
        values[request.id] = request
        return true
    }

    static func get(_ id: UInt64, remove: Bool = false) -> Request? {
        lock.lock(); defer { lock.unlock() }
        return remove ? values.removeValue(forKey: id) : values[id]
    }
}

private struct Sound: @unchecked Sendable {
    let buffer: AVAudioPCMBuffer
}

private final class Capture: @unchecked Sendable {
    private let lock = NSLock()
    private var engine: AVAudioEngine?
    private var stopped = false
    private var tapped = false
    private let continuation: AsyncStream<Sound>.Continuation

    init(continuation: AsyncStream<Sound>.Continuation) {
        self.continuation = continuation
    }

    func start(request: Request) throws {
        lock.lock(); defer { lock.unlock() }
        guard !stopped && request.accepting else { throw CancellationError() }
        let engine = AVAudioEngine()
        let input = engine.inputNode
        let format = input.outputFormat(forBus: 0)
        guard format.sampleRate > 0 && format.channelCount > 0 else {
            throw Failure("Микрофон недоступен. Проверьте выбранное устройство ввода в настройках macOS.")
        }
        let continuation = self.continuation
        input.installTap(onBus: 0, bufferSize: AVAudioFrameCount(max(1024, format.sampleRate / 10)), format: format) { buffer, _ in
            guard request.accepting else { return }
            request.heard(buffer)
            // AVAudioEngine reuses tap buffers after this callback returns.
            guard let copy = buffer.copy() as? AVAudioPCMBuffer else {
                request.failedCapture("Не удалось сохранить звуковой буфер для распознавания.")
                continuation.finish()
                return
            }
            if case .dropped = continuation.yield(Sound(buffer: copy)) {
                request.failedCapture("Распознавание не успевает за записью. Попробуйте закрыть другие приложения.")
                continuation.finish()
            }
        }
        tapped = true
        self.engine = engine
        engine.prepare()
        request.resetMeter()
        do { try engine.start() }
        catch {
            input.removeTap(onBus: 0)
            tapped = false
            self.engine = nil
            throw error
        }
    }

    func stop() {
        lock.lock()
        if stopped { lock.unlock(); return }
        stopped = true
        let engine = engine
        self.engine = nil
        let tapped = tapped
        self.tapped = false
        lock.unlock()
        engine?.stop()
        if tapped { engine?.inputNode.removeTap(onBus: 0) }
        continuation.finish()
    }

    deinit { stop() }
}

private struct Failure: LocalizedError {
    let message: String
    init(_ message: String) { self.message = message }
    var errorDescription: String? { message }
}

@available(macOS 26.0, *)
private final class Converter {
    private var converter: AVAudioConverter?
    private var source: AVAudioFormat?
    let destination: AVAudioFormat

    init(destination: AVAudioFormat) { self.destination = destination }

    func convert(_ buffer: AVAudioPCMBuffer) throws -> AVAudioPCMBuffer {
        if buffer.format == destination { return buffer }
        if source != buffer.format {
            source = buffer.format
            converter = AVAudioConverter(from: buffer.format, to: destination)
            converter?.primeMethod = .none
        }
        guard let converter else { throw Failure("Формат микрофона не подходит для распознавания речи.") }
        let frames = AVAudioFrameCount(ceil(Double(buffer.frameLength) * destination.sampleRate / buffer.format.sampleRate) + 256)
        guard let output = AVAudioPCMBuffer(pcmFormat: destination, frameCapacity: frames) else {
            throw Failure("Не удалось подготовить звуковой буфер.")
        }
        let input = ConverterInput(buffer)
        var failure: NSError?
        let status = converter.convert(to: output, error: &failure) { _, result in input.take(result) }
        if let failure { throw failure }
        guard status != .error else { throw Failure("Не удалось преобразовать звук микрофона.") }
        return output
    }

    func flush() throws -> [AVAudioPCMBuffer] {
        guard let converter else { return [] }
        var tail: [AVAudioPCMBuffer] = []
        while true {
            try Task.checkCancellation()
            guard let output = AVAudioPCMBuffer(pcmFormat: destination, frameCapacity: 4096) else {
                throw Failure("Не удалось завершить преобразование звука.")
            }
            var failure: NSError?
            let status = converter.convert(to: output, error: &failure) { _, result in
                result.pointee = .endOfStream
                return nil
            }
            if let failure { throw failure }
            guard status != .error else { throw Failure("Не удалось завершить преобразование звука.") }
            if output.frameLength > 0 { tail.append(output) }
            if status == .endOfStream || output.frameLength == 0 { return tail }
        }
    }
}

// AVAudioConverter's callback is @Sendable although conversion consumes one
// buffer synchronously. This holder makes that one-shot handoff explicit.
private final class ConverterInput: @unchecked Sendable {
    private let lock = NSLock()
    private var supplied = false
    let buffer: AVAudioPCMBuffer
    init(_ buffer: AVAudioPCMBuffer) { self.buffer = buffer }

    func take(_ status: UnsafeMutablePointer<AVAudioConverterInputStatus>) -> AVAudioBuffer? {
        lock.lock(); defer { lock.unlock() }
        if supplied { status.pointee = .noDataNow; return nil }
        supplied = true
        status.pointee = .haveData
        return buffer
    }
}

@available(macOS 26.0, *)
@MainActor
private final class Dictation {
    let request: Request
    let locale: String
    var task: Task<Void, Never>?
    var results: Task<String, Error>?
    var producer: Task<Void, Error>?
    var meter: Task<Void, Never>?
    var analyzer: SpeechAnalyzer?

    init(request: Request, locale: String) {
        self.request = request
        self.locale = locale
    }

    func cancel() {
        request.stop(cancel: true)
        task?.cancel()
        results?.cancel()
        producer?.cancel()
        meter?.cancel()
        if let analyzer { Task { await analyzer.cancelAndFinishNow() } }
    }

    func run(file: String? = nil) async {
        defer {
            request.stop(cancel: false)
            meter?.cancel()
            _ = Requests.get(request.id, remove: true)
            Sessions.values.removeValue(forKey: request.id)
        }
        do {
            guard request.accepting else { throw CancellationError() }
            request.emit(0, "Подготовка локальной диктовки…")
            guard let locale = await DictationTranscriber.supportedLocale(equivalentTo: Locale(identifier: locale)) else {
                throw Failure("Выбранный язык не поддерживается локальной диктовкой на этом Mac.")
            }
            let transcriber = DictationTranscriber(locale: locale, preset: .progressiveLongDictation)
            if let installation = try await AssetInventory.assetInstallationRequest(supporting: [transcriber]) {
                request.setDownload(installation.progress)
                request.emit(0, "Загрузка языковой модели Apple…")
                try await installation.downloadAndInstall()
                request.setDownload(nil)
            }
            try Task.checkCancellation()
            guard request.accepting else { throw CancellationError() }
            let analyzer = SpeechAnalyzer(modules: [transcriber])
            self.analyzer = analyzer
            let request = self.request
            results = Task.detached {
                do {
                    var finalized = ""
                    for try await result in transcriber.results {
                        try Task.checkCancellation()
                        guard request.active else { throw CancellationError() }
                        let phrase = String(result.text.characters).trimmingCharacters(in: .whitespacesAndNewlines)
                        if result.isFinal {
                            finalized = Self.join(finalized, phrase)
                            request.emit(2, finalized)
                        } else {
                            request.emit(2, Self.join(finalized, phrase))
                        }
                    }
                    if request.accepting && request.hasCapture {
                        throw Failure("Сервис распознавания завершил работу до остановки записи.")
                    }
                    return finalized
                } catch {
                    // The input producer may be awaiting the next microphone
                    // buffer. Closing capture wakes it when recognition fails.
                    request.failedCapture("Сервис распознавания речи завершился с ошибкой: \(error.localizedDescription)")
                    request.stop(cancel: false)
                    throw error
                }
            }
            if let file {
                let audio = try AVAudioFile(forReading: URL(fileURLWithPath: file))
                request.emit(1)
                try await analyzer.start(inputAudioFile: audio, finishAfterFile: true)
                request.emit(6)
            } else {
                try await microphone(analyzer: analyzer, transcriber: transcriber)
            }
            guard let results else { throw Failure("Распознавание не запустилось.") }
            let text = try await results.value.trimmingCharacters(in: .whitespacesAndNewlines)
            guard request.active else { return }
            if text.isEmpty { throw Failure("Речь не распознана. Попробуйте говорить ближе к микрофону.") }
            request.emit(4, text)
        } catch is CancellationError {
            request.stop(cancel: false)
            if request.active { request.emit(5, request.failure ?? "Диктовка остановлена до начала записи.") }
        } catch {
            request.stop(cancel: false)
            if request.active { request.emit(5, "Не удалось распознать речь: \(error.localizedDescription)") }
        }
        results?.cancel()
        producer?.cancel()
        if let analyzer { await analyzer.cancelAndFinishNow() }
    }

    nonisolated static func join(_ left: String, _ right: String) -> String {
        if left.isEmpty { return right }
        if right.isEmpty { return left }
        return left + " " + right
    }

    func microphone(analyzer: SpeechAnalyzer, transcriber: DictationTranscriber) async throws {
        // TCC terminates an unbundled process without this purpose string.
        guard let purpose = Bundle.main.object(forInfoDictionaryKey: "NSMicrophoneUsageDescription") as? String,
              !purpose.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            throw Failure("Для доступа к микрофону запустите установленное приложение Concat Live.app.")
        }
        let permission = AVCaptureDevice.authorizationStatus(for: .audio)
        switch permission {
        case .authorized: break
        case .notDetermined:
            request.emit(0, "Разрешите доступ к микрофону в окне macOS…")
            let allowed = await AVCaptureDevice.requestAccess(for: .audio)
            guard allowed else { throw Failure("Доступ к микрофону запрещён. Разрешите его для Concat Live в Системных настройках → Конфиденциальность и безопасность → Микрофон.") }
        default:
            throw Failure("Доступ к микрофону запрещён. Разрешите его для Concat Live в Системных настройках → Конфиденциальность и безопасность → Микрофон.")
        }
        try Task.checkCancellation()
        guard request.accepting else { throw CancellationError() }
        guard let format = await SpeechAnalyzer.bestAvailableAudioFormat(compatibleWith: [transcriber]) else {
            throw Failure("Не найден формат звука для локальной диктовки.")
        }
        try await analyzer.prepareToAnalyze(in: format)
        guard request.accepting else { throw CancellationError() }
        let (audio, captured) = AsyncStream<Sound>.makeStream(bufferingPolicy: .bufferingOldest(32))
        let (input, supplied) = AsyncStream<AnalyzerInput>.makeStream(bufferingPolicy: .bufferingOldest(64))
        let capture = Capture(continuation: captured)
        try request.install(capture)
        let request = self.request
        producer = Task.detached {
            defer { supplied.finish() }
            let converter = Converter(destination: format)
            for await sound in audio {
                try Task.checkCancellation()
                guard request.active else { throw CancellationError() }
                let converted = try converter.convert(sound.buffer)
                if converted.frameLength == 0 { continue }
                if case .dropped = supplied.yield(AnalyzerInput(buffer: converted)) {
                    throw Failure("Распознавание не успевает за записью. Попробуйте закрыть другие приложения.")
                }
            }
            if let failure = request.failure { throw Failure(failure) }
            for converted in try converter.flush() {
                if case .dropped = supplied.yield(AnalyzerInput(buffer: converted)) {
                    throw Failure("Распознавание не успевает за записью. Попробуйте закрыть другие приложения.")
                }
            }
        }
        try await analyzer.start(inputSequence: input)
        try capture.start(request: request)
        request.emit(1)
        meter = Task {
            while !Task.isCancelled && request.accepting {
                let (level, seconds, stalled) = request.meter()
                request.emit(3, level: level, seconds: seconds)
                if seconds >= 180 {
                    request.emit(0, "Достигнут предел 3 минуты. Завершаю распознавание…")
                    request.stop(cancel: false)
                    request.emit(6)
                    break
                }
                if stalled > 6 {
                    request.stop(cancel: false)
                    request.emit(5, "Микрофон перестал передавать звук. Проверьте подключение устройства.")
                    self.producer?.cancel()
                    await analyzer.cancelAndFinishNow()
                    break
                }
                try? await Task.sleep(for: .milliseconds(100))
            }
        }
        guard let producer else { throw Failure("Не удалось запустить обработку звука.") }
        try await producer.value
        request.stop(cancel: false)
        request.emit(0, "Завершаю распознавание…")
        request.emit(6)
        try await analyzer.finalizeAndFinishThroughEndOfInput()
    }
}

@available(macOS 26.0, *)
@MainActor
private enum Sessions {
    static var values: [UInt64: Dictation] = [:]
}

@_cdecl("concat_dictation_available")
public func concatDictationAvailable() -> Int32 {
    if #available(macOS 26.0, *) { return 1 }
    return 0
}

private func begin(_ id: UInt64, _ locale: String, _ callback: @escaping DictationCallback, file: String? = nil) {
    let request = Request(id: id, callback: callback)
    guard Requests.insert(request) else {
        request.emit(5, "Другая диктовка уже запущена.")
        return
    }
    if #available(macOS 26.0, *) {
        Task { @MainActor in
            guard request.accepting else {
                if request.active { request.emit(5, request.failure ?? "Диктовка остановлена до начала записи.") }
                _ = Requests.get(id, remove: true)
                return
            }
            let session = Dictation(request: request, locale: locale)
            Sessions.values[id] = session
            session.task = Task { await session.run(file: file) }
        }
    } else {
        request.emit(5, "Для локальной диктовки Apple нужна macOS 26 или новее.")
        _ = Requests.get(id, remove: true)
    }
}

@_cdecl("concat_dictation_start")
public func concatDictationStart(_ id: UInt64, _ locale: UnsafePointer<CChar>, _ callback: @escaping DictationCallback) {
    begin(id, String(cString: locale), callback)
}

@_cdecl("concat_dictation_stop")
public func concatDictationStop(_ id: UInt64) {
    if let request = Requests.get(id) {
        request.stop(cancel: false)
        request.emit(6)
    }
}

@_cdecl("concat_dictation_cancel")
public func concatDictationCancel(_ id: UInt64) {
    guard let request = Requests.get(id, remove: true) else { return }
    request.stop(cancel: true)
    if #available(macOS 26.0, *) {
        Task { @MainActor in Sessions.values.removeValue(forKey: id)?.cancel() }
    }
}

#if CONCAT_DICTATION_QA
@_cdecl("concat_dictation_start_file")
public func concatDictationStartFile(_ id: UInt64, _ locale: UnsafePointer<CChar>, _ file: UnsafePointer<CChar>, _ callback: @escaping DictationCallback) {
    begin(id, String(cString: locale), callback, file: String(cString: file))
}
#endif
