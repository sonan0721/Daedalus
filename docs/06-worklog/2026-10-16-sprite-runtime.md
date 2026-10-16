# P03.2b 스프라이트·정사영 실행 작업 기록

상태: source/로컬 CPU·Metal·native 검증 통과·clean package/CI 인수 대기 · 2026-10-16

## 두 문서를 만든 이유

[스프라이트·정사영 상세 계약](../03-architecture/sprite-runtime-and-orthographic-view.md)은 기존 atlas CPU IR을 실제 GPU draw와 authority camera에 연결하기 전에 좌표·alpha·sort·공개 API·유한 자원·실제 view 관측을 정한다. 이 작업 기록은 source 감사/설계와 구현/실측을 나눠, 작은 atlas 검사나 새 MD 자체를 2D 엔진 완성으로 보고하지 않기 위해 만든다.

선행 P03.2a는 [PR11](https://github.com/sonan0721/Daedalus/pull/11) 최종 head `770bc71`의 다섯 CI/두 packaged-assets step, clean release/extracted assets31/native-model24, main `6d1711faf306bf644bf21235c86c9a31d7208fff` 동일 tree까지 인수했다. 새 branch는 `feat/asset-sprite-consumer`다. 전체 D1→D2→D3와 P03.2는 계속 열려 있다.

## source 감사와 선택

1. Luna xhigh가 atlas IR/loader/renderer/window/authority snapshot을 read-only 감사했다. atlas는 nearest/no-mip을 이미 보장하지만 sprite binding·alpha shader·orthographic projection은 없었다.
2. 기존256/512 geometry-entry 한도와 최대16,384 atlas region을 고려해 per-region quad 대신88-byte unit quad1개와 slot당16-byte UV row를 선택했다. 기본 최대 UV storage는256KiB이며 공통 예산/retirement에 포함한다.
3. texture upload helper가 full mip chain만 받는 것을 확인했다. atlas base-only payload를 같은 texture key/cache/admission에 추가하고 별도 sprite cache/decoder를 만들지 않는다.
4. public Camera/ProjectPresentation/RenderInstance/보고 struct literal은 유지한다. opaque RenderView·actual PacketViewSubmission·PresentedView·통합 presentation outcome 하나를 추가하고 기존 API는 adapter로 남긴다. Busy fallback의 실제 packet/projection을 기록한다.
5. window가 입력 없이도 local_camera를 만들어 이후 authority camera를 가릴 수 있음을 확인했다. 실제 입력 변경 때만 local view override를 생성한다. 2D pan/zoom과 actual view JSON을 함께 검증한다.
6. PNG의 straight alpha와 pinned wgpu blend/format 원본을 확인했다. sRGB target에서 linear unlit alpha 합성, opaque 뒤 별도 depth-free sprite pass다. non-sRGB-only target의 sprite는 write 이전 typed reject로 둔다.

이 항목들은 source/설계 리뷰이며 실행 결과가 아니다. sprite 외 일반 고해상도 filtering·tile/lighting/animation/UI·source reload는 후속 계약이다.

## 분담과 검사 순서

| 소유자 | 범위 | 상태 |
| --- | --- | --- |
| Luna xhigh content | private sprite/camera2d binding·strict schema/revalidation·metadata precharge·CPU 검사 | source 고정·focused72/strict Clippy/Wasm/fmt 통과; 별도 renderer 검사 source 작성 |
| Luna xhigh renderer | unit quad/UV slots·공통 texture lifetime·alpha/projection·actual view/window·CPU/GPU 검사 | source 고정·focused49/strict Clippy·조정자 Metal9 통과 |
| Luna xhigh CLI | 기존 공통 report·opt-in sprite smoke·timeout/cleanup 회귀 | source 고정·focused36/strict Clippy/build·Python55 통과 |
| 조정자 | MD·source/API 리뷰·통합/실제 Mac·원본 QA·Git/PR | CPU264/Metal9/smoke39/원본 QA 확인·clean package/CI 인수 대기 |

content→renderer→CLI→조정자 순서로 Cargo/shared target 소유권을 넘긴다. source 파일 소유권을 분리하고 새 dependency/두 번째 cold target/새 packaging backend는 계획하지 않는다. focused→통합→hardware→clean package→최종 다섯 PR CI/main 동일 tree가 모두 필요하다. 검사 명령/입력/소스·binary identity/counter는 실제 실행 후에만 추가한다.

## 착수 checkpoint

상세 계약은 `5d1be86`에 code보다 먼저 커밋했다. Luna xhigh 세 담당자에게 content/renderer/CLI·scripts 소유권과 public accessor 계약을 전달했다. content만 Cargo owner로 focused 검사를 시작하고 renderer/CLI는 의존 API를 공유해 disjoint source 편집을 진행한다. default smoke와 CPU CI에 GPU를 강제하지 않으며 후속 실제 hardware/원본 QA/clean package와 최종 CI는 여전히 미실행이다.

공개 원본 QA 입력은 ignored `dist/qa-assets/sprite-runtime-project/`에 준비했다. Tiny Dungeon의 원본 PNG5294 bytes/SHA-256 `d24e60a41e4ac7a745c0304dfde121143688557f40215f23221c29cfe683825f`를 그대로 복사했고 atlas descriptor의16×16 region4개만 별도 작성했다. 두 atlas StableId·네 sprite와 camera-only entity로 unit quad/texture 공유 및 실제 픽셀 아트를 확인할 예정이다. 현재는 입력 준비만 했으며 새 sprite binary의 validation/render는 미실행이다.

초기 CLI source 리뷰에서 실제 view JSON을 capture artifact payload 안의 `capture.render_view`로 명확히 했다. 기존 last_presented/window camera key와 새 view의 camera serialization이 따로 늘지 않도록 공통 camera 변환 재사용을 요청했다. 후속 live capture 계약에도 실제 used projection/local zoom/Busy ready metadata 재사용을 반영했지만 해당 capture 기능은 아직 구현하지 않았다.

## content 첫 source 고정

Luna xhigh content가 private sprite/camera binding과 typed 공통 projection helper를 구현했다. 조정자는 atlas/region kind·ref/tint 오류, project-wide camera uniqueness/renderer/presentation 충돌, unit scale/range, legacy camera-only 지원과 sprite Unsupported, validate의 borrowed catalog 재구성, map/ID allocation 이전 metadata charge를 source 리뷰했다. catalog 전체 clone이나 별도 decoder는 추가하지 않았다.

담당자의 frozen source focused 결과: `cargo test -p daedalus-content --locked`72/72, all-target/all-feature strict Clippy, `cargo check -p daedalus-content --lib --target wasm32-unknown-unknown --locked`, scoped rustfmt/diff check 통과. Wasm은 library compile이며 실제 브라우저 실행 인수가 아니다. 조정자의 workspace/Metal/native sprite 통합은 아직 미실행이다.

Cargo/shared target은 content에서 renderer로 인계했다. content 담당자는 새 `sprite_packet_tests.rs` 한 파일만 source 작성해 independent alpha/UV/order/reuse 검사를 병렬 준비하고, renderer 담당자는 핵심 GPU/window와 no-input/Busy/budget 검사를 맡는다. 이 source-only 분담으로 같은 테스트 파일/컴파일 락의 중복을 피한다. 실제 소요 시간 개선율은 측정하지 않았다.

구현 중 조정자 source 리뷰에서 UV write counter 이중 합산과 configured instance cap 대신 최대치 고정 UV buffer를 지적했다. renderer 담당자는 각각 한 합산 지점·checked `max_instances*16` charge/allocation으로 수정했다고 보고했고 실제 통합 검사는 후속이다. 추가로 정적 sprite binding String clone/sort를 매 extract마다 하는 경로를 찾아 prepared generation의 Arc binding/order 공유를 계약에 반영했다. 이는 필요한 admission/authority scan까지 제거한다는 뜻이 아니다.

CLI 작성자의 전체 Python 실행에서 기존 process timeout 세 실패가 보고됐다. 원인은 아직 확정하지 않았고 log 보존·build 경합이 없는 재실행과 실제 timeout/pipe 단계 조사를 요청했다. focused sprite/report 검사는 통과했지만 전체 suite 성공으로 바꾸어 적지 않는다.

## 첫 조정자 broad와 추가 리뷰

source 고정 checkpoint에서 조정자가 `cargo test --workspace --all-features --locked`를 실행해259 CPU pass/0 fail/6hardware ignored를 확인했다. log는 ignored `dist/project-evidence/p03-sprite-workspace-tests.log`다. 이는 후속 source 보완 전 checkpoint다. CLI 담당자의 독립 post-Cargo Python discovery는55/7.309초로 통과했고 기존 timeout 검사 assertion/재시도 규칙은 변경하지 않았다. 앞선 세 실패의 원인은 확정하지 않는다.

read-only 독립 리뷰는 view submit의 typed non-sRGB early reject·shared cache/stamp/UV admission과 실제 PresentedView 갱신을 확인했다. capture는 target를 만든 뒤 format reject해 계약과 어긋나는 점, Busy fallback의 실제 projection/non-sRGB 무변경 검사가 없는 점을 찾아 renderer owner에게 보완 요청했다. hardware 결과는 아직 없다.

추가 source 감사에서 ProjectRenderResources의 prepared binding/template/order CPU metadata는 device renderer의8 MiB bookkeeping에 포함되지 않았음을 확인했다. 계약을 별도 prepared metadata8 MiB preflight·명시적 estimator/getter로 보완했다. 두 owner의 최대 reservation은 합산16 MiB이며 catalog payload/scratch와도 다르다. bounded allocation 전 borrowed precharge와 shared backing 단1회 계산을 구현/검증해야 하므로 기존259 CPU 통과만으로 새계약을 인수하지 않는다. final/current viewer view와 last-presented view 의미도 문서에서 분명히 나눴다.

Luna renderer의 위 리뷰 보완 focused 결과는45 CPU pass/5GPU ignored, all-target/all-feature strict Clippy/fmt/diff pass다. Busy regression에 실제 fallback target과 old-ready Perspective+camera override target의 readback 비교를 넣었지만 아직 hardware 실행하지 않았다. metadata source는 8 MiB admission 이전 borrowed count/key 집계와 normal/reject/shared-template 검사를 포함하며 조정자 최종 검증은 후속이다.

## 투명 PNG source 감사와 추가 계약

별도 read-only alpha 감사가 clear alpha0..1 허용·sprite straight-source blend·sRGB target·raw PNG write를 추적했다. alpha1 배경 GPU reference만으로는 투명 PNG를 검증하지 못한다. 투명 배경 위 빨강 alpha.5의 premultiplied target 약 `[188,0,0,128]`을 straight PNG로 쓰면 외부 합성 시 색이 다시 줄어드는 실제 오류다. nonblack alpha0 clear의 RGB도 잘못 기여한다. 공식 PNG §6.2/§12.3와 pinned wgpu source를 다시 확인했다.

code 전 [상세 계약](../03-architecture/sprite-runtime-and-orthographic-view.md)에 capture의 premultiplied-linear clear/target, 공통 readback의 in-place straight PNG 변환, alpha255 fast path/alpha0 정규화와 일반 native window의 불투명 정책을 추가했다. 새 target/RGBA buffer/PNG writer를 중복 만들지 않는 제작 파이프라인 선택이다. alpha0/.25/1 nonblack 배경과 overlap의 실제 GPU 검사·양자화 tolerance 및 기존 opaque 색/자원 회귀가 필요하다. 이 추가는 구현/실행 결과가 아니다.

조정자가 frozen scripts의 전체 `python3 -m unittest discover -s scripts -p 'test_*.py'`를 Cargo 실행과 겹치지 않게 독립 수행해55건/7.007초/0 fail을 확인했다. 이는 Python/프로세스 회귀 근거이며 후속 renderer source의 GPU·native 동작 근거는 아니다. 앞선 timeout 실패는 로그에서 보존하며 원인을 확정하지 않는다.

## 최종 source 고정과 조정자 CPU 검증

Luna가 private capture alpha policy와 공통 in-place PNG 변환, packet/legacy native의 opaque clear 및 Opaque surface 우선 선택을 구현했다. metadata estimator는 빈 painter Arc header도 charge하고 HashMap bucket을 checked power-of-two/load-factor로 계산한다. 56/57-entry 경계·alpha0/255·clear encoding 검사를 추가했다. 새 GPU fixture의 borrowed manifest compile 오류는 해당 Luna 담당자가 수정한 뒤 source를 고정했다.

focused CPU49건 뒤 strict Clippy가 `No space left on device`로 실패했다. `cargo clean -p daedalus-renderer`로 해당 패키지의 재생성 가능한 출력만 정리하고 다른 crate의 의존 캐시·소스·dist/QA 자료는 유지했다. 약400 MiB에서8.6 GiB 여유를 회복한 뒤 같은 target으로 CPU49·strict Clippy/fmt/diff를 통과했다. 이는 디스크 장애 복구이며 성능 개선 배율이나 전체 cache 삭제가 아니다.

Cargo handoff 뒤 조정자가 frozen 통합 source로 `cargo test --workspace --all-features --locked`를 실행해 **264 CPU pass/0 fail/10 hardware ignored**를 확인했다(compile7.67초; 전체 wall time과 구분). all-target/all-feature workspace strict Clippy와 workspace fmt/docs/diff도 통과했다. 로그는 ignored `dist/project-evidence/p03-sprite-final-workspace-tests.log`, `p03-sprite-final-clippy.log`다. 실제 Metal/native/public asset와 clean package/CI는 별도 확인한다.

첫 실제 Metal 명령 `cargo test -p daedalus-renderer --all-features --locked -- --ignored --nocapture --test-threads=1`은 library8건 중5 pass/3 fail로 중단됐다. integration GPU 시험은 이 실패 뒤 실행되지 않았다. 실패 로그를 `p03-sprite-metal-tests-first-failure.log`로 보존했다. 조정자 source 리뷰는 투명 overlap reference의 blue/red StableId 순서 불일치, opaque-view fixture의 잘못된 digest, Busy fixture의 두 번째 주입 serial이 이전 synthetic completion watermark보다 낮아질 가능성을 확인했다. 담당 Luna에게 각각 원인 검토·해당 fixture 수정을 맡겼으며 tolerance·계약을 완화하지 않는다. 이 실행을 GPU 통과로 계산하지 않는다.

세 fixture를 수정했다: 독립 reference를 실제 ASCII 순서 blue→red→gold로 맞췄고, 유효한64hex digest를 사용하며, 새 synthetic serial을 기존 watermark/submission/ledger 최대보다 높게 주입한다. renderer production/tolerance는 변경하지 않았다. Luna focused49/strict Clippy/fmt/diff 후 조정자가 같은 명령을 다시 실행해 **Metal library8 + 기존 GPU integration1 = 9 pass/0 fail**를 확인했다. 최종 로그는 `p03-sprite-metal-tests.log`다. Apple M3 Pro/Metal이며 이 실행은 실제 장시간 GPU stall latency·144fps·다른 플랫폼의 증거가 아니다.

통과 항목은 model scalar PBR/sRGB/wrap/filter, sprite UV/pivot/mirror/order/tint·dirty/reuse, nonblack clear alpha0/.25/1의 독립 straight-PNG reference, old-ready 실제 projection/target readback, early non-sRGB rejection, legacy packet parity, packet/legacy native-view opaque target readback과 기존513×289 padded capture다. RGBA tolerance는 투명 PNG reference의 RGB4/alpha1 byte, 기존 opaque sprite RGB3 byte이며 두 종류의 target 표현을 구분한다.

## 실제 CLI·공개 원본 검증

`cargo build -p daedalus-cli --features window --locked`가6.23초 compile로 성공했다. debug executable SHA-256은 `942d1b6224279c15eff3ad52eeb45c69b6281a60b98d5bdf41ec256443eddbed`다. 첫 combined smoke는 sprite camera의 near를 f64 literal0.1과 비교해 실패했다. 실제 report는 올바른 authority pose/정사영·일치하는 last-presented/final view와 f32 near0.10000000149011612를 보였다. engine output은 바꾸지 않고 해당 기대값을 정확한 f32 round-trip으로 고쳤다. 실패 stderr는 `p03-sprite-debug-smoke-first-failure.stderr`에 보존했다. Luna Python55/6.774초·py_compile·diff 후 source를 고정했다.

조정자의 `python3 scripts/smoke_project.py --cli target/debug/daedalus --assets --view --models --sprites`는 **39/39 pass·14.288578초**다. CPU18+assets13+native4+models2+sprites2이며 과거24건과 범위가 달라 speedup 비교가 아니다. `p03-sprite-debug-smoke.json`에 binary/체크/실제 authority·view·resource 근거를 저장했다. model/sprite 모두 viewer가 살아 있는 동안 별도 batch capture를 실행했다. live-window capture나 source reload는 아니다.

같은 binary로 아래 원본 QA를 수행했다.

```sh
target/debug/daedalus project capture dist/qa-assets/sprite-runtime-project --output dist/project-evidence/p03-public-sprites.png --width 960 --height 640 --ticks 1
target/debug/daedalus project view dist/qa-assets/sprite-runtime-project --seconds 8 --width 960 --height 640 --report dist/project-evidence/p03-public-sprites-native.json
```

원본 PNG hash는 위 착수 때와 동일하다. project digest는 `sha256:992a19db4c46f2f4875cb78fc7ad03a29bc307f061cec36ef7ac84160ce32906`, 출력 PNG SHA-256은 `b593ea465c69dc97a70aa70485fa459ffe97f9bd22458355326dd120ca028008`다. 실제 이미지를 열어 네 캐릭터의 upright orientation·nearest 픽셀·투명 영역을 확인했다. offscreen PNG이며 물리 window screenshot과 구분한다.

네 sprite/two atlas ID는 unit quad1개/geometry upload88 bytes, texture upload1회/135,168 bytes, sampler1/pair1을 공유했다. 최초 instance write8회/576 bytes(각128+16), material4회/192 bytes, globals1회/128 bytes다. native는960×640/8초/939 present/TimeLimit, last-presented tick482/sequence483/ortho span4였다. geometry/texture/instance/material/globals 업로드가 최초 이후 증가하지 않았다. native counters/report와 offscreen 이미지가 별도 근거이며 이 run은144fps·고부하 품질·physical2D pan/zoom 입력 인수가 아니다.
