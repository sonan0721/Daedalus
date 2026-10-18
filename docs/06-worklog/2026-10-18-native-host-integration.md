# Native host와 실제 CLI 에셋 교체 통합

상태: P03.3b.3.3 통합 검증 중 · 2026-10-18 · D1/D2/D3 미완료

## 목적과 시작 상태

공통 host·managed window·CLI adapter의 개별 검사를 실제 외부 프로젝트의 같은 창/세션에서 연결한다. [이전 집중 근거](2026-10-17-render-generation.md)는 단일 모델 SDK 창과 CLI CPU/정적 검사까지이며, 혼합 에셋의 실제 CLI 교체 통과가 아니었다. [인수표](../04-plan/implementation-checklist.md)의 host/CLI·pixels/pose/ACK/회수 행을 닫기 전에 결함과 해결 근거를 기록한다. 전체 제품 목표와 이 하위 검증을 구분한다.

시작 source는 작업 branch `feat/native-project-host`의 `fce7cd3`이며 Git 작업 트리는 깨끗했다. 기존 debug CLI는 SHA-256 `ac6fcdea7adc9ccc96d3d7b5244fedf7821a432d9e8f63012d4db7e8c330ebcf`다. 기존 binary를 먼저 사용하고 이 검증을 위해 자동 재빌드하거나 새로운 게임 프로젝트를 만들지 않았다. 소스·스크립트는 검증 동안 동결했다.

## 실제 통합 첫 시도: 초기 device 준비 실패

```sh
python3 scripts/smoke_project.py --cli target/debug/daedalus --assets --view --models --sprites --asset-preparation --asset-activation
```

실행기는 `scripts/run_logged.py`와 고유 raw `dist/project-evidence/p03-host-native-cli-root-attempt01.log`다. exit1·14.395초, raw SHA-256 `c0c196e8a980bb74e9d7fa7a7809690bd614dbd88aad3a7c439cf422af4b417b`를 보존했다. 최종 JSON이 완성되기 전에 실패했으므로 앞서 실행됐을 수 있는 sub-smoke를 성공 건수로 계산하지 않는다. 이 시도는 native audio를 요청하지 않았다.

혼합 model/orthographic sprite fixture의 첫 actual-present 대기에서 host lifecycle이 Failed가 됐다. code는 `initial_device_prepare_failed`, message는 `packet stage generation or upload accounting overflowed`다. generation1/source digest `sha256:60e3fbc7a1d963cd33caf1603e20e7bd04b1f51262081b433e8fec433c511ee1`, last_presented/queued_publication은 null이었다. CPU status는 metadata5116byte·scratch peak12438byte·frame slots1/3이며 frame reserve16MiB였다. 후보 Stage/Commit에 도달한 실패가 아니다.

## 수정 전에 고정한 경계

- 기존 renderer resource planner와 checked accounting의 정확한 실패 지점을 먼저 찾는다. 오류 문자열만 보고 실제 정수 고갈이나 예산 부족이라고 단정하지 않는다.
- 단일 모델 창 통과를 model+sprite 초기 bootstrap 검증으로 대체하지 않는다. 혼합 fixture를 줄이거나 항목을 빼서 통과시키지 않는다.
- 실제 원인이 확인된 부분만 Luna xhigh가 수정하고 기존 CPU/GPU regression에 재현을 넣는다. 별도 importer·renderer·새 Engine·새 smoke framework를 만들지 않는다.
- queue·memory·deadline 한도를 늘려 증상을 가리지 않는다. 부분 GPU owner 보존·nonwaiting UI·same-device staging 계약을 유지한다.
- source 동결 후 관련 집중 검사→실제 CLI 재실행→전체 workspace/CI/package 인수로 진행한다. 실패 raw를 덮어쓰거나 실제 pixel 검증을 metadata로 바꾸지 않는다.

Luna의 첫 작업은 읽기 전용 진단이며 아직 root cause/fix 성공으로 보고하지 않는다. 후속 [live capture 계약](../03-architecture/live-capture-and-artifacts.md)도 읽기 전용 API 감사를 진행하지만, 이 결함을 남긴 채 capture 구현을 같은 source에 섞지 않는다.


## Source 진단과 수정 계약

Luna의 읽기 전용 진단과 root의 해당 source 대조는 `prepare_stage_create_batch`/`preflight_stage_create_batch`의 공유 texture-layout 중복 계산을 찾았다. ModelPipeline과 SpritePipeline은 각각3개의 pipeline을 만들고 layout이 없을 때1개를 더 만든다. 두 task를 같은 초기 상태에서 따로 예상하면8이지만 실제 첫 task 뒤에는 layout이 있으므로7개다. `create_stage_resources`의 실제7/계획8 불일치가 CounterExhausted를 반환한다. 단일 모델 SDK가 통과한 범위와 혼합 bootstrap 실패를 설명하는 source 원인이며 아직 수정 실행 성공은 아니다.

