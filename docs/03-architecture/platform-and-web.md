# 플랫폼·웹·배포 계약

상태: macOS·웹 우선 설계 제안. 아직 대응 검증된 플랫폼 없음 · 2026-10-15

## 출발점

macOS와 WebGPU 브라우저를 초기 설계 대상으로 삼는다. Windows·Linux·iOS·Android·후속 콘솔 확장을 위해 OS·GPU·입력·I/O·저장·오디오·수명을 platform adapter로 나눈다. 사용자의 전체 플랫폼 목표와 현재 지원 인증을 구분한다.

기준 개발 장비는 MacBook Pro, M3 Pro CPU 12/GPU 18코어, 통합 메모리 18GB, 내장 화면 3024×1964이며 목표는 144fps 이상이다. 내부 해상도·품질·허용 초과율은 미결이다. 통합 메모리를 RAM·VRAM의 독립된 예산으로 중복 계산하지 않는다.

## 플랫폼별 검증 대상

| 대상 | 실행 배치 후보 | 검증할 차이 |
| --- | --- | --- |
| macOS | OS main loop + simulation owner + renderer + 배경 pool | Metal·window 수명·IME·Retina·전력·복귀 |
| WebGPU browser | host main의 입력·DOM 접점 + simulation worker + GPU owner | worker 초기화·공유 메모리·GPU 소유·비동기 I/O·탭 중단 |
| Windows/Linux client | 네이티브 어댑터 | backend·device·gamepad·driver·package |
| Linux dedicated server | renderer 없는 네이티브 프로세스 | CPU·network·storage·signal·관측·GPU 의존 제거 |
| iOS/Android | 모바일 어댑터 | 메모리·발열·touch·app suspend·audio route·store |
| Console | 비공개 SDK 어댑터 후보 | 벤더 접근·toolchain·GPU backend·인증·운영 조건 미조사 |

wgpu는 네이티브 backend와 Wasm의 WebGPU 등을 제공한다. 브라우저 GPU 객체를 네이티브처럼 스레드 간 공유할 수 있다고 전제하지 않는다. [wgpu 공식 문서](https://docs.rs/wgpu/latest/wgpu/)

## 브라우저 스레드 설계

Web Workers + Wasm 공유 메모리 경로와, 공유 메모리 없는 message passing 경로를 비교한다. 공유 메모리에는 secure context·cross-origin isolation 같은 배포 조건이 있다. 직접 호스팅·외부 iframe에서 가능한 조건을 확인해 배포 계약으로 삼는다. [SharedArrayBuffer 문서](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/SharedArrayBuffer)

wasm-bindgen-rayon은 Workers·공유 메모리를 쓰는 후보이고 초기화·빌드 조건이 있다. 채택 시 정확한 toolchain·feature를 고정한다. Bevy 표준 pipelined rendering은 non-Wasm으로 표시되므로 네이티브 구성을 그대로 웹 지원이라 부르지 않는다. [wasm-bindgen-rayon](https://github.com/RReverser/wasm-bindgen-rayon), [Bevy pipeline](https://docs.rs/bevy/latest/bevy/render/pipelined_rendering/struct.PipelinedRenderingPlugin.html)

GPU owner를 main 또는 지원을 확인한 worker에 두는 안을 비교한다. device와 GPU 자원은 해당 소유자에 제한한다. 입력·resize·IME·visibility는 메시지로 전달하며 DOM과 Wasm worker가 ECS를 공유 가변 참조하지 않는다.

공유 메모리가 없는 환경에서 분할 작업을 협조 실행하는 안도 검토한다. 같은 30명·시뮬레이션·화질 계약을 만족했다는 근거가 없으면 정식 지원으로 표시하지 않는다. 미지원 상태와 제한을 명확히 반환한다.

## 공통 어댑터

| 어댑터 | 공통 계약 | 웹 특유의 조사 |
| --- | --- | --- |
| Clock/Lifecycle | 단조 시간·pause/resume·tick 정책 | background tab 제한·복귀 후 따라잡기 |
| Asset IO | hash 요청·취소·진행·캐시 | fetch·CORS·quota·부분 다운로드 |
| Storage | schema·checkpoint·완료 ack | IndexedDB/OPFS 후보·quota·eviction |
| Transport | 입력·복제 메시지·대역폭 제어 | native UDP를 직접 쓴다는 전제 제거. WebTransport 등 비교 |
| Script host | C# SDK 의미·module generation·직렬화 상태 | .NET Wasm 실행·Rust Wasm bridge·payload·배포 제한을 후속 검증 |
| Audio | stream·bus·device 상태 | 사용자 활성화·resume·worker 경계 |
| Text/Input | IME·clipboard·focus·touch·gamepad | DOM 접점과 네이티브 UI 렌더의 연결 |
| Control | request ID·결과·진단 | 권한·다운로드·로컬 bridge |

## 버전과 배포

[ADR-0003](../05-decisions/0003-csharp-and-native-gameplay.md)에 따라 PC는 C#/.NET 10 CoreCLR 동적 모드를 우선 설계한다. 웹·모바일의 다운로드형 모드는 현재 동일 방식 요구에서 제외한다. 같은 게임 기능·저장/API 의미라는 목표와 같은 런타임 binary·assembly 로딩 방식은 구분한다. 웹에서 C# 게임을 실행할 실제 경로는 미검증이며 TS 제품 지원으로 자동 대체하지 않는다. Rust 게임플레이의 native·Wasm 빌드도 플랫폼별 의존성 검증이 필요하다.

소스형 오픈소스 엔진, 설치 가능한 네이티브 산출물, 웹 배포를 설계한다. engine version, 공개 API, script API, asset cook version, world save schema, network protocol을 별도 식별자로 manifest에 둔다. 모든 호환성을 엔진 버전 문자열 하나로 판정하지 않는다.

호환 기간·SemVer 적용 시작·pre-release 의미·migration 정책은 후속 ADR에서 정한다. 현재 릴리스·지원 보장은 없다. 콘솔 SDK를 공개 저장소에 넣는 것을 전제로 하지 않으며 공개 코어와 연결 경계를 구분한다.

## 미결 사항

초기 브라우저·최소 버전, 외부 iframe, 기능/API 동등성 검증 범위, 내부 해상도·품질·144fps 부하 계약, 웹 C# host·저장·전송 gateway, 어댑터별 최소 기술 실험이 필요하다.
