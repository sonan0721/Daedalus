# P02.3a.3: host capture·encoder·artifact 수명 작업 기록

상태: source 착수 · 실제 host PNG/reader/shutdown 인수 전 · 2026-10-18

## 만든 이유

[renderer a.2](2026-10-18-live-capture-renderer.md#a2-잔여-검증과-로컬-인수)의 actual surface 픽셀과 [runtime a.1](2026-10-18-live-capture-foundation.md)의 ticket/credit을 실제 reusable host에서 연결한다. renderer 단독 fixture를 CLI/authority 연결 성공으로 바꾸지 않고 PNG·파일·ACK 뒤 reader·종료를 각각 추적하기 위한 기록이다. [host 계약](../03-architecture/live-capture-host-artifacts.md)의 header·수명·실패 순서가 선행한다.

## 소유권과 구현 순서

| 담당 | Source | 책임 |
| --- | --- | --- |
| Luna A | 새 host capture module·집중 CPU test·lib export·host Cargo feature | 한 encoder/I/O worker·유한 ingress/fence·publication/artifact registry·reader public API, 정확한 header 선행 공유 |
| Luna C | 기존 host.rs | opt-in options/factory·actual generation/stage routing·run_view channel·frame 송신 전 registry 등록·coordinator take/transfer/stop |
| Luna D | 기존 cpu_worker.rs 및 필요시 별도 capture-progress module | catch 바깥 durable pending·actual readback/Weak lookup·Vec move/encoder/result/completion·GPU owner/stop 진행 |
| root | 설계·리뷰·통합 검증·Git | header 합의·source/Cargo freeze·actual host proof·원본/hash·제한 기록 |

A가 public/private DTO·constructor·error variants·getter를 먼저 동결하고 C/D가 그 실제 header를 사용한다. 같은 file을 여러 담당이 수정하지 않는다. Cargo.lock은 A만 소유하며 pinned graph의 png0.18.1/sha2 0.10.9를 재사용한다. compiler lease는 A 한 명이며 다른 담당 source가 해당 crate의 compiler 입력을 바꿀 때는 기다린다. renderer source는 a.2 commit084bb83으로 동결했다. a.3에서 CLI protocol/source나 외부 게임을 만들지 않는다.

API02 합의 원본은 ignored `dist/project-evidence/p02-capture-host-api-02.md`, SHA-256 `df1addfb3f068bc75e526c752e4f81c615391ea2ee0082a179c995579d87c82c`다. 앞선 API01도 덮어쓰지 않았다. review에서 실제 session binding·CPU ingress-drain/renderer client·encoder와 renderer 상태 전달·실패 health/실제 종료 구분을 보완한 뒤 세 담당자의 source 착수를 확정했다. 이 artifact는 통신 중 가정을 맞추기 위한 header이며 실행 증거가 아니다.

source 리뷰 뒤 private ingress는 borrowed pending-slot transaction으로 바뀌었고, CPU는 keyed encoder queued/active/result query로 중단된 submit을 대조한다. API02는 초기 합의 이력이며 최종 source의 hash·검증 범위는 동결 후 별도로 기록한다. 공개 capture API는 이 private 소유권 보완으로 바뀌지 않는다.

## 동결한 핵심 경계

- capture opt-in default off, 기존 service/actor/consumer를 확장한다. actual renderer와 encoder bootstrap이 Ready인 뒤만 runtime Ready다.
- capture 모드는 host start에서 channel/worker를 CPU spawn 전에 준비하고 window client/server만 `run_view`까지 보관한다. 채널 생성은 장치 Ready가 아니며, 창을 열지 않은 종료도 검증한다. capture-off의 기존 channel 생성 시점은 유지한다.
- runtime request는 take 직후 durable coordinator slot에 설치하고 실제 stage key와 함께 같은 Box로 ingress를 통과한다. fence는 take한 요청의 transfer/확정 처분 뒤만 닫는다.
- renderer key는 channel 전체의 checked 단조 counter로 할당한다. client별 runtime sequence와 혼용하지 않고 Busy 때 같은 key를 유지한다.
- frame 송신 전 actual opaque publication/key/Weak binding을 예약한다. CPU owner는 selected Weak를 대조하고 실제 token을 고정한 뒤 pixels를 읽는다.
- CPU loop는 readback/fence·준비/GPU poll을 계속 진행하고 RGBA Vec를 단일 encoder로 move한다. PNG encode·파일 작업을 CPU/window callback에 넣지 않는다.
- 독립 work lease와 partial path/cleanup 상태는 job catch 바깥에 둔다. partial 실패 정리는 interest가 살아 있어도 진행하고, 완결 file은 마지막 interest 뒤 실제 삭제해야 credit을 반환한다.
- public native reader는 strong capability+immutable binding을 유지한다. registry는 weak interest+work만 유지하며 reader-held ACK/shutdown을 명시한다.

## 인수 상태

초기 source 리뷰에서 CPU의 try API 뒤에 연결된 blocking metrics lock, 삭제할 entry를 IO 전에 local로 꺼내며 생긴 unwind 수명 공백, encoder 종료만으로 capture Stopped를 게시하는 조건을 발견했다. 각각 encoder 쪽 계측/CPU try 경계, 삭제 성공까지 durable ledger 유지, CPU의 전체 capture-drained 신호로 보완하도록 요청했다. renderer/encoder submit 직전 panic과 수락 뒤 panic도 실제 keyed state로 구분해야 하므로 nonwaiting reconciliation과 주입 회귀를 추가한다. 아직 수정 후 실행을 통과했다는 기록은 아니다.

PNG 경로는 pinned `png0.18.1/src/encoder.rs`를 직접 대조했다. `write_image_data`는 압축된 image 전체를 Vec로 만든 뒤 writer에 전달하므로 capped writer만 두어도 그 임시 할당은 남는다. [공식 streaming API](https://docs.rs/png/0.18.1/png/struct.Writer.html#method.stream_writer_with_size)로 고정64KiB chunk를 사용하도록 수정 중이다. 실제 쓰인 bytes의 증분 hash·상한과 partial 정리는 유지한다. 확인한 registry source SHA-256은 `787f6f8922588a74436c76a3b2c2b5689206f7ed13a19a36227b8809710812ce`이며 access2026-10-18이다. 이는 source 기반 메모리 경로 보완이고 RSS/시간 감소율은 미측정이다.

| Gate | 상태 |
| --- | --- |
| public/private header·feature-off 경계 | source/API 동결 진행 |
| ingress take/transfer panic·fence/stop race | 미검증 |
| real-publication registry·Busy/pin/capacity | 미검증 |
| streaming PNG cap·hash·partial/실패 정리·encode-once | 미검증 |
| artifact capability 인증·ACK/reader·파일 삭제/credit·shutdown | 미검증 |
| 실제 host same-surface PNG·실제 authority/identity 대조 | 미실행 |
| CLI/protocol·workspace/managed/Python·두 package/다섯 CI | 후속 a.4, 미실행 |

컴파일/CPU/GPU의 실제 command·실패·raw hash는 실행 뒤 기록한다. source/빈 interface·synthetic metadata는 실제 host 성공의 증거가 아니다. a.3/a.4·P02 및 D1/D2/D3는 미통과다.
