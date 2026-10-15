# Managed host and Rust bridge (E04)

This folder contains the first .NET gameplay host and its engine integration fixtures. It targets .NET SDK `10.0.402`, runtime `10.0.13`, and C# `14.0`; `global.json` disables SDK roll-forward. The host calls the actual Rust `Engine` through the `daedalus-bridge` C ABI. It does not keep a managed ECS or a second authoritative transform store.

## Build and run

From the repository root, ensure the pinned .NET SDK installation is on `PATH` (`global.json` selects 10.0.402). On systems that install the SDK under `$HOME/.dotnet`, set `DOTNET_ROOT="$HOME/.dotnet"` and add `$DOTNET_ROOT` to `PATH`. Then build the bridge and managed projects:

```sh
cargo build -p daedalus-bridge
dotnet build managed/Daedalus.slnx --configuration Release
```

The dynamic library is normally `target/debug/libdaedalus_bridge.dylib` on macOS and `target/debug/libdaedalus_bridge.so` on Linux. Set its explicit path for the host:

```sh
DAEDALUS_NATIVE_PATH="$PWD/target/debug/libdaedalus_bridge.dylib" \
  dotnet run --project managed/Daedalus.ManagedHost --configuration Release -- --self-test
```

`--self-test` creates a Rust engine, creates a real Rust entity, submits a revision-checked transform command, advances one Rust tick, and checks the Rust `StepReport` acknowledgement and queried position. `--help` prints the host command surface. `--reload <manifest-v1> <manifest-v2> [ticks]` loads a module, invokes one module tick, replaces it through a collectible `AssemblyLoadContext`, and optionally invokes the replacement. A manifest is UTF-8 JSON with `id`, `version`, `sdkApiVersion` (currently `1`), `entryType`, and a relative `assembly` path. Its assembly and `.deps.json` must remain inside that module directory.

The engine integration test executable builds and loads three independent fixture assemblies. It checks direct native create/query/set/step, V1 to V2 state migration with entity identity and position continuity, successful staged command acknowledgement, migration failure after a staged write, old-module retention, and repeated load-context unload tracking:

```sh
DAEDALUS_NATIVE_PATH="$PWD/target/debug/libdaedalus_bridge.dylib" \
  dotnet run --project managed/Daedalus.ManagedIntegrationTests --configuration Release
```

The fixtures are narrow engine tests, not playable projects. The tests use the real native library built from the workspace. Managed commands are staged per callback and sent in a single bounded transform batch only after the callback succeeds. Replacement commits wait for the next Rust `StepReport` and accept the new generation only when all staged commands report `Applied`. This initial bridge refuses atomic module writes while any command is already pending; the bridge owns a default engine without registered Rust gameplay systems.

Within one callback, `GetTransform` reads the most recent transform staged by that callback; repeated `SetTransform` calls for one entity coalesce into one command. Cleanup registrations are capped at 4,096 per module generation and are removed and run when their staging callback fails. The host retains only weak references to pending unloads, caps that list at 256, and rejects another replacement at 255 pending generations so the active generation still has room for disposal. `UnloadedContextCount` is cumulative; `LiveUnloadedContextCount` reports the retained pending set after pruning collected contexts. The native registry separately caps engines at 16, buffers at 64, and retained response bytes at 32 MiB. Those are ownership and allocation limits, not a process-wide CPU budget; each Rust engine uses the runtime's configured Bevy compute pool.

The C# `SafeHandle` wrappers currently require a 64-bit process. P/Invoke passes bridge tokens as `ulong`; a `DangerousAddRef` keeps the `SafeHandle` alive during each native exchange. A 32-bit managed host is not supported by this slice.

## ABI v1

The C ABI exports:

```text
daedalus_bridge_abi_version() -> u32
daedalus_bridge_engine_create(out u64 token) -> i32
daedalus_bridge_engine_destroy(u64 token) -> i32
daedalus_bridge_engine_exchange_json(u64 token, const u8* input, usize input_len,
                                    out u64 buffer_token, out const u8* data, out usize len) -> i32
daedalus_bridge_buffer_release(u64 buffer_token) -> i32
```

Engine and response-buffer values are monotonically allocated registry tokens, never native pointers. Stale, released, or unknown tokens fail lookup. The host copies a response while a `SafeHandle` owns the output token, then releases it. Calls hold the bridge registry mutex through each engine exchange, tick, and destruction; perform these synchronous control operations on a host/control thread, not a frame-presentation callback.

Exchange requests are UTF-8 JSON using snake_case fields. These are separate request examples:

```json
{"operations":[{"op":"spawn_transform","transform":{"translation":[0,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]}}]}
{"operations":[{"op":"get_transform","entity":{"session":"1","index":0,"generation":1}}]}
{"operations":[{"op":"set_transform","target":{"session":"1","index":0,"generation":1},"expected_revision":0,"transform":{"translation":[1,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]}}]}
{"operations":[{"op":"step","count":1}]}
{"operations":[{"op":"list_entities"}]}
{"operations":[{"op":"latest_snapshot"}]}
```

`atomic_commands: true` is a restricted transform-command batch: each entity may appear once, all handles and expected revisions are prevalidated, the queue must be empty, and the bounded queue has capacity 256. Requests are limited to 1 MiB and 256 operations; combined step count is at most 120; transform entities are capped at 16,384; responses are capped at 8 MiB. The registry permits 16 engine instances, 64 retained response buffers, and 32 MiB total response bytes. Calls that cannot reserve output capacity return a resource-limit status before world mutation. A buffer pointer is borrowed only until its token is released; callers must not read it concurrently with release.

This is the initial copied JSON control surface, not the finalized managed SDK. It currently covers transform creation/query, revision-checked transform commands, explicit simulation steps, entity listings, and latest presentation snapshots. It does not provide component schemas, general ECS queries, event subscriptions, permission enforcement, async compilation, a system scheduler for C#, content handles, or runtime packaging. Modules execute trusted code in-process; `AssemblyLoadContext` does not sandbox filesystem, native calls, or CPU use. Module replacement is whole-assembly state migration, not `MetadataUpdater` hot edit. The bridge uses one serialized registry mutex and should not be treated as a measured high-throughput per-component API.
