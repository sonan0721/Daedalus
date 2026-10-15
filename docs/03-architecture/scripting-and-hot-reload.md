# 스크립팅·엔진 API·핫리로드 계약

상태: C# 주 SDK 결정 반영, 상세 host·reload 구현은 초안 · 2026-10-15

## 책임 분리

주 언어는 C# 14/.NET 10 LTS/CoreCLR이다. AI와 게임 개발자가 규칙·AI·UI와 PC 플러그인을 작성하고 빠르게 수정할 수 있는 공식 SDK를 설계한다. Rust 게임플레이는 빌드형으로 등록하며 native hot-reload는 초기 범위에서 제외한다. [게임플레이·모드 SDK](gameplay-and-mod-sdk.md), [ADR-0003](../05-decisions/0003-csharp-and-native-gameplay.md)

Rust는 권한 있는 world state와 무거운 계산을 소유한다. script는 복사된 읽기 데이터·안전한 handle·버전 있는 command API를 사용한다. ECS 참조·GPU 포인터·임의 native FFI를 script 객체에 직접 노출하지 않는 안이다.

## 런타임 소유권

CLR host·모듈 수명은 지정 관리자가 소유하고 게임 시스템의 실행은 scheduler가 조정한다. C# 모든 코드를 단일 스레드로 직렬화해야 한다는 뜻은 아니다. 시스템 read/write·phase·상태 공유·협조 취소·허용 병렬성을 등록한다. Task.Run 등으로 scheduler 밖에서 world를 수정하거나 엔진이 빌린 메모리를 임의 worker에 넘기지 않는다.

C#은 host/runtime 기능과 협조 취소·신뢰 경계를 검토한다. AssemblyLoadContext는 보안 sandbox가 아니며 실행 중 코드·외부 참조가 남으면 unload되지 않을 수 있다. 긴 native 호출·협조하지 않는 CLR 코드를 deadline만으로 안전하게 강제 종료할 수 있다고 가정하지 않는다. host API도 제한된 작업량을 가져야 한다. 웹은 별도 실행 모델을 검증한다.

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
2. staging에서는 공식 SDK의 world 변경을 유보한다. 신뢰한 모듈도 file/network 등 되돌릴 수 없는 부작용을 만들지 않는 작성 계약을 지켜야 한다. 임의 .NET 코드의 외부 부작용까지 rollback할 수 있다는 보장은 아니다.
3. 동일 schema의 로직 교체와 상태 schema 변경을 구분한다.
4. 위치·체력 등 권한 상태는 Rust world에 남긴다. module 내부 상태는 명시적 직렬화 schema·migration 대상으로 삼는다.
5. safe tick 경계에서 새 generation을 적용한다. 객체 identity나 closure를 무조건 이어 붙이지 않는다.
6. 오류·시간 초과·migration 실패 시 기존 로직과 상태를 유지한다. 새 코드와 반쯤 변환된 상태를 섞지 않는다.
7. 이전 generation의 timer·event·promise·coroutine·callback 결과는 명시 취소 또는 거부한다.
8. 이전 runtime/module이 참조 때문에 회수되지 않는지 반복 reload로 확인한다.

C# Hot Reload의 지원 edit와 이 module 교체 방식은 같은 기능이 아니다. PC 모드 host는 CoreCLR이며 동적 assembly 로딩을 제한하는 Native AOT를 기본안으로 삼지 않는다. metadata 변경은 runtime 지원을 사전 확인하고 compiler가 지원하지 않는 편집은 적용 전에 거부한다. 프로세스 상태가 불명확해진 런타임 실패까지 항상 rollback할 수 있다고 보장하지 않는다. [Microsoft Hot Reload 도구](https://learn.microsoft.com/en-us/dotnet/core/tools/dotnet-watch), [MetadataUpdater](https://learn.microsoft.com/en-us/dotnet/api/system.reflection.metadata.metadataupdater.applyupdate?view=net-10.0)

MetadataUpdater 적용 시 이미 실행 중인 메서드는 이전 IL을 계속 실행할 수 있다. 세대가 섞이면 안 되는 변경은 시스템 진입을 잠시 유보하고 관련 작업이 안전 경계에 도달한 뒤 전환한다. 예산 내에 관련 작업을 정리하지 못하면 교체를 연기·거부하며 렌더 스레드가 이를 동기 대기하지 않게 한다.

오토파일럿 입력·조회·대기·캡처는 코드 교체 없이 같은 세션에서 수행한다. 새 게임 로직 주입만 이 reload 경로를 사용한다. Rust 게임플레이 수정은 재빌드·재시작한다. [오토파일럿 상세](autopilot-and-live-testing.md)

## 측정과 실패 시험

`저장 시각`, `감지`, `변환`, `타입 검사`, `평가`, `migration`, `commit`, `첫 영향 tick`, `첫 표시 frame`을 구분한다. 원안 5/50ms는 확정 기준이 아니다. fixture 크기·의존 모듈 수·cold/warm을 함께 기록한다.

문법 오류·API 불일치·순환 모듈·무한 루프·OOM·삭제 entity·재진입·지연 callback·동시 파일 변경·반복 migration·실행 중 shutdown을 시험한다. UI·렌더가 이전 상태를 표시하며 응답을 유지하는지 별도로 측정한다.

## 후속 결정

주 언어·PC 런타임 계열·Rust 반영 방식은 결정했다. 공식 API schema, PC host 진입점·패키징, web 실행, 디버거, 지원 edit·모듈 교체 범위, 권한·메모리·시간 한도, migration 정책의 세부 ADR이 필요하다.
