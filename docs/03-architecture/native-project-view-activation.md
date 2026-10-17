# Native project view activation: CLI adapter와 report v2

상태: P03.3b.3.3d 구현 전 계약 · 2026-10-17 · 실제 CLI/창 인수 전

## 목적과 source 감사

[Native project host](native-project-host.md)를 실제 CLI에서 소비하되 project load·actor·CPU extraction·GPU 준비·retirement를 CLI 안에 다시 만들지 않기 위한 문서다. 기존 `project_view.rs`의 실행은 initial packet을 동기 준비하고 별도 packet coordinator를 실행한다. 새 host는 resource-only initial 입력과 actual present ACK를 사용하므로 기존 final_packet 기반 JSON을 그대로 채울 수 없다. adapter 동작과 관측 의미를 먼저 고정한다.

이 경로는 engine integration이며 샘플 게임을 추가하지 않는다. 기존 non-activation/audio path는 보존하고 새 source activation은 명시적 flag로 선택한다. 모든 non-audio CLI가 이미 host로 이전됐다는 주장은 하지 않는다. 후속 공통화는 실제 중복과 필요한 동작을 확인한 뒤 수행한다.

## flag와 시작 순서

- `project view --asset-activation`은 `--stdio-control`을 요구한다. 새 기본 동작으로 몰래 활성화하지 않는다.
- activation과 `--audio` 조합은 project/cache/worker/device 생성 전에 거부한다. P03.3c의 audio generation 연결은 아직 미구현이다.
- `--asset-preparation --asset-activation`은 허용한다. activation이 이미 포함하는 같은 preparation worker를 사용하며 두 번째 service를 시작하지 않는다.
- 기존 크기·runtime/frame limit·controlled entity·report 검증을 유지하고 stale activation help/error를 고친다.
- args 검증 직후, CLI의 preparation service와 `load_project_runtime`보다 먼저 activation branch로 진입한다. host options에 선택한 Manual/Realtime config와 project 경로를 전달한다.

초기 source load/CPU prepare는 현재 host의 동기 startup/창 생성 전 경계를 따른다. 이 adapter로 CPU load 전 loading UI를 구현했다고 하지 않는다. run_view는 macOS 호출/UI thread에서 실행한다.

## host와 CLI의 역할

| host만 수행 | CLI adapter가 수행 |
| --- | --- |
| initial source load·같은 authority actor·bindings 수집·CPU owner·실제 SessionFrame extraction·stage/frame 전달·actual present ACK·회수·종료 소유권 | clap 검증·기존 JSONL transport/controller·diagnostic key→session operation mapping·report JSON/파일 출력 |

CLI는 `NativeProjectHost::start`, `attach_control`, `control`, `run_view`, `try_shutdown`을 사용한다. protocol controller는 attached client를 받는다. `collect_bindings`, `ProjectRenderResources::prepare`, initial packet extraction 또는 두 번째 packet coordinator를 새 branch에서 호출하지 않는다. 작은 JSONL/input pump는 source/frame/device payload를 소유하지 않는다.

project 이름·schema/revision/entity count는 attached client의 `ProjectInfo { offset:0, limit:Some(1) }` 한 page에서 얻는다. 전체 bindings scan을 반복하지 않는다. `--controlled-entity`는 같은 session의 StableId `GetTransform`으로 확인한다. 기존 typed WindowInputSink와 diagnostic action/Manual Step helper를 재사용한다. 이것은 범용 gameplay input SDK 전체 인수가 아니다.

## 표시와 report v2

activation report는 `report_version: 2`, `presentation_mode: "managed_activation"`을 명시한다. 기존 non-activation report는 보존한다. CLI serialization과 renderer/window/resource 통계 helper는 가능한 범위에서 재사용하며 두 관측의 의미를 합치지 않는다.

| 관측 | 근거·의미 |
| --- | --- |
| `queued_publication` | host status의 `last_queued_publication()`. 기존 runtime ProjectPublicationIdentity이며 Frame.try_send 성공한 ingress enqueue. 전달 전은 null. UI dequeue·draw·present 보장이 아님 |
| `last_presented` | 실제 managed window의 마지막 game present metadata. bootstrap clear·Busy·Timeout은 포함하지 않음. publication/snapshot sequence·tick·원본 digest·actual view/physical dimensions 구분 |
| window/resources | 실제 ManagedWindowOutcome·WindowReport·유한 CPU/GPU counters. 이전 final packet Arc를 만들어 통계를 채우지 않음 |
| final tick/termination/shutdown counters | host.shutdown_report()의 기존 SessionShutdownReport. 없는 경우 값을 지어내지 않음 |

legacy `accepted_packet`은 activation report v2에서 생략한다. queued token이나 마지막 present를 UI가 수락한 final packet처럼 이름만 바꾸어 채우지 않는다. authority latest publication과 GPU 준비/실제 표시도 다르다. IDs/tick/sequence의 기존 decimal string wire 표현과 1MiB final JSON 한도를 유지한다.

## close·오류·종료

ProtocolController의 Close는 host control.request_view_close에 연결한다. view-close는 one-shot consumer를 닫고 추가 CPU frame 요청을 중지하며 같은 authority read/Step는 whole stop 전까지 가능하다. renderer의 ManagedWindowRead::Closed를 Empty/Busy와 같게 처리하지 않는다. Closed를 관측하면 consumer close·view-closed lifecycle로 수렴한다.

창 결과/오류 뒤에도 host를 살아 있게 유지하고 whole stop을 요청한다. tiny adapter pump의 종료를 관측하고 host.try_shutdown을 bounded deadline 안에서 poll한다. unfinished thread를 join하거나 Drop이 완료된 것처럼 성공을 내지 않는다. 출력/stdio worker는 기존 유한 transport의 detach/close/backpressure 규칙을 따른다. 실패 report는 확보한 실제 근거만 사용한다.

## 검증·파일 소유권

Luna A는 CLI project_view.rs와 private activation child module/관련 tests, 필요한 최소 protocol helper visibility만 편집한다. D가 host/workspace manifests/Cargo.lock과 focused Cargo lease를 단독 소유한다. CLI dependency/window feature 연결은 D의 manifest phase 이후 조정한다. renderer/window source는 검증한 snapshot에서 동결한다.

CPU 검사는 flag 효과 전 거부·redundant preparation·report v2의 의미와 범위를 확인한다. existing smoke helper에 같은 session의 실제 view 준비→source edit→Stage→Commit→actual pose/pixels/present ACK→회수와 close/stop 검사를 붙인다. fixture·별도 batch capture·CPU ready가 실제 native present를 대체하지 않는다. Rust SDK 공개 소비 검사와 CLI 실제 창 검사를 모두 통과해야 한다. hardware/전체 회귀/CI/package 인수 전 이 계약과 P03/D1/D2/D3는 미완료다.
