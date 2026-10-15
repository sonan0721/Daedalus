# 다이달로스 문서 지도

작성일: 2026-10-15 · 라운드: R01~R02 / E01 이후 · 단계: 설계와 모듈별 구현·검증

## 문서 읽는 순서

`비전 → 질문과 답변 → 조사 → 아키텍처 → 목표와 게이트 → 결정 기록`

이 문서들은 엔진 구현을 지시하기 위한 기반이다. R01은 조사 첫 라운드이며 설계 완료를 뜻하지 않는다. 사용자 답변에 따라 문서를 갱신하고, 아직 실행하지 않은 실험은 계획으로 남긴다.

## 상태와 근거 표기

| 표기 | 의미 |
| --- | --- |
| 사용자 확정 | 대화에서 사용자가 직접 정한 요구 |
| 조사 확인 | 연결된 1차 자료의 해당 내용까지 확인 |
| 설계 제안 | 다이달로스에 적용하기 위한 작성자의 제안 |
| 미결 | 답변·비교·실험이 더 필요 |
| 실측 | 환경·입력·명령·결과가 있는 실행 근거. 초기 CPU·Metal 검증은 작업 기록 참조 |

문서에 수치가 있어도 `제안`이면 계약이나 달성 결과가 아니다. 아키텍처 문서는 사용자 확정 요구를 반영한 초안이다. 기본 읽기 단위는 개별 파일이며 하나의 거대한 명세서에 모든 판단을 숨기지 않는다.

## 파일별 목적

