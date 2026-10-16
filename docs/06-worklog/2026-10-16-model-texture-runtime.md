# P03.2a 모델·텍스처 실행 작업 기록

상태: 로컬 통합 검증 통과·clean package/PR CI 대기·인수 미통과 · 2026-10-16

## 이번 파일과 선행 문서를 만든 이유

[상세 계약](../03-architecture/model-texture-runtime.md)은 P03.1 catalog를 실제 모델·텍스처 GPU draw에 연결하기 전에 JSON binding, 공개 API 호환성, cache 잠금, matrix/색/sampler, bounded resource transaction과 검증을 정한다. 이 작업 기록은 설계·Luna 구현·조정자 리뷰·실측을 구분하고 아직 실행하지 않은 시험을 통과로 바꾸지 않기 위해 만들었다. 개발 반복 비용의 공통화는 [파이프라인 계획](../04-plan/development-iteration-pipeline.md)에 함께 연결한다.

선행 P03.1은 PR10 최종 head `1c4631c`와 다섯 CI·두 packaged assets step·main `5c04db8` 동일 tree까지 인수했다. 새 branch는 `feat/asset-model-consumers`이며 상세 계약은 `e29c376`에 먼저 기록했다. 전체 D1→D2→D3 goal은 계속 active이고 현재 P03.2a만 작업 중이다.

## source 감사와 선택

1. PreparedEntity 공개 field 추가는 struct literal을 깨뜨린다. binding은 private PreparedProject map/accessor로 추가한다.
2. catalog의 decoded image 등은 owned Arc이며 cache pin은 disk eviction/owner lock 수명만 가진다. eager CLI는 명시적으로 pin을 해제해 창이 별도 capture/검증의 cache를 점유하지 않게 한다. default pinned API는 보존한다.
3. 같은 mesh를 여러 glTF node가 참조하면 primitive index가 중복된다. authority handle+node/primitive occurrence ordinal로 draw를 구분한다.
4. 기존 shader는 ACES를 적용한다. 최종 pixel을 원래 sRGB byte와 단순 비교하지 않고 독립 색 변환과 기존 scalar PBR reference를 사용한다.
5. resident256 MiB에 old128+new128만으로도 가득 찬다. upload staging128 MiB를 별도 추적하고 frame/overlay queue write까지 completion 수명으로 계산한다. 실제 전체 VRAM/RSS 한도라고 홍보하지 않는다.

## 분담과 현재 검사 상태

| 소유자 | 범위 | 현재 |
| --- | --- | --- |
| Luna xhigh content | binding·revalidation·opt-in pin detach·CPU regression | source 고정·focused 검사 통과·renderer에 Cargo 인계 |
| Luna xhigh renderer | model packet·texture/sampler/mip·GPU 예산/transaction·계측·검사 | source 고정·focused 검증 뒤 조정자에 Cargo 인계 |
| Luna xhigh CLI | 공통 loader·view/capture·opt-in 기존 smoke 확장 | source 고정·focused36/strict Clippy·build 통과; concurrent smoke callback 추가 |
| 조정자 | MD·source review·실제 통합·Git/PR·실측 | 아래 workspace·Python·실제 GPU/native 통과; clean package/CI 대기 |

새 dependency/별도 target/새 packaging backend는 현재 계획하지 않는다. 구현 결과와 검사 명령·환경·입력·identity·실제 counter는 source 고정 후 이 파일에 추가한다. content/renderer/CLI의 완료 보고만으로 gate를 통과시키지 않는다.

## Content 첫 구현 checkpoint

담당 Luna xhigh가 고정한 source에서 다음 focused 검사를 통과했다. 조정자는 binding parsing/revalidation, private API, cache pin clone/reopen 수명과 retained metadata admission 순서를 source 리뷰했다. 새 map의 ID/record estimate를 borrowed scene에서 계산하고 catalog metadata에 먼저 charge한 뒤 map을 allocation한다. catalog 전체를 검증용으로 clone하지 않는다.

```sh
cargo test -p daedalus-content --locked
cargo fmt -p daedalus-content -- --check
cargo clippy -p daedalus-content --all-targets --locked -- -D warnings
cargo check -p daedalus-content --lib --target wasm32-unknown-unknown --locked
```

결과: CPU69/69, fmt·strict Clippy·Wasm library compile 통과. Wasm 실행 검증을 뜻하지 않는다. 네 새 regression은 native/byte binding과 revalidation, legacy source digest 동등성, component 오류/충돌, 명시적 detach/pinned clone 뒤 reopen 및 geometry/PNG backing 생존을 다룬다. 전체 renderer/CLI source는 작업 중이라 workspace/실제 GPU gate는 미실행이다.

