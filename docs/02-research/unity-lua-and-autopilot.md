# Unity의 Lua 연동과 다이달로스 오토파일럿

상태: 공식 자료 조사 + 사용자 경험에 대한 제한된 추론 · 2026-10-15

## 조사 질문과 사용자 맥락

사용자는 Unity 6.3 프로젝트에서 오토파일럿 형태의 플레이테스트에 Lua가 필요했던 것으로 기억한다. 당시 코드를 확인하지 않았으므로 사용한 플러그인, 실제 런타임, Lua 선택의 정확한 원인은 미확인이다. 아래 xLua는 비교 사례이지 해당 프로젝트가 사용했다는 주장이 아니다.

이 조사에서는 게임플레이의 주 언어, 실행 중 테스트 명령 전달, 새 코드 주입을 분리한다. 사용자는 다이달로스에서 플레이 조작·검증과 새 게임 로직 주입 모두를 원하며, C#으로 이를 설계하는 방향에 동의했다.

## 확인한 사실

| 항목 | 공식 자료에서 확인한 내용 | 판단의 한계 |
| --- | --- | --- |
| Unity 6.3 런타임 | 해당 버전 문서는 Mono와 IL2CPP를 설명 | 일반 .NET CoreCLR host의 기능을 Unity에 그대로 가정할 수 없음 |
| C# 플레이테스트 | Unity Test Framework의 UnityTest는 Play Mode에서 coroutine으로 프레임을 넘기며 테스트 가능 | 이미 실행 중인 임의 세션에 새 테스트 소스를 무중단 주입한다는 보장은 아님 |
| Lua 임베딩 | xLua는 Lua VM에서 문자열 코드를 실행하고 C# API를 호출하는 경로 제공 | 타 언어 경계·상태 정리·할당 비용까지 없어지는 것은 아님 |
| Lua hotfix | xLua는 사전 설정·코드 생성·주입을 거친 C# 메서드의 Lua 대체를 설명 | 어떤 C#·엔진 내부 코드든 무제한 교체하는 기능은 아님 |
| 현대 .NET | 지원되는 IL/metadata 변경 적용과 collectible AssemblyLoadContext에 의한 모듈 수명 관리 가능 | 임의 변경의 무조건 성공·즉시 unload·상태 보존을 뜻하지 않음 |

근거: [Unity 6.3 backends](https://docs.unity3d.com/6000.3/Documentation/Manual/scripting-backends.html), [UnityTest](https://docs.unity3d.com/Packages/com.unity.test-framework@1.4/manual/reference-attribute-unitytest.html), [xLua](https://github.com/Tencent/xLua/blob/master/README_EN.md), [xLua hotfix](https://github.com/Tencent/xLua/blob/master/Assets/XLua/Doc/Hotfix_EN.md), [MetadataUpdater](https://learn.microsoft.com/en-us/dotnet/api/system.reflection.metadata.metadataupdater.applyupdate?view=net-10.0), [.NET unloadability](https://learn.microsoft.com/en-us/dotnet/standard/assembly/unloadability).

## 당시 Lua를 쓴 이유에 대한 추론

플레이 중 AI가 `이동 → 프레임 대기 → 공격 → 상태 확인 → 캡처` 같은 새 절차를 반복해서 만들었다면, 이미 게임에 들어 있는 Lua VM에 절차를 보내는 방식이 편리했을 수 있다. 테스트를 바꿀 때마다 Unity 프로젝트 C# 재컴파일·code reload 경로를 거치지 않는 통로가 된다. Unity는 코드 수정 등으로 재컴파일·reload가 발생할 수 있음을 설명한다. 이 일반 설명을 당시 프로젝트의 실제 지연 측정으로 취급하지 않는다. [Unity compilation and reload](https://docs.unity.com/en-us/engine/6000.6/manual/scripting/compilation-and-code-reload)

또 다른 가능성은 기존 오토파일럿 도구가 Lua 실행 API만 노출했거나, 이미 Lua용 행동·대기·조회 바인딩이 갖춰져 있었던 경우다. 이를 확인할 프로젝트 자료는 아직 없다.

따라서 당시 “Lua가 필요했다”는 경험은 실행 중 제어 통로의 필요성을 보여준다. Unity의 C#으로 자동 테스트가 불가능했다거나 CoreCLR이 없으면 반드시 Lua가 필요하다는 결론으로 확대하지 않는다. 플레이테스트 목적이라는 답변을 모바일 출시 후 hotfix 요구로 바꾸어 기록하지 않는다.

## 다이달로스에 적용하는 결정

| 요구 | 기본 경로 | reload 필요 여부 |
| --- | --- | --- |
| 기존 기능을 조작·관찰·검증 | 외부 AI/테스트 실행기가 CLI·MCP 공통 제어 서비스 사용 | 게임 코드 reload 불필요 |
| 새로운 테스트 순서·분기·반복 | 외부 실행기에서 시나리오를 수정해 같은 세션에 요청 | 시나리오 변경만으로 world 재생성하지 않음 |
| 게임 규칙·컴포넌트·시스템 추가/변경 | C# 컴파일·검증·staging·지원 edit 또는 모듈 교체 | 상태 migration·안전한 tick 경계 필요 |
| Rust 게임플레이 변경 | 엔진과 함께 빌드·프로세스 재시작 | 초기에는 native hot-reload를 제공하지 않음 |

주 언어는 C#이며 오토파일럿을 위해 Lua/Luau를 필수로 추가하지 않는다. CoreCLR 채택만으로 이 API가 구현되는 것은 아니다. Rust world 소유권과 제어·reload 계약을 별도로 구축해야 한다. 상세 계약은 [오토파일럿](../03-architecture/autopilot-and-live-testing.md), [게임플레이 SDK](../03-architecture/gameplay-and-mod-sdk.md), [ADR-0003](../05-decisions/0003-csharp-and-native-gameplay.md)으로 연결한다.

## 반례·비용과 재검토 조건

외부 실행기의 명령 API에 필요한 동작이 없으면 실제 테스트를 수행할 수 없다. 먼저 입력·조회·대기·캡처와 등록된 게임 명령을 충분히 노출해야 한다. 임의 코드 평가를 추가하는 것만으로 플레이테스트가 정확해지지는 않는다.

C# 게임 코드 교체는 컴파일·JIT·GC·수명 정리 비용을 가진다. 지원하지 않는 변경은 이유를 반환하고 명시적 재시작 경로를 제공한다. Lua도 VM 상태·host object·callback을 자동으로 안전하게 교체해 주지는 않는다.

추후 작은 임베디드 시나리오 언어가 실제 요구로 입증되면 별도 ADR로 비교한다. 지금은 두 번째 언어·VM·SDK를 유지하는 비용을 추가하지 않는다. 어떤 경로도 아직 구현·성능 검증하지 않았다.
