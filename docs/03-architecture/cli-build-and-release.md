# Daedalus CLI 로컬 빌드·태그 릴리스

상태: 로컬 package·native PR dry build 확인, 태그 publish 미실행 · 2026-10-16

## 사용자 요구

엔진의 주 진입점인 `daedalus` CLI를 실제 배포 산출물로 관리한다. 지금은 로컬에서 빌드·버전·실행 파일을 관리하고, 후속 릴리스 태그 push가 GitHub Release 빌드와 업로드를 유발하게 한다. 이 작업에서는 태그를 생성하거나 공개 릴리스를 발행하지 않는다. CI build artifact와 공개 Release asset도 구분한다.

## 빌드 산출물

초기 배포 단위는 타깃별 native CLI 실행 파일, 원본 엔진 라이선스, 사용 안내, JSON build manifest, archive·SHA-256이다. 최초 검증 범위는 이 Mac의 `aarch64-apple-darwin`이다. GitHub 빌드는 macOS-arm64·Linux-x64부터 실제 빌드/실행 상태를 검증하고 확대한다. Windows·web·mobile 산출물 이름만 만들어 지원으로 표시하지 않는다.

Rust 의존성의 package ID·버전·license 선언·source checksum과 배포 package 안의 license/notice 텍스트를 inventory와 함께 모은다. metadata의 CLI 도달 graph와 feature/target 조건을 기록하며, 보수적인 포함 목록을 실제 binary symbol 수준 SBOM이라고 부르지 않는다. 빠진 license 원문은 공개 tag package에서 명시적 오류로 처리하고, arbitrary 사용자 경로를 자동 수집하지 않는다. 자동 수집이 모든 법적 의무 검토를 대신한다고 주장하지 않는다.

