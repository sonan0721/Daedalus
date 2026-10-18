# P02.3a.1: live capture runtime foundation 작업 기록

상태: P02.3a.1 로컬 통합 인수, 실제 backend/PNG 전 · 2026-10-18

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
| legacy/no sink·Starting/Ready/Unsupported/Detached·one-shot binding | `capture_is_opt_in_and_starts_unresolved` 및 `capture_readiness_is_one_shot_and_unsupported_or_detached_states_are_typed` 통과 |
| timeout/default/inspect·normal/image/queue 거부 rollback·두 client의 전체 ID·counter 고갈 | `invalid_capture_timeouts...`, `capture_queue_full...`, `capture_ticket_and_enqueue_sequence_exhaustion...`, `capture_result_credit_refusal...`, `capture_uses_full_ticket_identity...` 통과 |
| Manual Step reserve·handoff Busy deferral 중 원래 request 보존 | `capture_handoff_busy_keeps_the_request_owned_while_manual_step_progresses` 및 full-ticket case 통과 |
| ACK 뒤 progress 단독·capability 단독·work lease 단독의 포화/회복 | public `captured_terminal_uses_real_publication_identity_and_retained_owners_hold_image_slot`의 별도 수명/성공 ID 검사 통과 |
| take 전/후 timeout/abandon·responder loss/duplicate/receiver close·stop/panic | timeout·dropping-ticket·normal-stop·actor-panic 실제 actor case 통과, close-before-cancel 수정 뒤 workspace 회귀 통과 |
| 실제 commit 전후 actor source binding·old-frame/current-generation 불일치 거부 | `capture_dispatch_binds_committed_source_even_while_an_old_frame_is_held`와 negotiated size/format·encoding case 통과 |

아래 최종 freeze와 행별 실제 결과로 a.1만 로컬 인수한다. renderer/host/실제 PNG는 별도 a.2~a.4이며 전체 capture gate는 계속 미완료다. 마지막 PR·두 package·실제 Mac capture 인수는 a.4에서 확인한다.

## Lifecycle 집중 검사와 공개 API 첫 실행

`runtime-capture-lifecycle-attempt02`는9통과·114 filtered·0실패, wrapper3.323초였다. SHA-256은 `d95b05aaf71e78cbe733d1881b2ecad8a6e2c61ea10531cc9417882d275766b8`이다. 각 fixture는 실제 actor의 readiness/권한·전체 ticket ID·Step·Busy deferral·take 전후 timeout·responder 소멸/중복/closed endpoint·panic·commit 뒤 source binding을 검사한다. format/크기와 encoding 검사도 이9개에 포함되며 별도로 합산하지 않는다.

`runtime-allfeatures-tests-attempt01`은 내부123·공개 capture2·기타 integration2를 통과했다. raw SHA-256은 `f014d49dcca7d7848180c9933e9253eaa6597b737e33c53cb558c05413453fcc`다. 공개 test source의 unused-mut 경고 하나는 이후 제거했다. 이 실행의 성공을 경고 제거 뒤의 strict/전체 workspace 검사로 대신하지 않으며, capture consumer는 metadata만 제공하는 CPU fixture다.

기본 feature library 검사01은111통과·0실패, SHA-256 `02511c80e630d90e832aaf23da2a9c0b5c1de3470ed2d846d88b8fd8c9c1cdaa`였다. CLI all-feature check01도 통과했으며 SHA-256은 `a459cf85fc065bcc81124e8361a7f24f8d2b16990d179c5dd1ca43a43ff2b22b`다. 이 source snapshot 뒤에도 아래 strict 수정이 있어 최종 인수 실행과 구분한다.

## Strict 검사에서 찾은 구조 문제

runtime all-target/all-feature Clippy01은exit101이었다. Captured metadata가264bytes인 enum,144bytes인 dispatch error, actor argument8개, 중첩 if와 같은 Cancelled 분기를 검출했다. raw SHA-256은 `6f1bc50a310db8fde1ab348761bf933c98f6aa93a6a00315a8e9fbff946afb0c`다.

