# 초기 구현 의존성 기준선

상태: manifest·배포 source 확인 + 모듈별 빌드 검증 진행 · 확인일 2026-10-16

## 이 문서를 둔 이유

조사 후보 목록과 실제 빌드에 들어간 부품을 구분한다. `Cargo.lock`은 Rust 전이 의존성의 정확한 해석 결과이고, 이 문서는 주요 선택의 역할·native 요구·검증 경계를 설명한다. 완전한 SBOM이나 모든 전이 라이선스 검토를 대신하지 않는다. 배포 전 notice·license 파일과 플랫폼별 runtime 동봉 조건을 다시 검사한다.

## 주요 기준선

| 부품 | 초기 선택 | 용도와 적용 경계 |
| --- | --- | --- |
| Rust | 1.94.0, edition2024 | root toolchain 고정. 엔진 자체 기준이며 모든 부품의 최소 버전을 뜻하지 않음 |
| Bevy ECS / tasks | 0.18.1 | ECS·병렬 executor 부품. Bevy App·renderer 전체 도입 아님 |
| Rayon | 1.12.0 | 유한 admission을 감싼 background CPU jobs |
| wgpu | 27.0.1 | 첫 실제 Metal capture 검증 버전. 최신 버전이라는 이유로 채택한 것은 아님 |
| winit | 0.30.12 | native OS event·window. macOS main thread 제약을 실행 통합에서 유지 |
| glam / png | 0.30.9 / 0.18.1 | 렌더 수학·실제 readback PNG 인코딩 |
| gltf / hound | 1.4.1 / 3.5.1 | P03.1 CPU glTF/GLB static subset·WAV decoder. 렌더·재생 소비와 별도 |
| oxijolt / Jolt | 1.1.0+jolt-5.6.0 / 5.6.0 | Rust safe adapter를 통해 C++ Jolt를 호출. 최소 shape·step·query부터 검증 |
| Rapier 2D | 0.36.1 | 별도 2D rigid-body world. 타일 유체와 다른 기능 |
| Kira | 0.12.5 | native audio manager와 실제 offline mixer. CPAL 장치 계층 사용 |
| C# / .NET | 14 / SDK10.0.402, runtime10.0.13 | 설치 확인한 macOS-arm64 기준. managed SDK·모듈 교체 검증 진행 |

정확한 serde·clap·thiserror 등 직접 의존성과 feature는 각 Cargo manifest, 전이 버전·checksum은 lockfile에서 확인한다. 이 표의 버전이 고정돼 있어도 브라우저·Windows·모바일 지원은 해당 타깃에서 별도로 검증해야 한다.

## Jolt native 경계

배포 crate source에서 oxijolt의 MSRV1.88, Rapier2D의 MSRV1.86을 확인했다. Kira manifest에는 MSRV가 선언되지 않아 임의 최소 Rust 버전을 적지 않는다. 이 조합의 엔진 지원 여부는 Rust1.94 실제 빌드 결과로 판단한다.

oxijolt-sys는 등록된 일부 Linux/Windows64bit 구성에서 release archive를 다운로드하고 package에 들어 있는 SHA-256을 검증하는 기본 `prebuilt` 경로를 제공한다. 맞는 archive가 없는 구성과 macOS는 C++ toolchain·CMake3.20 이상을 사용한 source build 경로를 둔다. `JOLTC_PREBUILT=off`는 archive 사용을 끈다. libclang은 기본 committed bindings 경로의 필수 조건이 아니며 `bindgen` feature는 별도 선택이다. 이는 해당 배포 source의 동작 확인이며 모든 타깃을 빌드했다는 뜻은 아니다. [oxijolt-sys 공식 설명](https://docs.rs/oxijolt-sys/latest/oxijolt_sys/), [빌드 규칙](https://github.com/pockerhead/oxijolt/blob/main/docs/building.md)

oxijolt는 process-wide 초기화를 한 번 수행하고 process lifetime 동안 Jolt global state를 유지한다. world 생성·해제의 native global map 접근을 직렬화하는 내부 경계가 있다. Daedalus는 각 world에 제한된 worker 수를 지정하고 ECS·Rayon의 worker 예산과 합산해 진단해야 한다. 의존성의 thread-safe API 선언만으로 엔진 전체의 무경합이나 프레임 지연을 증명하지 않는다. [oxijolt 공식 API](https://docs.rs/oxijolt/latest/oxijolt/)

## 라이선스·재배포

배포 manifest에서 oxijolt·Kira는 MIT OR Apache-2.0, Rapier2D는 Apache-2.0으로 확인했다. Jolt native source와 각 전이 부품의 별도 license·notice는 재배포 산출물에 반영해야 한다. Daedalus의 dual license가 외부 부품의 라이선스를 바꾸지 않는다. 현재 저장소에는 제3자 게임의 proprietary asset을 넣지 않는다.

.NET SDK 설치와 실제 플레이어에 CoreCLR runtime을 동봉한 배포는 서로 다른 작업이다. native bridge·runtime·플러그인의 CPU/OS·ABI 호환과 package layout은 배포 검증으로 남긴다. C# managed test fixture를 다른 플랫폼의 모드 지원 증거로 사용하지 않는다.

## 실행 근거

첫7개 Rust crate의 CPU·Metal 결과는 [첫 구현 기록](../06-worklog/2026-10-15-e01-foundation.md)에 있다. physics/audio/managed의 결과는 실제 명령을 수행한 뒤 후속 작업 기록과 루트 TESTING에 추가한다. source에서 읽은 버전·기능과 실행 통과를 같은 상태로 표기하지 않는다.

## P03.1 importer dependency 검토

`daedalus-content`는 gltf1.4.1의 default features를 끄고 `utils`만 켠다. 범용 glTF import feature의 image·base64·URL decoder를 추가하지 않고, 외부 URI·embedded PNG·이미지 한도는 엔진의 공통 resolver/PNG cooker로 처리한다. hound3.5.1은 WAV sample decode를 맡고 엔진이 frame/channel/finite sample·aggregate 한도를 검사한다. 정확한 실제 feature와 전이 graph는 manifest와 Cargo.lock이 기준이다.

이번 lock 변경의 신규 package는 gltf·gltf-json·gltf-derive1.4.1, hound3.5.1, inflections1.1.1, lazy_static1.5.1이다. 로컬 registry에 내려받은 manifest와 원문 license 파일을 확인했다. gltf 세 crate와 lazy_static은 MIT OR Apache-2.0, hound는 Apache-2.0, inflections는 MIT다. hound의 원문 파일명은 소문자 `license`이며 기존 notice collector의 대소문자 정규화 검색 대상이다. 따라서 파일명만 보고 license override를 추가하지 않는다.

새 graph의 첫 `cargo check -p daedalus-content --lib`는 통과했다. 이 시점의 단위/통합·strict Clippy·Wasm·실제 공개 에셋·배포 notice 검사는 진행 중이다. 최종 결과는 [P03.1 작업 기록](../06-worklog/2026-10-16-asset-import.md)에 따로 남긴다. 이 단일 library compile을 workspace·브라우저·runtime consumer 호환 증거로 확장하지 않는다.
