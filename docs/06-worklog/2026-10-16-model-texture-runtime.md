# P03.2a 모델·텍스처 실행 작업 기록

상태: 구현 진행·인수 미통과 · 2026-10-16

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
| Luna xhigh renderer | model packet·texture/sampler/mip·GPU 예산/transaction·계측·검사 | 구현 중; 현재 Cargo/shared target owner |
| Luna xhigh CLI | 공통 loader·view/capture·opt-in 기존 smoke 확장 | 공통 loader/packet capture source 작성·stats 의존 대기; Cargo 미실행 |
| 조정자 | MD·source review·실제 통합·Git/PR·실측 | 계약 docs check/diff check 통과; 새 runtime 검사는 미실행 |

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