root는 새 public Captured payload를 `Box<CaptureFrameMetadata>`로 변경하고 private dispatch는 최초 생성한 같은 `Box<CaptureRequest>`를 Busy/Detached에도 돌려주는 수정 방향을 승인했다. Busy error 때마다 새 Box를 만들지 않는다. actor의 consumer port는 private 묶음으로 전달하며 나머지 조건은 간소화한다. 공개 SDK test 담당도 새 Captured constructor만 맞춰 freeze한다. 경고 억제로 통과시키지 않고 수정 후 strict를 다시 실행한다.

boxed Captured constructor에 맞춘 공개 test의 최종 source hash는 `07dd8885b791a939477cb5291cc12bd1e5782867d13205528e60f8424c65c510`다. 이후 추가한 기본 timeout fixture의 source review에서도 첫 completed request가 아직 image slot을 보유한 점과, admission 내부 시계보다 이른 값을 deadline 상한으로 쓴 점을 실행 전에 찾았다. first request를 명시 해제하고 성공 admission의 before/after 시계로 default5초를 검사하도록 수정한다. 이는 아직 실행된 test 실패로 기록하지 않는다.

## 병렬 전체 실행에서 찾은 취소 공개 순서

추가 회귀 뒤 lifecycle04는14통과·114 filtered였고 SHA-256은 `1362ddbdd6f91f3881b5fc2373822795dfba40e5cf89b61da623c3fdbf1216c1`이다. strict Clippy02도 통과했다(`e9fb3eace4dbb85cbf2c759b36b089890f98665f9d6420b98dc7a93c638bc586`). 기본 feature 전체 실행01도 통과했다(`fac38556811275ec8a1bfc030d23d9a3c1e3c34177c540ba605db8d7a8ec9190`). 하지만 all-feature 전체02는126통과·2실패였다. SHA-256 `64a635ce2bc2b562370fc3fb9fd40f17acbf6af4c7125eed21424fdfe16801e4`의 raw를 root가 직접 대조했다.

- full-ticket/Step fixture의 completion `unwrap`은 허용된 nonwaiting Busy 응답에 실패했다. 같은 payload/responder를 유지하는 bounded retry로 fixture를 수정한다.
- abandon fixture는 consumer가 `is_cancelled=true`를 본 뒤에도 responder가 `Ok(())`를 반환했다. failure 경로가 cancel flag와 terminal을 공개한 뒤 pending receiver Drop으로 채널을 닫아 짧은 순서 구멍이 있었다. 이는 assertion을 완화할 문제가 아니다. pending receiver를 먼저 닫고 cancellation/terminal을 공개하도록 failure·shutdown·Drop guard를 수정한다. 이미 완료한 responder의 AlreadyCompleted 의미는 보존한다.

집중/기본/strict 성공이 이 실제 병렬 실패를 면제하지 않는다. source 수정 뒤 집중 회귀와 최종 전체 검사를 다시 실행하며 a.1 인수는 아직 보류다.

## 수정 뒤 source freeze와 통합 검사

receiver close를 먼저 수행하는 수정 뒤 focused06은14통과(`fe7e6c3ca0927116330348b90cd5e810da9aae66c3ee1ddfd346c9bfb078d3ac`)였다. 기본 전체02는 새 normal-stop fixture의 ACK가 정상 Busy를 반환해115통과·1실패했다(`f5969d346a1a34ee6462fee1408607c43bb8626a2fb26a4c3535b27d9b6eab93`). Busy만 제한 재시도하는 ACK helper로 고쳤다. 이 실패를 취소 순서 결함이나 장치 실패로 섞지 않는다.

그 뒤 all-feature 전체03은128 unit·capture public integration2·기타 integration2를 통과했다(`94a5d5c58dc3feb7c4a9d4f372b7f1a78762f7f6638ecc49688e3f7b9264c4c5`). 기본 전체03은116 unit·capture public2·기타 integration2를 통과했다(`cfff361d36ba783f91970a8e983ff595511a861f680d03669497694a27728a55`). 둘 다0실패·0 ignored다. strict03도 통과했다(`4bdfe8a135ec0815cd003c2f0a53c772808fa5b7f80621cad7092a52189f1d79`).

