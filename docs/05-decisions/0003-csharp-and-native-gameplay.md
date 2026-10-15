# ADR-0003: C# 주 SDK와 Rust 게임플레이·C++ 통합 경계

날짜: 2026-10-15 · 상태: 채택 — 언어·역할·반영 방식. host/ABI 구현·성능은 미검증

근거: A-026~034, REQ-017·020~025. ADR-0002의 script 미선정 상태와 확장 범위는 이 결정으로 구체화한다.

## 맥락과 사용자 결정

사용자는 구조화된 언어 체계·게임 개발자 친숙성·성능·확장형 모드를 중시하며 엔진에 V8·Chromium을 넣기를 원하지 않는다. 모드는 Steamworks PC 수준을 먼저 고려하고, 웹·모바일 모드 방식은 현재 선택을 제약하지 않아도 된다고 했다. 전체 엔진의 웹·추가 플랫폼 목표를 취소한 것은 아니다.

사용자는 C# 방향에 동의한 뒤 Unity 6.3에서 Lua를 사용했던 오토파일럿 경험을 재검토했다. 플레이 조작·검증과 새 로직 주입 모두를 원하며 C#으로 진행하는 데 동의했다. Rust 게임플레이 창구도 원하되 안정성을 위해 Rust는 빌드 반영, C#은 hot-reload로 시작하기로 선택했다.

## 결정

1. 공식 주 게임플레이·PC 모드 SDK는 **C# 14 + .NET 10 LTS + CoreCLR** 기준으로 설계한다.
2. Rust를 엔진 코어 기준으로 유지하고 C++ 라이브러리는 명시적 FFI 어댑터로 연결할 수 있게 한다. 코어를 두 언어로 중복 구현하지 않는다.
3. Rust 게임플레이 crate의 빌드 시 등록 경로를 제공한다. 초기 반영 방식은 재빌드·재시작이다. Rust/C++ native hot-reload는 초기 계약에 포함하지 않는다.
4. 일반 작성 API와 성능용 batch API는 하나의 world·schema·command·scheduler 의미를 사용한다. C# raw 객체나 외부 코드에 live ECS/GPU 수명을 직접 넘기지 않는다.
5. PC 다운로드형 managed 플러그인 경로를 설계한다. Native AOT를 해당 동적 모드 host의 기본 배포 방식으로 삼지 않는다.
6. 오토파일럿 제어 API와 C# 로직 reload를 모두 제공하도록 설계한다. 테스트 절차 전송마다 게임 assembly 교체를 요구하지 않는다.
7. Lua/Luau·TS를 추가 제품 언어로 함께 유지하지 않는다. 과거 TS 권고는 현재 기준선에서 대체한다. V8·Chromium은 포함하지 않는다.
8. 웹·모바일·콘솔의 C# 실행·배포 방식은 후속 플랫폼 명세로 다룬다. PC와 같은 동적 모드 로딩까지 공통 계약으로 약속하지 않는다.

## 언어·플랫폼·런타임 버전의 구분

| 이름 | 의미 | 이번 기준선 |
| --- | --- | --- |
| C# | 문법·타입 체계·언어 버전 | 14 |
| .NET | SDK·라이브러리·실행 플랫폼 | 10 LTS |
| CoreCLR | .NET의 실행 런타임 | .NET 10 계열의 PC host 경로 |

2026-10-15 확인 기준 C# 14는 최신 정식 버전이며 .NET 10에서 지원한다. C# 15/.NET 11은 preview다. .NET 10이라는 이름이 C# 10을 뜻하지 않으며 CoreCLR도 예전 C# 버전 이름이 아니다. .NET 10 LTS의 지원 종료일은 2028-11-14다. [C# 14](https://learn.microsoft.com/en-us/dotnet/csharp/whats-new/csharp-14), [C# 15 preview](https://learn.microsoft.com/en-us/dotnet/csharp/whats-new/csharp-15), [버전 대응](https://learn.microsoft.com/en-us/dotnet/csharp/language-reference/language-versioning), [.NET 지원 정책](https://dotnet.microsoft.com/en-us/platform/support/policy/dotnet-core)

정확한 SDK/runtime patch·roll-forward·servicing 정책은 구현 진입 BOM에서 고정한다. `latest`나 `preview`를 언어 버전 기준선으로 사용하지 않는다. 지원 종료 전에 런타임 업그레이드를 검토한다.

## 대안과 선택 이유

- TS + QuickJS는 V8 없이도 가능한 선택이다. 탈락 이유를 V8 필수라는 잘못된 전제로 설명하지 않는다. 현재는 C#의 작성 경험·.NET 플러그인 확장·직접 계산 코드가 사용자 우선순위에 더 맞는다.
- Luau는 게임용 VM·빠른 스크립트 교체·제한된 host 확장에 강점이 있다. 다만 오토파일럿은 공통 제어 API로 해결하고 두 번째 언어 운영을 시작하지 않는다.
- C# Native AOT는 다른 배포 시나리오의 후보가 될 수 있으나 동적 assembly loading 제약 때문에 PC 모드 host 기본안으로 사용하지 않는다. [Native AOT](https://learn.microsoft.com/en-us/dotnet/core/deploying/native-aot/)
- 단일 언어만 허용하면 연결 경계는 줄지만, Rust 코어를 가진 엔진에서 Rust 게임플레이를 빌드 시 등록하는 경로까지 막을 필요는 없다. 독립 native DLL 호환·교체 기능은 별도 문제로 남긴다.

## 결과·비용·제한

C# 선택은 비주얼 품질이나 144fps 달성의 보장이 아니다. Rust/wgpu 표현 역량, 데이터 배치, managed/native 호출, JIT·GC·스케줄과 아트 품질을 각각 검증한다. C#이 Lua보다 모든 작업에서 빠르다고 선언하지 않는다.

CoreCLR은 자동 REPL·autopilot·안전한 전체 상태 reload가 아니다. 컴파일러·host·SDK·명령·migration·callback 정리를 구축해야 한다. AssemblyLoadContext는 dependency/lifetime 경계이며 불신 모드 sandbox가 아니다. C++ FFI도 Rust의 메모리 안전성을 자동으로 보존하지 않으므로 소유권·오류·빌드 검증을 요구한다.

## 미결·검증·재검토

미결은 process entrypoint, runtime 패키징, native bridge 세부, schema 코드 생성, debugger, 지원 edit, reload 예산과 웹 실행 경로다. 주 언어를 선택했다고 R03 또는 구현 진입 게이트 전체를 완료 처리하지 않는다.

[게임플레이 SDK](../03-architecture/gameplay-and-mod-sdk.md)와 [오토파일럿 명세](../03-architecture/autopilot-and-live-testing.md)의 최소 fixture·오류·수명·성능 시험을 후속 구현에서 수행한다. 고정 runtime의 실제 host/플랫폼 제약이 필수 요구를 막거나 native hot-reload·불신 모드의 강제 격리가 필수가 되면 후속 ADR로 재검토한다.