공개 원본 Cube의 세 source는 hash를 바꾸지 않고 ignored `dist/qa-assets/model-runtime-project/`에 준비했다. 두 asset StableId를 두 entity에 연결해 후속 실제 view/capture에서 geometry/texture 공유를 확인할 입력이다. 이 입력 준비를 runtime 통과로 계산하지 않는다.

## 통합 전 리뷰·자동화 checkpoint

CLI의 공통 loader와 packet capture adapter, 공통 geometry/asset stats JSON 변환 및 기존 smoke의 opt-in `--models`를 Luna xhigh가 작성했다. script는 기존 offline glTF/PNG 생성과 authority/native window 제어를 재사용한다. model scene은 실제 참조하는 model descriptor만 남겨 불필요한 atlas/audio import를 하지 않는다. 별도 process의 SessionId 숫자가 같아도 정상인 현재 process-local 규칙을 확인해 잘못된 cross-process 부등식 검사를 제거했다.

조정자 독립 실행: `python3 -m unittest discover -s scripts -p 'test_*.py'`는54건/6.832초로 통과했고 docs/diff 검사도 통과했다. 이는 Python 회귀이며 실제 `--models` 바이너리 검사는 renderer/CLI 통합 뒤다. field 매핑 자체를 그대로 반복하는 단순 테스트는 추가하지 않고 실제 capture/view의 출력 검증을 사용한다.

renderer source 리뷰에서는 queued write 이전 transaction, 모델이 없는 legacy path, 불필요한 packed vertex→byte Vec 복사, cache hit/miss 계측 시점, last_submission 갱신 및 write0 frame의 callback/ledger 개수 제한을 확인·수정 요청했다. 특히 staging byte 한도만으로 callback 수가 제한되지 않으므로3개 in-flight submission 계약을 보완했다. 아직 해당 renderer focused/GPU 검사는 미통과 상태다.

## 통합 source 고정 후 조정자 독립 검증

content 변경은 `6afe7a4`, 계약 보완은 `cf11559`에 기록했다. 아래 검사는 그 위 renderer/CLI/Python 변경의 로컬 source 고정 상태다. 렌더/CLI Rust 변경은 모두 Luna xhigh 작성이며 조정자는 명세·source 리뷰·검사와 Git을 수행했다. Cargo는 같은 workspace/target을 한 소유자씩 사용했고 새 packaging backend·별도 cold target을 만들지 않았다.

```sh
cargo test --workspace --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo fmt --all --check
python3 -m unittest discover -s scripts -p 'test_*.py'
python3 scripts/smoke_project.py --cli target/debug/daedalus --assets
cargo test -p daedalus-renderer --all-features --locked -- --ignored --nocapture --test-threads=1
python3 scripts/smoke_project.py --cli target/debug/daedalus --view --models
```

| 실제 실행 | 결과 | ignored evidence 경로 |
| --- | --- | --- |
| all-feature workspace | CPU254 pass·0 fail·hardware5 ignored | `dist/project-evidence/p03-model-workspace-tests.log` |
| workspace all-target strict Clippy/fmt | pass | `p03-model-workspace-clippy.log` |
| Python 최종 concurrent hook 포함 |54 pass/7.145초 | `p03-model-python-final.log` |
| 실제 debug assets |31 pass/2.977975초 | `p03-model-assets-smoke.json` |
| 실제 Apple M3 Pro/Metal renderer |4 pass, lib3+integration1 | `p03-model-gpu-tests.log` |
| 실제 native/model 최종 concurrent smoke |24 pass/11.015236초 | `p03-model-native-concurrent-smoke.json` |

JSON/log 경로는 별도 표시가 없으면 ignored `dist/project-evidence/` 아래다. debug CLI SHA-256은 `f005722d653053a4badeba6ffa88eb29eea8ba98bbb366b7928413c43badffe5`다. CPU의 ignored5개 중 renderer4개만 이 단계에서 명시 실행했고 audio device probe는 이번 변경 범위 밖이라 재실행하지 않았다. 기존 focused content69/CLI36과 더한 중복 총수를 별도 통과 수로 홍보하지 않는다.

픽셀 검사는 비대칭2×2 texture의 네 중심, repeat/mirror/clamp의 U=-0.25, nearest/linear의 U=0.45, 강한 minification(1024 UV)의 실제 cooked mip를 확인한다. independent f64 piecewise sRGB decode·linear factor `[0.67,0.81,0.56]` 후 기존 scalar PBR reference와 비교한다. 일반 channel tolerance3/mip8은 quantization 허용치이며 실측 최대오차 수치가 아니다. 동일 renderer에서 initial→repeat→authority translation을 검증했고 repeat는 geometry/texture/sampler/pair/instance/material 증가0, translation10draw는 instance10회만 증가했다.

