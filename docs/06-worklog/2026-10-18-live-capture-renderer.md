# P02.3a.2: actual surface capture renderer 작업 기록

상태: P02.3a.2 renderer 로컬 인수 통과 · host/CLI·최종 통합 인수는 열림 · 2026-10-18

## 만든 이유

[runtime a.1 기록](2026-10-18-live-capture-foundation.md)의 CPU metadata fixture와 실제 surface 픽셀 복사를 구분한다. `f3880d7`은 요청·credit·수명 기반만 인수했으며 main의 기존 CLI는 여전히 `live_capture=false`다. 이번 기록은 [renderer 반환 계약](../03-architecture/live-capture-renderer-return.md)의 source·실패·CPU 검사·실제 native proof를 별도로 추적하기 위한 것이다. 첫103 CPU·세 native scenario 뒤 resize/old-ready·실제 GPU failure 보완을 마쳤다. 최종104 CPU·private GPU2·기존 생성자 회귀GPU1·native5 scenario가 통과해 a.2를 로컬 인수했다. host/CLI·workspace/package/CI 최종 통합은 후속이다.

## Source 소유권과 순서

Luna D가 renderer의 새 `window/capture.rs`·기존 managed/window·packet renderer/fence/drain·root exports와 집중 시험을 소유한다. 필요한 main-thread native harness와 최소 example manifest는 먼저 공유한다. 새 runtime dependency·Engine·배포 tag·게임 프로젝트를 추가하지 않는다. root는 문서·리뷰·통합/GPU 실행·Git을 맡는다. host a.3 담당자는 현재 읽기 전용 설계 감사만 한다.

runtime/public test source는 freeze됐다. Cargo lease는 renderer 담당자에게 넘겼으며 source를 멈춘 snapshot에서 focused CPU 검사를 실행한다. 다른 담당자는 Cargo를 동시에 실행하지 않는다. 첫 checkpoint는 channel/readback/fence CPU, 다음은 actual window copy/pump와 native harness다. full workspace/실제 GPU 검사는 root source 리뷰 후 별도로 실행한다.

source가 커지는 단계에서 별도 Luna A가 새 `window/capture/ownership_tests.rs`만 작성하도록 분리했다. parent API/private phase와 module include는 D가 유지한다. A는 먼저 case를 공유하고 test file freeze/hash를 D에게 전달하며 Cargo·부모 source를 수정하지 않는다. D는 해당 test가 작성 중일 때 test/all-target compile을 하지 않는다. CPU metadata fixture는 실제 GPU buffer나 pixels의 증거가 아니다.

## 동결한 핵심 경계

- 요청/반환 lane과 단일 buffercell을 수락 전에 함께 예약한다. 성공한 renderer key는 재사용하지 않는다. 전송 거부는 원래 boxed request를 보존한다.
- source는 실제 host stage routing의 key/session/generation/digest다. 선택은 실제 drawn `ManagedPresentedView`·tick floor·크기·view와 일치해야 한다.
- 실제 COPY_SRC/format 확인 뒤 one-shot Ready를 게시한다. 지원하지 않으면 normal rendering은 유지한다.
- cell guard를 얻은 뒤 create/install까지 유지해 UI panic 때 local Buffer를 남기지 않는다. scene/overlay→같은 acquired texture copy→present 호출 뒤만 Submitted다.
- map/unmap·Buffer handle Drop과 실제 copy queue fence를 구분한다. normal window Poll은 UI, 반환 owner 뒤 Poll은 CPU owner가 맡는다. view·rowcopy·swizzle·unmap·final Buffer drop은 worker 경계다.
- Drop은 abandon 신호이며, outstanding handle이 남으면 drain Pending이다. 실패/닫힘도 같은 cell을 cleanup으로 반환한다.

## 아직 열려 있는 인수

