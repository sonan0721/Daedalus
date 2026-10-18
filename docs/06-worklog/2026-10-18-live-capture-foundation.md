# P02.3a.1: live capture runtime foundation 작업 기록

상태: runtime 구현·집중 CPU 검증 중, 실제 backend/PNG 전 · 2026-10-18

## 만든 이유와 출발점

[PR20 native host 인수](2026-10-18-native-host-integration.md)는 main261812a에서 끝냈다. 같은 창의 metadata와 별도 batch PNG를 실제 live pixels로 혼동하지 않기 위해 다음 runtime 요청/수명 검증을 별도 기록한다. 상위 P02.3a·P03.3b.3.3·D1/D2/D3는 미완료다. branch는 `feat/live-capture-foundation`이고 [소유권 계약](../03-architecture/live-capture-ownership.md)을 먼저 commit/push했다.

## 동결한 주요 선택

- 기존 activation/render-consumer factory에 additive capture sibling을 붙여 initial PreparedProject·actor·worker를 공유한다. renderer/host dependency를 runtime에 넣지 않는다.
- client queue admission은 deadline·normal/image reserve·전체 ticket ID를 고정한다. source generation/digest와 default tick floor는 actor의 한 dispatch turn에서 bind한다. ticket sequence는 client별이므로 artifact ID에서 client를 생략하지 않는다.
- opaque strong capability·weak interest·독립 work lease로 ACK/retained progress·GPU/encoder/file·native reader 수명을 나눈다. timeout/abandon과 실제 image reserve 회복은 별개다.
- None timeout5초/max30초, capture1개, global/negotiated limits·finite view·고정 RGBA8/sRGB PNG metadata를 검증한다. surface format/encoding과 PNG output encoding은 구분한다.
- responder close와 완료 쓰기의 순서를 명확히 하고 Busy에는 원래 request/payload ownership을 보존한다. 기존 Manual Step lane/default/public struct를 유지한다.
- CPU consumer의 Ready/metadata 보고는 신뢰 handoff 계약 시험이다. 실제 COPY_SRC·GPU/map·PNG artifact·wire Capture op/live_capture=true는 후속이다.

## 파일과 검증 소유권

Luna A는 runtime의 session_capture 새 module·session_control/project_preparation/lib와 CLI project_view_control의 최소 exhaustive result/error mapping을 작성한다. Luna C는 별도 `tests/capture_public_api.rs`의 공개 SDK/real actor 통합 시험만 작성한다. private API를 추측하지 않고 공유한 public header를 소비한다. renderer/host 작업자는 읽기 전용 다음 경계 감사만 수행했다.

한 Cargo lease는 A가 갖는다. source snapshot을 멈춘 뒤 library 집중 검사부터 실행하고 수정은 명령 종료 뒤 재개한다. C가 integration test를 작성하는 동안 해당 test/all-targets 빌드를 하지 않는다. C의 첫 test freeze는 SHA-256 `ac61443cb2657a9b6dc002b4d80528737f47cf3286f2dd235a7471cde408c1ed`였다. 실제 실행의 unused-mut 경고 한 곳만 제거한 후 최종 source는 `933743cb5824a23784b065d23555c82d7714796cdaa6c94a387151a5ac7fe53a`로 freeze해 A에게 전달했다. 둘 다 source freeze 뒤 통합/전체 검사를 진행한다. root는 문서·source review·결과 대조·Git/CI 인수를 맡는다. 새 dependency·lockfile·test framework는 추가하지 않는다.

## 첫 compiler 결과

`cargo check -p daedalus-runtime --all-features --locked`의 첫 시도는 exit101·6.401초였다. missing import, Drop이 있는 ActorCapture에서 필드를 move한 terminal 경로와 unused import를 검출했다. root도 raw를 대조했으며 아직 source 테스트 통과가 아니다. Drop guard의 취소/정리 의미를 유지하는 수정 뒤 다시 집중 검사한다.

raw `dist/project-evidence/p02-capture-foundation-luna-runtime-check-attempt01.log` SHA-256은 `8441256ca82a7bb8a387253079229b7be3a335f03fa536f30f362056f3728372`다. 이전 실패를 덮어쓰지 않는다. 이후 결과와 최종 source hash는 실제 실행 뒤 추가한다.

두 번째 check는 새 negotiated-limit helper의 CaptureFrameMetadata import 누락으로exit101이었다. raw02 SHA-256은 `7548c46634dd43a6446ea5b9866bf0380cbab0d808f041499bc9c8c1bb0173af`다. 수정한 check03은exit0·1.054초·SHA-256 `d66416fd794c5fba06620cdc2ae076c464fff0551e56f787865e7e1b397c58ed`였다. 이후 source가 계속 수정되므로 이 check를 최종 source 전체 인수로 보고하지 않는다.

## 첫 library 회귀와 아직 열린 검증

runtime all-feature `--lib` 첫 실행은116통과·0 ignored, raw `p02-capture-foundation-luna-runtime-lib-tests-attempt01.log` SHA-256 `ac35adb91e0c639ee591ff979c2f4f4c89e5d2c1641f1bac25a3de50d08e8566`이다. 새 두 capture case는 negotiated 크기/format completion 거부와 sRGB encoding 일관성을 검사한다. 기존 test가 포함된116을 새 capture lifecycle116개로 보고하지 않는다.

