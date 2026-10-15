using System.Runtime.InteropServices;
using System.Text.Json;
using Daedalus.ManagedSdk;

namespace Daedalus.ManagedHost;

public sealed class NativeBridgeException(string code, string message, int completedOperations = 0)
    : Exception($"Native bridge {code}: {message}")
{
    public string Code { get; } = code;

    public int CompletedOperations { get; } = completedOperations;
}

public sealed class NativeEngineSession : IDisposable
{
    public const int ManagedAbiVersion = 1;
    private const int MaxRequestBytes = 1_048_576;
    private const int MaxResponseBytes = 8_388_608;

    private static readonly JsonSerializerOptions JsonOptions = new(JsonSerializerDefaults.Web)
    {
        PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower,
        PropertyNameCaseInsensitive = true,
    };

    private readonly EngineSafeHandle _engine;

    private NativeEngineSession(EngineSafeHandle engine)
    {
        _engine = engine;
    }

    public static NativeEngineSession Create()
    {
        var actualVersion = NativeBridge.AbiVersion();
        if (actualVersion != ManagedAbiVersion)
        {
            throw new NativeBridgeException(
                "abi_version_mismatch",
                $"managed host expects ABI {ManagedAbiVersion}, native library reports {actualVersion}");
        }

        var status = NativeBridge.CreateEngine(out var token);
        if (status != 0)
        {
            throw StatusError(status);
        }
        try
        {
            return new NativeEngineSession(new EngineSafeHandle(token));
        }
        catch
        {
            NativeBridge.DestroyEngine(token);
            throw;
        }
    }

    public EntityHandle SpawnTransform(TransformState transform)
    {
        transform.Validate();
        using var response = Exchange(
            atomicCommands: false,
            [new { op = "spawn_transform", transform }]);
        return Deserialize<EntityHandle>(Result(response, 0, "entity"));
    }

    public EntityState GetTransform(EntityHandle entity)
    {
        using var response = Exchange(
            atomicCommands: false,
            [new { op = "get_transform", entity }]);
        var result = response.RootElement.GetProperty("results")[0].GetProperty("result");
        return new EntityState(
            Deserialize<EntityHandle>(result.GetProperty("entity")),
            Deserialize<TransformState>(result.GetProperty("transform")),
            result.GetProperty("revision").GetUInt64());
    }

    public IReadOnlyList<EntityHandle> GetEntities()
    {
        using var response = Exchange(false, [new { op = "list_entities" }]);
        var entities = response.RootElement.GetProperty("results")[0]
            .GetProperty("result").GetProperty("entities");
        return entities.EnumerateArray()
            .Select(item => Deserialize<EntityHandle>(item.GetProperty("entity")))
            .ToArray();
    }

    public string SetTransform(EntityHandle target, EntityState current, TransformState transform)
    {
        transform.Validate();
        using var response = Exchange(false,
        [
            new
            {
                op = "set_transform",
                target,
                expectedRevision = current.Revision,
                transform,
            },
        ]);
        return response.RootElement.GetProperty("results")[0]
            .GetProperty("result").GetProperty("command_id").GetString()!;
    }

    internal IReadOnlyList<string> ApplyTransformBatch(IReadOnlyCollection<StagedTransform> transforms)
    {
        if (transforms.Count == 0)
        {
            return [];
        }
        if (transforms.Count > 256)
        {
            throw new NativeBridgeException("operation_limit", "managed callback staged more than 256 transform commands");
        }

        var operations = transforms.Select(static item => (object)new
        {
            op = "set_transform",
            target = item.Entity,
            expectedRevision = item.ExpectedRevision,
            transform = item.Transform,
        }).ToArray();
        using var response = Exchange(atomicCommands: true, operations);
        return response.RootElement.GetProperty("results").EnumerateArray()
            .Select(static item => item.GetProperty("command_id").GetString()!)
            .ToArray();
    }

    public StepResult Step(uint count = 1)
    {
        using var response = Exchange(false, [new { op = "step", count }]);
        var reports = response.RootElement.GetProperty("results")[0]
            .GetProperty("result").GetProperty("reports");
        var commands = new List<StepCommandResult>();
        foreach (var report in reports.EnumerateArray())
        {
            foreach (var command in report.GetProperty("commands").EnumerateArray())
            {
                var outcome = command.GetProperty("outcome");
                var applied = outcome.TryGetProperty("Applied", out _);
                commands.Add(new StepCommandResult(
                    command.GetProperty("command_id").GetString()!,
                    applied,
                    applied ? null : outcome.ToString()));
            }
        }
        return new StepResult(commands);
    }

