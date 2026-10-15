# C#·TypeScript·Luau: 실행·핫리로드·AI 제작 비교

상태: R01 공식 자료 기반 비교 + 적용 제안. 벤치마크 미실행·언어 미선정 · 2026-10-15

## 이번 결정의 맥락

사용자는 C#의 구조화된 타입·언어 체계, 게임 개발자에게 익숙한 작성 경험을 선호했다. CoreCLR을 쓰면 빠른 핫리로드가 가능하다고 기대했으며 AOT 제약 때문에 TS·Luau 제안을 받았다. 현재 요구는 공식 엔진 API 우선, 개발 중 reload 우선, 장기적으로 배포판 확장도 고려하는 것이다. AI 사용성·이식성·성능을 모두 중요하게 본다.

따라서 TS를 이미 채택한 것으로 취급하지 않는다. “AI 친화적”이라는 주장만으로 선택하거나 C#을 AOT 때문에 자동 탈락시키지 않는다. 아래는 **언어 + 실행 방식 + 배포 방식**을 비교한다.

## 공식 자료에서 확인한 사실

### C# / .NET

Native AOT는 배포 시 IL을 native 코드로 컴파일하며 JIT를 쓰지 않는다. 동적 assembly loading과 runtime code generation 등에 제한이 있다. 따라서 새 assembly를 로드하거나 IL을 갱신하는 개발 경로를 Native AOT 배포판에 그대로 적용한다고 전제할 수 없다. [Native AOT](https://learn.microsoft.com/en-us/dotnet/core/deploying/native-aot/)

`dotnet watch`의 Hot Reload는 지원되는 변경에 적용되며 지원하지 않는 변경은 재시작이 필요할 수 있다. Rust가 호스팅하는 엔진에 같은 개발 경험을 제공하려면 변경 감지·컴파일·runtime update·상태 보존을 통합해야 한다. [dotnet watch](https://learn.microsoft.com/en-us/dotnet/core/tools/dotnet-watch), [custom .NET host](https://learn.microsoft.com/en-us/dotnet/core/tutorials/netcore-hosting)

WebAssembly의 .NET 실행에는 IL 해석 경로와 Wasm AOT 경로가 있다. 공식 Blazor 문서는 AOT의 계산 성능과 다운로드 크기 비용을 설명한다. 이것은 Rust 엔진에 C#을 붙이는 구현이 이미 존재한다는 뜻이 아니다. [공식 Wasm AOT 문서](https://learn.microsoft.com/en-us/aspnet/core/blazor/webassembly-build-tools-and-aot?view=aspnetcore-10.0)

### TypeScript / JavaScript

TS 구문 변환 뒤 실행하는 것은 JS이고, oxc의 type stripping은 의미적 타입 검증과 다르다. 조회한 rquickjs upstream은 QuickJS-NG 바인딩으로 설명된다. deno_core는 V8 기반 코어이며 Deno CLI·모든 Web API·npm 환경 전체를 자동으로 제공하는 제품이 아니다. [oxc](https://oxc.rs/docs/guide/usage/transformer), [rquickjs upstream](https://raw.githubusercontent.com/DelSkayn/rquickjs/master/README.md), [deno_core API](https://docs.rs/deno_core/latest/deno_core/)

V8에는 JIT-less 모드도 있으므로 V8 채택만으로 모든 플랫폼에서 같은 최적화 성능을 얻는다고 판단하지 않는다. [V8 설명](https://v8.dev/blog/jitless)

### Luau

Luau는 게임플레이를 주요 용도로 삼는 최적화된 인터프리터와 선택적 x64·arm64 JIT를 설명한다. Luau와 LuaJIT는 같은 제품이 아니다. 브라우저에서는 native JIT를 그대로 쓴다고 가정하지 않는다. [Luau 성능](https://luau.org/performance/)

점진적 타입과 strict 분석 도구가 있다. 샌드박스용 임베딩 기능도 있지만 host API·자원 제한·신뢰 경계는 엔진이 책임져야 한다. 임의 bytecode를 안전한 입력으로 간주하지 않는다. [타입 안내](https://luau.org/getting-started/), [샌드박스 안내](https://luau.org/sandbox/)

## 다이달로스 관점 비교

아래의 적합성 판단은 조사에 기반한 설계 의견이며, 자체 성능 결과가 아니다.

| 기준 | C# | TypeScript | Luau |
| --- | --- | --- | --- |
| 구조화된 API | 명시적 타입·인터페이스·게임 개발 경험과 잘 맞는 후보 | 타입 선언·도구 진단·객체/함수 API를 구성하기 좋은 후보 | strict 타입·작은 언어 표면을 중심으로 구성 가능 |
| 개발 reload | CoreCLR 개발 host + 지원 edit 또는 module 교체 설계 | 모듈 변환·평가·세대 교체 설계 | bytecode 모듈 준비·세대 교체 설계 |
| 상태 보존 | CLR 객체 교체만으로 엔진 상태가 자동 보존되지 않음 | JS closure·global을 무조건 유지할 수 없음 | table·upvalue를 무조건 유지할 수 없음 |
| 네이티브 실행 | JIT·AOT 모드별 측정, Rust 경계·GC 고려 | V8·QuickJS별 별도 측정 | interpreter·native codegen별 별도 측정 |
| 웹 | .NET Wasm host·Rust Wasm 연결·payload·빌드 비용 비교 필요 | browser JS host가 자연스러운 후보, native와 의미 차이 검증 필요 | VM·compiler의 Wasm 통합과 바인딩·다운로드 비용 검증 필요 |
| 모바일·콘솔 | AOT·runtime·플랫폼 정책별 개발/배포 경로 검증 | JIT 전제 없는 경로·runtime 지원·정책 검증 | interpreter 경로·runtime 지원·정책 검증 |
| 격리·외부 코드 | 일반 CLR 객체 경계만으로 불신 코드 격리라고 보지 않음 | browser sandbox·VM·host API·프로세스 경계를 나누어 검토 | sandbox 지원을 활용하되 host 권한·VM 분리 검토 |
| UI 작성 | 언어 중립 view schema 또는 C# 선언 API | TSX 후보와 연결 가능. React가 필수는 아님 | table·선언 API·외부 schema 후보 |
| 외부 패키지 | .NET API·AOT/trim·권한 적합성 검사 | npm의 Node·DOM·native addon 가정 검사 | Luau·Lua 호환 범위·package 방식 검사 |
| 1인 유지보수 | runtime host·managed/native·Wasm 경로 비용 | native/browser runtime 차이·도구·타입 검사 비용 | C++ VM·분석기·Wasm·도구 통합 비용 |

## “어느 것이 더 빠른가”에 대한 현재 답

전체 순위를 정할 자체 근거가 없다. 최적화된 C# 또는 V8 JIT가 계산 처리량에 유리한 경우를 기대할 수 있지만, 이 기대를 특정 게임·AOT·웹·p99 지연의 보장으로 바꾸지 않는다. Luau는 게임 스크립트용 유력 후보이고 QuickJS는 작은 임베딩 경로의 후보이나, 둘 사이의 승자도 같은 API·실제 작업 없이 단정하지 않는다.

실제 비용은 `스크립트 계산 + Rust 경계 호출 + 데이터 복사 + allocation/GC + compile/reload + 스케줄 지연`이다. 개체별 get/set 수십만 번과 배치 명령 한 번은 다른 작업이다. 복셀 mesh·유체 전체·물리 solver를 스크립트로 옮겨야 한다는 요구는 없다. 고부하 코어는 Rust, 스크립트는 규칙·의사결정·UI와 배치 명령을 담당하는 안이 우선이다.

## 공정한 비교 실험 명세

현재 실행하지 않는다. 최소 기술 실험을 시작할 때 구현은 Luna xhigh에게 위임한다. 게임 샘플을 만들지 않고 엔진 바인딩·reload 시험만 수행한다.

| 실험 | 동일하게 맞출 입력 | 비교할 결과 |
| --- | --- | --- |
| 스크립트 순수 계산 | 분기·벡터·컬렉션·수치 의미·출력 hash | cold/warm 시간·CPU·allocation·p99 |
| Host API 호출 | 동일 entity 집합·read batch·command batch | 호출당 비용·전체 tick·copy bytes |
| 상태 보존 reload | 로직만 수정·schema 추가·삭제·타입 변경 | 실제 저장부터 첫 적용 tick까지·보존·복구 |
| 오류·과부하 | syntax/type/runtime 오류·무한 루프·무거운 host 함수 | 중단·이전 버전 유지·UI 응답·진단 |
| 네이티브·웹 | 동일 의미 API와 script fixture | 출력·예외·시간·payload·시작 시간 |
| AI 작성 | 같은 Luna xhigh·문제·API 문서·수정 횟수 예산 | 첫 성공률·미존재 API 호출·진단 후 수정 성공·사람 개입 |

GC·JIT warmup을 숨기지 않고 cold/warm을 따로 기록한다. 불필요한 루프가 제거되지 않게 결과를 검증한다. 여러 언어를 동시에 제품 지원하는 결론으로 실험 범위를 확대하지 않는다.

## 웹 통합에서 추가로 확인한 비용

대표 Rust 바인딩 mlua는 Luau 지원과 Wasm의 `wasm32-unknown-emscripten` 경로를 문서화한다. 이것을 일반적인 wasm-bindgen 타깃에 즉시 통합 가능하다는 증거로 쓰지 않는다. Luau의 웹 실행이 불가능하다는 판정도 아니다. rquickjs upstream master는 `wasm32-unknown-unknown` 지원과 host·stack 조건을 설명하지만 고정 release·engine 조합의 실제 시험이 필요하다. [mlua](https://raw.githubusercontent.com/mlua-rs/mlua/main/README.md), [rquickjs](https://raw.githubusercontent.com/DelSkayn/rquickjs/master/README.md)

## TS로 만든 게임은 없는가?

없다는 전제는 맞지 않는다. 다만 확인할 때 게임 전체·엔진 일부·게임플레이 스크립트·제작 도구를 나누어야 한다.

| 사례 | 공식 근거로 확인한 사용 범위 | 증명하지 않는 것 |
| --- | --- | --- |
| Bitburner | TS 기반 소스·빌드 설정 공개, 브라우저·Steam 배포 | 고부하 3D·물리·144fps 적합성 |
| Minecraft Bedrock | TS를 JS로 변환해 gameplay customization을 작성하는 공식 경로 | Minecraft 엔진 코어 전체가 TS라는 주장 |
| Cocos Creator | 제작사가 설명하는 TS 기반 엔진·게임 작성 방향 | Cocos2d-x 기반 모든 게임이 TS라는 주장 |
| Tencent PuerTS | Unity/Unreal/.NET에 TS script·host API bridge·backend 선택 제공 | 특정 게임의 채택·전체 성능·무비용 통합 |

근거: [Bitburner](https://github.com/bitburner-official/bitburner-src), [Minecraft 공식 안내](https://learn.microsoft.com/en-ca/minecraft/creator/documents/scriptinggettingstarted?view=minecraft-bedrock-stable), [Cocos의 선택 설명](https://www.cocos.com/en/post/84d2f3b46b8baf9d28b497f803e7f46d), [PuerTS](https://github.com/Tencent/puerts).

대형 네이티브 엔진에서 TS를 코어 계산의 기본 선택으로 권하지 않는 기술적 이유는 있다. TS 타입은 JS 런타임의 고정 메모리 배치·SIMD·allocation 제어를 자동 제공하지 않는다. host 경계 호출·데이터 복사·GC·JIT warmup/deoptimization·플랫폼별 JIT 제약·도구 통합 비용이 생길 수 있다. 이는 기술적 추론이며 산업 전체의 채택 비율을 조사한 통계는 아니다. C#·Luau도 GC·바인딩·플랫폼 비용에서 면제되지 않는다.

이 근거는 Rust 코어 위 공식 script로서 TS가 가능한 선택임을 보여주며, 다른 두 언어보다 무조건 낫다는 뜻은 아니다. PuerTS도 V8과 QuickJS의 크기·성능·디버깅 tradeoff를 구분한다. **TS 채택과 QuickJS 채택을 한 결정으로 묶지 않는다.**

## 지금 결정할 수 있는 것과 검증할 것

1. 언어 중립 상태·명령·진단·module generation 계약을 먼저 확정한다.
2. C# CoreCLR·TS·Luau의 사용 경험·플랫폼 비용을 비교하고 **지금 주 언어 선택을 토론한다**. 모든 선택을 벤치마크 뒤로 미루지 않는다.
3. 사용자가 언어를 고르면 ADR로 기준선을 남긴다. native/web runtime·정확한 version·통합 방식의 세부 근거를 이어서 고정한다.
4. 선택한 경로의 성능·상태 보존·플랫폼 가설을 최소 실험으로 검증한다. 자료만으로 실제 지연을 보장하지 않는다.
5. npm·불신 모드·추가 언어는 독립 확장으로 남긴다. 세 runtime을 모두 영구 통합하지 않는다.

현재 권고는 TypeScript strict다. 공식 API·타입 선언과 웹 배포의 연결을 우선한 적합성 판단이며, AI 성공률·최고 성능의 실측 결론은 아니다. C#의 구조화된 개발 경험을 더 중시하면 CoreCLR 개발과 AOT/Web 배포 통합 비용을 감수하는 선택도 유효하다. 사용자는 실제 게임 사례를 질문 중이며 언어는 아직 확정하지 않았다. 144fps에는 script budget과 Rust 배치 API가 함께 필요하다.
