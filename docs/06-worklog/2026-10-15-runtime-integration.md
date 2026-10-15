# 첫 실행 통합 checkpoint

날짜: 2026-10-15 · 구현/추가 코드 리뷰 Luna xhigh · 통합/명세/독립 검증 조정자

## 합쳐진 경로

10개 Rust crate의 권한 ECS·parallel schedule·유한 CPU task·소유 snapshot·persistent CLI·콘텐츠 데이터·wgpu capture·C# bridge·물리/audio adapter를 같은 workspace에서 빌드했다. adapter 라이브러리와 자동 ECS physics/audio phase 연결은 구분한다.

조정자가 실제 CLI `serve`에 spawn·step·snapshot·exit를 pipe로 전달했다. request ID와 session이 유지됐고 spawn한 Rust entity가 같은 handle/위치로 tick1 snapshot에 들어갔다. `capture` feature CLI는 Apple M3 Pro/Metal에서320×180 PNG를 만들고 adapter·fixture·RGBA8 byte 수230,400을 JSON으로 보고했다.

`version --json`은 실행 가능한 CLI와 renderer library compile 상태를 구분한다. `window`는 아직 false다. scene validate/patch는 content schema를 사용하지만 scene→ECS/renderer instantiation은 후속이다.

## 추가 리뷰와 수정

1. caller/자동 command ID namespace를 분리했다. pending/completed 자동 ID를 외부 ID로 제출해도 queue가 바뀌지 않는 CLI/실제 C ABI 회귀 검사를 추가했다.
2. task 설정 실패가 global ECS pool을 먼저 확정하지 않도록 fallible 준비를 앞에 뒀다. fresh-process test가 실패 후 정상 budget 생성까지 확인한다.
3. snapshot 포화/Busy/초과 시 lazy extractor를 실행하지 않는다. default100k render-eligible entity 상한과 steady-state payload byte 추정치를 두고 이전 snapshot을 유지한다. Transform 없는 ECS entity는 해당 상한에 포함하지 않는다. 추출은 try-lock 구간에서 수행하고 consumer는 기다리는 대신 Busy를 받을 수 있다. byte 추정치는 transient Vec→Arc 복사·allocator·header를 포함한 메모리 실측이 아니다.
4. CLI run의 entity 인수와 command queue 상한을 맞췄다. 기본 queue256을 넘는257개 entity의 실제 명령 적용을 검사했다.

## 조정자 최종 실행

```sh
cargo test --workspace --all-features --locked
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo build -p daedalus-bridge --locked
```

CPU80건 통과, hardware2건 기본 ignored, fmt·Clippy 통과. 별도로 실제 GPU capture와 CPAL 장치 probe가 성공했다. 최신 bridge로 C# self-test와2개 integration scenario도 다시 실행해 성공했다. .NET solution Release build는 경고0/오류0이었다. 세부 범위는 [C# 기록](2026-10-15-e04-managed.md)·[물리/audio 기록](2026-10-15-e06-physics-audio.md)에 있다.

## 열린 게이트

이것은 엔진 기반 통합이다. interactive window 실제 검증·runtime streaming·general rendering·UI/IME·animation/cutscene·navmesh·30-player network·내구성 저장·MCP·web export·전체 managed SDK·성능/장시간 검증은 완료되지 않았다. 80개 정확성 검사는144fps 또는 상용 참조 게임 품질의 증명이 아니다. 로컬 CLI package와 tag workflow는 별도 후속 commit·실행 근거로 관리한다.
