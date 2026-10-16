# P03.2 실제 model·texture·sprite·audio 소비

상태: a/b/c 첫 consumer와 P03.2 인수, source generation/reload·전체 P03 미통과 · 2026-10-16 · P03/DEV03/DEV05/DEV08

## 목적과 선행 조건

[P03.1](asset-import-foundation.md)의 CPU catalog를 실제 실행 경로에서 사용한다. `project cook`의 summary나 PNG decoder 성공은 화면 표시·2D·사운드 재생의 근거가 아니다. 동일 PreparedProject/catalog와 authority snapshot을 renderer/audio adapter가 소비하게 하며 CLI별 loader·geometry 복사·임시 fixture를 새로 만들지 않는다.

P03.1은 로컬 인수와 PR10 필수 CI·main 병합까지 확인했다. a 경로는 [상세 구현 계약](model-texture-runtime.md)으로 구체화했다. 아래 세 하위 경로를 각각 검증하되 P03.2는 모두 통과해야 체크한다. W단계/D1 인수와 두 게임 인수는 계속 별도다.

| 하위 경로 | 필요한 실제 결과 | 대체할 수 없는 검증 |
| --- | --- | --- |
| P03.2a static 3D | selected-scene hierarchy·여러 primitive·UV/base-color PNG·PBR factor·sampler가 실제 project packet과 GPU에 반영 | transform/material/texture 기준 픽셀·동일 geometry 공유·실제 외부 모델 표시 |
| P03.2b sprite 2D | atlas region/pivot/PPU·nearest/no-mip·투명도·2D camera/layer를 실제 project에서 사용 | 비대칭 atlas의 UV/좌우/상하·alpha 합성·겹침 순서·resize |
| P03.2c PCM audio | imported WAV가 기존 AudioService의 voice/output에 연결 | offline PCM·play/stop/voice 수명·실제 native device 출력 |

skin/animation·고급 재질·tile simulation·UI·spatial audio·web runtime은 해당 제품 영역과 후속 항목으로 남긴다. 이번 작은 subset을 엔진의 최종 기능 범위로 고정하지 않는다. source watch와 여러 candidate generation의 동시 준비/취소/last-good 교체는 P03.3이다.

## 확인한 현재 seam

2026-10-16 로컬 source 감사 기준이다. `PreparedProject`는 private field에 catalog를 추가할 수 있지만 `PreparedEntity`의 공개 field에 새 필드를 바로 추가하면 외부 struct literal을 깨뜨린다. 기존 `daedalus.mesh_renderer`는 legacy `daedalus.mesh`를 받는다. additive `daedalus.model_renderer`는 asset-aware 경로의 validated private binding map으로 연결했으며 로컬 실제 GPU/native 검증을 통과했다.

renderer의 `ProjectRenderResources::prepare`와 `extract`는 project/source identity를 고정하고 실제 session snapshot에서 transform을 가져온다. 이 경로를 확장한다. legacy `project_snapshot`/`RenderScene`·공개 `Vertex`/`MeshHandle`·기존 packet 사용의 의미는 유지한다. 기존 24-byte GPU vertex는 보존하고 모델용 32-byte position/normal/UV 경로를 추가했다. 모델 shader가 실제 sRGB texture와 primitive sampler를 사용한다.

AudioService는 이미 PCM·voice·offline/native backend를 가진다. importer를 서비스 안에 다시 만들지 않고 immutable PCM resource에서 재생 자료를 준비하는 adapter를 둔다. 실제 API/정수/수명은 각 하위 착수 전 문서의 확정 표와 구현을 대조한다.

## project binding과 호환성

schema 1의 generic component envelope 안에 additive component를 정의한다. 기존 loader의 지원 component 목록과 asset-aware loader의 목록을 구분하고, 미지원 실행 경로는 위치 있는 Unsupported를 반환한다.

