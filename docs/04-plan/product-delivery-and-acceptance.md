# 엔진 제품과 3D·2D 게임의 최종 인수 계약

상태: 전체 목표 진행 중 · 기존 완료 보고 정정 · 2026-10-16 · A-040/A-041

## 이 문서를 만든 이유

사용자의 목표는 AI와 함께 게임을 완성할 수 있는 엔진이다. 단일 청크·이동하는 상자·독립 backend 시험·CLI package는 그 목표의 일부 근거일 뿐이다. 조정자가 native live checkpoint 종료를 전체 목표 종료처럼 보고한 것을 정정하고, 무엇을 실제로 납품하고 검증해야 하는지 고정한다.

기존 기준선 `bbccc56`은 기반 구현이며 후속 P01.1 결과는 [준비도 기록](../06-worklog/2026-10-16-product-readiness-audit.md)에 추가한다. **사용 가능한 엔진 제품 인수는 미통과이며 전체 목표는 진행 중이다.** 과거 실험과 테스트 결과는 보존하되 상위 게이트 통과로 승격하지 않는다. 테스트 개수·PR 병합·문서 작성·의존성 도입을 제품 완료율로 환산하지 않는다.

## 사용자 목표와 순서

| 단계 | 납품물 | 다음 단계로 넘어가는 조건 | 현재 |
| --- | --- | --- | --- |
| D1 | AI-driven 제작에 사용할 수 있는 Daedalus 엔진·SDK·도구·배포 | 아래 엔진 기능 행 전체가 외부 프로젝트와 공통 제작 루프로 인수됨 | 미통과 |
| D2 | D1의 공개 엔진으로 제작한 Minecraft급 3D 게임 | 기능·시각·조작·멀티플레이·지속 월드·AI 제작 증거 인수 | 미착수, D1 대기 |
| D3 | 동일 엔진으로 제작한 Terraria급 2D 게임 | 2D 기능·시각·조작·멀티플레이·시뮬레이션·AI 제작 증거 인수 | 미착수, D2 대기 |

엔진 repository에는 엔진·SDK·도구·작은 계약 시험만 커밋한다. D2/D3는 별도 게임 프로젝트로 만든다. 이전의 “지금 샘플 게임을 시작하지 않는다”는 지시를 영구적인 게임 제작 금지로 해석하지 않는다. 사용자가 이번에 두 게임 제작을 명시했으나 D1이 먼저다.

D1을 검증하는 외부 consumer 시험은 disposable engine integration test다. 전용 게임 로직을 엔진 CLI에 넣거나 하드코딩한 fixture 실행을 외부 프로젝트 지원으로 보고하지 않는다. D2/D3 제작 중 엔진 결함이 발견되면 관련 D1 게이트를 재개방해 수정·회귀 검증한 뒤 계속한다.

## 엔진 인수 표

아래의 “필수 증거”는 CLI/MCP/SDK의 공개 경로로 얻는다. AI가 엔진 내부 파일을 게임마다 고쳐야 한다면 해당 제작 기능은 미통과다.

| ID | 엔진이 제공할 제작 기능 | D1 필수 증거 | D2 3D 사용 | D3 2D 사용 |
| --- | --- | --- | --- | --- |
| P01 | 프로젝트·설정·scene·entity 계층·prefab·참조·직렬화 | 외부 폴더 생성→검증→실행→수정→재실행, stable ID·오류 위치·복구 | 월드·캐릭터·배치 | 맵·캐릭터·레이어 |
| P02 | 공통 control service·CLI·실제 MCP·오토파일럿 | 동일 session에서 입력→조건 대기→상태/UI 조회→해당 tick/frame capture→assert→재현 | 이동·채굴·메뉴·네트워크 | 이동·채굴·메뉴·시뮬레이션 |
| P03 | 에셋 import/cook/load·dependency·incremental 갱신 | 외부 texture/model/sprite/audio/animation의 유효·손상·변경·취소·이전 정상 세대 유지 | 텍스처 블록·모델·캐릭터 | atlas·sprite·타일·본/프레임 |
| P04 | C# gameplay SDK·build/watch·상태 보존 reload·Rust build 경로 | 실제 외부 게임 module의 입력·world·UI·물리·audio 사용과 코드 오류 rollback | 3D 게임 로직 | 2D 게임 로직 |
| P05 | 3D/2D renderer·재질·조명·shadow·post·VFX·camera | 실제 에셋, 움직임·투명·DPI·pixel/HD·효과·다양한 재질의 영상/상태/성능 근거 | 복셀과 일반 mesh·skin | sprite/tile·레이어·2D 조명 |
| P06 | 3D/2D physics·character·query·trigger·action | ECS/SDK 연결, 충돌·계단/경사·고속 이동·타일/청크 변경과 충돌 갱신 | 캐릭터·블록·투사체 | 플랫폼·타일·투사체 |
| P07 | audio import/stream·mix·bus·2D/3D 공간화·lifetime | 실제 출력/정지/루프/볼륨, 상태와 mixer/offline 검증, scene unload/reload 후 회수 | 채굴·발소리·환경·음악 | 채굴·전투·환경·음악 |
| P08 | UI layout·텍스트·한글 IME·focus·widget·입력·스크롤 | 메뉴·HUD·인벤토리·설정·채팅·tooltip, 로딩 중 취소/조작, AI UI tree 조회 | 메뉴·hotbar·inventory | 메뉴·hotbar·inventory |
| P09 | animation·상태 전이·blend·events·action 연계 | 실제 3D skeletal·2D sprite/본, 이동/공격·중단·reload·소유권 검증 | 캐릭터·도구·적 | 캐릭터·도구·적 |
| P10 | authority dedicated server·replication·prediction/interpolation·AOI | 2 interactive client와 부하 client를 구분한 약 30명 시험, 손실/중복/역순·재접속·권한 거부 | 이동·설치/파괴·전투 | 이동·설치/파괴·유체/전투 |
| P11 | 저장·버전·영속화·복구 | world/entity/player 설정 저장→종료/장애→복원, 수정분·schema·손상 진단 | seed·block delta·player | seed·tile/fluid delta·player |
| P12 | 3D world 생성·streaming·상주 예산·light/collider 일치 | 실제 이동 기반 다중 청크 load/unload·수정·취소·priority·GPU/CPU memory, 일반 3D partition | 긴 이동·채굴·재입장 | 공통 streaming 기반 재사용 |
| P13 | 2D 파괴 지형·fluid·light·active region | 경계·보존·광원 제거·편집·streaming·복제·저장까지 같은 revision 추적 | 공통 데이터 기반 | 유체·빛·파괴·지하/지상 |
| P14 | navigation·행동·입력/action·camera 정책 | 변경 지형의 query·취소·stale·agent 이동, 실제 정상 입력 경로 재현 | 3D agent | 플랫폼 이동/jump 연결 |
| P15 | editorless DCC·scene/terrain/cutscene 제작 | AI가 읽는 schema·patch·검증·DCC export/import·timeline 재생, 런타임 viewer | 배치·terrain·연출 | 배치·타일·연출 |
| P16 | 배포·version·macOS/web·품질/안정성 | 깨끗한 환경의 native package·HTML/Wasm export·공통 API/저장 의미·진단·장시간 시험 | 두 target에 게임 실행 | 두 target에 게임 실행 |

