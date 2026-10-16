# 아키텍처 결정 기록

2026-10-15 · ADR = Architecture Decision Record

## 상태

- `제안`: 비교·토론 중. 구현 기준선으로 확정하지 않음.
- `채택`: 사용자 요구 또는 명시한 기술 판단으로 현재 기준선에 반영.
- `기각`: 채택하지 않은 이유를 보존.
- `대체`: 후속 ADR이 기존 결정을 변경. 과거 기록은 삭제하지 않음.

| ADR | 상태 | 내용 |
| --- | --- | --- |
| [0001](0001-engine-identity-and-scope.md) | 채택 | 독립 엔진·엔진 전용 저장소·문서 우선·공개 및 코딩 위임 |
| [0002](0002-runtime-boundaries.md) | 채택: 방향 | Rust/wgpu 기준선·분리된 실행·명령·유한 자원·플랫폼 의미 |
| [0003](0003-csharp-and-native-gameplay.md) | 채택 | C# 14/.NET 10/CoreCLR 주 SDK·빌드형 Rust 게임플레이·C++ 어댑터·오토파일럿 |
| [0004](0004-start-engine-implementation.md) | 채택 | 구현 착수 승인·모듈별 계약·Luna xhigh 코딩·실제 검증 추적 |
| [0005](0005-engine-product-and-game-validation.md) | 채택 | D1 엔진 제품→D2 3D 게임→D3 2D 게임 인수·엔진/게임 저장소 분리·반복 개발 자동화 |

스크립트 언어는 사용자와 최종 검토 후 C#으로 선택했다. 초기 TypeScript strict 권고는 ADR-0003으로 대체한다. Rust 코어·C++ 부품 통합과 Rust 게임 작성 경로를 유지하되 Rust는 재빌드, C#은 hot-reload로 반영한다. host 진입점·패키징·ECS·큐 알고리즘 등의 세부 구현은 아직 미선정이다.

## 기록 형식

각 ADR은 날짜·상태·결정 근거·대안·결정·결과·검증·재검토 조건·연결 요구를 포함한다. 라이브러리는 정확한 버전·feature·타깃·자료와 실험 범위를 기록한다. 설계 방향 채택과 성능 달성은 다른 상태다.