[Device 계약](../03-architecture/render-device-generation.md)에 batch-aware 공유 자원 계산을 먼저 명시했다. Luna xhigh는 `packet_renderer/staging.rs`의 공통 batch estimator/preflight와 기존 `staged_gpu_tests.rs`의 좁은 혼합 회귀만 소유한다. 필요하면 `renderer.rs`의 existing pipeline-call helper에 작은 내부 accessor만 제안한다. host/runtime/CLI·예산·timeout은 이 수정에 포함하지 않는다. 집중 source 동결 뒤 검사와 실제 CLI를 한 소유자가 순서대로 실행한다.


## 후속 live capture의 읽기 전용 감사

별도 Luna 감사는 existing activation factory·RequestTicket의 normal credit·managed actual view를 확인했다. root는 [capture 계약](../03-architecture/live-capture-and-artifacts.md)에 additive 같은-load factory·renderer의 runtime 독립성·host bridge pending/backstop·concrete image lease와 artifact reader·정확한 camera/projection/format 복사를 명시했다. runtime ACK DTO만으로는 tagged projection/camera 전체를 얻을 수 없음을 기록했다. source 구현과 capability 활성화는 하지 않았으며 이번 혼합 pipeline 수정과 별도 인수한다. 정확한 close/panic/backstop 소유권 signature는 다음 코드 착수 전 남은 동결 항목이다.


## 공유 layout 수정의 집중 검증

Luna가 production `packet_renderer/staging.rs`와 기존 `staged_gpu_tests.rs`만 수정했다. batch-local 공유-layout planner를 selector/preflight가 함께 사용하고, 실행의 실제 create-call 계수·동등성 검사는 유지했다. root는 source diff와 최종 hash를 대조했다. production hash는 `b6e7f9a00b05bb5687338db4fd98347ff9fa88145de23a98a2b49eb065ccc47b`, regression source는 `d152f62cec8d46c8de2fe0c10b555273046328087d94fdc135eb9881bc4a39c2`다. 한도·host/runtime/CLI source는 변경하지 않았다.

| 검사 | 실제 결과 | raw SHA-256 |
| --- | --- | --- |
| `p03-mixed-stage-renderer-cpu-luna-attempt01.log` | default renderer lib84통과·GPU29 ignored | `748ccc76337347a44fed6cb4515cff3faed1bb46217ea9d83a68e6602bb08c82` |
| `p03-mixed-stage-renderer-clippy-luna-attempt02.log` | all-target/all-feature strict exit0 | `16ab96481d602e52541639eee738c31a22bc4adfa2444059214e7b8899ef481d` |
| `p03-mixed-stage-metal-new-luna-attempt02.log` | 실제 M3 Pro/Metal 혼합 initial stage·7 create·Ready·첫 frame1통과 | `3ead32e94f49b1743a564a5031e140c7ec3f86b76d2192587796842874ce98cd` |
| `p03-mixed-stage-metal-cold-model-luna-attempt01.log` | 기존 cold-model atomic stage1통과 | `4f999851320fca752b0dc39b749c6e3e8619debb68a51a50d52ccde475c2729b` |
| `p03-mixed-stage-metal-quota-luna-attempt01.log` | 기존 create-quota/cache-hit stage1통과 | `baf4e8b30542aea9113564d409f190985be06d6e2329a61534af6804f8888af2` |

모두 `dist/project-evidence/`에 보존했다. default CPU84는 앞선 all-feature85와 feature 설정이 다르며 감소나 합산으로 해석하지 않는다. 실제 GPU3개는 이번 변경에 관련된 집중 검사이며 과거 전체29개와 합산하지 않는다. 이 texture target의 첫 frame 제출은 실제 native 창의 교체 픽셀 검증이 아니다.

새 GPU regression의 첫 시도는 Stage Ready 시점에 active geometry/textures가 있어야 한다는 test 기대 때문에 실패했다. staged record의 자원은 첫 실제 frame promotion 때 active로 옮겨지는 기존 계약이므로, pipeline 준비는 Ready에서 확인하고 geometry/texture는 `activate_frame` 뒤 확인하도록 기대를 고쳤다. 실패 raw `p03-mixed-stage-metal-new-luna-attempt01.log`·exit101·SHA-256 `5cc31e7d5f8c1d1d84d9fed6f38846fd9834e2615600ad8e1a00ed5735b16348`를 유지한다. production 실패가 추가로 재현됐다고 보고하지 않는다. 마지막 strict는 이 최종 regression bytes를 컴파일했다.

집중 lease를 반환받은 root가 실제 CLI를 새로 링크한 뒤 첫 실패와 같은 명령을 다시 실행한다. 이 기록 시점에서 실제 CLI 교체 성공·전체 workspace/CI/package 인수는 대기다.


## 실제 CLI 재실행: 교체 후 final report 불일치

새 CLI build는 exit0이며 raw `p03-mixed-host-cli-build-root-attempt01.log` hash는 `bb4daafcea1cd7d59c4ecb2e2e74da9f3385d91cc88a073532756d8644eb5bbb`, executable hash는 `51c8f7dfb56a015cf659ce6db69e8710da17e3a5796dd6fe2932717724b28de3`다. 같은 flags의 두 번째 실제 통합 실행 `p03-host-native-cli-root-attempt02.log`는 exit1·15.748초·SHA-256 `f1024ce3cff831823d0d50ecdf9df02998df24385b4b51572afb66729829e59b`로 종료했다.

