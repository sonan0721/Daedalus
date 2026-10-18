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
| [실제 에셋 검증 출처](02-research/asset-validation-provenance.md) | 공개 원본의 revision·license·hash·외부 변환과 미지원 입력을 고정 |
| [원안 주장 검증](02-research/claim-audit.md) | 144Hz 보장·전체 ECS 복사·핫리로드 수치 등의 오해를 정리 |
| [스택 선택지](02-research/stack-options.md) | 독립 엔진 관점에서 부품별 이점·결합 비용·실험 기준 비교 |
| [구현 의존성 기준선](02-research/implementation-dependency-baseline.md) | 실제 선택 버전·native 빌드·라이선스·타깃 검증 경계 |
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
| [에셋 import·cook·resource](03-architecture/asset-import-cook-and-residency.md) | 공식 포맷 조사와 모델/PNG/sprite/WAV·증분 cache·generation·장치 자원 수명의 상세 계약 |
| [공통 에셋 import 구현](03-architecture/asset-import-foundation.md) | 네 CPU 포맷·불변 IR·native/byte resolver·유한 cache·CLI의 실제 구현/검증 계약 |
| [에셋 실제 실행 소비](03-architecture/asset-runtime-consumers.md) | 다음 model/texture·sprite·audio 연결의 공통 packet·authority·자원 공유·검증 초안 |
| [스프라이트·정사영 계약](03-architecture/sprite-runtime-and-orthographic-view.md) | P03.2b의 실제 atlas/alpha·공통 unit quad·authority camera·실제 표시 view와 반복 검증 경계 |
| [Imported audio 실행 계약](03-architecture/imported-audio-runtime.md) | P03.2c의 공유 Frame bank·공통 actor/replay·장치 없는 PCM와 실제 native 출력·voice 수명 계약 |
| [Native 오디오 진행 진단](03-architecture/native-audio-progress-diagnostics.md) | 실제 finite voice 종료 실패를 position/state·stream 오류로 좁히고 전체 회귀 반복을 줄이는 opt-in 진단 계약 |
| [비동기 에셋 후보 준비](03-architecture/project-asset-preparation.md) | P03.3a의 동결한 단일 cache worker·최신 후보/취소·private payload·공통 제어·논리적 메모리 한도 계약 |
| [에셋 세대 전환·회수](03-architecture/project-asset-activation.md) | P03.3b/c의 live state 보존·consumer staging·actor commit·실제 표시·old audio/GPU 회수를 동결하기 위한 상세 초안 |
| [Renderer CPU generation](03-architecture/render-cpu-generation.md) | P03.3b.3.2a의 managed CPU owner·실제 leaf/별칭·frame 예약·검증·독립 논리 예산을 GPU 코드 전에 동결 |
| [Renderer device generation](03-architecture/render-device-generation.md) | P03.3b.3.2b의 same-device stage+draw·partial completeness·upload/use serial·pin union·drain을 동결 |
| [에셋 교체 authored 호환성](03-architecture/asset-replacement-compatibility.md) | P03.3b.1의 단일 content 비교기·허용 field/revision/order·유한 오류/임시 index를 먼저 동결해 consumer별 중복 판정을 방지 |
| [Headless CPU 세대 commit](03-architecture/headless-asset-generation.md) | P03.3b.2의 동일 authority·Stage/Commit/replay·초기 source consume·worker 회수·frame identity를 장치 연결 전에 검증 |
| [Render 세대 staged 준비·표시](03-architecture/staged-render-asset-generation.md) | P03.3b.3의 제한된 GPU 업로드·last-good pin·native lease/CPU 회수·실제 present ACK를 동결하기 위한 source 기반 상세 초안 |
| [Native project host](03-architecture/native-project-host.md) | typed 실행·consumer port·publication·CPU backing/종료를 CLI 밖의 공개 엔진 조합 경계로 만드는 구체 설계안 |
| [Native host 실제 통합 기록](06-worklog/2026-10-18-native-host-integration.md) | 혼합 모델·스프라이트의 실제 CLI 창/세대 교체에서 발견한 결함과 수정·인수 범위를 추적 |
| [Live capture runtime 작업 기록](06-worklog/2026-10-18-live-capture-foundation.md) | 다음 capture ticket·독립 image credit·capability/worker 수명과 public SDK/CPU 검증을 실제 backend와 구분해 추적 |
| [Live capture renderer 반환](03-architecture/live-capture-renderer-return.md) | UI panic 전후의 buffer 반환 슬롯·실제 queue fence·worker final drop을 source 구현 전에 고정 |
| [Live capture renderer 작업 기록](06-worklog/2026-10-18-live-capture-renderer.md) | CPU handoff 검증과 실제 surface 픽셀·닫힘/자원 회수 증거를 분리해 추적 |
| [Live capture host·artifact](03-architecture/live-capture-host-artifacts.md) | durable ingress·단일 encoder/I/O subworker·PNG/spool·ACK 뒤 native reader 수명을 source 전에 설계 |
| [Live capture CLI·protocol](03-architecture/live-capture-cli-protocol.md) | 기존 protocol 2 ticket과 실제 host capability의 PNG path·ACK·blocked stdout 수명을 연결하는 후속 계약 |
| [Native renderer owner 수명](03-architecture/native-renderer-owner-lifecycle.md) | device/packet 부분 초기화·paired create panic·managed 창 반환·worker final drop의 구현 전 소유권 계약 |
| [Managed packet 창](03-architecture/managed-packet-window.md) | resource-only 초기 loading·유한 stage/frame 전달·OOB 취소·실제 present ACK·오류에도 owner를 반환하는 기존 창 확장 계약 |
| [CLI view activation](03-architecture/native-project-view-activation.md) | host를 유일한 실행 owner로 소비하는 CLI 경계·flag·report v2·실제 표시/종료 관측과 기존 경로 호환 계약 |
| [Viewer 입력 정책](03-architecture/native-view-input-policy.md) | native 기본 조작을 유지하면서 protocol 자동 검증의 keyboard/camera 간섭을 분리하는 additive API 계약 |
| [Project generation consumer](03-architecture/project-generation-consumer.md) | renderer 의존 없는 source lease·Queued/Applied·실제 publication token·present ACK/회수의 첫 runtime 구현 계약 |
| [모델·텍스처 실행 계약](03-architecture/model-texture-runtime.md) | P03.2a의 실제 project binding·공통 창/capture·cache pin 해제·GPU 한도/색/수명·회귀 기준 |
| [물리·액션·오디오](03-architecture/physics-action-audio.md) | 캐릭터·판정·포즈·이벤트·오디오·경로의 엔진 계약 |
| [물리·오디오 backend](03-architecture/physics-and-audio-runtime.md) | 실제 Jolt/Rapier·audio 통합의 소유권·제한·최소 실행 검증 계약 |
| [네트워크·영속화](03-architecture/networking-and-persistence.md) | 서버 권한·관심 영역·월드 변경·복구 설계 |
| [CLI·MCP·관측](03-architecture/cli-mcp-observability.md) | AI가 관찰·제어·검증하는 구조화된 엔진 인터페이스 |
| [실행·제어·표시 통합](03-architecture/runtime-control-and-presentation.md) | 지속 세션·CPU 예산·CLI 캡처·독립 창 실행의 다음 통합 계약 |
| [Native live session](03-architecture/native-live-session.md) | 실제 창·권한 ECS·background terrain 연결과 입력/반응성 증거 계약 |
| [외부 project lifecycle](03-architecture/project-lifecycle.md) | 사용자 project/scene/mesh 자료를 같은 Engine·snapshot·renderer에 연결하는 선행 계약 |
| [같은 session의 공통 제어](03-architecture/session-control-service.md) | 하나의 project actor에 창/CLI/MCP/SDK를 연결하는 결과·clock·입력·capture·종료 계약 |
| [Legacy 결과 수명 공통화](03-architecture/legacy-session-result-lifetimes.md) | 진단용 실행의 수동 credit 반환을 공통 lease로 바꾸고 panic 중 unknown outcome을 명시하는 계약 |
| [Project 실행 창·protocol 2](03-architecture/project-view-control.md) | source 공유·실제 tick 0·동일 session JSONL·유한 I/O와 종료를 연결하는 CLI 계약 |
| [Live capture·artifact 수명](03-architecture/live-capture-and-artifacts.md) | 같은 실제 presentation의 비동기 readback·공통 ticket·유한 이미지/파일 소유권을 정의하는 다음 계약 |
| [Live capture 소유권](03-architecture/live-capture-ownership.md) | ACK·timeout·GPU·encoder·파일·native reader 수명을 독립 credit과 cycle 없는 capability/backstop으로 고정 |
| [렌더 자원 재사용](03-architecture/renderer-resource-reuse.md) | 반복 geometry 복사/업로드를 mesh handle·instance 갱신·in-flight cache 예산으로 줄이는 다음 설계 |
| [릴리스·공개 API](03-architecture/releases-and-public-api.md) | 엔진·API·저장·네트워크 버전과 HTML/Wasm export의 경계 |
| [CLI 빌드·태그 릴리스](03-architecture/cli-build-and-release.md) | 로컬 실행 파일 관리와 후속 tag push 기반 GitHub Release 자동화 계약 |
| [목표 지도](04-plan/goal-map.md) | 조사와 구현을 각각 끝낼 수 있는 goal로 분해 |
| [워터폴 마스터플랜](04-plan/waterfall-masterplan.md) | 단계별 산출물·게이트·되돌림 규칙 정의 |
| [제품 납품·최종 인수](04-plan/product-delivery-and-acceptance.md) | 엔진→실제 3D 게임→실제 2D 게임의 상위 목표와 P01~P16 완료 조건 |
| [개발 반복 파이프라인](04-plan/development-iteration-pipeline.md) | 반복되는 제작·binding·asset·AI 검증·배포 작업의 공통화·자동화와 효과 검증 |
| [엔진 구현 체크리스트](04-plan/implementation-checklist.md) | 착수 승인 이후 모듈별 구현·실제 검증·남은 품질 게이트 추적 |
| [검증·성능 기준](04-plan/validation-and-performance.md) | 정확성·지연·화질·플랫폼·서버 테스트 설계 |
| [위험 등록부](04-plan/risk-register.md) | 의존성·웹·스코프·수명·품질 위험과 대응 조건 기록 |
| [GitHub 운영·검증](04-plan/github-workflow.md) | 실제 적용한 브랜치 규칙·PR·CI·1인 유지보수 절차 기록 |
| [ADR 기록](05-decisions/README.md) | 확정 결정과 제안의 변경 이력 관리 |
| [ADR-0001](05-decisions/0001-engine-identity-and-scope.md) | 독립 엔진·엔진 전용 저장소·구현 전 문서화 확정 |
| [ADR-0002](05-decisions/0002-runtime-boundaries.md) | Rust/wgpu 기준선·실행 격리·명령·자원 예산의 방향 채택 |
| [ADR-0003](05-decisions/0003-csharp-and-native-gameplay.md) | C# 14/.NET 10/CoreCLR·빌드형 Rust 게임플레이·C++ 통합·오토파일럿 결정 |
| [ADR-0004](05-decisions/0004-start-engine-implementation.md) | 사용자 구현 착수 승인과 모듈별 설계·검증 진행 방식 |
| [ADR-0005](05-decisions/0005-engine-product-and-game-validation.md) | 엔진 제품 인수·별도 3D/2D 게임 검증·완료 보고 기준 정정 |
| [ADR-0006](05-decisions/0006-owned-session-control-results.md) | 공통 actor의 owned ticket·유한 replay/retention·Manual Step 포화 진행 계약 |
| [ADR-0007](05-decisions/0007-reusable-native-host-and-generation-consumer.md) | CLI 밖의 재사용 host·runtime/renderer 의존 분리·CPU/device 소유권과 유한 세대 보고 결정 |
| [R01 작업 기록](06-worklog/2026-10-15-r01.md) | 작성 문서별 목적·확인 범위·검증·후속 과제 기록 |
| [R02 스크립팅 작업 기록](06-worklog/2026-10-15-r02-scripting.md) | 언어·모드 범위 변경, 새 문서별 목적과 남은 통합 조사 기록 |
| [E01~E03 첫 구현 검증](06-worklog/2026-10-15-e01-foundation.md) | 실제 Rust·CLI·Metal 검증과 아직 완료하지 않은 기능의 경계 |
| [E04 C# 연결 검증](06-worklog/2026-10-15-e04-managed.md) | 실제 Rust state·명령 ACK·모듈 교체·실패 복구·context 회수 근거 |
| [E06 물리·오디오 검증](06-worklog/2026-10-15-e06-physics-audio.md) | Jolt/Rapier 실제 충돌과 Kira PCM·장치 probe의 실행 근거 |
| [첫 실행 통합 checkpoint](06-worklog/2026-10-15-runtime-integration.md) | 10개 crate·CPU80건·C# 실제 연결·CLI 제어·추가 리뷰 수정 근거 |
| [E08 CLI 패키징 검증](06-worklog/2026-10-16-cli-packaging.md) | 로컬 native 실행 파일·의존성 notice·archive 검증과 후속 tag 배포의 구분 |
| [Native live session 검증](06-worklog/2026-10-16-native-live-session.md) | 실제 창·ECS·background terrain 통합과 입력·종료·파이프라인 검증 근거 |
| [엔진 제품 준비도 감사](06-worklog/2026-10-16-product-readiness-audit.md) | 기반 부품과 실제 제작 기능의 차이·미완료·첫 의존성 기록 |
| [공통 에셋 import 작업 기록](06-worklog/2026-10-16-asset-import.md) | P03.1의 API 분담·실제 공개 자료 확인·검증 상태를 추적 |
| [모델·텍스처 실행 작업 기록](06-worklog/2026-10-16-model-texture-runtime.md) | P03.2a의 사전 계약·소유권·공통화·로컬/package/PR11 인수 근거를 추적 |
| [스프라이트 실행 작업 기록](06-worklog/2026-10-16-sprite-runtime.md) | P03.2b의 source/API 감사·반복 제거 선택·Luna 분담과 후속 실측을 구분 |
| [Imported audio 작업 기록](06-worklog/2026-10-16-imported-audio-runtime.md) | P03.2c의 source/API 선택과 이후 PCM·native·패키지 실측을 구분 |
| [비동기 후보 준비 작업 기록](06-worklog/2026-10-16-project-asset-preparation.md) | P03.3a의 memory 장부·단일 worker/cache 소유권·API 동결·실제 후속 검증을 구분 |
| [에셋 세대 전환 작업 기록](06-worklog/2026-10-16-project-asset-activation.md) | P03.3a 인수 이후 b/c source/API 리뷰·Luna 분담·실제 후속 검증과 pipeline 개선 추적 |
| [Native host 설계 작업 기록](06-worklog/2026-10-17-native-host-design.md) | CLI 밖 실행 재사용 설계·Rust probe·native 실패/회복과 test wait 수정의 실제 근거 |
| [Render generation 작업 기록](06-worklog/2026-10-17-render-generation.md) | b.2 최종 인수·새 runtime consumer 계약/코드 소유권과 후속 실제 renderer/host 검증을 추적 |
| [문서 템플릿](07-templates/specification-templates.md) | 조사·명세·goal·ADR의 재사용 형식 |

공개 기여·PR·현재 실행 가능한 체크는 루트의 [기여 규칙](../CONTRIBUTING.md), [테스트 안내](../TESTING.md), [운영 원칙](../GOVERNANCE.md)에 둔다.

## 관리 원칙

새 조사 문서는 조사 질문, 확인 사실, 다이달로스 적용안, 반례·비용, 미결 사항, 출처를 포함한다. 새 설계 문서는 소유자, 입력·출력, 실패 처리, 플랫폼 제약, 검증 기준을 포함한다. 문서를 추가하면 이 지도와 작업 기록을 함께 갱신하고 사용자에게 파일별 이유와 핵심 내용을 설명한다.
