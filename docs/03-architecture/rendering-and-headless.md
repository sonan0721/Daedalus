# 렌더링·시각 확장·헤드리스 캡처

상태: 독립 wgpu 렌더러 설계 제안. 구현·화질·성능 실측 없음 · 2026-10-15

## 엔진 책임

복셀·일반 메시·스켈레탈 캐릭터·픽셀/HD 2D·UI를 공통 GPU 자원·렌더 그래프에서 다룬다. 스타일라이즈드와 사실적 아트의 재질·광원·포스트를 확장할 수 있어야 한다. 특정 게임 이름을 재현 가능한 렌더 명세 대신 사용하지 않는다.

표현 계약에는 좌표·단위·색공간·재질 파라미터·조명 단위·투명·그림자·모션·히스토리·출력 형식을 포함한다. wgpu는 그래픽 API이며 PBR·톤매핑·UI를 자동 완성해주는 엔진이 아니다. [wgpu 문서](https://docs.rs/wgpu/latest/wgpu/)

## 파이프라인 제안

`snapshot 수신 → revision 검증 → 자원 준비·유한 upload → 가시성/LOD → shadow → opaque 3D/voxel → 투명·물·VFX → 2D 합성 → post → UI → present/capture`

실제 순서와 render path는 조사·실험으로 결정한다. UI의 표시 목적에 따라 scene post 적용 여부를 나눈다. “동일 파이프라인”은 자원·그래프 공유를 의미하며 모든 요소가 한 shader·draw call을 사용한다는 뜻이 아니다.

| 영역 | 필수 설계 질문 |
| --- | --- |
| PBR·재질 | base color·metallic/roughness·normal·emissive·alpha·sampler·커스텀 모델 ABI |
| 스타일 표현 | toon/ramp·outline·색/노출 제어·캐릭터 전용 shading 확장 |
| 조명·그림자 | clustered light 분류와 shadow atlas·캐시·갱신 예산을 별도 설계 |
| 일반 오픈월드 | 공간 분할·LOD·instance·skinning·floating origin·자원 residency |
| 복셀 | mesh material key·tile UV·AO·light volume·투명 경계 |
| 2D | 정렬·blend·pixel snap·HD sampling·normal-lit sprite·타일 조명 |
| TAA 등 temporal | motion vectors·jitter·history invalidation·disocclusion·UI 제외 정책 |
| AO·Bloom·볼류메트릭 | 해상도·깊이·노멀·노출·비용·capability·품질 단계 |
| GPU 파티클 | spawn·simulate·draw·pool·overflow·정렬·충돌 표현·reset |

단순 clustered light 지원이 수많은 동적 shadow를 값싸게 지원한다는 뜻은 아니다. TAA 이름만 추가하고 모션·히스토리 정합성을 생략하지 않는다. 첨단 효과는 사용자 참조와 성능 예산을 확보한 뒤 세부 명세를 만든다.

## GPU 자원 수명과 비용

CPU asset ready·GPU upload queued·GPU usable 상태를 구분한다. renderer가 buffer·texture·pipeline cache를 소유하고 worker 결과를 유한 bytes/time 내에서 반영한다. 대량 upload·pipeline 생성·shader compile·cache eviction·자원 해제를 계측한다.

새 mesh와 collider가 서로 다른 revision이면 시각·충돌 정책을 월드 계약에 따라 적용한다. teleport·camera cut·origin shift·큰 지형 변경·reload에는 temporal history를 적절히 무효화한다. device loss·surface resize·미지원 feature는 진단과 복구 상태로 노출한다.

## 창 없는 렌더와 서버 구분

오프스크린은 surface 없이 render target texture에 그리는 GPU 경로다. dedicated server는 GPU를 초기화하지 않는 simulation 경로다. Docker에 창이 없어도 adapter·driver·device 접근이 필요하며 noop backend를 실제 이미지 생성의 근거로 쓰지 않는다. [Bevy 공식 오프스크린 예제](https://github.com/bevyengine/bevy/blob/main/examples/app/headless_renderer.rs)

캡처 제안: `render texture → copy to staging buffer → GPU 완료 후 async map → 행 정렬 제거·색공간 변환 → encode worker → 파일·manifest`.

staging buffer는 유한 ring으로 관리하고 사용 중인 buffer를 덮어쓰거나 mapped 상태로 GPU에 재사용하지 않는다. `map_async` 완료·해제 수명을 따른다. 창 표시 경로에서 매 프레임 CPU readback을 강제하지 않는다. [wgpu Buffer](https://docs.rs/wgpu/latest/wgpu/struct.Buffer.html)

## 캡처 결과 계약

| 항목 | 내용 |
| --- | --- |
| 요청 | request ID·scene/fixture·seed·tick/frame·해상도·format·timeout |
| 이미지 | PNG 정확성 기준 후보, WebP 전달용 후보, animation 별도 옵션 |
| 부가 정보 | engine·asset·script hash, GPU·backend·driver, 색공간·camera·quality |
| 실패 | NoAdapter·UnsupportedFeature·Busy·MapFailed·EncodeFailed·Timeout |
| 시간 | cold 시작·warm render·readback·encode·전체 완료를 분리 |

WebP 정지 인코딩 지원과 애니메이션 구성은 별도 검증한다. 1초 애니메이션 출력 목표는 프레임 수·크기·시간 진행 방식·cold/warm 없이 확정하지 않는다. [WebP API](https://developers.google.com/speed/webp/docs/api)

## 검증

고정 입력으로 기본 재질·광원·픽셀/HD sprite·skinning·투명·UI·그림자 경계를 확인한다. 정확한 pixel 일치와 GPU/driver 허용 오차를 나누며 움직임의 고스팅·깜박임은 연속 프레임으로 본다. 144fps 목표는 장면·내부 해상도·품질별 p99와 GPU 시간을 명시해 평가한다.
