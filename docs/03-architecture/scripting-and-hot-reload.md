# 스크립팅·엔진 API·핫리로드 계약

상태: 언어 중립 설계 제안. C#·TS·Luau 미선정 · 2026-10-15

## 책임 분리

사용자가 스크립트에 기대하는 것은 AI가 규칙·AI·UI를 작성하고 빠르게 수정할 수 있는 공식 API다. 외부 패키지·불신 모드는 후속 확장으로 검토한다. TSX도 언어 선택 전에 필수로 확정하지 않는다. [언어 비교](../02-research/scripting-language-comparison.md)를 선행한다.

Rust는 권한 있는 world state와 무거운 계산을 소유한다. script는 복사된 읽기 데이터·안전한 handle·버전 있는 command API를 사용한다. ECS 참조·GPU 포인터·임의 native FFI를 script 객체에 직접 노출하지 않는 안이다.

## 런타임 소유권

한 VM/runtime 실행 영역은 지정 소유자가 관리한다. script task가 Rayon worker 사이에서 임의의 VM 객체를 공유하지 않는다. script tick의 의존성과 결과 적용 마감을 scheduler에 선언한다. 독립 VM 병렬화는 데이터 분할·통신 비용과 API 의미가 검증된 뒤 고려한다.

네이티브 JS·Luau는 실행 중단 hook·allocator 경계를 검토한다. C#은 host/runtime 기능과 협조 취소·격리 경계를 검토한다. browser JS worker 종료는 VM 재생성과 상태 복원을 수반하므로 native interrupt와 같은 동작으로 취급하지 않는다. 긴 native host 호출은 VM interrupt만으로 중단되지 않을 수 있어 host API도 제한된 작업량을 가져야 한다.

## 공식 API의 형태

| 분류 | 계약 |
| --- | --- |
| 읽기 | tick·query schema·immutable batch. script가 조회 시점을 알 수 있음 |
| 쓰기 | command ID·권한·handle generation·대상 tick·예상 revision·결과 |
| 시간·난수 | engine clock·seeded source. 재현 모드에서 host 전역 시간과 구분 |
| 자원 | asset handle·비동기 상태·취소·오류. file/network 직접 접근과 분리 |
| 이벤트 | 구독 token·module generation·순서·해제 규칙 |
| UI | view description·로컬 상태·입력 command. 권한 world와 분리 |
| 오류 | script/module·행·열·source map·phase·원인·복구 결과 |

배치 query·command를 우선해 객체 하나마다 언어 경계를 넘는 비용을 제한한다. API schema에서 타입 선언·문서·검증기를 생성할 수 있는 구조를 검토하되 코드 생성 도구를 지금 구현하지 않는다.

## 트랜잭션 방식 reload

`변경 감지 → 의존 그래프 계산 → compile/transform → 타입·API 검증 → staging 평가 → migration 준비 → 안전 경계 commit → 이전 세대 정리`

1. 연속 저장을 세대·content hash로 묶고 오래된 컴파일 결과를 무시한다.
2. staging에서 world·file·network에 되돌릴 수 없는 부작용을 발생시키지 않게 한다.
3. 동일 schema의 로직 교체와 상태 schema 변경을 구분한다.
4. 위치·체력 등 권한 상태는 Rust world에 남긴다. module 내부 상태는 명시적 직렬화 schema·migration 대상으로 삼는다.
5. safe tick 경계에서 새 generation을 적용한다. 객체 identity나 closure를 무조건 이어 붙이지 않는다.
6. 오류·시간 초과·migration 실패 시 기존 로직과 상태를 유지한다. 새 코드와 반쯤 변환된 상태를 섞지 않는다.
7. 이전 generation의 timer·event·promise·coroutine·callback 결과는 명시 취소 또는 거부한다.
8. 이전 runtime/module이 참조 때문에 회수되지 않는지 반복 reload로 확인한다.

C# Hot Reload의 지원 edit와 이 module 교체 방식은 같은 기능이 아니다. native 개발 runtime과 AOT/Web 배포 runtime의 허용 변경을 각각 명세한다. [Microsoft Hot Reload 도구](https://learn.microsoft.com/en-us/dotnet/core/tools/dotnet-watch)

## 측정과 실패 시험

`저장 시각`, `감지`, `변환`, `타입 검사`, `평가`, `migration`, `commit`, `첫 영향 tick`, `첫 표시 frame`을 구분한다. 원안 5/50ms는 확정 기준이 아니다. fixture 크기·의존 모듈 수·cold/warm을 함께 기록한다.

문법 오류·API 불일치·순환 모듈·무한 루프·OOM·삭제 entity·재진입·지연 callback·동시 파일 변경·반복 migration·실행 중 shutdown을 시험한다. UI·렌더가 이전 상태를 표시하며 응답을 유지하는지 별도로 측정한다.

## 후속 결정

공식 API schema, 최종 언어, native/web host, 패키지 범위, 디버거, 개발·배포 reload 수준, 권한·메모리·시간 한도, 스키마 migration 정책의 ADR이 필요하다.