| Gate | 상태 |
| --- | --- |
| CPU lane/소유권/거부/limit/alignment·map/fence/cancel/panic | 최종104 CPU·actual submit/map unwind·map-abort callback 회귀 통과 |
| 실제 surface capability·같은 acquired frame의 pixels/metadata | M3 Pro/Metal BGRA8 sRGB 640×480 실제 반복 capture 통과 |
| known color·RGBA/BGRA·manual-sRGB·overlay·resize·old-ready view | 네 texture format clear/readback·Metal BGRA8 sRGB surface·resize/old-ready 통과. PBR shader format parity 전체는 P05 후속 |
| close/worker drop·mapped view/queue fence·owner drain/final buffer drop | native held/drop Pending→Drained·private actual GPU unwind/abort cleanup 통과 |
| host artifact·PNG/file/reader·CLI protocol capture | 후속 a.3/a.4, 미구현 |

실제 실행 raw·source/binary hash·실패·장치 범위는 실행 뒤에만 추가한다. 별도 offscreen PNG를 이 gate의 증거로 쓰지 않는다. a.2·상위 P02.3a·D1/D2/D3는 미통과다.

## 첫 source review의 지적

아직 module 연결/집중 compile 전인 초안에서 root가 아래를 확인해 소유자에게 전달했다. 이는 실행된 테스트 실패가 아니다. 수정 뒤 source와 실제 회귀를 대조한다.

- readback이 Pending 관측 때 production phase를 QueuePending으로 바꾸고, 다음 호출은 Submitted만 허용해 정상 완료가 진행되지 않을 수 있었다. 진단 상태와 production phase를 구분한다.
- admission의 수동 cell 초기화와 별도 reserve helper가 중복되며 atomic key 게시가 빠졌다. detach/worker Drop/fallback이 같은 실제 key를 보도록 예약 책임을 한곳에 둔다.
- copy/buffer가 생기기 전의 취소·실패도 queue fence만 기다릴 수 있었다. GPU work가 없는 경우와 제출됐을 수 있는 경우를 구분하되 후자의 fence를 위조하지 않는다.
- immutable selected-view getter가 lock/Option clone으로 바뀌면 Busy와 metadata 부재를 혼동하고 cell 재사용 뒤 다른 frame을 읽을 수 있다. handle의 작은 immutable metadata snapshot으로 실제 선택을 보존한다.
- late worker 등록, return 소비 전 cell 재사용, QueuePending의 Released 오표시와 worker-detached/outstanding-handle fallback 조건을 보완한다.
- 성공 뒤 남은 예전 readback을 cleanup으로 바꾸거나 Drop할 때, 새 key의 live-handle/abandon flag를 덮어쓸 수 있는 경로를 찾았다. 완료된 handle의 cleanup은 무해하게 처리하고, 해제/취소 flag는 실제 key를 확인해 새 요청에 간섭하지 않게 한다. 해당 재사용 회귀도 별도 test 담당에게 전달했다.

후속 source는 private exhaustive CapturePhase enum, 최초 buffer 부재를 create 전에 검사하는 guard, immutable selected-view snapshot으로 수정 중이다. helper/source 변경은 실행된 회귀 통과와 별개이며 아직 이 항목을 인수하지 않는다.

host a.3의 읽기 전용 감사는 기존 CPU owner에 PNG/파일 I/O를 전부 넣으면 frame 준비·반환 GPU polling을 지연시킬 수 있다는 후속 검토를 진행한다. bounded encoder subjob과 durable request의 정확한 transfer/stop 순서는 source 착수 전에 별도 계약으로 확정하며, 아직 host 구현이나 성능 측정 결과가 아니다.

## 첫 집중 compiler 결과

actual copy/fence 구현을 더 연결하기 전에 source를 멈추고 `cargo check -p daedalus-renderer --lib --all-features --locked`를 실행했다. exit101·7.111초,8errors·2warnings였다. parent 밖으로 재노출할 내부 cell/guard/error의 visibility3곳, immutable view 변경 뒤 잘못된 dereference1곳, typed CellLockError의 WrongKey/InvalidState 누락4곳을 검출했다. 제거할 import와 아직 연결 중인 내부 alias 경고도 남았다. 이 결과를 테스트/GPU 성공으로 보고하지 않는다.

raw `dist/project-evidence/p02-capture-renderer-luna-attempt01.log` SHA-256은 `288b2982ec067d5ab63f210f09cf7292a2d8a9ac0652f1c3e6b37f98af1c5d64`다. 해당 기록을 보존하고 수정 뒤 새 attempt를 실행한다. 별도 test 파일 작성 중에는 lib-only check로 컴파일 범위를 한정했으며 test module 선언/컴파일은 test freeze 뒤에 진행한다.