| 파일 | 만든 이유와 핵심 내용 |
| --- | --- |
| [프로젝트 헌장](00-vision/project-charter.md) | 이름·정체성·사용자가 확정한 방향과 현재 작업 경계를 고정 |
| [출발 제안 보존](00-vision/seed-proposal.md) | 첫 제안의 의도·스택·6단계 계획을 요약 보존하고 수정 지점 추적 |
| [요구사항](00-vision/requirements.md) | 요구 ID를 부여해 설계·목표·테스트까지 연결 |
| [기능·품질 범위표](00-vision/feature-quality-matrix.md) | 렌더링 이외의 UI·액션·오디오·도구·배포 누락 방지 |
| [질문지](01-discovery/questionnaire.md) | 선택마다 어떤 아키텍처가 달라지는지 함께 질문 |
| [답변 기록](01-discovery/answer-log.md) | 실제 답변과 작성자의 제안을 구분해 저장 |
| [출처 등록부](02-research/source-register.md) | 공식 문서·원저자 자료의 검증 범위와 한계를 기록 |
| [원안 주장 검증](02-research/claim-audit.md) | 144Hz 보장·전체 ECS 복사·핫리로드 수치 등의 오해를 정리 |
| [스택 선택지](02-research/stack-options.md) | 독립 엔진 관점에서 부품별 이점·결합 비용·실험 기준 비교 |
| [스크립트 언어 비교](02-research/scripting-language-comparison.md) | C#·TS·Luau의 작성 경험·실행·reload·플랫폼 비용과 실제 사례 비교 |
| [Unity Lua·오토파일럿 조사](02-research/unity-lua-and-autopilot.md) | 과거 Lua 도입 경험을 플레이 제어·코드 교체·런타임 제약으로 나누어 검토 |
| [상용 엔진에서 배울 점](02-research/commercial-engine-lessons.md) | 공개 발표에서 확인한 원칙과 추정을 분리 |
| [조사 백로그](02-research/research-backlog.md) | 다음 조사 질문·산출물·종료 조건 지정 |
| [시스템 경계](03-architecture/system-context.md) | 엔진 모듈·의존성·공통 데이터 계약 설계 |
| [스케줄러·소유권](03-architecture/scheduler-and-ownership.md) | 스레드 역할·스냅샷 수명·과부하·종료 계약 설계 |
| [플랫폼·웹](03-architecture/platform-and-web.md) | macOS·브라우저 우선 설계와 후속 포팅 경계 |
| [스크립트·핫리로드](03-architecture/scripting-and-hot-reload.md) | Rust/C# 경계·상태 보존·실패 복구·타이밍 정의 |
| [게임플레이·모드 SDK](03-architecture/gameplay-and-mod-sdk.md) | C# wrapping·Rust 게임 작성·C++ FFI·PC 플러그인의 소유권과 검증 계약 |
| [Managed host·native bridge](03-architecture/managed-host-and-native-bridge.md) | 실제 PC CoreCLR 연결을 위한 진입점·ABI·모듈 교체·통합 시험 계약 |
| [오토파일럿·실행 중 테스트](03-architecture/autopilot-and-live-testing.md) | 플레이 조작·관찰·검증과 새 C# 로직 주입을 각각 명세 |
| [렌더·헤드리스](03-architecture/rendering-and-headless.md) | PBR/스타일라이즈드 렌더 확장과 비동기 캡처 |
| [복셀 스트리밍](03-architecture/voxel-streaming.md) | 청크 생성·메시·조명·충돌·저장 버전 정합성 |
| [2D 타일 시뮬레이션](03-architecture/tile-simulation.md) | 유체·조명·파괴 지형의 병렬 계산과 보존 규칙 |
| [에셋·UI·애니메이션](03-architecture/assets-ui-animation.md) | DCC부터 쿠킹·입력·글꼴·액션·오디오까지 지원 경계 |
| [물리·액션·오디오](03-architecture/physics-action-audio.md) | 캐릭터·판정·포즈·이벤트·오디오·경로의 엔진 계약 |
| [물리·오디오 backend](03-architecture/physics-and-audio-runtime.md) | 실제 Jolt/Rapier·audio 통합의 소유권·제한·최소 실행 검증 계약 |
| [네트워크·영속화](03-architecture/networking-and-persistence.md) | 서버 권한·관심 영역·월드 변경·복구 설계 |
| [CLI·MCP·관측](03-architecture/cli-mcp-observability.md) | AI가 관찰·제어·검증하는 구조화된 엔진 인터페이스 |
| [실행·제어·표시 통합](03-architecture/runtime-control-and-presentation.md) | 지속 세션·CPU 예산·CLI 캡처·독립 창 실행의 다음 통합 계약 |
| [릴리스·공개 API](03-architecture/releases-and-public-api.md) | 엔진·API·저장·네트워크 버전과 HTML/Wasm export의 경계 |
| [목표 지도](04-plan/goal-map.md) | 조사와 구현을 각각 끝낼 수 있는 goal로 분해 |
| [워터폴 마스터플랜](04-plan/waterfall-masterplan.md) | 단계별 산출물·게이트·되돌림 규칙 정의 |
| [엔진 구현 체크리스트](04-plan/implementation-checklist.md) | 착수 승인 이후 모듈별 구현·실제 검증·남은 품질 게이트 추적 |
| [검증·성능 기준](04-plan/validation-and-performance.md) | 정확성·지연·화질·플랫폼·서버 테스트 설계 |
| [위험 등록부](04-plan/risk-register.md) | 의존성·웹·스코프·수명·품질 위험과 대응 조건 기록 |
| [GitHub 운영·검증](04-plan/github-workflow.md) | 실제 적용한 브랜치 규칙·PR·CI·1인 유지보수 절차 기록 |
| [ADR 기록](05-decisions/README.md) | 확정 결정과 제안의 변경 이력 관리 |
| [ADR-0001](05-decisions/0001-engine-identity-and-scope.md) | 독립 엔진·엔진 전용 저장소·구현 전 문서화 확정 |
| [ADR-0002](05-decisions/0002-runtime-boundaries.md) | Rust/wgpu 기준선·실행 격리·명령·자원 예산의 방향 채택 |
| [ADR-0003](05-decisions/0003-csharp-and-native-gameplay.md) | C# 14/.NET 10/CoreCLR·빌드형 Rust 게임플레이·C++ 통합·오토파일럿 결정 |
| [ADR-0004](05-decisions/0004-start-engine-implementation.md) | 사용자 구현 착수 승인과 모듈별 설계·검증 진행 방식 |
| [R01 작업 기록](06-worklog/2026-10-15-r01.md) | 작성 문서별 목적·확인 범위·검증·후속 과제 기록 |
| [R02 스크립팅 작업 기록](06-worklog/2026-10-15-r02-scripting.md) | 언어·모드 범위 변경, 새 문서별 목적과 남은 통합 조사 기록 |
| [E01~E03 첫 구현 검증](06-worklog/2026-10-15-e01-foundation.md) | 실제 Rust·CLI·Metal 검증과 아직 완료하지 않은 기능의 경계 |
| [문서 템플릿](07-templates/specification-templates.md) | 조사·명세·goal·ADR의 재사용 형식 |

공개 기여·PR·현재 실행 가능한 체크는 루트의 [기여 규칙](../CONTRIBUTING.md), [테스트 안내](../TESTING.md), [운영 원칙](../GOVERNANCE.md)에 둔다.

## 관리 원칙

새 조사 문서는 조사 질문, 확인 사실, 다이달로스 적용안, 반례·비용, 미결 사항, 출처를 포함한다. 새 설계 문서는 소유자, 입력·출력, 실패 처리, 플랫폼 제약, 검증 기준을 포함한다. 문서를 추가하면 이 지도와 작업 기록을 함께 갱신하고 사용자에게 파일별 이유와 핵심 내용을 설명한다.
