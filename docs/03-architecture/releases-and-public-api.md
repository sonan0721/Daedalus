# 엔진 공개 API·릴리스·웹 내보내기

상태: CLI 로컬 빌드·후속 태그 자동 배포 사용자 확정. 나머지 배포 설계·릴리스 미발행 · 2026-10-15

## 배포할 대상

이 저장소는 엔진 소스·문서·도구·테스트만 포함한다. 장기 산출물 후보는 Rust 엔진 패키지·게임플레이 등록 API, 개발자 CLI, C# SDK·분석/생성 도구, native runtime/package 도구, web export template·HTML/JS/Wasm·cooked asset manifest, GPU 없는 dedicated server이다.

현재 게임 프로젝트나 export 파일을 생성하지 않는다. 향후 엔진이 만든 사용자의 게임 산출물과 엔진 자체 릴리스도 구분한다.

사용자가 CLI를 로컬에서 빌드·버전 관리하고 향후 release tag가 GitHub Release 배포를 유발하도록 요청했다. [CLI 빌드·릴리스 계약](cli-build-and-release.md)에 따라 구성하며, 현재 태그 생성·공개 발행은 하지 않는다.

## 버전 축

| 버전 | 호환성 질문 |
| --- | --- |
| Engine release | 소스·빌드·공개 API의 호환 수준은? |
| Script API | module이 요구하는 타입·command·행동 의미가 같은가? |
| Native bridge/SDK | C ABI·schema·CPU/OS와 빌드형 Rust crate API의 호환 조건은? |
| Asset cook | 기존 변환 자원을 재사용할 수 있는가? |
| World schema | 과거 저장을 읽고 migration 가능한가? |
| Network protocol | 해당 client/server가 같은 세션에 들어갈 수 있는가? |
| Content/generator | 같은 seed·asset ID가 같은 내용을 뜻하는가? |

CLI는 crate SemVer와 일치하는 `v<version>` 태그를 초기 규칙으로 쓴다. 나머지 API 축·실험 API 표시·deprecation 기간은 별도 결정으로 유지한다. 아직 안정 API·하위 호환 보장을 선언하거나 정식 릴리스를 발행하지 않는다.

## 웹 export의 사용자 경험

목표는 Unity식 웹 빌드처럼 HTML·JS·Wasm·에셋을 쉽게 내보내는 것이다. export command가 capability 요구·worker 파일·asset base URL·cache hash·필수 HTTP 조건을 manifest와 짧은 가이드로 함께 출력하는 안이다. 별도 호스팅 서비스 구축이 목적은 아니다.

멀티스레드 build가 필요한 cross-origin isolation·MIME·압축·캐시 조건을 사전 검사하고, 지원하지 않는 환경은 진단한다. “파일이 만들어졌다”와 “해당 브라우저·서버에서 실행 검증됐다”를 구분한다. local preview·자동 브라우저 smoke·깨끗한 캐시 첫 로드를 인수 절차에 포함한다.

웹·설치형은 기능·저장 의미·공식 script API를 유지한다. 화질 설정·내부 해상도·그림자·effect 비용은 기기별 profile로 조정할 수 있다. profile 변경이 gameplay 유체·충돌·서버 권한 의미를 바꾸지 않게 한다.

PC 다운로드형 C# 모드, Rust 게임플레이의 재빌드, 웹에 포함되는 게임 코드의 배포는 다른 산출물 계약이다. PC 동적 모드 host에 Native AOT를 기본 적용하지 않는다. 정확한 runtime 동봉·servicing·SDK patch·서명은 후속 BOM/패키징 조사로 결정한다. [SDK 상세](gameplay-and-mod-sdk.md)

## 공개 API와 기여

engine-internal 타입과 안정화된 public API를 구분하고 변경마다 영향·migration·검증을 기록한다. Rust 라이브러리 버전 변경이 모든 script·저장·network 버전을 무조건 바꾸지 않도록 명시적 계약을 둔다. 외부 기여는 [기여 규칙](../../CONTRIBUTING.md)과 ADR·goal·requirement로 추적한다.

릴리스 체크에는 라이선스·의존성 BOM·재현 가능한 명령·테스트 근거·플랫폼별 실제 상태·변경 내역·artifact hash를 포함한다. private SDK·외부 에셋을 엔진 라이선스로 잘못 배포하지 않는다. 콘솔 산출물은 접근 가능한 환경과 벤더 조건을 확인한 후 별도 경로로 다룬다.