두 번째 동일 lib-only check는 exit0·1.717초에 통과했다. raw `dist/project-evidence/p02-capture-renderer-luna-attempt02.log` SHA-256은 `0d3703f13809f00c6053ae9d4addb56c43e62607838c6d511655db9f45b74c18`이다. actual redraw/copy/submit/present·device drain 연결 전 snapshot이며 미사용 helper와 visibility 등을 포함한 warning13건이 남았다. strict check·CPU test·GPU 성공을 대신하지 않는다.

별도 ownership test 파일은 SHA-256 `fe39a2c278f1aace32f16e3857d4475d30578edb93ca7a9db43b3a5b936a14a1`로 freeze했다. Busy 시 같은 Box/key/source 반환, pre-copy 취소·실패의 return/cleanup 후 다음 key 수락, worker detach의 미소비 요청 정리, pending/map failure 상태, 이전 완료 handle과 새 key의 격리를 여섯 CPU test로 작성했다. 작성자는 scoped rustfmt·diff-check만 실행했다. production module include와 Cargo 실행은 renderer 담당자가 통합 뒤 맡으며, 테스트가 직접 설정한 metadata 상태는 실제 GPU 제출·완료·pixels의 증거가 아니다.

후속 읽기 전용 감사에서 per-request reserve가 영구 worker-detached flag를 false로 되돌리는 경합을 확인했다. submit의 worker-alive 관측 뒤 worker가 Drop되고 reserve가 실행되면 fallback이 영구 종료 신호를 놓칠 수 있었다. source는 영구 flag를 요청 초기화에서 제외하도록 수정했다. detached ingress의 실제 동작은 요청을 소비·거부하고 None을 반환하는 것이므로, test도 None→abandoned no-copy cleanup의 실제 Released→추가 요청 불가 순서로 맞췄다. 수정 test freeze SHA-256은 `364e76318e827fca20a315c9adf95867e67e047447abf3b471730f4194c7b059`이며 scoped rustfmt·diff-check만 통과했다. 아직 Cargo 회귀 결과는 없다.

native harness는 별도 Luna C가 새 `examples/managed_live_capture.rs`만 소유하도록 나눴다. D는 manifest와 renderer 연결·유일한 Cargo lease를 유지한다. harness는 main-thread 창과 공개 content/CPU owner/capture API를 사용해 반복 pixels·overlay·close-held·worker-drop을 검증하는 fixture이며 runtime authority 검증은 a.3/a.4에 남는다. C는 별도 Cargo/GPU를 실행하지 않고 파일 freeze를 전달한 뒤 D가 통합한다. 아직 harness source·native 실행 결과는 없다.

window 연결 초안의 후속 리뷰에서는 PacketRendererReady 전환 뒤 RendererReady 전용 accessor를 호출해 Ready를 게시하지 못하는 경로, pre-copy 취소/worker cleanup 뒤 재사용된 cell과 UI에 남은 이전 request의 경합을 확인했다. source는 실제 packet renderer의 장치 제한을 읽고, no-redraw pump에서 stale local key·취소·deadline을 처리하며 Busy는 보존하도록 수정 중이다. 별도로 frame interval이 길어도 pending map/cleanup과 deadline을 진행할 bounded capture wake가 필요한지 대조한다. 이들은 아직 실행된 회귀 결과가 아니다.

actual copy/present·반환 owner 연결 뒤 세 번째 lib-only check는 exit101·6.777초,3errors·1warning이었다. staging module의 StagingSubmission import 누락, callback helper visibility, UI guard보다 늦게 선언된 cell backing의 Drop 순서/lifetime을 검출했다. raw `dist/project-evidence/p02-capture-renderer-luna-attempt03.log` SHA-256은 `6e50424f76ea31e556174f0326abae69dd3ac077964fcf6e2e9c3ac041bf9ca6`다. source를 수정한 뒤 새 attempt로 재검사하며 실제 GPU 검증은 아직 하지 않았다.

## 첫 renderer CPU checkpoint와 후속 보완

