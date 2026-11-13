import Foundation
import XCTest
@testable import TimelordPresentation

final class PresentationTests: XCTestCase {
    private let now = Date(timeIntervalSince1970: 1_000_000)

    func testPauseCleanupIsDistinctFromStartingAndCompletedPause() throws {
        let status = try operationalStatus()
        let pausing = LivePresentation.snapshot(status: status, controlOperation: .pause, now: now)
        XCTAssertEqual(pausing.phase, .pausing)
        XCTAssertEqual(pausing.phase.title, "Pausing")
        XCTAssertEqual(pausing.timelord.title, "Stopping")
        XCTAssertEqual(pausing.explanation, "Pausing Timelord; waiting for the worker to stop.")
        let paused = LivePresentation.snapshot(status: nil, paused: true, now: now)
        XCTAssertEqual(paused.phase, .paused)
        let starting = LivePresentation.snapshot(status: nil, controlOperation: .start, now: now)
        XCTAssertEqual(starting.phase, .starting)
        let runningReload = LivePresentation.snapshot(status: status, controlOperation: .reload, now: now)
        XCTAssertEqual(runningReload.phase, .restarting)
        XCTAssertEqual(runningReload.explanation, "Restarting Timelord; waiting for the previous worker to stop.")
        let pausedReload = LivePresentation.snapshot(status: nil, paused: true, controlOperation: .reload, now: now)
        XCTAssertEqual(pausedReload.phase, .paused)
    }

    func testResponsiveFaultIsNotPresentedAsAnUnresponsiveWorker() throws {
        let data = Data("""
        {"api_version":1,"service_pid":123,"timelord_state":"fault","message":"Proof verification failed","production_enabled":true,"full_node":{"host":"localhost","state":"ready"},"boards":[],"started_unix":999000,"timestamp_unix":1000000,"proofs_delivered":0,"tuning_paused":false}
        """.utf8)
        let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
        let status = try decoder.decode(ServiceStatus.self, from: data)
        let snapshot = LivePresentation.snapshot(status: status, workerPID: status.servicePid, now: now)
        XCTAssertEqual(snapshot.phase, .serviceUnavailable)
        XCTAssertEqual(snapshot.timelord.title, "Stopped")
    }

    func testUnresponsiveWorkerIsNotPresentedAsStoppedUntilAbsent() {
        let existing = LivePresentation.snapshot(status: nil, workerPID: 123, error: "Status unavailable", now: now)
        XCTAssertEqual(existing.phase, .serviceUnavailable)
        XCTAssertEqual(existing.timelord.title, "Not responding")
        let absent = LivePresentation.snapshot(status: nil, error: "Status unavailable", now: now)
        XCTAssertEqual(absent.timelord.title, "Stopped")
    }

    func testServiceUptimeSurvivesAnEngineSessionRestart() throws {
        let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
        func status(sessionStart: Double) throws -> ServiceStatus {
            let original = try operationalStatus()
            // Preserve the fixture's node/board fields while changing only clocks.
            let object: [String: Any] = ["api_version":original.apiVersion, "service_pid":original.servicePid,
                "timelord_state":"running", "message":"", "production_enabled":true,
                "full_node":["host":"localhost", "state":"ready"], "boards":[],
                "started_unix":sessionStart, "service_started_unix":900000,
                "timestamp_unix":1000000, "latest_proof_unix":1000000,
                "proofs_delivered":0, "tuning_paused":false]
            return try decoder.decode(ServiceStatus.self, from: JSONSerialization.data(withJSONObject:object))
        }
        let before = LivePresentation.snapshot(status:try status(sessionStart:999000), now:now)
        let after = LivePresentation.snapshot(status:try status(sessionStart:999700), now:now)
        XCTAssertEqual(before.uptimeSeconds, 100000)
        XCTAssertEqual(after.uptimeSeconds, before.uptimeSeconds)
        XCTAssertEqual(before.sessionUptimeSeconds, 1000)
        XCTAssertEqual(after.sessionUptimeSeconds, 300)
    }

    func testDeliveredMedianUsesTheSameScrapeWindowAndStopsWhenPaused() {
        let fixture = PresentationScenario.runningOneBoard.snapshot(now: now)
        let snapshot = AppSnapshot(phase: .running, explanation: "", node: fixture.node, timelord: fixture.timelord, boards: fixture.boards, proofsToday: nil, uptimeSeconds: nil, now: now, serviceAvailable: true)
        let history = snapshot.boards[0].histories[0]
        XCTAssertEqual(fixture.recentMedian(for: history), "—", "An unavailable service must not show historical data as live telemetry")
        let values = history.sampledObservations(in: DateInterval(start: now.addingTimeInterval(-300), end: now)).map(\.ips).sorted()
        XCTAssertFalse(values.isEmpty)
        XCTAssertEqual(snapshot.recentMedian(for: history), DisplayFormat.ips((values[(values.count - 1) / 2] + values[values.count / 2]) / 2))
        XCTAssertEqual(PresentationScenario.runningOneBoard.snapshot(now: now, paused: true).recentMedian(for: history), "—")
    }

    func testRawHandoffKeepsOnlyARecentMeasurementAndIdleDoesNotShowItAsActive() throws {
        let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
        func sample(_ state: String, ips: Double = 1055000) throws -> RawSpeed {
            try decoder.decode(RawSpeed.self, from: Data("{\"engine\":0,\"ips\":\(ips),\"timestamp_unix\":1000000,\"state\":\"\(state)\"}".utf8))
        }
        let handoff = try sample("sampling")
        XCTAssertNil(handoff.freshIPS(at: now))
        XCTAssertEqual(handoff.displayText(at: now.addingTimeInterval(2)), "1.055M")
        XCTAssertEqual(handoff.displayText(at: now.addingTimeInterval(6)), "Starting")
        XCTAssertEqual(try sample("sampling", ips: -1).displayText(at: now), "Starting")
        let idle = try sample("idle")
        XCTAssertNil(idle.freshIPS(at: now))
        XCTAssertEqual(idle.displayText(at: now), "Idle")
        XCTAssertEqual(try sample("running").displayText(at: now.addingTimeInterval(6)), "—")
    }

    func testRawSpeedUsesThePhysicalEngineForEachAssignedChain() {
        let chains: [Chain] = [.challenge, .reward, .infusedChallenge]
        for count in 1...3 {
            let histories = Array(chains.prefix(count)).map(ChainHistory.empty)
            var board = BoardSnapshot(id: "a", name: "A", state: .active, temperatureC: 40,
                temperatureLimitC: 55, profile: "Automatic", frequencyMHz: 1900,
                voltageV: 1, histories: histories, lastProof: now,
                rawSpeeds: (0...2).map { RawSpeed(engine: $0, ips: Double(1_000_000 + $0), timestampUnix: now.timeIntervalSince1970, state: "running") })
            for (index, history) in histories.enumerated() {
                let expected = count == 1 ? 1 : index
                XCTAssertEqual(board.rawSpeed(for: history.chain)?.engine, expected)
                XCTAssertEqual(board.rawSpeed(for: history.chain)?.ips, Double(1_000_000 + expected))
            }
            board.rawSpeeds = []
            XCTAssertNil(board.rawSpeed(for: histories[0].chain))
        }
    }