    internal void ConfirmApplied(IReadOnlyCollection<string> expectedCommandIds)
    {
        if (expectedCommandIds.Count == 0)
        {
            return;
        }
        var report = Step(1);
        var outcomes = report.Commands.ToDictionary(static item => item.CommandId, StringComparer.Ordinal);
        foreach (var commandId in expectedCommandIds)
        {
            if (!outcomes.TryGetValue(commandId, out var outcome) || !outcome.Applied)
            {
                throw new NativeBridgeException(
                    "command_not_applied",
                    outcome?.Error ?? $"no StepReport result returned for command {commandId}");
            }
        }
    }

    public JsonDocument LatestSnapshot()
    {
        return Exchange(false, [new { op = "latest_snapshot" }]);
    }

    public void Dispose() => _engine.Dispose();

    private JsonDocument Exchange(bool atomicCommands, IReadOnlyList<object> operations)
    {
        var request = JsonSerializer.SerializeToUtf8Bytes(
            new { atomicCommands, operations },
            JsonOptions);
        if (request.Length > MaxRequestBytes)
        {
            throw new NativeBridgeException("request_too_large", "request exceeds the 1 MiB limit");
        }

        var requestPointer = Marshal.AllocHGlobal(request.Length);
        try
        {
            Marshal.Copy(request, 0, requestPointer, request.Length);
            var addedReference = false;
            try
            {
                _engine.DangerousAddRef(ref addedReference);
                var engineToken = checked((ulong)_engine.DangerousGetHandle().ToInt64());
                var status = NativeBridge.Exchange(
                    engineToken,
                    requestPointer,
                    checked((nuint)request.Length),
                    out var bufferToken,
                    out var dataPointer,
                    out var responseLength);
                if (bufferToken == 0)
                {
                    throw StatusError(status);
                }

                using var buffer = new BridgeBufferSafeHandle(bufferToken);
                if (dataPointer == nint.Zero || responseLength == 0 || responseLength > MaxResponseBytes)
                {
                    throw new NativeBridgeException("invalid_native_buffer", "native response buffer has invalid pointer or length");
                }

                var responseBytes = new byte[checked((int)responseLength)];
                Marshal.Copy(dataPointer, responseBytes, 0, responseBytes.Length);
                var response = JsonDocument.Parse(responseBytes);
                if (!response.RootElement.GetProperty("ok").GetBoolean())
                {
                    var error = response.RootElement.GetProperty("error");
                    var exception = new NativeBridgeException(
                        error.GetProperty("code").GetString() ?? "operation_failed",
                        error.GetProperty("message").GetString() ?? "native operation failed",
                        error.GetProperty("completed_operations").GetInt32());
                    response.Dispose();
                    throw exception;
                }
                if (status != 0)
                {
                    response.Dispose();
                    throw StatusError(status);
                }
                return response;
            }
            finally
            {
                if (addedReference)
                {
                    _engine.DangerousRelease();
                }
            }
        }
        finally
        {
            Marshal.FreeHGlobal(requestPointer);
        }
    }

    private static JsonElement Result(JsonDocument response, int operationIndex, string property)
    {
        return response.RootElement.GetProperty("results")[operationIndex]
            .GetProperty("result").GetProperty(property);
    }

    private static T Deserialize<T>(JsonElement element) =>
        element.Deserialize<T>(JsonOptions)
        ?? throw new NativeBridgeException("invalid_native_json", $"native response omitted {typeof(T).Name}");

    private static NativeBridgeException StatusError(int status) => status switch
    {
        1 => new NativeBridgeException("invalid_argument", "native ABI rejected a null or invalid pointer/length pair"),
        2 => new NativeBridgeException("invalid_handle", "native handle is stale or unknown"),
        3 => new NativeBridgeException("invalid_utf8", "request bytes are not valid UTF-8"),
        4 => new NativeBridgeException("too_large", "request or response exceeds the native ABI size limit"),
        5 => new NativeBridgeException("invalid_json", "request JSON is invalid"),
        6 => new NativeBridgeException("operation_failed", "one or more native operations failed"),
        7 => new NativeBridgeException("native_internal", "native bridge registry or runtime operation failed"),
        8 => new NativeBridgeException("resource_limit", "native bridge engine or response-buffer capacity is exhausted"),
        255 => new NativeBridgeException("native_panic", "Rust panicked inside the bridge boundary"),
        _ => new NativeBridgeException("native_status", $"native bridge returned status {status}"),
    };
}

internal sealed record StagedTransform(EntityHandle Entity, ulong ExpectedRevision, TransformState Transform);

public sealed record StepResult(IReadOnlyList<StepCommandResult> Commands);

public sealed record StepCommandResult(string CommandId, bool Applied, string? Error);