마지막 fixture 변경은 CaptureCapacity 거부를 받은 같은 client의 다음 성공 ID가1인지 직접 검사하고 Step은 별도 client로 옮긴 것이다. production 수정 없이 source를 freeze한 focused07은14통과·0실패(`3b47570d9265c8124c63d77757c20b67002ec1b0d03e1f95ec863e4fe10c16a8`), strict04도 통과했다(`e75e6450e187f1c9b5d92dc85fc25513f5142df590f4cb576219804a2572eb09`). 이전 전체03과 마지막 assertion-only snapshot을 구분한다. root가 같은 shared target의 Cargo lease를 인수하고 전체 workspace 검사로 마지막 snapshot을 검증한다.

| Freeze source | SHA-256 |
| --- | --- |
| `runtime/src/session_capture.rs` | `479082e8520651418482910ec3bfe863283b8f2086df2fe7cd6cf7f7db0a611d` |
| `runtime/src/session_control.rs` | `1bff84cb51ebf56920ea86062739e537a895bb97186735502a264eaa2fec7ac0` |
| `runtime/src/project_preparation.rs` | `1d6edc4234967c74e49cdec1963db8db03cc4103004e159e880230dff3cf0472` |
| `runtime/src/lib.rs` | `6b4ecf597434ff19daa34a5a944f3c71318607d075ec02438e2552abe9a9f286` |
| `cli/src/project_view_control.rs` | `c023043d773fc432ab67d2f46dda0ca23bb4a9b5e84bd8ac5b57faa015065973` |
| `runtime/tests/capture_public_api.rs` | `07dd8885b791a939477cb5291cc12bd1e5782867d13205528e60f8424c65c510` |

Python discovery는72통과·0실패, raw `p02-capture-foundation-python-root-attempt01.log` SHA-256 `0351d79cccd2697e40027f565ae39da246b2e8f951d0480f556c8dcfe5b4ca33`이다. 독립적인 Python 검사와 Cargo compile/test를 병행했으며 compile/test lease는 한 owner가 순차 사용했다. read-only fmt는 동결 source만 확인했다. renderer/host source는 이 단계에서 수정하지 않았다.

## Root의 최종 로컬 통합 인수

위 source hash 그대로 root가 아래를 실행했다. 각 raw는 `dist/project-evidence/`에 있고 이전 실패를 보존한다.

| 실제 검사 | 결과 | Raw SHA-256 |
| --- | --- | --- |
| `workspace-test-root-attempt01` | `cargo test --workspace --all-features --locked`:414통과·0실패·장치34 ignored,47.703초 | `0018deab1ccf4ecf0aeb132b1d2dc52bc18bccc44cc69fc802b470f733528a5a` |
| `workspace-check-root-attempt01` | workspace/all-feature/all-target check 통과 | `9423b244b9006ad3021722b5bc8e5704ad052aa192033c63414b04d0c3c25d4a` |
| `workspace-clippy-root-attempt01` | workspace/all-target/all-feature strict 통과 | `e934f2c388c9b9b43b5f644f0a4d6d98f131675185ff2127b24807baaa36a938` |
| `workspace-fmt-root-attempt01` | `cargo fmt --all --check` 통과 | `3befcb2021e0952b71ee0152cd3b22a745b1a7d807bd1eb6def87fbd6e805668` |

Raw 이름은 전부 `p02-capture-foundation-` prefix와 `.log` suffix를 가진다.414에는 집중14·공개 SDK2·기존 전체 검사가 이미 포함되며 합산해 더 큰 총수로 보고하지 않는다. Linux CI·package·managed·실제 새 GPU capture는 이 로컬 실행으로 검증하지 않았다.

P02.3a.1의 runtime 기반만 인수한다. 기존 factory/default/public struct literal·원래 service/worker·Manual Step·normal ticket 수명을 유지하고 renderer/host dependency나 wire parser를 추가하지 않았다. backend Ready·PNG bytes/file/registry·live_capture=true는 없다. 다음은 [renderer 반환 계약](../03-architecture/live-capture-renderer-return.md)에 따른 a.2이며 상위 P02.3a·P03·D1/D2/D3는 열려 있다.
