# PC managed host·Rust bridge 초기 구현 계약

상태: E04 구현 기준안. 실제 연결·reload 검증 전 · 2026-10-15

## 진입점과 역할

초기 C# 경로는 .NET 10 managed host가 Rust native library를 호출하는 방식으로 구현한다. CoreCLR을 임의 Rust executable에 직접 호스팅하는 hostfxr 경로보다 초기 패키징·디버깅을 단순하게 한다. Rust 단독 CLI·게임플레이 빌드 경로는 유지한다. browser C# 실행은 이 PC host의 지원 주장에 포함하지 않는다.

Rust가 world·entity·명령·tick·자원 수명을 소유한다. managed SDK는 공식 타입·명령·조회·module lifecycle을 제공하며 engine internals·GPU pointer를 노출하지 않는다. 같은 프로세스라는 사실이 sandbox를 뜻하지 않는다. 신뢰한 모드만 초기 경로로 로드한다.

## Native ABI

- engine/session 생성·해제, version/capability 조회, schema 있는 command batch와 snapshot/결과 조회를 최소 경계로 둔다.
- opaque handle, UTF-8 bytes와 명시적 길이, 고정 폭 정수와 오류 상태를 사용한다. Rust 기본 ABI·CLR 객체·C++ 객체 layout은 공개하지 않는다.
- 반환 메모리는 Rust 쪽 release API로 해제하고 호출자 입력 buffer는 호출 범위에서만 빌린다. 길이·null·UTF-8·크기 제한·해제 순서를 검증한다.
- panic/managed exception이 외부 ABI를 넘어가지 않게 한다. 복구 불가능한 native crash를 성공 오류 코드로 감추지 않는다.
- 함수의 thread/동시 호출 계약과 종료 중 호출 의미를 명시한다. frame thread에서 compile·module 교체·긴 command 완료를 동기 대기하지 않는다.
- 초기는 복사·직렬화 batch로 정확성을 검증한다. per-component P/Invoke 또는 무기한 pointer lease를 성능 기본안으로 만들지 않는다.

ABI 함수의 정확한 spelling은 Luna 구현과 리뷰 시 고정하고 버전·호환 검사를 함께 둔다. 세계 상태의 두 번째 managed 원본을 만들지 않는다.

## C# SDK와 모듈

공통 SDK assembly는 default context에서 공유한다. 모드에는 ID/version·SDK/engine 범위·entry type·의존성을 가진 manifest와 compiled assembly를 사용한다. 모드별 collectible AssemblyLoadContext와 dependency resolver를 적용하고 SDK type identity가 중복되지 않게 한다.

최소 module lifecycle은 준비·tick·상태 직렬화/복원·해제다. Rust의 권한 상태와 모듈의 로컬 상태를 구분한다. 초기 loader는 missing dependency·API 불일치·진입점 오류를 구체적으로 반환한다. 임의 NuGet/native 라이브러리의 모든 타깃 호환을 보장하지 않는다.

## 초기 hot-reload 경로

처음에는 전체 모듈 교체와 명시적 상태 migration을 구현한다. MetadataUpdater 기반 in-place edit는 별도 기능으로 남겨 compiler delta 지원과 전체 교체를 혼동하지 않는다.

새 module 파일 준비·검증 → 새 context에서 준비/상태 복원 → safe tick 경계에서 generation 전환 → 이전 module 정리·구독 해제 → context 회수 추적.

SDK를 통한 staging 명령은 전환 성공 전 world에 적용하지 않는다. 문법/build/manifest/로드/migration 실패 시 이전 모듈과 Rust world를 유지한다. 신뢰한 .NET 코드가 직접 수행한 외부 I/O는 rollback 대상이라고 약속하지 않는다. 이미 실행 중인 메서드나 외부 참조가 남으면 unload는 지연될 수 있으며, 이는 결과·진단에 표시한다.

## 최초 검증

1. 실제 .NET 10 host가 Rust library를 로드해 entity 생성·조회·수정하고 같은 entity handle/상태를 확인.
2. 모듈 V1→V2에서 로직이 바뀌고 Rust 위치·ID가 유지됨을 확인. fixture는 로직·상태 검증용이며 샘플 게임이 아님.
3. 잘못된 모듈·의존성·migration에서 이전 로직을 계속 사용하고 중간 명령을 적용하지 않음.
4. 반복 교체 후 context 회수와 callback/handle 정리를 확인. 시간 예산과 회수 불가를 별도 진단.
5. ABI buffer/handle·잘못된 입력·중복 release 계약·shutdown 오류를 확인.

실측 전에는 5/50ms reload 또는 144fps 보장을 쓰지 않는다. native 단위 시험과 실제 .NET 통합 시험을 따로 기록한다.

## 도구와 배포

로컬 SDK 10.0.402·runtime 10.0.13·osx-arm64를 확인했다. root global.json과 managed project 설정은 Luna가 작성한다. CI는 해당 SDK를 명시적으로 준비하며, 로컬 시스템의 오래된 dotnet 경로와 혼동하지 않는다.

네이티브 library 경로·CPU/OS·ABI version이 다르면 startup에서 실패한다. 개발 빌드 출력물을 사용하는 최초 smoke와 runtime을 동봉한 배포를 구분한다. 코드·native dependency를 임의로 다운로드하는 updater는 이번 host 구현에 추가하지 않는다.

근거: [공식 .NET 플러그인](https://learn.microsoft.com/en-us/dotnet/core/tutorials/creating-app-with-plugin-support), [unloadability](https://learn.microsoft.com/en-us/dotnet/standard/assembly/unloadability), [native library loading](https://learn.microsoft.com/en-us/dotnet/standard/native-interop/native-library-loading), [ADR-0003](../05-decisions/0003-csharp-and-native-gameplay.md).