- `daedalus.model_renderer`: 단일 model asset 참조. `asset_refs`와 `data.model`의 일치·중복·catalog kind를 검증한다. glTF scene 선택은 model import settings를 따른다. 첫 경로는 imported primitive material을 그대로 쓰고 entity-level material override를 받지 않는다.
- `daedalus.sprite_renderer`: atlas asset와 stable region ID를 참조한다. image asset를 다시 이름으로 추측하거나 atlas source를 재파싱하지 않는다. tint/transform/layer의 정확한 JSON·기준 좌표·정사영/actual view는 [P03.2b 상세 계약](sprite-runtime-and-orthographic-view.md)에 확정했다.
- audio: 명시적 asset 참조와 같은 actor의 play/stop 제어를 사용한다. load만으로 소리를 자동 재생하지 않는다. command·volume/pan·loop·voice 조회·유한 replay·오류와 native 종료의 실제 경계는 [P03.2c 상세 계약](imported-audio-runtime.md)에 확정했으며 CPU/native/clean archive 검증 뒤 CI/main 인수를 기다린다.

새 binding은 private PreparedProject의 별도 validated collection/accessor로 제공하는 방향이다. PreparedEntity를 공개 필드 추가로 깨거나 model 참조를 legacy mesh ID에 숨기지 않는다. 같은 entity에 서로 충돌하는 renderer component를 달았을 때의 오류와 source/JSON path를 명시한다. 현재 root entity 제약과 glTF 내부 node hierarchy는 다른 계층이며, model node 지원을 일반 scene hierarchy 지원으로 홍보하지 않는다.

## model 확장과 authority transform

prepare 시 selected-scene roots에서 node hierarchy를 따라 draw를 열거한다. glTF의 global primitive index는 같은 mesh를 여러 node가 참조하면 반복되므로 instance ID로 단독 사용하지 않는다. 각 `(node index, global primitive index)` 발생에 deterministic draw ordinal을 부여하고 generation 안에서 고정한다. `RenderInstanceId`는 실제 EntityHandle와 이 ordinal을 사용한다. node/primitive 개수와 expanded draw 한도를 allocation 전에 검사한다.

프레임의 최종 matrix는 `authority entity world × glTF ancestor/local matrices`다. entity transform을 원본 scene이나 CLI report에서 다시 가져오지 않는다. node hierarchy의 full affine matrix를 보존하고, 최종 matrix의 inverse-transpose로 normal을 변환한다. nonuniform scale·부모 회전·여러 node 공유를 검증한다. nonfinite/singular transform과 normal 계산 불능은 진단한다. [상세 계약](model-texture-runtime.md)대로 determinant가 정확히0이거나 nonfinite이면 거부하고 임의 near-zero epsilon은 사용하지 않는다. inverse/최종 matrix도 finite여야 한다.

현재 pipeline은 cull_mode None이다. mirrored transform을 지원할 때 이 상태의 픽셀을 검증하며, 나중에 back-face culling을 켜면 determinant sign과 winding/front-face를 함께 처리해야 한다. 무조건 인덱스를 뒤집어 공유 geometry를 node별로 복제하지 않는다.

geometry·imported material·image는 immutable handle로 공유하고 per-draw transform/material binding만 추출한다. 매 frame accessor decode·UV 변환·geometry hash·GPU buffer 생성을 반복하지 않는다. snapshot-only 변경이 node mapping/geometry를 다시 준비하지 않음을 counter로 확인한다.

## UV·색공간·재질·sampler

position/normal/UV0가 있는 명시 layout version을 additive resource/packet 경로로 제공한다. 기존 24-byte vertex의 암묵적인 reinterpretation을 금지한다. layout과 byte count는 GPU resource key·검증·예산에 포함한다. UV 없는 untextured model은 명시적인 shader path로 처리하고 textured material의 UV 누락은 importer 오류다.