2026-10-16 보완: 고정된 배포 crate와 upstream commit 모두 별도 license 파일 없이 Cargo의 표준 SPDX 선언만 제공하는 경우가 있다. Cargo는 `license`를 package가 배포되는 라이선스의 SPDX expression으로 정의한다. 이런 확인된 package는 checksum·선언 파일의 pinned URL·SPDX 식별자를 묶어 명시적으로 등록하고, 고정한 공식 SPDX license-list-data의 해당 reference text를 함께 제공할 수 있다. 이를 `declared_spdx_reference`로 표시해 upstream 원문과 구분하며 author/year/copyright holder를 만들어 채우지 않는다. 임의 expression의 자동 추정·미등록 누락·버전 변경은 여전히 오류다. 이는 알려진 선언을 보존하는 packaging 정책이며 법적 적합성 보증이 아니다. [Cargo license 의미](https://doc.rust-lang.org/cargo/reference/manifest.html#the-license-and-license-file-fields), [공식 SPDX 목록](https://github.com/spdx/license-list-data)

기본 CPU 제어 경로와 capture/window feature를 manifest에 기록한다. native live checkpoint의 `window` feature에 imported audio consumer checkpoint부터 `native-audio`를 추가한다. 실제 package feature는 `native-audio,window`이며 capture·renderer·offline mixer·실제 CPAL backend를 포함한다. Linux host에는 `libasound.so.2`가 필요하고 archive는 ALSA를 bundle하지 않는다. 두 native CI package는 `--assets --audio`의 공통 smoke를 실행하고 Linux linkage를 별도로 확인한다. `daedalus view` 실행·GPU 부재의 오류 계약을 유지하되 CI의 archive smoke는 display가 필요 없는 CPU 경로로 수행한다. 실행 파일 하나를 배포했다고 C# SDK·CoreCLR runtime·외부 에셋·게임 export가 모두 포함됐다고 하지 않는다. 초기 archive의 실제 포함 범위를 사용 안내에 명시한다.

### Imported audio 의존성의 curated 원문 보완

P03.2c의 strict package는 새 의존성10개의 license 원문 누락으로 중단했다. 고정된 objc2 upstream `LICENSE.md`는 MIT-only crate와 `Zlib OR Apache-2.0 OR MIT` crate를 구분하지만 표준 약관 전문을 담지는 않는다. 이 실제 upstream scope 파일을 exact package/VCS/checksum으로 보존하고, 그 선언에 해당하는 canonical 약관은 기존 고정 SPDX commit의 reference text로 명확히 구분해 함께 싣는다. package-specific copyright holder를 추정해 채우지 않는다. upstream scope에 적힌 SDK 관련 설명도 원문과 함께 보존하며 이 파일 수집을 모든 배포 조건의 검토 완료로 표시하지 않는다.

현재 collector는 supplemental reference에도 전체 expression의 identifier 검증을 수행하므로 MIT reference만 붙여 Zlib 제한을 우회할 수 없다. 이번 구현은 승인된 identifier set에 **Zlib 한 개만 명시적으로 추가**하고 기존 pinned `spdx/license-list-data` commit `31ba1a50e5397e00a304dbadc76531740e89ee48`의 `text/Zlib.txt` bytes/hash를 확인한다. 알 수 없는 identifier·변경된 version/checksum/path·hash 불일치는 계속 오류다. `declared_spdx_reference`로 upstream scope를 지우거나 arbitrary expression 자동 허용으로 바꾸지 않는다. 원문과 reference의 provenance·선언식 일치·triple expression/unknown 거부 회귀와 두 target strict collection을 검증한 뒤 package를 다시 만든다.

## 버전과 로컬 관리

`daedalus --version`은 실제 crate SemVer를 표시한다. build manifest에는 schema version, CLI version, exact Git commit, dirty 여부, target triple, Cargo profile, feature 목록, Rust toolchain, binary SHA-256, build 식별자를 남긴다. 조회하지 못한 값은 임의로 채우지 않는다. build 전후 HEAD를 비교해 변경 시 중단하며 dirty 여부는 양쪽 결과를 합친다. dirty 빌드는 로컬에서 허용하되 공개 릴리스 후보와 구분하고 tag 후보는 build 후에도 clean이어야 한다. 이는 source snapshot이나 byte-for-byte 재현 빌드 증명과 다르다.

로컬 packaging 명령은 `--locked`로 native release build를 수행하고 격리된 ignored 디렉터리 아래 산출물을 보관한다. 여러 빌드를 식별할 수 있게 이름·manifest를 연결하고 기존 산출물을 조용히 덮어쓰지 않는다. archive entry와 manifest에는 개인 경로를 쓰지 않고 프로젝트 source·로그·토큰·임의 사용자 파일을 자동 포함하지 않는다. 빌드물은 Git에 커밋하지 않는다.

배포용 Rust compiler 경로는 `--remap-path-prefix`로 repository·Cargo/Rustup cache·build home을 중립 prefix에 대응시킨다. 사용자의 encoded Rust flags 우선순위와 공백 구분 `RUSTFLAGS` 의미를 보존해 build subprocess에만 적용하고 실제 개인 경로는 manifest에 쓰지 않는다. 이 방식은 Cargo config의 `rustflags`보다 환경 변수를 우선하므로 별도 config flags가 필요한 사용자는 environment로 전달한다. compiler 생성 경로를 정규화하는 범위이며 외부 도구·임의 문자열·서명 정보까지 전부 제거한다는 보장은 아니다. [Rust 경로 remapping](https://doc.rust-lang.org/rustc/remap-source-paths.html), [Cargo flags 우선순위](https://doc.rust-lang.org/cargo/reference/config.html#buildrustflags)

archive에서 꺼낸 실행 파일로 `--version`과 CPU inspect smoke를 실행한다. macOS 실행 권한도 보존한다. SHA-256은 무결성 비교이며 code signing/notarization을 대신하지 않는다. 초기 unsigned 배포와 서명된 사용자 배포의 품질 게이트를 구분한다. SDK/framework-dependent host의 부속 packaging은 E04 결과와 연결해 후속 확장한다.

## 태그와 GitHub Release

초기 tag convention은 `v<CLI SemVer>`다. 정상 예시는 `v0.1.0`이며 prerelease 예시는 `v0.1.0-alpha.1`이다. tag의 version은 실제 CLI manifest version과 일치해야 한다. 0.x 버전의 API는 아직 안정화되지 않았으며 버전 문자열만으로 전체 엔진 품질 완료를 뜻하지 않는다.

태그 workflow는 source commit이 main 이력에 있는지와 version 일치를 검사한 뒤 `--locked` build·CPU smoke·packaging을 수행한다. 모든 대상의 성공 뒤 publish job 하나가 기존 tag를 검증해 archive·manifest·checksum을 업로드한다. prerelease tag는 prerelease로 표시한다. 실패한 플랫폼을 조용히 빼고 완전한 릴리스를 발행하지 않는다. 기존 Release asset을 자동 덮어쓰지 않는다.

PR 또는 수동 build 검증은 읽기 권한으로만 수행하며 공개 Release를 만들지 않는다. `contents: write`는 tag publish job에만 부여한다. Action은 공식 저장소의 실제 tag→commit을 확인해 SHA로 고정한다. 외부 PR 코드를 publish 권한으로 실행하는 `pull_request_target` 경로를 두지 않는다.

## 검증과 제한

1. 로컬 package 생성·manifest/실행 파일 version 일치·SHA-256 재계산·깨끗한 디렉터리에서 archive 추출/실행.
2. malformed tag·version 불일치·기존 출력·실패한 build·잘못된 target를 명시적 오류로 처리.
3. PR dry build에서 macOS/Linux archive를 생성하고 추출한 CLI를 실행. 이것은 tag 발행 성공의 증거가 아니다.
4. 최초 실제 태그 발행 시에만 Release asset의 플랫폼·checksum·다운로드 후 실행까지 기록한다. 현재 공개 Release는 만들지 않는다.

동일 소스를 동일 명령으로 다시 빌드하는 절차와 byte-for-byte reproducible build 증명은 다르다. 처음부터 후자를 달성했다고 표시하지 않는다. 자동 업데이트·패키지 매니저·Homebrew tap·코드 서명은 이 초기 작업의 완료 주장에 포함하지 않는다.

실제 실행 결과와 미검증 범위는 [E08 작업 기록](../06-worklog/2026-10-16-cli-packaging.md)에 남긴다.

근거: [GitHub push 이벤트](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows#push), [hosted runner 목록](https://docs.github.com/en/actions/reference/runners/github-hosted-runners), [gh release create](https://cli.github.com/manual/gh_release_create), [Cargo profiles](https://doc.rust-lang.org/cargo/reference/profiles.html). 조회일2026-10-15. workflow와 실제 실행 로그가 최종 플랫폼 근거다.