미구현 기능을 이름만 있는 MCP tool·빈 interface·가짜 응답·검은 capture로 통과시키지 않는다. 서로 다른 부품의 개별 시험이 성공해도 P행의 제작 경로가 연결되지 않았다면 그 행은 미통과다. Windows/mobile/console 장기 플랫폼 요구도 유지하며 실제 지원 표를 별도로 관리한다.

## 두 게임의 인수 범위

블록/타일을 보여주는 시연으로 끝내지 않는다. 각각의 게임에 시작/접속/설정 메뉴, 캐릭터 조작과 camera, world 생성·streaming·게임 내 편집, inventory/hotbar·획득·crafting·사용, 전투/행동·적/이동, animation, 질감·조명·효과, 효과음·음악, server/client, 저장/복원·재접속, 빌드/배포가 실제로 이어져야 한다. 2D는 파괴·유체·빛의 상호작용, 3D는 청크·블록과 일반 모델의 조합까지 확인한다.

게임의 기능은 공개 엔진 API·게임 코드·scene/asset 데이터로 작성한다. 공통 엔진 기능을 게임마다 다시 만들거나 게임 코드에 별도 렌더러·UI·네트워크·물리 엔진을 심어 인수표를 우회하지 않는다. 두 게임이 서로 다른 장르/차원이어도 같은 엔진 기능을 소비하는지 추적한다.

참조 게임에 가까운 품질이라는 요구는 유지한다. 이름만 비슷한 저품질 clone을 합격으로 정하지 않는다. 인터랙티브 플레이, 시간적 안정성을 보는 영상, reference 비교 카드, 기능별 기대/실제 동작을 함께 검수한다. 아직 정하지 않은 콘텐츠 총량·참조 버전·내부 해상도·성능 부하 카드는 작성자의 임의 축소로 확정하지 않고 별도 승인 전에는 품질 게이트를 열어둔다.

외부 에셋/텍스처는 원출처·작성자·license·hash·수정 내용을 기록하고 엔진 원본 license와 분리한다. 이 단계에서 특정 상용 게임의 원본 asset을 가져오는 것으로 가정하지 않는다.

## 워터폴과 완료 보고

상위 D1→D2→D3 순서는 고정한다. D1 내부 진행은 [워터폴 계획](waterfall-masterplan.md)의 미통과를 먼저 해결하며, product 경로에 필요한 선행 계약과 실제 기능을 통과시킨다. 검증 fixture가 P행을 대표하는지와 외부 consumer가 사용할 수 있는지를 따로 확인한다.

진행 보고는 `전체 목표: 진행 중 / 현재 D단계와 미통과 행 / 이번 변경·실제 검증 / 다음 의존 작업`으로 적는다. 완료라는 표현에는 반드시 대상을 붙이고, 전체 목표를 닫는 조건은 D1·D2·D3 인수 및 근거가 모두 충족된 경우뿐이다. gate 실패를 해결하지 않은 채 다음 단계 제작에 들어가지 않는다.

개발 반복 작업의 공통화·자동화·효과 검증은 [개발 반복 파이프라인](development-iteration-pipeline.md)에 기록한다. 속도 개선을 이유로 필수 기능·시험·제품 인수 범위를 줄이지 않는다.
