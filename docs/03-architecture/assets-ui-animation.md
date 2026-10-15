# 에셋·UI·제작 파이프라인

상태: 엔진 기능 설계 제안. 의존성·포맷·구현 미선정 · 2026-10-15

## DCC에서 런타임까지

`원본 에셋 → import 검증 → 중간 표현 → 플랫폼별 cook → hash manifest → 비동기 load/decode → GPU/오디오 준비 → 사용 가능`

원본과 runtime 산출물을 구분한다. runtime이 매번 Blender 파일을 직접 해석하거나 texture를 즉석 압축하는 전제를 두지 않는다. 포맷 후보마다 좌표계·단위·색공간·normal·tangent·skeleton·animation·재질·mipmap·texture 압축·dependency hash를 기록한다.

개별 파일만 감시하지 않고 의존성을 추적한다. 재질·texture·skeleton·shader가 바뀌면 영향받는 asset generation을 새로 준비한다. 새 자원이 준비되기 전에 이전 자원을 파괴하지 않는다. 오류 시 마지막 정상 자원과 진단을 유지하는 안이다.

## 공개 에셋 사용

사용자는 공개 에셋 활용을 허용했다. 다만 공개 다운로드 가능 여부와 저장소에 재배포할 권리는 다를 수 있으므로 에셋별 원출처·작성자·라이선스·버전/hash·수정·재배포 범위를 manifest에 남긴다. 원본 엔진의 MIT OR Apache-2.0을 외부 에셋에 자동 적용하지 않는다.

현재 대용량 에셋을 다운로드하거나 샘플 콘텐츠를 추가하지 않는다. 이후 최소 엔진 fixture에 에셋이 필요하면 목적·크기·출처·대체 가능성을 먼저 명세한다. 외부 리소스를 선택하는 조사와 엔진 저장소에 넣는 결정을 구분한다.

## UI는 레이아웃 이상이다

Taffy는 레이아웃 계산 후보다. 완성 UI에는 텍스트·입력·포커스·클리핑·렌더·접근성 등이 따로 필요하다. [Taffy 공식 소개](https://github.com/DioxusLabs/taffy)

| 계층 | 엔진 책임 |
| --- | --- |
| View | 언어 중립 node schema·stable key·상태·재조정·수명 |
| Layout | Flexbox/Grid 후보·크기 제약·DPI·측정·incremental 갱신 |
| Text | shaping·한글·fallback font·bidi·줄바꿈·glyph atlas·선택 |
| Input | hit testing·pointer capture·focus·IME·clipboard·gamepad·touch |
| Behavior | 버튼·스크롤·가상 리스트·modal·탐색·실행 결과 |
| Render | 배칭·clip/scissor·text·image·custom material·alpha·layer |
| Accessibility | 의미 정보와 OS/web 접근성 접점. 상세 지원 수준 후속 |

C# 선택에 따라 view schema와 C# 선언 API를 기본 작성 경로로 설계한다. Rust 게임플레이도 같은 view 의미를 사용할 수 있다. 구체 API·생성 도구는 미결이며 TSX·React runtime을 필수 의존성으로 추가하지 않는다. [SDK](gameplay-and-mod-sdk.md)

## 로딩 중 상호작용 계약

로딩 화면의 presentation 상태를 simulation과 분리한다. 진행·취소·메뉴·설정·채팅 등 필요한 동작을 사용자 시나리오로 고르고, 해당 명령의 수락·취소 가능 단계·오류를 표시한다. 아직 초기화하지 않은 world를 UI callback이 직접 읽지 않는다.

최소 UI 자원과 글꼴은 큰 월드 자산에 의존하지 않는 bootstrap 묶음으로 준비하는 안이다. UI layout·text shaping·glyph upload도 유한 예산으로 처리한다. background worker만 옮겨 놓고 UI에서 자원 완료를 기다리는 구조를 피한다.

## VFX와 애니메이션 연결

GPU 파티클·shader effect는 렌더 자원 수명과 예산을 따른다. Hanabi는 Bevy용 플러그인이므로 독립 엔진 연결 비용을 검토하며, 설계 참고 또는 별도 wgpu 모듈과 비교한다. [Hanabi](https://github.com/djeedai/bevy_hanabi)

스켈레탈·sprite·Spine·ozz 후보는 [물리·액션·애니메이션](physics-action-audio.md) 계약으로 연결한다. Spine runtime 조건은 채택 전에 별도로 확인한다. [Spine 조건](https://en.esotericsoftware.com/spine-runtimes-license)

## 검증과 남은 설계

한글 조합·붙여넣기·포커스 전환·게임패드 탐색·다양한 DPI·픽셀/HD·긴 목록·창 resize·로딩 취소·폰트 실패·반복 reload를 시험한다. UI 스크린샷과 실제 입력 시험을 함께 두며 둘 중 하나로 전체 지원을 선언하지 않는다.

후속 문서에는 asset format/cook, 텍스트 스택, 위젯 상태·접근성, view schema, 임포트 오류 모델, shader/material API를 각각 상세화한다.