submission 포화 검사는 실제 GPU draw와 주입한 completion ledger 상태를 조합해 결정적으로 검사한다. write0 frame까지 최대3submission, Busy old-ready 처리, queue write 이전 reject, completion 뒤 회수를 검증했지만 실제 장시간 GPU stall/144fps/부하 중 반응성 측정이 아니다. source 리뷰에서 cache miss/hit 계산 시점, legacy untextured path, 불필요한 packed-byte 복사, fixed overlay charge, draw 전에 전체 allocation admission, no-ready Busy의 surface drop/no-present를 확인했다.

## 창과 별도 capture의 동시 작업 검사

최종 smoke는 viewer의 `project_info` terminal/ack 뒤 같은 임시 project의 batch capture를 실행한다. capture 종료 뒤 viewer process가 여전히 살아 있음을 확인하고 authority get/set/Step·snapshot·실제 presented packet까지 대조한다. report의 `concurrent_with_native_view:true`는 이 순서의 근거다. 두 process의 SessionId 숫자는 각각1이어도 정상이며 process-local 식별 규칙을 바꾸지 않았다.

이는 eager CLI가 CPU owned IR을 유지한 채 disk pin과 cache owner를 해제해 창/캡처의 반복 작업이 충돌하지 않음을 검증한다. default pinned API와 pinned clone 차단은 content regression으로 유지한다. PNG는 별도 batch session의 출력이며 기존 live-window frame capture 미구현 상태를 대체하지 않는다.

## 공개 원본 모델과 texture의 실제 출력

[provenance](../02-research/asset-validation-provenance.md)의 Khronos Cube CC0 원본 세 파일은 변경하지 않았다. ignored QA project `dist/qa-assets/model-runtime-project/`의 두 asset StableId/two root entity는 같은 glTF/bin/PNG를 참조한다. 아래 hash는 원본 provenance와 동일하다.

| 원본 | bytes | SHA-256 |
| --- | --- | --- |
| Cube.gltf |3531| `9580ff77ad6a1a601fb570aaad6148f8e4067244d2430337a43edbb4d627e85d` |
| Cube.bin |1800| `ece1e675655d75762aaa7e920d93167442d3b9672324694716528ed4e018e4b3` |
| Cube_BaseColor.png |565730| `0750d5a03c1bebc640571e309f66c6e88efbff2ef4c120619466a7014551bb9a` |

```sh
target/debug/daedalus project capture dist/qa-assets/model-runtime-project --output dist/project-evidence/p03-public-cube-model.png --width 960 --height 640 --ticks 1
target/debug/daedalus project view dist/qa-assets/model-runtime-project --clock manual --seconds 12 --width 960 --height 640 --report dist/project-evidence/p03-public-cube-native-report.json
```

source digest `sha256:e6fa43a687b25ce65829dd0e958dd39b11f60137aa47e52d5ef3314cab694d52`,2draw/72vertex/72index. 서로 다른 두 asset/entity가 geometry upload1회/1296 bytes, texture upload1회/1,398,100 bytes(512²와 전체 mip), sampler1/pair1을 공유했다. 실제 PNG를 시각 확인했고 두 cube 모두 cream marble texture가 표시됐다. 원본/출처는 엔진 archive에 포함하지 않는다.

capture의 resident charge는4,283,124 bytes, bookkeeping reservation5,491,200 bytes, staging charged peak1,410,544 bytes, CPU upload scratch charged peak1296 bytes였다. 이 값은 logical 예산/계측이며 전체 VRAM/RSS가 아니다. capture 종료의 staging/in-flight는0이다. 단1회 debug cold 실행 real2.13초는 일반 성능 보장이 아니다.

12초 실제 native surface는 TimeLimit로 종료했고1409frame을 presented했다. authority tick0에서 geometry1/texture1·instance2/material2/global1 이후 재업로드가 증가하지 않았다. 마지막512present 간격 p50≈8.331ms/p95≈8.527ms/p99≈8.639ms였고 GPU 완료 지연·144fps/high-load 증거로 사용하지 않는다. native surface report와 실제 offscreen pixels를 확인했으며 bare CLI 창은 CUA 앱 목록에 없어 이번에 물리 창 screenshot을 얻었다고 표시하지 않는다. 공개 입력의 별도30초 view/capture 수동 시도는 생존 중 실행 시각을 확정하지 못했으므로 동시성 근거는 위 자동 smoke만 사용한다.

## 남은 인수

clean source commit의 local release archive/extracted binary, 최종 PR 다섯 CI와 두 packaged-assets step, main 동일 tree가 아직 남아 있다. P03.2a는 이 하위 gate가 끝난 뒤에만 체크한다. P03.2b sprite·P03.2c audio·P03.3 reload·P03.4 반복 루프 품질과 P02 live capture/MCP/SDK, 전체 D1→D2→D3는 계속 미완료다.
