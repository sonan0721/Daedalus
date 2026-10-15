# 독립 엔진의 기술 선택지

상태: 비교 초안. 어느 조합도 통합·성능 검증하지 않음 · 2026-10-15

## 선택 원칙

독립 엔진을 만든다는 사용자의 결정에 따라 엔진 소유의 데이터·스케줄·렌더·제어 계약을 유지한다. 재사용 부품을 충분히 활용하되, 다른 엔진의 App·World·RenderGraph·버전 전체에 의존하는 비용을 명시한다. Bevy 전체 포크는 기본 경로에서 제외하며, 전체 엔진의 구현은 참고 자료로 활용할 수 있다.

| 영역 | 후보 | 적합성 질문 | 채택 전 증거 |
| --- | --- | --- | --- |
| ECS | bevy_ecs / hecs 등 경량 ECS / 자체 구현 | 스케줄러 결합·변경 감지·직렬화·Wasm 의존성이 적절한가? | 소유권·교체 경계·실제 작업 패턴·안전성 검토 |
| CPU 병렬 | Rayon / Bevy task 계층 / 전용 executor | deadline·취소·공정성·재진입·웹을 어떻게 처리하는가? | CPU 경쟁·포화·종료·웹 초기화 실험 |
| 통신 | bounded Crossbeam / mailbox / 최신 상태 게시 구조 | 순서·드롭·메모리·수명 요구가 무엇인가? | 메시지 종류별 포화·회수 모델 |
| GPU | wgpu | 장치 feature·웹 소유권·셰이더·진단 경로는? | macOS+웹 최소 경로, device loss·readback |
| TS 실행 | rquickjs / deno_core / 브라우저 JS host | 모듈·디버그·npm·성능·일관성·빌드 크기는? | 동일 API conformance·스케줄·migration 비교 |
| TS 도구 | oxc + 별도 타입 검사 후보 | TS/TSX·source map·순환 모듈·의존 그래프 지원은? | 실제 모듈 집합의 cold/warm reload |
| 3D 물리 | Jolt 바인딩 / Rust 대안 | C++ FFI·Wasm·메모리·job adapter·라이선스 유지 비용은? | 결정적 경계·충돌 변경·타깃 빌드 |
| 2D 물리 | Rapier 2D / 타일 전용 충돌 / 조합 | 캐릭터·타일·고속 충돌을 누가 소유하는가? | 파괴 경계·일방 통행·연속 충돌 계약 |
| 유체·빛 | 엔진 소유 CPU 셀 시스템 / GPU 보조 | authoritative 상태와 시각 상태를 어떻게 구분하는가? | 보존·재현·경계·확장 시험 |
| UI | Taffy + 텍스트·입력·렌더 계층 | 한글·IME·게임패드·접근성·가상화까지 누가 구현하는가? | 레이아웃 외 기능별 비용·플랫폼 시험 |
| GPU 파티클 | 자체 wgpu 모듈 / Hanabi 통합·설계 참고 | Bevy renderer 종속 없이 필요한 기능을 수용할 수 있는가? | 의존성 그래프·큐/VRAM 예산·웹 compute |
| 애니메이션 | 자체 Rust 모듈 / ozz 바인딩 / sprite·Spine 어댑터 | import·blend·event·라이선스·웹 비용은? | 애니메이션 계약·자산 변환·타깃 빌드 |
| 네트워크 | 독립 transport + 엔진 replication / Lightyear 계층 활용 | Bevy 결합·Wasm 전송·대량 지형 변경에 적합한가? | 전송 API·관련 Cargo features·AOI 부하 |
| 캡처 | PNG 기준 + WebP·애니메이션 어댑터 | 정확성 비교·색공간·인코딩 비용·웹 경로는? | 정지·다중 프레임 분리 측정 |
| 오디오·내비 | 아직 미선정 | 웹·모바일·파괴 지형·실시간 제약을 어떻게 만족하는가? | 별도 조사 문서 필수 |

## 발견한 의존성 위험

확인 시점 Bevy pipelined API 문서는 0.20.0을 표시한다. 조회한 Hanabi 호환 표는 0.19↔Bevy 0.19를, Lightyear README는 0.30↔Bevy 0.19를 제시한다. 따라서 각 프로젝트의 `latest`를 독립 선택하여 하나의 검증된 조합이라고 적을 수 없다. [Bevy API](https://docs.rs/bevy/latest/bevy/render/pipelined_rendering/struct.PipelinedRenderingPlugin.html), [Hanabi](https://github.com/djeedai/bevy_hanabi), [Lightyear](https://github.com/cBournhonesque/lightyear)

이 관찰은 전체 통합의 불가능성 판정이 아니다. 특정 버전·feature를 고정해 조사해야 한다는 근거이다. 독립 엔진에서는 우선 ECS 부품의 범위를 정하고 렌더·네트워크 의존성을 분리해 비교한다.

## 의존성 채택 카드

채택하려는 부품마다 `목적 / 직접·전이 의존성 / 고정 version·commit / features / MSRV·툴체인 / native·Wasm·mobile / 스레드·allocator / 라이선스 / 유지보수 상태 / 대안 / 교체 비용 / 실패 실험 / ADR`을 남긴다.

R01에는 Cargo workspace를 만들지 않는다. 사용자의 플랫폼·표현·네트워크 규모 답변을 반영해 R03의 조사 범위를 확정한다.