base-color image는 sRGB texture로 sample하고 linear 영역에서 base-color factor를 곱해 기존 PBR lighting을 수행한다. 현재 target의 manual sRGB encode와 겹쳐 두 번 encode하지 않는다. UV origin은 glTF 규격과 wgpu texture convention을 맞춘 비대칭 four-color 입력으로 검증하며, import와 shader에서 각각 뒤집지 않는다.

glTF의 실제 sampler는 `ModelTextureRef::sampler()`가 기준이다. owner image의 공통 mip cook policy를 primitive sampler로 오인하지 않는다. 동일 image라도 두 material의 nearest/linear·repeat/mirrored/clamp·mipmap 설정은 각각 유지한다. GPU texture residency는 image/layout/device identity로 공유하고 sampler cache는 실제 sampler descriptor로 공유한다. 모든 cooked mip의 dimensions/offset/bytes와 upload 순서를 검사한다.

첫 3D material은 P03.1 opaque subset을 그대로 소비한다. alpha·normal/data map·emissive·custom material은 검증 없이 대체하지 않는다. 2D sprite는 별도 unlit alpha 합성 경로가 필요하며 3D opaque shader를 그대로 써서 완료 처리하지 않는다.

## 자원 한도와 수명

geometry cache와 별도의 무제한 image/sampler cache를 만들지 않는다. 기존 packet geometry preferred64/hard128 MiB에 texture·mip·sampler/bind-group·instance/material metadata를 합산하는 device admission을 정의한다. numeric 기본값·active/retired entry 상한·별도 staging128 MiB·최대 in-flight submission3은 [상세 계약](model-texture-runtime.md)에 확정했고 로컬 검증했다. CPU import의128 MiB 한도를 GPU 전체 한도로 혼동하지 않는다.

비용 계산은 checked arithmetic으로 allocation/create/write 전에 수행한다. 장치 세대와 layout을 resource identity에 포함하고, 이전 submission의 실제 완료 전 retired resource를 재사용·회수하지 않는다. queue submit 성공을 GPU 완료로 취급하지 않는다. 후보가 예산을 넘거나 pinned resource 때문에 들어갈 수 없으면 명시적으로 Pending/Busy/Rejected를 반환한다. 현재 정상 frame을 먼저 파괴하거나 required texture를 흰색 fallback으로 숨기지 않는다.

P03.2의 최초 batch load와 P03.3의 실행 중 streaming을 구분한다. 일반 redraw/fixed tick에서 파일 I/O·PNG/WAV decode를 호출하지 않는다. render-thread의 장치 작업도 측정·예산 대상이며 이 단계만으로 zero-stall·144fps·로딩 UI 인수를 선언하지 않는다.

audio PCM은 catalog identity로 재사용하고 voice별 전체 PCM 복제를 피한다. 기존 Kira resource 변환에 실제 복사가 필요하면 1회 conversion과 byte 수를 계측한다. stop 요청과 callback/voice의 실제 종료를 나누고, 마지막 사용자가 끝나기 전 resource를 회수하지 않는다. device callback에서 파일 I/O·decode·큰 allocation을 하지 않는다.

## 공통 경로와 검증

project view와 batch capture는 같은 prepared resources/packet/shader를 사용한다. batch capture는 새 session이며 live-window capture라고 보고하지 않는다. 같은 live frame capture는 [별도 계약](live-capture-and-artifacts.md)을 따른다. CLI용 모델 loader·테스트용 sprite renderer·게임 전용 WAV player를 만들어 공개 엔진 사용 경로를 우회하지 않는다.

CPU 검사는 selected roots·공유 mesh의 여러 node·여러 primitive·matrix·stable draw ID·asset-kind 오류·합산 한도·수명을 다룬다. GPU 검사는 실제 texture/UV/sRGB·sampler·mip·정상 픽셀과 resource upload 수를 확인한다. 2D/audio는 각각의 실제 출력 증거를 별도로 남긴다. 원본 provenance는 [기록](../02-research/asset-validation-provenance.md)을 재사용하고 테스트에 외부 다운로드를 추가하지 않는다.

