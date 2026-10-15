# 1차 자료 등록부

확인일: 2026-10-15 · R01 · 상태: 문서 조사. 빌드·통합·성능 검증은 수행하지 않음.

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

## 조사 실패·부족 기록

- WebGPU 사양 전체는 조회 도구의 크기 제한으로 읽지 못했다. 필요한 절 단위로 후속 확인한다.
- 과거 fluid CA 블로그 주소는 본문을 확보하지 못했다. R01 유체 설계는 자체 제안이며 해당 글이나 Terraria의 실제 구현에 근거했다고 주장하지 않는다.
- Lightyear의 과거 또는 잘못된 저장소 주소 대신 현재 조회된 `cBournhonesque/lightyear`를 사용한다.
- 콘솔 SDK·실기기·유료 런타임·DCC 도구의 통합 시험은 수행하지 않았다.

## 후속 근거 강화

R01은 적합성 위험을 찾는 조사다. 후보별 릴리스 tag·commit, MSRV, 전이 의존성, feature 조합, 라이선스 파일, 지원 타깃을 고정한 BOM과 최소 통합 실험 결과가 있어야 채택할 수 있다. 조회 시점에 보이는 버전을 “전체 스택의 최신 호환 조합”으로 표현하지 않는다.
