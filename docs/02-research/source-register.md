# 1차 자료 등록부

확인일: 2026-10-15~16 · R01~R02 및 P03 후속 · 상태: 이 등록부는 문서 조사이며 실제 빌드·통합·성능 근거는 별도 worklog 참조.

## 자료 사용 규칙

소스 작성자·공식 프로젝트·표준·공식 발표 자료를 우선한다. `latest`, `main`, `master`는 움직이는 주소다. 아래 확인 기록은 채택 버전이 아니며 R03에서 tag·commit·기능 플래그·타깃·도구체인으로 고정해야 한다. 소개 페이지를 읽은 것과 소스 구현을 검증한 것을 구분한다.

| ID | 자료 | 확인한 범위 | 다이달로스에서의 용도·한계 |
| --- | --- | --- | --- |
| S01 | [Bungie: Multithreading the Entire Destiny Engine, GDC 2015](https://www.gdcvault.com/play/1022164/Multithreading-the-Entire-Destiny) | 세션 개요: job graph, 데이터 수명 검증 | 원칙 참고. Destiny 2의 현행 구현 증거 아님. 전체 강연 분석은 후속 |
| S02 | [Blizzard: Overwatch Gameplay Architecture and Netcode, GDC 2017](https://www.gdcvault.com/play/1024001/-Overwatch-Gameplay-Architecture-and) | 세션 개요: ECS·네트워크 시뮬레이션 | 게임플레이·네트워크 연결 참고. 전체 스레드 토폴로지 증거 아님 |
| S03 | [Bevy PipelinedRenderingPlugin](https://docs.rs/bevy/latest/bevy/render/pipelined_rendering/struct.PipelinedRenderingPlugin.html) | 문서에 0.20.0 표시, sync·extract·render와 non-Wasm 조건 | 분리와 동기화의 실제 경계 참고. 그대로 독립 엔진에 이식된다는 뜻 아님 |
| S04 | [winit EventLoopBuilder](https://docs.rs/winit/latest/winit/event_loop/struct.EventLoopBuilder.html) | 생성·플랫폼 관련 API 문서 | OS 이벤트 루프 스레드 제약 확인 출발점 |
| S05 | [Crossbeam channel](https://docs.rs/crossbeam-channel/latest/crossbeam_channel/) | bounded/unbounded·blocking/try API | 큐 포화 정책 필요. 채널 채택 자체가 지연 보장은 아님 |
| S06 | [Rayon ThreadPoolBuilder](https://docs.rs/rayon/latest/rayon/struct.ThreadPoolBuilder.html) | 풀 생성·스레드 수 설정·수명 | CPU 작업 실행 후보. 우선순위·I/O 정책은 엔진 책임 |
| S07 | [wgpu Buffer](https://docs.rs/wgpu/latest/wgpu/struct.Buffer.html) | map_async·매핑 수명 | 비동기 readback 설계 참고 |
| S08 | [Bevy 공식 headless renderer 예제](https://github.com/bevyengine/bevy/blob/main/examples/app/headless_renderer.rs) | 오프스크린 렌더·이미지 복사 소스 예제 | 엔진 내부 캡처 경로 참고. 다이달로스에서 실행해 본 것은 아님 |
| S09 | [QuickJS 매뉴얼](https://bellard.org/quickjs/quickjs.html) | 임베딩·런타임 개요 | JS 엔진 후보. Deno·Node 호환성 보장 자료 아님 |
| S10 | [rquickjs 설명](https://docs.rs/rquickjs/latest/rquickjs/) / [Runtime API](https://docs.rs/rquickjs/latest/rquickjs/struct.Runtime.html) | 락·실험적 parallel, interrupt·메모리·스택 설정 | 전용 소유자·시간 예산 설계. allocator별 메모리 제한 차이 주의 |
| S11 | [oxc Transformer](https://oxc.rs/docs/guide/usage/transformer) | TypeScript type stripping·JSX 변환 | 변환 후보. 의미적 타입 검사나 전체 reload 시간 증거 아님 |
| S12 | [notify](https://docs.rs/notify/latest/notify/) | 파일 감시 API·플랫폼 주의사항 | 중복·유실·원자 저장 처리 설계 |
| S13 | [Hanabi README](https://github.com/djeedai/bevy_hanabi) / [Cargo manifest](https://raw.githubusercontent.com/djeedai/bevy_hanabi/main/Cargo.toml) | Bevy GPU 파티클 플러그인, 호환 표·의존성 | 독립 wgpu 라이브러리로 간주하면 안 됨 |
| S14 | [Lightyear README](https://github.com/cBournhonesque/lightyear) / [Cargo manifest](https://raw.githubusercontent.com/cBournhonesque/lightyear/main/Cargo.toml) | Bevy용 서버 권한 라이브러리·호환 표·의존성 | 독립 엔진에서 어댑터 또는 다른 전송 계층 비교 필요 |
| S15 | [Taffy](https://github.com/DioxusLabs/taffy) | Block·Flexbox·Grid 레이아웃 | 텍스트·IME·그리기·위젯을 제공하는 완성 UI 엔진은 아님 |
| S16 | [Jolt Physics](https://github.com/jrouwe/JoltPhysics) | C++ 멀티코어 rigid body 라이브러리 | Rust 바인딩·Wasm 빌드·작업 풀 결합은 별도 검증 |
| S17 | [ozz-animation README](https://raw.githubusercontent.com/guillaumeblanc/ozz-animation/master/README.md) | C++ 애니메이션 라이브러리 개요 | Rust·에셋 변환·웹 통합 비용 후속 조사 |
| S18 | [Spine Runtimes License](https://en.esotericsoftware.com/spine-runtimes-license) | 런타임 통합·재배포 조건 문서 | 조건 확인 전 코어 필수 의존성으로 확정하지 않음 |
| S19 | [0 FPS: Meshing in a Minecraft Game](https://0fps.net/2012/06/30/meshing-in-a-minecraft-game/) | 원저자의 greedy meshing 설명 | 알고리즘 후보·최악 형태 비교. 실제 Minecraft 내부 구현 주장 아님 |
| S20 | [Rapier 2D](https://docs.rs/rapier2d/latest/rapier2d/) | Rust 2D 물리 라이브러리 개요 | Jolt와 별개인 2D 충돌 선택지 |
| S21 | [wasm-bindgen-rayon](https://github.com/RReverser/wasm-bindgen-rayon) | Workers·공유 메모리·초기화·빌드 조건 | 웹 작업 풀 후보. 네이티브 Rayon과 동일 배포가 아님 |
| S22 | [WebP API](https://developers.google.com/speed/webp/docs/api) | 인코딩 API와 관련 API 문서 연결 | 정지 이미지·애니메이션 패키징 선택을 분리 |
| S23 | [GitHub protected branches](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-protected-branches/about-protected-branches) | 브랜치 보호·체크·리뷰 기능 | 1인 유지보수에 맞는 실제 설정 확인 |
| S24 | [GitHub Actions secure use](https://docs.github.com/en/actions/reference/security/secure-use) | 최소 권한·의존 Action 고정 등 | 공개 PR CI 설계 근거 |
| S25 | [Godot 기여 규칙](https://raw.githubusercontent.com/godotengine/godot/master/CONTRIBUTING.md) | 이슈·제안·작은 PR·재현 절차 | 절차 참고. 프로젝트 규모와 규칙을 그대로 복제하지 않음 |
| S26 | [Bevy 기여 입구](https://github.com/bevyengine/bevy/blob/main/CONTRIBUTING.md) | 공식 기여·AI 정책 문서로의 연결 | 외부 기여자가 찾기 쉬운 문서 구조 참고 |
| S27 | [Mozilla: SharedArrayBuffer](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/SharedArrayBuffer) | secure context·cross-origin isolation·공유 메모리 | 브라우저 배포와 스레딩 조건 확인 |
| S28 | [wgpu crate 문서](https://docs.rs/wgpu/latest/wgpu/) | 표시 버전 30.0.1, backend·Wasm 객체 제약 | 플랫폼 추상화 경계. 모든 장치의 동일 feature 지원 증거 아님 |
| S29 | [.NET Native AOT](https://learn.microsoft.com/en-us/dotnet/core/deploying/native-aot/) | 실행·동적 로딩·코드 생성 제약 | C# 개발/배포 경로 비교 |
| S30 | [dotnet watch](https://learn.microsoft.com/en-us/dotnet/core/tools/dotnet-watch) | 지원 edit·재시작·감시 | C# Hot Reload가 모든 변경을 처리한다는 가정 검토 |
| S31 | [.NET custom host](https://learn.microsoft.com/en-us/dotnet/core/tutorials/netcore-hosting) | native host 경계 | Rust/CoreCLR 통합 비용 비교 |
| S32 | [.NET Wasm AOT](https://learn.microsoft.com/en-us/aspnet/core/blazor/webassembly-build-tools-and-aot?view=aspnetcore-10.0) | 해석/AOT·계산·다운로드 비용 | native CoreCLR과 웹 실행을 구분 |
| S33 | [Luau 성능](https://luau.org/performance/) | interpreter·선택적 x64/arm64 JIT | 타깃별 실행 경로 비교. 자체 속도 순위 아님 |
| S34 | [Luau 타입·도구](https://luau.org/getting-started/) | strict·분석기·타입 | 작성 경험 비교 |
| S35 | [Luau sandbox](https://luau.org/sandbox/) | host 경계·VM 분리·interrupt·bytecode | 모드·외부 코드 범위 비교 |
| S36 | [V8 JIT-less](https://v8.dev/blog/jitless) | JIT 없는 실행 모드 | V8 항상 JIT 성능이라는 가정 제거 |
| S37 | [deno_core API](https://docs.rs/deno_core/latest/deno_core/) | V8·JsRuntime·resource owner | VM과 Deno/npm 제품 전체를 구분. 과거 별도 저장소 README는 deno로 통합됐다고 표시 |
| S38 | [TypeScript Handbook](https://www.typescriptlang.org/docs/handbook/intro.html) | 정적 타입 검사·언어 기능 | 타입·API·작성 경험 비교 |
| S39 | [mlua README](https://raw.githubusercontent.com/mlua-rs/mlua/main/README.md) | Luau 지원·문서화된 Wasm Emscripten 타깃 | wasm-bindgen 구성의 즉시 호환을 가정하지 않음 |
| S40 | [rquickjs upstream README](https://raw.githubusercontent.com/DelSkayn/rquickjs/master/README.md) | QuickJS-NG·Wasm·host 조건 | main branch 주장과 고정 release 검증을 구분. 동명 fork를 upstream 근거로 쓰지 않음 |
| S41 | [MetadataUpdater](https://learn.microsoft.com/en-us/dotnet/api/system.reflection.metadata.metadataupdater.applyupdate?view=net-10.0) | metadata·IL·PDB delta 적용 API | CoreCLR host의 update 기능·지원 여부와 상태 migration 구분 |
| S42 | [Apple M3 Pro 14인치 사양](https://support.apple.com/en-us/117736) | 화면·ProMotion 최대120Hz | 내부 144fps 처리량과 실제 표시율 구분 |
| S43 | [Minecraft TypeScript scripting](https://learn.microsoft.com/en-ca/minecraft/creator/documents/scriptinggettingstarted?view=minecraft-bedrock-stable) | 공식 TS→JS gameplay customization | Minecraft 엔진 전체가 TS라는 근거 아님 |
| S44 | [Tencent PuerTS](https://github.com/Tencent/puerts) | Unity/Unreal script·host bridge·backend 비교 | 실제 엔진 통합 경로. 특정 상용 게임 사용은 별도 근거 필요 |
| S45 | [Cocos의 TS 선택 설명](https://www.cocos.com/en/post/84d2f3b46b8baf9d28b497f803e7f46d) | 제작사 설명·선택 이유 | 도입 근거·주장. 다이달로스 144fps 근거 아님 |
| S46 | [Bitburner source](https://github.com/bitburner-official/bitburner-src) / [manifest](https://raw.githubusercontent.com/bitburner-official/bitburner-src/dev/package.json) | 배포된 게임·TS 기반 소스·도구 | TS 게임의 실제 사례. 하이엔드 3D 성능 증거 아님 |
| S47 | [C# 14](https://learn.microsoft.com/en-us/dotnet/csharp/whats-new/csharp-14) | 최신 정식 C#·.NET 10 지원 명시 | 언어 버전 기준. 엔진 host·성능 검증 자료 아님 |
| S48 | [C# 15 preview](https://learn.microsoft.com/en-us/dotnet/csharp/whats-new/csharp-15) | C# 15/.NET 11 preview 상태 | preview를 정식 기준선으로 혼동하지 않음 |
| S49 | [C# 버전 대응](https://learn.microsoft.com/en-us/dotnet/csharp/language-reference/language-versioning) | framework별 기본 언어 버전·설정 | C# 10과 .NET 10 구분, 재현 가능한 compiler 설정 |
| S50 | [.NET 지원 정책](https://dotnet.microsoft.com/en-us/platform/support/policy/dotnet-core) | .NET 10 LTS·2028-11-14 지원 종료 | major 기준선과 servicing 근거. 조회 페이지의 과거 patch를 오늘 최신이라고 쓰지 않음 |
| S51 | [.NET SIMD](https://learn.microsoft.com/en-us/dotnet/standard/simd) | Numerics·가속 조건·vector 지원 | CPU/runtime 조건과 batch 자료 배치 설계. 자체 벤치마크 아님 |
| S52 | [.NET 플러그인](https://learn.microsoft.com/en-us/dotnet/core/tutorials/creating-app-with-plugin-support) | AssemblyLoadContext·dependency resolver·불신 코드 경고 | 모드 의존성·type identity·신뢰 경계. ALC를 sandbox로 간주하지 않음 |
| S53 | [.NET unloadability](https://learn.microsoft.com/en-us/dotnet/standard/assembly/unloadability) | collectible ALC·협조적 unload·외부 참조 | 반복 reload 회수·callback/job 정리. 즉시 종료 보장 아님 |
| S54 | [Unity 6.3 scripting backends](https://docs.unity3d.com/6000.3/Documentation/Manual/scripting-backends.html) | Mono·IL2CPP 설명 | 사용자 경험의 버전 맥락. 과거 프로젝트 backend를 확인한 것은 아님 |
| S55 | [UnityTest 1.4 문서](https://docs.unity3d.com/Packages/com.unity.test-framework@1.4/manual/reference-attribute-unitytest.html) | Play Mode coroutine·yield 기반 테스트 | C# 자동 테스트 가능성. 사용자의 설치 package 버전이나 실행 중 새 소스 주입 지원 증거 아님 |
| S56 | [xLua README](https://github.com/Tencent/xLua/blob/master/README_EN.md) | C#/Lua 호출·VM·문자열 평가·예제 | 임베디드 제어 통로의 사례. 사용자 프로젝트의 채택 증거 아님 |
| S57 | [xLua Hotfix](https://github.com/Tencent/xLua/blob/master/Assets/XLua/Doc/Hotfix_EN.md) | 생성·주입 사전 조건·대체 API·제약 | hotfix와 일반 테스트 명령 구분. 무제한 코드 교체 아님 |
| S58 | [Unity compilation/reload](https://docs.unity.com/en-us/engine/6000.6/manual/scripting/compilation-and-code-reload) | 코드 수정·Play mode 등과 reload 관계의 일반 설명 | 6.6 참고 문서. Unity 6.3 프로젝트의 정확한 설정·지연을 증명하지 않음 |
| S59 | [Steam Workshop](https://partner.steamgames.com/doc/features/workshop) | 게임의 Workshop 통합·배포 기능 안내 | 배포 adapter 비교. 런타임·hot-reload·플러그인 API를 대신하지 않음 |
| S60 | [.NET native library loading](https://learn.microsoft.com/en-us/dotnet/standard/native-interop/native-library-loading) | 플랫폼별 native library 이름·검색·resolver | 모드 native dependency·타깃 진단 설계 |
| S61 | [Rust external ABI](https://doc.rust-lang.org/reference/items/external-blocks.html#abi) | Rust ABI 안정성 미보장·C ABI 타깃 의미 | build-time Rust API와 binary plugin ABI 구분 |
| S62 | [Rust FFI](https://doc.rust-lang.org/nomicon/ffi.html) | C 경계·소유권·unsafe·callback·unwind | C++ 어댑터의 계약·검증. Rust 안전성이 foreign 코드를 자동 보증하지 않음 |
| S63 | [CXX](https://cxx.rs/) | Rust/C++ bridge의 목적·경계 | 통합 후보. 채택 버전·타깃·실제 엔진 통합은 미검증 |
| S64 | [Bevy ComputeTaskPool0.18.1](https://docs.rs/bevy_tasks/0.18.1/bevy_tasks/struct.ComputeTaskPool.html) | global pool 초기화·유효 thread 수 | process 예산 불일치를 숨기지 않는 병렬 ECS 계약 |
| S65 | [oxijolt](https://docs.rs/oxijolt/latest/oxijolt/) / [native build](https://github.com/pockerhead/oxijolt/blob/main/docs/building.md) | 배포 source1.1.0+jolt-5.6.0의 초기화·CMake·prebuilt 경계와 대조 | Jolt native 통합. 기능 소개는 엔진 검증 완료가 아님 |
| S66 | [Kira Panning](https://docs.rs/kira/latest/kira/struct.Panning.html) | 0.12.5에서 pan -1..1 의미 확인 | 엔진 volume/pan·실제 PCM 검증 계약 |
| S67 | [glTF 2.0 규격](https://registry.khronos.org/glTF/specs/2.0/glTF-2.0.html) | 2026-10-16: GLB·외부 buffer/image·URI·image type·material/accessor 규격 | bounded engine resolver와 명시적인 import subset 설계. 아직 importer 구현/전체 conformance 아님 |
| S68 | [gltf 1.4.1 manifest](https://docs.rs/crate/gltf/1.4.1/source/Cargo.toml), [reader](https://docs.rs/gltf/1.4.1/gltf/mesh/struct.Reader.html) | 2026-10-16: utils reader·default import의 image/base64/urlencoding 의존·라이선스/MSRV 선언 | defaults-off parser 후보. 실제 lock/타깃 검증 전 미채택 |
| S69 | [png 0.18.1 Decoder](https://docs.rs/png/0.18.1/png/struct.Decoder.html) | 2026-10-16: decoder limits·caller buffer API와 제한 경계 | 기존 encoder 의존성의 bounded decoder 재사용안. image pipeline은 미구현 |
| S70 | [hound 3.5.1 manifest](https://docs.rs/crate/hound/3.5.1/source/Cargo.toml), [API](https://docs.rs/hound/3.5.1/hound/) | 2026-10-16: PCM/float WAV·runtime dependency 없음·Apache-2.0 | bounded WAV importer 후보. 미채택·재생/streaming 완료 아님 |
| S71 | [MCP 2026-07-28 versioning](https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning), [legacy lifecycle](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle) | 2026-10-16: 현대 per-request/discover와 legacy initialize·transport 종료 차이 | 같은 engine session에 대한 MCP adapter의 버전별 검증. 현재 MCP 미구현 |
| S72 | [rmcp 3.5.1 manifest](https://docs.rs/crate/rmcp/3.5.1/source/Cargo.toml), [features](https://docs.rs/crate/rmcp/3.5.1/features) | 2026-10-16 Luna 조사: Rust/MSRV/license·server/stdio/macros 기능과 전이 의존성 | exact-version SDK 후보. actual lock/build·두 protocol 시험 전 미채택 |
| S73 | [Rust File::try_lock](https://doc.rust-lang.org/std/fs/struct.File.html#method.try_lock) | 2026-10-16: 1.89 안정화·nonblocking lock·WouldBlock/Error·descriptor 수명/플랫폼 제약 | cache exclusive ownership 후보. inode unlink/clone 재잠금 회피, 실제 OS 시험 별도 |
| S74 | [Khronos Cube](https://github.com/KhronosGroup/glTF-Sample-Assets/tree/edc7c9e67c639d230715049ee31f9a96a6babbbe/Models/Cube) | 2026-10-16: 고정 원본 payload·모델별 CC0와 metadata CC-BY 구분·외부 BIN/PNG | 실제 import 입력. 다운로드 hash/구조 확인만 했으며 cook/표시 성공 아님 |
| S75 | [Kenney Tiny Dungeon](https://kenney.nl/assets/tiny-dungeon) | 2026-10-16: 공식 CC0·archive license·palette PNG 원본 확보 | 실제 2D decode/atlas 입력. 게임 아트 선정·sprite runtime 검증 아님 |
| S76 | [Kenney Interface Sounds](https://kenney.nl/assets/interface-sounds) | 2026-10-16: 공식 CC0·실제 archive는 OGG, 외부 FFmpeg 6.0으로 WAV 파생 | WAV importer 실제 오디오 입력. engine OGG/FFmpeg 통합·재생 검증 아님 |
| S77 | [Kenney Nature Kit](https://kenney.nl/assets/nature-kit) | 2026-10-16: 공식/archive CC0·선택 GLB의 KHR_materials_unlit 선언 | 첫 subset의 미지원 진단 입력. 확장을 삭제해 성공으로 간주하지 않음 |
| S78 | [W3C PNG gAMA](https://www.w3.org/TR/png-3/#11gAMA)·[sRGB](https://www.w3.org/TR/png-3/#11sRGB) | 2026-10-16: gamma power 관계·sRGB 태그의 우선순위·alpha의 별도 의미 확인 | 실제 gamma-tagged palette 원본을 보존해 color-role sRGB 정규화. 전체 ICC/CMS 지원 근거 아님 |
| S79 | [glTF image transfer 정의](https://registry.khronos.org/glTF/specs/2.0/glTF-2.0.html#images)·[규격 원문](https://github.com/KhronosGroup/glTF/blob/main/specification/2.0/Specification.adoc) | 2026-10-16: image의 embedded 색공간보다 material의 transfer 정의를 사용 | standalone PNG 정규화와 glTF encoded-color 정책을 공통 decoder/API 안에서 구분하고 cache non-alias 검증 |
| S80 | [wgpu v27.0.1 types 원본](https://github.com/gfx-rs/wgpu/blob/v27.0.1/wgpu-types/src/lib.rs) | 2026-10-16: pinned local source와 ALPHA_BLENDING factors·sRGB texture format·CompositeAlphaMode 정의 대조 | sprite의 linear 합성과 PNG/native alpha 경계 설계. 새 sprite runtime 통과 근거 아님 |
| S81 | [PNG 3rd edition alpha](https://www.w3.org/TR/2025/REC-png-3-20250624/#6AlphaRepresentation) | 2026-10-16: PNG RGB의 unassociated alpha·§12.3 변환/alpha0 규칙 확인 | decoder alpha와 GPU 합성·PNG encode 표현을 구분. premultiplied linear target은 decode 후 나누고 다시 encode하는 엔진 설계 |
| S82 | [Kira pinned StaticSoundData](https://raw.githubusercontent.com/tesselode/kira/74b3ef0fd8bd15f90e3b469208ee741f04374a62/crates/kira/src/sound/static_sound/data.rs) | 2026-10-16: local kira0.12.5 VCS metadata의 commit과 upstream raw source를 대조해 Arc Frame sharing·loop configuration 확인. 개별 docs.rs API와 다른 raw 페이지 일부는 조회 실패 | imported audio의 one-time bank conversion/voice 공유 설계. native 출력·callback drop·플랫폼 통과 근거 아님 |

S67~S70의 확인 결과와 CPU cook/GPU resource identity 적용안은 [에셋 상세 계약](../03-architecture/asset-import-cook-and-residency.md)에 둔다. 해당 추가는 포맷 조사이며 새 dependency를 Cargo에 설치한 결과가 아니다.

S71~S72는 [session service의 MCP 기준선](../03-architecture/session-control-service.md)에 연결한다. 조사 확인과 실제 SDK 조합/stdio conformance 통과를 구분한다.

S74~S77의 실제 원본·archive·변환 hash와 라이선스 범위는 [에셋 출처 기록](asset-validation-provenance.md)에 둔다. 에셋은 ignored 로컬 QA 자료이며 engine source/package에 포함하지 않는다.

## 조사 실패·부족 기록

- WebGPU 사양 전체는 조회 도구의 크기 제한으로 읽지 못했다. 필요한 절 단위로 후속 확인한다.
- 과거 fluid CA 블로그 주소는 본문을 확보하지 못했다. R01 유체 설계는 자체 제안이며 해당 글이나 Terraria의 실제 구현에 근거했다고 주장하지 않는다.
- Lightyear의 과거 또는 잘못된 저장소 주소 대신 현재 조회된 `cBournhonesque/lightyear`를 사용한다.
- 콘솔 SDK·실기기·유료 런타임·DCC 도구의 통합 시험은 수행하지 않았다.

## 후속 근거 강화

R01은 적합성 위험을 찾는 조사다. 후보별 릴리스 tag·commit, MSRV, 전이 의존성, feature 조합, 라이선스 파일, 지원 타깃을 고정한 BOM과 최소 통합 실험 결과가 있어야 채택할 수 있다. 조회 시점에 보이는 버전을 “전체 스택의 최신 호환 조합”으로 표현하지 않는다.