공개 SDK test의 첫 source review에서 Status flush가 ticket sequence를 소비한 뒤 다음 capture ID를 잘못 예상한 문제와, work lease가 계속 남아 progress/capability 단독 수명을 분리하지 못한 기대를 찾았다. 이는 실행된 test 실패가 아니다. C는 성공한 Status ID를 정확히 추적하고 progress·capability·work lease 각각의 단독 retention과 실제 admission/recovery를 검사하도록 수정해 freeze했다. 잠깐의 Busy/QueueFull은 bounded retry하고 나머지 오류와 잘못된 ID는 실패하도록 유지한다.

별도로 내부 library 실행02는117통과·1실패였다. `capture_uses_full_ticket_identity_waits_for_requested_tick_and_keeps_step_lane_free`가 Step으로 이미 sequence1을 사용한 두 번째 client의 다음 capture를 sequence1로 예상했지만 실제 값은2였다. raw `p02-capture-foundation-luna-runtime-lib-tests-attempt02.log` SHA-256은 `52362d367880b519418df2f3c8deb79a289bcbf45031030c11e2b4894236ca16`이다. 새 client로 sequence 충돌 검사를 분리하는 fixture 수정이며, production ID 재사용이나 Step lane 실패를 입증한 결과가 아니다. 수정 뒤 실제 재실행 결과는 별도 기록한다.

| a.1의 나머지 acceptance | 현재 근거/상태 |
| --- | --- |
| legacy/no sink·Starting/Ready/Unsupported/Detached·one-shot binding | public fixture 작성/리뷰 중, 전체 상태 검증 대기 |
| timeout/default/inspect·normal/image/queue 거부 rollback·두 client의 전체 ID·counter 고갈 | 집중 회귀 대기 |
| Manual Step reserve·handoff Busy deferral 중 원래 request 보존 | 집중 회귀 대기 |
| ACK 뒤 progress 단독·capability 단독·work lease 단독의 포화/회복 | public SDK fixture 수정 중 |
| take 전/후 timeout/abandon·responder loss/duplicate/receiver close·stop/panic | 집중 회귀 대기 |
| 실제 commit 전후 actor source binding·old-frame/current-generation 불일치 거부 | 집중 회귀 대기 |

두 source 담당의 최종 freeze와 이 행별 실제 결과 뒤에 a.1 인수를 판단한다. renderer/host/실제 PNG는 별도 a.2~a.4이며 전체 capture gate는 계속 미완료다.

## Lifecycle 집중 검사와 공개 API 첫 실행

`runtime-capture-lifecycle-attempt02`는9통과·114 filtered·0실패, wrapper3.323초였다. SHA-256은 `d95b05aaf71e78cbe733d1881b2ecad8a6e2c61ea10531cc9417882d275766b8`이다. 각 fixture는 실제 actor의 readiness/권한·전체 ticket ID·Step·Busy deferral·take 전후 timeout·responder 소멸/중복/closed endpoint·panic·commit 뒤 source binding을 검사한다. format/크기와 encoding 검사도 이9개에 포함되며 별도로 합산하지 않는다.

`runtime-allfeatures-tests-attempt01`은 내부123·공개 capture2·기타 integration2를 통과했다. raw SHA-256은 `f014d49dcca7d7848180c9933e9253eaa6597b737e33c53cb558c05413453fcc`다. 공개 test source의 unused-mut 경고 하나는 이후 제거했다. 이 실행의 성공을 경고 제거 뒤의 strict/전체 workspace 검사로 대신하지 않으며, capture consumer는 metadata만 제공하는 CPU fixture다.

기본 feature library 검사01은111통과·0실패, SHA-256 `02511c80e630d90e832aaf23da2a9c0b5c1de3470ed2d846d88b8fd8c9c1cdaa`였다. CLI all-feature check01도 통과했으며 SHA-256은 `a459cf85fc065bcc81124e8361a7f24f8d2b16990d179c5dd1ca43a43ff2b22b`다. 이 source snapshot 뒤에도 아래 strict 수정이 있어 최종 인수 실행과 구분한다.

## Strict 검사에서 찾은 구조 문제

runtime all-target/all-feature Clippy01은exit101이었다. Captured metadata가264bytes인 enum,144bytes인 dispatch error, actor argument8개, 중첩 if와 같은 Cancelled 분기를 검출했다. raw SHA-256은 `6f1bc50a310db8fde1ab348761bf933c98f6aa93a6a00315a8e9fbff946afb0c`다.

root는 새 public Captured payload를 `Box<CaptureFrameMetadata>`로 변경하고 private dispatch는 최초 생성한 같은 `Box<CaptureRequest>`를 Busy/Detached에도 돌려주는 수정 방향을 승인했다. Busy error 때마다 새 Box를 만들지 않는다. actor의 consumer port는 private 묶음으로 전달하며 나머지 조건은 간소화한다. 공개 SDK test 담당도 새 Captured constructor만 맞춰 freeze한다. 경고 억제로 통과시키지 않고 수정 후 strict를 다시 실행한다.