이번에는 initial GPU 준비를 넘어서 gen2 실제 present·tick1·원래 handle/transform 보존·old source retirement·정상 Close/StopRequested까지 진행했다. 마지막 report validator에서 top-level source_digest가 초기 `60e3fbc7...`로 남아 있고 host.active_source_digest와 실제 last_presented는 최종 `fe3ec093...`인 불일치를 검출했다. report host는 stopped/failure null이었으며 authority final tick1과 ticket shutdown credit0이었다. 이 부분 성공을 최종 suite 통과 건수로 합산하지 않으며 원본 전체 report는 실패 raw에 보존했다.

원인은 activation adapter가 초기 ProjectInfo의 digest를 final report에 사용한 것이다. 위 CLI 계약에 최종 host status의 실제 active digest 사용과 status unavailable의 null 의미를 먼저 추가했다. Luna는 activation.rs의 해당 report 필드만 수정하고 기존 실제 validator로 재검증한다. 초기 loader 재실행·별도 authority·한도/timeout 변경은 하지 않는다.


## Final digest 수정과 카메라 검증의 입력 전제

Luna가 activation report의 digest를 final status accessor로 매핑했다. root가 accessor 반환형을 대조해 Option 조합의 타입 실수를 컴파일 전에 바로잡도록 요청했고, 이 source-review 피드백을 실행된 compiler 실패로 세지 않는다. 최종 activation.rs hash는 `0b9833a8924312967b0d6e3f160c61680459705755a4c001be40a8e3d58b7abf`다. CLI build02는 exit0·raw SHA-256 `35a43bfb6335d726590c14b58f2e890fbba5761cf8ea09a0d118d59f6b816103`이다.

같은 전체 flags의 실제 통합03은 exit1·21.056초·raw SHA-256 `472d1983b4a5d18616d1fd478a2686f6dcb416d20ed40cbad059a00807743c3b`다. 이번 실패는 기존 sprite authority-camera 검사의 “로컬 입력 없음” 전제였다. 실패 report에 실제 key events6·local_camera_changes5가 있고, 실제/최종 camera X가0.413761인 상태를 기록했다. 입력의 외부 발생 주체를 단정하거나 이를 새 GPU 초기화 실패로 분류하지 않는다. 전체 suite 통과로 보고하지 않으며 assertion을 완화하지 않는다.

반복 비용과 창 활성화 횟수를 줄이기 위해 기존 helper의 `--view --asset-activation` 조합으로 현재 managed 경로를 분리해 검증한다. 새 framework·숨은 camera input 차단·timeout 증가·입력 없는 척하는 fixture 변경은 하지 않는다. 전체 flags와 집중 실행의 검사 범위/결과는 구분한다. quiet authority-camera의 독립 실제 재검증은 열린 상태다.


## 실제 집중 CLI 교체 35통과

동일한 기존 helper의 `--view --asset-activation` 집중 실행04가35/35통과했다. exit0·11.039초, raw `p03-host-native-cli-root-attempt04.log` SHA-256 `1cb6f5050754135f8da707dd2e0b81c80bdc0ab7fbf123c5cb32474b71d16745`다. executable hash는 `b0cc89fb06c1d38fc91d012056254ed6eba0edcf034c334838d16fbbe797c757`이다. 이35개에는 기존 CPU18·headless activation·native viewer lifecycle와 새 same-window managed replacement가 포함되며 전체 flags의 실패 실행을 통과로 바꾸지 않는다.

실제 managed session1에서 initial digest `sha256:60e3fbc7a1d963cd33caf1603e20e7bd04b1f51262081b433e8fec433c511ee1`가 committed digest `sha256:fe3ec0938cf560c14adda2662713d720ce96d2778fe9bebc56a4e553aac11d85`로 바뀌었다. 실제 gen2/tick1/publication3/authority snapshot2와 present sequence100을 관측했다. handle은 같은 session1/index1/generation0이며 translation[-1,0.25,-3]/scale[1.1,1,1]을 유지했다. retired cooked bytes와 retired generation count는0이고 final report도 committed digest·stopped·StopRequested·final tick1이었다.

이는 실제 창의 표시 metadata·세션 상태·이전 source 회수와 final 보고의 근거다. helper의 `pixel_capture`는null이며 같은 창 이미지 readback·GPU 전체 물리 메모리0·144fps를 증명하지 않는다. 전체 workspace/CI/package와 quiet authority-camera 회귀는 아직 다음 단계다.

자동 검증 창의 native 입력 간섭을 줄이는 명시적 protocol-only viewer mode를 별도 유지보수 계약으로 검토한다. 기존 기본 interactive API/default와 검증 assertion은 보존하며, 창 생명주기와 실제 protocol mutation/Step 동작을 그대로 검사한다. 이 기능은 아직 source 작성/검증 전이다.