기존 `scripts/smoke_project.py`의 process/timeout/hash/cleanup을 확장하는 것을 우선한다. GPU/device가 필요한 mode는 opt-in으로 두고 CPU/package 기본 의미를 몰래 바꾸지 않는다. 구현 전 각 하위 계약에서 JSON schema·public API·한도·파일 소유자·명령·성공/실패 기준을 확정한다. 이 초안만으로 착수 조건이나 P03.2 체크박스를 통과시키지 않는다.

## P03.2c 착수 전 source 감사

2026-10-16 Luna xhigh read-only 감사로 imported AudioAsset은 immutable `Arc<[f32]>`, 현재 PcmClip은 `Arc<[kira::Frame]>`를 보관함을 확인했다. 현재 `from_interleaved`는 Vec를 받아 stereo frame으로 변환하므로 borrowed-slice 진입점을 추가하면 임시 `samples.to_vec()` 한 번을 피할 수 있다. mono→stereo와 Frame layout 변환 자체는 필요하며 Vec→Arc 최종화의 임시 peak도 예산에 포함해야 한다. 재생 voice는 StaticSoundData의 Frame Arc를 공유하므로 전체 PCM을 voice마다 다시 변환할 필요가 없다. 같은 AudioAsset cook key의 alias는 한 번 변환하는 bounded project bank 후보로 둔다. 현재 project bank/adapter는 구현하지 않았다.

공통 SessionControlService actor가 project/catalog와 같은 권한·owned ticket을 갖고 있으므로 별도 CLI WAV player보다 이 actor의 명시적 opt-in audio sidecar를 검토한다. 기존 SessionControlConfig literal과 device-free 기본 경로를 보존해야 한다. stop은 Kira command 요청이고 mixer가 종료를 관찰한 것과 다르다. 현재 service에 per-voice 상태 조회가 없어 stale handle·stop requested·actual ended·회수와 유한 wire ID 수명은 상세 계약에서 확정해야 한다. CPU 검증은 실제 imported WAV→bank→실제 offline Kira PCM을 사용하고 native device 출력은 별도 opt-in이다. 이 감사만으로 audio consumer나 headless/SDK 연결을 통과시키지 않는다.

추가 read-only 감사에서 protocol2 request ID의 중복 방지는 ticket 보유 중에만 유효하고 ack/forget 뒤 mapping이 지워짐을 확인했다. 현재 authority replay는 SetTransform의 CommandId에만 있다. 따라서 재시도하는 Play를 request correlation만으로 idempotent라 부르면 안 된다. 상세 계약에서 audio mutation의 replay identity/유한 보유/expiry와 permission을 정한 뒤 구현한다. actor의 offline PCM 진행도 명시적 audio 연산으로 설계해야 하며 현재 Engine Manual Step이 audio까지 진행한다고 간주하지 않는다. 결과 ticket/JSON에 긴 PCM 배열을 싣지 않고 bounded state/요약과 실제 내부 PCM 검사를 구분한다.

native Kira/CPAL은 stream 생성과 finite voice의 실제 mixer 종료 관찰로 callback 진행을 확인할 수 있다. 현재 API에는 callback counter/loopback capture가 없어 사람이 들은 소리나 출력 장치의 실제 음압을 자동 검증했다고 보고하지 않는다. 기본 startup은 계속 장치가 필요 없어야 하며 선택한 native backend의 실패를 offline 성공으로 바꾸지 않는다. 조사 당시 CLI package는 window feature만 빌드했다. 후속 audio 구현은 native-audio+window feature/manifest/notices와 Linux ALSA build/runtime 의존성을 추가했고 실제 로컬/CI 인수 상태는 상세 worklog로 추적한다. 이 항목들은 P03.2c 선행 조사이며 b 인수 전에 audio 구현을 착수하지 않는다.