수정 뒤 lib-only check attempt04는 exit0·3.151초,8warnings로 통과했다. 첫 lib test attempt05는98passed/0failed/29hardware ignored였다. 이후 strict attempt06은 private interface/미사용 항목·인자 수·조건/산술 진단11건, attempt07은 제거한 private Poisoned variant의 잔여 match2건으로 실패했다. 각각의 raw를 보존하고 원인 수정 뒤 아래 snapshot을 동결했다. 98에는 ownership6·source/readiness2·window stale-key/wake2가 포함되며 별도로 합산하지 않는다.

| 검사 | raw (`dist/project-evidence/`) | SHA-256 | 실제 결과 |
| --- | --- | --- | --- |
| all-features lib check | `p02-capture-renderer-luna-attempt04.log` | `e30d4ac4cf364bca4c86be31dfde06a5e5e4a8c4bb69f80caa056d7f43490101` | 통과, strict 전 snapshot |
| all-features strict lib Clippy | `p02-capture-renderer-luna-attempt08.log` | `174c529931dd06a9d1b002fff508e0728d5aa8c5d70d8340d74fb950b4a31bc1` | 통과 |
| all-features lib test | `p02-capture-renderer-luna-attempt09.log` | `c03460d90105a524497ef217c883c2f47c7fed69bea8e53a9c020bbeee6c160d` | 98통과/0failed/29hardware ignored |
| no-default-features lib check | `p02-capture-renderer-luna-attempt10.log` | `fce896d20943baf85d7d505f4cf34ce8f887371325375caea5d45af23e3882b8` | 통과 |

