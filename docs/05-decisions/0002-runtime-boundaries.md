# ADR-0002: 실행·데이터·플랫폼 경계 기준선

상태: 방향 채택, 구현 선택·성능 검증 전 · 근거: 요청 원안과 REQ-001·003~007·013~020 · 2026-10-15

## 결정

1. 요청 원안의 Rust 코어·wgpu GPU 추상화를 설계 기준선으로 유지한다. 외부 부품 버전·바인딩은 따로 고정한다.
2. simulation의 권한 상태와 presentation·렌더·로딩 UI 상태를 분리한다. 실제 스레드·worker 배치는 플랫폼별이다.
3. 렌더에는 필요한 읽기 전용 데이터만 게시한다. 전체 ECS 이중 복제를 기본 설계로 삼지 않는다.
4. script·net·CLI/MCP는 검증된 명령과 세대 있는 handle을 사용한다. 외부가 live world를 임의 수정하지 않는다.
5. 작업·큐·snapshot·자산·GPU upload·저장 대기에 유한 예산과 포화·취소·종료 의미를 둔다.
6. 서버 권한·월드 저장·복제 의미를 초기 설계에 포함한다. 렌더 없는 dedicated server와 GPU offscreen을 구분한다.
7. 웹·설치형의 기능·저장 의미·script API를 유지하며 기기별 화질 profile은 허용한다.
8. 우선 공식 script API를 설계하고 npm·불신 모드·임의 native extension을 기본 기능으로 가정하지 않는다.

## 제외한 대안과 이유

단일 live ECS의 전역 lock을 렌더·script·네트워크가 공유하는 구조는 긴 임계구역과 불분명한 수명 위험이 크다. 무제한 queue는 압력을 메모리와 지연으로 옮긴다. 모든 플랫폼의 물리적 스레드 배치를 동일하게 강제하면 웹·OS 제약과 충돌한다. 이 대안들은 기본 경로로 택하지 않는다.

## 아직 결정하지 않은 것

script 언어·VM, ECS 제품, executor·queue·snapshot 알고리즘, tick rate, 메모리·업로드 수치, 렌더 path, protocol·save schema. 이 ADR은 이들 세부 선택을 대신하지 않는다.

## 검증과 변경 조건

수명·포화·늦은 결과·느린 simulation·플랫폼 conformance·30명 부하로 검증한다. 동일 요구를 더 단순하게 충족하는 근거 또는 실제 플랫폼 제약이 나오면 재검토한다. [실행 설계](../03-architecture/scheduler-and-ownership.md), [플랫폼](../03-architecture/platform-and-web.md), [검증 계획](../04-plan/validation-and-performance.md).