    func testRawSpeedExpiresAndRejectsInvalidValues() throws {
        let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
        let speed = try decoder.decode(RawSpeed.self, from: Data(#"{"engine":2,"ips":1055000,"timestamp_unix":1000000}"#.utf8))
        XCTAssertEqual(speed.engine, 2)
        XCTAssertEqual(speed.freshIPS(at: now.addingTimeInterval(5)), 1055000)
        XCTAssertNil(speed.freshIPS(at: now.addingTimeInterval(6)))
        XCTAssertNil(speed.freshIPS(at: now.addingTimeInterval(-1)))
    }

    func testScrapeMeanWeightsTimeInsteadOfProofCountAndKeepsLowRecoveryValues() {
        // A burst of three fast outputs must not outweigh three scrapes held at 400K.
        let start = Date(timeIntervalSince1970: 900_000)
        let window = DateInterval(start: start, duration: 120)
        let history = ChainHistory(chain: .challenge, samples: [
            ProofObservation(date: start, ips: 400_000),
            ProofObservation(date: start.addingTimeInterval(89), ips: 400_000),
            ProofObservation(date: start.addingTimeInterval(91), ips: 1_000_000),
            ProofObservation(date: start.addingTimeInterval(92), ips: 1_000_000),
            ProofObservation(date: start.addingTimeInterval(93), ips: 1_000_000)],
            captureIntervals: [window], events: [HistoryEvent(date: start, end: nil, kind: .recovery)])
        XCTAssertEqual(history.sampledObservations(in: window).map(\.ips), [400_000,400_000,400_000,1_000_000])
        XCTAssertEqual(history.statistics(in: window).average, 550_000)
    }

    func testNormalHistoryFetchLagDoesNotMakeFreshIPSUnavailable() {
        let start = Date(timeIntervalSince1970: 900_000)
        let history = ChainHistory(chain: .challenge, samples: [ProofObservation(date: start.addingTimeInterval(25), ips: 900_000)], captureIntervals: [DateInterval(start: start, duration: 30)], events: [])
        XCTAssertTrue(history.hasFreshValue(at: start.addingTimeInterval(45)))
        XCTAssertFalse(history.hasFreshValue(at: start.addingTimeInterval(60)))
    }

    func testStaleGaugeCreatesGapWithoutInventingZerosOrBridgingTheChart() {
        let start = Date(timeIntervalSince1970: 900_000)
        let window = DateInterval(start: start, duration: 180)
        let history = ChainHistory(chain: .challenge, samples: [
            ProofObservation(date: start, ips: 900_000),
            ProofObservation(date: start.addingTimeInterval(150), ips: 1_000_000)],
            captureIntervals: [window], events: [])
        XCTAssertFalse(history.hasFreshValue(at: start.addingTimeInterval(90)))
        XCTAssertEqual(history.sampledObservations(in: window).map(\.ips), [900_000,900_000,1_000_000,1_000_000])
        XCTAssertEqual(history.plotPoints(in: window).map(\.segment), [0,0,1,1])
        XCTAssertEqual(history.statistics(in: window).coverage, 1)
    }

    func testHeadlineUsesLatestOutputFromAnyChainAndExpiresEvenWithHistory() {
        let end = Date(timeIntervalSince1970: 900_120)
        let window = DateInterval(start: end.addingTimeInterval(-120), end: end)
        let cc = ChainHistory(chain: .challenge, samples: [ProofObservation(date: window.start, ips: 400_000, sequence: 1)], captureIntervals: [window], events: [])
        let rc = ChainHistory(chain: .reward, samples: [ProofObservation(date: window.start.addingTimeInterval(60), ips: 1_000_000, sequence: 2)], captureIntervals: [window], events: [])
        let board = BoardSnapshot(id: "a", name: "A", state: .active, temperatureC: nil, temperatureLimitC: 55, profile: "Manual", frequencyMHz: nil, voltageV: nil, histories: [rc,cc], lastProof: end)
        func snapshot(_ now: Date, phase: AppPhase = .running) -> AppSnapshot {
            AppSnapshot(phase: phase, explanation: "", node: ConnectionSnapshot(title: "", detail: "", tone: .positive), timelord: ConnectionSnapshot(title: "", detail: "", tone: .positive), boards: [board], proofsToday: 2, uptimeSeconds: 120, now: now, serviceAvailable: true)
        }
        XCTAssertEqual(snapshot(end).headlineSpeed, "760K")
        XCTAssertEqual(snapshot(end.addingTimeInterval(30)).headlineSpeed, "—")
        XCTAssertEqual(snapshot(end, phase: .paused).headlineSpeed, "—")
        let sameTime = ChainHistory(chain: .challenge, samples: [ProofObservation(date: end, ips: 400_000, sequence: 10), ProofObservation(date: end, ips: 1_000_000, sequence: 2)], captureIntervals: [window], events: [])
        XCTAssertEqual(sameTime.sampledObservations(in: window).last?.ips, 400_000)
    }

    func testBoardTileShowsRecentMeanWhileHeadlineKeepsHourlyMean() {
        let end = Date(timeIntervalSince1970: 900_000)
        let window = HistoryRange.hour.interval(endingAt: end)
        let samples = (0...60).map { minute in
            ProofObservation(date: window.start.addingTimeInterval(Double(minute) * 60), ips: minute < 55 ? 400_000 : 1_000_000)
        }
        let history = ChainHistory(chain: .challenge, samples: samples, captureIntervals: [window], events: [])
        let board = BoardSnapshot(id: "a", name: "A", state: .active, temperatureC: nil, temperatureLimitC: 55, profile: "Manual", frequencyMHz: nil, voltageV: nil, histories: [history], lastProof: end)
        func snapshot(_ now: Date, phase: AppPhase = .running) -> AppSnapshot {
            AppSnapshot(phase: phase, explanation: "", node: ConnectionSnapshot(title: "", detail: "", tone: .positive), timelord: ConnectionSnapshot(title: "", detail: "", tone: .positive), boards: [board], proofsToday: 61, uptimeSeconds: 3600, now: now, serviceAvailable: true)
        }
        XCTAssertEqual(snapshot(end).recentSpeed(for: history), "1.000M")
        XCTAssertEqual(snapshot(end).headlineSpeed, "455K")
        XCTAssertEqual(snapshot(end.addingTimeInterval(90)).recentSpeed(for: history), "1.000M")
        XCTAssertEqual(snapshot(end.addingTimeInterval(90)).recentMedian(for: history), "1.000M")
        XCTAssertEqual(snapshot(end.addingTimeInterval(90), phase: .waitingForProofs).recentSpeed(for: history), "1.000M")
        XCTAssertEqual(snapshot(end.addingTimeInterval(90), phase: .nodeUnreachable).recentMedian(for: history), "1.000M")
        XCTAssertEqual(snapshot(end.addingTimeInterval(400)).recentSpeed(for: history), "—")
        XCTAssertEqual(snapshot(end.addingTimeInterval(400)).recentMedian(for: history), "—")
        XCTAssertEqual(snapshot(end, phase: .paused).recentSpeed(for: history), "—")
    }

    func testNativeClientFaultShowsReasonWithoutClaimingServiceReady() throws {
        let view = LivePresentation.snapshot(status: try operationalStatus(state: "fault", message: "Board A needs attention: Board A: Native control closed before acknowledgement", faultKind: "control_closed"), now: now)
        XCTAssertEqual(view.phase, .serviceUnavailable)
        XCTAssertEqual(view.runningTitle, "Proof service stopped")
        XCTAssertTrue(view.explanation.contains("ASIC client connection closed unexpectedly"))
        XCTAssertTrue(view.explanation.contains("Native control closed before acknowledgement"))
        XCTAssertEqual(view.timelord.title, "Stopped")
        XCTAssertEqual(view.timelord.tone, .critical)
        let other = LivePresentation.snapshot(status: try operationalStatus(state: "fault", message: "Other failure"), now: now)
        XCTAssertEqual(other.explanation, "Other failure")
        let untyped = LivePresentation.snapshot(status: try operationalStatus(state: "fault", message: "Board A needs attention: Native control closed before acknowledgement"), now: now)
        XCTAssertEqual(untyped.explanation, "Board A needs attention: Native control closed before acknowledgement")
    }

    func testRestoredChartKeepsLowSamplesAndDoesNotBridgeRestartDowntime() throws {
        let object: [String: Any] = ["api_version":3,"service_pid":42,"timestamp_unix":1000000,
            "capture_intervals":[["start_unix":999880,"end_unix":999910],["start_unix":999970,"end_unix":1000000]],
            "estimated_observations":[["id":1,"chain":"challenge","timestamp_unix":999900,"ips":400000],["id":2,"chain":"challenge","timestamp_unix":999990,"ips":1000000]],
            "events":[["id":1,"timestamp_unix":999910,"kind":"telemetry_gap","end_unix":999970]]]
        let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
        let history = try decoder.decode(HistoryPayload.self, from: JSONSerialization.data(withJSONObject: object))
        let view = LivePresentation.snapshot(status: try operationalStatus(), history: HistoryProjection(payload: history, boards: try operationalStatus().boards), now: now)
        let chain = try XCTUnwrap(view.headlineHistory)
        let window = HistoryRange.quarterHour.interval(endingAt: now)
        let points = chain.plotPoints(in: window)
        XCTAssertEqual(points.count, 2)
        XCTAssertNotEqual(points[0].segment, points[1].segment)
        XCTAssertEqual(chain.statistics(in: window).average, 700000)
        XCTAssertEqual(chain.statistics(in: window).coverage, 60.0 / 900.0, accuracy: 0.0001)
        XCTAssertNil(chain.observation(near: Date(timeIntervalSince1970: 999940), in: window))
    }

    func testAutomaticTuningKeepsLiveProofMetricsAndClearsStaleLearningState() throws {
        let status=try operationalStatus()
        let object: [String:Any] = ["api_version":1,"service_pid":42,"service_state":"ready","timelord_state":"running","message":"Producing verified proofs.","production_enabled":true,"started_unix":999000,"timestamp_unix":1000000,"latest_proof_unix":1000000,"proofs_delivered":7,"tuning_paused":false,
            "full_node":["host":"localhost","port":8444,"network":"mainnet","state":"ready"],
            "boards":[["id":"a","name":"A","client_port":18110,"assigned_chains":["challenge","reward","infused_challenge"],"connected_clients":3,"hardware_state":"connected","temperature_c":42.5,"temperature_limit_c":55,"frequency_mhz":1450,"voltage_v":1,"profile":"Automatic","latest_proof_unix":1000000,"automatic":["state":"testing","message":"Testing a 10 MHz increase","active_seconds":90,"cooldown_seconds":0]]],"connected_client_count":3,"delivered_ips":1000000]
        let decoder=JSONDecoder();decoder.keyDecodingStrategy = .convertFromSnakeCase
        let automatic=try decoder.decode(ServiceStatus.self,from:JSONSerialization.data(withJSONObject:object))
        let view=LivePresentation.snapshot(status:automatic,history:try operationalHistory(),now:now)
        XCTAssertEqual(view.phase,.running);XCTAssertEqual(view.headlineSpeed,"700K");XCTAssertEqual(view.proofsToday,status.proofsDelivered)
        XCTAssertEqual(view.runningTitle,"Running · Optimizing");XCTAssertTrue(view.boards[0].isAutomatic);XCTAssertEqual(view.boards[0].automaticTitle,"Testing settings")
        var paused=object;paused["tuning_paused"]=true
        let pausedStatus=try decoder.decode(ServiceStatus.self,from:JSONSerialization.data(withJSONObject:paused))
        XCTAssertEqual(LivePresentation.snapshot(status:pausedStatus,now:now).runningTitle,"Running")
        let stale=LivePresentation.snapshot(status:automatic,now:now.addingTimeInterval(20));XCTAssertFalse(stale.serviceAvailable);XCTAssertFalse(stale.automaticActive)
    }

    func testNodeSyncIsIndependentOfTimelordAndExpires() throws {
        func node(_ synced: Bool, _ syncing: Bool, _ timestamp: Double) throws -> NodeSyncStatus {
            let json = "{\"synced\":\(synced),\"syncing\":\(syncing),\"height\":9500241,\"target_height\":9501000,\"timestamp_unix\":\(timestamp)}"
            let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
            return try decoder.decode(NodeSyncStatus.self, from: Data(json.utf8))
        }
        let synced = try node(true, false, now.timeIntervalSince1970)
        let stopped = LivePresentation.snapshot(status: nil, nodeSync: synced, paused: true, now: now)
        XCTAssertEqual(stopped.node.detail, "Synced")
        XCTAssertEqual(stopped.node.height, 9_500_241)
        XCTAssertEqual(stopped.phase, .paused)
        let syncing = LivePresentation.snapshot(status: nil, nodeSync: try node(false, true, now.timeIntervalSince1970), now: now)
        XCTAssertEqual(syncing.node.detail, "Syncing")
        let stale = LivePresentation.snapshot(status: nil, nodeSync: synced, now: now.addingTimeInterval(31))
        XCTAssertEqual(stale.node.detail, "Sync status unavailable")
        XCTAssertNil(stale.node.height)
        let future = LivePresentation.snapshot(status: nil, nodeSync: synced, now: now.addingTimeInterval(-10))
        XCTAssertNil(future.node.height)
        XCTAssertEqual(LivePresentation.snapshot(status: nil).node.detail, "Sync status unavailable")
    }

    func testRecoveryRetainsObservedSlowProofsInAverage() {
        let window = DateInterval(start: now.addingTimeInterval(-100), end: now)
        let history = ChainHistory(chain: .challenge,
            samples: [ProofObservation(date: now.addingTimeInterval(-90), ips: 1_000_000), ProofObservation(date: now.addingTimeInterval(-50), ips: 400_000), ProofObservation(date: now.addingTimeInterval(-20), ips: 1_000_000)],
            captureIntervals: [window], events: [HistoryEvent(date: now.addingTimeInterval(-50), end: nil, kind: .recovery)])
        let stats = history.statistics(in: window)
        XCTAssertEqual(stats.average, 800_000)
        XCTAssertEqual(stats.minimum, 400_000)
        XCTAssertEqual(stats.coverage, 1)
        XCTAssertEqual(Set(history.plotPoints(in: window).map(\.segment)).count, 1)
    }

    func testMissingCaptureBreaksLineAndHasSeparateCoverage() {
        let window = DateInterval(start: now.addingTimeInterval(-100), end: now)
        let history = ChainHistory(chain: .reward,
            samples: [ProofObservation(date: now.addingTimeInterval(-90), ips: 900_000), ProofObservation(date: now.addingTimeInterval(-20), ips: 950_000)],
            captureIntervals: [DateInterval(start: window.start, duration: 40), DateInterval(start: now.addingTimeInterval(-40), end: now)], events: [])
        XCTAssertEqual(history.statistics(in: window).coverage, 0.8, accuracy: 0.0001)
        XCTAssertEqual(history.statistics(in: window).average, 925_000)
        XCTAssertEqual(history.plotPoints(in: window).map(\.segment), [0, 1])
    }

    func testKnownWorkOutageDoesNotBecomeMissingTelemetry() {
        let window = DateInterval(start: now.addingTimeInterval(-100), end: now)
        let history = ChainHistory(chain: .reward,
            samples: [ProofObservation(date: now.addingTimeInterval(-90), ips: 900_000), ProofObservation(date: now.addingTimeInterval(-20), ips: 950_000)],
            captureIntervals: [window], events: [HistoryEvent(date: now.addingTimeInterval(-60), end: now.addingTimeInterval(-30), kind: .nodeUnavailable)])
        XCTAssertEqual(history.statistics(in: window).coverage, 1)
        XCTAssertEqual(history.plotPoints(in: window).map(\.segment), [0, 1])
        XCTAssertEqual(history.statistics(in: window).observations, 2)
    }

    func testOverlappingCaptureIsNotCountedTwice() {
        let window = DateInterval(start: now.addingTimeInterval(-100), end: now)
        let history = ChainHistory(chain: .challenge, samples: [], captureIntervals: [window, window, DateInterval(start: now.addingTimeInterval(-200), end: now)], events: [])
        XCTAssertEqual(history.statistics(in: window).coverage, 1)
    }

    func testAdjacentCaptureDoesNotCreateArtificialLineBreaks() {
        let window = DateInterval(start: now.addingTimeInterval(-100), end: now)
        let history = ChainHistory(chain: .challenge,
            samples: [ProofObservation(date: now.addingTimeInterval(-90), ips: 900_000), ProofObservation(date: now.addingTimeInterval(-20), ips: 950_000)],
            captureIntervals: [DateInterval(start: window.start, duration: 60), DateInterval(start: now.addingTimeInterval(-60), end: now)], events: [])
        XCTAssertEqual(history.statistics(in: window).coverage, 1)
        XCTAssertEqual(history.plotPoints(in: window).map(\.segment), [0, 0, 0])
    }

    func testHoverOverUnknownDataDoesNotShowANearbyHealthyValue() {
        let window = DateInterval(start: now.addingTimeInterval(-100), end: now)
        let history = ChainHistory(chain: .challenge,
            samples: [ProofObservation(date: now.addingTimeInterval(-90), ips: 900_000), ProofObservation(date: now.addingTimeInterval(-20), ips: 950_000)],
            captureIntervals: [DateInterval(start: window.start, duration: 40), DateInterval(start: now.addingTimeInterval(-40), end: now)], events: [])
        XCTAssertNil(history.observation(near: now.addingTimeInterval(-50), in: window))
        XCTAssertEqual(history.observation(near: now.addingTimeInterval(-10), in: window)?.ips, 950_000)
    }

    func testInvalidAndOutsideWindowObservationsAreNotPlotted() {
        let window = HistoryRange.hour.interval(endingAt: now)
        let history = ChainHistory(chain: .challenge,
            samples: [ProofObservation(date: now, ips: .nan), ProofObservation(date: now, ips: -.infinity), ProofObservation(date: now, ips: -1), ProofObservation(date: window.start.addingTimeInterval(-1), ips: 2_000_000), ProofObservation(date: now.addingTimeInterval(-10), ips: 0)], captureIntervals: [window], events: [])
        XCTAssertEqual(history.statistics(in: window).observations, 1)
        XCTAssertEqual(history.statistics(in: window).average, 0)
    }

    func testEmptyHistoryNeverLooksLikeZeroSpeed() {
        let history = ChainHistory.empty(.infusedChallenge)
        let stats = history.statistics(in: HistoryRange.hour.interval(endingAt: now))
        XCTAssertNil(stats.average)
        XCTAssertEqual(stats.coverage, 0)
        XCTAssertEqual(DisplayFormat.ips(stats.average), "—")
    }

    func testFixtureLayoutsCoverEachChainOnceWithoutSummingIPS() {
        for scenario in [PresentationScenario.runningOneBoard, .runningThreeBoards] {
            let snapshot = scenario.snapshot(now: now)
            let chains = snapshot.boards.flatMap(\.histories).map(\.chain)
            XCTAssertEqual(Set(chains), Set(Chain.allCases))
            XCTAssertEqual(chains.count, 3)
            XCTAssertFalse(snapshot.serviceAvailable)
            XCTAssertLessThan(snapshot.headlineHistory!.statistics(in: HistoryRange.hour.interval(endingAt: now)).average!, 1_100_000)
        }
        XCTAssertEqual(PresentationScenario.runningOneBoard.snapshot(now: now).boards.count, 1)
        XCTAssertEqual(PresentationScenario.runningThreeBoards.snapshot(now: now).boards.count, 3)
    }

    func testFailureScenariosKeepTheirOwnStates() {
        XCTAssertEqual(PresentationScenario.waitingForNode.snapshot(now: now).phase, .waitingForNode)
        XCTAssertNil(PresentationScenario.waitingForNode.snapshot(now: now).headlineHistory?.statistics(in: HistoryRange.hour.interval(endingAt: now)).average)
        XCTAssertEqual(PresentationScenario.nodeUnreachable.snapshot(now: now).phase, .nodeUnreachable)
        XCTAssertEqual(PresentationScenario.boardDisconnected.snapshot(now: now).boards[2].state, .disconnected)
        XCTAssertEqual(PresentationScenario.temperatureLimit.snapshot(now: now).boards[1].state, .thermalStop)
        XCTAssertEqual(PresentationScenario.runningThreeBoards.snapshot(now: now, paused: true).phase, .paused)
    }

    func testFixtureDoesNotDiscardRecoveryObservations() {
        let history = PresentationScenario.runningOneBoard.snapshot(now: now).headlineHistory!
        let window = HistoryRange.hour.interval(endingAt: now)
        XCTAssertLessThan(history.statistics(in: window).minimum!, 700_000)
        XCTAssertLessThan(history.statistics(in: window).coverage, 1)
    }

    func testHelperReadyIsNotProductiveAndStaleStatusIsUnavailable() throws {
        let json = """
        {"api_version":1,"service_pid":42,"service_state":"ready","timelord_state":"running","message":"fixture","production_enabled":false,
         "started_unix":999000,"timestamp_unix":1000000,"latest_proof_unix":1000000,"proofs_delivered":0,"tuning_paused":false,
         "full_node":{"host":"localhost","port":8444,"network":"mainnet","state":"connected"},
         "boards":[{"id":"a","name":"A","client_port":18110,"assigned_chains":["challenge","reward","infused_challenge"],"connected_clients":3,"hardware_state":"not_inspected","profile":"Manual"}],"connected_client_count":3,"delivered_ips":1000000}
        """
        let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
        let status = try decoder.decode(ServiceStatus.self, from: Data(json.utf8))
        let view = LivePresentation.snapshot(status: status, now: now)
        XCTAssertEqual(view.phase, .setupRequired)
        XCTAssertEqual(view.headlineSpeed, "—")
        XCTAssertNil(view.latestProof)
        XCTAssertEqual(view.boards[0].state, .uninspected)
        let stale = LivePresentation.snapshot(status: status, now: now.addingTimeInterval(20))
        XCTAssertFalse(stale.serviceAvailable)
        XCTAssertTrue(stale.boards.isEmpty)
    }
}

extension PresentationTests {
    func testStoppedConfiguredAutomaticBoardExplainsHowToStart() {
        var config = AppConfiguration()
        config.service.startAutomatically = false
        config.node.certificate = "/fixture/cert"; config.node.privateKey = "/fixture/key"; config.node.caCertificate = "/fixture/ca"
        config.hardware.clientBinary = "/fixture/client"
        let device = DiscoveredDevice(name: "Fixture", location: 1, serial: "fixture-only")
        config.hardware = BoardDiscovery.reconcile([device], hardware: config.hardware, service: config.service)
        let ready = LivePresentation.snapshot(status: nil, configuration: config, detectedDevices: [device], now: now)
        XCTAssertEqual(ready.runningTitle, "Ready to run")
        XCTAssertTrue(ready.explanation.contains("Enable the background service"))
        config.service.startAutomatically = true
        let automaticStart = LivePresentation.snapshot(status: nil, configuration: config, detectedDevices: [device], now: now)
        XCTAssertEqual(automaticStart.explanation, ready.explanation, "Legacy app-start preference cannot override durable intent")
        XCTAssertNil(ready.proofsToday)
        let paused = LivePresentation.snapshot(status: nil, configuration: config, detectedDevices: [device], paused: true, now: now)
        XCTAssertEqual(paused.runningTitle, "Paused")
        let disconnected = LivePresentation.snapshot(status: nil, configuration: config, detectedDevices: [], now: now)
        XCTAssertFalse(disconnected.readyToRun)
        config.node.certificate = nil
        let incomplete = LivePresentation.snapshot(status: nil, configuration: config, detectedDevices: [device], now: now)
        XCTAssertFalse(incomplete.readyToRun)
    }
    func testManagedOwnerEnrichesUSBLocationWithoutDuplicateIdentity() {
        let owner = DiscoveredDevice(name: "Managed", location: 7, serial: "A")
        let hiddenSerial = DiscoveredDevice(name: "USB", location: 7)
        let merged = BoardDiscovery.includingManagedOwner(owner, in: [hiddenSerial])
        XCTAssertEqual(merged.map(\.id), [7])
        XCTAssertEqual(merged.first?.serial, "A")
        let repeated = BoardDiscovery.includingManagedOwner(owner, in: merged)
        XCTAssertEqual(repeated.map(\.id), [7])
        let replacement = DiscoveredDevice(name: "Fresh", location: 7, serial: "B")
        XCTAssertEqual(BoardDiscovery.includingManagedOwner(owner, in: [replacement]).first?.serial, "B")
        let moved = DiscoveredDevice(name: "Fresh", location: 8, serial: "A")
        XCTAssertEqual(BoardDiscovery.includingManagedOwner(owner, in: [moved]).map(\.id), [8])
        for inventory in [[hiddenSerial, moved], [moved, hiddenSerial]] {
            let result = BoardDiscovery.includingManagedOwner(owner, in: inventory)
            XCTAssertEqual(result.count, 2)
            XCTAssertEqual(result.filter { $0.serial == "A" }.count, 1)
            XCTAssertEqual(result.first { $0.serial == "A" }?.location, 8)
            XCTAssertEqual(result.first { $0.location == 7 }?.serial, nil)
        }
        let reconciled = BoardDiscovery.reconcile(merged, hardware: HardwareConfiguration(), service: ServiceConfiguration())
        XCTAssertEqual(reconciled.boards.count, 1)
        XCTAssertEqual(reconciled.boards.first?.serial, "A")
    }

    func testDetectedBoardsUseAutomaticSeedAndRememberProfilesAcrossUsbPorts() {
        var hardware=HardwareConfiguration()
        hardware=BoardDiscovery.reconcile([DiscoveredDevice(name:"Chia VDF A",location:4113,serial:"one")],hardware:hardware,service:ServiceConfiguration())
        let first=hardware.boards[0]
        XCTAssertEqual(first.profileName,"Automatic")
        XCTAssertEqual(first.frequencyMhz,1450);XCTAssertEqual(first.voltageV,1.0)
        hardware.boards[0].profile="manual";hardware.boards[0].frequencyMhz=1540
        hardware=BoardDiscovery.reconcile([],hardware:hardware,service:ServiceConfiguration())
        XCTAssertTrue(hardware.boards.isEmpty);XCTAssertEqual(hardware.rememberedBoards.count,1)
        hardware=BoardDiscovery.reconcile([DiscoveredDevice(name:"Chia VDF A",location:9001,serial:"one")],hardware:hardware,service:ServiceConfiguration())
        XCTAssertEqual(hardware.boards[0].id,first.id)
        XCTAssertEqual(hardware.boards[0].profileName,"Manual")
        XCTAssertEqual(hardware.boards[0].frequencyMhz,1540)
        XCTAssertEqual(hardware.boards[0].deviceLocation,9001)
        hardware=BoardDiscovery.reconcile([DiscoveredDevice(name:"Chia VDF A",location:9001,serial:"replacement")],hardware:hardware,service:ServiceConfiguration())
        XCTAssertEqual(hardware.boards[0].profileName,"Automatic")
        XCTAssertNotEqual(hardware.boards[0].id,first.id)
    }
    func testAutomaticPortsSkipHTTPListenersAcrossHotplug() {
        var config=AppConfiguration()
        config.service.statusListen="127.0.0.1:18111"
        config.service.observabilityListen="[::]:18110"
        config.service.supervisorObservabilityListen="127.0.0.1:18112"
        let devices=(0..<3).map {DiscoveredDevice(name:"ASIC",location:4113+$0,serial:"serial-\($0)")}
        // Also repair remembered client ports when HTTP configuration changes.
        config.hardware=BoardDiscovery.reconcile(devices,hardware:config.hardware,service:config.service)
        for count in [3,0,1,3] {
            config.hardware=BoardDiscovery.reconcile(Array(devices.prefix(count)),hardware:config.hardware,service:config.service)
            let ports=Set(config.hardware.boards.map(\.clientPort))
            XCTAssertEqual(ports.count,count)
            XCTAssertFalse(ports.contains(18110))
            XCTAssertFalse(ports.contains(18111))
            XCTAssertFalse(ports.contains(18112))
        }
    }
    func testAutomaticEngineAssignmentsAndNoConfiguredGhostCards() {
        var config=AppConfiguration()
        let devices=(0..<3).map {DiscoveredDevice(name:"ASIC",location:4113+$0,serial:"serial-\($0)")}
        for (count,expected) in [(1,[3]),(2,[2,1]),(3,[1,1,1])] {
            config.hardware=BoardDiscovery.reconcile(Array(devices.prefix(count)),hardware:config.hardware,service:config.service)
            XCTAssertEqual(BoardTopology.counts(config.hardware.boards),expected)
            let snapshot=LivePresentation.snapshot(status:nil,configuration:config,detectedDevices:Array(devices.prefix(count)),now:now)
            XCTAssertEqual(snapshot.boards.count,count)
            XCTAssertEqual(snapshot.boards.map { $0.histories.count },expected)
            XCTAssertEqual(Set(snapshot.boards.flatMap(\.histories).map(\.chain)),Set(Chain.allCases))
        }
        config.hardware.chainAssignments = ChainAssignments(challenge: "serial-2", reward: "serial-0", infusedChallenge: "serial-1")
        let mapped = LivePresentation.snapshot(status:nil,configuration:config,detectedDevices:devices,now:now)
        XCTAssertEqual(mapped.boards.map { $0.histories.map(\.chain) }, [[.reward], [.infusedChallenge], [.challenge]])
        let pinned = BoardDiscovery.reconcile([DiscoveredDevice(name: "ASIC", location: 999, serial: "serial-2")], hardware: config.hardware, service: config.service)
        XCTAssertEqual(pinned.boards.count, 3)
        XCTAssertEqual(pinned.boards.map(\.serial), config.hardware.boards.map(\.serial))
        XCTAssertEqual(pinned.boards[2].deviceLocation, 999)
        let unplugged=LivePresentation.snapshot(status:nil,configuration:config,detectedDevices:[],now:now)
        XCTAssertTrue(unplugged.boards.isEmpty)
    }
    func testSupervisedBoardsRemainVisibleWhenUSBDiscoveryOnlySeesOneBoard() throws {
        func status(states: [String], clients: [Int]) throws -> ServiceStatus {
            let boards: [[String: Any]] = states.enumerated().map { index, state in
                ["id":"board-\(index)", "name":"ASIC \(index)", "serial":"serial-\(index)",
                 "device_location":index + 1, "assigned_chains":[Chain.allCases[index].rawValue],
                 "connected_clients":clients[index], "hardware_state":state,
                 "latest_proof_unix":1000000, "profile":"Automatic"]
            }
            let data: [String: Any] = ["api_version":1, "service_pid":42,
                "timelord_state":"running", "message":"", "production_enabled":true,
                "started_unix":999000, "timestamp_unix":1000000, "latest_proof_unix":1000000,
                "proofs_delivered":7, "tuning_paused":false, "connected_client_count":clients.reduce(0,+),
                "full_node":["host":"localhost", "state":"ready"], "boards":boards]
            let decoder=JSONDecoder();decoder.keyDecodingStrategy = .convertFromSnakeCase
            return try decoder.decode(ServiceStatus.self,from:JSONSerialization.data(withJSONObject:data))
        }
        let discovered=[DiscoveredDevice(name:"ASIC 0",location:1,serial:"serial-0")]
        let healthy=try status(states:["connected","connected","connected"],clients:[1,1,1])
        // Hotplug can leave discovery/configuration with only one active entry,
        // while the service already owns all three recovered boards.
        var config=AppConfiguration()
        var board=ConfiguredBoard(id:"board-0",name:"ASIC 0",port:18110,location:1)
        board.serial="serial-0";config.hardware.boards=[board]
        let snapshot=LivePresentation.snapshot(status:healthy,configuration:config,detectedDevices:discovered,now:now)
        XCTAssertEqual(snapshot.boards.map(\.id),["board-0","board-1","board-2"])
        XCTAssertEqual(snapshot.boards.map(\.state),[.active,.active,.active])

        let idle=try status(states:["connected","connected","connected"],clients:[1,1,0])
        XCTAssertEqual(LivePresentation.snapshot(status:idle,detectedDevices:[],now:now).boards.count,3,
                       "An idle ICC owner's USB metadata can also be hidden")
        let starting=try status(states:["starting","starting","starting"],clients:[1,0,0])
        XCTAssertEqual(LivePresentation.snapshot(status:starting,detectedDevices:[],now:now).boards.map(\.id),["board-0"])
        let absent=try status(states:["fault","reconnecting","starting"],clients:[0,0,0])
        XCTAssertTrue(LivePresentation.snapshot(status:absent,detectedDevices:[],now:now).boards.isEmpty,
                      "Configured or unavailable clients alone are not presence evidence")
        XCTAssertTrue(LivePresentation.snapshot(status:healthy,detectedDevices:[],now:now.addingTimeInterval(15)).boards.isEmpty,
                      "Stale status cannot preserve board presence")
    }

    func testRecoveryRemainsVisibleAndCountsDownWithoutRequiringRun() throws {
        let status = try operationalStatus(production: false, state: "waiting_for_clients", recoveryReason: "Cooling ASIC", retryAfter: 1_000_010)
        let snapshot = LivePresentation.snapshot(status: status, detectedDevices: [], now: now)
        XCTAssertEqual(snapshot.phase, .starting)
        XCTAssertEqual(snapshot.boards.count, 1)
        XCTAssertEqual(snapshot.boards.first?.state, .recovering)
        XCTAssertEqual(snapshot.boards.first?.recoveryText(at: now), "Cooling ASIC · Retrying in 10s")
        XCTAssertEqual(snapshot.boards.first?.recoveryText(at: now.addingTimeInterval(11)), "Cooling ASIC · Retrying automatically")
    }

    private func operationalStatus(production: Bool = true, latest: Double = 1_000_000, state: String = "running", message: String = "Producing verified proofs.", chains: [String] = ["challenge","reward","infused_challenge"], faultKind: String? = nil, recoveryReason: String? = nil, retryAfter: Double? = nil) throws -> ServiceStatus {
        var board: [String: Any] = ["id":"a","name":"A","client_port":18110,"assigned_chains":chains,"connected_clients":3,"hardware_state":"connected","temperature_c":42.5,"temperature_limit_c":55,"frequency_mhz":1540,"voltage_v":1,"recoveries":2,"latest_proof_unix":latest,"profile":"Manual"]
        if let faultKind { board["fault_kind"] = faultKind }
        if let recoveryReason { board["recovery_reason"] = recoveryReason; board["hardware_state"] = "fault" }
        if let retryAfter { board["retry_after_unix"] = retryAfter }
        let object: [String: Any] = ["api_version":1,"service_pid":42,"service_state":"ready","timelord_state":state,"message":message,"production_enabled":production,"started_unix":999000,"timestamp_unix":1000000,"latest_proof_unix":latest,"proofs_delivered":7,"tuning_paused":false,
            "full_node":["host":"localhost","port":8444,"network":"mainnet","state":"ready"],
            "boards":[board],"connected_client_count":3,"delivered_ips":1000000]
        let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
        return try decoder.decode(ServiceStatus.self, from: JSONSerialization.data(withJSONObject: object))
    }
    private func operationalHistory(pid: Int = 42, latestIPS: Double = 1_000_000) throws -> HistoryProjection {
        let object: [String: Any] = ["api_version":3,"service_pid":pid,"timestamp_unix":1000000,"capture_intervals":[["start_unix":996400,"end_unix":1000000]],
            "estimated_observations":[["id":1,"chain":"challenge","timestamp_unix":999900,"ips":400000],["id":2,"chain":"challenge","timestamp_unix":999970,"ips":latestIPS]],
            "events":[["id":1,"timestamp_unix":999900,"board_id":"a","kind":"recovery"],["id":2,"timestamp_unix":999910,"board_id":NSNull(),"kind":"node_unavailable","end_unix":999920]]]
        let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
        return HistoryProjection(payload: try decoder.decode(HistoryPayload.self, from: JSONSerialization.data(withJSONObject: object)), boards: try operationalStatus().boards)
    }
    func testHistoryProjectionReusesArraysWhileFreshnessAndClockClippingStayLive() throws {
        let history = try operationalHistory()
        let status = try operationalStatus()
        var cache = HistorySelectionCache()
        let first = LivePresentation.snapshot(status: status, history: history, historyCache: &cache, now: now)
        let second = LivePresentation.snapshot(status: status, history: history, historyCache: &cache, now: now.addingTimeInterval(2))
        let a = try XCTUnwrap(first.headlineHistory)
        let b = try XCTUnwrap(second.headlineHistory)
        a.samples.withUnsafeBufferPointer { first in
            b.samples.withUnsafeBufferPointer { second in
                XCTAssertEqual(first.baseAddress, second.baseAddress, "status polling rebuilt the chart observations")
            }
        }
        let changedSelection = LivePresentation.snapshot(status: try operationalStatus(chains: ["reward"]), history: history, historyCache: &cache, now: now)
        XCTAssertTrue(try XCTUnwrap(changedSelection.headlineHistory).samples.isEmpty, "cache retained an unassigned chain")
        let newPayload = try operationalHistory(latestIPS: 2_000_000)
        let updated = LivePresentation.snapshot(status: status, history: newPayload, historyCache: &cache, now: now)
        XCTAssertEqual(updated.headlineHistory?.samples.last?.ips, 2_000_000, "new payload did not invalidate the selection cache")
        XCTAssertEqual(first.headlineHistory?.samples.last?.ips, 1_000_000, "cached snapshots must stay immutable")
        let clockAhead = LivePresentation.snapshot(status: status, history: history, historyCache: &cache, now: now.addingTimeInterval(-2))
        XCTAssertEqual(try XCTUnwrap(clockAhead.headlineHistory).captureIntervals.last?.end, now.addingTimeInterval(-2))
        let stale = LivePresentation.snapshot(status: status, history: history, historyCache: &cache, now: now.addingTimeInterval(400))
        XCTAssertFalse(stale.serviceAvailable)
        XCTAssertNil(stale.headlineHistory)
    }

    func testSerialChainOverridesSurviveConfigurationEditsAndDiscovery() throws {
        var config = AppConfiguration()
        config.hardware.chainAssignments = ChainAssignments(challenge: "serial-c", reward: "serial-a", infusedChallenge: "serial-b")
        let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
        let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
        var restored = try decoder.decode(AppConfiguration.self, from: encoder.encode(config))
        restored.node.host = "another-node"
        restored.hardware = BoardDiscovery.reconcile([DiscoveredDevice(name: "ASIC", location: 99, serial: "serial-c")], hardware: restored.hardware, service: restored.service)
        let saved = try decoder.decode(AppConfiguration.self, from: encoder.encode(restored))
        XCTAssertEqual(saved.hardware.chainAssignments, config.hardware.chainAssignments)
        var json = try XCTUnwrap(JSONSerialization.jsonObject(with: encoder.encode(saved)) as? [String: Any])
        var hardware = try XCTUnwrap(json["hardware"] as? [String: Any]); hardware.removeValue(forKey: "chain_assignments"); json["hardware"] = hardware
        XCTAssertNil(try decoder.decode(AppConfiguration.self, from: JSONSerialization.data(withJSONObject: json)).hardware.chainAssignments)
    }
    func testDistinctProofsAtTheSameTimestampKeepSeparateChartIdentities() {
        let history = ChainHistory(chain: .challenge, samples: [ProofObservation(date: now.addingTimeInterval(-10), ips: 900000, id: "proof-1"), ProofObservation(date: now.addingTimeInterval(-10), ips: 900000, id: "proof-2")], captureIntervals: [HistoryRange.hour.interval(endingAt: now)], events: [])
        XCTAssertEqual(Set(history.observations(in: HistoryRange.hour.interval(endingAt: now)).map(\.id)).count, 2)
        XCTAssertEqual(history.statistics(in: HistoryRange.hour.interval(endingAt: now)).observations, 1)
    }
    func testOperationalStatusAndHistoryUseSampledVerifiedOutputsWithoutSummingChains() throws {
        let snapshot = LivePresentation.snapshot(status: try operationalStatus(), history: try operationalHistory(), now: now)
        XCTAssertEqual(snapshot.phase, .running)
        XCTAssertEqual(snapshot.headlineSpeed, "700K")
        XCTAssertEqual(snapshot.proofsToday, 7)
        XCTAssertEqual(snapshot.boards.first?.temperatureC, 42.5)
        XCTAssertEqual(snapshot.boards.first?.state, .active)
        XCTAssertEqual(snapshot.headlineHistory?.plotPoints(in: HistoryRange.hour.interval(endingAt: now)).map(\.segment), [0, 1])
    }
    func testStaleProofAndAnotherServiceHistoryCannotLookProductive() throws {
        let staleProof = LivePresentation.snapshot(status: try operationalStatus(latest: 999800), history: try operationalHistory(pid: 43), now: now)
        XCTAssertEqual(staleProof.phase, .waitingForProofs)
        XCTAssertEqual(staleProof.headlineSpeed, "—")
        XCTAssertNotEqual(staleProof.boards.first?.state, .active)
        let incapable = LivePresentation.snapshot(status: try operationalStatus(production: false), history: try operationalHistory(), now: now)
        XCTAssertEqual(incapable.headlineSpeed, "—")
        XCTAssertNil(incapable.latestProof)
        XCTAssertNil(incapable.proofsToday)
    }
    func testBoardFaultDoesNotHideHealthyBoardGaugeOrHover() {
        let start = Date(timeIntervalSince1970: 0)
        let window = DateInterval(start: start, duration: 120)
        let samples = [ProofObservation(date: start.addingTimeInterval(25), ips: 1_100_000, boardID: "healthy"),
                       ProofObservation(date: start.addingTimeInterval(55), ips: 1_200_000, boardID: "healthy")]
        let failure = HistoryEvent(date: start.addingTimeInterval(20), end: nil, kind: .boardFault, boardID: "failed")
        let history = ChainHistory(chain: .challenge, samples: samples, captureIntervals: [window], events: [failure])
        XCTAssertEqual(history.sampledObservations(in: window).count, 4)
        XCTAssertNotNil(history.observation(near: start.addingTimeInterval(60), in: window))
        XCTAssertEqual(Set(history.plotPoints(in: window).map(\.segment)), [0])
        let ownFailure = HistoryEvent(date: start.addingTimeInterval(20), end: nil, kind: .boardFault, boardID: "healthy")
        XCTAssertTrue(ChainHistory(chain: .challenge, samples: samples, captureIntervals: [window], events: [ownFailure]).sampledObservations(in: window).isEmpty)
    }
    func testOneStoppedBoardDoesNotMakeHealthyBoardOrServiceLookStopped() throws {
        func status(_ states: [String]) throws -> ServiceStatus {
            let boards: [[String: Any]] = states.enumerated().map { index, state in
                ["id": "board-\(index)", "name": "ASIC \(index)", "assigned_chains": [index == 0 ? "challenge" : "infused_challenge"],
                 "connected_clients": state == "connected" ? 1 : 0, "hardware_state": state, "latest_proof_unix": 1_000_000, "profile": "Automatic"]
            }
            let object: [String: Any] = ["api_version": 1, "service_pid": 42, "timestamp_unix": 1_000_000, "started_unix": 999000,
                "timelord_state": "running", "message": "Producing proofs", "production_enabled": true, "proofs_delivered": 7, "tuning_paused": false,
                "latest_proof_unix": 1_000_000, "full_node": ["host": "localhost", "port": 8444, "network": "mainnet", "state": "ready"], "boards": boards]
            let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
            return try decoder.decode(ServiceStatus.self, from: JSONSerialization.data(withJSONObject: object))
        }
        for failed in ["fault", "thermal_stop", "starting", "reconnecting"] {
            let view = LivePresentation.snapshot(status: try status(["connected", failed]), now: now)
            XCTAssertEqual(view.phase, .running)
            XCTAssertEqual(view.boards[0].state, .active)
            XCTAssertNotEqual(view.boards[1].state, .active)
        }
        XCTAssertEqual(LivePresentation.snapshot(status: try status(["fault", "fault"]), now: now).phase, .serviceUnavailable)
        let reconnecting = LivePresentation.snapshot(status: try status(["reconnecting", "reconnecting"]), now: now)
        XCTAssertEqual(reconnecting.phase, .waitingForProofs)
        XCTAssertTrue(reconnecting.boards.allSatisfy { $0.state == .disconnected })
    }

    func testAdmissionFailureShowsActualReasonInsteadOfNodeOrMissingBoardAdvice() throws {
        let message = "ASIC 1 needs attention: Admission native owner did not confirm hardware quiescence"
        func status(_ hardware: String, node: String = "disconnected", state: String = "waiting_for_clients") throws -> ServiceStatus {
            let object: [String: Any] = ["api_version": 1, "service_pid": 42, "timestamp_unix": 1_000_000, "started_unix": 999000,
                "timelord_state": state, "message": message, "production_enabled": true, "proofs_delivered": 0, "tuning_paused": false,
                "full_node": ["host": "fixture", "port": 58445, "network": "ephemeral", "state": node],
                "boards": [["id": "a", "name": "ASIC 1", "assigned_chains": [], "connected_clients": 0, "hardware_state": hardware, "profile": "Automatic"]]]
            let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
            return try decoder.decode(ServiceStatus.self, from: JSONSerialization.data(withJSONObject: object))
        }
        // An active FTDI owner can hide enumeration metadata. Lifecycle rows
        // describe checking/failure, without claiming detection or productivity.
        let failed = LivePresentation.snapshot(status: try status("quarantined"), detectedDevices: [], now: now)
        XCTAssertEqual(failed.phase, .boardMissing)
        XCTAssertEqual(failed.explanation, message)
        XCTAssertEqual(failed.boards.count, 1)
        XCTAssertEqual(failed.boards.first?.state, .needsAttention)
        XCTAssertEqual(failed.boards.first?.state.title, "Needs attention")
        XCTAssertEqual(failed.headlineSpeed, "—")
        let checking = LivePresentation.snapshot(status: try status("checking", node: "waiting_for_hardware"), detectedDevices: [], now: now)
        XCTAssertEqual(checking.phase, .starting)
        XCTAssertEqual(checking.boards.first?.state, .checking)
        XCTAssertEqual(checking.explanation, message)
        let unavailable = LivePresentation.snapshot(status: try status("connected", state: "waiting_for_node"), now: now)
        XCTAssertEqual(unavailable.phase, .nodeUnreachable)
        let nodeFailure = LivePresentation.snapshot(status: try status("quarantined", state: "waiting_for_node"), now: now)
        XCTAssertEqual(nodeFailure.phase, .nodeUnreachable, "Admission advice must not mask a real node-session failure")
        let paused = LivePresentation.snapshot(status: try status("quarantined"), paused: true, now: now)
        XCTAssertEqual(paused.phase, .paused)
    }

}
