# 실제 공개 에셋의 import 검증 출처

상태: 원본·라이선스·hash와 CPU import 확인, Cube 실제 GPU 표시 확인·2D/audio 소비 전 · 2026-10-16 · P03.1/DEV03

## 목적과 범위

[공통 import 계약](../03-architecture/asset-import-foundation.md)을 자체 생성한 작은 입력으로만 검사하지 않도록 실제 공개 자료의 원본·변환·지원 경계를 고정한다. 이 목록은 게임 아트 선정이나 게임 품질 인수가 아니다. CPU cook 성공과 화면 표시·음향 출력도 서로 다른 검증이다.

원본과 변환 파일은 로컬의 ignored `dist/qa-assets/`에만 둔다. 엔진 source/package에 에셋이나 다운로드 절차를 넣지 않는다. 자동 smoke는 네트워크 없이 동작하며 아래 자료의 다운로드/변환은 별도 실제 입력 검증이다. 엔진의 MIT OR Apache-2.0은 외부 에셋의 라이선스를 바꾸지 않는다.

## 3D: Khronos Sample Assets의 Cube

[원작 설명](https://github.com/KhronosGroup/glTF-Sample-Assets/blob/edc7c9e67c639d230715049ee31f9a96a6babbbe/Models/Cube/README.md)과 [모델별 라이선스](https://github.com/KhronosGroup/glTF-Sample-Assets/blob/edc7c9e67c639d230715049ee31f9a96a6babbbe/Models/Cube/LICENSE.md)를 확인했다. 모델 저작자는 UX3D/Norbert Nopper로 표기되어 있다. 모델 payload는 CC0-1.0이며 README 등 metadocumentation에는 별도의 CC-BY-4.0 표기가 있다. 저장소 전체를 동일 라이선스의 에셋 묶음으로 취급하지 않는다.

고정 revision은 `edc7c9e67c639d230715049ee31f9a96a6babbbe`다. 다운로드 base URL은 `https://raw.githubusercontent.com/KhronosGroup/glTF-Sample-Assets/<revision>/Models/Cube/`이며 다음 파일의 실제 bytes를 확보했다.

| 원본 경로 | bytes | SHA-256 |
| --- | ---: | --- |
| glTF/Cube.gltf | 3,531 | 9580ff77ad6a1a601fb570aaad6148f8e4067244d2430337a43edbb4d627e85d |
| glTF/Cube.bin | 1,800 | ece1e675655d75762aaa7e920d93167442d3b9672324694716528ed4e018e4b3 |
| glTF/Cube_BaseColor.png | 565,730 | 0750d5a03c1bebc640571e309f66c6e88efbff2ef4c120619466a7014551bb9a |
| LICENSE.md | 766 | 9f5611b2615694034748e579fd3b61bf760b2ae9cd084b6c5d3822132f391b3c |
| README.md | 776 | 63b36a9a36b751d00ca02bd7f63802d1ac3ab8177aabd0be92dae19ab55d2a63 |

실제 JSON/IHDR/chunk를 확인한 결과 scene/node/mesh/material은 각 1개이며 indexed triangles는 36 vertices/36 indices, POSITION/NORMAL/UV0와 추가 TANGENT를 가진다. 재질은 opaque base-color PNG, metallic 0, roughness 0.079다. PNG는 512×512, 8-bit RGB이며 IHDR/IDAT/IEND만 있다. 미사용 TANGENT와 normal map 등이 요구하는 tangent의 의미를 구분한다. 후속 실제 CLI CPU cook은 같은 모델2개 ID의 geometry1회 cook/공유 key와36/36 counts를 확인했다. 후속 P03.2a에서 원본 두 entity의 texture 실제 표시와 geometry/texture 단1회 upload, native redraw/capture를 확인했다. 정확한 command/counter와 한계는 [모델 실행 기록](../06-worklog/2026-10-16-model-texture-runtime.md)을 따른다. 이것은 static subset이며 일반 엔진/게임 인수는 아니다.

## 2D: Kenney Tiny Dungeon

[공식 배포 페이지](https://kenney.nl/assets/tiny-dungeon)와 archive 안 `License.txt`의 CC0 표기를 확인했다. archive 내부 명칭은 Tiny Dungeon 1.0, 작성일은 2022-07-05다.

- 원본 archive: [kenney_tiny-dungeon.zip](https://kenney.nl/media/pages/assets/tiny-dungeon/f8422efb44-1674742415/kenney_tiny-dungeon.zip), 98,530 bytes, SHA-256 `c109438ab06f65fd80f9b2686a4cf9c7c11dc64444b47333ec71d602f8bb5fc7`.
- 선택 이미지: `Tilemap/tilemap_packed.png`, 5,294 bytes, SHA-256 `d24e60a41e4ac7a745c0304dfde121143688557f40215f23221c29cfe683825f`.
- License.txt: 570 bytes, SHA-256 `9f574a2f1f636a3db8a0665ba90212f34e2b5e61ecb533d77c05237766374111`.

이미지 IHDR는 192×176, 8-bit indexed palette다. 실제 chunk는 IHDR/gAMA/PLTE/tRNS/IDAT/IEND이며 gAMA 값은45455, sRGB chunk는 없다. 첫 실제 CLI cook은 이 색정보를 Unsupported로 종료했다. 원본을 재저장하지 않고 [명시 color policy](../03-architecture/asset-import-foundation.md)로 해석하는 보완을 진행한다. 16×16 region의 atlas JSON은 엔진 검증용으로 별도 작성하고 이미지 원본과 혼동하지 않는다. 이미지 확장·region 경계·nearest/no-mip을 검증하며 sprite renderer나 tile game이 완성됐다는 뜻은 아니다.

보완 후 원본 SHA를 유지한 실제 CPU cook에서192×176 image와1 region atlas를 확인했다. warm 실행의 PNG decode0·atlas cook0도 확인했다. 실제 sprite 표시·타일 시뮬레이션은 별도다.

## 오디오: Kenney Interface Sounds와 명시적인 변환

[공식 배포 페이지](https://kenney.nl/assets/interface-sounds)와 archive 안 `License.txt`의 CC0 표기를 확인했다. archive 내부 명칭은 Interface Sounds 1.0, 작성일은 2020-02-11이다. 실제 오디오 100개는 OGG이며 WAV 원본으로 기록하지 않는다.

| 파일 | bytes | SHA-256 |
| --- | ---: | --- |
| [배포 archive](https://kenney.nl/media/pages/assets/interface-sounds/fa43c1dd4d-1677589452/kenney_interface-sounds.zip) | 834,536 | f2193d072726d6758a5f7871b2dcc54dcce0d5c35c6f0a62f92549b327c81232 |
| Audio/back_001.ogg | 6,997 | 07db973f79f6ae0f2edc34561e7592e24d0577455919fb602cb8ecc0da991dcf |
| License.txt | 574 | f7966c773bbed0eca6a9c75081c44a178b38eae112724dbb5fdfbd4192d118a9 |
| 로컬 변환 back_001.pcm16.wav | 5,456 | 090a56ddb09b9b94dbebd26790881d39ff7438eb82d8591bc56691f51c97ea04 |

로컬 FFmpeg 6.0을 외부 변환 도구로 한 번 사용했다. 엔진에 FFmpeg나 OGG decoder를 통합한 것은 아니다. 명령은 `ffmpeg -nostdin -hide_banner -loglevel error -n -i back_001.ogg -map_metadata -1 -c:a pcm_s16le back_001.pcm16.wav`다. 생성 WAV는 mono/44,100 Hz/2,689 frames/16-bit PCM이다. codec을 변환한 파생물이며 원래 OGG를 손실 전 원음으로 복원한 것이 아니다. 원본 CC0 표기와 선택한 원본 path/hash도 함께 보존한다. 다른 FFmpeg build의 출력 hash가 같다고 보장하지 않는다.

## 미지원 진단 입력: Kenney Nature Kit

[공식 페이지](https://kenney.nl/assets/nature-kit)와 archive 안 `License.txt`의 CC0 표기를 확인했다. 공식 페이지 업데이트란은 1.0, 실제 archive 내부 명칭은 Nature Kit 2.1로 달라서 취득한 자료를 후자로 기록한다.

| 파일 | bytes | SHA-256 |
| --- | ---: | --- |
| [배포 archive](https://kenney.nl/media/pages/assets/nature-kit/37ac38a37b-1677698939/kenney_nature-kit.zip) | 10,537,521 | fa7974a0d342bfe63c38664ba9f8ec1a4aab8ea25f099bdc56870e33588c4d9d |
| Models/GLTF format/rock_smallA.glb | 3,044 | df9fff9d711e61370e8df0caa2514c89b8f8a8dc6c6fafaf4eb2ec79c5ae07c1 |
| License.txt | 611 | cb96b75e3560ac78d7a53ce6f083f4cdb5c53faea6141b62d63458dcfe1e4b9d |

GLB JSON chunk에 `extensionsUsed: [KHR_materials_unlit]`가 있다. P03.1은 이 확장을 미지원으로 진단하는 계약이므로 원본의 성공을 요구하지 않는다. 선언을 지우고 opaque PBR로 표시하는 암묵적 변환은 하지 않는다. 미지원 오류가 source/path를 포함하는지 후속 검증한다.

실제 원본의 CLI 실행은 exit1, code `unsupported`, source `rock_smallA.glb`, path `extensionsUsed`와 해당 extension 이름을 반환했다. 원본 hash는 위와 동일하다.

## 실행 근거의 위치

CLI executable/source identity, command, catalog/source digest, cold/warm counters와 미지원 진단은 [작업 기록](../06-worklog/2026-10-16-asset-import.md)에 있다. 실제 WAV summary도 mono44,100Hz/2,689 frames와 원본 변환 hash를 확인했다. 위 hash 자체는 취득/원본 확인의 근거이고, 실제 CPU cook 결과를 화면 표시·재생 성공으로 확대하지 않는다. P03.1과 후속 runtime consumer 결과를 구분한다.
