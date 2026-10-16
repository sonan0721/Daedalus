# P03.2b 스프라이트·정사영 실행 작업 기록

상태: 상세 계약 확정·Luna 구현 진행·인수 미통과 · 2026-10-16

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
| Luna xhigh renderer | unit quad/UV slots·공통 texture lifetime·alpha/projection·actual view/window·CPU/GPU 검사 | 구현 진행 |
| Luna xhigh CLI | 기존 공통 report·opt-in sprite smoke·timeout/cleanup 회귀 | 구현 진행 |
| 조정자 | MD·source/API 리뷰·통합/실제 Mac·원본 QA·Git/PR | 계약 확정; 구현 검사 미실행 |

content→renderer→CLI→조정자 순서로 Cargo/shared target 소유권을 넘긴다. source 파일 소유권을 분리하고 새 dependency/두 번째 cold target/새 packaging backend는 계획하지 않는다. focused→통합→hardware→clean package→최종 다섯 PR CI/main 동일 tree가 모두 필요하다. 검사 명령/입력/소스·binary identity/counter는 실제 실행 후에만 추가한다.

## 착수 checkpoint

상세 계약은 `5d1be86`에 code보다 먼저 커밋했다. Luna xhigh 세 담당자에게 content/renderer/CLI·scripts 소유권과 public accessor 계약을 전달했다. content만 Cargo owner로 focused 검사를 시작하고 renderer/CLI는 의존 API를 공유해 disjoint source 편집을 진행한다. default smoke와 CPU CI에 GPU를 강제하지 않으며 후속 실제 hardware/원본 QA/clean package와 최종 CI는 여전히 미실행이다.

공개 원본 QA 입력은 ignored `dist/qa-assets/sprite-runtime-project/`에 준비했다. Tiny Dungeon의 원본 PNG5294 bytes/SHA-256 `d24e60a41e4ac7a745c0304dfde121143688557f40215f23221c29cfe683825f`를 그대로 복사했고 atlas descriptor의16×16 region4개만 별도 작성했다. 두 atlas StableId·네 sprite와 camera-only entity로 unit quad/texture 공유 및 실제 픽셀 아트를 확인할 예정이다. 현재는 입력 준비만 했으며 새 sprite binary의 validation/render는 미실행이다.

초기 CLI source 리뷰에서 실제 view JSON을 capture artifact payload 안의 `capture.render_view`로 명확히 했다. 기존 last_presented/window camera key와 새 view의 camera serialization이 따로 늘지 않도록 공통 camera 변환 재사용을 요청했다. 후속 live capture 계약에도 실제 used projection/local zoom/Busy ready metadata 재사용을 반영했지만 해당 capture 기능은 아직 구현하지 않았다.

## content 첫 source 고정

Luna xhigh content가 private sprite/camera binding과 typed 공통 projection helper를 구현했다. 조정자는 atlas/region kind·ref/tint 오류, project-wide camera uniqueness/renderer/presentation 충돌, unit scale/range, legacy camera-only 지원과 sprite Unsupported, validate의 borrowed catalog 재구성, map/ID allocation 이전 metadata charge를 source 리뷰했다. catalog 전체 clone이나 별도 decoder는 추가하지 않았다.

담당자의 frozen source focused 결과: `cargo test -p daedalus-content --locked`72/72, all-target/all-feature strict Clippy, `cargo check -p daedalus-content --lib --target wasm32-unknown-unknown --locked`, scoped rustfmt/diff check 통과. Wasm은 library compile이며 실제 브라우저 실행 인수가 아니다. 조정자의 workspace/Metal/native sprite 통합은 아직 미실행이다.

Cargo/shared target은 content에서 renderer로 인계했다. content 담당자는 새 `sprite_packet_tests.rs` 한 파일만 source 작성해 independent alpha/UV/order/reuse 검사를 병렬 준비하고, renderer 담당자는 핵심 GPU/window와 no-input/Busy/budget 검사를 맡는다. 이 source-only 분담으로 같은 테스트 파일/컴파일 락의 중복을 피한다. 실제 소요 시간 개선율은 측정하지 않았다.