이 snapshot은 actual window/pixels·harness·workspace/package 인수가 아니다. 후속 host source 감사에서 present ACK metadata가 async capture 완료 전에 제거될 수 있음을 확인했다. [명세 보완](../03-architecture/live-capture-host-artifacts.md#실제-publication의-상관관계와-수명)은 actual selected frame을 renderer cell이 strong pin하고 readback은 Weak만 반환해 host가 실제 opaque publication을 read/cleanup 전에 고정하도록 한다. UI/handle Drop이 큰 frame backing을 해제하지 않으며 worker release만 Buffer와 pin을 함께 마감한다. D가 해당 source와 byte/layout·post-submit unwind CPU 회귀를 추가하며, 새 focused Cargo lease도 D만 보유한다. 이 변경은 위98건의 snapshot 뒤이므로 재검증 전이다.

harness의 실패 정책도 source 리뷰했다. report 전송 직후 thread 종료를 단 한 번 검사하면 scheduler 경합으로 실패할 수 있어 bounded 종료 확인 뒤 join하도록 했다. returned owner 전달 실패·GPU poll/drain deadline을 실제 정리로 바꿔서는 안 된다. 이 예제의 회복 불가능한 경우는 `cleanup_complete:false`·`unresolved_gpu_owner_retained:true`인 실패로 표시하고 process 수명까지 owner를 보존한다. UI에서 Drop하거나 Pending을 Drained로 바꾸지 않는다. 정상 인수는 actual Drained 뒤 worker final Drop만 허용하며, 이 fatal 예제 정책을 production host의 graceful cleanup 구현으로 계산하지 않는다.


## Frame pin 보완 뒤 CPU 검증과 example compile

production `try_take_return`에서 test build에만 frame 부재를 허용하던 분기는 제거했다. CPU test도 실제 content validation/CPU owner가 만든 frame을 cell에 설치한다. readback은 해당 frame의 Weak만 노출하고 buffer와 strong pin은 worker read/cleanup/fallback이 차례로 해제한다. 네 surface format의 padded-row RGBA/BGRA 변환·gamma 재적용 방지, 크기/정렬/device-buffer limit, 짧은 mapped range를 CPU bytes로 대조했다. unwind fixture의 map/fence 상태는 test가 설정한 metadata이며 실제 GPU panic의 증거가 아니다.

attempt11은 이미 필요한 lib check에 `--timings`만 더해 통과했다. raw `p02-capture-renderer-luna-attempt11.log` SHA-256 `2f4f6cf49e859034a53e82c7f9a35f7723dfd0950be422bce4c1e7be2a6776e9`, runner elapsed8.346초다. timestamped HTML `target/cargo-timings/cargo-timing-20261018T025659.424079Z.html` SHA-256 `abde26a2fb61deada3d8d3d72747316469be70c7094c88689c8685e3ae4d3608`은 전체8.2초·fresh dependency155/dirty renderer1·renderer check7.77초를 기록했다. warm dependency cache의 변경된 lib check 한 번이며 executable link·cold build·가속 개선율 측정은 아니다.

attempt12는 새 byte fixture의 height 타입 불일치 한 건으로 compile 실패했고, 수정 뒤 attempt13은103passed/0failed/29hardware ignored였다. attempt14 all-target strict는 example의 unused import·non-Clone client 사용·도달 불가능 event arm 세 건으로 실패했다. 해당 파일 소유자에게 수정한 뒤 attempt16/21은 남은 example Clippy 네 건(map identity·동일 if 분기·불필요 borrow·중첩 if)을 검출했다. attempt19 no-default strict는 feature-gated import 한 건을 검출해 cfg 경계를 수정했다. 모든 실패 원본은 다음 새 attempt와 별도로 보존한다.

| 최종 CPU 범위(실제 native 실행 전) | raw (`dist/project-evidence/`) | SHA-256 | 결과 |
| --- | --- | --- | --- |
| all-features lib test | `p02-capture-renderer-luna-attempt22.log` | `e90aeac1eaf2934fce437757b3b676432eb4b8857cedd42bbfdbe687839b8ffc` | 103통과/0failed/29hardware ignored |
| no-default lib test | `p02-capture-renderer-luna-attempt18.log` | `29b58447b142743ecb52e991c108558e173fd7f0cf4c1ccf15c6fabf6e4b148a` | 58통과/0failed/28hardware ignored |
| no-default all-target strict Clippy | `p02-capture-renderer-luna-attempt20.log` | `bd2c6d1f0c2f65157bc47e6694836f5388b49799783ef9a4e1ea74b7bb08c1e7` | 통과 |

두 lib 구성의 test는 겹치는 case를 포함하므로161개의 고유 시험으로 합산하지 않는다. C는 example strict 수정 뒤 SHA-256 `47212c90f93e8094e5c9f18a86cce9f80ae58a2d598561b229c190fedc80dc7b`로 다시 동결했다. 이 시점은 all-target strict/example build 재검사 대기이며 native/GPU 실행 전이다. 앞선 example hash를 다시 검사해 같은 지적을 반복한 왕복이 있어, 완료된 담당자를 명시적으로 재시작하고 **새 hash의 동결 통보 뒤** Cargo 소유자가 후속 검사를 실행하도록 조정했다. 시간 절감 수치는 측정하지 않았다.


새 example freeze 뒤 attempt23 all-target/all-features strict Clippy는 통과했다(raw SHA-256 `29bc13e1336484516b9ca97b15237a1242271acf6cc07e35eb8bd2b29c3d031b`). attempt24 explicit example check도 통과했다(`53d9436b56b0b24466797e8159e9b6cd751f032e1e3787524da1a93d9f7e565c`). raw 이름은 각각 `p02-capture-renderer-luna-attempt23.log`·`...attempt24.log`이며 scoped formatting/whitespace 검사를 마치고 Cargo lease를 root에게 반환했다. `cargo check`는 실행 파일 링크를 대신하지 않으므로 root는 별도 필요한 example build에 timings를 붙여 실행한다. 실제 native capture는 그 executable 빌드·source 리뷰 뒤에만 진행한다.


## 첫 실제 Metal surface checkpoint

root의 첫 executable build는6.081초에 통과했다. `--timings` HTML `target/cargo-timings/cargo-timing-20261018T030343.412062Z.html`(SHA-256 `d1c157fe5c0508f3fd96e48a2acf08923bb87f288f1e285a894d8958d2df5c2a`)은 renderer lib3.89초·example1.74초를 기록했다. library check와 다른 작업 범위이므로 앞선8.2초보다 컴파일이 개선됐다는 비교는 하지 않는다.

첫 native attempt는 창 생성 전에 fixture scene의 잘못된 `presentation` field를 거부했다. 이 설정은 `ProjectManifest` 소속이다. root schema 리뷰에서도 배치 오류를 놓쳤고, C가 manifest로 옮기고 나머지 field를 실제 schema와 대조했다. 실패 raw `p02-capture-renderer-native-capture-01.log` SHA-256은 `40adb3a041a5189c7e25dd095a44e05487f9ff1912e768eaa05b72b2649b09f7`이다. 첫 실패를 GPU 결과나 성공으로 바꾸지 않는다.

수정 example SHA-256은 `15ebd3c1585caeede90b40f169a05b23d78203a6ab502fb62b9fc463fda02a87`이다. 재빌드 `p02-capture-renderer-root-example-build-02.log`는3.271초/exit0·SHA-256 `7e9b0bf4d642af05b99cbcd9df94e8d558fffae3edf8e7b51dd0dcac6cfcff66`이며, 실제 세 scenario에 쓴 executable `target/debug/examples/managed_live_capture` SHA-256은 **`07a56e937e16b531b9dd34cfd3d9f6349f4f7bb8ab62b0c821dda422b48bd59b`**다. 마지막 example strict와 전체 fmt도 통과했다. raw는 각각 `p02-capture-renderer-root-example-strict-01.log`(`d49416dd2eb6a7f432e294ea8e93cbd90ef387f499ca397f087f26c7c7ded7d4`)·`p02-capture-renderer-root-fmt-01.log`(`797ce29e0dc3520b6d917de4e054f83cababad2b1248c7ee674212688e67ad90`)다.

같은 executable을 Mac main-thread native 창에서 순서대로 실행했다. 실제 adapter Apple M3 Pro·Metal·IntegratedGpu, negotiated surface `bgra8_unorm_srgb`, physical640×480이었다. OS 창을 외부 자동화로 읽거나 조작한 증거는 아니다. fixture의 authority는 `synthetic_snapshot_fixture`이며 actual runtime SessionControl/CLI capture와 구분한다.

| Scenario | raw (`dist/project-evidence/`) | raw SHA-256 | 관측 |
| --- | --- | --- | --- |
| capture | `p02-capture-renderer-native-capture-02.log` | `3f2cf08c8998e22fae744838f63220db52330b88df0f42b12a0a9171e8809b44` | 같은 source/tick/publication의 actual present1/21에서 overlay 포함/제거 PNG2개, known clear·mesh·decode 일치, 최종 Drained |
| close-held | `p02-capture-renderer-native-close-held-01.log` | `42213288fa07b0105794992ec71d9daad487298c7f5fc5b7dc3006b920f42664` | held readback 동안 drain Pending, 창 종료 후 pixels/PNG 완료, release 뒤 Drained |
| worker-drop | `p02-capture-renderer-native-worker-drop-01.log` | `4e9c773e57f125b588aed7021413d749ecb1c3aef4d20a1adf4ad13dda844f9a` | held handle 동안 Pending, 실제 worker endpoint→readback Drop 뒤 fallback Drained |

세 report 모두 success/cleanup_complete=true·unresolved_gpu_owner_retained=false였다. 각 raw와 같은 stem의 별도 directory에 `report.json`·PNG가 있다. root는 저장된 PNG를 Pillow로 다시 읽고 RGBA/PNG SHA-256·크기·세 sample을 report와 독립 대조했다. 이 대조의 최초 stdout은 tool 응답으로 관찰했으며 별도 raw 파일은 남기지 않았다. 실제 두 capture 파일도 `view_image`로 열어 빨간 triangle·첫 loading bar·두 번째 bar 부재를 확인했다.

| 저장 PNG | 파일 bytes | PNG SHA-256 |
| --- | --- | --- |
| `p02-capture-renderer-native-capture-02/capture-1.png` | 10,330 | `ce13f3d30f597ab981a67d1a18924d09f6867d750070f763f6a89ab118c38bf3` |
| `p02-capture-renderer-native-capture-02/capture-2.png` | 10,588 | `970b7946336d64186181f8ea6400e5489e2aea6d138a67e1844852a03ae24b61` |
| `p02-capture-renderer-native-close-held-01/capture-1.png` | 10,329 | `711e5779cffb26a54c17f478c081077e33ba6e9ce8ae81c7326e807a98be3ee8` |

두 capture의 RGBA SHA-256은 각각 `50c5c5836f91b55931821ad14ed8fc91252fa0357fee09326ac125b9080bbb6e`·`6c730e97c625fb6b9196065636581be1c7af09377182b2655695f6213fc8d91c`다. 두 번째 clear sample은48/56/69/255, 첫 overlay probe는109/207/244/255이며 두 번째 같은 위치는48/56/69/255였다. 같은 actual frame을 copy하고 BGRA를 RGBA로 바꾸되 gamma를 다시 적용하지 않는 이 장치의 경로를 확인했다. 다른 surface format·resize·forced old-ready·실제 GPU failure/panic은 이번 세 scenario에서 미실행이다. a.2 전체, host a.3/CLI a.4, P02와 D1/D2/D3는 계속 열려 있다.


## a.2 잔여 검증과 로컬 인수

D가 latest-only physical resize control·actual device limit·winit Some(actual)/Resized 공통 reconfigure와 poisoned slot 복구를 구현했다. C는 native resize·64개 unique geometry 후보의 old-ready 시나리오를 추가했다. old-ready는 capture 후와 같거나 더 최신 present sequence를 가진 하나의 coherent status에서 active/staging을 읽는다. 첫 overlay 전환을 검사하는 validator와 같은 frame identity validator를 분리해 다른 시나리오가 불필요한 overlay 차이에 의존하지 않게 했다.

private GPU test는 production copy/readback/fence 경로와 실제 buffer/map callback을 사용한다. 첫 submit 직후 callback 등록 전, map 등록 직후의 의도된 panic을 catch하고 cell/pin이 남는지 검사한다. 특히 첫 cleanup은 fence 등록/관측 전 false여야 한다. 이후 nonwaiting Poll·유한1ms test-worker backoff로 실제 fence/map을 마감한다. pending map의 unmap 취소는 실제 wgpu error callback을 발생시키며 synthetic flag를 성공 근거로 쓰지 않는다. 네 texture format은 실제 rendered clear/manual-sRGB clear·채널 순서·padding 제거를 검사한다. 이 fixture는 mesh가 없는 clear pass이므로 PBR mesh shader parity를 증명하지 않는다.

compiler attempt26은 test fixture의 빠진 mutable binding1건으로 실패했다. 수정 뒤 test-target compile27/29, strict28/30/31을 거쳐 아래 최종 snapshot을 검증했다. source를 바꾼 뒤의 focused 재검사이며 실패 원본을 보존했다. 최종 source/config/lock·binary·attempt25~35 raw hash는 ignored `p02-capture-renderer-root-gpu-freeze-02.json`에 동결했다. inventory SHA-256은 `11f601d11ad75c016e5f2b38ba0ccf4fa6de5cc3a1dd720f647b68edb250b480`, example source는 `25dd6e8aa5e1c6b587ac806d0b09c13aae82b7ae7259b610be106723763f1955`, actual executable은 **`3aab00347cb1865b99b47e6c195046083b1fcc00978630c50b22762f98756d70`**이다.

| 최종 source 검사 | raw (`dist/project-evidence/`) | SHA-256 | 결과 |
| --- | --- | --- | --- |
| renderer all-feature lib test | `p02-capture-renderer-luna-attempt32.log` | `c54e077de95e14931a2b50fb19784b058238c45f9da596633b6245651965a8fe` | 104passed/0failed/31hardware ignored |
| 실제 dev-build example | `p02-capture-renderer-luna-attempt33.log` | `38e54ed14cc0acbd1f482c44c771f2b3e02e68df94de9158671482bcfd47b6ec` | 통과,5.258초. alias의 첫 통합 build이며 범위가 다른 과거 build와 속도 비교하지 않음 |
| all-target/all-feature strict | `p02-capture-renderer-luna-attempt34.log` | `1ff32d8efc0d83279f1443ba37c89d4ec8f1376c3f8c8ae169ec980a12b887cc` | 통과 |
| all-target/no-default strict | `p02-capture-renderer-luna-attempt35.log` | `69ecb54e37caf2c424740aca9d484354a8e58b4a0ffd9f6a018078ed18aab3a7` | 통과 |
| root full-workspace fmt | `p02-capture-renderer-root-fmt-02.log` | `6eebc123677b84ec958334e9b9d19398b69651dc64e19fffa2b2528ceaa07aa5` | 통과 |
| root private adapter GPU | `p02-capture-renderer-native-gpu-01.log` | `27d83e634309ed7b57e1a363d496a0c8ff264e408156898751d95faf71de2529` | 2passed/0failed, 네 format과 실제 submit/map/abort 경로. 의도한 panic trace2개는 catch된 test 입력 |
| 기존 default offscreen ctor/alpha GPU | `p02-capture-renderer-default-constructor-gpu-01.log` | `5c747699659c163877f427e53798185c95ab69eaff95e61534d68a4c431a3a78` | 정확한 기존 test1passed, public ctor의 adapter 공통화 회귀 |

같은 dev-build executable의 native 검증은 Apple M3 Pro·Metal·BGRA8UnormSrgb surface에서 순서대로 실행했다.

| Native scenario | raw (`dist/project-evidence/`) | SHA-256 | 실제 관측 |
| --- | --- | --- | --- |
| resize | `p02-capture-renderer-native-resize-01.log` | `93a2b4bf248700e5128c02378431d63b34f9b6ba9fad24544e6c85ac94029075` | actual640×480→513×257, actual aspect projection 변경, tightly packed RGBA/PNG 일치 |
| old-ready | `p02-capture-renderer-native-old-ready-01.log` | `b497310ebfcf55bdaafc615584776653d5ae258b0b6ef403ce29ef89be92e9b0` | 후보 key2 Uploading의 create calls2→48, initial generation1 capture present22, coherent afterstatus present43, 후보 미commit·취소·drain |
| capture regression | `p02-capture-renderer-native-capture-03.log` | `91fda43cc4084f6e4ec10249bc9525e08189f796207a3ef1763c4afd95e09c79` | 첫 overlay/후속 기본 화면 pixels·identity·decode·drain |
| close-held regression | `p02-capture-renderer-native-close-held-02.log` | `e4fb86fe92b62a78e732ffa326317e6852230c33a4bcb22b74fbb2d8b56175cf` | held readback 동안 Pending, close 뒤 readback 해제 후 Drained |
| worker-drop regression | `p02-capture-renderer-native-worker-drop-02.log` | `25b893a5e922293f995d75aadde6f543cd2a7ed1351683b694d77d18aa37f08d` | actual endpoint/handle Drop 뒤 fallback Drained |

native 다섯 report는 success/cleanup_complete=true·unresolved owner=false다. old-ready 시점의 uploaded_bytes는0이며 실제 GPU resource create 단계의 Uploading 관측이다. 이미 texture/geometry bytes가 제출된 시점이라고 바꾸지 않는다. resize의 copy row pitch2304/tight2052/padding252는 wgpu alignment에 따라 계산한 값이며 API가 raw mapped pitch를 별도로 보고한 값은 아니다. 실제513×257 tightly packed pixels와 saved PNG가 일치함은 독립 확인했다.

root는 saved PNG7개를 디스크에서 다시 decode해 PNG/RGBA hash·크기·세 sample을 report와 대조했다. raw `p02-capture-renderer-native-disk-verify-02.log` SHA-256은 `84fa02483d29201347ef1d2949db8beb42de295bccc052765e1acdd9d45ec772`다. resize·old-ready 최종 이미지도 직접 열어 봤다. resize 최종 PNG `dd11e5322d78c5a5b34ad3d66d46b2b9aad2a005b2a9aaa4dd28fa8f3c4988c1`(5,380bytes), old-ready 최종 PNG `970b7946336d64186181f8ea6400e5489e2aea6d138a67e1844852a03ae24b61`이다.

위 범위로 **a.2 renderer 로컬 인수**를 통과한다. 네 format은 adapter texture proof이고 negotiated native surface는 위 format 하나다. 실제 runtime authority·host artifact/encoder·CLI wire capture는 a.3/a.4이며 아직 미구현이다. 다른 OS/browser·전체 PBR shader parity·OS scanout·144fps·최종 workspace/package/CI 인수까지 완료한 것으로 확장하지 않는다. P02.3a·P02와 D1/D2/D3는 계속 미통과다.
