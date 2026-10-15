# E01~E03 첫 구현 검증

날짜: 2026-10-15 · 담당: Luna xhigh 3개 작업, 조정자 설계·리뷰·독립 검증

## 구현과 확인

코어·tasks·world·runtime·CLI·renderer·content의 7개 crate를 구축했다. Rust 1.94.0, Bevy ECS 0.18.1, Rayon 1.12.0, wgpu 27.0.1, winit 0.30.12를 고정하고 Cargo.lock을 생성했다. 최신 버전이라는 이유 대신 설치 toolchain과 실제 빌드·API 경계에 맞춰 선택했다.

조정자는 workspace all-features 테스트 50건, fmt, Clippy를 실행해 통과를 확인했다. 별도 GPU 테스트 1건도 실제 M3 Pro Metal에서 통과했고 513×289 PNG를 직접 확인했다. CLI inspect의 실제 JSON entity/session/generation·transform·revision을 확인했다.

리뷰에서 snapshot 임시 Arc의 보관량 계산, task 결과 수집의 blocking lock, 자기 worker shutdown 대기, revision 중복 증가, clock 최대 tick overflow, 복셀 이웃 면 소유권·출력 예산 등의 문제를 지적했고 Luna가 수정·회귀 시험을 추가했다. 조정자는 소스·테스트·CI 코드를 직접 작성하지 않았다.

## 현재 제한

- 초기 ECS schedule은 단일 스레드이고 Rayon은 설정 가능한 유한 배경 작업이다. 예산 있는 병렬 ECS와 전체 실행 루프 연결은 후속 작업이다.
- renderer의 실제 검증은 Metal offscreen이다. window는 컴파일·입력 큐 시험까지이며 수동 상호작용은 아직 확인하지 않았다.
- 재질·depth·light·tone mapping 기본 경로이며 shadow/texture/IBL/temporal/VFX·최적화는 미구현이다.
- 유체는 닫힌 region의 순차 보존 solver, 빛은 bounded full recompute, terrain은 작은 heightfield, 경로는 occupancy BFS다. 전역 병렬 fluid·navmesh 완성으로 부르지 않는다.
- C# bridge·physics/audio는 다음 병렬 작업에서 구현 중이다. 웹·30명·144fps·장시간 품질 검증은 미완료다.

## 새 문서와 후속

[구현 체크리스트](../04-plan/implementation-checklist.md)는 모듈별 진행과 종료 근거를 추적한다. [ADR-0004](../05-decisions/0004-start-engine-implementation.md)는 사용자의 착수 승인과 미완료 gate 유지 원칙을 기록한다. [Managed host 계약](../03-architecture/managed-host-and-native-bridge.md)은 C# 연결을, [물리·오디오 계약](../03-architecture/physics-and-audio-runtime.md)은 실제 backend 통합 검증을 정의한다. 이 작업 기록은 첫 실행 근거와 제한을 보존한다.

후속은 CLI/renderer/content 연결·지속 세션 제어·예산 있는 병렬 ECS, 실제 CoreCLR/native 모듈 교체, Jolt/Rapier·audio 통합이다. 샘플 게임은 추가하지 않는다.
